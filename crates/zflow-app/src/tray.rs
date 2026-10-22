//! The menu bar icon on a Mac and the notification-area icon on Windows:
//! how sharing goes, the computers, and the way into the window.

use zflow::app::api::Request;
use zz_gpui::{App, Entity};
use zz_gpui_tray::{Menu, MenuEntry, Tray, TrayEvent, TrayOptions};

use crate::{
    copy,
    model::{Model, Page, Snapshot},
    platform::{self, SharedServices},
};

/// The live icon, kept as long as the app runs.
struct AppTray {
    tray: Option<Tray>,
    menu: Menu,
    look: Option<(String, bool, bool)>,
}

impl zz_gpui::Global for AppTray {}

/// The menu for `snapshot`. `update` is a newer version, once one is found.
pub fn entries(
    snapshot: Option<&Snapshot>,
    update: Option<&str>,
    lens: Option<&str>,
) -> Vec<MenuEntry> {
    #[cfg(not(target_os = "macos"))]
    let _ = lens;
    let mut entries = vec![MenuEntry::label(format!(
        "zflow · {}",
        snapshot.map_or("Starting…", |snapshot| snapshot.status.title.as_str())
    ))];
    if let Some(snapshot) = snapshot {
        if let Some(sharing) = snapshot.sharing {
            entries.push(MenuEntry::check("Sharing", "sharing", sharing));
        }
        let found: Vec<_> = snapshot
            .unplaced
            .iter()
            .filter(|computer| copy::placeable(computer))
            .collect();
        if !snapshot.peers.is_empty() || !found.is_empty() {
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::label("Computers"));
            for peer in &snapshot.peers {
                entries.push(MenuEntry::item(
                    format!("{} — {}", peer.name, copy::peer_state(peer.state)),
                    format!("computer:{}", peer.name),
                ));
            }
            // Placing one happens on the arrangement.
            for computer in found {
                entries.push(MenuEntry::item(
                    format!("{} — Place…", computer.name),
                    "computers",
                ));
            }
        }
        #[cfg(target_os = "macos")]
        {
            let linked: Vec<_> = snapshot
                .peers
                .iter()
                .filter(|peer| copy::linked(peer.state))
                .collect();
            if !linked.is_empty() {
                entries.push(MenuEntry::Separator);
                for peer in linked {
                    let hotkey = if lens == Some(peer.name.as_str()) {
                        format!("\t{}", crate::lens::HOTKEY_LABEL)
                    } else {
                        String::new()
                    };
                    entries.push(MenuEntry::item(
                        format!("Look at {}{hotkey}", peer.name),
                        format!("lens:{}", peer.name),
                    ));
                }
            }
        }
    }
    entries.push(MenuEntry::Separator);
    entries.push(MenuEntry::item("Open zflow", "open"));
    entries.push(MenuEntry::item(
        update.map_or_else(
            || "Check for Updates…".to_owned(),
            |version| format!("Update to {version}…"),
        ),
        "updates",
    ));
    entries.push(MenuEntry::item("Quit zflow", "quit"));
    entries
}

pub fn init(model: &Entity<Model>, cx: &mut App) {
    let (events, inbox) = async_channel::unbounded();
    let menu = Menu::default();
    menu.set(entries(None, None, None));
    let tray = Tray::spawn(
        TrayOptions {
            id: crate::APP_ID.into(),
            tooltip: "zflow".into(),
            icon: platform::tray_look(true, false),
            // A Mac's menu bar icon opens its menu on any click; on Windows
            // the icon opens the window and its menu is a right click away.
            menu_on_click: cfg!(target_os = "macos"),
        },
        menu.clone(),
        events,
    );
    if tray.is_none() {
        log::warn!("zflow has no tray icon here");
    }
    cx.set_global(AppTray {
        tray,
        menu,
        look: None,
    });
    cx.observe(model, |model, cx| refresh(&model, cx)).detach();
    if let Some(services) = cx
        .try_global::<SharedServices>()
        .map(|services| services.0.clone())
    {
        let model = model.clone();
        cx.observe(&services, move |_, cx| refresh(&model, cx))
            .detach();
    }
    let model = model.clone();
    cx.spawn(async move |cx| {
        while let Ok(event) = inbox.recv().await {
            let () = cx.update(|cx| handle(&event, &model, cx));
        }
    })
    .detach();
}

