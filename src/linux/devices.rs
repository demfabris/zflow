use std::{
    io,
    path::{Path, PathBuf},
};

use evdev::{
    AbsoluteAxisCode, AttributeSetRef, BusType, Device, KeyCode, PropType, RelativeAxisCode,
};

pub const ZFLOW_VENDOR_ID: u16 = 0x1209;
pub const ZFLOW_KEYBOARD_PRODUCT_ID: u16 = 0x5a01;
pub const ZFLOW_POINTER_PRODUCT_ID: u16 = 0x5a02;
pub const ZFLOW_TOUCHPAD_PRODUCT_ID: u16 = 0x5a03;
pub const ZFLOW_DEVICE_VERSION: u16 = 1;
pub const ZFLOW_KEYBOARD_NAME: &str = "zflow remote keyboard";
pub const ZFLOW_POINTER_NAME: &str = "zflow remote pointer";
pub const ZFLOW_TOUCHPAD_NAME: &str = "zflow remote touchpad";
pub const ZFLOW_KEYBOARD_PHYS: &str = "zflow/remote/keyboard";
pub const ZFLOW_POINTER_PHYS: &str = "zflow/remote/pointer";
pub const ZFLOW_TOUCHPAD_PHYS: &str = "zflow/remote/touchpad";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualDeviceRole {
    Keyboard,
    Pointer,
    Touchpad,
}

impl VirtualDeviceRole {
    pub const fn product_id(self) -> u16 {
        match self {
            Self::Keyboard => ZFLOW_KEYBOARD_PRODUCT_ID,
            Self::Pointer => ZFLOW_POINTER_PRODUCT_ID,
            Self::Touchpad => ZFLOW_TOUCHPAD_PRODUCT_ID,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Keyboard => ZFLOW_KEYBOARD_NAME,
            Self::Pointer => ZFLOW_POINTER_NAME,
            Self::Touchpad => ZFLOW_TOUCHPAD_NAME,
        }
    }

    pub const fn physical_path(self) -> &'static str {
        match self {
            Self::Keyboard => ZFLOW_KEYBOARD_PHYS,
            Self::Pointer => ZFLOW_POINTER_PHYS,
            Self::Touchpad => ZFLOW_TOUCHPAD_PHYS,
        }
    }
}

/// What an event node is, for capturing every keyboard and pointer when no
/// devices are configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceClass {
    Keyboard,
    /// A mouse, pointing stick or trackball.
    Pointer,
    Touchpad,
}

