use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    os::fd::RawFd,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use evdev::KeyCode;
use mio::{Events, Interest, Poll, Token, Waker, unix::SourceFd};
use sd_notify::NotifyState;
use thiserror::Error;
use tokio::sync::mpsc::{
    self,
    error::{TryRecvError, TrySendError},
};

use crate::{
    config::{Config, DeviceSelector as ConfigDeviceSelector},
    core::{HidUsage, KeyboardMode, PointerButton, ReceiverEffect},
    linux::{
        CaptureFrame, CaptureReadError, CaptureSet, CaptureSetError, CaptureTransition,
        CapturedDeviceFrame, DeviceInfo, InjectionError, KeyState, OwnershipEffect, OwnershipPhase,
        SourceOwnership, VirtualInput, enumerate_devices, evdev_button_to_pointer,
        evdev_key_to_hid,
    },
};

use super::probe_virtual_device_readiness;

const WAKE_TOKEN: Token = Token(0);
const FIRST_DEVICE_TOKEN: usize = 1;
const MAX_COMMANDS_PER_TICK: usize = 256;
const MAX_IDLE_POLL: Duration = Duration::from_secs(2);
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);
const READINESS_PROBE_INTERVAL: Duration = Duration::from_millis(25);
const COMMAND_CAPACITY: usize = 256;
const EVENT_CAPACITY: usize = 256;
const CAPTURE_CAPACITY: usize = 1_024;

#[derive(Debug, Clone)]
pub struct LinuxRuntimeConfig {
    pub capture_devices: Vec<ConfigDeviceSelector>,
    pub activation_chord: Vec<String>,
    pub escape_chord: Vec<String>,
    pub experimental_touchpad: bool,
}

impl LinuxRuntimeConfig {
    pub fn from_config(config: &Config) -> Self {
        Self {
            capture_devices: config.input.capture_devices.clone(),
            activation_chord: config.input.activation_chord.clone(),
            escape_chord: config.input.escape_chord.clone(),
            experimental_touchpad: config.input.experimental_touchpad,
        }
    }

    pub fn validate(&self) -> Result<(), LinuxRuntimeError> {
        ConfiguredChord::parse(&self.activation_chord)?;
        ConfiguredChord::parse(&self.escape_chord)?;
        Ok(())
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
        /// The sending peer's keyboard mode, only in the batch that opens an
        /// activation.
        keyboard: Option<KeyboardMode>,
        touch_captured_at: Option<Instant>,
        /// Sent only after every effect reaches the uinput backend.
        applied: Option<tokio::sync::oneshot::Sender<Instant>>,
    },
    Reload {
        config: LinuxRuntimeConfig,
        applied: tokio::sync::oneshot::Sender<Result<(), LinuxRuntimeError>>,
    },
    /// Whether the focused desktop app is a terminal.
    DesktopFocus {
        terminal: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeCloseReason {
    LocalRelease,
    TransportLost,
    DeviceRemoved,
    CaptureFault,
    BackendFault,
    Backpressure,
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
    InjectionRejected,
    ReadinessProbeFailed,
    ServiceNotificationFailed,
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

/// What the daemon reads back from the input thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxRuntimeStatus {
    pub ownership: OwnershipPhase,
    pub selected_peer: Option<String>,
}

/// Read-only capture resolution used by setup diagnostics.
///
/// This deliberately shares the runtime's exact all-or-none selector rules,
/// so `zflow doctor` cannot claim a configuration is usable when the daemon
/// would refuse to grab it. With no selectors it lists every keyboard and
/// pointer the scan could open.
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
        !self.selected_paths.is_empty() && self.unmatched == 0 && self.ambiguous == 0
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
            ownership: OwnershipPhase::Idle,
            selected_peer: None,
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
    commands: mpsc::Sender<RuntimeCommand>,
    waker: Arc<Waker>,
    status: Arc<Mutex<LinuxRuntimeStatus>>,
    stop: Arc<AtomicBool>,
}

impl LinuxRuntimeControl {
    pub fn send(&self, command: RuntimeCommand) -> Result<(), RuntimeCommandError> {
        match self.commands.try_send(command) {
            Ok(()) => self.wake(),
            Err(TrySendError::Full(_)) => Err(RuntimeCommandError::Full),
            Err(TrySendError::Closed(_)) => Err(RuntimeCommandError::Stopped),
        }
    }

