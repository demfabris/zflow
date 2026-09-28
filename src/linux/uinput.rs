use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    io,
    time::{Duration, Instant},
};

use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode,
    PropType, RelativeAxisCode, UinputAbsSetup, uinput::VirtualDevice,
};
use thiserror::Error;

use crate::core::{
    ContactId, HidUsage, KeyRemap, KeyboardMode, MotionDelta, PointerButton, SourceDimensions,
    TouchContact, TouchState,
};

use super::{
    MAX_TOUCHPAD_CONTACTS, MappingError, VirtualDeviceRole, ZFLOW_DEVICE_VERSION, ZFLOW_VENDOR_ID,
    hid_to_evdev_key, mapped_evdev_keys, mapped_pointer_buttons, pointer_button_to_evdev,
};

const WHEEL_CLICK_UNITS: i64 = 120;
// 200 x 150 mm holds the largest real trackpads (a Magic Trackpad is about
// 160 x 115 mm) with margin, so contacts are placed by millimetres, not clipped.
const TOUCHPAD_MAX_X: i32 = 5_999;
const TOUCHPAD_MAX_Y: i32 = 4_499;
const TOUCHPAD_RESOLUTION: i32 = 30;
/// Assumed for a contact that arrives without a source size: the old 100 x 73 mm pad.
const UNSIZED_SOURCE: SourceDimensions = SourceDimensions {
    width: 10_000,
    height: 7_300,
};

#[derive(Debug, Error)]
pub enum InjectionError {
    #[error(transparent)]
    Unsupported(#[from] MappingError),
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
    /// Turns the peer's physical key edges into the edges injected here.
    remap: KeyRemap,
    /// Injected usages, after the remap.
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
            remap: KeyRemap::new(KeyboardMode::Standard),
            held: BTreeSet::new(),
        })
    }

    /// Starts an activation in `mode`, with nothing from the last one down.
    pub fn begin(&mut self, mode: KeyboardMode) -> Result<(), InjectionError> {
        self.release_all()?;
        self.remap.reset(mode);
        Ok(())
    }

    /// The mode the current activation started in.
    pub fn mode(&self) -> KeyboardMode {
        self.remap.mode()
    }

    /// Whether the focused app is a terminal. Only keys pressed after this
    /// see it.
    pub fn set_terminal(&mut self, terminal: bool) {
        self.remap.set_terminal(terminal);
    }

    /// Applies one physical key edge from the peer.
    pub fn set_key(&mut self, usage: HidUsage, pressed: bool) -> Result<(), InjectionError> {
        // Refuse a key this device cannot type before the remap records it.
        hid_to_evdev_key(usage)?;
        for (output, pressed) in self.remap.key(usage, pressed) {
            self.inject(output, pressed)?;
        }
        Ok(())
    }

    /// Call before a button press, a scroll or a touch replacement with
    /// contacts. `busy` says whether a button or contact is still down after
    /// it.
    pub fn pointer(&mut self, busy: bool) -> Result<(), InjectionError> {
        for (output, pressed) in self.remap.pointer(busy) {
            self.inject(output, pressed)?;
        }
        Ok(())
    }

    /// Call before a button release or a touch replacement with no
    /// contacts. It presses nothing.
    pub fn pointer_released(&mut self, busy: bool) {
        self.remap.pointer_released(busy);
    }

    /// Each edge is its own report, so a modifier change lands before the
    /// key it goes with.
    fn inject(&mut self, usage: HidUsage, pressed: bool) -> Result<(), InjectionError> {
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

    pub fn held(&self) -> &BTreeSet<HidUsage> {
        &self.held
    }

    /// Releases every injected key and forgets the peer's held keys. The
    /// mode stays.
    pub fn release_all(&mut self) -> Result<(), InjectionError> {
        self.remap.reset(self.remap.mode());
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
        let scroll_x = relative_value(motion.scroll_x);
        let scroll_y = relative_value(motion.scroll_y);
        let mut events = Vec::with_capacity(6);
        push_relative(
            &mut events,
            RelativeAxisCode::REL_X,
            relative_value(motion.dx),
        );
        push_relative(
            &mut events,
            RelativeAxisCode::REL_Y,
            relative_value(motion.dy),
        );
        push_relative(&mut events, RelativeAxisCode::REL_HWHEEL_HI_RES, scroll_x);
        push_relative(&mut events, RelativeAxisCode::REL_WHEEL_HI_RES, scroll_y);

        let legacy_x = legacy_wheel_delta(&mut self.legacy_hwheel_remainder, scroll_x);
        let legacy_y = legacy_wheel_delta(&mut self.legacy_wheel_remainder, scroll_y);
        push_relative(&mut events, RelativeAxisCode::REL_HWHEEL, legacy_x);
        push_relative(&mut events, RelativeAxisCode::REL_WHEEL, legacy_y);
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
    last_report_time: Duration,
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
            last_report_time: Duration::ZERO,
        })
    }

    pub fn replace(&mut self, touch: &TouchState) -> Result<(), InjectionError> {
        self.replace_at(touch, None)
    }

    pub fn replace_at(
        &mut self,
        touch: &TouchState,
        captured_at: Option<Instant>,
    ) -> Result<(), InjectionError> {
        for (mut events, next) in plan_touchpad_events(&self.state, touch)? {
            if !events.is_empty() {
                let age = captured_at
                    .filter(|_| !touch.is_empty())
                    .map(|time| time.elapsed());
                let now = monotonic_time().map_err(|source| InjectionError::Emit {
                    role: VirtualDeviceRole::Touchpad,
                    source,
                })?;
                let timestamp = touch_report_time(now, age, self.last_report_time);
                stamp_touch_report(&mut events, timestamp);
                self.device
                    .emit(&events)
                    .map_err(|source| InjectionError::Emit {
                        role: VirtualDeviceRole::Touchpad,
                        source,
                    })?;
                self.last_report_time = timestamp;
            }
            self.state = next;
        }
        Ok(())
    }

    pub fn release_all(&mut self) -> Result<(), InjectionError> {
        self.replace(&TouchState::default())
    }
}

