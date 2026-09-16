//! Headless zflow core, platform adapters, transport, and local control plane.

pub mod app;
pub mod capture;
pub mod cli;
pub mod config;
pub mod control;
pub mod core;
#[cfg(target_os = "linux")]
pub mod daemon;
pub mod desktop;
pub mod discovery;
#[cfg(target_os = "macos")]
mod ffi;
pub mod identity;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod metrics;
pub mod pairing;
pub mod peer_view;
#[cfg(target_os = "linux")]
pub mod runtime;
pub mod session;
pub mod transport;
pub mod wire;
