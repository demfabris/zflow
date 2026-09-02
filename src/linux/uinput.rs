use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    io,
};

use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode,
    PropType, RelativeAxisCode, UinputAbsSetup, uinput::VirtualDevice,
};
use thiserror::Error;

use crate::core::{ContactId, HidUsage, MotionDelta, PointerButton, TouchContact, TouchState};

use super::{
    MAX_TOUCHPAD_CONTACTS, MappingError, VirtualDeviceRole, ZFLOW_DEVICE_VERSION, ZFLOW_VENDOR_ID,
    hid_to_evdev_key, mapped_evdev_keys, mapped_pointer_buttons, pointer_button_to_evdev,
};

const WHEEL_CLICK_UNITS: i64 = 120;
const TOUCHPAD_MAX_X: i32 = 2_999;
const TOUCHPAD_MAX_Y: i32 = 2_199;
const TOUCHPAD_RESOLUTION: i32 = 30;

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
    #[error("touch state has {actual} contacts; virtual touchpad supports at most {maximum}")]
    TooManyTouchContacts { actual: usize, maximum: usize },
    #[error("touch input arrived while the experimental virtual touchpad is disabled")]
    TouchpadDisabled,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TargetContact {
    slot: usize,
    tracking_id: i32,
    x: i32,
    y: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TouchpadState {
    contacts: BTreeMap<ContactId, TargetContact>,
    next_tracking_id: i32,
}

impl Default for TouchpadState {
    fn default() -> Self {
        Self {
            contacts: BTreeMap::new(),
            next_tracking_id: 1,
        }
    }
}

#[derive(Debug)]
pub struct VirtualTouchpad {
    device: VirtualDevice,
    state: TouchpadState,
}

impl VirtualTouchpad {
    pub fn create() -> Result<Self, InjectionError> {
        let role = VirtualDeviceRole::Touchpad;
        let mut keys = AttributeSet::<KeyCode>::new();
        for key in [
            KeyCode::BTN_TOUCH,
            KeyCode::BTN_TOOL_FINGER,
            KeyCode::BTN_TOOL_DOUBLETAP,
            KeyCode::BTN_TOOL_TRIPLETAP,
            KeyCode::BTN_TOOL_QUADTAP,
            KeyCode::BTN_LEFT,
        ] {
            keys.insert(key);
        }
        let mut properties = AttributeSet::<PropType>::new();
        properties.insert(PropType::POINTER);
        properties.insert(PropType::BUTTONPAD);
        let phys = CString::new(role.physical_path()).expect("stable zflow phys has no NUL");
        let axis = |code, maximum| {
            UinputAbsSetup::new(code, AbsInfo::new(0, 0, maximum, 0, 0, TOUCHPAD_RESOLUTION))
        };
        let result = (|| {
            VirtualDevice::builder()?
                .name(role.name())
                .input_id(InputId::new(
                    BusType::BUS_VIRTUAL,
                    ZFLOW_VENDOR_ID,
                    role.product_id(),
                    ZFLOW_DEVICE_VERSION,
                ))
                .with_phys(&phys)?
                .with_properties(&properties)?
                .with_keys(&keys)?
                .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_X, TOUCHPAD_MAX_X))?
                .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_Y, TOUCHPAD_MAX_Y))?
                .with_absolute_axis(&UinputAbsSetup::new(
                    AbsoluteAxisCode::ABS_MT_SLOT,
                    AbsInfo::new(0, 0, (MAX_TOUCHPAD_CONTACTS - 1) as i32, 0, 0, 0),
                ))?
                .with_absolute_axis(&UinputAbsSetup::new(
                    AbsoluteAxisCode::ABS_MT_TRACKING_ID,
                    AbsInfo::new(0, 0, 65_535, 0, 0, 0),
                ))?
                .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_MT_POSITION_X, TOUCHPAD_MAX_X))?
                .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_MT_POSITION_Y, TOUCHPAD_MAX_Y))?
                .build()
        })();
        let device = result.map_err(|source| InjectionError::Create { role, source })?;
        Ok(Self {
            device,
            state: TouchpadState::default(),
        })
    }

    pub fn replace(&mut self, touch: &TouchState) -> Result<(), InjectionError> {
        for (events, next) in plan_touchpad_events(&self.state, touch)? {
            if !events.is_empty() {
                self.device
                    .emit(&events)
                    .map_err(|source| InjectionError::Emit {
                        role: VirtualDeviceRole::Touchpad,
                        source,
                    })?;
            }
            self.state = next;
        }
        Ok(())
    }

    pub fn release_all(&mut self) -> Result<(), InjectionError> {
        self.replace(&TouchState::default())
    }
}

