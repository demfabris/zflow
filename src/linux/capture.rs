use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    time::Instant,
};

use evdev::{InputEvent, KeyCode, raw_stream::RawDevice};
use thiserror::Error;

use crate::capture::CapturedDeviceFrame;

use super::{AggregateInputState, DeviceInfo, FrameAccumulator, MappingError, TouchAccumulator};

#[derive(Debug)]
struct CaptureNode {
    path: PathBuf,
    /// Raw reads pass SYN_DROPPED through. The synced reader hides it and
    /// rebuilds state on its own, which leaves stale multitouch slots.
    device: RawDevice,
    frames: FrameAccumulator,
    touch: Option<TouchAccumulator>,
}

impl CaptureNode {
    fn open(info: &DeviceInfo) -> io::Result<(Self, Vec<KeyCode>)> {
        // Capture only reads and grabs, and setup grants read access alone.
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&info.path)?;
        let device = RawDevice::try_from(file)?;
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

    /// mio registers descriptors edge-triggered, and one fetch reads only a
    /// batch. Drain until the kernel buffer is empty so a trailing key-up or
    /// finger lift is not left behind until the device reports again.
    fn fetch_events(&mut self) -> io::Result<Vec<InputEvent>> {
        let mut events = Vec::new();
        loop {
            match self.device.fetch_events() {
                Ok(batch) => events.extend(batch),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(events),
                Err(error) => return Err(error),
            }
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

/// Grabs every target or none. With `skip_busy`, a target another program
/// already grabbed (EBUSY) is skipped instead, as long as at least one other
/// target is grabbed; the skipped paths are returned. A skipped target was
/// never ours, so rollback never releases it.
fn grab_transaction<T: GrabTarget>(
    targets: &mut [T],
    skip_busy: bool,
) -> Result<Vec<PathBuf>, GrabError> {
    let mut grabbed = Vec::with_capacity(targets.len());
    let mut skipped = Vec::new();
    for index in 0..targets.len() {
        let path = targets[index].target_path().to_owned();
        match targets[index].take_grab() {
            Ok(()) => grabbed.push(index),
            Err(error) if skip_busy && error.raw_os_error() == Some(libc::EBUSY) => {
                skipped.push(path);
            }
            Err(source) => return Err(roll_back(targets, &grabbed, path, source)),
        }
    }
    if grabbed.is_empty()
        && let Some(path) = skipped.pop()
    {
        return Err(GrabError {
            path,
            source: io::Error::from_raw_os_error(libc::EBUSY),
            rolled_back: 0,
            rollback_failures: Vec::new(),
        });
    }
    Ok(skipped)
}

fn roll_back<T: GrabTarget>(
    targets: &mut [T],
    grabbed: &[usize],
    path: PathBuf,
    source: io::Error,
) -> GrabError {
    let mut rollback_failures = Vec::new();
    for &index in grabbed.iter().rev() {
        if let Err(error) = targets[index].release_grab() {
            rollback_failures.push(UngrabFailure {
                path: targets[index].target_path().to_owned(),
                error,
            });
        }
    }
    GrabError {
        path,
        source,
        rolled_back: grabbed.len(),
        rollback_failures,
    }
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

    /// Takes every node's held keys from the kernel. A node another program
    /// grabbed, as keyd grabs its source keyboard, sends zflow no events, so
    /// a key held on it when zflow opened it would otherwise stay held here
    /// and block every activation.
    pub fn resync_held(&mut self) {
        for node in &self.nodes {
            if let Ok(held) = node.device.get_key_state() {
                self.aggregate.resync_held(&node.path, held.iter());
            }
        }
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
    ///
    /// With `skip_busy`, used when capturing every keyboard and pointer, a
    /// node another program already grabbed is left to it and returned. A
    /// remapper such as keyd holds its source that way and re-emits on a
    /// virtual node, which is grabbed instead.
    pub fn grab_all(&mut self, skip_busy: bool) -> Result<Vec<PathBuf>, CaptureSetError> {
        if self.nodes.is_empty() {
            return Err(CaptureSetError::Empty);
        }
        if self.grabbed {
            return Ok(Vec::new());
        }
        if !self.aggregate.is_neutral() || !self.aggregate.all_at_boundary() {
            return Err(CaptureSetError::NotNeutral);
        }
        if !self.kernel_is_neutral()? {
            return Err(CaptureSetError::NotNeutral);
        }
        let skipped = grab_transaction(&mut self.nodes, skip_busy)?;
        self.grabbed = true;

        match self.kernel_is_neutral() {
            Ok(true) => Ok(skipped),
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

    /// Releases only nodes this process grabbed. A node skipped as busy stays
    /// with the program that holds it.
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

        let node = &mut self.nodes[index];
        map_events(
            path,
            &mut self.aggregate,
            &mut node.frames,
            node.touch.as_mut(),
            events,
        )
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

    /// Fail-safe teardown: try explicit ungrabs, then close all descriptors.
    /// Closing guarantees kernel grab release even if an ioctl failed. The
    /// next periodic scan reopens the set.
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

/// Maps one batch of raw events into complete frames. SYN_DROPPED fails the
/// batch, so the runtime closes the capture set and rebuilds it from fresh
/// kernel state instead of guessing what was lost.
fn map_events(
    path: &Path,
    aggregate: &mut AggregateInputState,
    frames: &mut FrameAccumulator,
    mut touch: Option<&mut TouchAccumulator>,
    events: Vec<InputEvent>,
) -> Result<Vec<CapturedDeviceFrame>, CaptureReadError> {
    let mut captured = Vec::new();
    for event in events {
        aggregate.observe(path, event);
        let touch_state = touch
            .as_deref_mut()
            .map(|touch| touch.push(event))
            .transpose()
            .map_err(|_| CaptureReadError::Mapping {
                path: path.to_owned(),
                source: MappingError::InvalidTouchState,
            })?
            .flatten();
        match frames.push(event) {
            Ok(Some(mut frame)) => {
                if let Some((state, event_count)) = touch_state {
                    frame.touch_snapshot = Some(state);
                    frame.event_count = frame.event_count.saturating_add(event_count);
                }
                captured.push(CapturedDeviceFrame {
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
    Ok(captured)
}

fn is_device_removed(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
        || error.raw_os_error() == Some(libc::ENODEV)
        || error.raw_os_error() == Some(libc::ENXIO)
}

#[cfg(test)]
mod tests {
    use evdev::{AbsoluteAxisCode, EventType, SynchronizationCode};

    use super::*;
    use crate::linux::TouchAxisRange;

    fn event(kind: EventType, code: u16, value: i32) -> InputEvent {
        InputEvent::new(kind.0, code, value)
    }

    fn sync(code: SynchronizationCode) -> InputEvent {
        event(EventType::SYNCHRONIZATION, code.0, 0)
    }

    fn finger_down() -> Vec<InputEvent> {
        vec![
            event(EventType::ABSOLUTE, AbsoluteAxisCode::ABS_MT_SLOT.0, 0),
            event(
                EventType::ABSOLUTE,
                AbsoluteAxisCode::ABS_MT_TRACKING_ID.0,
                7,
            ),
            event(
                EventType::ABSOLUTE,
                AbsoluteAxisCode::ABS_MT_POSITION_X.0,
                40,
            ),
            event(
                EventType::ABSOLUTE,
                AbsoluteAxisCode::ABS_MT_POSITION_Y.0,
                30,
            ),
            sync(SynchronizationCode::SYN_REPORT),
        ]
    }

    fn touchpad() -> TouchAccumulator {
        TouchAccumulator::new(
            TouchAxisRange::new(0, 100, 10).unwrap(),
            TouchAxisRange::new(0, 100, 10).unwrap(),
        )
    }

    #[test]
    fn raw_batches_map_complete_frames_with_touch_state() {
        let path = Path::new("/dev/input/event4");
        let mut aggregate = AggregateInputState::default();
        aggregate.add_device(path, []);
        let mut frames = FrameAccumulator::default();
        let mut touch = touchpad();
        let captured = map_events(
            path,
            &mut aggregate,
            &mut frames,
            Some(&mut touch),
            finger_down(),
        )
        .unwrap();
        assert_eq!(captured.len(), 1);
        let state = captured[0].frame.touch_snapshot.as_ref().unwrap();
        assert_eq!(state.len(), 1);
        assert!(aggregate.all_at_boundary());
    }

    #[test]
    fn syn_dropped_fails_closed_for_keys_and_touch() {
        let path = Path::new("/dev/input/event4");
        let mut aggregate = AggregateInputState::default();
        aggregate.add_device(path, []);
        let keys = vec![
            event(EventType::KEY, KeyCode::KEY_A.code(), 1),
            sync(SynchronizationCode::SYN_REPORT),
            sync(SynchronizationCode::SYN_DROPPED),
            event(EventType::KEY, KeyCode::KEY_A.code(), 0),
            sync(SynchronizationCode::SYN_REPORT),
        ];
        let error = map_events(
            path,
            &mut aggregate,
            &mut FrameAccumulator::default(),
            None,
            keys,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            CaptureReadError::Mapping {
                source: MappingError::SynchronizationLost,
                ..
            }
        ));

        // A lifted finger lost in the overflow must not stay down.
        let mut touch = touchpad();
        let mut lost_lift = finger_down();
        lost_lift.push(sync(SynchronizationCode::SYN_DROPPED));
        let error = map_events(
            path,
            &mut aggregate,
            &mut FrameAccumulator::default(),
            Some(&mut touch),
            lost_lift,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            CaptureReadError::Mapping {
                source: MappingError::InvalidTouchState,
                ..
            }
        ));
    }

    #[derive(Debug)]
    struct FakeTarget {
        path: PathBuf,
        /// The errno a grab fails with, such as EBUSY for a node another
        /// program already grabbed.
        grab_error: Option<i32>,
        fail_release: bool,
        grabbed: bool,
        releases: usize,
    }

    impl FakeTarget {
        fn new(path: &str) -> Self {
            Self {
                path: path.into(),
                grab_error: None,
                fail_release: false,
                grabbed: false,
                releases: 0,
            }
        }

        fn failing(path: &str, errno: i32) -> Self {
            Self {
                grab_error: Some(errno),
                ..Self::new(path)
            }
        }
    }

    impl GrabTarget for FakeTarget {
        fn target_path(&self) -> &Path {
            &self.path
        }

        fn take_grab(&mut self) -> io::Result<()> {
            if let Some(errno) = self.grab_error {
                return Err(io::Error::from_raw_os_error(errno));
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
            FakeTarget::new("one"),
            FakeTarget::new("two"),
            FakeTarget::failing("three", libc::EPERM),
        ];
        let error = grab_transaction(&mut targets, false).unwrap_err();
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
                fail_release: true,
                ..FakeTarget::new("one")
            },
            FakeTarget::failing("two", libc::EPERM),
        ];

        let error = grab_transaction(&mut targets, false).unwrap_err();

        assert_eq!(error.rollback_failures.len(), 1);
        assert!(targets[0].grabbed);
        assert_eq!(targets[0].releases, 1);
    }

    #[test]
    fn capture_all_skips_a_node_another_program_grabbed() {
        // keyd holds the physical keyboard and types through its own node.
        let mut targets = [
            FakeTarget::new("keyd virtual keyboard"),
            FakeTarget::failing("keyboard", libc::EBUSY),
            FakeTarget::new("mouse"),
        ];

        let skipped = grab_transaction(&mut targets, true).unwrap();

        assert_eq!(skipped, [PathBuf::from("keyboard")]);
        assert!(targets[0].grabbed && targets[2].grabbed);
        assert!(!targets[1].grabbed);
        assert!(targets.iter().all(|target| target.releases == 0));
    }

    #[test]
    fn configured_devices_still_roll_back_on_a_busy_node() {
        let mut targets = [
            FakeTarget::new("keyd virtual keyboard"),
            FakeTarget::failing("keyboard", libc::EBUSY),
            FakeTarget::new("mouse"),
        ];

        let error = grab_transaction(&mut targets, false).unwrap_err();

        assert_eq!(error.path, PathBuf::from("keyboard"));
        assert_eq!(error.source.raw_os_error(), Some(libc::EBUSY));
        assert_eq!(error.rolled_back, 1);
        assert!(targets.iter().all(|target| !target.grabbed));
        assert_eq!(targets[0].releases, 1);
        assert_eq!(targets[2].releases, 0);
    }

    #[test]
    fn capture_all_fails_when_every_node_is_busy() {
        let mut targets = [
            FakeTarget::failing("keyboard", libc::EBUSY),
            FakeTarget::failing("mouse", libc::EBUSY),
        ];

        let error = grab_transaction(&mut targets, true).unwrap_err();

        assert_eq!(error.source.raw_os_error(), Some(libc::EBUSY));
        assert_eq!(error.rolled_back, 0);
        assert!(targets.iter().all(|target| target.releases == 0));
    }

    #[test]
    fn rollback_releases_only_nodes_this_process_grabbed() {
        let mut targets = [
            FakeTarget::new("one"),
            FakeTarget::failing("busy", libc::EBUSY),
            FakeTarget::new("two"),
            FakeTarget::failing("broken", libc::EIO),
        ];

        let error = grab_transaction(&mut targets, true).unwrap_err();

        assert_eq!(error.path, PathBuf::from("broken"));
        assert_eq!(error.rolled_back, 2);
        assert!(targets.iter().all(|target| !target.grabbed));
        assert_eq!(
            targets.each_ref().map(|target| target.releases),
            [1, 0, 1, 0]
        );
    }
}
