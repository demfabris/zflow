mod desktop;
mod negotiate;
mod receive;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use tokio::sync::{mpsc, oneshot};

use desktop::DesktopRelay;
use negotiate::{negotiate, validate_negotiated_control, validate_negotiated_motion};
use receive::Inbound;

use crate::{
    capture::{
        CaptureFrame, CaptureTransition, CapturedDeviceFrame, KeyState, MAX_TOUCHPAD_CONTACTS,
    },
    config::{Config, PlayoutMode},
    core::{
        InputCapabilities, InputCapability, MonotonicTimeMicros, NegotiatedSession,
        NegotiationOffer, PlayoutConfig, PlayoutDelayMode, ReceiverConfig, ReceiverEffect,
        ReliableControl, Sender, SenderConfig, SenderTick, SessionCloseReason, SessionContext,
        TransportGeneration,
    },
    metrics::{SessionMetrics, SessionMetricsSnapshot},
    transport::{InputChannels, InputConnection, InputControlMessage, InputDatagram},
};

const SESSION_COMMAND_CAPACITY: usize = 512;
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONTROL_MESSAGES_PER_SECOND: u32 = 20_000;
const MAX_DATAGRAMS_PER_SECOND: u32 = 50_000;
// QUIC only guarantees 1200-byte packets, which leave Quinn about 1162 bytes
// of datagram payload until an MTU probe succeeds. The largest real frame (five
// touch contacts) is about 330 bytes.
const OFFER_DATAGRAM_SIZE: u32 = 1_024;

#[derive(Debug, Clone)]
pub struct SessionOptions {
    offer: NegotiationOffer,
    playout: PlayoutConfig,
}

impl SessionOptions {
    pub fn from_config(config: &Config) -> Result<Self> {
        let checkpoint = Duration::from_millis(config.transport.checkpoint_ms);
        let lease = Duration::from_millis(config.transport.lease_ms);
        SenderConfig::new(checkpoint, lease)?;
        ReceiverConfig::new(lease)?;
        let mut playout = PlayoutConfig::default();
        playout.delay_mode = match config.playout.mode {
            PlayoutMode::Fixed => PlayoutDelayMode::Fixed,
            PlayoutMode::Adaptive => PlayoutDelayMode::Adaptive,
        };
        playout.fixed_delay = Duration::from_millis(config.playout.fixed_delay_ms);
        playout.minimum_delay = Duration::from_millis(config.playout.minimum_delay_ms);
        playout.maximum_delay = Duration::from_millis(config.playout.maximum_delay_ms);
        playout.adaptive_percentile = (config.playout.percentile * 100.0).round() as u8;
        playout.validate()?;
        // Snapshot acks wait for playout. A delay near the lease makes every
        // held-key renewal miss its ack deadline.
        if playout
            .fixed_delay
            .max(playout.maximum_delay)
            .saturating_mul(2)
            >= lease
        {
            bail!(
                "playout.fixed_delay_ms and playout.maximum_delay_ms must be less than half of transport.lease_ms"
            );
        }

        let mut capabilities = InputCapabilities::from([
            InputCapability::Keyboard,
            InputCapability::ConsumerControls,
            InputCapability::Pointer,
            InputCapability::Scroll,
        ]);
        if config.input.experimental_touchpad {
            capabilities.insert(InputCapability::Touch);
        }
        let required_capabilities = InputCapabilities::from([
            InputCapability::Keyboard,
            InputCapability::Pointer,
            InputCapability::Scroll,
        ]);
        Ok(Self {
            offer: NegotiationOffer {
                maximum_datagram_size: OFFER_DATAGRAM_SIZE,
                supported_capabilities: capabilities,
                required_capabilities,
                pointer_units: BTreeSet::from([crate::core::PointerUnit::DeviceUnaccelerated]),
                scroll_fields: crate::core::ScrollFields {
                    high_resolution: true,
                    source_unit: false,
                    source_resolution: false,
                    discrete_steps: true,
                    phase: false,
                    momentum_phase: false,
                },
                maximum_contacts: if config.input.experimental_touchpad {
                    MAX_TOUCHPAD_CONTACTS as u16
                } else {
                    0
                },
                maximum_receiver_lease_ms: config.transport.lease_ms as u32,
                maximum_checkpoint_bound_ms: config.transport.checkpoint_ms as u32,
            },
            playout,
        })
    }
}

pub struct SessionEvent {
    pub session_id: u64,
    pub peer: String,
    pub kind: SessionEventKind,
}

pub enum SessionEventKind {
    Desktop {
        request: crate::desktop::DesktopRequest,
        reply: oneshot::Sender<crate::desktop::DesktopResponse>,
    },
    ReceiverEffects {
        effects: Vec<ReceiverEffect>,
        touch_captured_at: Option<Instant>,
        received_at: Instant,
        applied: oneshot::Sender<Result<(), String>>,
    },
    OutboundEnded,
    Closed {
        reason: String,
    },
}

enum SessionCommand {
    Desktop {
        id: u64,
        request: crate::desktop::DesktopRequest,
        reply: oneshot::Sender<crate::desktop::DesktopResponse>,
    },
    BeginOutbound(SessionContext),
    Capture(CapturedDeviceFrame),
    EndOutbound {
        reason: SessionCloseReason,
        sent: oneshot::Sender<Result<(), String>>,
    },
    Close(SessionCloseReason),
}

#[derive(Clone)]
pub struct SessionHandle {
    id: u64,
    peer: Arc<str>,
    generation: TransportGeneration,
    commands: mpsc::Sender<SessionCommand>,
    connection: crate::transport::DatagramChannel,
    metrics: Arc<Mutex<SessionMetrics>>,
    desktop_id: Arc<std::sync::atomic::AtomicU64>,
    capabilities: InputCapabilities,
}

pub(crate) fn desktop_operation(request: &crate::desktop::DesktopRequest) -> &'static str {
    use crate::desktop::DesktopRequest::*;
    match request {
        Snapshot => "snapshot",
        Prepare { .. } => "prepare",
        Poll { .. } => "poll",
        Finish { .. } => "finish",
    }
}

pub(crate) fn desktop_response_kind(response: &crate::desktop::DesktopResponse) -> &'static str {
    use crate::desktop::DesktopResponse::*;
    match response {
        Snapshot { .. } => "snapshot",
        Prepared { .. } => "prepared",
        Active => "active",
        Returned { .. } => "returned",
        Finished => "finished",
        Unavailable { .. } => "unavailable",
    }
}

impl SessionHandle {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn peer(&self) -> &str {
        &self.peer
    }

    pub fn generation(&self) -> TransportGeneration {
        self.generation
    }

    /// Capabilities both peers agreed on during negotiation.
    pub fn capabilities(&self) -> &InputCapabilities {
        &self.capabilities
    }

    pub fn metrics_snapshot(&self) -> SessionMetricsSnapshot {
        let snapshot_data = { lock_metrics(&self.metrics).snapshot_data() };
        let mut snapshot = snapshot_data.summarize();
        snapshot.datagram_queue_drops = self.connection.dropped_datagrams();
        snapshot
    }

    pub fn record_receive_to_runtime_dispatch(&self, received_at: Instant) {
        lock_metrics(&self.metrics)
            .receive_to_runtime_dispatch_us
            .record(received_at.elapsed().as_secs_f64() * 1_000_000.0);
    }

    pub fn record_receive_to_inject(&self, received_at: Instant, applied_at: Instant) {
        let elapsed = applied_at.saturating_duration_since(received_at);
        lock_metrics(&self.metrics)
            .receive_to_inject_us
            .record(elapsed.as_secs_f64() * 1_000_000.0);
    }

    pub fn record_arming_to_grab(&self, elapsed: Duration) {
        lock_metrics(&self.metrics)
            .arming_to_grab_us
            .record(elapsed.as_secs_f64() * 1_000_000.0);
    }

    pub fn record_switch_time_leakage(&self, events: u64) {
        let mut metrics = lock_metrics(&self.metrics);
        metrics.switch_time_leakage_events =
            metrics.switch_time_leakage_events.saturating_add(events);
    }

    pub fn begin_outbound(&self, context: SessionContext) -> Result<()> {
        self.commands
            .try_send(SessionCommand::BeginOutbound(context))
            .map_err(|error| anyhow!("peer session command queue rejected activation: {error}"))
    }

    pub fn capture(&self, frame: CapturedDeviceFrame) -> Result<()> {
        self.commands
            .try_send(SessionCommand::Capture(frame))
            .map_err(|error| anyhow!("peer session capture queue is unavailable: {error}"))
    }

    pub async fn end_outbound(&self, reason: SessionCloseReason) -> Result<()> {
        let (sent, receipt) = oneshot::channel();
        self.commands
            .try_send(SessionCommand::EndOutbound { reason, sent })
            .map_err(|error| anyhow!("peer session command queue rejected release: {error}"))?;
        receipt
            .await
            .map_err(|_| anyhow!("peer session closed before release was sent"))?
            .map_err(anyhow::Error::msg)
    }

    /// True once the session has stopped, whatever the reason.
    pub fn is_closed(&self) -> bool {
        self.commands.is_closed()
    }

    pub fn close(&self, reason: SessionCloseReason) {
        let _ = self.commands.try_send(SessionCommand::Close(reason));
        // Revocation and backend teardown cannot wait for a peer to drain its
        // critical stream. Transport closure wakes the actor; its single exit
        // path runs receiver lifecycle cleanup and reports synthetic releases.
        self.connection.close();
    }
}

