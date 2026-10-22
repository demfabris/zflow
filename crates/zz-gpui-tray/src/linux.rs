use async_channel::Sender;
use ksni::blocking::TrayMethods as _;

use super::{Menu, MenuEntry, TrayEvent, TrayIcon, TrayOptions, decode, send};

/// The largest edge sent to the host; panels draw the icon at 16 to 32 px.
const ICON_SIZE: u32 = 48;

/// A live `StatusNotifierItem`. Dropping it shuts the service down, which is
/// what removes the icon.
pub(super) struct Service {
    handle: ksni::blocking::Handle<SniTray>,
}

impl Service {
    pub(super) fn menu_changed(&self) {
        let _ = self.handle.update(|_| {});
    }

    pub(super) fn set_look(&self, tooltip: &str, icon: &TrayIcon) {
        let tooltip = tooltip.to_owned();
        let pixmap = pixmap(icon);
        let _ = self.handle.update(move |tray| {
            tray.tooltip = tooltip;
            tray.icon = pixmap;
        });
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        drop(self.handle.shutdown());
    }
}

pub(super) fn spawn(
    options: TrayOptions,
    menu: Menu,
    events: Sender<TrayEvent>,
) -> Option<Service> {
    let tray = SniTray {
        id: options.id,
        tooltip: options.tooltip,
        icon: pixmap(&options.icon),
        menu_on_click: options.menu_on_click,
        menu,
        events,
    };
    match tray.assume_sni_available(true).spawn() {
        Ok(handle) => Some(Service { handle }),
        Err(error) => {
            log::warn!(target: "zz::tray", "no system tray host: {error}");
            None
        }
    }
}

struct SniTray {
    id: String,
    tooltip: String,
    icon: Vec<ksni::Icon>,
    menu_on_click: bool,
    menu: Menu,
    events: Sender<TrayEvent>,
}

impl ksni::Tray for SniTray {
    const MENU_ON_ACTIVATE: bool = false;

    fn watcher_online(&self) {
        send(&self.events, TrayEvent::Available(true));
    }

    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        send(&self.events, TrayEvent::Available(false));
        true
    }

    fn id(&self) -> String {
        self.id.clone()
    }

    fn title(&self) -> String {
        self.tooltip.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.tooltip.clone(),
            ..Default::default()
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon.clone()
    }

    // GNOME's AppIndicator extension opens the menu on every click instead
    // of delivering this, so an app should repeat its activation in the menu.
    fn activate(&mut self, _x: i32, _y: i32) {
        if !self.menu_on_click {
            send(&self.events, TrayEvent::Activate);
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        self.menu
            .entries()
            .into_iter()
            .map(|entry| match entry {
                MenuEntry::Separator => ksni::MenuItem::Separator,
                MenuEntry::Item {
                    label,
                    action,
                    checked: Some(checked),
                } => ksni::menu::CheckmarkItem {
                    label,
                    checked,
                    enabled: action.is_some(),
                    activate: activate(action),
                    ..Default::default()
                }
                .into(),
                MenuEntry::Item {
                    label,
                    action,
                    checked: None,
                } => ksni::menu::StandardItem {
                    label,
                    enabled: action.is_some(),
                    activate: activate(action),
                    ..Default::default()
                }
                .into(),
            })
            .collect()
    }
}

fn activate(action: Option<String>) -> Box<dyn Fn(&mut SniTray) + Send + 'static> {
    Box::new(move |tray: &mut SniTray| {
        if let Some(action) = &action {
            send(&tray.events, TrayEvent::Action(action.clone()));
        }
    })
}

// SNI pixel data is ARGB32 in network byte order.
fn pixmap(icon: &TrayIcon) -> Vec<ksni::Icon> {
    let Some(rgba) = decode(icon) else {
        return Vec::new();
    };
    let rgba = if rgba.width() > ICON_SIZE || rgba.height() > ICON_SIZE {
        image::imageops::resize(
            &rgba,
            ICON_SIZE,
            ICON_SIZE,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        rgba
    };
    let (width, height) = rgba.dimensions();
    let data = rgba
        .pixels()
        .flat_map(|pixel| {
            let [r, g, b, a] = pixel.0;
            [a, r, g, b]
        })
        .collect();
    vec![ksni::Icon {
        width: width as i32,
        height: height as i32,
        data,
    }]
}
