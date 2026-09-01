use std::{
    collections::BTreeSet,
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    time::Instant,
};

use evdev::{Device, InputEvent, KeyCode};
use thiserror::Error;

use super::{
    AggregateInputState, CaptureFrame, DeviceInfo, FrameAccumulator, MappingError, TouchAccumulator,
};

#[derive(Debug)]
struct CaptureNode {
    path: PathBuf,
    device: Device,
    frames: FrameAccumulator,
    touch: Option<TouchAccumulator>,
}

impl CaptureNode {
    fn open(info: &DeviceInfo) -> io::Result<(Self, Vec<KeyCode>)> {
        let device = Device::open(&info.path)?;
        device.set_nonblocking(true)?;
        let held = device.get_key_state()?.iter().collect();
        let touch = TouchAccumulator::from_device(&device)?;
        Ok((
            Self {
                path: info.path.clone(),
                device,
                frames: FrameAccumulator::default(),
                touch,
            },
            held,
        ))
    }

    fn fetch_events(&mut self) -> io::Result<Vec<InputEvent>> {
        match self.device.fetch_events() {
            Ok(events) => Ok(events.collect()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }
}

trait GrabTarget {
    fn target_path(&self) -> &Path;
    fn take_grab(&mut self) -> io::Result<()>;
    fn release_grab(&mut self) -> io::Result<()>;
}

impl GrabTarget for CaptureNode {
    fn target_path(&self) -> &Path {
        &self.path
    }

    fn take_grab(&mut self) -> io::Result<()> {
        self.device.grab()
    }

    fn release_grab(&mut self) -> io::Result<()> {
        self.device.ungrab()
    }
}

#[derive(Debug)]
pub struct UngrabFailure {
    pub path: PathBuf,
    pub error: io::Error,
}

#[derive(Debug, Error)]
#[error("failed to grab {path}: {source}; rolled back {rolled_back} earlier grab(s)")]
pub struct GrabError {
    pub path: PathBuf,
    #[source]
    pub source: io::Error,
    pub rolled_back: usize,
    pub rollback_failures: Vec<UngrabFailure>,
}

fn grab_transaction<T: GrabTarget>(targets: &mut [T]) -> Result<(), GrabError> {
    for index in 0..targets.len() {
        if let Err(source) = targets[index].take_grab() {
            let path = targets[index].target_path().to_owned();
            let mut rollback_failures = Vec::new();
            for target in targets[..index].iter_mut().rev() {
                if let Err(error) = target.release_grab() {
                    rollback_failures.push(UngrabFailure {
                        path: target.target_path().to_owned(),
                        error,
                    });
                }
            }
            return Err(GrabError {
                path,
                source,
                rolled_back: index,
                rollback_failures,
            });
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum CaptureSetError {
    #[error("the configured capture set is empty")]
    Empty,
    #[error("refusing to capture zflow's own virtual device {0}")]
    VirtualDevice(PathBuf),
    #[error("capture device {path} appears more than once")]
    Duplicate { path: PathBuf },
    #[error("failed to open or inspect capture device {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("capture set is not neutral")]
    NotNeutral,
    #[error(transparent)]
    Grab(#[from] GrabError),
    #[error("failed to inspect held state on {path}: {source}")]
    InspectState {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot change the capture set while physical devices are grabbed")]
    ActiveSetChange,
}

#[derive(Debug, Error)]
pub enum CaptureReadError {
    #[error("capture device is not open: {0}")]
    UnknownDevice(PathBuf),
    #[error("capture device {path} was removed; all remaining grabs were released")]
    DeviceRemoved { path: PathBuf },
    #[error("failed to read capture device {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to map event from {path}: {source}")]
    Mapping {
        path: PathBuf,
        #[source]
        source: MappingError,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedDeviceFrame {
    pub device_path: PathBuf,
    pub frame: CaptureFrame,
    /// When the terminating SYN_REPORT completed in the capture loop.
    pub captured_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconcileOutcome {
    pub added: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    pub activation_must_close: bool,
}

/// Open physical input nodes plus their aggregate ownership state. Merely
/// constructing this set never grabs anything, so Idle remains outside the
/// local input path.
#[derive(Debug, Default)]
pub struct CaptureSet {
    nodes: Vec<CaptureNode>,
    aggregate: AggregateInputState,
    grabbed: bool,
}

impl CaptureSet {
    pub fn open(devices: &[DeviceInfo]) -> Result<Self, CaptureSetError> {
        if devices.is_empty() {
            return Err(CaptureSetError::Empty);
        }
        let mut set = Self::default();
        let mut seen = BTreeSet::new();
        for info in devices {
            if info.is_zflow_virtual() {
                return Err(CaptureSetError::VirtualDevice(info.path.clone()));
            }
            if !seen.insert(info.path.clone()) {
                return Err(CaptureSetError::Duplicate {
                    path: info.path.clone(),
                });
            }
            set.open_one(info)?;
        }
        Ok(set)
    }

    fn open_one(&mut self, info: &DeviceInfo) -> Result<(), CaptureSetError> {
        let (node, held) = CaptureNode::open(info).map_err(|source| CaptureSetError::Open {
            path: info.path.clone(),
            source,
        })?;
        self.aggregate.add_device(info.path.clone(), held);
        self.nodes.push(node);
        self.nodes.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(())
    }

    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.nodes.iter().map(|node| node.path.as_path())
    }

    pub fn raw_fd(&self, path: &Path) -> Option<std::os::fd::RawFd> {
        self.nodes
            .iter()
            .find(|node| node.path == path)
            .map(|node| node.device.as_raw_fd())
    }

    pub fn aggregate_state(&self) -> &AggregateInputState {
        &self.aggregate
    }

    pub fn is_grabbed(&self) -> bool {
        self.grabbed
    }

    /// Performs fresh EVIOCGKEY checks over every node. Unlike the tracked
    /// state this closes the arming gap after startup or a dropped frame.
    pub fn kernel_is_neutral(&self) -> Result<bool, CaptureSetError> {
        for node in &self.nodes {
            let held =
                node.device
                    .get_key_state()
                    .map_err(|source| CaptureSetError::InspectState {
                        path: node.path.clone(),
                        source,
                    })?;
            if held.iter().next().is_some() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Acquires every EVIOCGRAB or none. Neutrality is checked both before and
    /// after the transaction; an input racing the ioctl causes full rollback.
    pub fn grab_all(&mut self) -> Result<(), CaptureSetError> {
        if self.nodes.is_empty() {
            return Err(CaptureSetError::Empty);
        }
        if self.grabbed {
            return Ok(());
        }
        if !self.aggregate.is_neutral() || !self.aggregate.all_at_boundary() {
            return Err(CaptureSetError::NotNeutral);
        }
        if !self.kernel_is_neutral()? {
            return Err(CaptureSetError::NotNeutral);
        }
        grab_transaction(&mut self.nodes)?;
        self.grabbed = true;

        match self.kernel_is_neutral() {
            Ok(true) => Ok(()),
            Ok(false) => {
                let _ = self.ungrab_all();
                Err(CaptureSetError::NotNeutral)
            }
            Err(error) => {
                let _ = self.ungrab_all();
                Err(error)
            }
        }
    }

    pub fn ungrab_all(&mut self) -> Vec<UngrabFailure> {
        let mut failures = Vec::new();
        for node in self.nodes.iter_mut().rev() {
            if node.device.is_grabbed()
                && let Err(error) = node.device.ungrab()
            {
                failures.push(UngrabFailure {
                    path: node.path.clone(),
                    error,
                });
            }
        }
        self.grabbed = self.nodes.iter().any(|node| node.device.is_grabbed());
        failures
    }

    /// Reads every currently-ready event from one node. Ownership tracking sees
    /// the raw event before mapping, while callers see frames only after
    /// SYN_REPORT.
    pub fn read_ready(
        &mut self,
        path: &Path,
    ) -> Result<Vec<CapturedDeviceFrame>, CaptureReadError> {
        let Some(index) = self.nodes.iter().position(|node| node.path == path) else {
            return Err(CaptureReadError::UnknownDevice(path.to_owned()));
        };
        let events = match self.nodes[index].fetch_events() {
            Ok(events) => events,
            Err(error) if is_device_removed(&error) => {
                self.ungrab_all();
                let removed = self.nodes.remove(index);
                self.aggregate.remove_device(&removed.path);
                return Err(CaptureReadError::DeviceRemoved { path: removed.path });
            }
            Err(source) => {
                return Err(CaptureReadError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };

        let mut frames = Vec::new();
        for event in events {
            self.aggregate.observe(path, event);
            let touch = self.nodes[index]
                .touch
                .as_mut()
                .map(|touch| touch.push(event))
                .transpose()
                .map_err(|_| CaptureReadError::Mapping {
                    path: path.to_owned(),
                    source: MappingError::InvalidTouchState,
                })?
                .flatten();
            match self.nodes[index].frames.push(event) {
                Ok(Some(mut frame)) => {
                    if let Some((state, event_count)) = touch {
                        frame.touch_snapshot = Some(state);
                        frame.event_count = frame.event_count.saturating_add(event_count);
                    }
                    frames.push(CapturedDeviceFrame {
                        device_path: path.to_owned(),
                        frame,
                        captured_at: Instant::now(),
                    });
                }
                Ok(None) => {}
                Err(source) => {
                    return Err(CaptureReadError::Mapping {
                        path: path.to_owned(),
                        source,
                    });
                }
            }
        }
        Ok(frames)
    }

    /// Applies a periodic-rescan selection. A removed active node closes the
    /// whole activation. New nodes never join an already grabbed set; they are
    /// picked up on the next Arming transition.
    pub fn reconcile(
        &mut self,
        selected: &[DeviceInfo],
    ) -> Result<ReconcileOutcome, CaptureSetError> {
        let selected_paths = selected
            .iter()
            .map(|info| info.path.clone())
            .collect::<BTreeSet<_>>();
        let current_paths = self
            .nodes
            .iter()
            .map(|node| node.path.clone())
            .collect::<BTreeSet<_>>();
        let removed = current_paths
            .difference(&selected_paths)
            .cloned()
            .collect::<Vec<_>>();
        let added_infos = selected
            .iter()
            .filter(|info| !current_paths.contains(&info.path))
            .collect::<Vec<_>>();

        let activation_must_close = self.grabbed && !removed.is_empty();
        if activation_must_close {
            self.ungrab_all();
        }
        self.nodes.retain(|node| !removed.contains(&node.path));
        for path in &removed {
            self.aggregate.remove_device(path);
        }

        let mut added = Vec::new();
        if !self.grabbed {
            for info in added_infos {
                if info.is_zflow_virtual() {
                    return Err(CaptureSetError::VirtualDevice(info.path.clone()));
                }
                self.open_one(info)?;
                added.push(info.path.clone());
            }
        }
        Ok(ReconcileOutcome {
            added,
            removed,
            activation_must_close,
        })
    }

    /// Suspend is fail-safe: try explicit ungrabs, then close all descriptors.
    /// Closing guarantees kernel grab release even if an ioctl failed. Resume
    /// reconstructs the set from the next periodic scan.
    pub fn suspend(&mut self) -> Vec<UngrabFailure> {
        let failures = self.ungrab_all();
        self.nodes.clear();
        self.aggregate = AggregateInputState::default();
        self.grabbed = false;
        failures
    }
}

impl Drop for CaptureSet {
    fn drop(&mut self) {
        let _ = self.ungrab_all();
        // Device fd close is the final, infallible-with-respect-to-EVIOCGRAB
        // safety net performed by the evdev Device drop implementation.
    }
}

fn is_device_removed(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
        || error.raw_os_error() == Some(libc::ENODEV)
        || error.raw_os_error() == Some(libc::ENXIO)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FakeTarget {
        path: PathBuf,
        fail_grab: bool,
        fail_release: bool,
        grabbed: bool,
        releases: usize,
    }

    impl GrabTarget for FakeTarget {
        fn target_path(&self) -> &Path {
            &self.path
        }

        fn take_grab(&mut self) -> io::Result<()> {
            if self.fail_grab {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            self.grabbed = true;
            Ok(())
        }

        fn release_grab(&mut self) -> io::Result<()> {
            self.releases += 1;
            if self.fail_release {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            self.grabbed = false;
            Ok(())
        }
    }

    #[test]
    fn grab_failure_rolls_back_every_earlier_device() {
        let mut targets = [
            FakeTarget {
                path: "one".into(),
                fail_grab: false,
                fail_release: false,
                grabbed: false,
                releases: 0,
            },
            FakeTarget {
                path: "two".into(),
                fail_grab: false,
                fail_release: false,
                grabbed: false,
                releases: 0,
            },
            FakeTarget {
                path: "three".into(),
                fail_grab: true,
                fail_release: false,
                grabbed: false,
                releases: 0,
            },
        ];
        let error = grab_transaction(&mut targets).unwrap_err();
        assert_eq!(error.path, PathBuf::from("three"));
        assert_eq!(error.rolled_back, 2);
        assert!(targets.iter().all(|target| !target.grabbed));
        assert_eq!(targets[0].releases, 1);
        assert_eq!(targets[1].releases, 1);
    }

    #[test]
    fn grab_failure_reports_a_failed_rollback_that_still_holds_the_device() {
        let mut targets = [
            FakeTarget {
                path: "one".into(),
                fail_grab: false,
                fail_release: true,
                grabbed: false,
                releases: 0,
            },
            FakeTarget {
                path: "two".into(),
                fail_grab: true,
                fail_release: false,
                grabbed: false,
                releases: 0,
            },
        ];

        let error = grab_transaction(&mut targets).unwrap_err();

        assert_eq!(error.rollback_failures.len(), 1);
        assert!(targets[0].grabbed);
        assert_eq!(targets[0].releases, 1);
    }
}
