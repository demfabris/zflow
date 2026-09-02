use std::fmt;

use evdev::{EventType, InputEvent, KeyCode, RelativeAxisCode, SynchronizationCode};
use thiserror::Error;

use crate::{
    capture::{CaptureFrame, CaptureTransition, KeyState},
    core::{HidUsage, HidUsagePage, MotionDelta, PointerButton},
};

const HID_KEYBOARD: u16 = 0x07;
const HID_CONSUMER: u16 = 0x0c;
const WHEEL_CLICK_UNITS: i64 = 120;

#[derive(Debug, Clone, Copy)]
struct KeyMapping {
    evdev: u16,
    page: u16,
    usage: u16,
}

impl KeyMapping {
    const fn keyboard(evdev: u16, usage: u16) -> Self {
        Self {
            evdev,
            page: HID_KEYBOARD,
            usage,
        }
    }

    const fn consumer(evdev: u16, usage: u16) -> Self {
        Self {
            evdev,
            page: HID_CONSUMER,
            usage,
        }
    }

    const fn hid(self) -> HidUsage {
        HidUsage::new(HidUsagePage(self.page), self.usage)
    }
}

// Linux input-event-codes.h to USB HID Usage Tables. This intentionally lists
// only keys whose semantics are unambiguous. Unknown/vendor usages are ignored
// rather than guessed or allowed to tear down an otherwise valid activation.
const KEY_MAP: &[KeyMapping] = &[
    KeyMapping::keyboard(1, 0x29), // Esc
    KeyMapping::keyboard(2, 0x1e),
    KeyMapping::keyboard(3, 0x1f),
    KeyMapping::keyboard(4, 0x20),
    KeyMapping::keyboard(5, 0x21),
    KeyMapping::keyboard(6, 0x22),
    KeyMapping::keyboard(7, 0x23),
    KeyMapping::keyboard(8, 0x24),
    KeyMapping::keyboard(9, 0x25),
    KeyMapping::keyboard(10, 0x26),
    KeyMapping::keyboard(11, 0x27),
    KeyMapping::keyboard(12, 0x2d),
    KeyMapping::keyboard(13, 0x2e),
    KeyMapping::keyboard(14, 0x2a),
    KeyMapping::keyboard(15, 0x2b),
    KeyMapping::keyboard(16, 0x14), // Q
    KeyMapping::keyboard(17, 0x1a),
    KeyMapping::keyboard(18, 0x08),
    KeyMapping::keyboard(19, 0x15),
    KeyMapping::keyboard(20, 0x17),
    KeyMapping::keyboard(21, 0x1c),
    KeyMapping::keyboard(22, 0x18),
    KeyMapping::keyboard(23, 0x0c),
    KeyMapping::keyboard(24, 0x12),
    KeyMapping::keyboard(25, 0x13),
    KeyMapping::keyboard(26, 0x2f),
    KeyMapping::keyboard(27, 0x30),
    KeyMapping::keyboard(28, 0x28),
    KeyMapping::keyboard(29, 0xe0),
    KeyMapping::keyboard(30, 0x04), // A
    KeyMapping::keyboard(31, 0x16),
    KeyMapping::keyboard(32, 0x07),
    KeyMapping::keyboard(33, 0x09),
    KeyMapping::keyboard(34, 0x0a),
    KeyMapping::keyboard(35, 0x0b),
    KeyMapping::keyboard(36, 0x0d),
    KeyMapping::keyboard(37, 0x0e),
    KeyMapping::keyboard(38, 0x0f),
    KeyMapping::keyboard(39, 0x33),
    KeyMapping::keyboard(40, 0x34),
    KeyMapping::keyboard(41, 0x35),
    KeyMapping::keyboard(42, 0xe1),
    KeyMapping::keyboard(43, 0x31),
    KeyMapping::keyboard(44, 0x1d), // Z
    KeyMapping::keyboard(45, 0x1b),
    KeyMapping::keyboard(46, 0x06),
    KeyMapping::keyboard(47, 0x19),
    KeyMapping::keyboard(48, 0x05),
    KeyMapping::keyboard(49, 0x11),
    KeyMapping::keyboard(50, 0x10),
    KeyMapping::keyboard(51, 0x36),
    KeyMapping::keyboard(52, 0x37),
    KeyMapping::keyboard(53, 0x38),
    KeyMapping::keyboard(54, 0xe5),
    KeyMapping::keyboard(55, 0x55),
    KeyMapping::keyboard(56, 0xe2),
    KeyMapping::keyboard(57, 0x2c),
    KeyMapping::keyboard(58, 0x39),
    KeyMapping::keyboard(59, 0x3a), // F1..F10
    KeyMapping::keyboard(60, 0x3b),
    KeyMapping::keyboard(61, 0x3c),
    KeyMapping::keyboard(62, 0x3d),
    KeyMapping::keyboard(63, 0x3e),
    KeyMapping::keyboard(64, 0x3f),
    KeyMapping::keyboard(65, 0x40),
    KeyMapping::keyboard(66, 0x41),
    KeyMapping::keyboard(67, 0x42),
    KeyMapping::keyboard(68, 0x43),
    KeyMapping::keyboard(69, 0x53),
    KeyMapping::keyboard(70, 0x47),
    KeyMapping::keyboard(71, 0x5f),
    KeyMapping::keyboard(72, 0x60),
    KeyMapping::keyboard(73, 0x61),
    KeyMapping::keyboard(74, 0x56),
    KeyMapping::keyboard(75, 0x5c),
    KeyMapping::keyboard(76, 0x5d),
    KeyMapping::keyboard(77, 0x5e),
    KeyMapping::keyboard(78, 0x57),
    KeyMapping::keyboard(79, 0x59),
    KeyMapping::keyboard(80, 0x5a),
    KeyMapping::keyboard(81, 0x5b),
    KeyMapping::keyboard(82, 0x62),
    KeyMapping::keyboard(83, 0x63),
    KeyMapping::keyboard(85, 0x94), // LANG5
    KeyMapping::keyboard(86, 0x64), // non-US backslash
    KeyMapping::keyboard(87, 0x44),
    KeyMapping::keyboard(88, 0x45),
    KeyMapping::keyboard(89, 0x87), // international/language keys
    KeyMapping::keyboard(90, 0x92),
    KeyMapping::keyboard(91, 0x93),
    KeyMapping::keyboard(92, 0x8a),
    KeyMapping::keyboard(93, 0x88),
    KeyMapping::keyboard(94, 0x8b),
    KeyMapping::keyboard(95, 0x8c),
    KeyMapping::keyboard(96, 0x58),
    KeyMapping::keyboard(97, 0xe4),
    KeyMapping::keyboard(98, 0x54),
    KeyMapping::keyboard(99, 0x46),
    KeyMapping::keyboard(100, 0xe6),
    KeyMapping::keyboard(102, 0x4a),
    KeyMapping::keyboard(103, 0x52),
    KeyMapping::keyboard(104, 0x4b),
    KeyMapping::keyboard(105, 0x50),
    KeyMapping::keyboard(106, 0x4f),
    KeyMapping::keyboard(107, 0x4d),
    KeyMapping::keyboard(108, 0x51),
    KeyMapping::keyboard(109, 0x4e),
    KeyMapping::keyboard(110, 0x49),
    KeyMapping::keyboard(111, 0x4c),
    KeyMapping::consumer(113, 0xe2), // mute/volume/power
    KeyMapping::consumer(114, 0xea),
    KeyMapping::consumer(115, 0xe9),
    KeyMapping::consumer(116, 0x30),
    KeyMapping::keyboard(117, 0x67),
    KeyMapping::keyboard(119, 0x48),
    KeyMapping::keyboard(121, 0x85),
    KeyMapping::keyboard(122, 0x90),
    KeyMapping::keyboard(123, 0x91),
    KeyMapping::keyboard(124, 0x89),
    KeyMapping::keyboard(125, 0xe3),
    KeyMapping::keyboard(126, 0xe7),
    KeyMapping::keyboard(127, 0x65),
    KeyMapping::keyboard(128, 0x78),
    KeyMapping::keyboard(129, 0x79),
    KeyMapping::keyboard(130, 0xa3),
    KeyMapping::keyboard(131, 0x7a),
    KeyMapping::keyboard(132, 0x77),
    KeyMapping::keyboard(133, 0x7c),
    KeyMapping::keyboard(134, 0x74),
    KeyMapping::keyboard(135, 0x7d),
    KeyMapping::keyboard(136, 0x7e),
    KeyMapping::keyboard(137, 0x7b),
    KeyMapping::keyboard(138, 0x75),
    KeyMapping::keyboard(139, 0x76),
    KeyMapping::consumer(140, 0x192), // calculator
    KeyMapping::consumer(142, 0x32),
    KeyMapping::consumer(143, 0x83),
    KeyMapping::consumer(150, 0x196),
    KeyMapping::consumer(155, 0x18a),
    KeyMapping::consumer(156, 0x22a),
    KeyMapping::consumer(157, 0x194),
    KeyMapping::consumer(158, 0x224),
    KeyMapping::consumer(159, 0x225),
    KeyMapping::consumer(161, 0xb8),
    KeyMapping::consumer(163, 0xb5),
    KeyMapping::consumer(164, 0xcd),
    KeyMapping::consumer(165, 0xb6),
    KeyMapping::consumer(166, 0xb7),
    KeyMapping::consumer(167, 0xb2),
    KeyMapping::consumer(168, 0xb4),
    KeyMapping::consumer(172, 0x223),
    KeyMapping::consumer(173, 0x227),
    KeyMapping::keyboard(179, 0xb6),
    KeyMapping::keyboard(180, 0xb7),
    KeyMapping::consumer(181, 0x201),
    KeyMapping::consumer(182, 0x279),
    KeyMapping::keyboard(183, 0x68), // F13..F24
    KeyMapping::keyboard(184, 0x69),
    KeyMapping::keyboard(185, 0x6a),
    KeyMapping::keyboard(186, 0x6b),
    KeyMapping::keyboard(187, 0x6c),
    KeyMapping::keyboard(188, 0x6d),
    KeyMapping::keyboard(189, 0x6e),
    KeyMapping::keyboard(190, 0x6f),
    KeyMapping::keyboard(191, 0x70),
    KeyMapping::keyboard(192, 0x71),
    KeyMapping::keyboard(193, 0x72),
    KeyMapping::keyboard(194, 0x73),
];

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum MappingError {
    #[error("evdev key/button code {0} has no unambiguous USB HID mapping")]
    UnsupportedEvdevCode(u16),
    #[error("USB HID usage page 0x{page:04x}, usage 0x{usage:04x} is unsupported on Linux")]
    UnsupportedHidUsage { page: u16, usage: u16 },
    #[error("invalid evdev key value {0}; expected 0 (up), 1 (down), or 2 (repeat)")]
    InvalidKeyValue(i32),
    #[error("SYN_DROPPED requires the capture device state to be reconciled")]
    SynchronizationLost,
    #[error("failed to map multitouch state")]
    InvalidTouchState,
}