/// Classifies a node from its capabilities close to how udev's input_id does,
/// so the daemon wants the same nodes the packaged udev rule lets it read.
/// Anything else, such as a power button, a pen tablet, a touchscreen or a
/// gamepad, is left alone.
pub fn classify_device(
    keys: Option<&AttributeSetRef<KeyCode>>,
    relative: Option<&AttributeSetRef<RelativeAxisCode>>,
    absolute: Option<&AttributeSetRef<AbsoluteAxisCode>>,
    properties: &AttributeSetRef<PropType>,
) -> Option<DeviceClass> {
    let has_key = |key| keys.is_some_and(|keys| keys.contains(key));
    let has_relative = |axis| relative.is_some_and(|axes| axes.contains(axis));
    let has_absolute = |axis| absolute.is_some_and(|axes| axes.contains(axis));

    // udev's keyboard test is every key from Esc to D: the digits, the Q row
    // and A, S, D. Media keys and power buttons fail it. Space is required
    // too, so the node can type text.
    let keyboard = (KeyCode::KEY_ESC.code()..=KeyCode::KEY_D.code())
        .all(|code| has_key(KeyCode::new(code)))
        && has_key(KeyCode::KEY_SPACE);
    if keyboard {
        return Some(DeviceClass::Keyboard);
    }
    // udev's mouse buttons run from BTN_LEFT up to the first joystick button.
    let mouse_button = (KeyCode::BTN_LEFT.code()..KeyCode::BTN_TRIGGER.code())
        .any(|code| has_key(KeyCode::new(code)));
    if has_relative(RelativeAxisCode::REL_X)
        && has_relative(RelativeAxisCode::REL_Y)
        && mouse_button
    {
        return Some(DeviceClass::Pointer);
    }
    let pen = has_key(KeyCode::BTN_TOOL_PEN) || has_key(KeyCode::BTN_STYLUS);
    if has_absolute(AbsoluteAxisCode::ABS_X)
        && has_absolute(AbsoluteAxisCode::ABS_Y)
        && has_key(KeyCode::BTN_TOOL_FINGER)
        && !pen
        && !properties.contains(PropType::DIRECT)
    {
        return Some(DeviceClass::Touchpad);
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub path: PathBuf,
    pub name: Option<String>,
    pub physical_path: Option<String>,
    pub unique_name: Option<String>,
    pub bus: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
    pub class: Option<DeviceClass>,
}

impl DeviceInfo {
    pub fn from_device(path: PathBuf, device: &Device) -> Self {
        let id = device.input_id();
        Self {
            path,
            name: device.name().map(str::to_owned),
            physical_path: device.physical_path().map(str::to_owned),
            unique_name: device.unique_name().map(str::to_owned),
            bus: id.bus_type().0,
            vendor: id.vendor(),
            product: id.product(),
            version: id.version(),
            class: classify_device(
                device.supported_keys(),
                device.supported_relative_axes(),
                device.supported_absolute_axes(),
                device.properties(),
            ),
        }
    }

    pub fn zflow_role(&self) -> Option<VirtualDeviceRole> {
        if self.bus != BusType::BUS_VIRTUAL.0
            || self.vendor != ZFLOW_VENDOR_ID
            || self.version != ZFLOW_DEVICE_VERSION
        {
            return None;
        }
        [
            VirtualDeviceRole::Keyboard,
            VirtualDeviceRole::Pointer,
            VirtualDeviceRole::Touchpad,
        ]
        .into_iter()
        .find(|role| {
            self.product == role.product_id()
                && self.name.as_deref() == Some(role.name())
                && self.physical_path.as_deref() == Some(role.physical_path())
        })
    }

    pub fn is_zflow_virtual(&self) -> bool {
        self.zflow_role().is_some()
    }
}

#[derive(Debug)]
pub struct DeviceOpenFailure {
    pub path: PathBuf,
    pub error: io::Error,
}

#[derive(Debug, Default)]
pub struct DeviceScan {
    pub devices: Vec<DeviceInfo>,
    pub failures: Vec<DeviceOpenFailure>,
}

impl DeviceScan {
    pub fn physical_devices(&self) -> impl Iterator<Item = &DeviceInfo> {
        self.devices
            .iter()
            .filter(|device| !device.is_zflow_virtual())
    }
}

/// Enumerates `/dev/input/event*` directly. Hotplug intentionally uses a
/// periodic rescan rather than libudev, keeping the service's platform surface
/// and AF_NETLINK needs small.
pub fn enumerate_devices() -> io::Result<DeviceScan> {
    enumerate_devices_in(Path::new("/dev/input"))
}

pub fn enumerate_devices_in(input_dir: &Path) -> io::Result<DeviceScan> {
    let mut paths = std::fs::read_dir(input_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.strip_prefix("event").is_some_and(|suffix| {
                        !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
                    })
                })
        })
        .collect::<Vec<_>>();
    paths.sort();

    let mut scan = DeviceScan::default();
    for path in paths {
        match Device::open(&path) {
            Ok(device) => scan.devices.push(DeviceInfo::from_device(path, &device)),
            Err(error) => scan.failures.push(DeviceOpenFailure { path, error }),
        }
    }
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use evdev::AttributeSet;

    use super::*;

    fn info(path: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            path: path.into(),
            name: Some(name.into()),
            physical_path: Some("usb-1/input0".into()),
            unique_name: None,
            bus: BusType::BUS_USB.0,
            vendor: 1,
            product: 2,
            version: 3,
            class: None,
        }
    }

    fn classify(
        keys: &[KeyCode],
        relative: &[RelativeAxisCode],
        absolute: &[AbsoluteAxisCode],
        properties: &[PropType],
    ) -> Option<DeviceClass> {
        let key_set = keys.iter().collect::<AttributeSet<_>>();
        let relative_set = relative.iter().collect::<AttributeSet<_>>();
        let absolute_set = absolute.iter().collect::<AttributeSet<_>>();
        let properties = properties.iter().collect::<AttributeSet<_>>();
        // evdev reports no set for an event type the node lacks.
        classify_device(
            (!keys.is_empty()).then_some(&*key_set),
            (!relative.is_empty()).then_some(&*relative_set),
            (!absolute.is_empty()).then_some(&*absolute_set),
            &properties,
        )
    }

    fn full_keyboard() -> Vec<KeyCode> {
        (KeyCode::KEY_ESC.code()..=KeyCode::KEY_KPDOT.code())
            .map(KeyCode::new)
            .collect()
    }

    #[test]
    fn keyboards_pointers_and_touchpads_are_classified_like_udev() {
        use AbsoluteAxisCode as Abs;
        use KeyCode as Key;
        use RelativeAxisCode as Rel;

        // keyd's virtual keyboard carries every key, like a real keyboard.
        assert_eq!(
            classify(&full_keyboard(), &[], &[], &[]),
            Some(DeviceClass::Keyboard)
        );
        let mouse_buttons = [Key::BTN_LEFT, Key::BTN_RIGHT, Key::BTN_MIDDLE];
        // A mouse, such as OpenLogi's virtual one.
        assert_eq!(
            classify(
                &mouse_buttons,
                &[Rel::REL_X, Rel::REL_Y, Rel::REL_WHEEL],
                &[],
                &[]
            ),
            Some(DeviceClass::Pointer)
        );
        // A pointing stick, and a trackball with only side buttons.
        assert_eq!(
            classify(
                &mouse_buttons,
                &[Rel::REL_X, Rel::REL_Y],
                &[],
                &[PropType::POINTER, PropType::POINTING_STICK]
            ),
            Some(DeviceClass::Pointer)
        );
        assert_eq!(
            classify(&[Key::BTN_SIDE], &[Rel::REL_X, Rel::REL_Y], &[], &[]),
            Some(DeviceClass::Pointer)
        );
        let touchpad_axes = [
            Abs::ABS_X,
            Abs::ABS_Y,
            Abs::ABS_MT_SLOT,
            Abs::ABS_MT_POSITION_X,
            Abs::ABS_MT_POSITION_Y,
            Abs::ABS_MT_TRACKING_ID,
        ];
        assert_eq!(
            classify(
                &[Key::BTN_LEFT, Key::BTN_TOOL_FINGER, Key::BTN_TOUCH],
                &[],
                &touchpad_axes,
                &[PropType::POINTER, PropType::BUTTONPAD]
            ),
            Some(DeviceClass::Touchpad)
        );
    }

    #[test]
    fn other_input_nodes_are_not_captured() {
        use AbsoluteAxisCode as Abs;
        use KeyCode as Key;
        use RelativeAxisCode as Rel;

        let touch_axes = [
            Abs::ABS_X,
            Abs::ABS_Y,
            Abs::ABS_MT_POSITION_X,
            Abs::ABS_MT_POSITION_Y,
        ];
        let mut no_digits = full_keyboard();
        no_digits.retain(|key| !(Key::KEY_1.code()..=Key::KEY_0.code()).contains(&key.code()));
        let mut no_space = full_keyboard();
        no_space.retain(|&key| key != Key::KEY_SPACE);
        for (what, class) in [
            ("power button", classify(&[Key::KEY_POWER], &[], &[], &[])),
            (
                "media keys",
                classify(
                    &[Key::KEY_VOLUMEUP, Key::KEY_PLAYPAUSE, Key::KEY_SPACE],
                    &[Rel::REL_HWHEEL],
                    &[],
                    &[],
                ),
            ),
            // udev does not call this a keyboard, so it would stay unreadable.
            ("keys without digits", classify(&no_digits, &[], &[], &[])),
            ("keys without space", classify(&no_space, &[], &[], &[])),
            (
                "motion without a mouse button",
                classify(&[], &[Rel::REL_X, Rel::REL_Y], &[], &[]),
            ),
            (
                "touchscreen",
                classify(
                    &[Key::BTN_TOUCH, Key::BTN_TOOL_FINGER],
                    &[],
                    &touch_axes,
                    &[PropType::DIRECT],
                ),
            ),
            (
                "pen tablet",
                classify(
                    &[Key::BTN_TOOL_PEN, Key::BTN_TOOL_FINGER, Key::BTN_STYLUS],
                    &[],
                    &touch_axes,
                    &[PropType::POINTER],
                ),
            ),
            (
                "gamepad",
                classify(
                    &[Key::BTN_SOUTH, Key::BTN_EAST, Key::BTN_TRIGGER],
                    &[],
                    &[Abs::ABS_X, Abs::ABS_Y],
                    &[],
                ),
            ),
        ] {
            assert_eq!(class, None, "{what}");
        }
    }

    #[test]
    fn exact_virtual_identifiers_are_always_excluded() {
        for (index, role) in [
            VirtualDeviceRole::Keyboard,
            VirtualDeviceRole::Pointer,
            VirtualDeviceRole::Touchpad,
        ]
        .into_iter()
        .enumerate()
        {
            let path = format!("/dev/input/event{}", index + 8);
            let mut virtual_device = info(&path, role.name());
            virtual_device.bus = BusType::BUS_VIRTUAL.0;
            virtual_device.vendor = ZFLOW_VENDOR_ID;
            virtual_device.product = role.product_id();
            virtual_device.version = ZFLOW_DEVICE_VERSION;
            virtual_device.physical_path = Some(role.physical_path().into());
            assert_eq!(virtual_device.zflow_role(), Some(role));

            let scan = DeviceScan {
                devices: vec![virtual_device],
                failures: vec![],
            };
            assert_eq!(scan.physical_devices().count(), 0);
        }
    }
}