    /// Delivers a fail-safe lifecycle command through temporary queue
    /// backpressure. Callers must tear down the daemon if this bounded wait
    /// still fails, because the descriptor-owning thread cannot otherwise know
    /// that a terminal write completed.
    pub async fn send_critical(
        &self,
        command: RuntimeCommand,
        timeout: Duration,
    ) -> Result<(), RuntimeCommandError> {
        // A full queue drains only while the input thread runs.
        self.wake()?;
        match tokio::time::timeout(timeout, self.commands.send(command)).await {
            Ok(Ok(())) => self.wake(),
            Ok(Err(_)) => Err(RuntimeCommandError::Stopped),
            Err(_) => Err(RuntimeCommandError::Full),
        }
    }

    pub fn status(&self) -> LinuxRuntimeStatus {
        lock_status(&self.status).clone()
    }

    fn wake(&self) -> Result<(), RuntimeCommandError> {
        self.waker.wake().map_err(|_| RuntimeCommandError::Wake)
    }
}

#[cfg(test)]
impl LinuxRuntimeControl {
    /// A control whose commands wait in the returned queue, with the poll its
    /// sends wake in place of the input thread.
    pub(crate) fn queue(capacity: usize) -> (Self, mpsc::Receiver<RuntimeCommand>, Poll) {
        let poll = Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), WAKE_TOKEN).unwrap());
        let (commands, receiver) = mpsc::channel(capacity);
        let control = Self {
            commands,
            waker,
            status: Arc::new(Mutex::new(LinuxRuntimeStatus::default())),
            stop: Arc::new(AtomicBool::new(false)),
        };
        (control, receiver, poll)
    }
}

/// The input thread plus the queues it fills. The daemon awaits both
/// receivers; the thread never blocks on them.
pub struct LinuxRuntime {
    control: LinuxRuntimeControl,
    pub events: mpsc::Receiver<RuntimeEvent>,
    pub captured: mpsc::Receiver<CapturedDeviceFrame>,
    thread: Option<JoinHandle<()>>,
}