pub fn evdev_key_to_hid(key: KeyCode) -> Result<HidUsage, MappingError> {
    KEY_MAP
        .iter()
        .find(|mapping| mapping.evdev == key.code())
        .copied()
        .map(KeyMapping::hid)
        .ok_or(MappingError::UnsupportedEvdevCode(key.code()))
}

pub fn hid_to_evdev_key(usage: HidUsage) -> Result<KeyCode, MappingError> {
    KEY_MAP
        .iter()
        .find(|mapping| mapping.page == usage.page.0 && mapping.usage == usage.usage.0)
        .map(|mapping| KeyCode::new(mapping.evdev))
        .ok_or(MappingError::UnsupportedHidUsage {
            page: usage.page.0,
            usage: usage.usage.0,
        })
}

pub fn evdev_button_to_pointer(key: KeyCode) -> Result<PointerButton, MappingError> {
    let button = match key {
        KeyCode::BTN_LEFT => 1,
        KeyCode::BTN_RIGHT => 2,
        KeyCode::BTN_MIDDLE => 3,
        KeyCode::BTN_SIDE => 4,
        KeyCode::BTN_EXTRA => 5,
        KeyCode::BTN_FORWARD => 6,
        KeyCode::BTN_BACK => 7,
        KeyCode::BTN_TASK => 8,
        _ => return Err(MappingError::UnsupportedEvdevCode(key.code())),
    };
    Ok(PointerButton(button))
}