impl Drop for VirtualTouchpad {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

fn plan_touchpad_events(
    previous: &TouchpadState,
    touch: &TouchState,
) -> Result<Vec<(Vec<InputEvent>, TouchpadState)>, InjectionError> {
    if touch.len() > MAX_TOUCHPAD_CONTACTS {
        return Err(InjectionError::TooManyTouchContacts {
            actual: touch.len(),
            maximum: MAX_TOUCHPAD_CONTACTS,
        });
    }

    let new_contacts = touch
        .iter()
        .filter(|contact| !previous.contacts.contains_key(&contact.id))
        .count();
    let unused_slots = MAX_TOUCHPAD_CONTACTS - previous.contacts.len();
    if new_contacts <= unused_slots {
        return Ok(vec![plan_touchpad_report(previous, touch)]);
    }

    let surviving_touch = TouchState::new(
        touch
            .iter()
            .filter(|contact| previous.contacts.contains_key(&contact.id))
            .cloned(),
    )
    .expect("surviving contacts retain unique ids");
    let (release_events, released) = plan_touchpad_report(previous, &surviving_touch);
    let (landing_events, next) = plan_touchpad_report(&released, touch);
    Ok(vec![(release_events, released), (landing_events, next)])
}

fn plan_touchpad_report(
    previous: &TouchpadState,
    touch: &TouchState,
) -> (Vec<InputEvent>, TouchpadState) {
    let mut next = TouchpadState {
        contacts: BTreeMap::new(),
        next_tracking_id: previous.next_tracking_id,
    };
    let mut used_slots = [false; MAX_TOUCHPAD_CONTACTS];
    for existing in previous.contacts.values() {
        used_slots[existing.slot] = true;
    }
    for contact in touch.iter() {
        let target = if let Some(existing) = previous.contacts.get(&contact.id) {
            TargetContact {
                x: scale_touch_axis(contact, true),
                y: scale_touch_axis(contact, false),
                ..*existing
            }
        } else {
            let slot = used_slots
                .iter()
                .position(|used| !used)
                .expect("report planner guarantees a free touchpad slot");
            used_slots[slot] = true;
            let tracking_id = next.next_tracking_id;
            next.next_tracking_id = if tracking_id == 65_535 {
                1
            } else {
                tracking_id + 1
            };
            TargetContact {
                slot,
                tracking_id,
                x: scale_touch_axis(contact, true),
                y: scale_touch_axis(contact, false),
            }
        };
        next.contacts.insert(contact.id, target);
    }

    if next.contacts == previous.contacts {
        return (Vec::new(), next);
    }

    let mut events = Vec::new();
    for (id, old) in &previous.contacts {
        if !next.contacts.contains_key(id) {
            push_absolute(&mut events, AbsoluteAxisCode::ABS_MT_SLOT, old.slot as i32);
            push_absolute(&mut events, AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1);
        }
    }
    for (id, current) in &next.contacts {
        let old = previous.contacts.get(id);
        if old == Some(current) {
            continue;
        }
        push_absolute(
            &mut events,
            AbsoluteAxisCode::ABS_MT_SLOT,
            current.slot as i32,
        );
        if old.is_none() {
            push_absolute(
                &mut events,
                AbsoluteAxisCode::ABS_MT_TRACKING_ID,
                current.tracking_id,
            );
        }
        if old.is_none_or(|old| old.x != current.x) {
            push_absolute(&mut events, AbsoluteAxisCode::ABS_MT_POSITION_X, current.x);
        }
        if old.is_none_or(|old| old.y != current.y) {
            push_absolute(&mut events, AbsoluteAxisCode::ABS_MT_POSITION_Y, current.y);
        }
    }

    let old_count = previous.contacts.len();
    let new_count = next.contacts.len();
    let old_tool = touch_tool_key(old_count);
    let new_tool = touch_tool_key(new_count);
    if old_tool != new_tool {
        if let Some(key) = old_tool {
            push_key(&mut events, key, false);
        }
        if let Some(key) = new_tool {
            push_key(&mut events, key, true);
        }
    }
    if (old_count == 0) != (new_count == 0) {
        push_key(&mut events, KeyCode::BTN_TOUCH, new_count != 0);
    }
    if let Some(primary) = next.contacts.values().min_by_key(|contact| contact.slot) {
        push_absolute(&mut events, AbsoluteAxisCode::ABS_X, primary.x);
        push_absolute(&mut events, AbsoluteAxisCode::ABS_Y, primary.y);
    }
    (events, next)
}

fn scale_touch_axis(contact: &TouchContact, horizontal: bool) -> i32 {
    let (value, maximum, source_extent) = if horizontal {
        (
            contact.x,
            TOUCHPAD_MAX_X,
            contact.source_dimensions.map(|size| size.width),
        )
    } else {
        (
            contact.y,
            TOUCHPAD_MAX_Y,
            contact.source_dimensions.map(|size| size.height),
        )
    };
    let Some(source_extent) = source_extent.filter(|extent| *extent > 0) else {
        return value.clamp(0, maximum);
    };
    let value = i64::from(value.clamp(0, i32::try_from(source_extent).unwrap_or(i32::MAX)));
    ((value * i64::from(maximum) + i64::from(source_extent) / 2) / i64::from(source_extent)) as i32
}

fn touch_tool_key(count: usize) -> Option<KeyCode> {
    match count {
        0 => None,
        1 => Some(KeyCode::BTN_TOOL_FINGER),
        2 => Some(KeyCode::BTN_TOOL_DOUBLETAP),
        3 => Some(KeyCode::BTN_TOOL_TRIPLETAP),
        _ => Some(KeyCode::BTN_TOOL_QUADTAP),
    }
}

fn push_absolute(events: &mut Vec<InputEvent>, axis: AbsoluteAxisCode, value: i32) {
    events.push(InputEvent::new(EventType::ABSOLUTE.0, axis.0, value));
}

fn push_key(events: &mut Vec<InputEvent>, key: KeyCode, pressed: bool) {
    events.push(InputEvent::new(
        EventType::KEY.0,
        key.code(),
        i32::from(pressed),
    ));
}

#[derive(Debug)]
pub struct VirtualInput {
    pub keyboard: VirtualKeyboard,
    pub pointer: VirtualPointer,
    pub touchpad: Option<VirtualTouchpad>,
}

impl VirtualInput {
    pub fn create(experimental_touchpad: bool) -> Result<Self, InjectionError> {
        // If pointer creation fails, keyboard drops immediately and uinput
        // removes it; callers never observe half a virtual device pair.
        let keyboard = VirtualKeyboard::create()?;
        let pointer = VirtualPointer::create()?;
        let touchpad = experimental_touchpad
            .then(VirtualTouchpad::create)
            .transpose()?;
        Ok(Self {
            keyboard,
            pointer,
            touchpad,
        })
    }