fn refresh(model: &Entity<Model>, cx: &mut App) {
    let update = cx
        .try_global::<SharedServices>()
        .and_then(|services| services.0.read(cx).updates.available.clone());
    let snapshot = model.read(cx).snapshot.clone();
    #[cfg(target_os = "macos")]
    let lens = crate::lens::target(cx);
    #[cfg(not(target_os = "macos"))]
    let lens: Option<String> = None;
    let next = entries(snapshot.as_ref(), update.as_deref(), lens.as_deref());
    let healthy = snapshot.as_ref().is_some_and(|snapshot| {
        matches!(
            snapshot.status.state,
            zflow::app::api::State::Ready
                | zflow::app::api::State::Controlling
                | zflow::app::api::State::Controlled
        )
    });
    let title = snapshot.as_ref().map_or_else(
        || "Needs attention".to_owned(),
        |snapshot| snapshot.status.title.clone(),
    );
    let tooltip = match &update {
        Some(_) => "zflow, update available".to_owned(),
        None => format!("zflow, {title}"),
    };
    let tray = cx.global_mut::<AppTray>();
    if tray.menu.set(next)
        && let Some(icon) = &tray.tray
    {
        icon.menu_changed();
    }
    let look = (tooltip.clone(), healthy, update.is_some());
    if tray.look.as_ref() != Some(&look) {
        if let Some(icon) = &tray.tray {
            icon.set_look(&tooltip, &platform::tray_look(healthy, update.is_some()));
        }
        tray.look = Some(look);
    }
}

fn handle(event: &TrayEvent, model: &Entity<Model>, cx: &mut App) {
    let action = match event {
        TrayEvent::Activate => "open",
        TrayEvent::Action(action) => action.as_str(),
        TrayEvent::Available(_) => return,
    };
    match action {
        "open" => crate::open(cx),
        "sharing" => {
            let enabled = model.read(cx).snapshot.as_ref().and_then(|s| s.sharing) != Some(true);
            model.read(cx).send(Request::SetSharing { enabled });
        }
        "computers" => {
            model.update(cx, |model, cx| model.show(Page::Computers, cx));
            crate::open(cx);
        }
        "updates" => platform::check_updates(cx),
        "quit" => platform::quit(cx),
        action => {
            if let Some(name) = action.strip_prefix("computer:") {
                model.update(cx, |model, cx| {
                    model.show(Page::Computer(name.to_owned()), cx);
                });
                crate::open(cx);
            } else if let Some(name) = action.strip_prefix("lens:") {
                #[cfg(target_os = "macos")]
                crate::lens::look(name, cx);
                #[cfg(not(target_os = "macos"))]
                let _ = name;
            }
        }
    }
}

/// An SVG drawn as a square PNG `size` pixels across.
#[cfg(target_os = "macos")]
pub fn rasterize(svg: &str, size: u32) -> Vec<u8> {
    let Ok(tree) = resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default()) else {
        return Vec::new();
    };
    let Some(mut pixmap) = resvg::tiny_skia::Pixmap::new(size, size) else {
        return Vec::new();
    };
    let scale = size as f32 / tree.size().width().max(tree.size().height());
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_menu_starts_with_the_status_and_ends_with_quit() {
        let menu = entries(None, Some("0.8.0"), None);
        assert_eq!(menu.first(), Some(&MenuEntry::label("zflow · Starting…")));
        assert_eq!(menu.last(), Some(&MenuEntry::item("Quit zflow", "quit")));
        assert!(menu.contains(&MenuEntry::item("Update to 0.8.0…", "updates")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_mark_rasterizes() {
        let png = rasterize(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10"/></svg>"#,
            36,
        );
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
    }
}