pub async fn start_session(
    connection: InputConnection,
    peer: String,
    generation: TransportGeneration,
    options: SessionOptions,
    events: mpsc::Sender<SessionEvent>,
) -> Result<SessionHandle> {
    let id = random_nonzero_u64()?;
    let (commands, command_rx) = mpsc::channel(SESSION_COMMAND_CAPACITY);
    let (ready_tx, ready_rx) = oneshot::channel();
    let channels = connection.into_channels();
    let connection = channels.datagrams.clone();
    let metrics = Arc::new(Mutex::new(SessionMetrics::default()));
    let handle = SessionHandle {
        id,
        peer: Arc::from(peer.as_str()),
        generation,
        commands,
        connection: connection.clone(),
        metrics: metrics.clone(),
        desktop_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        capabilities: InputCapabilities::default(),
    };

    let reporter = Reporter {
        session_id: id,
        peer: peer.clone(),
        events: events.clone(),
        metrics,
    };
    tokio::spawn(async move {
        let result = run_session(reporter, channels, command_rx, options, ready_tx).await;
        connection.close();
        let reason = result
            .err()
            .map_or_else(|| "session closed".to_owned(), |error| error.to_string());
        tracing::debug!(%peer, session_id = id, %reason, "input session actor stopped");
        let _ = events
            .send(SessionEvent {
                session_id: id,
                peer,
                kind: SessionEventKind::Closed { reason },
            })
            .await;
    });

    match tokio::time::timeout(NEGOTIATION_TIMEOUT, ready_rx).await {
        Ok(Ok(Ok(capabilities))) => Ok(SessionHandle {
            capabilities,
            ..handle
        }),
        Ok(Ok(Err(error))) => {
            handle.close(SessionCloseReason::ProtocolViolation);
            Err(anyhow!(error))
        }
        Ok(Err(_)) => {
            handle.close(SessionCloseReason::ProtocolViolation);
            bail!("peer session stopped during negotiation")
        }
        Err(_) => {
            handle.close(SessionCloseReason::ProtocolViolation);
            bail!("peer session negotiation timed out")
        }
    }
}

async fn run_session(
    reporter: Reporter,
    mut channels: InputChannels,
    mut commands: mpsc::Receiver<SessionCommand>,
    options: SessionOptions,
    ready: oneshot::Sender<Result<InputCapabilities, String>>,
) -> Result<()> {
    let setup = async {
        let negotiated = negotiate(&mut channels, &options.offer).await?;
        channels
            .datagrams
            .configure_maximum(negotiated.maximum_datagram_size)?;
        let lease = Duration::from_millis(u64::from(negotiated.receiver_lease_ms));
        let checkpoint = Duration::from_millis(u64::from(negotiated.checkpoint_bound_ms));
        let sender_config = SenderConfig::new(checkpoint, lease)?;
        let receiver_config = ReceiverConfig::new(lease)?;
        Ok::<_, anyhow::Error>((negotiated, sender_config, receiver_config))
    }
    .await;
    let (negotiated, sender_config, receiver_config) = match setup {
        Ok(setup) => setup,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return Err(error);
        }
    };
    let _ = ready.send(Ok(negotiated.capabilities.clone()));

    let clock = MonotonicClock::new();
    let mut desktop = DesktopRelay::default();
    let mut sender = None;
    let mut capture_merge = CaptureMerger::default();
    let mut inbound = Inbound::new(
        reporter.clone(),
        receiver_config,
        options.playout,
        clock.now(),
    )?;
    let mut control_rate = EventRate::new(MAX_CONTROL_MESSAGES_PER_SECOND);
    let mut datagram_rate = EventRate::new(MAX_DATAGRAMS_PER_SECOND);
    let mut last_tick_at = clock.now();

    let run_result: Result<()> = async {
    loop {
        desktop.dispatch(
            &reporter,
            inbound.has_pending_controls(),
            inbound.active_context().is_some(),
        )?;
        inbound.track_activation(&clock);
        let deadline = inbound
            .deadline(sender.as_ref(), last_tick_at)
            .map(|deadline| clock.0 + Duration::from_micros(deadline.0));
        tokio::select! {
            response = desktop.next_reply() => {
                let (id, response) = response?;
                desktop.send_reply(&reporter, &mut channels, id, response).await?;
            }

            command = commands.recv() => {
                let Some(command) = command else {
                    break;
                };
                match command {
                    SessionCommand::Desktop { id, request, reply } => {
                        desktop.request(&reporter, &mut channels, id, request, reply).await?;
                    }
                    SessionCommand::BeginOutbound(context) => {
                        if sender.is_some() {
                            bail!("outbound activation is already open");
                        }
                        let now = clock.now();
                        capture_merge.clear();
                        let mut next = Sender::new(sender_config, context, now)?;
                        let enter = next.enter(now)?;
                        channels.control_send.send_control(&enter).await?;
                        sender = Some(next);
                    }
                    SessionCommand::Capture(mut frame) => {
                        // Frames queued before the activation ended are stale.
                        let Some(active) = sender.as_mut() else {
                            reporter.count_stale();
                            continue;
                        };
                        let captured_at = frame.captured_at;
                        // Stamp input with when it was captured, not when this
                        // actor got to it. Clamp so sender time never runs
                        // backwards or ahead of now.
                        let at = clock
                            .at(captured_at)
                            .min(clock.now())
                            .max(active.last_observed_time());
                        let transitions = capture_merge.merge(&mut frame);
                        send_capture(&mut channels, active, &negotiated, frame.frame, transitions, at)
                            .await?;
                        reporter
                            .metrics()
                            .capture_to_send_us
                            .record(captured_at.elapsed().as_secs_f64() * 1_000_000.0);
                    }
                    SessionCommand::EndOutbound { reason, sent } => {
                        let result = async {
                            if let Some(mut active) = sender.take()
                                && active.is_remote()
                            {
                                let leave = active.leave(reason, clock.now())?;
                                channels.control_send.send_control(&leave).await?;
                            }
                            capture_merge.clear();
                            reporter.emit(SessionEventKind::OutboundEnded)?;
                            Ok::<_, anyhow::Error>(())
                        }
                        .await;
                        let failed = result.as_ref().err().map(ToString::to_string);
                        let _ = sent.send(result.map_err(|error| error.to_string()));
                        if let Some(error) = failed {
                            bail!(error);
                        }
                    }
                    SessionCommand::Close(reason) => {
                        // The caller closes the transport right away. Connection
                        // loss releases the peer's receiver state; a SessionClose
                        // sent here would race that close and usually be lost.
                        tracing::debug!(peer = %reporter.peer, session_id = reporter.session_id, ?reason, "input session closed locally");
                        sender = None;
                        break;
                    }
                }
            }
            received = channels.control_receive.receive() => {
                control_rate.observe()?;
                let received_at = Instant::now();
                match received? {
                    InputControlMessage::Desktop(message) => desktop.receive(&reporter, message)?,
                    InputControlMessage::NegotiationOffer(_) | InputControlMessage::NegotiatedSession(_) => {
                        bail!("peer repeated session negotiation");
                    }
                    InputControlMessage::Reliable(message) => {
                        validate_negotiated_control(&message.payload, &negotiated)?;
                        match message.payload {
                            ReliableControl::SnapshotAck(ack) => {
                                // An ack can trail a return or a quick re-entry.
                                let Some(active) = sender
                                    .as_mut()
                                    .filter(|active| active.session() == message.session)
                                else {
                                    reporter.count_stale();
                                    continue;
                                };
                                active.acknowledge_snapshot(ack)?;
                                let mut metrics = reporter.metrics();
                                metrics.snapshot_acknowledgements =
                                    metrics.snapshot_acknowledgements.saturating_add(1);
                            }
                            _ => {
                                inbound
                                    .receive_control(&mut channels, message, received_at, clock.now())
                                    .await?;
                            }
                        }
                    }
                }
            }
            received = channels.datagrams.receive() => {
                datagram_rate.observe()?;
                let received_at = Instant::now();
                match received? {
                    InputDatagram::Motion(frame) => {
                        validate_negotiated_motion(&frame, &negotiated)?;
                        inbound.receive_motion(frame, received_at, clock.now())?;
                    }
                    InputDatagram::Probe(probe) => {
                        inbound.receive_probe(&channels, probe, sender.as_ref(), clock.now())?;
                    }
                }
            }
            scheduled_at = wait_for_deadline(deadline) => {
                reporter
                    .metrics()
                    .scheduler_lateness_us
                    .record(scheduled_at.elapsed().as_secs_f64() * 1_000_000.0);
                let now = clock.now();
                last_tick_at = now;
                if let Some(active) = sender.as_mut() {
                    match active.tick(now)? {
                        SenderTick::Checkpoint(checkpoint) => {
                            channels.control_send.send_control(&checkpoint).await?;
                            let mut metrics = reporter.metrics();
                            metrics.lease_renewals = metrics.lease_renewals.saturating_add(1);
                        }
                        SenderTick::ExitRemote(_, close) => {
                            sender = None;
                            // The receiver may still hold the activation open
                            // if only its acks were lost; it has to close for
                            // the desktop handoff to finish.
                            channels.control_send.send_control(&close).await?;
                            reporter.emit(SessionEventKind::OutboundEnded)?;
                        }
                        SenderTick::Idle => {}
                    }
                }
                inbound.tick(&mut channels, &clock, now).await?;
            }
        }
    }
    Ok(())
    }.await;
    let outbound_cleanup_result = if sender.as_ref().is_some_and(Sender::is_remote) {
        sender.take();
        capture_merge.clear();
        reporter.emit(SessionEventKind::OutboundEnded)
    } else {
        Ok(())
    };
    let cleanup_result = inbound.close(&mut channels, clock.now()).await;
    outbound_cleanup_result?;
    cleanup_result?;
    run_result
}

