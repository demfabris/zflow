//! Platform-neutral input captured by a source adapter.

use std::{path::PathBuf, time::Instant};

use crate::core::{HidUsage, MotionDelta, PointerButton, TouchState};

pub const MAX_TOUCHPAD_CONTACTS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Released,
    Pressed,
    Repeat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTransition {
    Key {
        usage: HidUsage,
        state: KeyState,
    },
    Button {
        button: PointerButton,
        state: KeyState,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CaptureFrame {
    pub transitions: Vec<CaptureTransition>,
    pub motion: MotionDelta,
    pub touch_snapshot: Option<TouchState>,
    /// Source events consumed while assembling this frame.
    pub event_count: u64,
}

impl CaptureFrame {
    pub fn is_empty(&self) -> bool {
        self.transitions.is_empty()
            && self.motion == MotionDelta::default()
            && self.touch_snapshot.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedDeviceFrame {
    pub device_path: PathBuf,
    pub frame: CaptureFrame,
    /// When the source adapter completed this frame.
    pub captured_at: Instant,
}
