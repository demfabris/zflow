//! Headless platform runtime.
//!
//! The Linux implementation deliberately keeps all input descriptors and
//! virtual-device state on one synchronous thread. That thread is also the
//! systemd watchdog heartbeat: a wedged input loop cannot be hidden by an
//! unrelated liveness task.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod readiness;

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "linux")]
pub use readiness::*;
