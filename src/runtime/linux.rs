use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    os::fd::RawFd,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use evdev::KeyCode;
use mio::{Events, Interest, Poll, Token, Waker, unix::SourceFd};
use sd_notify::NotifyState;
use thiserror::Error;

use crate::{
    config::{Config, DeviceSelector as ConfigDeviceSelector},
    core::{HidUsage, Modifier, PointerButton, ReceiverEffect},
    linux::{
        CaptureFrame, CaptureReadError, CaptureSet, CaptureSetError, CaptureTransition,
        CapturedDeviceFrame, DeviceInfo, InjectionError, KeyState, OwnershipEffect, OwnershipPhase,
        PeriodicDeviceScanner, SourceOwnership, VirtualInput, enumerate_devices,
        evdev_button_to_pointer, evdev_key_to_hid,
    },
};

use super::{ReadinessPaths, VirtualDeviceReadiness, probe_virtual_device_readiness_in};

const WAKE_TOKEN: Token = Token(0);
const FIRST_DEVICE_TOKEN: usize = 1;
const MAX_COMMANDS_PER_TICK: usize = 256;
const MAX_IDLE_POLL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct LinuxRuntimeConfig {
    pub capture_devices: Vec<ConfigDeviceSelector>,
    pub activation_chord: Vec<String>,
    pub escape_chord: Vec<String>,
    pub experimental_touchpad: bool,
    pub rescan_interval: Duration,
    pub readiness_probe_interval: Duration,
    pub command_capacity: usize,
    pub event_capacity: usize,
    pub capture_capacity: usize,
    pub readiness_paths: ReadinessPaths,
}