pub fn pointer_button_to_evdev(button: PointerButton) -> Result<KeyCode, MappingError> {
    match button.0 {
        1 => Ok(KeyCode::BTN_LEFT),
        2 => Ok(KeyCode::BTN_RIGHT),
        3 => Ok(KeyCode::BTN_MIDDLE),
        4 => Ok(KeyCode::BTN_SIDE),
        5 => Ok(KeyCode::BTN_EXTRA),
        6 => Ok(KeyCode::BTN_FORWARD),
        7 => Ok(KeyCode::BTN_BACK),
        8 => Ok(KeyCode::BTN_TASK),
        usage => Err(MappingError::UnsupportedHidUsage { page: 0x09, usage }),
    }
}

pub(crate) fn mapped_evdev_keys() -> impl Iterator<Item = KeyCode> {
    KEY_MAP.iter().map(|mapping| KeyCode::new(mapping.evdev))
}

pub(crate) fn mapped_pointer_buttons() -> impl Iterator<Item = KeyCode> {
    [
        KeyCode::BTN_LEFT,
        KeyCode::BTN_RIGHT,
        KeyCode::BTN_MIDDLE,
        KeyCode::BTN_SIDE,
        KeyCode::BTN_EXTRA,
        KeyCode::BTN_FORWARD,
        KeyCode::BTN_BACK,
        KeyCode::BTN_TASK,
    ]
    .into_iter()
}

