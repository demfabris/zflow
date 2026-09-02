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
mod touch;
mod uinput;

pub use crate::capture::{
    CaptureFrame, CaptureTransition, CapturedDeviceFrame, KeyState, MAX_TOUCHPAD_CONTACTS,
};
pub use capture::*;
pub use devices::*;
pub use mapping::*;
pub use ownership::*;
pub use seat::*;
pub use touch::*;
pub use uinput::*;