impl LinuxRuntime {
    pub fn spawn(config: LinuxRuntimeConfig) -> Result<Self, LinuxRuntimeError> {
        config.validate()?;
        let poll = Poll::new().map_err(LinuxRuntimeError::Poll)?;
        let waker =
            Arc::new(Waker::new(poll.registry(), WAKE_TOKEN).map_err(LinuxRuntimeError::Poll)?);
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
        let (capture_tx, capture_rx) = mpsc::channel(CAPTURE_CAPACITY);
        let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel(1);
        let status = Arc::new(Mutex::new(LinuxRuntimeStatus::default()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_status = status.clone();
        let thread_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("zflow-linux-input".into())
            .spawn(move || {
                match RuntimeLoop::new(
                    poll,
                    config,
                    command_rx,
                    thread_stop,
                    event_tx,
                    capture_tx,
                    thread_status,
                ) {
                    Ok(mut runtime) => {
                        let _ = startup_tx.send(Ok(()));
                        runtime.run();
                    }
                    Err(error) => {
                        let _ = startup_tx.send(Err(error));
                    }
                }
            })
            .map_err(LinuxRuntimeError::Spawn)?;

        match startup_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                control: LinuxRuntimeControl {
                    commands: command_tx,
                    waker,
                    status,
                    stop,
                },
                events: event_rx,
                captured: capture_rx,
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

    pub fn shutdown(mut self) -> Result<(), LinuxRuntimeError> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<(), LinuxRuntimeError> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        // A flag rather than a queued command, so a full queue cannot delay
        // teardown or leave a detached descriptor-owning thread.
        self.control.stop.store(true, Ordering::Release);
        let _ = self.control.waker.wake();
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

#[derive(Debug, Clone)]
struct ConfiguredChord(BTreeSet<ChordMember>);

impl ConfiguredChord {
    fn parse(names: &[String]) -> Result<Self, LinuxRuntimeError> {
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

    /// Closed descriptors report no more releases, so forget what they held.
    fn clear(&mut self) {
        self.held_by_device.clear();
        self.latched = false;
    }

    fn remove_device(&mut self, path: &Path) {
        self.held_by_device.remove(path);
        self.latched = self.chord.0.iter().all(|member| {
            self.held_by_device
                .values()
                .any(|held| held.contains(member))
        });
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
    commands: mpsc::Receiver<RuntimeCommand>,
    stop: Arc<AtomicBool>,
    event_tx: mpsc::Sender<RuntimeEvent>,
    capture_tx: mpsc::Sender<CapturedDeviceFrame>,
    status: Arc<Mutex<LinuxRuntimeStatus>>,
    config: LinuxRuntimeConfig,
    capture: CaptureSet,
    ownership: SourceOwnership,
    virtual_input: VirtualInput,
    /// Mode for the next activation that opens.
    next_keyboard: KeyboardMode,
    /// Kept here too so a replaced virtual keyboard starts with it.
    terminal: bool,
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
    commands_pending: bool,
    stopping: bool,
}

impl RuntimeLoop {
    fn new(
        poll: Poll,
        config: LinuxRuntimeConfig,
        commands: mpsc::Receiver<RuntimeCommand>,
        stop: Arc<AtomicBool>,
        event_tx: mpsc::Sender<RuntimeEvent>,
        capture_tx: mpsc::Sender<CapturedDeviceFrame>,
        status: Arc<Mutex<LinuxRuntimeStatus>>,
    ) -> Result<Self, RuntimeStartupError> {
        let virtual_input = VirtualInput::create(config.experimental_touchpad)?;
        let activation_chord = ChordTracker::new(
            ConfiguredChord::parse(&config.activation_chord)
                .expect("runtime configuration was validated before thread startup"),
        );
        let escape_chord = ChordTracker::new(
            ConfiguredChord::parse(&config.escape_chord)
                .expect("runtime configuration was validated before thread startup"),
        );
        let scan = enumerate_devices().map_err(RuntimeStartupError::DeviceScan)?;
        let selection = select_configured(&config.capture_devices, scan.physical_devices());
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
            stop,
            event_tx,
            capture_tx,
            status,
            config,
            capture,
            ownership: SourceOwnership::default(),
            virtual_input,
            next_keyboard: KeyboardMode::Standard,
            terminal: false,
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
            commands_pending: false,
            stopping: false,
        };
        runtime.register_capture_descriptors()?;
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
        if now >= self.next_rescan {
            self.rescan();
            self.next_rescan = now + RESCAN_INTERVAL;
        }
        if !self.ready_notified && now >= self.next_readiness_probe {
            self.probe_readiness();
            self.next_readiness_probe = now + READINESS_PROBE_INTERVAL;
        }
    }

    fn poll_timeout(&self, now: Instant) -> Duration {
        if self.commands_pending {
            return Duration::ZERO;
        }
        let mut next = (now + MAX_IDLE_POLL).min(self.next_rescan);
        if !self.ready_notified {
            next = next.min(self.next_readiness_probe);
        }
        if let Some(watchdog) = self.next_watchdog {
            next = next.min(watchdog);
        }
        next.saturating_duration_since(now)
    }

    fn probe_readiness(&mut self) {
        match probe_virtual_device_readiness() {
            Ok(readiness) => {
                if readiness.is_ready_for(self.config.experimental_touchpad) {
                    if sd_notify::notify(&[NotifyState::Ready]).is_ok() {
                        self.ready_notified = true;
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
        self.commands_pending = false;
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
                self.commands_pending = true;
            }
        }
        if self.stop.load(Ordering::Acquire) {
            self.stopping = true;
        }
    }

    fn handle_command(&mut self, command: RuntimeCommand) {
        match command {
            RuntimeCommand::Activate { peer } => self.activate(peer),
            RuntimeCommand::Release { transport_live } => self.release(transport_live),
            RuntimeCommand::TerminalSent { transport_live } => self.terminal_sent(transport_live),
            RuntimeCommand::ReceiverEffects {
                effects,
                keyboard,
                touch_captured_at,
                applied,
            } => {
                if let Some(keyboard) = keyboard {
                    self.next_keyboard = keyboard;
                }
                if self.inject(effects, touch_captured_at)
                    && let Some(applied) = applied
                {
                    let _ = applied.send(Instant::now());
                }
            }
            RuntimeCommand::Reload { config, applied } => {
                let _ = applied.send(self.reload(config));
            }
            RuntimeCommand::DesktopFocus { terminal } => {
                self.terminal = terminal;
                self.virtual_input.keyboard.set_terminal(terminal);
            }
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
        // Nothing is grabbed yet, so an event read after this only brings
        // the tracked state closer to the kernel's.
        self.capture.resync_held();
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

    fn inject(&mut self, effects: Vec<ReceiverEffect>, touch_captured_at: Option<Instant>) -> bool {
        for effect in effects {
            if let Err(diagnostic) = apply_receiver_effect(
                &mut self.virtual_input,
                effect,
                touch_captured_at,
                self.next_keyboard,
            ) {
                self.diagnostic(diagnostic);
                if diagnostic == RuntimeDiagnostic::InjectionRejected {
                    // Without an acknowledgement the daemon closes only the
                    // sending peer, whose cleanup releases what it holds.
                    return false;
                }
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

    fn reload(&mut self, config: LinuxRuntimeConfig) -> Result<(), LinuxRuntimeError> {
        config.validate()?;
        if self.ownership.phase() != OwnershipPhase::Idle {
            self.force_source_release(RuntimeCloseReason::LocalRelease);
        }
        self.activation_chord = ChordTracker::new(
            ConfiguredChord::parse(&config.activation_chord)
                .expect("reloaded runtime configuration was validated"),
        );
        self.escape_chord = ChordTracker::new(
            ConfiguredChord::parse(&config.escape_chord)
                .expect("reloaded runtime configuration was validated"),
        );
        if config.experimental_touchpad != self.config.experimental_touchpad {
            self.release_receiver_state(RuntimeCloseReason::LocalRelease);
            // The session stays open, so its activation keeps its keyboard
            // mode on the new devices.
            let mode = self.virtual_input.keyboard.mode();
            let mut replacement = VirtualInput::create(config.experimental_touchpad)
                .map_err(LinuxRuntimeError::ReloadVirtualInput)?;
            replacement.keyboard.set_terminal(self.terminal);
            replacement
                .keyboard
                .begin(mode)
                .map_err(LinuxRuntimeError::ReloadVirtualInput)?;
            self.virtual_input = replacement;
            self.ready_notified = false;
            self.next_readiness_probe = Instant::now();
        }
        self.config = config;
        self.rescan();
        Ok(())
    }

    fn rescan(&mut self) {
        self.deregister_capture_descriptors();
        let scan = match enumerate_devices() {
            Ok(scan) => scan,
            Err(_) => {
                self.diagnostic(RuntimeDiagnostic::CaptureReadFailed);
                let _ = self.register_capture_descriptors();
                return;
            }
        };
        let selection = select_configured(&self.config.capture_devices, scan.physical_devices());
        let previous_selection_complete = self.capture_selection_complete;
        self.capture_selection_complete = selection.is_complete();
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
                self.suspend_capture();
            }
        }
        if !scan.failures.is_empty() && selection.unmatched > 0 {
            self.diagnostic(RuntimeDiagnostic::CaptureOpenFailed);
        }
        if self.register_capture_descriptors().is_err() {
            self.diagnostic(RuntimeDiagnostic::CaptureOpenFailed);
            self.force_source_release(RuntimeCloseReason::CaptureFault);
            self.suspend_capture();
        }
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
                self.suspend_capture();
                self.next_rescan = Instant::now();
            }
            Err(_) => {
                self.diagnostic(RuntimeDiagnostic::CaptureReadFailed);
                self.force_source_release(RuntimeCloseReason::CaptureFault);
                self.deregister_capture_descriptors();
                self.suspend_capture();
                self.next_rescan = Instant::now();
            }
        }
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
                    self.diagnostic(RuntimeDiagnostic::CapturedFrameQueueFull);
                    self.force_source_release(RuntimeCloseReason::Backpressure);
                }
                Err(TrySendError::Closed(_)) => {
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
            OwnershipEffect::AcquireGrabs => match self
                .capture
                .grab_all(self.config.capture_devices.is_empty())
            {
                Ok(skipped) => {
                    for path in skipped {
                        tracing::info!(
                            path = %path.display(),
                            "left a capture device to the program that grabbed it"
                        );
                    }
                    let _ = self.ownership.grab_succeeded();
                    self.emit_ownership();
                }
                Err(_) => {
                    // An EVIOCGRAB transaction can fail to roll back an
                    // earlier node, and post-grab neutrality rollback can fail
                    // too. Descriptor close is the only reliable recovery.
                    self.deregister_capture_descriptors();
                    self.suspend_capture();
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
                    self.suspend_capture();
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
    }

    fn suspend_capture(&mut self) {
        self.capture.suspend();
        self.activation_chord.clear();
        self.escape_chord.clear();
    }

    fn force_source_release(&mut self, reason: RuntimeCloseReason) {
        let effect = self.ownership.force_release();
        match effect {
            OwnershipEffect::CloseActivationAndReleaseGrabs => {
                let failures = self.capture.ungrab_all();
                if !failures.is_empty() || self.capture.is_grabbed() {
                    self.suspend_capture();
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
    }

    fn stop_all(&mut self) {
        self.force_source_release(RuntimeCloseReason::Stop);
        self.deregister_capture_descriptors();
        let failures = self.capture.suspend();
        self.activation_chord.clear();
        self.escape_chord.clear();
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

    /// Every ownership or peer change goes through here, so the shared status
    /// is updated before the daemon hears about it.
    fn emit_ownership(&mut self) {
        let phase = self.ownership.phase();
        *lock_status(&self.status) = LinuxRuntimeStatus {
            ownership: phase,
            selected_peer: self.selected_peer.clone(),
        };
        self.emit(RuntimeEvent::OwnershipChanged {
            phase,
            selected_peer: self.selected_peer.clone(),
            changed_at: Instant::now(),
            arming_leakage_events: self.arming_leakage_events,
        });
    }

    fn queue_terminal_or_fail_closed(&mut self) {
        if !self.emit(RuntimeEvent::TerminalRequested) {
            // The daemon cannot acknowledge an event it never received. Drop
            // grabs now instead of leaving ownership stuck in Releasing.
            self.force_source_release(RuntimeCloseReason::Backpressure);
        }
    }

    fn diagnostic(&mut self, diagnostic: RuntimeDiagnostic) {
        self.emit(RuntimeEvent::Diagnostic(diagnostic));
    }

    fn emit(&mut self, event: RuntimeEvent) -> bool {
        match self.event_tx.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Closed(_)) => {
                self.stopping = true;
                false
            }
        }
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
    /// Every selector resolved to one node, or, with no selectors, at least
    /// one keyboard or pointer was found.
    fn is_complete(&self) -> bool {
        !self.selected.is_empty() && self.unmatched == 0 && self.ambiguous == 0
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
    if selectors.is_empty() {
        // No selectors captures every keyboard and pointer. zflow's own
        // devices are left out by identity; other programs' virtual devices,
        // such as a remapper's output, are captured like hardware.
        return CaptureSelection {
            selected: devices
                .filter(|device| device.class.is_some() && !device.is_zflow_virtual())
                .cloned()
                .collect(),
            ..CaptureSelection::default()
        };
    }
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
        && selector
            .uniq
            .as_deref()
            .is_none_or(|uniq| device.unique_name.as_deref() == Some(uniq))
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

fn apply_receiver_effect(
    virtual_input: &mut VirtualInput,
    effect: ReceiverEffect,
    touch_captured_at: Option<Instant>,
    keyboard: KeyboardMode,
) -> Result<(), RuntimeDiagnostic> {
    // Keys the keyboard held back go down before a click, a scroll or a
    // touch, so Option+click works. `pointer` is whether this effect presses
    // something, and whether a button or contact is still down after it. A
    // release presses nothing: a seat hold lets it through after dropping its
    // press.
    let pointer = match &effect {
        ReceiverEffect::Button {
            button, pressed, ..
        } => Some((
            *pressed,
            *pressed
                || virtual_input
                    .pointer
                    .held()
                    .iter()
                    .any(|held| held != button)
                || virtual_input.touching(),
        )),
        ReceiverEffect::Motion { delta, .. } if delta.scroll_x != 0 || delta.scroll_y != 0 => {
            Some((
                true,
                !virtual_input.pointer.held().is_empty() || virtual_input.touching(),
            ))
        }
        ReceiverEffect::TouchReplaced { state, .. } => Some((
            !state.is_empty(),
            !state.is_empty() || !virtual_input.pointer.held().is_empty(),
        )),
        _ => None,
    };
    match pointer {
        Some((true, busy)) => virtual_input
            .keyboard
            .pointer(busy)
            .map_err(|error| injection_diagnostic(&error))?,
        Some((false, busy)) => virtual_input.keyboard.pointer_released(busy),
        None => {}
    }
    let result: Result<(), InjectionError> = match effect {
        ReceiverEffect::Motion { delta, .. } => virtual_input.pointer.motion(delta),
        ReceiverEffect::Key { key, pressed, .. } => virtual_input.keyboard.set_key(key, pressed),
        ReceiverEffect::Button {
            button, pressed, ..
        } => virtual_input.pointer.set_button(button, pressed),
        ReceiverEffect::TouchReplaced { state, synthetic } => {
            virtual_input.replace_touch_at(&state, if synthetic { None } else { touch_captured_at })
        }
        ReceiverEffect::ActivationClosed { .. } => {
            return virtual_input
                .release_all()
                .map_err(|_| RuntimeDiagnostic::InjectionFailed);
        }
        ReceiverEffect::ActivationOpened(_) => virtual_input.keyboard.begin(keyboard),
        ReceiverEffect::SnapshotAck { .. } | ReceiverEffect::Rejected { .. } => Ok(()),
    };
    result.map_err(|error| injection_diagnostic(&error))
}

/// Only a failed device write leaves kernel state uncertain. Any other error
/// is input the backend cannot represent, such as touch after the touchpad
/// was disabled, and rejects just the peer that sent it.
fn injection_diagnostic(error: &InjectionError) -> RuntimeDiagnostic {
    match error {
        InjectionError::Emit { .. } | InjectionError::Create { .. } => {
            RuntimeDiagnostic::InjectionFailed
        }
        _ => RuntimeDiagnostic::InjectionRejected,
    }
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
        DeviceClass, FrameAccumulator, ZFLOW_DEVICE_VERSION, ZFLOW_POINTER_NAME,
        ZFLOW_POINTER_PHYS, ZFLOW_POINTER_PRODUCT_ID, ZFLOW_VENDOR_ID,
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

    fn release() -> RuntimeCommand {
        RuntimeCommand::Release {
            transport_live: false,
        }
    }

    #[tokio::test]
    async fn critical_command_reports_a_full_queue_at_its_bound() {
        let (control, _receiver, _poll) = LinuxRuntimeControl::queue(1);
        control.send(release()).unwrap();
        assert_eq!(control.send(release()), Err(RuntimeCommandError::Full));
        assert_eq!(
            control.send_critical(release(), Duration::ZERO).await,
            Err(RuntimeCommandError::Full)
        );
    }

    #[tokio::test]
    async fn critical_command_waits_for_the_input_thread_to_drain() {
        let (control, mut receiver, mut poll) = LinuxRuntimeControl::queue(1);
        control.send(release()).unwrap();
        let drain = std::thread::spawn(move || {
            // The input thread sleeps in poll until the sender wakes it.
            let mut events = Events::with_capacity(4);
            poll.poll(&mut events, Some(Duration::from_secs(5)))
                .unwrap();
            assert!(events.iter().any(|event| event.token() == WAKE_TOKEN));
            assert!(receiver.try_recv().is_ok());
            receiver
        });
        control
            .send_critical(release(), Duration::from_secs(5))
            .await
            .unwrap();
        let mut receiver = drain.join().unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Ok(RuntimeCommand::Release {
                transport_live: false
            })
        ));
        drop(receiver);
        assert_eq!(control.send(release()), Err(RuntimeCommandError::Stopped));
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
            // Configured selectors never look at the class.
            class: None,
        }
    }

    fn selector(path: &str, name: &str, physical_path: &str) -> ConfigDeviceSelector {
        ConfigDeviceSelector {
            path: path.into(),
            name: Some(name.into()),
            phys: Some(physical_path.into()),
            uniq: None,
            vendor: Some(0x1234),
            product: Some(0x5678),
        }
    }

    #[test]
    fn chord_is_aggregate_edge_triggered_across_devices() {
        let chord = ConfiguredChord::parse(&["KEY_LEFTCTRL".into(), "KEY_F12".into()]).unwrap();
        let mut tracker = ChordTracker::new(chord);
        assert!(!tracker.observe(Path::new("one"), &frame(&[(KeyCode::KEY_LEFTCTRL, 1)])));
        assert!(tracker.observe(Path::new("two"), &frame(&[(KeyCode::KEY_F12, 1)])));
        assert!(!tracker.observe(Path::new("two"), &frame(&[(KeyCode::KEY_F12, 2)])));
        assert!(!tracker.observe(Path::new("one"), &frame(&[(KeyCode::KEY_LEFTCTRL, 0)])));
        assert!(tracker.observe(Path::new("one"), &frame(&[(KeyCode::KEY_LEFTCTRL, 1)])));
    }

    #[test]
    fn removal_clears_composite_chord_state() {
        let chord = ConfiguredChord::parse(&["KEY_LEFTCTRL".into(), "KEY_F12".into()]).unwrap();
        let mut tracker = ChordTracker::new(chord);
        tracker.observe(Path::new("one"), &frame(&[(KeyCode::KEY_LEFTCTRL, 1)]));
        tracker.remove_device(Path::new("one"));
        assert!(!tracker.observe(Path::new("two"), &frame(&[(KeyCode::KEY_F12, 1)])));
    }

    #[test]
    fn suspended_capture_forgets_a_half_held_chord() {
        // After SYN_DROPPED or a read fault the descriptors close, so the
        // release of a held chord key is never seen.
        let chord = ConfiguredChord::parse(&["KEY_LEFTCTRL".into(), "KEY_F12".into()]).unwrap();
        let mut tracker = ChordTracker::new(chord);
        tracker.observe(Path::new("one"), &frame(&[(KeyCode::KEY_LEFTCTRL, 1)]));
        tracker.clear();
        assert!(!tracker.observe(Path::new("one"), &frame(&[(KeyCode::KEY_F12, 1)])));
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
            uniq: None,
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
    fn uniq_separates_identical_devices_behind_one_bluetooth_adapter() {
        // Bluetooth devices report the adapter address as phys.
        let keyboard = |path: &str, uniq: &str| DeviceInfo {
            unique_name: Some(uniq.into()),
            ..device(path, "BT Keyboard", "aa:aa:aa:aa:aa:aa", 0x1234, 0x5678)
        };
        let first = keyboard("/dev/input/event8", "11:11:11:11:11:11");
        let second = keyboard("/dev/input/event9", "22:22:22:22:22:22");
        let mut configured = selector("/dev/input/event3", "BT Keyboard", "aa:aa:aa:aa:aa:aa");
        configured.uniq = Some("22:22:22:22:22:22".into());

        let selection = select_configured(&[configured], [&first, &second].into_iter());

        assert!(selection.is_complete());
        assert_eq!(selection.capture_set()[0].path, second.path);
    }

    #[test]
    fn exact_path_fallback_does_not_capture_zflow_virtual_devices() {
        let configured = ConfigDeviceSelector {
            path: "/dev/input/event8".into(),
            name: None,
            phys: None,
            uniq: None,
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

    fn classified(path: &str, name: &str, class: Option<DeviceClass>) -> DeviceInfo {
        DeviceInfo {
            class,
            ..device(path, name, "usb-0000:01/input0", 0x1234, 0x5678)
        }
    }

    fn zflow_pointer(path: &str) -> DeviceInfo {
        let mut pointer = device(
            path,
            ZFLOW_POINTER_NAME,
            ZFLOW_POINTER_PHYS,
            ZFLOW_VENDOR_ID,
            ZFLOW_POINTER_PRODUCT_ID,
        );
        pointer.bus = BusType::BUS_VIRTUAL.0;
        pointer.version = ZFLOW_DEVICE_VERSION;
        pointer.class = Some(DeviceClass::Pointer);
        pointer
    }

    #[test]
    fn empty_selectors_capture_every_keyboard_and_pointer_but_zflows_own() {
        let keyboard = classified(
            "/dev/input/event3",
            "AT Translated Set 2 keyboard",
            Some(DeviceClass::Keyboard),
        );
        let power = classified("/dev/input/event4", "Power Button", None);
        let touchpad = classified(
            "/dev/input/event5",
            "SYNA Touchpad",
            Some(DeviceClass::Touchpad),
        );
        let mut keyd = classified(
            "/dev/input/event20",
            "keyd virtual keyboard",
            Some(DeviceClass::Keyboard),
        );
        keyd.bus = BusType::BUS_VIRTUAL.0;
        let mut openlogi = classified(
            "/dev/input/event21",
            "OpenLogi virtual mouse",
            Some(DeviceClass::Pointer),
        );
        openlogi.bus = BusType::BUS_VIRTUAL.0;
        let own = zflow_pointer("/dev/input/event22");

        let selection = select_configured(
            &[],
            [&keyboard, &power, &touchpad, &keyd, &openlogi, &own].into_iter(),
        );

        assert!(selection.is_complete());
        let selected = selection
            .capture_set()
            .iter()
            .map(|device| device.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            selected,
            [&keyboard, &touchpad, &keyd, &openlogi].map(|device| device.path.clone())
        );
    }

    #[test]
    fn empty_selectors_without_a_keyboard_or_pointer_cannot_activate() {
        let power = classified("/dev/input/event4", "Power Button", None);
        let own = zflow_pointer("/dev/input/event22");

        let selection = select_configured(&[], [&power, &own].into_iter());
        let nothing = select_configured(&[], [].iter());

        for selection in [selection, nothing] {
            assert!(!selection.is_complete());
            assert!(selection.capture_set().is_empty());
            assert_eq!(
                activation_capture_blocker(selection.is_complete(), true),
                Some(RuntimeDiagnostic::CaptureSelectionIncomplete)
            );
        }
    }

    #[test]
    fn configured_selectors_ignore_other_keyboards_and_pointers() {
        let configured = selector(
            "/dev/input/event3",
            "Actually Good Keyboard",
            "usb-0000:01/input0",
        );
        let keyboard = classified(
            "/dev/input/event3",
            "Actually Good Keyboard",
            Some(DeviceClass::Keyboard),
        );
        let mouse = DeviceInfo {
            class: Some(DeviceClass::Pointer),
            ..device(
                "/dev/input/event4",
                "Other Mouse",
                "usb-0000:02/input0",
                0x1234,
                0x9999,
            )
        };

        let selection = select_configured(&[configured], [&keyboard, &mouse].into_iter());

        assert!(selection.is_complete());
        assert_eq!(selection.capture_set(), [keyboard]);
    }

    #[test]
    fn runtime_defaults_do_not_claim_the_linux_vt_switch_chord() {
        let config = LinuxRuntimeConfig::from_config(&Config::default());
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
    fn only_backend_io_failures_stop_the_runtime() {
        use crate::linux::{MappingError, VirtualDeviceRole};
        for rejected in [
            InjectionError::TouchpadDisabled,
            InjectionError::TooManyTouchContacts {
                actual: 6,
                maximum: 5,
            },
            InjectionError::Unsupported(MappingError::UnsupportedHidUsage { page: 7, usage: 0 }),
        ] {
            assert_eq!(
                injection_diagnostic(&rejected),
                RuntimeDiagnostic::InjectionRejected
            );
        }
        assert_eq!(
            injection_diagnostic(&InjectionError::Emit {
                role: VirtualDeviceRole::Pointer,
                source: io::Error::from(io::ErrorKind::BrokenPipe),
            }),
            RuntimeDiagnostic::InjectionFailed
        );
    }

    #[test]
    #[ignore = "requires root-equivalent access to /dev/uinput and selected evdev nodes"]
    fn privileged_runtime_smoke() {
        let runtime =
            LinuxRuntime::spawn(LinuxRuntimeConfig::from_config(&Config::default())).unwrap();
        runtime.shutdown().unwrap();
    }
}