async fn wait_for_deadline(deadline: Option<Instant>) -> Instant {
    let Some(deadline) = deadline else {
        return std::future::pending().await;
    };
    tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    deadline
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CaptureMember {
    Key(crate::core::HidUsage),
    Button(crate::core::PointerButton),
}

#[derive(Debug, Default)]
struct CaptureMerger {
    held_by_device: BTreeMap<PathBuf, BTreeSet<CaptureMember>>,
}

impl CaptureMerger {
    /// Takes the frame's transitions and returns the presses and releases of
    /// the union of all devices. Repeats never change it.
    fn merge(&mut self, captured: &mut CapturedDeviceFrame) -> Vec<(CaptureMember, bool)> {
        let mut aggregate = Vec::with_capacity(captured.frame.transitions.len());
        for transition in captured.frame.transitions.drain(..) {
            let (member, state) = match transition {
                CaptureTransition::Key { usage, state } => (CaptureMember::Key(usage), state),
                CaptureTransition::Button { button, state } => {
                    (CaptureMember::Button(button), state)
                }
            };
            let pressed = match state {
                KeyState::Pressed => true,
                KeyState::Released => false,
                KeyState::Repeat => continue,
            };
            let held_before = self
                .held_by_device
                .values()
                .any(|held| held.contains(&member));
            let device = self
                .held_by_device
                .entry(captured.device_path.clone())
                .or_default();
            let changed = if pressed {
                device.insert(member)
            } else {
                device.remove(&member)
            };
            if !changed {
                continue;
            }
            let held_after = self
                .held_by_device
                .values()
                .any(|held| held.contains(&member));
            if held_before != held_after {
                aggregate.push((member, pressed));
            }
        }
        self.held_by_device.retain(|_, held| !held.is_empty());
        aggregate
    }

    fn clear(&mut self) {
        self.held_by_device.clear();
    }
}

async fn send_capture(
    channels: &mut InputChannels,
    sender: &mut Sender,
    negotiated: &NegotiatedSession,
    frame: CaptureFrame,
    transitions: Vec<(CaptureMember, bool)>,
    now: MonotonicTimeMicros,
) -> Result<()> {
    let touch = negotiated
        .capabilities
        .contains(InputCapability::Touch)
        .then_some(frame.touch_snapshot)
        .flatten();
    let motion = frame.motion;
    match touch {
        Some(state) if sender.held_state().active_touch.is_empty() && !state.is_empty() => {
            let begin = sender.touch_begin(state, now)?;
            channels.control_send.send_control(&begin).await?;
            if motion != crate::core::MotionDelta::default() {
                let frame = sender.capture_motion(motion, None, now)?;
                channels.datagrams.send_motion(&frame)?;
            }
        }
        Some(state) if !sender.held_state().active_touch.is_empty() && state.is_empty() => {
            if motion != crate::core::MotionDelta::default() {
                let frame = sender.capture_motion(motion, None, now)?;
                channels.datagrams.send_motion(&frame)?;
            }
            let end = sender.touch_end(false, now)?;
            channels.control_send.send_control(&end).await?;
        }
        Some(state) if !state.is_empty() => {
            let frame = sender.capture_motion(motion, Some(state), now)?;
            channels.datagrams.send_motion(&frame)?;
        }
        _ if motion != crate::core::MotionDelta::default() => {
            let frame = sender.capture_motion(motion, None, now)?;
            channels.datagrams.send_motion(&frame)?;
        }
        _ => {}
    }
    for (member, pressed) in transitions {
        let message = match (member, pressed) {
            (CaptureMember::Key(usage), true) => sender.key_down(usage, now)?,
            (CaptureMember::Key(usage), false) => sender.key_up(usage, now)?,
            (CaptureMember::Button(button), true) => sender.button_down(button, now)?,
            (CaptureMember::Button(button), false) => sender.button_up(button, now)?,
        };
        channels.control_send.send_control(&message).await?;
    }
    Ok(())
}

/// Where one session reports events and metrics.
#[derive(Clone)]
struct Reporter {
    session_id: u64,
    peer: String,
    events: mpsc::Sender<SessionEvent>,
    metrics: Arc<Mutex<SessionMetrics>>,
}

impl Reporter {
    fn event(&self, kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            session_id: self.session_id,
            peer: self.peer.clone(),
            kind,
        }
    }

    fn emit(&self, kind: SessionEventKind) -> Result<()> {
        self.events
            .try_send(self.event(kind))
            .map_err(|error| anyhow!("daemon session event queue is unavailable: {error}"))
    }

    /// Waits for queue space instead of failing when the queue is full.
    async fn send(&self, kind: SessionEventKind) -> Result<()> {
        self.events
            .send(self.event(kind))
            .await
            .map_err(|_| anyhow!("daemon session event router stopped"))
    }

    fn metrics(&self) -> MutexGuard<'_, SessionMetrics> {
        lock_metrics(&self.metrics)
    }

    fn count_stale(&self) {
        let mut metrics = self.metrics();
        metrics.stale_events_rejected = metrics.stale_events_rejected.saturating_add(1);
    }
}

fn lock_metrics(metrics: &Arc<Mutex<SessionMetrics>>) -> MutexGuard<'_, SessionMetrics> {
    metrics
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn random_nonzero_u64() -> Result<u64> {
    loop {
        let mut bytes = [0_u8; 8];
        getrandom::fill(&mut bytes)
            .map_err(|error| anyhow!("could not generate a session identifier: {error}"))?;
        let value = u64::from_le_bytes(bytes);
        if value != 0 {
            return Ok(value);
        }
    }
}

struct MonotonicClock(Instant);

struct EventRate {
    window_started: Instant,
    count: u32,
    maximum: u32,
}

impl EventRate {
    fn new(maximum: u32) -> Self {
        Self {
            window_started: Instant::now(),
            count: 0,
            maximum,
        }
    }

    fn observe(&mut self) -> Result<()> {
        if self.window_started.elapsed() >= Duration::from_secs(1) {
            self.window_started = Instant::now();
            self.count = 0;
        }
        self.count = self.count.saturating_add(1);
        if self.count > self.maximum {
            bail!("peer exceeded the input event-rate limit");
        }
        Ok(())
    }
}

impl MonotonicClock {
    fn new() -> Self {
        Self(Instant::now())
    }

    fn now(&self) -> MonotonicTimeMicros {
        self.at(Instant::now())
    }

