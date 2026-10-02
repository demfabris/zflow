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
// On Linux the service finds computers, and only the record type is used.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[cfg_attr(target_os = "linux", allow(dead_code))]
mod nearby;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod pairing;
#[cfg(target_os = "macos")]
mod sharing;
#[cfg(target_os = "macos")]
pub(crate) use native::NativeApp;
