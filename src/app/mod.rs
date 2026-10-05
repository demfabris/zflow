//! Application services shared by the native Mac app and Linux desktop agent.

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod api;
#[cfg(target_os = "linux")]
mod desktop;
#[cfg(target_os = "linux")]
pub mod desktop_agent;
#[cfg(target_os = "linux")]
pub mod gnome;
pub(crate) mod handoff;
pub mod layout_model;
pub mod model;
#[cfg(target_os = "macos")]
mod native;
#[cfg(target_os = "linux")]
pub mod updates;
// On Linux the service finds computers itself.
#[cfg(target_os = "macos")]
mod nearby;
#[cfg(target_os = "macos")]
mod sharing;
#[cfg(target_os = "macos")]
pub(crate) use native::NativeApp;
