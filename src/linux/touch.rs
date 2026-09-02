use std::io;

use evdev::{AbsoluteAxisCode, Device, EventType, InputEvent, SynchronizationCode};

use crate::{
    capture::MAX_TOUCHPAD_CONTACTS,
    core::{ContactId, SourceDimensions, TouchContact, TouchState, TouchTool},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchAxisRange {
    minimum: i32,
    maximum: i32,
}

impl TouchAxisRange {
    pub fn new(minimum: i32, maximum: i32) -> Option<Self> {
        (maximum > minimum).then_some(Self { minimum, maximum })
    }

    fn normalize(self, value: i32) -> i32 {
        value.clamp(self.minimum, self.maximum) - self.minimum
    }

    fn extent(self) -> u32 {
        u32::try_from(self.maximum - self.minimum).expect("validated touch axis extent")
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct SlotState {
    tracking_id: Option<ContactId>,
    x: i32,
    y: i32,
}

/// Converts Linux type-B multitouch slot updates into complete contact states.
/// A snapshot is emitted only at SYN_REPORT, preserving kernel frame semantics.
#[derive(Debug)]
pub struct TouchAccumulator {
    x: TouchAxisRange,
    y: TouchAxisRange,
    slots: [SlotState; MAX_TOUCHPAD_CONTACTS],
    current_slot: Option<usize>,
    dirty: bool,
    event_count: u64,
}

impl TouchAccumulator {
    pub fn from_device(device: &Device) -> io::Result<Option<Self>> {
        let Some(axes) = device.supported_absolute_axes() else {
            return Ok(None);
        };
        for required in [
            AbsoluteAxisCode::ABS_MT_SLOT,
            AbsoluteAxisCode::ABS_MT_TRACKING_ID,
            AbsoluteAxisCode::ABS_MT_POSITION_X,
            AbsoluteAxisCode::ABS_MT_POSITION_Y,
        ] {
            if !axes.contains(required) {
                return Ok(None);
            }
        }

        let mut x = None;
        let mut y = None;
        for (axis, info) in device.get_absinfo()? {
            match axis {
                AbsoluteAxisCode::ABS_MT_POSITION_X => {
                    x = TouchAxisRange::new(info.minimum(), info.maximum())
                }
                AbsoluteAxisCode::ABS_MT_POSITION_Y => {
                    y = TouchAxisRange::new(info.minimum(), info.maximum())
                }
                _ => {}
            }
        }
        Ok(match (x, y) {
            (Some(x), Some(y)) => Some(Self::new(x, y)),
            _ => None,
        })
    }

    pub fn new(x: TouchAxisRange, y: TouchAxisRange) -> Self {
        Self {
            x,
            y,
            slots: [SlotState::default(); MAX_TOUCHPAD_CONTACTS],
            current_slot: Some(0),
            dirty: false,
            event_count: 0,
        }
    }

    /// Returns a complete state and the number of mapped events at SYN_REPORT.
    pub fn push(&mut self, event: InputEvent) -> io::Result<Option<(TouchState, u64)>> {
        if event.event_type() == EventType::SYNCHRONIZATION {
            return match SynchronizationCode(event.code()) {
                SynchronizationCode::SYN_REPORT if self.dirty => {
                    let state = self.snapshot()?;
                    let count = std::mem::take(&mut self.event_count);
                    self.dirty = false;
                    Ok(Some((state, count)))
                }
                SynchronizationCode::SYN_REPORT => Ok(None),
                SynchronizationCode::SYN_DROPPED => {
                    Err(io::Error::other("multitouch synchronization was lost"))
                }
                _ => Ok(None),
            };
        }
        if event.event_type() != EventType::ABSOLUTE {
            return Ok(None);
        }

        match AbsoluteAxisCode(event.code()) {
            AbsoluteAxisCode::ABS_MT_SLOT => {
                self.current_slot = usize::try_from(event.value())
                    .ok()
                    .filter(|slot| *slot < MAX_TOUCHPAD_CONTACTS);
                self.mapped();
            }
            AbsoluteAxisCode::ABS_MT_TRACKING_ID => {
                if let Some(slot) = self.current_slot {
                    self.slots[slot].tracking_id = if event.value() < 0 {
                        None
                    } else {
                        Some(ContactId(event.value() as u32))
                    };
                }
                self.mapped();
            }
            AbsoluteAxisCode::ABS_MT_POSITION_X => {
                if let Some(slot) = self.current_slot {
                    self.slots[slot].x = self.x.normalize(event.value());
                }
                self.mapped();
            }
            AbsoluteAxisCode::ABS_MT_POSITION_Y => {
                if let Some(slot) = self.current_slot {
                    self.slots[slot].y = self.y.normalize(event.value());
                }
                self.mapped();
            }
            _ => {}
        }
        Ok(None)
    }

    fn mapped(&mut self) {
        self.dirty = true;
        self.event_count = self.event_count.saturating_add(1);
    }

    fn snapshot(&self) -> io::Result<TouchState> {
        let dimensions = Some(SourceDimensions {
            width: self.x.extent(),
            height: self.y.extent(),
        });
        TouchState::new(self.slots.iter().filter_map(|slot| {
            slot.tracking_id.map(|id| TouchContact {
                id,
                x: slot.x,
                y: slot.y,
                pressure: None,
                major: None,
                minor: None,
                orientation_millidegrees: None,
                tool: TouchTool::Finger,
                source_dimensions: dimensions,
            })
        }))
        .map_err(|id| io::Error::other(format!("duplicate active touch tracking id {}", id.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(axis: AbsoluteAxisCode, value: i32) -> InputEvent {
        InputEvent::new(EventType::ABSOLUTE.0, axis.0, value)
    }

    fn report() -> InputEvent {
        InputEvent::new(
            EventType::SYNCHRONIZATION.0,
            SynchronizationCode::SYN_REPORT.0,
            0,
        )
    }

    fn accumulator() -> TouchAccumulator {
        TouchAccumulator::new(
            TouchAxisRange::new(100, 1_399).unwrap(),
            TouchAxisRange::new(50, 772).unwrap(),
        )
    }

    #[test]
    fn type_b_contact_lifecycle_emits_complete_states_only_at_report_boundaries() {
        let mut touch = accumulator();
        assert!(
            touch
                .push(abs(AbsoluteAxisCode::ABS_MT_SLOT, 0))
                .unwrap()
                .is_none()
        );
        touch
            .push(abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 41))
            .unwrap();
        touch
            .push(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 750))
            .unwrap();
        touch
            .push(abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, 411))
            .unwrap();
        let (landed, count) = touch.push(report()).unwrap().unwrap();
        assert_eq!(count, 4);
        assert_eq!(landed.len(), 1);
        let contact = landed.get(ContactId(41)).unwrap();
        assert_eq!((contact.x, contact.y), (650, 361));
        assert_eq!(
            contact.source_dimensions,
            Some(SourceDimensions {
                width: 1_299,
                height: 722,
            })
        );
        assert!(touch.push(report()).unwrap().is_none());

        touch
            .push(abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, 500))
            .unwrap();
        let (moved, _) = touch.push(report()).unwrap().unwrap();
        assert_eq!(moved.get(ContactId(41)).unwrap().y, 450);

        touch
            .push(abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1))
            .unwrap();
        let (lifted, _) = touch.push(report()).unwrap().unwrap();
        assert!(lifted.is_empty());
    }

    #[test]
    fn contacts_in_unsupported_high_slots_are_ignored_without_corrupting_low_slots() {
        let mut touch = accumulator();
        touch.push(abs(AbsoluteAxisCode::ABS_MT_SLOT, 7)).unwrap();
        touch
            .push(abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 99))
            .unwrap();
        touch
            .push(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 500))
            .unwrap();
        let (state, _) = touch.push(report()).unwrap().unwrap();
        assert!(state.is_empty());
    }
}
