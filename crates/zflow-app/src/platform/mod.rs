//! What only one system has: its rows under the shared ones, how the app
//! starts at login, updates itself, asks for permissions and lives in the
//! menu bar or tray.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "macos")]
pub use macos::*;
#[cfg(windows)]
pub use windows::*;

use serde::Deserialize;
use zz_gpui::Entity;

/// The platform's services, as a global the window and menus reach.
pub struct SharedServices(pub Entity<Services>);

impl zz_gpui::Global for SharedServices {}

/// The rows a system adds under the shared ones. Each system fills its own
/// and leaves the rest unset; Linux sends none.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Platform {
    /// Mac: whether zflow may move the pointer here.
    pub accessibility: Option<bool>,
    /// Mac: whether zflow may look for computers on the network.
    pub local_network: Option<LocalNetwork>,
    /// Mac: whether `AirDrop`'s radio pauses while input is shared.
    pub block_awdl: Option<bool>,
    /// Windows: whether the engine runs as administrator.
    pub elevated: Option<bool>,
    /// Windows: where the engine listens, and this computer's addresses.
    pub listen: Option<String>,
    pub addresses: Vec<String>,
    /// Windows: this computer's name, as others see it.
    pub name: Option<String>,
    /// Windows: whether input can be sent from this desktop right now.
    pub available: Option<bool>,
    /// Windows: a notice from the engine about a link.
    pub notice: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalNetwork {
    /// Unknown until zflow first looks for computers, which is when macOS asks.
    #[default]
    Unknown,
    Allowed,
    Blocked,
}

/// What the Updates rows show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Updates {
    /// Why this copy cannot update itself.
    pub unavailable: Option<String>,
    /// A newer version, once one is found.
    pub available: Option<String>,
    /// Whether it checks on its own; `None` where it cannot.
    pub automatic: Option<bool>,
    /// Mac: whether it also installs on its own.
    pub install_automatically: Option<bool>,
    pub checking: bool,
    /// The outcome of the last check or install.
    pub note: Option<String>,
}