    fn at(&self, instant: Instant) -> MonotonicTimeMicros {
        let elapsed = instant.saturating_duration_since(self.0);
        MonotonicTimeMicros(u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        net::{IpAddr, Ipv4Addr, SocketAddr},
    };

    use tempfile::TempDir;

    use super::{
        negotiate::select_negotiation,
        receive::{
            discard_pending_context, enqueue_pending_control, is_synthetic_release,
            pending_control_is_ready, session_deadline,
        },
        *,
    };
    use crate::{
        core::{
            ActivationId, AnchorKind, ClockConfig, ClockMapper, ControlSequence, CumulativeMotion,
            HidUsage, MotionAnchor, MotionDelta, MotionSequence, PointerButton, ProbeExchange,
            Receiver, ReceiverPlayout, ReliableControlMessage, SessionEpoch, SnapshotAck,
            TouchState,
        },
        identity::Identity,
        transport::{
            TransportError, accept_input, connect_input, input_client_config, input_server_config,
        },
    };

    fn reporter(
        session_id: u64,
        peer: &str,
        events: mpsc::Sender<SessionEvent>,
        metrics: Arc<Mutex<SessionMetrics>>,
    ) -> Reporter {
        Reporter {
            session_id,
            peer: peer.to_owned(),
            events,
            metrics,
        }
    }

    fn identity() -> (TempDir, Identity) {
        let directory = tempfile::tempdir().unwrap();
        let identity = Identity::load_or_create(directory.path()).unwrap();
        (directory, identity)
    }

    fn options() -> SessionOptions {
        SessionOptions::from_config(&Config::default()).unwrap()
    }

    fn context() -> SessionContext {
        SessionContext {
            session_epoch: SessionEpoch([7; 16]),
            transport_generation: TransportGeneration(1),
            activation_id: ActivationId(1),
        }
    }

    fn active_receiver(lease: Duration) -> Receiver {
        let context = context();
        let mut receiver =
            Receiver::new(ReceiverConfig::new(lease).unwrap(), MonotonicTimeMicros(0)).unwrap();
        receiver
            .receive_control(
                ReliableControlMessage {
                    session: context,
                    sequence: ControlSequence(1),
                    payload: ReliableControl::Enter,
                },
                MonotonicTimeMicros(0),
            )
            .unwrap();
        receiver
    }

    async fn input_channel_pair() -> (
        InputChannels,
        InputChannels,
        quinn::Endpoint,
        quinn::Endpoint,
    ) {
        let (_left_directory, left_identity) = identity();
        let (_right_directory, right_identity) = identity();
        let left_client = input_client_config(&left_identity, right_identity.spki()).unwrap();
        let right_server = input_server_config(&right_identity, left_identity.spki()).unwrap();
        let listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let server_endpoint = quinn::Endpoint::server(right_server.quinn_config(), listen).unwrap();
        let client_endpoint = quinn::Endpoint::client(listen).unwrap();
        let server_address = server_endpoint.local_addr().unwrap();
        let accept_endpoint = server_endpoint.clone();
        let accepted = tokio::spawn(async move {
            let incoming = accept_endpoint.accept().await.unwrap();
            accept_input(incoming, &right_server).await.unwrap()
        });
        let left = connect_input(&client_endpoint, server_address, &left_client)
            .await
            .unwrap();
        let right = accepted.await.unwrap();
        (
            left.into_channels(),
            right.into_channels(),
            client_endpoint,
            server_endpoint,
        )
    }

    #[test]
    fn negotiation_is_symmetric_and_requires_the_linux_baseline() {
        let offer = options().offer;
        let selected = select_negotiation(&offer, &offer).unwrap();
        assert!(selected.capabilities.contains(InputCapability::Keyboard));
        assert!(selected.capabilities.contains(InputCapability::Pointer));
        assert!(selected.capabilities.contains(InputCapability::Scroll));

        let mut incomplete = offer.clone();
        incomplete.supported_capabilities = InputCapabilities::from([InputCapability::Keyboard]);
        assert!(select_negotiation(&offer, &incomplete).is_err());
    }

    #[test]
    fn playout_delay_must_stay_below_half_the_lease() {
        let mut config = Config::default();
        config.playout.maximum_delay_ms = 449;
        SessionOptions::from_config(&config).unwrap();
        config.playout.maximum_delay_ms = 450;
        let error = SessionOptions::from_config(&config)
            .unwrap_err()
            .to_string();
        assert!(error.contains("half of transport.lease_ms"), "{error}");

        let mut config = Config::default();
        config.playout.fixed_delay_ms = 450;
        assert!(SessionOptions::from_config(&config).is_err());
    }

    #[test]
    fn touch_is_optional_and_uses_the_smaller_contact_limit() {
        let baseline = options().offer;
        let mut config = Config::default();
        config.input.experimental_touchpad = true;
        let touch = SessionOptions::from_config(&config).unwrap().offer;

        let mixed = select_negotiation(&baseline, &touch).unwrap();
        assert!(!mixed.capabilities.contains(InputCapability::Touch));
        assert_eq!(mixed.contact_limit, 0);

        let mut smaller = touch.clone();
        smaller.maximum_contacts = 3;
        let selected = select_negotiation(&touch, &smaller).unwrap();
        assert!(selected.capabilities.contains(InputCapability::Touch));
        assert_eq!(selected.contact_limit, 3);
    }

    #[test]
    fn negotiated_capabilities_reject_omitted_event_families_before_receiver_state() {
        let offer = options().offer;
        let selected = select_negotiation(&offer, &offer).unwrap();
        assert!(!selected.capabilities.contains(InputCapability::Touch));
        assert!(
            validate_negotiated_control(
                &ReliableControl::TouchBegin {
                    initial_state: TouchState::default(),
                },
                &selected,
            )
            .is_err()
        );

        let frame = crate::core::MotionFrame {
            session: SessionContext {
                session_epoch: SessionEpoch([1; 16]),
                transport_generation: TransportGeneration(1),
                activation_id: ActivationId(1),
            },
            motion_sequence: MotionSequence(1),
            control_watermark: ControlSequence(1),
            sender_capture_time: MonotonicTimeMicros(1),
            totals: crate::core::CumulativeMotion::ZERO,
            touch_snapshot: Some(TouchState::default()),
        };
        assert!(validate_negotiated_motion(&frame, &selected).is_err());

        let mut without_consumer = offer.clone();
        without_consumer.supported_capabilities = InputCapabilities::from([
            InputCapability::Keyboard,
            InputCapability::Pointer,
            InputCapability::Scroll,
        ]);
        let without_consumer = select_negotiation(&offer, &without_consumer).unwrap();
        assert!(
            validate_negotiated_control(
                &ReliableControl::KeyDown {
                    key: HidUsage::consumer(0xe9),
                },
                &without_consumer,
            )
            .is_err()
        );
    }

    #[test]
    fn quiet_session_deadlines_preserve_checkpoints_leases_and_pending_controls() {
        let now = MonotonicTimeMicros(0);
        let probe = MonotonicTimeMicros(100_000);
        let mut receiver = Receiver::new(
            ReceiverConfig::new(Duration::from_millis(900)).unwrap(),
            now,
        )
        .unwrap();
        assert_eq!(
            session_deadline(None, &receiver, None, false, now, probe),
            None
        );
        let mut sender = Sender::new(
            SenderConfig::new(Duration::from_millis(250), Duration::from_millis(900)).unwrap(),
            context(),
            now,
        )
        .unwrap();
        sender.enter(now).unwrap();
        assert_eq!(
            session_deadline(Some(&sender), &receiver, None, false, now, probe),
            sender.next_deadline()
        );
        receiver = active_receiver(Duration::from_millis(5));
        assert_eq!(
            session_deadline(None, &receiver, None, false, now, probe),
            Some(probe)
        );
        receiver
            .receive_control(
                ReliableControlMessage {
                    session: context(),
                    sequence: ControlSequence(2),
                    payload: ReliableControl::KeyDown {
                        key: HidUsage::keyboard(4),
                    },
                },
                now,
            )
            .unwrap();
        assert_eq!(
            session_deadline(None, &receiver, None, false, now, probe),
            Some(MonotonicTimeMicros(5_000))
        );
        assert_eq!(
            session_deadline(None, &receiver, None, true, now, probe),
            Some(MonotonicTimeMicros(1_000))
        );
    }

    #[test]
    fn capture_merge_keeps_overlapping_devices_held_until_the_last_release() {
        let key = HidUsage::keyboard(0xe0);
        let mut merge = CaptureMerger::default();
        let mut frame = |device: &str, state| {
            merge.merge(&mut CapturedDeviceFrame {
                device_path: device.into(),
                frame: CaptureFrame {
                    transitions: vec![CaptureTransition::Key { usage: key, state }],
                    motion: MotionDelta::default(),
                    touch_snapshot: None,
                    event_count: 1,
                },
                captured_at: Instant::now(),
            })
        };

        let key = CaptureMember::Key(key);
        assert_eq!(frame("/dev/input/one", KeyState::Pressed), [(key, true)]);
        assert!(frame("/dev/input/two", KeyState::Pressed).is_empty());
        assert!(frame("/dev/input/one", KeyState::Repeat).is_empty());
        assert!(frame("/dev/input/one", KeyState::Released).is_empty());
        assert_eq!(frame("/dev/input/two", KeyState::Released), [(key, false)]);
    }

    #[test]
    fn only_an_empty_synthetic_touch_counts_as_a_release() {
        let touching = TouchState::new([crate::core::TouchContact {
            id: crate::core::ContactId(1),
            x: 0,
            y: 0,
            pressure: None,
            major: None,
            minor: None,
            orientation_millidegrees: None,
            tool: crate::core::TouchTool::Finger,
            source_dimensions: None,
        }])
        .unwrap();
        let touch = |state: &TouchState, synthetic| ReceiverEffect::TouchReplaced {
            state: state.clone(),
            synthetic,
        };
        assert!(is_synthetic_release(&touch(&TouchState::default(), true)));
        assert!(!is_synthetic_release(&touch(&touching, true)));
        assert!(!is_synthetic_release(&touch(&TouchState::default(), false)));
        assert!(is_synthetic_release(&ReceiverEffect::Key {
            key: HidUsage::keyboard(4),
            pressed: false,
            synthetic: true,
        }));
    }

    #[test]
    fn event_rate_rejects_work_past_the_fixed_window_bound() {
        let mut rate = EventRate::new(2);
        rate.observe().unwrap();
        rate.observe().unwrap();
        assert!(rate.observe().is_err());
    }

    #[test]
    fn anchored_controls_wait_in_fifo_and_follow_clock_remapping() {
        let context = context();
        let receiver = active_receiver(Duration::from_millis(100));
        let mut config = PlayoutConfig {
            delay_mode: PlayoutDelayMode::Fixed,
            fixed_delay: Duration::from_millis(50),
            ..PlayoutConfig::default()
        };
        config.minimum_delay = config.fixed_delay;
        config.maximum_delay = config.fixed_delay;
        let playout = Some(ReceiverPlayout::new(config, context).unwrap());
        let mut clock = ClockMapper::new(ClockConfig::default()).unwrap();
        let metrics = Arc::new(Mutex::new(SessionMetrics::default()));
        let anchor = MotionAnchor {
            activation_id: context.activation_id,
            through_motion_sequence: MotionSequence(1),
            sender_capture_time: MonotonicTimeMicros(1_000),
            totals: CumulativeMotion::new(12, 0, 0, 0),
            final_touch_state: TouchState::default(),
            kind: AnchorKind::Checkpoint,
        };
        let mut pending = VecDeque::new();
        enqueue_pending_control(
            &mut pending,
            ReliableControlMessage {
                session: context,
                sequence: ControlSequence(2),
                payload: ReliableControl::ButtonDown {
                    button: PointerButton::PRIMARY,
                    anchor,
                },
            },
            Instant::now(),
            MonotonicTimeMicros(11_000),
            4_096,
        )
        .unwrap();
        enqueue_pending_control(
            &mut pending,
            ReliableControlMessage {
                session: context,
                sequence: ControlSequence(3),
                payload: ReliableControl::KeyDown {
                    key: HidUsage::keyboard(4),
                },
            },
            Instant::now(),
            MonotonicTimeMicros(11_001),
            4_096,
        )
        .unwrap();

        assert!(
            !pending_control_is_ready(
                pending.front().unwrap(),
                &receiver,
                &playout,
                &mut clock,
                MonotonicTimeMicros(60_999),
                &metrics,
            )
            .unwrap()
        );
        assert_eq!(
            pending.len(),
            2,
            "later controls must remain behind the anchor"
        );
        assert_eq!(receiver.lease_deadline(), None);

        clock
            .ingest_probe(ProbeExchange {
                receiver_sent_at: MonotonicTimeMicros(0),
                sender_received_at: MonotonicTimeMicros(0),
                sender_echoed_at: MonotonicTimeMicros(0),
                receiver_received_at: MonotonicTimeMicros(0),
            })
            .unwrap();
        assert!(
            pending_control_is_ready(
                pending.front().unwrap(),
                &receiver,
                &playout,
                &mut clock,
                MonotonicTimeMicros(51_000),
                &metrics,
            )
            .unwrap()
        );
    }

    #[test]
    fn deferred_control_queue_is_bounded_and_cleared_with_its_activation() {
        let context = context();
        let message = ReliableControlMessage {
            session: context,
            sequence: ControlSequence(2),
            payload: ReliableControl::KeyDown {
                key: HidUsage::keyboard(4),
            },
        };
        let mut pending = VecDeque::new();
        enqueue_pending_control(
            &mut pending,
            message.clone(),
            Instant::now(),
            MonotonicTimeMicros(0),
            1,
        )
        .unwrap();
        assert!(
            enqueue_pending_control(
                &mut pending,
                message,
                Instant::now(),
                MonotonicTimeMicros(0),
                1,
            )
            .is_err()
        );

        let mut receiver = active_receiver(Duration::from_millis(10));
        let closed = receiver.active_context().unwrap();
        receiver.tick(MonotonicTimeMicros(10_000)).unwrap();
        discard_pending_context(&mut pending, closed);
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn snapshot_ack_waits_for_backend_application() {
        let (mut channels, mut peer, _client_endpoint, _server_endpoint) =
            input_channel_pair().await;
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let metrics = Arc::new(Mutex::new(SessionMetrics::default()));
        let context = context();
        let task = tokio::spawn(async move {
            let mut inbound = Inbound::new(
                reporter(1, "peer", event_tx, metrics),
                ReceiverConfig::new(Duration::from_millis(900)).unwrap(),
                PlayoutConfig::default(),
                MonotonicTimeMicros(0),
            )
            .unwrap();
            inbound
                .emit(
                    &mut channels,
                    vec![
                        ReceiverEffect::Key {
                            key: HidUsage::keyboard(4),
                            pressed: true,
                            synthetic: false,
                        },
                        ReceiverEffect::SnapshotAck {
                            session: context,
                            ack: SnapshotAck {
                                snapshot_sequence: ControlSequence(2),
                                accepted_generation: context.transport_generation,
                            },
                        },
                    ],
                    Instant::now(),
                    None,
                )
                .await
        });

        let event = event_rx.recv().await.unwrap();
        let SessionEventKind::ReceiverEffects {
            effects, applied, ..
        } = event.kind
        else {
            panic!("expected receiver effects");
        };
        assert!(matches!(
            effects.as_slice(),
            [ReceiverEffect::Key { pressed: true, .. }]
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), peer.control_receive.receive())
                .await
                .is_err(),
            "wire ACK escaped before backend application"
        );
        applied.send(Ok(())).unwrap();
        task.await.unwrap().unwrap();

        let InputControlMessage::Reliable(message) =
            tokio::time::timeout(Duration::from_secs(1), peer.control_receive.receive())
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("expected reliable ACK");
        };
        assert!(matches!(message.payload, ReliableControl::SnapshotAck(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn loopback_session_delivers_capture_to_a_fake_runtime_boundary() {
        let (_left_directory, left_identity) = identity();
        let (_right_directory, right_identity) = identity();
        let left_client = input_client_config(&left_identity, right_identity.spki()).unwrap();
        let right_server = input_server_config(&right_identity, left_identity.spki()).unwrap();
        let listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let server_endpoint = quinn::Endpoint::server(right_server.quinn_config(), listen).unwrap();
        let client_endpoint = quinn::Endpoint::client(listen).unwrap();
        let server_address = server_endpoint.local_addr().unwrap();
        let accept_endpoint = server_endpoint.clone();
        let accept_config = right_server.clone();
        let accepted = tokio::spawn(async move {
            let incoming = accept_endpoint.accept().await.unwrap();
            accept_input(incoming, &accept_config).await.unwrap()
        });
        let left_connection = connect_input(&client_endpoint, server_address, &left_client)
            .await
            .unwrap();
        let right_connection = accepted.await.unwrap();

        let (event_tx, mut event_rx) = mpsc::channel(64);
        let left = start_session(
            left_connection,
            "right".into(),
            TransportGeneration(1),
            options(),
            event_tx.clone(),
        );
        let right = start_session(
            right_connection,
            "left".into(),
            TransportGeneration(1),
            options(),
            event_tx,
        );
        let (left, right) = tokio::join!(left, right);
        let left = left.unwrap();
        let right = right.unwrap();
        let context = SessionContext {
            session_epoch: SessionEpoch([7; 16]),
            transport_generation: TransportGeneration(1),
            activation_id: ActivationId(1),
        };
        left.begin_outbound(context).unwrap();
        left.capture(CapturedDeviceFrame {
            device_path: "/dev/input/fake".into(),
            frame: CaptureFrame {
                transitions: vec![
                    CaptureTransition::Key {
                        usage: HidUsage::keyboard(0x04),
                        state: KeyState::Pressed,
                    },
                    CaptureTransition::Button {
                        button: PointerButton::PRIMARY,
                        state: KeyState::Pressed,
                    },
                ],
                motion: MotionDelta {
                    dx: 12,
                    dy: -3,
                    scroll_y: 30,
                    ..MotionDelta::default()
                },
                touch_snapshot: None,
                event_count: 5,
            },
            captured_at: Instant::now(),
        })
        .unwrap();

        let mut fake_runtime = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let event = event_rx.recv().await.unwrap();
                if event.peer != "left" {
                    continue;
                }
                if let SessionEventKind::ReceiverEffects {
                    effects, applied, ..
                } = event.kind
                {
                    fake_runtime.extend(effects);
                    applied.send(Ok(())).unwrap();
                    let has_key = fake_runtime
                        .iter()
                        .any(|effect| matches!(effect, ReceiverEffect::Key { pressed: true, .. }));
                    let has_button = fake_runtime.iter().any(|effect| {
                        matches!(effect, ReceiverEffect::Button { pressed: true, .. })
                    });
                    let motion =
                        fake_runtime
                            .iter()
                            .fold(MotionDelta::default(), |mut total, effect| {
                                if let ReceiverEffect::Motion { delta, .. } = effect {
                                    total.dx += delta.dx;
                                    total.dy += delta.dy;
                                    total.scroll_x += delta.scroll_x;
                                    total.scroll_y += delta.scroll_y;
                                }
                                total
                            });
                    let has_motion = motion.dx == 12 && motion.dy == -3 && motion.scroll_y == 30;
                    if has_key && has_button && has_motion {
                        break;
                    }
                }
            }
        })
        .await
        .expect("fake runtime did not receive the loopback input");
        let last_motion = fake_runtime
            .iter()
            .rposition(|effect| matches!(effect, ReceiverEffect::Motion { .. }))
            .unwrap();
        let button = fake_runtime
            .iter()
            .position(|effect| matches!(effect, ReceiverEffect::Button { pressed: true, .. }))
            .unwrap();
        assert!(
            last_motion < button,
            "anchored button reached the backend before cumulative motion matured"
        );

        let left_metrics = left.metrics_snapshot();
        let right_metrics = right.metrics_snapshot();
        assert_eq!(left_metrics.capture_to_send_us.unwrap().count, 1);
        assert!(right_metrics.capture_to_send_us.is_none());
        tokio::time::timeout(Duration::from_secs(3), async {
            while right.metrics_snapshot().clock_offset_us.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("receiver did not establish a clock mapping");

        left.close(SessionCloseReason::LocalRelease);
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut released_key = false;
            let mut released_button = false;
            let mut closed = false;
            while !(released_key && released_button && closed) {
                let event = event_rx.recv().await.unwrap();
                if event.peer != "left" {
                    continue;
                }
                if let SessionEventKind::ReceiverEffects {
                    effects, applied, ..
                } = event.kind
                {
                    released_key |= effects.iter().any(|effect| {
                        matches!(
                            effect,
                            ReceiverEffect::Key {
                                pressed: false,
                                synthetic: true,
                                ..
                            }
                        )
                    });
                    released_button |= effects.iter().any(|effect| {
                        matches!(
                            effect,
                            ReceiverEffect::Button {
                                pressed: false,
                                synthetic: true,
                                ..
                            }
                        )
                    });
                    closed |= effects
                        .iter()
                        .any(|effect| matches!(effect, ReceiverEffect::ActivationClosed { .. }));
                    applied.send(Ok(())).unwrap();
                }
            }
        })
        .await
        .expect("closing the transport did not synthesize held-state releases");
        let receiver_metrics = right.metrics_snapshot();
        assert!(receiver_metrics.clock_offset_us.is_some());
        assert!(receiver_metrics.synthetic_releases >= 2);
        assert_eq!(receiver_metrics.loss, 0);
        right.close(SessionCloseReason::LocalRelease);
        client_endpoint.wait_idle().await;
        server_endpoint.wait_idle().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn transport_loss_ends_active_outbound_before_session_closes() {
        let (_left_directory, left_identity) = identity();
        let (_right_directory, right_identity) = identity();
        let left_client = input_client_config(&left_identity, right_identity.spki()).unwrap();
        let right_server = input_server_config(&right_identity, left_identity.spki()).unwrap();
        let listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let server_endpoint = quinn::Endpoint::server(right_server.quinn_config(), listen).unwrap();
        let client_endpoint = quinn::Endpoint::client(listen).unwrap();
        let server_address = server_endpoint.local_addr().unwrap();
        let accept_endpoint = server_endpoint.clone();
        let accept_config = right_server.clone();
        let accepted = tokio::spawn(async move {
            let incoming = accept_endpoint.accept().await.unwrap();
            accept_input(incoming, &accept_config).await.unwrap()
        });
        let left_connection = connect_input(&client_endpoint, server_address, &left_client)
            .await
            .unwrap();
        let right_connection = accepted.await.unwrap();

        let (event_tx, mut event_rx) = mpsc::channel(64);
        let (left, right) = tokio::join!(
            start_session(
                left_connection,
                "right".into(),
                TransportGeneration(1),
                options(),
                event_tx.clone(),
            ),
            start_session(
                right_connection,
                "left".into(),
                TransportGeneration(1),
                options(),
                event_tx,
            )
        );
        let left = left.unwrap();
        let right = right.unwrap();
        left.begin_outbound(context()).unwrap();

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let event = event_rx.recv().await.unwrap();
                if let SessionEventKind::ReceiverEffects {
                    effects, applied, ..
                } = event.kind
                {
                    let opened = event.peer == "left"
                        && effects
                            .iter()
                            .any(|effect| matches!(effect, ReceiverEffect::ActivationOpened(_)));
                    applied.send(Ok(())).unwrap();
                    if opened {
                        break;
                    }
                }
            }
        })
        .await
        .expect("peer did not observe the outbound activation");

