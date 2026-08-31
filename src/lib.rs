//! Headless zflow core, platform adapters, transport, and local control plane.

pub mod cli;
pub mod config;
pub mod control;
pub mod core;
pub mod daemon;
pub mod discovery;
pub mod identity;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod metrics;
pub mod pairing;
#[cfg(target_os = "linux")]
pub mod runtime;
#[cfg(target_os = "linux")]
pub mod session;
pub mod transport;
pub mod wire;