#[derive(Default)]
pub struct FrameAccumulator {
    transitions: Vec<CaptureTransition>,
    event_count: u64,
    dx: i64,
    dy: i64,
    wheel_legacy: i64,
    hwheel_legacy: i64,
    wheel_hi_res: i64,
    hwheel_hi_res: i64,
    saw_wheel_hi_res: bool,
    saw_hwheel_hi_res: bool,
}

impl fmt::Debug for FrameAccumulator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FrameAccumulator")
            .finish_non_exhaustive()
    }
}

impl FrameAccumulator {
    /// Adds one kernel event. A frame is returned only for a complete
    /// `SYN_REPORT`, including an empty frame used as an ownership boundary.
    pub fn push(&mut self, event: InputEvent) -> Result<Option<CaptureFrame>, MappingError> {
        match event.event_type() {
            EventType::SYNCHRONIZATION => match SynchronizationCode(event.code()) {
                SynchronizationCode::SYN_REPORT => Ok(Some(self.finish())),
                SynchronizationCode::SYN_DROPPED => Err(MappingError::SynchronizationLost),
                _ => Ok(None),
            },
            EventType::KEY => {
                let key = KeyCode::new(event.code());
                let state = match event.value() {
                    0 => KeyState::Released,
                    1 => KeyState::Pressed,
                    2 => KeyState::Repeat,
                    value => return Err(MappingError::InvalidKeyValue(value)),
                };
                let transition = if key.code() >= KeyCode::BTN_LEFT.code()
                    && key.code() <= KeyCode::BTN_TASK.code()
                {
                    CaptureTransition::Button {
                        button: evdev_button_to_pointer(key)?,
                        state,
                    }
                } else {
                    let usage = match evdev_key_to_hid(key) {
                        Ok(usage) => usage,
                        Err(MappingError::UnsupportedEvdevCode(_)) => return Ok(None),
                        Err(error) => return Err(error),
                    };
                    CaptureTransition::Key { usage, state }
                };
                self.transitions.push(transition);
                self.event_count = self.event_count.saturating_add(1);
                Ok(None)
            }
            EventType::RELATIVE => {
                let value = i64::from(event.value());
                let mapped = match RelativeAxisCode(event.code()) {
                    RelativeAxisCode::REL_X => {
                        self.dx += value;
                        true
                    }
                    RelativeAxisCode::REL_Y => {
                        self.dy += value;
                        true
                    }
                    RelativeAxisCode::REL_WHEEL => {
                        self.wheel_legacy += value;
                        true
                    }
                    RelativeAxisCode::REL_HWHEEL => {
                        self.hwheel_legacy += value;
                        true
                    }
                    RelativeAxisCode::REL_WHEEL_HI_RES => {
                        self.saw_wheel_hi_res = true;
                        self.wheel_hi_res += value;
                        true
                    }
                    RelativeAxisCode::REL_HWHEEL_HI_RES => {
                        self.saw_hwheel_hi_res = true;
                        self.hwheel_hi_res += value;
                        true
                    }
                    _ => false,
                };
                if mapped {
                    self.event_count = self.event_count.saturating_add(1);
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn finish(&mut self) -> CaptureFrame {
        let scroll_y = if self.saw_wheel_hi_res {
            self.wheel_hi_res
        } else {
            self.wheel_legacy * WHEEL_CLICK_UNITS
        };
        let scroll_x = if self.saw_hwheel_hi_res {
            self.hwheel_hi_res
        } else {
            self.hwheel_legacy * WHEEL_CLICK_UNITS
        };
        let frame = CaptureFrame {
            transitions: std::mem::take(&mut self.transitions),
            motion: MotionDelta {
                dx: self.dx,
                dy: self.dy,
                scroll_x,
                scroll_y,
            },
            touch_snapshot: None,
            event_count: self.event_count,
        };
        *self = Self::default();
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: EventType, code: u16, value: i32) -> InputEvent {
        InputEvent::new(kind.0, code, value)
    }

    #[test]
    fn common_keyboard_and_modifier_mappings_round_trip() {
        for (key, expected) in [
            (KeyCode::KEY_A, HidUsage::keyboard(0x04)),
            (KeyCode::KEY_ENTER, HidUsage::keyboard(0x28)),
            (KeyCode::KEY_LEFTCTRL, HidUsage::keyboard(0xe0)),
            (KeyCode::KEY_RIGHTMETA, HidUsage::keyboard(0xe7)),
            (KeyCode::KEY_VOLUMEUP, HidUsage::consumer(0xe9)),
        ] {
            assert_eq!(evdev_key_to_hid(key).unwrap(), expected);
            assert_eq!(hid_to_evdev_key(expected).unwrap(), key);
        }
    }

    #[test]
    fn key_map_is_bijective() {
        let mut evdev_codes = std::collections::BTreeSet::new();
        let mut hid_usages = std::collections::BTreeSet::new();
        for mapping in KEY_MAP {
            assert!(
                evdev_codes.insert(mapping.evdev),
                "duplicate evdev code {}",
                mapping.evdev
            );
            assert!(
                hid_usages.insert((mapping.page, mapping.usage)),
                "duplicate HID usage {:04x}:{:04x}",
                mapping.page,
                mapping.usage
            );
            assert_eq!(
                hid_to_evdev_key(mapping.hid()).unwrap(),
                KeyCode::new(mapping.evdev)
            );
        }
    }

    #[test]
    fn pointer_buttons_round_trip() {
        for number in 1..=8 {
            let button = PointerButton(number);
            assert_eq!(
                evdev_button_to_pointer(pointer_button_to_evdev(button).unwrap()),
                Ok(button)
            );
        }
    }

    #[test]
    fn unsupported_usage_is_a_specific_error() {
        assert_eq!(
            hid_to_evdev_key(HidUsage::new(HidUsagePage(0xff00), 7)),
            Err(MappingError::UnsupportedHidUsage {
                page: 0xff00,
                usage: 7,
            })
        );
    }

    #[test]
    fn unmapped_vendor_key_does_not_discard_mapped_events_or_end_the_frame() {
        let mut accumulator = FrameAccumulator::default();
        accumulator
            .push(InputEvent::new(
                EventType::KEY.0,
                KeyCode::KEY_BRIGHTNESSUP.code(),
                1,
            ))
            .unwrap();
        accumulator
            .push(InputEvent::new(EventType::KEY.0, KeyCode::KEY_A.code(), 1))
            .unwrap();
        let frame = accumulator
            .push(InputEvent::new(
                EventType::SYNCHRONIZATION.0,
                SynchronizationCode::SYN_REPORT.0,
                0,
            ))
            .unwrap()
            .unwrap();

        assert_eq!(
            frame.transitions,
            vec![CaptureTransition::Key {
                usage: HidUsage::keyboard(4),
                state: KeyState::Pressed,
            }]
        );
        assert_eq!(frame.event_count, 1);
    }

    #[test]
    fn emits_only_complete_frames_and_prefers_high_resolution_wheel() {
        let mut accumulator = FrameAccumulator::default();
        assert_eq!(
            accumulator
                .push(event(EventType::RELATIVE, RelativeAxisCode::REL_X.0, 4))
                .unwrap(),
            None
        );
        accumulator
            .push(event(EventType::RELATIVE, RelativeAxisCode::REL_WHEEL.0, 1))
            .unwrap();
        accumulator
            .push(event(
                EventType::RELATIVE,
                RelativeAxisCode::REL_WHEEL_HI_RES.0,
                30,
            ))
            .unwrap();
        let frame = accumulator
            .push(event(
                EventType::SYNCHRONIZATION,
                SynchronizationCode::SYN_REPORT.0,
                0,
            ))
            .unwrap()
            .unwrap();
        assert_eq!(
            frame.motion,
            MotionDelta {
                dx: 4,
                scroll_y: 30,
                ..MotionDelta::default()
            }
        );
        assert_eq!(frame.event_count, 3);
    }

    #[test]
    fn legacy_wheel_is_normalized_to_high_resolution_units() {
        let mut accumulator = FrameAccumulator::default();
        accumulator
            .push(event(
                EventType::RELATIVE,
                RelativeAxisCode::REL_HWHEEL.0,
                -2,
            ))
            .unwrap();
        let frame = accumulator
            .push(event(
                EventType::SYNCHRONIZATION,
                SynchronizationCode::SYN_REPORT.0,
                0,
            ))
            .unwrap()
            .unwrap();
        assert_eq!(frame.motion.scroll_x, -240);
    }
}