fn monotonic_time() -> io::Result<Duration> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // CLOCK_MONOTONIC matches both Rust Instant and libinput's evdev clock.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Duration::new(time.tv_sec as u64, time.tv_nsec as u32))
}

fn touch_report_time(now: Duration, age: Option<Duration>, previous: Duration) -> Duration {
    // uinput rejects future timestamps and timestamps older than ten seconds.
    let capture = age
        .filter(|age| *age < Duration::from_secs(10))
        .map_or(now, |age| now.saturating_sub(age));
    capture.max(previous).min(now)
}

fn stamp_touch_report(events: &mut [InputEvent], timestamp: Duration) {
    for event in events {
        *event = InputEvent::from(libc::input_event {
            time: libc::timeval {
                tv_sec: timestamp.as_secs() as libc::time_t,
                tv_usec: timestamp.subsec_micros().into(),
            },
            type_: event.event_type().0,
            code: event.code(),
            value: event.value(),
        });
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
                x: touch_axis(contact, true),
                y: touch_axis(contact, false),
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
                x: touch_axis(contact, true),
                y: touch_axis(contact, false),
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

/// Places a contact by millimetres with the source surface centered on the pad,
/// so libinput sees the finger travel the source measured.
fn touch_axis(contact: &TouchContact, horizontal: bool) -> i32 {
    let source = contact.source_dimensions.unwrap_or(UNSIZED_SOURCE);
    let (value, extent, maximum) = if horizontal {
        (contact.x, source.width, TOUCHPAD_MAX_X)
    } else {
        (contact.y, source.height, TOUCHPAD_MAX_Y)
    };
    // maximum / 2 + (value - extent / 2) * TOUCHPAD_RESOLUTION / 100, doubled
    // to stay in integers, then rounded.
    let doubled = i64::from(maximum) * 100
        + (2 * i64::from(value) - i64::from(extent)) * i64::from(TOUCHPAD_RESOLUTION);
    (doubled + 100).div_euclid(200).clamp(0, maximum.into()) as i32
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

    /// Whether a contact is down on the virtual touchpad.
    pub fn touching(&self) -> bool {
        self.touchpad
            .as_ref()
            .is_some_and(|touchpad| !touchpad.state.contacts.is_empty())
    }

    pub fn replace_touch(&mut self, touch: &TouchState) -> Result<(), InjectionError> {
        self.replace_touch_at(touch, None)
    }

    pub fn replace_touch_at(
        &mut self,
        touch: &TouchState,
        captured_at: Option<Instant>,
    ) -> Result<(), InjectionError> {
        match &mut self.touchpad {
            Some(touchpad) => touchpad.replace_at(touch, captured_at),
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
}

impl Drop for VirtualInput {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

fn push_relative(events: &mut Vec<InputEvent>, axis: RelativeAxisCode, value: i32) {
    if value != 0 {
        events.push(InputEvent::new(EventType::RELATIVE.0, axis.0, value));
    }
}

/// Peers choose motion deltas and input_event carries an i32. Clamping an
/// absurd delta keeps it from stopping the receiver for every peer.
fn relative_value(value: i64) -> i32 {
    value.clamp(i32::MIN.into(), i32::MAX.into()) as i32
}

fn legacy_wheel_delta(remainder: &mut i64, high_resolution: i32) -> i32 {
    let total = *remainder + i64::from(high_resolution);
    *remainder = total % WHEEL_CLICK_UNITS;
    // The remainder is under one click, so the quotient fits an i32.
    (total / WHEEL_CLICK_UNITS) as i32
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
            // A Magic Trackpad, in hundredths of a millimetre.
            source_dimensions: Some(SourceDimensions {
                width: 16_000,
                height: 11_500,
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
    fn every_keyboard_mode_output_has_an_evdev_key() {
        let outputs = crate::core::output_usages();
        assert!(!outputs.is_empty());
        for usage in outputs {
            assert!(hid_to_evdev_key(usage).is_ok(), "{usage:?}");
        }
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
    fn oversized_peer_motion_is_clamped_instead_of_failing() {
        assert_eq!(relative_value(i64::MAX), i32::MAX);
        assert_eq!(relative_value(i64::MIN), i32::MIN);
        assert_eq!(relative_value(-7), -7);
        let mut remainder = WHEEL_CLICK_UNITS - 1;
        assert_eq!(
            i64::from(legacy_wheel_delta(&mut remainder, i32::MAX)),
            (i64::from(i32::MAX) + WHEEL_CLICK_UNITS - 1) / WHEEL_CLICK_UNITS
        );
        let mut remainder = 1 - WHEEL_CLICK_UNITS;
        assert!(legacy_wheel_delta(&mut remainder, i32::MIN) < 0);
    }

    #[test]
    fn touch_timestamps_are_monotonic_bounded_and_cleanup_uses_now() {
        let now = Duration::from_secs(30);
        let previous = now - Duration::from_millis(20);
        assert_eq!(
            touch_report_time(now, Some(Duration::from_millis(10)), previous),
            now - Duration::from_millis(10)
        );
        assert_eq!(
            touch_report_time(now, Some(Duration::from_millis(50)), previous),
            previous
        );
        assert_eq!(
            touch_report_time(now, Some(Duration::from_secs(11)), previous),
            now
        );
        assert_eq!(touch_report_time(now, None, previous), now);
        assert_eq!(
            touch_report_time(now, Some(Duration::ZERO), now + Duration::from_secs(1)),
            now
        );
        let mut events = vec![InputEvent::new(
            EventType::ABSOLUTE.0,
            AbsoluteAxisCode::ABS_MT_POSITION_X.0,
            42,
        )];
        stamp_touch_report(&mut events, previous);
        assert_eq!(
            events[0]
                .timestamp()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap(),
            previous
        );
        assert_eq!(events[0].value(), 42);
    }

    #[test]
    fn jitter_burst_keeps_capture_intervals_in_touch_reports() {
        use crate::core::{
            ActivationId, ClockMapper, ClockSample, ControlSequence, CumulativeMotion,
            MonotonicTimeMicros, MotionFrame, MotionSequence, PlayoutConfig, ReceiverPlayout,
            SessionContext, SessionEpoch, TransportGeneration,
        };
        let session = SessionContext {
            session_epoch: SessionEpoch([1; 16]),
            transport_generation: TransportGeneration(1),
            activation_id: ActivationId(1),
        };
        let mut clock = ClockMapper::default();
        clock
            .ingest_sample(ClockSample::exact(
                MonotonicTimeMicros(0),
                MonotonicTimeMicros(0),
            ))
            .unwrap();
        let mut playout = ReceiverPlayout::new(PlayoutConfig::default(), session).unwrap();
        let mut arrivals: Vec<_> = (1..=15u64)
            .map(|sequence| {
                let capture = sequence * 16_000;
                let arrival = match sequence {
                    8 | 9 => 195_000,
                    10 => 196_000,
                    11 => 197_000,
                    12 => 214_000,
                    _ => capture + 1_000,
                };
                // 2 mm per frame, ending at the middle of the trackpad.
                let contact = touch(1, (5_000 + sequence * 200) as i32, 5_750);
                (
                    arrival,
                    MotionFrame {
                        session,
                        motion_sequence: MotionSequence(sequence),
                        control_watermark: ControlSequence(0),
                        sender_capture_time: MonotonicTimeMicros(capture),
                        totals: CumulativeMotion::ZERO,
                        touch_snapshot: Some(TouchState::new([contact]).unwrap()),
                    },
                )
            })
            .collect();
        arrivals.sort_by_key(|(arrival, frame)| (*arrival, frame.motion_sequence));
        let mut arrivals = arrivals.into_iter().peekable();
        let origin = Duration::from_secs(30);
        let mut previous = origin;
        let mut state = TouchpadState::default();
        let mut emitted = Vec::new();
        for now in (0..400_000u64).step_by(1_000) {
            while arrivals.peek().is_some_and(|(arrival, _)| *arrival <= now) {
                let (arrival, frame) = arrivals.next().unwrap();
                playout
                    .ingest_frame(frame, MonotonicTimeMicros(arrival), &clock)
                    .unwrap();
            }
            if let Some(step) = playout.poll(MonotonicTimeMicros(now)).unwrap()
                && let Some(touch) = step.touch_snapshot
            {
                let timestamp = touch_report_time(
                    origin + Duration::from_micros(now),
                    Some(Duration::from_micros(now - step.mapped_capture_time.0)),
                    previous,
                );
                let (mut events, next) = one_touchpad_report(&state, &touch);
                stamp_touch_report(&mut events, timestamp);
                assert!(events.iter().all(|event| {
                    event
                        .timestamp()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        == timestamp
                }));
                state = next;
                previous = timestamp;
                emitted.push((step.through_sequence.0, now, timestamp));
            }
        }
        let ninth = emitted.iter().find(|event| event.0 == 9).unwrap();
        let tenth = emitted.iter().find(|event| event.0 == 10).unwrap();
        assert_eq!(
            tenth.1 - ninth.1,
            1_000,
            "fixture must reproduce a compressed delivery burst"
        );
        assert_eq!(tenth.2 - ninth.2, Duration::from_millis(16));
        // libinput normalizes this 2mm movement to 12ms. Arrival timing gives
        // 24mm (>20mm jump threshold); preserved capture timing gives 1.5mm.
        assert_eq!(2.0 * 12_000.0 / (tenth.1 - ninth.1) as f64, 24.0);
        assert_eq!(2.0 * 12_000.0 / (tenth.2 - ninth.2).as_micros() as f64, 1.5);
        assert_eq!(state.contacts[&ContactId(1)].x, 3_000);
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
    fn touch_moves_the_same_millimetres_on_the_pad_centered_and_clamped() {
        let at = |x, y| {
            let contact = touch(1, x, y);
            (touch_axis(&contact, true), touch_axis(&contact, false))
        };
        let millimetre = TOUCHPAD_RESOLUTION;
        // The middle of the trackpad lands in the middle of the pad.
        assert_eq!(at(8_000, 5_750), (3_000, 2_250));
        // 20 mm across and 10 mm down stay 20 mm and 10 mm: no stretch.
        let (x0, y0) = at(4_000, 3_000);
        let (x1, y1) = at(6_000, 4_000);
        assert_eq!((x1 - x0, y1 - y0), (20 * millimetre, 10 * millimetre));
        // The whole 160 x 115 mm surface fits, with equal margins.
        assert_eq!(at(0, 0), (600, 525));
        assert_eq!(at(16_000, 11_500), (5_400, 3_975));
        assert_eq!(at(-100_000, i32::MIN), (0, 0));
        assert_eq!(at(i32::MAX, 100_000), (TOUCHPAD_MAX_X, TOUCHPAD_MAX_Y));

        // A smaller Linux pad is centered too.
        let mut small = touch(1, 5_000, 3_000);
        small.source_dimensions = Some(SourceDimensions {
            width: 10_000,
            height: 6_000,
        });
        assert_eq!(touch_axis(&small, true), 3_000);
        assert_eq!(touch_axis(&small, false), 2_250);
    }

    #[test]
    fn touch_without_source_size_uses_a_centered_100_by_73_mm_area() {
        let at = |x, y| {
            let mut contact = touch(1, x, y);
            contact.source_dimensions = None;
            (touch_axis(&contact, true), touch_axis(&contact, false))
        };
        assert_eq!(at(5_000, 3_650), (3_000, 2_250));
        assert_eq!(at(0, 0), (1_500, 1_155));
        assert_eq!(at(10_000, 7_300), (4_500, 3_345));
    }

    #[test]
    fn touchpad_plan_places_contacts_by_millimetres_and_emits_final_lift() {
        let initial = TouchState::new([touch(1, 16_000, 11_500)]).unwrap();
        let (landing, state) = one_touchpad_report(&TouchpadState::default(), &initial);
        for (axis, value) in [
            (AbsoluteAxisCode::ABS_MT_POSITION_X, 5_400),
            (AbsoluteAxisCode::ABS_MT_POSITION_Y, 3_975),
            (AbsoluteAxisCode::ABS_X, 5_400),
            (AbsoluteAxisCode::ABS_Y, 3_975),
        ] {
            assert!(
                landing
                    .iter()
                    .any(|event| event.code() == axis.0 && event.value() == value)
            );
        }

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
