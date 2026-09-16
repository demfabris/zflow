//! Application services shared by the native Mac app and Linux desktop agent.

#[cfg(target_os = "linux")]
mod desktop;
#[cfg(target_os = "linux")]
pub mod desktop_agent;
pub(crate) mod displays;
#[cfg(target_os = "macos")]
mod handoff;
pub mod layout_model;
pub mod model;
#[cfg(target_os = "macos")]
mod native;
#[cfg(target_os = "macos")]
mod nearby;
#[cfg(target_os = "macos")]
mod pairing;
#[cfg(target_os = "macos")]
mod probe;
#[cfg(target_os = "macos")]
mod sharing;
#[cfg(target_os = "macos")]
pub(crate) use native::NativeApp;