impl LinuxRuntimeConfig {
    pub fn from_config(config: &Config) -> Self {
        Self {
            capture_devices: config.input.capture_devices.clone(),
            activation_chord: config.input.activation_chord.clone(),
            escape_chord: config.input.escape_chord.clone(),
            experimental_touchpad: config.input.experimental_touchpad,
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), LinuxRuntimeError> {
        if self.rescan_interval.is_zero() {
            return Err(LinuxRuntimeError::InvalidConfig(
                "device rescan interval must be non-zero",
            ));
        }
        if self.readiness_probe_interval.is_zero() {
            return Err(LinuxRuntimeError::InvalidConfig(
                "readiness probe interval must be non-zero",
            ));
        }
        if self.command_capacity == 0 || self.event_capacity == 0 || self.capture_capacity == 0 {
            return Err(LinuxRuntimeError::InvalidConfig(
                "runtime channel capacities must be non-zero",
            ));
        }
        ConfiguredChord::parse(&self.activation_chord, ChordPurpose::Activation)?;
        ConfiguredChord::parse(&self.escape_chord, ChordPurpose::Escape)?;
        Ok(())
    }
}

impl Default for LinuxRuntimeConfig {
    fn default() -> Self {
        Self {
            capture_devices: Vec::new(),
            activation_chord: vec![
                "KEY_LEFTCTRL".into(),
                "KEY_LEFTMETA".into(),
                "KEY_F12".into(),
            ],
            escape_chord: vec![
                "KEY_LEFTCTRL".into(),
                "KEY_LEFTMETA".into(),
                "KEY_BACKSPACE".into(),
            ],
            experimental_touchpad: false,
            rescan_interval: Duration::from_secs(2),
            readiness_probe_interval: Duration::from_millis(25),
            command_capacity: 256,
            event_capacity: 256,
            capture_capacity: 1024,
            readiness_paths: ReadinessPaths::default(),
        }
    }
}

/// Commands are deliberately content-opaque at the runtime boundary. This
/// type has no `Debug` implementation so an accidental command trace cannot
/// print receiver key effects.
pub enum RuntimeCommand {
    Activate {
        peer: String,
    },
    Release {
        transport_live: bool,
    },
    TerminalSent {
        transport_live: bool,
    },
    ReceiverEffects {
        effects: Vec<ReceiverEffect>,
        /// Sent only after every effect reaches the uinput backend.
        applied: Option<tokio::sync::oneshot::Sender<Instant>>,
    },
    Suspend,
    Resume,
    Reload {
        config: LinuxRuntimeConfig,
        applied: tokio::sync::oneshot::Sender<Result<(), LinuxRuntimeError>>,
    },
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeCloseReason {
    LocalRelease,
    TransportLost,
    DeviceRemoved,
    CaptureFault,
    BackendFault,
    Backpressure,
    Suspend,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeDiagnostic {
    CaptureSelectionIncomplete,
    CaptureOpenFailed,
    CaptureReadFailed,
    CaptureMappingLost,
    GrabFailed,
    UngrabRequiredDescriptorClose,
    CapturedFrameQueueFull,
    InjectionFailed,
    ReadinessProbeFailed,
    ServiceNotificationFailed,
    EventQueueFull,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    Ready,
    ActivationChord,
    OwnershipChanged {
        phase: OwnershipPhase,
        selected_peer: Option<String>,
        changed_at: Instant,
        /// Mappable evdev events observed after activation was requested but
        /// before EVIOCGRAB completed.
        arming_leakage_events: u64,
    },
    TerminalRequested,
    ActivationClosed(RuntimeCloseReason),
    ReceiverStateReleased(RuntimeCloseReason),
    Diagnostic(RuntimeDiagnostic),
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeDeviceStatus {
    pub path: PathBuf,
    pub grabbed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxRuntimeStatus {
    pub running: bool,
    pub suspended: bool,
    pub ready: bool,
    pub virtual_devices: VirtualDeviceReadiness,
    pub ownership: OwnershipPhase,
    pub selected_peer: Option<String>,
    pub capture_devices: Vec<RuntimeDeviceStatus>,
    pub capture_selection_complete: bool,
    pub unmatched_selectors: usize,
    pub ambiguous_selectors: usize,
    pub dropped_events: u64,
    pub dropped_capture_frames: u64,
    pub last_diagnostic: Option<RuntimeDiagnostic>,
}

/// Read-only capture resolution used by setup diagnostics.
///
/// This deliberately shares the runtime's exact all-or-none selector rules,
/// so `zflow doctor` cannot claim a configuration is usable when the daemon
/// would refuse to grab it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSelectionDiagnostic {
    pub selected_paths: Vec<PathBuf>,
    pub configured: usize,
    pub unmatched: usize,
    pub ambiguous: usize,
    pub scan_failures: usize,
}

impl CaptureSelectionDiagnostic {
    pub fn is_complete(&self) -> bool {
        self.configured > 0 && self.unmatched == 0 && self.ambiguous == 0
    }
}

pub fn diagnose_capture_selection(
    selectors: &[ConfigDeviceSelector],
) -> io::Result<CaptureSelectionDiagnostic> {
    let scan = enumerate_devices()?;
    let scan_failures = scan.failures.len();
    let selection = select_configured(selectors, scan.devices.iter());
    Ok(CaptureSelectionDiagnostic {
        selected_paths: selection
            .capture_set()
            .iter()
            .map(|device| device.path.clone())
            .collect(),
        configured: selection.configured,
        unmatched: selection.unmatched,
        ambiguous: selection.ambiguous,
        scan_failures,
    })
}

impl Default for LinuxRuntimeStatus {
    fn default() -> Self {
        Self {
            running: false,
            suspended: false,
            ready: false,
            virtual_devices: VirtualDeviceReadiness::default(),
            ownership: OwnershipPhase::Idle,
            selected_peer: None,
            capture_devices: Vec::new(),
            capture_selection_complete: false,
            unmatched_selectors: 0,
            ambiguous_selectors: 0,
            dropped_events: 0,
            dropped_capture_frames: 0,
            last_diagnostic: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum LinuxRuntimeError {
    #[error("invalid Linux runtime configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("could not create the Linux input poller: {0}")]
    Poll(#[source] io::Error),
    #[error("could not spawn the Linux input thread: {0}")]
    Spawn(#[source] io::Error),
    #[error("Linux input runtime startup failed: {0}")]
    Startup(#[from] RuntimeStartupError),
    #[error("Linux input runtime exited during startup")]
    StartupChannelClosed,
    #[error("Linux input runtime thread panicked")]
    ThreadPanicked,
    #[error("could not replace the virtual input devices during reload: {0}")]
    ReloadVirtualInput(#[source] InjectionError),
}

#[derive(Debug, Error)]
pub enum RuntimeStartupError {
    #[error("could not create the virtual input devices: {0}")]
    VirtualInput(#[from] InjectionError),
    #[error("could not scan physical input devices: {0}")]
    DeviceScan(#[source] io::Error),
    #[error("could not open the configured capture set: {0}")]
    Capture(#[from] CaptureSetError),
    #[error("could not register a capture descriptor: {0}")]
    Register(#[source] io::Error),
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeCommandError {
    #[error("Linux input runtime command queue is full")]
    Full,
    #[error("Linux input runtime has stopped")]
    Stopped,
    #[error("could not wake the Linux input runtime")]
    Wake,
}

#[derive(Clone)]
pub struct LinuxRuntimeControl {
    commands: SyncSender<RuntimeCommand>,
    waker: Arc<Waker>,
    status: Arc<Mutex<LinuxRuntimeStatus>>,
    alive: Arc<AtomicBool>,
}

impl LinuxRuntimeControl {
    pub fn send(&self, command: RuntimeCommand) -> Result<(), RuntimeCommandError> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(RuntimeCommandError::Stopped);
        }
        match self.commands.try_send(command) {
            Ok(()) => self.waker.wake().map_err(|_| RuntimeCommandError::Wake),
            Err(TrySendError::Full(_)) => Err(RuntimeCommandError::Full),
            Err(TrySendError::Disconnected(_)) => Err(RuntimeCommandError::Stopped),
        }
    }

    /// Delivers a fail-safe lifecycle command through temporary queue
    /// backpressure. Callers must tear down the daemon if this bounded retry
    /// still fails, because the descriptor-owning thread cannot otherwise know
    /// that a terminal write completed.
    pub fn send_critical(
        &self,
        mut command: RuntimeCommand,
        timeout: Duration,
    ) -> Result<(), RuntimeCommandError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            if !self.alive.load(Ordering::Acquire) {
                return Err(RuntimeCommandError::Stopped);
            }
            match self.commands.try_send(command) {
                Ok(()) => return self.waker.wake().map_err(|_| RuntimeCommandError::Wake),
                Err(TrySendError::Full(returned)) => {
                    command = returned;
                    if Instant::now() >= deadline {
                        return Err(RuntimeCommandError::Full);
                    }
                    self.waker.wake().map_err(|_| RuntimeCommandError::Wake)?;
                    thread::sleep(Duration::from_millis(1));
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(RuntimeCommandError::Stopped);
                }
            }
        }
    }

    pub fn status(&self) -> LinuxRuntimeStatus {
        lock_status(&self.status).clone()
    }
}

pub struct LinuxRuntime {
    control: LinuxRuntimeControl,
    events: Receiver<RuntimeEvent>,
    captured_frames: Receiver<CapturedDeviceFrame>,
    thread: Option<JoinHandle<()>>,
}

impl LinuxRuntime {
    pub fn spawn(config: LinuxRuntimeConfig) -> Result<Self, LinuxRuntimeError> {
        config.validate()?;
        let poll = Poll::new().map_err(LinuxRuntimeError::Poll)?;
        let waker =
            Arc::new(Waker::new(poll.registry(), WAKE_TOKEN).map_err(LinuxRuntimeError::Poll)?);
        let (command_tx, command_rx) = mpsc::sync_channel(config.command_capacity);
        let (event_tx, event_rx) = mpsc::sync_channel(config.event_capacity);
        let (capture_tx, capture_rx) = mpsc::sync_channel(config.capture_capacity);
        let (startup_tx, startup_rx) = mpsc::sync_channel(1);
        let status = Arc::new(Mutex::new(LinuxRuntimeStatus::default()));
        let alive = Arc::new(AtomicBool::new(true));

        let thread_status = status.clone();
        let thread_alive = alive.clone();
        let thread = thread::Builder::new()
            .name("zflow-linux-input".into())
            .spawn(move || {
                match RuntimeLoop::new(
                    poll,
                    config,
                    command_rx,
                    event_tx,
                    capture_tx,
                    thread_status.clone(),
                ) {
                    Ok(mut runtime) => {
                        let _ = startup_tx.send(Ok(()));
                        runtime.run();
                    }
                    Err(error) => {
                        let _ = startup_tx.send(Err(error));
                    }
                }
                thread_alive.store(false, Ordering::Release);
                let mut status = lock_status(&thread_status);
                status.running = false;
                status.ready = false;
            })
            .map_err(LinuxRuntimeError::Spawn)?;

        match startup_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                control: LinuxRuntimeControl {
                    commands: command_tx,
                    waker,
                    status,
                    alive,
                },
                events: event_rx,
                captured_frames: capture_rx,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error.into())
            }
            Err(_) => {
                let _ = thread.join();
                Err(LinuxRuntimeError::StartupChannelClosed)
            }
        }
    }

    pub fn control(&self) -> LinuxRuntimeControl {
        self.control.clone()
    }

    pub fn events(&self) -> &Receiver<RuntimeEvent> {
        &self.events
    }

    pub fn captured_frames(&self) -> &Receiver<CapturedDeviceFrame> {
        &self.captured_frames
    }

    pub fn status(&self) -> LinuxRuntimeStatus {
        self.control.status()
    }

    pub fn shutdown(mut self) -> Result<(), LinuxRuntimeError> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<(), LinuxRuntimeError> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        // A full command queue must not turn normal teardown into a detached
        // descriptor-owning thread. Wake the loop, then wait for one bounded
        // channel slot so Stop is guaranteed to be observed.
        let _ = self.control.waker.wake();
        let _ = self.control.commands.send(RuntimeCommand::Stop);
        thread.join().map_err(|_| LinuxRuntimeError::ThreadPanicked)
    }
}

impl Drop for LinuxRuntime {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ChordMember {
    Key(HidUsage),
    Button(PointerButton),
}

#[derive(Debug, Clone, Copy)]
enum ChordPurpose {
    Activation,
    Escape,
}

#[derive(Debug, Clone)]
struct ConfiguredChord(BTreeSet<ChordMember>);

impl ConfiguredChord {
    fn parse(names: &[String], _purpose: ChordPurpose) -> Result<Self, LinuxRuntimeError> {
        if names.is_empty() {
            return Err(LinuxRuntimeError::InvalidConfig(
                "input chords must not be empty",
            ));
        }
        let mut members = BTreeSet::new();
        for name in names {
            let key = KeyCode::from_str(name).map_err(|_| {
                LinuxRuntimeError::InvalidConfig("an input chord contains an unknown key name")
            })?;
            let member = if key.code() >= KeyCode::BTN_LEFT.code()
                && key.code() <= KeyCode::BTN_TASK.code()
            {
                ChordMember::Button(evdev_button_to_pointer(key).map_err(|_| {
                    LinuxRuntimeError::InvalidConfig("an input chord contains an unsupported key")
                })?)
            } else {
                ChordMember::Key(evdev_key_to_hid(key).map_err(|_| {
                    LinuxRuntimeError::InvalidConfig("an input chord contains an unsupported key")
                })?)
            };
            if !members.insert(member) {
                return Err(LinuxRuntimeError::InvalidConfig(
                    "an input chord contains a duplicate key",
                ));
            }
        }
        Ok(Self(members))
    }
}

#[derive(Debug, Clone)]
struct ChordTracker {
    chord: ConfiguredChord,
    held_by_device: BTreeMap<PathBuf, BTreeSet<ChordMember>>,
    latched: bool,
}

impl ChordTracker {
    fn new(chord: ConfiguredChord) -> Self {
        Self {
            chord,
            held_by_device: BTreeMap::new(),
            latched: false,
        }
    }

    fn observe(&mut self, path: &Path, frame: &CaptureFrame) -> bool {
        let held = self.held_by_device.entry(path.to_owned()).or_default();
        for transition in &frame.transitions {
            let (member, state) = match transition {
                CaptureTransition::Key { usage, state } => (ChordMember::Key(*usage), *state),
                CaptureTransition::Button { button, state } => {
                    (ChordMember::Button(*button), *state)
                }
            };
            match state {
                KeyState::Pressed | KeyState::Repeat => {
                    held.insert(member);
                }
                KeyState::Released => {
                    held.remove(&member);
                }
            }
        }
        let complete = self.chord.0.iter().all(|member| {
            self.held_by_device
                .values()
                .any(|held| held.contains(member))
        });
        let triggered = complete && !self.latched;
        self.latched = complete;
        triggered
    }

    fn remove_device(&mut self, path: &Path) {
        self.held_by_device.remove(path);
        self.latched = self.chord.0.iter().all(|member| {
            self.held_by_device
                .values()
                .any(|held| held.contains(member))
        });
    }

    fn clear(&mut self) {
        self.held_by_device.clear();
        self.latched = false;
    }
}

#[derive(Debug)]
struct RegisteredDevice {
    path: PathBuf,
    fd: RawFd,
}

struct RuntimeLoop {
    poll: Poll,
    poll_events: Events,
    commands: Receiver<RuntimeCommand>,
    event_tx: SyncSender<RuntimeEvent>,
    capture_tx: SyncSender<CapturedDeviceFrame>,
    status: Arc<Mutex<LinuxRuntimeStatus>>,
    config: LinuxRuntimeConfig,
    scanner: PeriodicDeviceScanner,
    capture: CaptureSet,
    ownership: SourceOwnership,
    virtual_input: VirtualInput,
    activation_chord: ChordTracker,
    escape_chord: ChordTracker,
    selected_peer: Option<String>,
    arming_leakage_events: u64,
    capture_selection_complete: bool,
    registrations: BTreeMap<Token, RegisteredDevice>,
    next_device_token: usize,
    next_rescan: Instant,
    next_readiness_probe: Instant,
    next_watchdog: Option<Instant>,
    watchdog_period: Option<Duration>,
    ready_notified: bool,
    suspended: bool,
    stopping: bool,
}

impl RuntimeLoop {
    fn new(
        poll: Poll,
        config: LinuxRuntimeConfig,
        commands: Receiver<RuntimeCommand>,
        event_tx: SyncSender<RuntimeEvent>,
        capture_tx: SyncSender<CapturedDeviceFrame>,
        status: Arc<Mutex<LinuxRuntimeStatus>>,
    ) -> Result<Self, RuntimeStartupError> {
        let virtual_input = VirtualInput::create(config.experimental_touchpad)?;
        let activation_chord = ChordTracker::new(
            ConfiguredChord::parse(&config.activation_chord, ChordPurpose::Activation)
                .expect("runtime configuration was validated before thread startup"),
        );
        let escape_chord = ChordTracker::new(
            ConfiguredChord::parse(&config.escape_chord, ChordPurpose::Escape)
                .expect("runtime configuration was validated before thread startup"),
        );
        let mut scanner = PeriodicDeviceScanner::new(config.rescan_interval);
        scanner.refresh().map_err(RuntimeStartupError::DeviceScan)?;
        let selection = select_configured(&config.capture_devices, scanner.known());
        let capture = if selection.capture_set().is_empty() {
            CaptureSet::default()
        } else {
            CaptureSet::open(selection.capture_set())?
        };
        let now = Instant::now();
        let watchdog_period = watchdog_tick_interval(sd_notify::watchdog_enabled());
        let mut runtime = Self {
            poll,
            poll_events: Events::with_capacity(128),
            commands,
            event_tx,
            capture_tx,
            status,
            config,
            scanner,
            capture,
            ownership: SourceOwnership::default(),
            virtual_input,
            activation_chord,
            escape_chord,
            selected_peer: None,
            arming_leakage_events: 0,
            capture_selection_complete: selection.is_complete(),
            registrations: BTreeMap::new(),
            next_device_token: FIRST_DEVICE_TOKEN,
            next_rescan: now,
            next_readiness_probe: now,
            next_watchdog: watchdog_period.map(|period| now + period),
            watchdog_period,
            ready_notified: false,
            suspended: false,
            stopping: false,
        };
        runtime.register_capture_descriptors()?;
        {
            let mut status = lock_status(&runtime.status);
            status.running = true;
            status.capture_selection_complete = selection.is_complete();
            status.unmatched_selectors = selection.unmatched;
            status.ambiguous_selectors = selection.ambiguous;
        }
        runtime.refresh_status();
        if let Some(diagnostic) = capture_selection_diagnostic(None, selection.is_complete()) {
            runtime.diagnostic(diagnostic);
        }
        Ok(runtime)
    }

    fn run(&mut self) {
        while !self.stopping {
            // Also inspect the queue without relying on a distinct Waker edge:
            // mio coalesces wakes, and a capped drain may leave work queued.
            self.drain_commands();
            self.service_timers();
            if self.stopping {
                break;
            }
            let timeout = self.poll_timeout(Instant::now());
            if self
                .poll
                .poll(&mut self.poll_events, Some(timeout))
                .is_err()
            {
                self.diagnostic(RuntimeDiagnostic::CaptureReadFailed);
                self.force_source_release(RuntimeCloseReason::CaptureFault);
                break;
            }

            let tokens = self
                .poll_events
                .iter()
                .map(|event| event.token())
                .collect::<Vec<_>>();
            for token in tokens {
                if token == WAKE_TOKEN {
                    self.drain_commands();
                } else if let Some(path) = self
                    .registrations
                    .get(&token)
                    .map(|registration| registration.path.clone())
                {
                    self.read_capture_path(&path);
                }
                if self.stopping {
                    break;
                }
            }
            self.service_timers();
        }
        self.stop_all();
    }

    fn service_timers(&mut self) {
        let now = Instant::now();
        if self.next_watchdog.is_some_and(|deadline| now >= deadline) {
            if sd_notify::notify(&[NotifyState::Watchdog]).is_err() {
                self.diagnostic(RuntimeDiagnostic::ServiceNotificationFailed);
            }
            self.next_watchdog = self.watchdog_period.map(|period| now + period);
        }
        if !self.suspended && now >= self.next_rescan {
            self.rescan();
            self.next_rescan = now + self.config.rescan_interval;
        }
        if !self.ready_notified && now >= self.next_readiness_probe {
            self.probe_readiness();
            self.next_readiness_probe = now + self.config.readiness_probe_interval;
        }
    }

    fn poll_timeout(&self, now: Instant) -> Duration {
        let mut next = now + MAX_IDLE_POLL;
        if !self.suspended {
            next = next.min(self.next_rescan);
        }
        if !self.ready_notified {
            next = next.min(self.next_readiness_probe);
        }
        if let Some(watchdog) = self.next_watchdog {
            next = next.min(watchdog);
        }
        next.saturating_duration_since(now)
    }

    fn probe_readiness(&mut self) {
        let paths = &self.config.readiness_paths;
        match probe_virtual_device_readiness_in(
            &paths.input_dir,
            &paths.sys_class_input_dir,
            &paths.udev_database_dir,
        ) {
            Ok(readiness) => {
                lock_status(&self.status).virtual_devices = readiness;
                if readiness.is_ready_for(self.config.experimental_touchpad) {
                    if sd_notify::notify(&[NotifyState::Ready]).is_ok() {
                        self.ready_notified = true;
                        lock_status(&self.status).ready = true;
                        self.emit(RuntimeEvent::Ready);
                    } else {
                        self.diagnostic(RuntimeDiagnostic::ServiceNotificationFailed);
                    }
                }
            }
            Err(_) => self.diagnostic(RuntimeDiagnostic::ReadinessProbeFailed),
        }
    }

    fn drain_commands(&mut self) {
        for index in 0..MAX_COMMANDS_PER_TICK {
            match self.commands.try_recv() {
                Ok(command) => self.handle_command(command),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.stopping = true;
                    break;
                }
            }
            if self.stopping {
                break;
            }
            if index + 1 == MAX_COMMANDS_PER_TICK {
                // Do not let a permanently busy producer starve descriptor
                // reads or the watchdog. The next zero-time poll handles more.
                self.next_rescan = self.next_rescan.min(Instant::now());
            }
        }
    }

    fn handle_command(&mut self, command: RuntimeCommand) {
        match route_command(self.suspended, &command) {
            CommandRoute::IgnoreWhileSuspended => {}
            CommandRoute::Activate => {
                let RuntimeCommand::Activate { peer } = command else {
                    unreachable!()
                };
                self.activate(peer);
            }
            CommandRoute::Release => {
                let RuntimeCommand::Release { transport_live } = command else {
                    unreachable!()
                };
                self.release(transport_live);
            }
            CommandRoute::TerminalSent => {
                let RuntimeCommand::TerminalSent { transport_live } = command else {
                    unreachable!()
                };
                self.terminal_sent(transport_live);
            }
            CommandRoute::Inject => {
                let RuntimeCommand::ReceiverEffects { effects, applied } = command else {
                    unreachable!()
                };
                if self.inject(effects)
                    && let Some(applied) = applied
                {
                    let _ = applied.send(Instant::now());
                }
            }
            CommandRoute::Suspend => self.suspend(),
            CommandRoute::Resume => self.resume(),
            CommandRoute::Reload => {
                let RuntimeCommand::Reload { config, applied } = command else {
                    unreachable!()
                };
                let _ = applied.send(self.reload(config));
            }
            CommandRoute::Stop => self.stopping = true,
        }
    }

    fn activate(&mut self, peer: String) {
        if let Some(diagnostic) = activation_capture_blocker(
            self.capture_selection_complete,
            self.capture.aggregate_state().is_empty(),
        ) {
            self.diagnostic(diagnostic);
            return;
        }
        if self.ownership.request_activation().is_err() {
            return;
        }
        self.arming_leakage_events = 0;
        self.selected_peer = Some(peer);
        self.emit_ownership();
        self.advance_ownership_boundary();
    }

    fn release(&mut self, transport_live: bool) {
        if !transport_live {
            self.force_source_release(RuntimeCloseReason::TransportLost);
            return;
        }
        match self.ownership.request_release(true) {
            OwnershipEffect::QueueTerminal => self.queue_terminal_or_fail_closed(),
            OwnershipEffect::CancelActivation => {
                self.selected_peer = None;
                self.emit(RuntimeEvent::ActivationClosed(
                    RuntimeCloseReason::LocalRelease,
                ));
            }
            _ => {}
        }
        self.emit_ownership();
        self.advance_ownership_boundary();
    }

    fn terminal_sent(&mut self, transport_live: bool) {
        if !transport_live {
            self.force_source_release(RuntimeCloseReason::TransportLost);
            return;
        }
        if self.ownership.terminal_sent().is_err() {
            return;
        }
        self.advance_ownership_boundary();
    }

    fn inject(&mut self, effects: Vec<ReceiverEffect>) -> bool {
        for effect in effects {
            if let Err(diagnostic) = apply_receiver_effect(&mut self.virtual_input, effect) {
                self.diagnostic(diagnostic);
                // A failed emit followed by a failed release can leave kernel
                // key state behind. Stop the descriptor-owning thread so Drop
                // closes the uinput pair. The systemd watchdog then restarts
                // the daemon; manual launchers remain safely unable to inject.
                self.release_receiver_state(RuntimeCloseReason::BackendFault);
                self.stopping = true;
                return false;
            }
        }
        true
    }

    fn suspend(&mut self) {
        if self.suspended {
            return;
        }
        self.force_source_release(RuntimeCloseReason::Suspend);
        self.deregister_capture_descriptors();
        let failures = self.capture.suspend();
        if !failures.is_empty() {
            self.diagnostic(RuntimeDiagnostic::UngrabRequiredDescriptorClose);
        }
        self.release_receiver_state(RuntimeCloseReason::Suspend);
        self.activation_chord.clear();
        self.escape_chord.clear();
        self.suspended = true;
        self.refresh_status();
    }

    fn resume(&mut self) {
        if !self.suspended {
            return;
        }
        if self.virtual_input.release_all().is_err() {
            self.diagnostic(RuntimeDiagnostic::InjectionFailed);
            self.stopping = true;
            return;
        }
        self.suspended = false;
        self.next_rescan = Instant::now();
        self.next_readiness_probe = Instant::now();
        self.rescan();
        self.refresh_status();
    }

    fn reload(&mut self, config: LinuxRuntimeConfig) -> Result<(), LinuxRuntimeError> {
        config.validate()?;
        if self.ownership.phase() != OwnershipPhase::Idle {
            self.force_source_release(RuntimeCloseReason::LocalRelease);
        }
        self.activation_chord = ChordTracker::new(
            ConfiguredChord::parse(&config.activation_chord, ChordPurpose::Activation)
                .expect("reloaded runtime configuration was validated"),
        );
        self.escape_chord = ChordTracker::new(
            ConfiguredChord::parse(&config.escape_chord, ChordPurpose::Escape)
                .expect("reloaded runtime configuration was validated"),
        );
        if config.experimental_touchpad != self.config.experimental_touchpad {
            self.release_receiver_state(RuntimeCloseReason::LocalRelease);
            let replacement = VirtualInput::create(config.experimental_touchpad)
                .map_err(LinuxRuntimeError::ReloadVirtualInput)?;
            self.virtual_input = replacement;
            self.ready_notified = false;
            self.next_readiness_probe = Instant::now();
            let mut status = lock_status(&self.status);
            status.ready = false;
            status.virtual_devices = VirtualDeviceReadiness::default();
        }
        self.scanner = PeriodicDeviceScanner::new(config.rescan_interval);
        self.config = config;
        self.next_rescan = Instant::now();
        if !self.suspended {
            self.rescan();
        }
        Ok(())
    }

    fn rescan(&mut self) {
        self.deregister_capture_descriptors();
        let result = match self.scanner.refresh() {
            Ok(result) => result,
            Err(_) => {
                self.diagnostic(RuntimeDiagnostic::CaptureReadFailed);
                let _ = self.register_capture_descriptors();
                return;
            }
        };
        let selection = select_configured(&self.config.capture_devices, self.scanner.known());
        let previous_selection_complete = self.capture_selection_complete;
        self.capture_selection_complete = selection.is_complete();
        {
            let mut status = lock_status(&self.status);
            status.capture_selection_complete = selection.is_complete();
            status.unmatched_selectors = selection.unmatched;
            status.ambiguous_selectors = selection.ambiguous;
        }
        if let Some(diagnostic) =
            capture_selection_diagnostic(Some(previous_selection_complete), selection.is_complete())
        {
            self.diagnostic(diagnostic);
        }

        if selection_loss_closes_activation(self.ownership.phase(), selection.is_complete()) {
            self.force_source_release(RuntimeCloseReason::DeviceRemoved);
        }

        match self.capture.reconcile(selection.capture_set()) {
            Ok(outcome) => {
                for path in &outcome.removed {
                    self.activation_chord.remove_device(path);
                    self.escape_chord.remove_device(path);
                }
                if !outcome.removed.is_empty() {
                    self.release_receiver_state(RuntimeCloseReason::DeviceRemoved);
                }
                if outcome.activation_must_close {
                    self.force_source_release(RuntimeCloseReason::DeviceRemoved);
                }
            }
            Err(_) => {
                self.diagnostic(RuntimeDiagnostic::CaptureOpenFailed);
                self.force_source_release(RuntimeCloseReason::CaptureFault);
                self.capture.suspend();
            }
        }
        if !result.failures.is_empty() && selection.unmatched > 0 {
            self.diagnostic(RuntimeDiagnostic::CaptureOpenFailed);
        }
        if self.register_capture_descriptors().is_err() {
            self.diagnostic(RuntimeDiagnostic::CaptureOpenFailed);
            self.force_source_release(RuntimeCloseReason::CaptureFault);
            self.capture.suspend();
        }
        self.refresh_status();
    }

    fn read_capture_path(&mut self, path: &Path) {
        match self.capture.read_ready(path) {
            Ok(frames) => {
                for captured in frames {
                    self.handle_captured_frame(captured);
                }
            }
            Err(CaptureReadError::DeviceRemoved { path }) => {
                self.activation_chord.remove_device(&path);
                self.escape_chord.remove_device(&path);
                self.force_source_release(RuntimeCloseReason::DeviceRemoved);
                self.release_receiver_state(RuntimeCloseReason::DeviceRemoved);
                self.next_rescan = Instant::now();
            }
            Err(CaptureReadError::Mapping { .. }) => {
                self.diagnostic(RuntimeDiagnostic::CaptureMappingLost);
                self.force_source_release(RuntimeCloseReason::CaptureFault);
                self.deregister_capture_descriptors();
                self.capture.suspend();
                self.next_rescan = Instant::now();
            }
            Err(_) => {
                self.diagnostic(RuntimeDiagnostic::CaptureReadFailed);
                self.force_source_release(RuntimeCloseReason::CaptureFault);
                self.deregister_capture_descriptors();
                self.capture.suspend();
                self.next_rescan = Instant::now();
            }
        }
        self.refresh_status();
    }

    fn handle_captured_frame(&mut self, captured: CapturedDeviceFrame) {
        if self.ownership.phase() == OwnershipPhase::Arming {
            self.arming_leakage_events = self
                .arming_leakage_events
                .saturating_add(captured.frame.event_count);
        }
        let activation = self
            .activation_chord
            .observe(&captured.device_path, &captured.frame);
        let escape = self
            .escape_chord
            .observe(&captured.device_path, &captured.frame);

        match self.ownership.phase() {
            OwnershipPhase::Idle if activation => {
                self.emit(RuntimeEvent::ActivationChord);
            }
            OwnershipPhase::Remote if escape => {
                // The completing frame is consumed locally. Any chord prefix
                // already forwarded is reconciled by the terminal receiver
                // close, so the escape key itself never reaches the peer.
                match self.ownership.request_release(true) {
                    OwnershipEffect::QueueTerminal => self.queue_terminal_or_fail_closed(),
                    _ => unreachable!("Remote release always queues a terminal"),
                }
                self.emit_ownership();
            }
            OwnershipPhase::Remote => match self.capture_tx.try_send(captured) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    {
                        let mut status = lock_status(&self.status);
                        status.dropped_capture_frames =
                            status.dropped_capture_frames.saturating_add(1);
                    }
                    self.diagnostic(RuntimeDiagnostic::CapturedFrameQueueFull);
                    self.force_source_release(RuntimeCloseReason::Backpressure);
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.force_source_release(RuntimeCloseReason::Backpressure);
                }
            },
            _ => {}
        }
        self.advance_ownership_boundary();
    }

    fn advance_ownership_boundary(&mut self) {
        let kernel_neutral = if self.ownership.phase() == OwnershipPhase::Arming {
            match self.capture.kernel_is_neutral() {
                Ok(neutral) => neutral,
                Err(_) => {
                    let _ = self.ownership.request_release(false);
                    self.selected_peer = None;
                    self.diagnostic(RuntimeDiagnostic::GrabFailed);
                    self.emit_ownership();
                    return;
                }
            }
        } else {
            false
        };
        let effect = self
            .ownership
            .at_complete_boundary(self.capture.aggregate_state(), kernel_neutral);
        match effect {
            OwnershipEffect::AcquireGrabs => match self.capture.grab_all() {
                Ok(()) => {
                    let _ = self.ownership.grab_succeeded();
                    self.emit_ownership();
                }
                Err(_) => {
                    // An EVIOCGRAB transaction can fail to roll back an
                    // earlier node, and post-grab neutrality rollback can fail
                    // too. Descriptor close is the only reliable recovery.
                    self.deregister_capture_descriptors();
                    self.capture.suspend();
                    self.next_rescan = Instant::now();
                    let _ = self.ownership.grab_failed();
                    self.selected_peer = None;
                    self.diagnostic(RuntimeDiagnostic::GrabFailed);
                    self.diagnostic(RuntimeDiagnostic::UngrabRequiredDescriptorClose);
                    self.emit_ownership();
                }
            },
            OwnershipEffect::ReleaseGrabs => {
                let failures = self.capture.ungrab_all();
                if !failures.is_empty() || self.capture.is_grabbed() {
                    self.capture.suspend();
                    self.next_rescan = Instant::now();
                    self.diagnostic(RuntimeDiagnostic::UngrabRequiredDescriptorClose);
                }
                let _ = self.ownership.release_completed();
                self.selected_peer = None;
                self.emit(RuntimeEvent::ActivationClosed(
                    RuntimeCloseReason::LocalRelease,
                ));
                self.emit_ownership();
            }
            _ => {}
        }
        self.refresh_status();
    }

    fn force_source_release(&mut self, reason: RuntimeCloseReason) {
        let effect = self.ownership.device_removed();
        match effect {
            OwnershipEffect::CloseActivationAndReleaseGrabs => {
                let failures = self.capture.ungrab_all();
                if !failures.is_empty() || self.capture.is_grabbed() {
                    self.capture.suspend();
                    self.next_rescan = Instant::now();
                    self.diagnostic(RuntimeDiagnostic::UngrabRequiredDescriptorClose);
                }
                let _ = self.ownership.release_completed();
                self.selected_peer = None;
                self.emit(RuntimeEvent::ActivationClosed(reason));
                self.emit_ownership();
            }
            OwnershipEffect::CancelActivation => {
                self.selected_peer = None;
                self.emit(RuntimeEvent::ActivationClosed(reason));
                self.emit_ownership();
            }
            OwnershipEffect::None => {}
            _ => unreachable!("forced ownership release has a closed effect set"),
        }
        self.refresh_status();
    }

    fn stop_all(&mut self) {
        self.force_source_release(RuntimeCloseReason::Stop);
        self.deregister_capture_descriptors();
        let failures = self.capture.suspend();
        if !failures.is_empty() {
            self.diagnostic(RuntimeDiagnostic::UngrabRequiredDescriptorClose);
        }
        if self.virtual_input.release_all().is_err() {
            self.diagnostic(RuntimeDiagnostic::InjectionFailed);
        }
        self.emit(RuntimeEvent::ReceiverStateReleased(
            RuntimeCloseReason::Stop,
        ));
        let _ = sd_notify::notify(&[NotifyState::Stopping]);
        self.emit(RuntimeEvent::Stopped);
    }

    fn release_receiver_state(&mut self, reason: RuntimeCloseReason) {
        if self.virtual_input.release_all().is_err() {
            self.diagnostic(RuntimeDiagnostic::InjectionFailed);
            // Do not accept another injection command on a backend that may
            // still hold kernel state. RuntimeLoop drop closes both devices.
            self.stopping = true;
        }
        self.emit(RuntimeEvent::ReceiverStateReleased(reason));
    }

    fn register_capture_descriptors(&mut self) -> Result<(), RuntimeStartupError> {
        for path in self.capture.paths().map(Path::to_owned).collect::<Vec<_>>() {
            let Some(fd) = self.capture.raw_fd(&path) else {
                continue;
            };
            ensure_close_on_exec(fd).map_err(RuntimeStartupError::Register)?;
            let token = Token(self.next_device_token);
            self.next_device_token = self.next_device_token.saturating_add(1);
            let mut source = SourceFd(&fd);
            self.poll
                .registry()
                .register(&mut source, token, Interest::READABLE)
                .map_err(RuntimeStartupError::Register)?;
            self.registrations
                .insert(token, RegisteredDevice { path, fd });
        }
        Ok(())
    }

    fn deregister_capture_descriptors(&mut self) {
        for registration in std::mem::take(&mut self.registrations).into_values() {
            let mut source = SourceFd(&registration.fd);
            let _ = self.poll.registry().deregister(&mut source);
        }
    }

    fn emit_ownership(&mut self) {
        self.emit(RuntimeEvent::OwnershipChanged {
            phase: self.ownership.phase(),
            selected_peer: self.selected_peer.clone(),
            changed_at: Instant::now(),
            arming_leakage_events: self.arming_leakage_events,
        });
        self.refresh_status();
    }

    fn queue_terminal_or_fail_closed(&mut self) {
        if !self.emit(RuntimeEvent::TerminalRequested) {
            // The daemon cannot acknowledge an event it never received. Drop
            // grabs now instead of leaving ownership stuck in Releasing.
            self.force_source_release(RuntimeCloseReason::Backpressure);
        }
    }

    fn diagnostic(&mut self, diagnostic: RuntimeDiagnostic) {
        lock_status(&self.status).last_diagnostic = Some(diagnostic);
        self.emit(RuntimeEvent::Diagnostic(diagnostic));
    }

    fn emit(&mut self, event: RuntimeEvent) -> bool {
        match self.event_tx.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                let mut status = lock_status(&self.status);
                status.dropped_events = status.dropped_events.saturating_add(1);
                status.last_diagnostic = Some(RuntimeDiagnostic::EventQueueFull);
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                self.stopping = true;
                false
            }
        }
    }

    fn refresh_status(&self) {
        let grabbed = self.capture.is_grabbed();
        let mut status = lock_status(&self.status);
        status.running = true;
        status.suspended = self.suspended;
        status.ownership = self.ownership.phase();
        status.selected_peer = self.selected_peer.clone();
        status.capture_selection_complete = self.capture_selection_complete;
        status.capture_devices = self
            .capture
            .paths()
            .map(|path| RuntimeDeviceStatus {
                path: path.to_owned(),
                grabbed,
            })
            .collect();
    }
}

#[derive(Debug, Default)]
struct CaptureSelection {
    selected: Vec<DeviceInfo>,
    configured: usize,
    unmatched: usize,
    ambiguous: usize,
}

impl CaptureSelection {
    fn is_complete(&self) -> bool {
        self.configured > 0 && self.unmatched == 0 && self.ambiguous == 0
    }

    fn capture_set(&self) -> &[DeviceInfo] {
        if self.is_complete() {
            &self.selected
        } else {
            &[]
        }
    }
}

fn select_configured<'a>(
    selectors: &[ConfigDeviceSelector],
    devices: impl Iterator<Item = &'a DeviceInfo>,
) -> CaptureSelection {
    let devices = devices.collect::<Vec<_>>();
    let mut resolved = Vec::with_capacity(selectors.len());
    let mut unmatched = 0;
    let mut ambiguous = 0;
    for selector in selectors {
        let matches = devices
            .iter()
            .copied()
            .filter(|device| capture_selector_matches(selector, device))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => {
                unmatched += 1;
                resolved.push(None);
            }
            [device] => resolved.push(Some((*device).clone())),
            _ => {
                ambiguous += 1;
                resolved.push(None);
            }
        }
    }

    // Two logical selectors resolving to one event node is also ambiguous.
    // Silently deduplicating would make the configured all-or-none contract
    // claim success for a set different from the one the operator named.
    let mut path_counts = BTreeMap::<PathBuf, usize>::new();
    for device in resolved.iter().flatten() {
        *path_counts.entry(device.path.clone()).or_default() += 1;
    }
    let selected = resolved
        .into_iter()
        .filter_map(|device| {
            let device = device?;
            if path_counts.get(&device.path) == Some(&1) {
                Some(device)
            } else {
                ambiguous += 1;
                None
            }
        })
        .collect();

    CaptureSelection {
        selected,
        configured: selectors.len(),
        unmatched,
        ambiguous,
    }
}

/// Returns whether one physical device has the stable identity named by a
/// configured capture selector.
///
/// Device listings use this same predicate so an event-node renumber cannot
/// make their capture-set membership disagree with the runtime.
pub fn capture_selector_matches(selector: &ConfigDeviceSelector, device: &DeviceInfo) -> bool {
    if device.is_zflow_virtual() {
        return false;
    }
    let stable_identity_available = selector
        .phys
        .as_deref()
        .is_some_and(|physical| !physical.is_empty())
        && selector.vendor.is_some()
        && selector.product.is_some();
    let metadata_matches = selector
        .name
        .as_deref()
        .is_none_or(|name| device.name.as_deref() == Some(name))
        && selector
            .phys
            .as_deref()
            .is_none_or(|physical| device.physical_path.as_deref() == Some(physical))
        && selector.vendor.is_none_or(|vendor| device.vendor == vendor)
        && selector
            .product
            .is_none_or(|product| device.product == product);

    metadata_matches && (stable_identity_available || selector.path == device.path)
}

fn activation_capture_blocker(
    selection_complete: bool,
    capture_empty: bool,
) -> Option<RuntimeDiagnostic> {
    if !selection_complete {
        Some(RuntimeDiagnostic::CaptureSelectionIncomplete)
    } else if capture_empty {
        Some(RuntimeDiagnostic::CaptureOpenFailed)
    } else {
        None
    }
}

fn capture_selection_diagnostic(
    previous_complete: Option<bool>,
    current_complete: bool,
) -> Option<RuntimeDiagnostic> {
    (!current_complete && previous_complete != Some(false))
        .then_some(RuntimeDiagnostic::CaptureSelectionIncomplete)
}

fn selection_loss_closes_activation(phase: OwnershipPhase, selection_complete: bool) -> bool {
    !selection_complete && phase != OwnershipPhase::Idle
}

fn ensure_close_on_exec(fd: RawFd) -> io::Result<()> {
    // SAFETY: F_GETFD and F_SETFD do not dereference pointers. `fd` is owned
    // by the live CaptureSet for the duration of both calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::FD_CLOEXEC == 0 {
        let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn apply_receiver_effect(
    virtual_input: &mut VirtualInput,
    effect: ReceiverEffect,
) -> Result<(), RuntimeDiagnostic> {
    let result: Result<(), InjectionError> = match effect {
        ReceiverEffect::Motion { delta, .. } => virtual_input.pointer.motion(delta),
        ReceiverEffect::Key { key, pressed, .. } => virtual_input.keyboard.set_key(key, pressed),
        ReceiverEffect::Button {
            button, pressed, ..
        } => virtual_input.pointer.set_button(button, pressed),
        ReceiverEffect::Modifier {
            modifier, pressed, ..
        } => virtual_input
            .keyboard
            .set_key(modifier_usage(modifier), pressed),
        ReceiverEffect::TouchReplaced { state, .. } => virtual_input.replace_touch(&state),
        ReceiverEffect::ActivationClosed { .. } => {
            return virtual_input
                .release_all()
                .map_err(|_| RuntimeDiagnostic::InjectionFailed);
        }
        ReceiverEffect::ActivationOpened(_)
        | ReceiverEffect::ScrollBegan(_)
        | ReceiverEffect::ScrollEnded { .. }
        | ReceiverEffect::SnapshotAck { .. }
        | ReceiverEffect::TakeoverAccepted { .. }
        | ReceiverEffect::Rejected { .. } => Ok(()),
    };
    result.map_err(|_| RuntimeDiagnostic::InjectionFailed)
}

fn modifier_usage(modifier: Modifier) -> HidUsage {
    let usage = match modifier {
        Modifier::LeftControl => 0xe0,
        Modifier::LeftShift => 0xe1,
        Modifier::LeftAlt => 0xe2,
        Modifier::LeftMeta => 0xe3,
        Modifier::RightControl => 0xe4,
        Modifier::RightShift => 0xe5,
        Modifier::RightAlt => 0xe6,
        Modifier::RightMeta => 0xe7,
    };
    HidUsage::keyboard(usage)
}

pub fn watchdog_tick_interval(watchdog_timeout: Option<Duration>) -> Option<Duration> {
    watchdog_timeout.map(|timeout| {
        let half = timeout / 2;
        if half.is_zero() {
            Duration::from_micros(1)
        } else {
            half
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandRoute {
    Activate,
    Release,
    TerminalSent,
    Inject,
    Suspend,
    Resume,
    Reload,
    Stop,
    IgnoreWhileSuspended,
}

fn route_command(suspended: bool, command: &RuntimeCommand) -> CommandRoute {
    match command {
        RuntimeCommand::Stop => CommandRoute::Stop,
        RuntimeCommand::Resume => CommandRoute::Resume,
        RuntimeCommand::Reload { .. } => CommandRoute::Reload,
        RuntimeCommand::Suspend => CommandRoute::Suspend,
        RuntimeCommand::Activate { .. } if suspended => CommandRoute::IgnoreWhileSuspended,
        RuntimeCommand::ReceiverEffects { .. } if suspended => CommandRoute::IgnoreWhileSuspended,
        RuntimeCommand::Release { .. } if suspended => CommandRoute::IgnoreWhileSuspended,
        RuntimeCommand::TerminalSent { .. } if suspended => CommandRoute::IgnoreWhileSuspended,
        RuntimeCommand::Activate { .. } => CommandRoute::Activate,
        RuntimeCommand::Release { .. } => CommandRoute::Release,
        RuntimeCommand::TerminalSent { .. } => CommandRoute::TerminalSent,
        RuntimeCommand::ReceiverEffects { .. } => CommandRoute::Inject,
    }
}

fn lock_status(
    status: &Mutex<LinuxRuntimeStatus>,
) -> std::sync::MutexGuard<'_, LinuxRuntimeStatus> {
    status
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use evdev::{BusType, EventType, InputEvent, SynchronizationCode};

    use super::*;
    use crate::linux::{
        FrameAccumulator, ZFLOW_DEVICE_VERSION, ZFLOW_POINTER_NAME, ZFLOW_POINTER_PHYS,
        ZFLOW_POINTER_PRODUCT_ID, ZFLOW_VENDOR_ID,
    };

    fn frame(events: &[(KeyCode, i32)]) -> CaptureFrame {
        let mut accumulator = FrameAccumulator::default();
        for (key, value) in events {
            accumulator
                .push(InputEvent::new(EventType::KEY.0, key.code(), *value))
                .unwrap();
        }
        accumulator
            .push(InputEvent::new(
                EventType::SYNCHRONIZATION.0,
                SynchronizationCode::SYN_REPORT.0,
                0,
            ))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn critical_command_reports_a_full_queue_at_its_bound() {
        let poll = Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), WAKE_TOKEN).unwrap());
        let (commands, _receiver) = mpsc::sync_channel(1);
        commands.try_send(RuntimeCommand::Stop).unwrap();
        let control = LinuxRuntimeControl {
            commands,
            waker,
            status: Arc::new(Mutex::new(LinuxRuntimeStatus::default())),
            alive: Arc::new(AtomicBool::new(true)),
        };

        assert_eq!(
            control.send_critical(RuntimeCommand::Stop, Duration::ZERO),
            Err(RuntimeCommandError::Full)
        );
    }

    fn device(
        path: &str,
        name: &str,
        physical_path: &str,
        vendor: u16,
        product: u16,
    ) -> DeviceInfo {
        DeviceInfo {
            path: path.into(),
            name: Some(name.into()),
            physical_path: Some(physical_path.into()),
            unique_name: None,
            bus: BusType::BUS_USB.0,
            vendor,
            product,
            version: 1,
            has_keyboard_keys: true,
            has_pointer_buttons: false,
            has_relative_pointer: false,
            has_high_resolution_wheel: false,
        }
    }

    fn selector(path: &str, name: &str, physical_path: &str) -> ConfigDeviceSelector {
        ConfigDeviceSelector {
            path: path.into(),
            name: Some(name.into()),
            phys: Some(physical_path.into()),
            vendor: Some(0x1234),
            product: Some(0x5678),
        }
    }

    #[test]
    fn chord_is_aggregate_edge_triggered_across_devices() {
        let chord = ConfiguredChord::parse(
            &["KEY_LEFTCTRL".into(), "KEY_F12".into()],
            ChordPurpose::Activation,
        )
        .unwrap();
        let mut tracker = ChordTracker::new(chord);
        assert!(!tracker.observe(Path::new("one"), &frame(&[(KeyCode::KEY_LEFTCTRL, 1)])));
        assert!(tracker.observe(Path::new("two"), &frame(&[(KeyCode::KEY_F12, 1)])));
        assert!(!tracker.observe(Path::new("two"), &frame(&[(KeyCode::KEY_F12, 2)])));
        assert!(!tracker.observe(Path::new("one"), &frame(&[(KeyCode::KEY_LEFTCTRL, 0)])));
        assert!(tracker.observe(Path::new("one"), &frame(&[(KeyCode::KEY_LEFTCTRL, 1)])));
    }

    #[test]
    fn removal_clears_composite_chord_state() {
        let chord = ConfiguredChord::parse(
            &["KEY_LEFTCTRL".into(), "KEY_F12".into()],
            ChordPurpose::Activation,
        )
        .unwrap();
        let mut tracker = ChordTracker::new(chord);
        tracker.observe(Path::new("one"), &frame(&[(KeyCode::KEY_LEFTCTRL, 1)]));
        tracker.remove_device(Path::new("one"));
        assert!(!tracker.observe(Path::new("two"), &frame(&[(KeyCode::KEY_F12, 1)])));
    }

    #[test]
    fn stable_selector_survives_event_node_renumbering() {
        let configured = selector(
            "/dev/input/event3",
            "Actually Good Keyboard",
            "usb-0000:01/input0",
        );
        let renumbered = device(
            "/dev/input/event19",
            "Actually Good Keyboard",
            "usb-0000:01/input0",
            0x1234,
            0x5678,
        );

        assert!(capture_selector_matches(&configured, &renumbered));
        let selection = select_configured(&[configured], [&renumbered].into_iter());

        assert!(selection.is_complete());
        assert_eq!(selection.capture_set()[0].path, renumbered.path);
    }

    #[test]
    fn selector_without_stable_tuple_uses_exact_path_only() {
        let configured = ConfigDeviceSelector {
            path: "/dev/input/event3".into(),
            name: Some("Legacy Keyboard".into()),
            phys: None,
            vendor: Some(0x1234),
            product: Some(0x5678),
        };
        let original_path = device(
            "/dev/input/event3",
            "Legacy Keyboard",
            "usb-legacy/input0",
            0x1234,
            0x5678,
        );
        let renumbered = DeviceInfo {
            path: "/dev/input/event19".into(),
            ..original_path.clone()
        };

        assert!(capture_selector_matches(&configured, &original_path));
        assert!(!capture_selector_matches(&configured, &renumbered));
        let exact = select_configured(
            std::slice::from_ref(&configured),
            [&original_path].into_iter(),
        );
        let moved = select_configured(&[configured], [&renumbered].into_iter());

        assert!(exact.is_complete());
        assert!(!moved.is_complete());
        assert!(moved.capture_set().is_empty());
    }

    #[test]
    fn partial_capture_set_cannot_activate_or_open_a_subset() {
        let keyboard = selector(
            "/dev/input/event3",
            "Actually Good Keyboard",
            "usb-0000:01/input0",
        );
        let mouse = selector(
            "/dev/input/event4",
            "Actually Good Mouse",
            "usb-0000:02/input0",
        );
        let present = device(
            "/dev/input/event19",
            "Actually Good Keyboard",
            "usb-0000:01/input0",
            0x1234,
            0x5678,
        );

        let selection = select_configured(&[keyboard, mouse], [&present].into_iter());

        assert!(!selection.is_complete());
        assert_eq!(selection.unmatched, 1);
        assert!(selection.capture_set().is_empty());
        assert_eq!(
            activation_capture_blocker(selection.is_complete(), true),
            Some(RuntimeDiagnostic::CaptureSelectionIncomplete)
        );
    }

    #[test]
    fn incomplete_capture_selection_diagnostic_is_edge_triggered() {
        assert_eq!(
            capture_selection_diagnostic(None, false),
            Some(RuntimeDiagnostic::CaptureSelectionIncomplete)
        );
        assert_eq!(capture_selection_diagnostic(Some(false), false), None);
        assert_eq!(capture_selection_diagnostic(Some(false), true), None);
        assert_eq!(
            capture_selection_diagnostic(Some(true), false),
            Some(RuntimeDiagnostic::CaptureSelectionIncomplete)
        );
    }

    #[test]
    fn selector_removal_closes_remote_and_empties_the_capture_plan() {
        let configured = selector(
            "/dev/input/event3",
            "Actually Good Keyboard",
            "usb-0000:01/input0",
        );
        let present = device(
            "/dev/input/event3",
            "Actually Good Keyboard",
            "usb-0000:01/input0",
            0x1234,
            0x5678,
        );
        let complete = select_configured(std::slice::from_ref(&configured), [&present].into_iter());
        let removed = select_configured(std::slice::from_ref(&configured), [].iter());

        assert!(complete.is_complete());
        assert!(!selection_loss_closes_activation(
            OwnershipPhase::Remote,
            complete.is_complete()
        ));
        assert!(removed.capture_set().is_empty());
        assert!(selection_loss_closes_activation(
            OwnershipPhase::Remote,
            removed.is_complete()
        ));
        assert!(!selection_loss_closes_activation(
            OwnershipPhase::Idle,
            removed.is_complete()
        ));
    }

    #[test]
    fn stable_selector_rejects_ambiguous_devices() {
        let configured = selector("/dev/input/event3", "Clone Keyboard", "usb-clone/input0");
        let first = device(
            "/dev/input/event8",
            "Clone Keyboard",
            "usb-clone/input0",
            0x1234,
            0x5678,
        );
        let second = device(
            "/dev/input/event9",
            "Clone Keyboard",
            "usb-clone/input0",
            0x1234,
            0x5678,
        );

        let selection = select_configured(&[configured], [&first, &second].into_iter());

        assert!(!selection.is_complete());
        assert_eq!(selection.ambiguous, 1);
        assert!(selection.capture_set().is_empty());
    }

    #[test]
    fn exact_path_fallback_does_not_capture_zflow_virtual_devices() {
        let configured = ConfigDeviceSelector {
            path: "/dev/input/event8".into(),
            name: None,
            phys: None,
            vendor: None,
            product: None,
        };
        let mut virtual_pointer = device(
            "/dev/input/event8",
            ZFLOW_POINTER_NAME,
            ZFLOW_POINTER_PHYS,
            ZFLOW_VENDOR_ID,
            ZFLOW_POINTER_PRODUCT_ID,
        );
        virtual_pointer.bus = BusType::BUS_VIRTUAL.0;
        virtual_pointer.version = ZFLOW_DEVICE_VERSION;

        let selection = select_configured(&[configured], [&virtual_pointer].into_iter());

        assert!(!selection.is_complete());
        assert_eq!(selection.unmatched, 1);
        assert!(selection.capture_set().is_empty());
    }

    #[test]
    fn lifecycle_commands_route_safely_while_suspended() {
        assert_eq!(
            route_command(
                true,
                &RuntimeCommand::Activate {
                    peer: "desk".into()
                }
            ),
            CommandRoute::IgnoreWhileSuspended
        );
        assert_eq!(
            route_command(
                true,
                &RuntimeCommand::ReceiverEffects {
                    effects: Vec::new(),
                    applied: None,
                }
            ),
            CommandRoute::IgnoreWhileSuspended
        );
        assert_eq!(
            route_command(true, &RuntimeCommand::Resume),
            CommandRoute::Resume
        );
        assert_eq!(
            route_command(true, &RuntimeCommand::Stop),
            CommandRoute::Stop
        );
    }

    #[test]
    fn runtime_defaults_do_not_claim_the_linux_vt_switch_chord() {
        let config = LinuxRuntimeConfig::default();
        assert_eq!(
            config.activation_chord,
            ["KEY_LEFTCTRL", "KEY_LEFTMETA", "KEY_F12"]
        );
        assert_eq!(
            config.escape_chord,
            ["KEY_LEFTCTRL", "KEY_LEFTMETA", "KEY_BACKSPACE"]
        );
    }

    #[test]
    fn watchdog_ticks_at_half_the_manager_deadline() {
        assert_eq!(
            watchdog_tick_interval(Some(Duration::from_secs(2))),
            Some(Duration::from_secs(1))
        );
        assert_eq!(watchdog_tick_interval(None), None);
    }

    #[test]
    fn modifier_mapping_matches_usb_hid_modifier_block() {
        assert_eq!(
            modifier_usage(Modifier::LeftControl),
            HidUsage::keyboard(0xe0)
        );
        assert_eq!(
            modifier_usage(Modifier::RightMeta),
            HidUsage::keyboard(0xe7)
        );
    }

    #[test]
    #[ignore = "requires root-equivalent access to /dev/uinput and selected evdev nodes"]
    fn privileged_runtime_smoke() {
        let runtime = LinuxRuntime::spawn(LinuxRuntimeConfig::default()).unwrap();
        runtime.shutdown().unwrap();
    }
}