        right.connection.close();
        let lifecycle = tokio::time::timeout(Duration::from_secs(3), async {
            let mut lifecycle = Vec::new();
            loop {
                let event = event_rx.recv().await.unwrap();
                match event.kind {
                    SessionEventKind::ReceiverEffects { applied, .. } => {
                        applied.send(Ok(())).unwrap();
                    }
                    SessionEventKind::OutboundEnded if event.peer == "right" => {
                        lifecycle.push("outbound-ended");
                    }
                    SessionEventKind::Closed { .. } if event.peer == "right" => {
                        lifecycle.push("closed");
                        break lifecycle;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("sender session did not report transport loss");
        assert_eq!(lifecycle, ["outbound-ended", "closed"]);

        left.close(SessionCloseReason::LocalRelease);
        client_endpoint.wait_idle().await;
        server_endpoint.wait_idle().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn negotiated_datagram_size_fits_before_mtu_discovery() {
        let (_left_directory, left_identity) = identity();
        let (_right_directory, right_identity) = identity();
        let client = input_client_config(&left_identity, right_identity.spki()).unwrap();
        let server = input_server_config(&right_identity, left_identity.spki()).unwrap();
        // A lost MTU probe leaves the path at QUIC's 1200-byte floor.
        let mut transport = quinn::TransportConfig::default();
        transport.mtu_discovery_config(None);
        let mut floor = server.quinn_config();
        floor.transport_config(Arc::new(transport));
        let listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let server_endpoint = quinn::Endpoint::server(floor, listen).unwrap();
        let client_endpoint = quinn::Endpoint::client(listen).unwrap();
        let address = server_endpoint.local_addr().unwrap();
        let (events, _event_rx) = mpsc::channel(64);
        let mut oversized = options();
        oversized.offer.maximum_datagram_size = 1_200;

        for (options, fits) in [(oversized, false), (options(), true)] {
            let accept_endpoint = server_endpoint.clone();
            let accept_config = server.clone();
            let accepted = tokio::spawn(async move {
                let incoming = accept_endpoint.accept().await.unwrap();
                accept_input(incoming, &accept_config).await.unwrap()
            });
            let left = connect_input(&client_endpoint, address, &client)
                .await
                .unwrap();
            let right = accepted.await.unwrap();
            let (left, right) = tokio::join!(
                start_session(
                    left,
                    "right".into(),
                    TransportGeneration(1),
                    options.clone(),
                    events.clone(),
                ),
                start_session(
                    right,
                    "left".into(),
                    TransportGeneration(1),
                    options,
                    events.clone(),
                )
            );
            match right {
                Ok(right) => {
                    assert!(fits, "a 1200-byte offer cannot fit the QUIC floor");
                    right.close(SessionCloseReason::LocalRelease);
                }
                Err(error) => {
                    assert!(!fits, "default offer failed at the QUIC floor: {error}");
                    // The real cause reaches the caller instead of a generic stop.
                    assert!(error.to_string().contains("path maximum"), "{error}");
                }
            }
            if let Ok(left) = left {
                left.close(SessionCloseReason::LocalRelease);
            }
        }
    }

    #[test]
    fn transport_errors_remain_sendable() {
        fn assert_send<T: Send>() {}
        assert_send::<TransportError>();
    }
    async fn desktop_test_pair() -> (
        SessionHandle,
        SessionHandle,
        mpsc::Receiver<SessionEvent>,
        quinn::Endpoint,
        quinn::Endpoint,
    ) {
        fn spawn(
            channels: InputChannels,
            id: u64,
            events: mpsc::Sender<SessionEvent>,
        ) -> (
            SessionHandle,
            oneshot::Receiver<Result<InputCapabilities, String>>,
        ) {
            let (commands, command_rx) = mpsc::channel(512);
            let (ready, receipt) = oneshot::channel();
            let connection = channels.datagrams.clone();
            let metrics = Arc::new(Mutex::new(SessionMetrics::default()));
            let handle = SessionHandle {
                id,
                peer: Arc::from("test"),
                generation: TransportGeneration(1),
                commands,
                connection: connection.clone(),
                metrics: metrics.clone(),
                desktop_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
                capabilities: InputCapabilities::default(),
            };
            tokio::spawn(async move {
                let _ = run_session(
                    reporter(id, "test", events, metrics),
                    channels,
                    command_rx,
                    options(),
                    ready,
                )
                .await;
                connection.close();
            });
            (handle, receipt)
        }
        let (left, right, client, server) = input_channel_pair().await;
        let (events, event_rx) = mpsc::channel(64);
        let (left, left_ready) = spawn(left, 1, events.clone());
        let (right, right_ready) = spawn(right, 2, events);
        let (a, b) = tokio::join!(left_ready, right_ready);
        a.unwrap().unwrap();
        b.unwrap().unwrap();
        (left, right, event_rx, client, server)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_sessions_answer_desktop_requests_without_periodic_ticks() {
        use crate::desktop::{DesktopRequest, DesktopResponse};
        let (left, right, mut events, _client, _server) = desktop_test_pair().await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(left.metrics_snapshot().scheduler_lateness_us.is_none());
        assert!(right.metrics_snapshot().scheduler_lateness_us.is_none());
        let source = left.clone();
        let request =
            tokio::spawn(async move { source.desktop_request(DesktopRequest::Snapshot).await });
        let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        let SessionEventKind::Desktop { reply, .. } = event.kind else {
            panic!("expected desktop request")
        };
        reply
            .send(DesktopResponse::unavailable("test desktop"))
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), request)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            DesktopResponse::Unavailable { .. }
        ));
        assert!(left.metrics_snapshot().scheduler_lateness_us.is_none());
        assert!(right.metrics_snapshot().scheduler_lateness_us.is_none());
        left.close(SessionCloseReason::LocalRelease);
        right.close(SessionCloseReason::LocalRelease);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unsupported_button_is_dropped_without_closing_the_peer() {
        let (left, right, mut events, _client, _server) = desktop_test_pair().await;
        left.begin_outbound(context()).unwrap();
        let key = |state| CaptureTransition::Key {
            usage: HidUsage::keyboard(4),
            state,
        };
        // The Mac bridge reports buttons up to 32; Linux injects 1 through 8.
        let button = |state| CaptureTransition::Button {
            button: PointerButton(9),
            state,
        };
        for transition in [
            key(KeyState::Pressed),
            button(KeyState::Pressed),
            button(KeyState::Released),
            key(KeyState::Released),
        ] {
            left.capture(CapturedDeviceFrame {
                device_path: "fake".into(),
                captured_at: Instant::now(),
                frame: CaptureFrame {
                    transitions: vec![transition],
                    ..CaptureFrame::default()
                },
            })
            .unwrap();
        }
        let mut keys = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while keys.len() < 2 {
                let SessionEventKind::ReceiverEffects {
                    effects, applied, ..
                } = events.recv().await.unwrap().kind
                else {
                    continue;
                };
                for effect in effects {
                    match effect {
                        ReceiverEffect::Key {
                            pressed, synthetic, ..
                        } => keys.push((pressed, synthetic)),
                        ReceiverEffect::Button { .. } => {
                            panic!("unsupported button reached the backend")
                        }
                        ReceiverEffect::ActivationClosed { .. } => {
                            panic!("unsupported button closed the activation")
                        }
                        _ => {}
                    }
                }
                applied.send(Ok(())).unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(keys, [(true, false), (false, false)]);
        assert_eq!(right.metrics_snapshot().unsupported_inputs_dropped, 2);
        left.close(SessionCloseReason::LocalRelease);
        right.close(SessionCloseReason::LocalRelease);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn desktop_finish_waits_for_applied_leave() {
        use crate::desktop::*;
        let (left, right, mut events, _client, _server) = desktop_test_pair().await;
        let source = left.clone();
        let prepare = tokio::spawn(async move {
            source
                .desktop_request(DesktopRequest::Prepare {
                    token: 7,
                    edge: Edge::Left,
                    start: 0,
                    end: FRACTION_MAX,
                    position: 500_000,
                })
                .await
        });
        let event = events.recv().await.unwrap();
        let SessionEventKind::Desktop { request, reply } = event.kind else {
            panic!("expected prepare");
        };
        assert!(matches!(request, DesktopRequest::Prepare { token: 7, .. }));
        reply
            .send(DesktopResponse::Prepared {
                geometry: Geometry {
                    monitors: vec![Rect {
                        x: 0,
                        y: 0,
                        width: 1920,
                        height: 1080,
                    }],
                },
                position: Point { x: 3, y: 540 },
            })
            .unwrap();
        assert!(matches!(
            prepare.await.unwrap().unwrap(),
            DesktopResponse::Prepared { .. }
        ));
        left.begin_outbound(context()).unwrap();
        left.capture(CapturedDeviceFrame {
            device_path: "fake".into(),
            captured_at: Instant::now(),
            frame: CaptureFrame {
                transitions: vec![CaptureTransition::Key {
                    usage: HidUsage::keyboard(4),
                    state: KeyState::Pressed,
                }],
                ..CaptureFrame::default()
            },
        })
        .unwrap();
        loop {
            let event = events.recv().await.unwrap();
            if let SessionEventKind::ReceiverEffects {
                effects, applied, ..
            } = event.kind
            {
                let key = effects
                    .iter()
                    .any(|e| matches!(e, ReceiverEffect::Key { pressed: true, .. }));
                applied.send(Ok(())).unwrap();
                if key {
                    break;
                }
            }
        }
        left.end_outbound(SessionCloseReason::LocalRelease)
            .await
            .unwrap();
        let source = left.clone();
        let finish = tokio::spawn(async move {
            source
                .desktop_request(DesktopRequest::Finish { token: 7 })
                .await
        });
        let mut saw_release = false;
        loop {
            let event = events.recv().await.unwrap();
            match event.kind {
                SessionEventKind::ReceiverEffects {
                    effects, applied, ..
                } => {
                    if effects
                        .iter()
                        .any(|e| matches!(e, ReceiverEffect::ActivationClosed { .. }))
                    {
                        assert!(
                            effects
                                .iter()
                                .any(|e| matches!(e, ReceiverEffect::Key { pressed: false, .. }))
                        );
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        assert!(!finish.is_finished(), "Finish overtook backend cleanup");
                        saw_release = true;
                    }
                    applied.send(Ok(())).unwrap();
                }
                SessionEventKind::Desktop { request, reply } => {
                    assert!(saw_release, "Finish overtook Leave");
                    assert_eq!(request, DesktopRequest::Finish { token: 7 });
                    reply.send(DesktopResponse::Finished).unwrap();
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(finish.await.unwrap().unwrap(), DesktopResponse::Finished);
        left.close(SessionCloseReason::LocalRelease);
        right.close(SessionCloseReason::LocalRelease);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelled_desktop_prepare_closes_transport() {
        use crate::desktop::*;
        let (left, right, mut events, _client, _server) = desktop_test_pair().await;
        let source = left.clone();
        let prepare = tokio::spawn(async move {
            source
                .desktop_request(DesktopRequest::Prepare {
                    token: 1,
                    edge: Edge::Left,
                    start: 0,
                    end: FRACTION_MAX,
                    position: 1,
                })
                .await
        });
        let event = events.recv().await.unwrap();
        let SessionEventKind::Desktop { reply, .. } = event.kind else {
            panic!("expected desktop request");
        };
        prepare.abort();
        let _ = prepare.await;
        tokio::time::timeout(Duration::from_secs(1), right.connection.closed())
            .await
            .unwrap();
        drop(reply);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unavailable_desktop_is_explicit_and_dropped_bridge_closes_session() {
        use crate::desktop::*;
        let (left, right, mut events, _client, _server) = desktop_test_pair().await;
        let source = left.clone();
        let pending =
            tokio::spawn(async move { source.desktop_request(DesktopRequest::Snapshot).await });
        let event = events.recv().await.unwrap();
        let SessionEventKind::Desktop { reply, .. } = event.kind else {
            panic!("expected desktop request");
        };
        reply
            .send(DesktopResponse::unavailable("Start the GNOME receiver"))
            .unwrap();
        assert!(matches!(
            pending.await.unwrap().unwrap(),
            DesktopResponse::Unavailable { .. }
        ));
        left.begin_outbound(context()).unwrap();
        left.capture(CapturedDeviceFrame {
            device_path: "fake".into(),
            captured_at: Instant::now(),
            frame: CaptureFrame {
                transitions: vec![CaptureTransition::Key {
                    usage: HidUsage::keyboard(4),
                    state: KeyState::Pressed,
                }],
                ..CaptureFrame::default()
            },
        })
        .unwrap();
        loop {
            if let SessionEventKind::ReceiverEffects {
                effects, applied, ..
            } = events.recv().await.unwrap().kind
            {
                let pressed = effects
                    .iter()
                    .any(|e| matches!(e, ReceiverEffect::Key { pressed: true, .. }));
                applied.send(Ok(())).unwrap();
                if pressed {
                    break;
                }
            }
        }
        let source = left.clone();
        let pending = tokio::spawn(async move {
            source
                .desktop_request(DesktopRequest::Poll { token: 7 })
                .await
        });
        let event = events.recv().await.unwrap();
        let SessionEventKind::Desktop { reply, .. } = event.kind else {
            panic!("expected desktop request");
        };
        drop(reply);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let SessionEventKind::ReceiverEffects {
                    effects, applied, ..
                } = events.recv().await.unwrap().kind
                {
                    let released = effects.iter().any(|e| {
                        matches!(
                            e,
                            ReceiverEffect::Key {
                                pressed: false,
                                synthetic: true,
                                ..
                            }
                        )
                    });
                    applied.send(Ok(())).unwrap();
                    if released {
                        break;
                    }
                }
            }
        })
        .await
        .expect("Bridge loss must release the held key");
        let error = pending.await.unwrap().unwrap_err().to_string();
        assert!(
            error.contains("response channel closed before reply"),
            "{error}"
        );
        tokio::time::timeout(Duration::from_secs(1), right.connection.closed())
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn desktop_request_diagnostics_distinguish_timeout_and_closed_queue() {
        use crate::desktop::DesktopRequest;
        let (left, _right, _events, _client, _server) = desktop_test_pair().await;
        let mut source = left.clone();
        let (commands, _held_receiver) = mpsc::channel(1);
        source.commands = commands;
        let error = source
            .desktop_request(DesktopRequest::Snapshot)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("request timed out after 1000 ms"), "{error}");

        let (commands, receiver) = mpsc::channel(1);
        source.commands = commands;
        drop(receiver);
        let error = source
            .desktop_request(DesktopRequest::Snapshot)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("request queue unavailable: channel closed"),
            "{error}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn acks_and_captures_that_trail_a_return_are_stale_not_fatal() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let (channels, mut peer, _client, _server) = input_channel_pair().await;
            let (commands, command_rx) = mpsc::channel(8);
            let (events, _event_rx) = mpsc::channel(16);
            let (ready, receipt) = oneshot::channel();
            let metrics = Arc::new(Mutex::new(SessionMetrics::default()));
            let actor = tokio::spawn(run_session(
                reporter(1, "late-ack-peer", events, metrics.clone()),
                channels,
                command_rx,
                options(),
                ready,
            ));
            let negotiated = negotiate(&mut peer, &options().offer).await.unwrap();
            peer.datagrams
                .configure_maximum(negotiated.maximum_datagram_size)
                .unwrap();
            receipt.await.unwrap().unwrap();

            let first = context();
            let second = SessionContext {
                activation_id: ActivationId(2),
                ..first
            };
            let late_ack = ReliableControlMessage {
                session: first,
                sequence: ControlSequence(1),
                payload: ReliableControl::SnapshotAck(SnapshotAck {
                    snapshot_sequence: ControlSequence(2),
                    accepted_generation: first.transport_generation,
                }),
            };
            let stale = |count| {
                let metrics = metrics.clone();
                async move {
                    while lock_metrics(&metrics).stale_events_rejected < count {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
            };
            let end_outbound = || async {
                let (sent, done) = oneshot::channel();
                commands
                    .send(SessionCommand::EndOutbound {
                        reason: SessionCloseReason::LocalRelease,
                        sent,
                    })
                    .await
                    .unwrap();
                done.await.unwrap().unwrap();
            };

            commands
                .send(SessionCommand::BeginOutbound(first))
                .await
                .unwrap();
            end_outbound().await;
            // The checkpoint ack and a queued capture land after the return.
            peer.control_send.send_control(&late_ack).await.unwrap();
            commands
                .send(SessionCommand::Capture(CapturedDeviceFrame {
                    device_path: "fake".into(),
                    captured_at: Instant::now(),
                    frame: CaptureFrame {
                        motion: MotionDelta {
                            dx: 4,
                            ..MotionDelta::default()
                        },
                        ..CaptureFrame::default()
                    },
                }))
                .await
                .unwrap();
            stale(2).await;

            // A quick re-entry must also survive the old activation's ack.
            commands
                .send(SessionCommand::BeginOutbound(second))
                .await
                .unwrap();
            peer.control_send.send_control(&late_ack).await.unwrap();
            stale(3).await;
            end_outbound().await;
            assert!(!actor.is_finished(), "a stale event closed the session");
            commands
                .send(SessionCommand::Close(SessionCloseReason::LocalRelease))
                .await
                .unwrap();
            actor.await.unwrap().unwrap();
        })
        .await
        .expect("stale ack regression timed out");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sender_returns_when_a_receiver_stops_acking_neutral_state() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut config = Config::default();
            config.transport.lease_ms = 90;
            config.transport.checkpoint_ms = 30;
            let options = SessionOptions::from_config(&config).unwrap();
            let (channels, mut peer, _client, _server) = input_channel_pair().await;
            let (commands, command_rx) = mpsc::channel(8);
            let (events, mut event_rx) = mpsc::channel(16);
            let (ready, receipt) = oneshot::channel();
            let actor = tokio::spawn(run_session(
                reporter(
                    1,
                    "silent-receiver",
                    events,
                    Arc::new(Mutex::new(SessionMetrics::default())),
                ),
                channels,
                command_rx,
                options.clone(),
                ready,
            ));
            let negotiated = negotiate(&mut peer, &options.offer).await.unwrap();
            peer.datagrams
                .configure_maximum(negotiated.maximum_datagram_size)
                .unwrap();
            receipt.await.unwrap().unwrap();
            let key = |state| {
                SessionCommand::Capture(CapturedDeviceFrame {
                    device_path: "fake".into(),
                    captured_at: Instant::now(),
                    frame: CaptureFrame {
                        transitions: vec![CaptureTransition::Key {
                            usage: HidUsage::keyboard(4),
                            state,
                        }],
                        ..CaptureFrame::default()
                    },
                })
            };
            let mut response = ControlSequence(0);
            // Ack every held-state snapshot, up to and including `until`.
            let mut ack_until =
                async |peer: &mut InputChannels, until: fn(&ReliableControl) -> bool| loop {
                    let InputControlMessage::Reliable(message) =
                        peer.control_receive.receive().await.unwrap()
                    else {
                        panic!("expected reliable control");
                    };
                    if matches!(message.payload, ReliableControl::StateSnapshot(_)) {
                        response.0 += 1;
                        let ack = ReliableControlMessage {
                            session: message.session,
                            sequence: response,
                            payload: ReliableControl::SnapshotAck(SnapshotAck {
                                snapshot_sequence: message.sequence,
                                accepted_generation: message.session.transport_generation,
                            }),
                        };
                        peer.control_send.send_control(&ack).await.unwrap();
                    }
                    if until(&message.payload) {
                        break;
                    }
                };

            commands
                .send(SessionCommand::BeginOutbound(context()))
                .await
                .unwrap();
            commands.send(key(KeyState::Pressed)).await.unwrap();
            ack_until(&mut peer, |payload| {
                matches!(payload, ReliableControl::StateSnapshot(_))
            })
            .await;
            commands.send(key(KeyState::Released)).await.unwrap();
            ack_until(&mut peer, |payload| {
                matches!(payload, ReliableControl::KeyUp { .. })
            })
            .await;

            // The receiver closed the activation during a stall, so neutral
            // checkpoints go unanswered from here on.
            let released = Instant::now();
            loop {
                match event_rx.recv().await.unwrap().kind {
                    SessionEventKind::OutboundEnded => break,
                    SessionEventKind::Closed { reason } => panic!("session failed: {reason}"),
                    _ => {}
                }
            }
            assert!(
                released.elapsed() < Duration::from_millis(500),
                "sender stayed remote for {:?}",
                released.elapsed()
            );
            assert!(!actor.is_finished(), "returning must not end the session");
            // If only the acks were lost, the receiver still holds the
            // activation open until the sender says it is gone.
            loop {
                let InputControlMessage::Reliable(message) =
                    peer.control_receive.receive().await.unwrap()
                else {
                    continue;
                };
                if let ReliableControl::SessionClose { reason, .. } = message.payload {
                    assert_eq!(reason, SessionCloseReason::LeaseExpired);
                    break;
                }
            }
            commands
                .send(SessionCommand::Close(SessionCloseReason::LocalRelease))
                .await
                .unwrap();
            actor.await.unwrap().unwrap();
        })
        .await
        .expect("silent receiver regression timed out");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn anchors_carry_capture_time_not_handling_time() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let (channels, mut peer, _client, _server) = input_channel_pair().await;
            let (commands, command_rx) = mpsc::channel(8);
            let (events, _event_rx) = mpsc::channel(16);
            let (ready, receipt) = oneshot::channel();
            let actor = tokio::spawn(run_session(
                reporter(
                    1,
                    "capture-time-peer",
                    events,
                    Arc::new(Mutex::new(SessionMetrics::default())),
                ),
                channels,
                command_rx,
                options(),
                ready,
            ));
            let negotiated = negotiate(&mut peer, &options().offer).await.unwrap();
            peer.datagrams
                .configure_maximum(negotiated.maximum_datagram_size)
                .unwrap();
            receipt.await.unwrap().unwrap();
            let button = |button, state, captured_at| {
                SessionCommand::Capture(CapturedDeviceFrame {
                    device_path: "fake".into(),
                    captured_at,
                    frame: CaptureFrame {
                        transitions: vec![CaptureTransition::Button { button, state }],
                        ..CaptureFrame::default()
                    },
                })
            };

            commands
                .send(SessionCommand::BeginOutbound(context()))
                .await
                .unwrap();
            let InputControlMessage::Reliable(enter) =
                peer.control_receive.receive().await.unwrap()
            else {
                panic!("expected Enter");
            };
            assert_eq!(enter.payload, ReliableControl::Enter);
            let mut next_anchor = async || loop {
                let InputControlMessage::Reliable(message) =
                    peer.control_receive.receive().await.unwrap()
                else {
                    panic!("expected reliable control");
                };
                if let Some(anchor) = message.payload.motion_anchor() {
                    break anchor.sender_capture_time.0;
                }
            };
            // Captured 8 ms apart, handled together after a stall.
            let base = Instant::now();
            tokio::time::sleep(Duration::from_millis(20)).await;
            for (target, state, offset) in [
                (PointerButton::PRIMARY, KeyState::Pressed, 0),
                (PointerButton(2), KeyState::Pressed, 8),
                (PointerButton::PRIMARY, KeyState::Released, 4),
            ] {
                let captured_at = base + Duration::from_millis(offset);
                commands
                    .send(button(target, state, captured_at))
                    .await
                    .unwrap();
            }
            let first = next_anchor().await;
            let second = next_anchor().await;
            let third = next_anchor().await;
            assert!(
                (second - first).abs_diff(8_000) <= 1,
                "capture spacing became {} us",
                second - first
            );
            assert_eq!(third, second, "an older capture moved sender time back");
            assert!(!actor.is_finished());
            commands
                .send(SessionCommand::Close(SessionCloseReason::LocalRelease))
                .await
                .unwrap();
            actor.await.unwrap().unwrap();
        })
        .await
        .expect("capture time regression timed out");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn touch_capture_times_survive_datagrams_and_checkpoint_recovery() {
        use crate::core::{ContactId, SourceDimensions, TouchContact, TouchTool};

        fn touch(x: i32) -> TouchState {
            TouchState::new([TouchContact {
                id: ContactId(1),
                x,
                y: 500,
                pressure: None,
                major: None,
                minor: None,
                orientation_millidegrees: None,
                tool: TouchTool::Finger,
                source_dimensions: Some(SourceDimensions {
                    width: 2000,
                    height: 1000,
                }),
            }])
            .unwrap()
        }

        async fn next_touch(
            events: &mut mpsc::Receiver<SessionEvent>,
            expected: &TouchState,
        ) -> (Option<Instant>, oneshot::Sender<Result<(), String>>) {
            loop {
                match events
                    .recv()
                    .await
                    .expect("receiver event stream ended")
                    .kind
                {
                    SessionEventKind::ReceiverEffects {
                        effects,
                        touch_captured_at,
                        applied,
                        ..
                    } => {
                        if effects.iter().any(|effect| {
                            matches!(effect,
                                ReceiverEffect::TouchReplaced { state, .. } if state == expected
                            )
                        }) {
                            return (touch_captured_at, applied);
                        }
                        applied.send(Ok(())).unwrap();
                    }
                    _ => panic!("unexpected session event"),
                }
            }
        }

        tokio::time::timeout(Duration::from_secs(3), async {
            let (channels, mut peer, _client, _server) = input_channel_pair().await;
            let mut config = Config::default();
            config.input.experimental_touchpad = true;
            config.transport.lease_ms = 900;
            config.transport.checkpoint_ms = 250;
            config.playout.mode = PlayoutMode::Fixed;
            config.playout.fixed_delay_ms = 3;
            let options = SessionOptions::from_config(&config).unwrap();
            let offer = options.offer.clone();
            let (commands, command_rx) = mpsc::channel(4);
            let (events, mut event_rx) = mpsc::channel(16);
            let (ready, receipt) = oneshot::channel();
            let mut actor = tokio::spawn(run_session(
                reporter(1, "manual-touch-peer", events, Arc::new(Mutex::new(SessionMetrics::default()))),
                channels,
                command_rx,
                options,
                ready,
            ));
            let negotiated = negotiate(&mut peer, &offer).await.unwrap();
            assert!(negotiated.capabilities.contains(InputCapability::Touch));
            peer.datagrams.configure_maximum(negotiated.maximum_datagram_size).unwrap();
            receipt.await.unwrap().unwrap();
            let mut sender = Sender::new(
                SenderConfig::new(Duration::from_millis(250), Duration::from_millis(900)).unwrap(),
                context(),
                MonotonicTimeMicros(0),
            ).unwrap();
            peer.control_send.send_control(&sender.enter(MonotonicTimeMicros(0)).unwrap()).await.unwrap();
            peer.control_send.send_control(&sender.touch_begin(touch(100), MonotonicTimeMicros(0)).unwrap()).await.unwrap();
            let (initial_time, initial_applied) = next_touch(&mut event_rx, &touch(100)).await;
            assert_eq!(initial_time, None, "TouchBegin carries no source timestamp");
            initial_applied.send(Ok(())).unwrap();

            let motion = sender.capture_motion(
                MotionDelta::default(), Some(touch(200)), MonotonicTimeMicros(100_000),
            ).unwrap();
            peer.datagrams.send_motion(&motion).unwrap();
            let (motion_time, motion_applied) = next_touch(&mut event_rx, &touch(200)).await;
            let motion_time = motion_time.expect("datagram touch lost its capture timestamp");
            // Delay backend application so arrival/injection timestamps cannot
            // accidentally satisfy the eight-millisecond source spacing below.
            tokio::time::sleep(Duration::from_millis(40)).await;
            motion_applied.send(Ok(())).unwrap();

            let _lost = sender.capture_motion(
                MotionDelta::default(), Some(touch(300)), MonotonicTimeMicros(108_000),
            ).unwrap();
            let checkpoint = sender.snapshot(MonotonicTimeMicros(108_000)).unwrap();
            peer.control_send.send_control(&checkpoint).await.unwrap();
            let (checkpoint_time, checkpoint_applied) = next_touch(&mut event_rx, &touch(300)).await;
            let checkpoint_time = checkpoint_time.expect("checkpoint recovery lost its capture timestamp");
            let spacing = checkpoint_time.duration_since(motion_time);
            assert!(spacing.abs_diff(Duration::from_millis(8)) < Duration::from_millis(2),
                "source samples were 8 ms apart, but backend timestamps were {spacing:?} apart");
            assert!(tokio::time::timeout(Duration::from_millis(20), peer.control_receive.receive()).await.is_err(),
                "checkpoint ACK escaped before touch was applied");
            checkpoint_applied.send(Ok(())).unwrap();
            let InputControlMessage::Reliable(reply) = peer.control_receive.receive().await.unwrap() else {
                panic!("expected checkpoint acknowledgement");
            };
            assert!(matches!(reply.payload, ReliableControl::SnapshotAck(ack)
                if ack.snapshot_sequence == checkpoint.sequence));

            commands.send(SessionCommand::Close(SessionCloseReason::LocalRelease)).await.unwrap();
            loop {
                tokio::select! {
                    result = &mut actor => { result.unwrap().unwrap(); break; }
                    event = event_rx.recv() => {
                        if let Some(SessionEvent { kind: SessionEventKind::ReceiverEffects { applied, .. }, .. }) = event {
                            applied.send(Ok(())).unwrap();
                        }
                    }
                }
            }
        }).await.expect("touch session regression timed out");
    }
}
