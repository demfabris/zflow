use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use evdev::{BusType, Device, InputId, KeyCode, RelativeAxisCode};
use thiserror::Error;

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
    pub has_keyboard_keys: bool,
    pub has_pointer_buttons: bool,
    pub has_relative_pointer: bool,
    pub has_high_resolution_wheel: bool,
}

impl DeviceInfo {
    pub fn from_device(path: PathBuf, device: &Device) -> Self {
        let id = device.input_id();
        let keys = device.supported_keys();
        let axes = device.supported_relative_axes();
        Self {
            path,
            name: device.name().map(str::to_owned),
            physical_path: device.physical_path().map(str::to_owned),
            unique_name: device.unique_name().map(str::to_owned),
            bus: id.bus_type().0,
            vendor: id.vendor(),
            product: id.product(),
            version: id.version(),
            has_keyboard_keys: keys.is_some_and(|keys| {
                keys.contains(KeyCode::KEY_A) || keys.contains(KeyCode::KEY_ENTER)
            }),
            has_pointer_buttons: keys.is_some_and(|keys| keys.contains(KeyCode::BTN_LEFT)),
            has_relative_pointer: axes.is_some_and(|axes| {
                axes.contains(RelativeAxisCode::REL_X) && axes.contains(RelativeAxisCode::REL_Y)
            }),
            has_high_resolution_wheel: axes.is_some_and(|axes| {
                axes.contains(RelativeAxisCode::REL_WHEEL_HI_RES)
                    || axes.contains(RelativeAxisCode::REL_HWHEEL_HI_RES)
            }),
        }
    }

