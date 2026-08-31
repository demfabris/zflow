//! Linux input backbone.
//!
//! Physical devices are observed through evdev and grabbed only for an active
//! remote-input activation. Remote input is injected through a stable uinput
//! keyboard/pointer pair. None of this module depends on a desktop session.

mod capture;
mod devices;
mod mapping;
mod ownership;
mod seat;
mod uinput;

pub use capture::*;
pub use devices::*;
pub use mapping::*;
pub use ownership::*;
pub use seat::*;
pub use uinput::*;
