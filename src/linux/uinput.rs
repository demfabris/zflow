use std::{collections::BTreeSet, ffi::CString, io};

use evdev::{
    AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode, PropType, RelativeAxisCode,
    uinput::VirtualDevice,
};
use thiserror::Error;

use crate::core::{HidUsage, MotionDelta, PointerButton};

use super::{
    MappingError, VirtualDeviceRole, ZFLOW_DEVICE_VERSION, ZFLOW_VENDOR_ID, hid_to_evdev_key,
    mapped_evdev_keys, mapped_pointer_buttons, pointer_button_to_evdev,
};

const WHEEL_CLICK_UNITS: i64 = 120;

#[derive(Debug, Error)]
pub enum InjectionError {
    #[error(transparent)]
    Unsupported(#[from] MappingError),
    #[error("{axis} value {value} does not fit the Linux input_event i32 field")]
    ValueOutOfRange { axis: &'static str, value: i64 },
    #[error("cannot repeat USB HID usage {usage:?} because it is not held")]
    RepeatOfReleasedKey { usage: HidUsage },
    #[error("failed to create {role:?} virtual device: {source}")]
    Create {
        role: VirtualDeviceRole,
        #[source]
        source: io::Error,
    },
    #[error("failed to emit {role:?} virtual input: {source}")]
    Emit {
        role: VirtualDeviceRole,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Error)]
#[error("failed to release {count} virtual input device(s)")]
pub struct ReleaseAllError {
    count: usize,
    pub failures: Vec<InjectionError>,
}

impl ReleaseAllError {
    fn new(failures: Vec<InjectionError>) -> Self {
        Self {
            count: failures.len(),
            failures,
        }
    }
}

fn build_virtual_device(
    role: VirtualDeviceRole,
    keys: &evdev::AttributeSetRef<KeyCode>,
    relative_axes: Option<&evdev::AttributeSetRef<RelativeAxisCode>>,
    properties: Option<&evdev::AttributeSetRef<PropType>>,
) -> Result<VirtualDevice, InjectionError> {
    let phys = CString::new(role.physical_path()).expect("stable zflow phys has no NUL");
    let result = (|| {
        let mut builder = VirtualDevice::builder()?
            .name(role.name())
            .input_id(InputId::new(
                BusType::BUS_VIRTUAL,
                ZFLOW_VENDOR_ID,
                role.product_id(),
                ZFLOW_DEVICE_VERSION,
            ))
            .with_phys(&phys)?
            .with_keys(keys)?;
        if let Some(axes) = relative_axes {
            builder = builder.with_relative_axes(axes)?;
        }
        if let Some(properties) = properties {
            builder = builder.with_properties(properties)?;
        }
        builder.build()
    })();
    result.map_err(|source| InjectionError::Create { role, source })
}

#[derive(Debug)]
pub struct VirtualKeyboard {
    device: VirtualDevice,
    held: BTreeSet<HidUsage>,
}

impl VirtualKeyboard {
    pub fn create() -> Result<Self, InjectionError> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for key in mapped_evdev_keys() {
            keys.insert(key);
        }
        let device = build_virtual_device(VirtualDeviceRole::Keyboard, &keys, None, None)?;
        Ok(Self {
            device,
            held: BTreeSet::new(),
        })
    }

    pub fn set_key(&mut self, usage: HidUsage, pressed: bool) -> Result<(), InjectionError> {
        let key = hid_to_evdev_key(usage)?;
        if self.held.contains(&usage) == pressed {
            return Ok(());
        }
        self.emit(&[InputEvent::new(
            EventType::KEY.0,
            key.code(),
            i32::from(pressed),
        )])?;
        if pressed {
            self.held.insert(usage);
        } else {
            self.held.remove(&usage);
        }
        Ok(())
    }

    pub fn repeat_key(&mut self, usage: HidUsage) -> Result<(), InjectionError> {
        if !self.held.contains(&usage) {
            return Err(InjectionError::RepeatOfReleasedKey { usage });
        }
        let key = hid_to_evdev_key(usage)?;
        self.emit(&[InputEvent::new(EventType::KEY.0, key.code(), 2)])
    }

    pub fn held(&self) -> &BTreeSet<HidUsage> {
        &self.held
    }

    pub fn release_all(&mut self) -> Result<(), InjectionError> {
        if self.held.is_empty() {
            return Ok(());
        }
        let events = self
            .held
            .iter()
            .map(|usage| {
                hid_to_evdev_key(*usage).map(|key| InputEvent::new(EventType::KEY.0, key.code(), 0))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.emit(&events)?;
        self.held.clear();
        Ok(())
    }

    fn emit(&mut self, events: &[InputEvent]) -> Result<(), InjectionError> {
        self.device
            .emit(events)
            .map_err(|source| InjectionError::Emit {
                role: VirtualDeviceRole::Keyboard,
                source,
            })
    }
}

impl Drop for VirtualKeyboard {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

#[derive(Debug)]
pub struct VirtualPointer {
    device: VirtualDevice,
    held: BTreeSet<PointerButton>,
    legacy_wheel_remainder: i64,
    legacy_hwheel_remainder: i64,
}

impl VirtualPointer {
    pub fn create() -> Result<Self, InjectionError> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for button in mapped_pointer_buttons() {
            keys.insert(button);
        }
        let mut axes = AttributeSet::<RelativeAxisCode>::new();
        for axis in [
            RelativeAxisCode::REL_X,
            RelativeAxisCode::REL_Y,
            RelativeAxisCode::REL_WHEEL,
            RelativeAxisCode::REL_HWHEEL,
            RelativeAxisCode::REL_WHEEL_HI_RES,
            RelativeAxisCode::REL_HWHEEL_HI_RES,
        ] {
            axes.insert(axis);
        }
        let mut properties = AttributeSet::<PropType>::new();
        properties.insert(PropType::POINTER);
        let device = build_virtual_device(
            VirtualDeviceRole::Pointer,
            &keys,
            Some(&axes),
            Some(&properties),
        )?;
        Ok(Self {
            device,
            held: BTreeSet::new(),
            legacy_wheel_remainder: 0,
            legacy_hwheel_remainder: 0,
        })
    }

    pub fn set_button(
        &mut self,
        button: PointerButton,
        pressed: bool,
    ) -> Result<(), InjectionError> {
        let key = pointer_button_to_evdev(button)?;
        if self.held.contains(&button) == pressed {
            return Ok(());
        }
        self.emit(&[InputEvent::new(
            EventType::KEY.0,
            key.code(),
            i32::from(pressed),
        )])?;
        if pressed {
            self.held.insert(button);
        } else {
            self.held.remove(&button);
        }
        Ok(())
    }

    /// Emits one complete pointer report. High-resolution wheel events are
    /// always paired with the accumulated legacy detent event Linux requires.
    pub fn motion(&mut self, motion: MotionDelta) -> Result<(), InjectionError> {
        let mut events = Vec::with_capacity(6);
        push_relative(&mut events, RelativeAxisCode::REL_X, "pointer x", motion.dx)?;
        push_relative(&mut events, RelativeAxisCode::REL_Y, "pointer y", motion.dy)?;
        push_relative(
            &mut events,
            RelativeAxisCode::REL_HWHEEL_HI_RES,
            "horizontal high-resolution wheel",
            motion.scroll_x,
        )?;
        push_relative(
            &mut events,
            RelativeAxisCode::REL_WHEEL_HI_RES,
            "vertical high-resolution wheel",
            motion.scroll_y,
        )?;

        let legacy_x = legacy_wheel_delta(&mut self.legacy_hwheel_remainder, motion.scroll_x);
        let legacy_y = legacy_wheel_delta(&mut self.legacy_wheel_remainder, motion.scroll_y);
        push_relative(
            &mut events,
            RelativeAxisCode::REL_HWHEEL,
            "horizontal legacy wheel",
            legacy_x,
        )?;
        push_relative(
            &mut events,
            RelativeAxisCode::REL_WHEEL,
            "vertical legacy wheel",
            legacy_y,
        )?;
        if events.is_empty() {
            return Ok(());
        }
        self.emit(&events)
    }

    pub fn held(&self) -> &BTreeSet<PointerButton> {
        &self.held
    }

    pub fn release_all(&mut self) -> Result<(), InjectionError> {
        if self.held.is_empty() {
            return Ok(());
        }
        let events = self
            .held
            .iter()
            .map(|button| {
                pointer_button_to_evdev(*button)
                    .map(|key| InputEvent::new(EventType::KEY.0, key.code(), 0))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.emit(&events)?;
        self.held.clear();
        Ok(())
    }

    fn emit(&mut self, events: &[InputEvent]) -> Result<(), InjectionError> {
        self.device
            .emit(events)
            .map_err(|source| InjectionError::Emit {
                role: VirtualDeviceRole::Pointer,
                source,
            })
    }
}

impl Drop for VirtualPointer {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

#[derive(Debug)]
pub struct VirtualInput {
    pub keyboard: VirtualKeyboard,
    pub pointer: VirtualPointer,
}

impl VirtualInput {
    pub fn create() -> Result<Self, InjectionError> {
        // If pointer creation fails, keyboard drops immediately and uinput
        // removes it; callers never observe half a virtual device pair.
        let keyboard = VirtualKeyboard::create()?;
        let pointer = VirtualPointer::create()?;
        Ok(Self { keyboard, pointer })
    }

    pub fn release_all(&mut self) -> Result<(), ReleaseAllError> {
        let mut failures = Vec::new();
        if let Err(error) = self.keyboard.release_all() {
            failures.push(error);
        }
        if let Err(error) = self.pointer.release_all() {
            failures.push(error);
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(ReleaseAllError::new(failures))
        }
    }

    pub fn suspend(&mut self) -> Result<(), ReleaseAllError> {
        self.release_all()
    }

    pub fn device_removed(&mut self) -> Result<(), ReleaseAllError> {
        self.release_all()
    }
}

impl Drop for VirtualInput {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

fn push_relative(
    events: &mut Vec<InputEvent>,
    axis: RelativeAxisCode,
    axis_name: &'static str,
    value: i64,
) -> Result<(), InjectionError> {
    if value == 0 {
        return Ok(());
    }
    let value = i32::try_from(value).map_err(|_| InjectionError::ValueOutOfRange {
        axis: axis_name,
        value,
    })?;
    events.push(InputEvent::new(EventType::RELATIVE.0, axis.0, value));
    Ok(())
}

fn legacy_wheel_delta(remainder: &mut i64, high_resolution: i64) -> i64 {
    let total = *remainder + high_resolution;
    let clicks = total / WHEEL_CLICK_UNITS;
    *remainder = total % WHEEL_CLICK_UNITS;
    clicks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_wheel_accumulates_fractional_detents_in_both_directions() {
        let mut remainder = 0;
        assert_eq!(legacy_wheel_delta(&mut remainder, 30), 0);
        assert_eq!(remainder, 30);
        assert_eq!(legacy_wheel_delta(&mut remainder, 90), 1);
        assert_eq!(remainder, 0);
        assert_eq!(legacy_wheel_delta(&mut remainder, -60), 0);
        assert_eq!(legacy_wheel_delta(&mut remainder, -60), -1);
        assert_eq!(remainder, 0);
    }

    #[test]
    fn pointer_value_range_error_names_the_axis() {
        let mut events = Vec::new();
        let error =
            push_relative(&mut events, RelativeAxisCode::REL_X, "pointer x", i64::MAX).unwrap_err();
        assert!(matches!(
            error,
            InjectionError::ValueOutOfRange {
                axis: "pointer x",
                value: i64::MAX
            }
        ));
    }
}