    pub fn input_id(&self) -> InputId {
        InputId::new(BusType(self.bus), self.vendor, self.product, self.version)
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeviceSelector {
    pub path: Option<PathBuf>,
    pub name: Option<String>,
    pub physical_path: Option<String>,
    pub unique_name: Option<String>,
}

impl DeviceSelector {
    pub fn path(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            name: None,
            physical_path: None,
            unique_name: None,
        }
    }

    pub fn matches(&self, device: &DeviceInfo) -> bool {
        self.path.as_ref().is_none_or(|path| path == &device.path)
            && self
                .name
                .as_deref()
                .is_none_or(|name| device.name.as_deref() == Some(name))
            && self
                .physical_path
                .as_deref()
                .is_none_or(|phys| device.physical_path.as_deref() == Some(phys))
            && self
                .unique_name
                .as_deref()
                .is_none_or(|unique| device.unique_name.as_deref() == Some(unique))
    }

    pub fn is_empty(&self) -> bool {
        self.path.is_none()
            && self.name.is_none()
            && self.physical_path.is_none()
            && self.unique_name.is_none()
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SelectionError {
    #[error("capture device selector {index} is empty and would match every input device")]
    EmptySelector { index: usize },
    #[error("capture device selector {index} matched no physical input device: {selector:?}")]
    NoMatch {
        index: usize,
        selector: DeviceSelector,
    },
}

/// Resolves a logical capture set. One selector may intentionally match
/// several nodes of a composite keyboard; duplicate paths are collapsed.
pub fn select_devices(
    selectors: &[DeviceSelector],
    scan: &DeviceScan,
) -> Result<Vec<DeviceInfo>, SelectionError> {
    let mut selected = BTreeMap::<PathBuf, DeviceInfo>::new();
    for (index, selector) in selectors.iter().enumerate() {
        if selector.is_empty() {
            return Err(SelectionError::EmptySelector { index });
        }
        let matches = scan
            .physical_devices()
            .filter(|device| selector.matches(device))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return Err(SelectionError::NoMatch {
                index,
                selector: selector.clone(),
            });
        }
        for device in matches {
            selected.insert(device.path.clone(), device.clone());
        }
    }
    Ok(selected.into_values().collect())
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceDelta {
    pub added: Vec<DeviceInfo>,
    pub removed: Vec<DeviceInfo>,
}

impl DeviceDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

#[derive(Debug)]
pub struct RescanResult {
    pub delta: DeviceDelta,
    pub failures: Vec<DeviceOpenFailure>,
}

#[derive(Debug)]
pub struct PeriodicDeviceScanner {
    interval: Duration,
    last_scan: Option<Instant>,
    known: BTreeMap<PathBuf, DeviceInfo>,
}

impl PeriodicDeviceScanner {
    pub fn new(interval: Duration) -> Self {
        assert!(
            !interval.is_zero(),
            "input rescan interval must be non-zero"
        );
        Self {
            interval,
            last_scan: None,
            known: BTreeMap::new(),
        }
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    pub fn is_due(&self, now: Instant) -> bool {
        self.last_scan
            .is_none_or(|last_scan| now.duration_since(last_scan) >= self.interval)
    }

    pub fn poll(&mut self, now: Instant) -> io::Result<Option<RescanResult>> {
        if !self.is_due(now) {
            return Ok(None);
        }
        self.refresh_at(now).map(Some)
    }

    pub fn refresh(&mut self) -> io::Result<RescanResult> {
        self.refresh_at(Instant::now())
    }

    fn refresh_at(&mut self, now: Instant) -> io::Result<RescanResult> {
        let scan = enumerate_devices()?;
        self.last_scan = Some(now);
        Ok(self.apply_scan(scan))
    }

    pub fn apply_scan(&mut self, scan: DeviceScan) -> RescanResult {
        let current = scan
            .devices
            .into_iter()
            .filter(|device| !device.is_zflow_virtual())
            .map(|device| (device.path.clone(), device))
            .collect::<BTreeMap<_, _>>();

        let paths = self
            .known
            .keys()
            .chain(current.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut delta = DeviceDelta::default();
        for path in paths {
            match (self.known.get(&path), current.get(&path)) {
                (None, Some(added)) => delta.added.push(added.clone()),
                (Some(removed), None) => delta.removed.push(removed.clone()),
                (Some(old), Some(new)) if old != new => {
                    delta.removed.push(old.clone());
                    delta.added.push(new.clone());
                }
                _ => {}
            }
        }
        self.known = current;
        RescanResult {
            delta,
            failures: scan.failures,
        }
    }

    pub fn known(&self) -> impl Iterator<Item = &DeviceInfo> {
        self.known.values()
    }
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
            has_keyboard_keys: true,
            has_pointer_buttons: false,
            has_relative_pointer: false,
            has_high_resolution_wheel: false,
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
            let error = select_devices(&[DeviceSelector::path(path)], &scan).unwrap_err();
            assert!(matches!(error, SelectionError::NoMatch { .. }));
        }
    }

    #[test]
    fn selector_can_match_composite_nodes_and_deduplicates_overlaps() {
        let scan = DeviceScan {
            devices: vec![
                info("/dev/input/event2", "Composite Keyboard"),
                info("/dev/input/event3", "Composite Keyboard"),
            ],
            failures: vec![],
        };
        let selected = select_devices(
            &[
                DeviceSelector {
                    path: None,
                    name: Some("Composite Keyboard".into()),
                    physical_path: None,
                    unique_name: None,
                },
                DeviceSelector::path("/dev/input/event2"),
            ],
            &scan,
        )
        .unwrap();
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn rescanner_reports_add_change_and_remove() {
        let mut scanner = PeriodicDeviceScanner::new(Duration::from_secs(2));
        let first = scanner.apply_scan(DeviceScan {
            devices: vec![info("/dev/input/event1", "one")],
            failures: vec![],
        });
        assert_eq!(first.delta.added.len(), 1);

        let second = scanner.apply_scan(DeviceScan {
            devices: vec![info("/dev/input/event1", "replacement")],
            failures: vec![],
        });
        assert_eq!(second.delta.added.len(), 1);
        assert_eq!(second.delta.removed.len(), 1);

        let third = scanner.apply_scan(DeviceScan::default());
        assert_eq!(third.delta.removed.len(), 1);
    }
}
