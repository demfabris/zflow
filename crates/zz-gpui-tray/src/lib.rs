//! A tray icon with a menu: a `StatusNotifierItem` on Linux, an `NSStatusItem`
//! in the macOS menu bar, and a notification-area icon on Windows.
//!
//! The app keeps a [`Menu`] current and reads [`TrayEvent`]s from a channel,
//! on whatever thread suits it. Each backend builds the native menu from the
//! [`Menu`] when it opens, so the app never touches a native menu itself.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

use std::sync::Arc;

use async_channel::Sender;
use parking_lot::Mutex;

/// What the person did with the icon.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrayEvent {
    /// A primary click on the icon itself, where the platform reports one.
    Activate,
    /// A menu item, by the action it was given.
    Action(String),
    /// Whether a tray host shows the icon; it can come and go with the
    /// desktop shell.
    Available(bool),
}

/// One row of the menu.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuEntry {
    Separator,
    Item {
        label: String,
        /// What [`TrayEvent::Action`] carries; `None` shows the row dimmed.
        action: Option<String>,
        /// A check mark beside the label; `None` draws a plain row.
        checked: Option<bool>,
    },
}

impl MenuEntry {
    /// A row that sends `action`.
    pub fn item(label: impl Into<String>, action: impl Into<String>) -> Self {
        Self::Item {
            label: label.into(),
            action: Some(action.into()),
            checked: None,
        }
    }

    /// A dimmed row that only says something.
    pub fn label(label: impl Into<String>) -> Self {
        Self::Item {
            label: label.into(),
            action: None,
            checked: None,
        }
    }

    /// A row with a check mark that sends `action`.
    pub fn check(label: impl Into<String>, action: impl Into<String>, checked: bool) -> Self {
        Self::Item {
            label: label.into(),
            action: Some(action.into()),
            checked: Some(checked),
        }
    }
}

/// The menu the icon opens, shared with the backend's thread.
#[derive(Clone, Debug, Default)]
pub struct Menu(Arc<Mutex<Vec<MenuEntry>>>);

impl Menu {
    pub fn entries(&self) -> Vec<MenuEntry> {
        self.0.lock().clone()
    }

    /// Replaces the menu. Returns whether it changed.
    pub fn set(&self, entries: Vec<MenuEntry>) -> bool {
        let mut current = self.0.lock();
        if *current == entries {
            return false;
        }
        *current = entries;
        true
    }
}

/// The icon's picture, as PNG bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayIcon {
    pub png: Vec<u8>,
    /// macOS draws a template image in the menu bar's own ink, so it reads
    /// on light and dark bars. Only its alpha counts.
    pub template: bool,
}

#[derive(Clone, Debug)]
pub struct TrayOptions {
    /// A stable id, such as the app's reverse-DNS name.
    pub id: String,
    /// What the icon says on hover.
    pub tooltip: String,
    pub icon: TrayIcon,
    /// A primary click opens the menu, as menu bar icons do on macOS,
    /// instead of sending [`TrayEvent::Activate`].
    pub menu_on_click: bool,
}

/// A live tray icon. Dropping it removes the icon.
pub struct Tray {
    #[cfg(target_os = "linux")]
    backend: linux::Service,
    #[cfg(target_os = "macos")]
    backend: macos::StatusItem,
    #[cfg(target_os = "windows")]
    backend: windows::NotifyIcon,
}

impl Tray {
    /// Puts the icon up. `None` where the platform has no tray, or no host
    /// answers. On macOS, call it on the main thread.
    pub fn spawn(options: TrayOptions, menu: Menu, events: Sender<TrayEvent>) -> Option<Self> {
        #[cfg(target_os = "linux")]
        let backend = linux::spawn(options, menu, events)?;
        #[cfg(target_os = "macos")]
        let backend = macos::spawn(options, menu, events)?;
        #[cfg(target_os = "windows")]
        let backend = windows::spawn(options, menu, events)?;
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = (options, menu, events);
            return None;
        }
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        Some(Self { backend })
    }

    /// Tells the host the menu changed. Call it after [`Menu::set`] returns
    /// true; the backends that build the menu on each open ignore it.
    pub fn menu_changed(&self) {
        #[cfg(target_os = "linux")]
        self.backend.menu_changed();
    }

    /// Changes the hover text and the picture.
    pub fn set_look(&self, tooltip: &str, icon: &TrayIcon) {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        self.backend.set_look(tooltip, icon);
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        let _ = (tooltip, icon);
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn decode(icon: &TrayIcon) -> Option<image::RgbaImage> {
    match image::load_from_memory_with_format(&icon.png, image::ImageFormat::Png) {
        Ok(image) => Some(image.into_rgba8()),
        Err(error) => {
            log::warn!(target: "zz::tray", "could not decode the tray icon: {error}");
            None
        }
    }
}

fn send(events: &Sender<TrayEvent>, event: TrayEvent) {
    if let Err(error) = events.try_send(event) {
        log::warn!(target: "zz::tray", "dropped a tray event: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_menu_reports_only_real_changes() {
        let menu = Menu::default();
        assert!(menu.set(vec![MenuEntry::item("Open", "open")]));
        assert!(!menu.set(vec![MenuEntry::item("Open", "open")]));
        assert!(menu.set(vec![MenuEntry::check("Sharing", "sharing", true)]));
        assert_eq!(
            menu.entries(),
            vec![MenuEntry::Item {
                label: "Sharing".into(),
                action: Some("sharing".into()),
                checked: Some(true),
            }]
        );
    }
}