    pub fn replace_touch(&mut self, touch: &TouchState) -> Result<(), InjectionError> {
        match &mut self.touchpad {
            Some(touchpad) => touchpad.replace(touch),
            None if touch.is_empty() => Ok(()),
            None => Err(InjectionError::TouchpadDisabled),
        }
    }

    pub fn release_all(&mut self) -> Result<(), ReleaseAllError> {
        let mut failures = Vec::new();
        if let Err(error) = self.keyboard.release_all() {
            failures.push(error);
        }
        if let Err(error) = self.pointer.release_all() {
            failures.push(error);
        }
        if let Some(touchpad) = &mut self.touchpad
            && let Err(error) = touchpad.release_all()
        {
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
    use crate::core::{SourceDimensions, TouchContact, TouchTool};

    fn touch(id: u32, x: i32, y: i32) -> TouchContact {
        TouchContact {
            id: ContactId(id),
            x,
            y,
            pressure: None,
            major: None,
            minor: None,
            orientation_millidegrees: None,
            tool: TouchTool::Finger,
            source_dimensions: Some(SourceDimensions {
                width: 1_299,
                height: 722,
            }),
        }
    }

    fn one_touchpad_report(
        previous: &TouchpadState,
        touch: &TouchState,
    ) -> (Vec<InputEvent>, TouchpadState) {
        let mut reports = plan_touchpad_events(previous, touch).unwrap();
        assert_eq!(reports.len(), 1);
        reports.pop().unwrap()
    }

    fn tracking_transitions(events: &[InputEvent]) -> Vec<(i32, i32)> {
        let mut slot = 0;
        let mut transitions = Vec::new();
        for event in events {
            if event.event_type() != EventType::ABSOLUTE {
                continue;
            }
            if event.code() == AbsoluteAxisCode::ABS_MT_SLOT.0 {
                slot = event.value();
            } else if event.code() == AbsoluteAxisCode::ABS_MT_TRACKING_ID.0 {
                transitions.push((slot, event.value()));
            }
        }
        transitions
    }

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

    #[test]
    fn touchpad_plan_preserves_slots_and_releases_lifted_contacts() {
        let initial = TouchState::new([touch(10, 100, 200), touch(20, 900, 500)]).unwrap();
        let (landing, state) = one_touchpad_report(&TouchpadState::default(), &initial);
        assert!(landing.iter().any(|event| {
            event.event_type() == EventType::KEY
                && event.code() == KeyCode::BTN_TOOL_DOUBLETAP.code()
                && event.value() == 1
        }));
        assert_eq!(state.contacts[&ContactId(10)].slot, 0);
        assert_eq!(state.contacts[&ContactId(20)].slot, 1);

        let moved = TouchState::new([touch(10, 200, 300), touch(20, 900, 500)]).unwrap();
        let (movement, state) = one_touchpad_report(&state, &moved);
        assert!(movement.iter().any(|event| {
            event.event_type() == EventType::ABSOLUTE
                && event.code() == AbsoluteAxisCode::ABS_MT_POSITION_X.0
        }));
        assert_eq!(state.contacts[&ContactId(10)].slot, 0);

        let remaining = TouchState::new([touch(20, 900, 500)]).unwrap();
        let (lift, state) = one_touchpad_report(&state, &remaining);
        assert!(lift.iter().any(|event| {
            event.event_type() == EventType::ABSOLUTE
                && event.code() == AbsoluteAxisCode::ABS_MT_TRACKING_ID.0
                && event.value() == -1
        }));
        assert_eq!(state.contacts[&ContactId(20)].slot, 1);
    }

    #[test]
    fn touchpad_plan_scales_source_dimensions_and_emits_final_lift() {
        let initial = TouchState::new([touch(1, 1_299, 722)]).unwrap();
        let (landing, state) = one_touchpad_report(&TouchpadState::default(), &initial);
        assert!(landing.iter().any(|event| {
            event.code() == AbsoluteAxisCode::ABS_MT_POSITION_X.0 && event.value() == TOUCHPAD_MAX_X
        }));
        assert!(landing.iter().any(|event| {
            event.code() == AbsoluteAxisCode::ABS_MT_POSITION_Y.0 && event.value() == TOUCHPAD_MAX_Y
        }));

        let (lift, empty) = one_touchpad_report(&state, &TouchState::default());
        assert!(empty.contacts.is_empty());
        assert!(lift.iter().any(|event| {
            event.event_type() == EventType::KEY
                && event.code() == KeyCode::BTN_TOUCH.code()
                && event.value() == 0
        }));
    }

    #[test]
    fn touchpad_plan_uses_a_fresh_slot_for_simultaneous_lift_and_land() {
        let initial = TouchState::new([touch(1, 100, 200)]).unwrap();
        let (_, state) = one_touchpad_report(&TouchpadState::default(), &initial);
        assert_eq!(state.contacts[&ContactId(1)].slot, 0);

        let replacement = TouchState::new([touch(2, 900, 500)]).unwrap();
        let reports = plan_touchpad_events(&state, &replacement).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(tracking_transitions(&reports[0].0), vec![(0, -1), (1, 2)]);
        assert_eq!(reports[0].1.contacts[&ContactId(2)].slot, 1);
    }

    #[test]
    fn touchpad_plan_splits_full_capacity_replacement_across_reports() {
        let initial = TouchState::new([
            touch(1, 100, 200),
            touch(2, 300, 200),
            touch(3, 500, 200),
            touch(4, 700, 200),
            touch(5, 900, 200),
        ])
        .unwrap();
        let (_, state) = one_touchpad_report(&TouchpadState::default(), &initial);

        let replacement = TouchState::new([
            touch(1, 100, 200),
            touch(2, 300, 200),
            touch(3, 500, 200),
            touch(4, 700, 200),
            touch(6, 1_100, 200),
        ])
        .unwrap();
        let reports = plan_touchpad_events(&state, &replacement).unwrap();

        assert_eq!(reports.len(), 2);
        assert_eq!(tracking_transitions(&reports[0].0), vec![(4, -1)]);
        assert_eq!(tracking_transitions(&reports[1].0), vec![(4, 6)]);
        for id in 1..=4 {
            assert_eq!(
                reports[0].1.contacts[&ContactId(id)].slot,
                (id - 1) as usize
            );
            assert_eq!(
                reports[1].1.contacts[&ContactId(id)].slot,
                (id - 1) as usize
            );
        }
        assert_eq!(reports[1].1.contacts[&ContactId(6)].slot, 4);
    }
}
