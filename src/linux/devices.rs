use std::{
    io,
    path::{Path, PathBuf},
};

use evdev::{BusType, Device};

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
