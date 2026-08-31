use std::{
    collections::BTreeMap,
    fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use crate::linux::{DeviceInfo, VirtualDeviceRole, ZFLOW_KEYBOARD_NAME, ZFLOW_POINTER_NAME};

/// Parsed udev database properties. The parser accepts both the `E:KEY=value`
/// records stored below `/run/udev/data` and the `KEY=value` form printed by
/// `udevadm info --query=property`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UdevProperties(BTreeMap<String, String>);

impl UdevProperties {
    pub fn parse(text: &str) -> Self {
        let properties = text
            .lines()
            .filter_map(|line| {
                let line = line.strip_prefix("E:").unwrap_or(line);
                let (key, value) = line.split_once('=')?;
                (!key.is_empty()).then(|| (key.to_owned(), value.to_owned()))
            })
            .collect();
        Self(properties)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    fn is_one(&self, key: &str) -> bool {
        self.get(key) == Some("1")
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VirtualDeviceReadiness {
    pub keyboard: bool,
    pub pointer: bool,
}

impl VirtualDeviceReadiness {
    pub fn is_ready(self) -> bool {
        self.keyboard && self.pointer
    }

    pub fn observe(&mut self, device: &DeviceInfo, properties: &UdevProperties) {
        let Some(role) = device.zflow_role() else {
            return;
        };
        let on_seat_zero = properties.get("ID_SEAT") == Some("seat0");
        let common = properties.is_one("ZFLOW_VIRTUAL_DEVICE")
            && properties.is_one("ZFLOW_CAPTURE_EXCLUDE")
            && properties.is_one("ID_INPUT")
            && on_seat_zero;
        match role {
            VirtualDeviceRole::Keyboard => {
                self.keyboard |= common
                    && device.name.as_deref() == Some(ZFLOW_KEYBOARD_NAME)
                    && properties.get("ZFLOW_DEVICE_ROLE") == Some("remote-keyboard")
                    && properties.is_one("ID_INPUT_KEY")
                    && properties.is_one("ID_INPUT_KEYBOARD");
            }
            VirtualDeviceRole::Pointer => {
                self.pointer |= common
                    && device.name.as_deref() == Some(ZFLOW_POINTER_NAME)
                    && properties.get("ZFLOW_DEVICE_ROLE") == Some("remote-pointer")
                    && properties.is_one("ID_INPUT_MOUSE");
            }
        }
    }
}

/// Reads the udev database record corresponding to an event character device.
/// Looking up the record by `major:minor` avoids spawning `udevadm` from the
/// input-owning service and observes exactly the properties udev published.
pub fn read_udev_properties(event_path: &Path, database_dir: &Path) -> io::Result<UdevProperties> {
    let metadata = fs::metadata(event_path)?;
    let device = metadata.rdev();
    let record = database_dir.join(format!("c{}:{}", libc::major(device), libc::minor(device)));
    fs::read_to_string(record).map(|text| UdevProperties::parse(&text))
}

pub fn probe_virtual_device_readiness() -> io::Result<VirtualDeviceReadiness> {
    probe_virtual_device_readiness_in(
        Path::new("/dev/input"),
        Path::new("/sys/class/input"),
        Path::new("/run/udev/data"),
    )
}

pub fn probe_virtual_device_readiness_in(
    input_dir: &Path,
    sys_class_input_dir: &Path,
    database_dir: &Path,
) -> io::Result<VirtualDeviceReadiness> {
    let mut readiness = VirtualDeviceReadiness::default();
    for entry in fs::read_dir(sys_class_input_dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.strip_prefix("event").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            continue;
        }
        let Some(device) = device_info_from_sysfs(input_dir.join(name), &entry.path()) else {
            continue;
        };
        if device.zflow_role().is_none() {
            continue;
        }
        match read_udev_properties(&device.path, database_dir) {
            Ok(properties) => readiness.observe(&device, &properties),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(readiness)
}

fn device_info_from_sysfs(event_path: PathBuf, class_path: &Path) -> Option<DeviceInfo> {
    let device = class_path.join("device");
    Some(DeviceInfo {
        path: event_path,
        name: read_trimmed(device.join("name")),
        physical_path: read_trimmed(device.join("phys")),
        unique_name: read_trimmed(device.join("uniq")),
        bus: read_hex_u16(device.join("id/bustype"))?,
        vendor: read_hex_u16(device.join("id/vendor"))?,
        product: read_hex_u16(device.join("id/product"))?,
        version: read_hex_u16(device.join("id/version"))?,
        // Readiness only needs the stable identifiers above. Classification is
        // deliberately taken from udev rather than inferred from capabilities.
        has_keyboard_keys: false,
        has_pointer_buttons: false,
        has_relative_pointer: false,
        has_high_resolution_wheel: false,
    })
}

fn read_trimmed(path: PathBuf) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn read_hex_u16(path: PathBuf) -> Option<u16> {
    let value = fs::read_to_string(path).ok()?;
    u16::from_str_radix(value.trim(), 16).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadinessPaths {
    pub input_dir: PathBuf,
    pub sys_class_input_dir: PathBuf,
    pub udev_database_dir: PathBuf,
}

impl Default for ReadinessPaths {
    fn default() -> Self {
        Self {
            input_dir: PathBuf::from("/dev/input"),
            sys_class_input_dir: PathBuf::from("/sys/class/input"),
            udev_database_dir: PathBuf::from("/run/udev/data"),
        }
    }
}

#[cfg(test)]
mod tests {
    use evdev::BusType;

    use super::*;
    use crate::linux::{
        ZFLOW_DEVICE_VERSION, ZFLOW_KEYBOARD_PHYS, ZFLOW_KEYBOARD_PRODUCT_ID, ZFLOW_POINTER_PHYS,
        ZFLOW_POINTER_PRODUCT_ID, ZFLOW_VENDOR_ID,
    };

    fn virtual_info(role: VirtualDeviceRole) -> DeviceInfo {
        let (path, name, physical_path, product) = match role {
            VirtualDeviceRole::Keyboard => (
                "/dev/input/event10",
                ZFLOW_KEYBOARD_NAME,
                ZFLOW_KEYBOARD_PHYS,
                ZFLOW_KEYBOARD_PRODUCT_ID,
            ),
            VirtualDeviceRole::Pointer => (
                "/dev/input/event11",
                ZFLOW_POINTER_NAME,
                ZFLOW_POINTER_PHYS,
                ZFLOW_POINTER_PRODUCT_ID,
            ),
        };
        DeviceInfo {
            path: path.into(),
            name: Some(name.into()),
            physical_path: Some(physical_path.into()),
            unique_name: None,
            bus: BusType::BUS_VIRTUAL.0,
            vendor: ZFLOW_VENDOR_ID,
            product,
            version: ZFLOW_DEVICE_VERSION,
            has_keyboard_keys: role == VirtualDeviceRole::Keyboard,
            has_pointer_buttons: role == VirtualDeviceRole::Pointer,
            has_relative_pointer: role == VirtualDeviceRole::Pointer,
            has_high_resolution_wheel: role == VirtualDeviceRole::Pointer,
        }
    }

    #[test]
    fn parses_udev_database_and_udevadm_property_forms() {
        let properties = UdevProperties::parse(
            "I:123\nE:ID_INPUT=1\nE:ID_SEAT=seat0\nZFLOW_DEVICE_ROLE=remote-keyboard\n",
        );
        assert_eq!(properties.get("ID_INPUT"), Some("1"));
        assert_eq!(properties.get("ID_SEAT"), Some("seat0"));
        assert_eq!(properties.get("ZFLOW_DEVICE_ROLE"), Some("remote-keyboard"));
        assert_eq!(properties.get("I:123"), None);
    }

    #[test]
    fn readiness_requires_both_exact_roles_and_classification() {
        let mut readiness = VirtualDeviceReadiness::default();
        let keyboard = UdevProperties::parse(
            "E:ZFLOW_VIRTUAL_DEVICE=1\nE:ZFLOW_CAPTURE_EXCLUDE=1\nE:ZFLOW_DEVICE_ROLE=remote-keyboard\nE:ID_SEAT=seat0\nE:ID_INPUT=1\nE:ID_INPUT_KEY=1\nE:ID_INPUT_KEYBOARD=1\n",
        );
        readiness.observe(&virtual_info(VirtualDeviceRole::Keyboard), &keyboard);
        assert!(!readiness.is_ready());

        let pointer = UdevProperties::parse(
            "E:ZFLOW_VIRTUAL_DEVICE=1\nE:ZFLOW_CAPTURE_EXCLUDE=1\nE:ZFLOW_DEVICE_ROLE=remote-pointer\nE:ID_SEAT=seat0\nE:ID_INPUT=1\nE:ID_INPUT_MOUSE=1\n",
        );
        readiness.observe(&virtual_info(VirtualDeviceRole::Pointer), &pointer);
        assert!(readiness.is_ready());
    }

    #[test]
    fn wrong_seat_or_missing_classification_is_not_ready() {
        let properties = UdevProperties::parse(
            "E:ZFLOW_VIRTUAL_DEVICE=1\nE:ZFLOW_CAPTURE_EXCLUDE=1\nE:ZFLOW_DEVICE_ROLE=remote-pointer\nE:ID_SEAT=seat1\nE:ID_INPUT=1\n",
        );
        let mut readiness = VirtualDeviceReadiness::default();
        readiness.observe(&virtual_info(VirtualDeviceRole::Pointer), &properties);
        assert_eq!(readiness, VirtualDeviceReadiness::default());
    }
}
