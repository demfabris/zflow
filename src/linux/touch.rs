use std::{io, sync::Once};

use evdev::{AbsoluteAxisCode, EventType, InputEvent, SynchronizationCode, raw_stream::RawDevice};

use crate::{
    capture::MAX_TOUCHPAD_CONTACTS,
    core::{ContactId, SourceDimensions, TouchContact, TouchState, TouchTool},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchAxisRange {
    minimum: i32,
    maximum: i32,
    /// Units per millimetre; 0 when the driver does not report it.
    resolution: i32,
}

impl TouchAxisRange {
    pub fn new(minimum: i32, maximum: i32, resolution: i32) -> Option<Self> {
        (maximum > minimum).then_some(Self {
            minimum,
            maximum,
            resolution,
        })
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
    /// Device units per 100 mm on x and y.
    units_per_100mm: [i64; 2],
    slots: [SlotState; MAX_TOUCHPAD_CONTACTS],
    current_slot: Option<usize>,
    dirty: bool,
    event_count: u64,
}

impl TouchAccumulator {
    pub fn from_device(device: &RawDevice) -> io::Result<Option<Self>> {
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
                    x = TouchAxisRange::new(info.minimum(), info.maximum(), info.resolution())
                }
                AbsoluteAxisCode::ABS_MT_POSITION_Y => {
                    y = TouchAxisRange::new(info.minimum(), info.maximum(), info.resolution())
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
        let units_per_100mm = if x.resolution > 0 && y.resolution > 0 {
            [x.resolution, y.resolution].map(|resolution| 100 * i64::from(resolution))
        } else {
            static LOGGED: Once = Once::new();
            LOGGED.call_once(|| {
                tracing::warn!("touchpad reports no resolution; assuming it is 100 mm wide");
            });
            // Square units keep the pad's aspect ratio.
            [i64::from(x.extent()); 2]
        };
        Self {
            x,
            y,
            units_per_100mm,
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
                // Slots changed while events were lost. Fail rather than keep
                // contacts the kernel may already have lifted.
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
        let [x_scale, y_scale] = self.units_per_100mm;
        let dimensions = Some(SourceDimensions {
            width: hundredths_mm(self.x.extent().into(), x_scale) as u32,
            height: hundredths_mm(self.y.extent().into(), y_scale) as u32,
        });
        TouchState::new(self.slots.iter().filter_map(|slot| {
            slot.tracking_id.map(|id| TouchContact {
                id,
                x: hundredths_mm(slot.x.into(), x_scale),
                y: hundredths_mm(slot.y.into(), y_scale),
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

/// Converts non-negative device units to hundredths of a millimetre, rounded.
fn hundredths_mm(units: i64, units_per_100mm: i64) -> i32 {
    let hundredths = (units * 10_000 + units_per_100mm / 2) / units_per_100mm;
    hundredths.min(i32::MAX.into()) as i32
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
            TouchAxisRange::new(100, 1_399, 13).unwrap(),
            TouchAxisRange::new(50, 772, 10).unwrap(),
        )
    }

    fn land(touch: &mut TouchAccumulator, x: i32, y: i32) -> TouchContact {
        for (axis, value) in [
            (AbsoluteAxisCode::ABS_MT_SLOT, 0),
            (AbsoluteAxisCode::ABS_MT_TRACKING_ID, 41),
            (AbsoluteAxisCode::ABS_MT_POSITION_X, x),
            (AbsoluteAxisCode::ABS_MT_POSITION_Y, y),
        ] {
            touch.push(abs(axis, value)).unwrap();
        }
        let (state, _) = touch.push(report()).unwrap().unwrap();
        state.get(ContactId(41)).unwrap().clone()
    }

    #[test]
    fn positions_and_size_use_the_axis_resolution() {
        // 13 units/mm across and 10 down: the pad is 99.92 x 72.2 mm.
        let contact = land(&mut accumulator(), 750, 411);
        assert_eq!((contact.x, contact.y), (5_000, 3_610));
        assert_eq!(
            contact.source_dimensions,
            Some(SourceDimensions {
                width: 9_992,
                height: 7_220,
            })
        );
    }

    #[test]
    fn missing_resolution_assumes_100_mm_wide_with_square_units() {
        for y_resolution in [0, 20] {
            let mut touch = TouchAccumulator::new(
                TouchAxisRange::new(0, 2_000, 0).unwrap(),
                TouchAxisRange::new(0, 1_000, y_resolution).unwrap(),
            );
            let contact = land(&mut touch, 1_000, 500);
            assert_eq!((contact.x, contact.y), (5_000, 2_500));
            assert_eq!(
                contact.source_dimensions,
                Some(SourceDimensions {
                    width: 10_000,
                    height: 5_000,
                })
            );
        }
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
        assert_eq!((contact.x, contact.y), (5_000, 3_610));
        assert!(touch.push(report()).unwrap().is_none());

        touch
            .push(abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, 500))
            .unwrap();
        let (moved, _) = touch.push(report()).unwrap().unwrap();
        assert_eq!(moved.get(ContactId(41)).unwrap().y, 4_500);

        touch
            .push(abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1))
            .unwrap();
        let (lifted, _) = touch.push(report()).unwrap().unwrap();
        assert!(lifted.is_empty());
    }

    #[test]
    fn dropped_events_fail_instead_of_keeping_stale_contacts() {
        let mut touch = accumulator();
        touch.push(abs(AbsoluteAxisCode::ABS_MT_SLOT, 0)).unwrap();
        touch
            .push(abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 41))
            .unwrap();
        touch.push(report()).unwrap().unwrap();
        let dropped = InputEvent::new(
            EventType::SYNCHRONIZATION.0,
            SynchronizationCode::SYN_DROPPED.0,
            0,
        );
        assert!(touch.push(dropped).is_err());
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
