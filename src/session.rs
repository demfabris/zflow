use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::{mpsc, oneshot};

use crate::{
    capture::{CaptureTransition, CapturedDeviceFrame, KeyState, MAX_TOUCHPAD_CONTACTS},
    config::{Config, PlayoutMode},
    core::{
        ActiveScroll, ClockConfig, ClockMapper, ControlSequence, HeldState, HidUsage, HidUsagePage,
        InputCapabilities, InputCapability, MonotonicTimeMicros, MotionAnchor, MotionSequence,
        NegotiatedSession, NegotiationOffer, PlayoutConfig, PlayoutDelayMode, ProbeExchange,
        ProbeMessage, ProbePayload, ProbeSequence, ProtocolVersion, Receiver, ReceiverConfig,
        ReceiverEffect, ReceiverLifecycle, ReceiverPlayout, RejectionReason, ReliableControl,
        ReliableControlMessage, ScrollUnit, Sender, SenderConfig, SenderTick, SessionCloseReason,
        SessionContext, SessionEpoch, TouchState, TransportGeneration,
    },
    metrics::{SessionMetrics, SessionMetricsSnapshot},
    transport::{InputChannels, InputConnection, InputControlMessage, InputDatagram},
    wire::CURRENT_PROTOCOL_VERSION,
};

const SESSION_COMMAND_CAPACITY: usize = 512;
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_TICK: Duration = Duration::from_millis(1);
const PROBE_INTERVAL: Duration = Duration::from_millis(100);
const PROBE_MAX_AGE: Duration = Duration::from_secs(2);
const MAX_PENDING_PROBES: usize = 64;
const CLOSE_SEND_TIMEOUT: Duration = Duration::from_millis(50);
const MAX_CONTROL_MESSAGES_PER_SECOND: u32 = 20_000;
const MAX_DATAGRAMS_PER_SECOND: u32 = 50_000;
const OFFER_DATAGRAM_SIZE: u32 = 1_200;

struct PendingControl {
    message: ReliableControlMessage,
    received_at: Instant,
    receiver_received_at: MonotonicTimeMicros,
}

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
                protocol_versions: vec![CURRENT_PROTOCOL_VERSION],
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
    /// One scoped desktop operation. A timeout closes transport so a late warp cannot
    /// leave the source believing that a cancelled handoff completed.
    pub async fn desktop_request(
        &self,
        request: crate::desktop::DesktopRequest,
    ) -> Result<crate::desktop::DesktopResponse> {
        request.validate()?;
        let operation = desktop_operation(&request);
        let started = Instant::now();
        let id = self
            .desktop_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        struct CancelOnDrop {
            handle: SessionHandle,
            completed: bool,
            id: u64,
            operation: &'static str,
            started: Instant,
        }
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                if !self.completed {
                    tracing::debug!(peer = %self.handle.peer, session_id = self.handle.id, request_id = self.id, operation = self.operation, elapsed_ms = self.started.elapsed().as_millis() as u64, "desktop request wait canceled; closing transport");
                    self.handle.close(SessionCloseReason::BackendUnavailable);
                }
            }
        }
        let mut guard = CancelOnDrop {
            handle: self.clone(),
            completed: false,
            id,
            operation,
            started,
        };
        anyhow::ensure!(id != 0, "Desktop request ID exhausted");
        let (reply, receiver) = oneshot::channel();
        if let Err(error) = self
            .commands
            .try_send(SessionCommand::Desktop { id, request, reply })
        {
            guard.completed = true;
            self.close(SessionCloseReason::BackendUnavailable);
            tracing::warn!(peer = %self.peer, session_id = self.id, request_id = id, operation, %error, "desktop request queue unavailable");
            bail!("Desktop {operation} request queue unavailable: {error}");
        }
        if operation != "poll" {
            tracing::debug!(peer = %self.peer, session_id = self.id, request_id = id, operation, "desktop request queued");
        } else {
            tracing::trace!(peer = %self.peer, session_id = self.id, request_id = id, operation, "desktop request queued");
        }
        match tokio::time::timeout(
            Duration::from_millis(crate::desktop::REQUEST_TIMEOUT_MS),
            receiver,
        )
        .await
        {
            Ok(Ok(response)) => {
                guard.completed = true;
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let outcome = desktop_response_kind(&response);
                if operation != "poll"
                    || elapsed_ms >= crate::desktop::POLL_HOLD_MS + 150
                    || outcome != "active"
                {
                    tracing::debug!(peer = %self.peer, session_id = self.id, request_id = id, operation, outcome, elapsed_ms, "desktop request completed");
                } else {
                    tracing::trace!(peer = %self.peer, session_id = self.id, request_id = id, operation, outcome, elapsed_ms, "desktop request completed");
                }
                if let crate::desktop::DesktopResponse::Unavailable { reason } = &response {
                    tracing::warn!(peer = %self.peer, session_id = self.id, request_id = id, operation, %reason, "desktop receiver unavailable");
                }
                Ok(response)
            }
            Ok(Err(_)) => {
                guard.completed = true;
                self.close(SessionCloseReason::BackendUnavailable);
                tracing::warn!(peer = %self.peer, session_id = self.id, request_id = id, operation, elapsed_ms = started.elapsed().as_millis() as u64, "desktop response channel closed before reply");
                bail!(
                    "Desktop {operation} response channel closed before reply; input session ended"
                )
            }
            Err(_) => {
                guard.completed = true;
                self.close(SessionCloseReason::BackendUnavailable);
                tracing::warn!(peer = %self.peer, session_id = self.id, request_id = id, operation, elapsed_ms = started.elapsed().as_millis() as u64, timeout_ms = crate::desktop::REQUEST_TIMEOUT_MS, "desktop request timed out");
                bail!(
                    "Desktop {operation} request timed out after {} ms",
                    crate::desktop::REQUEST_TIMEOUT_MS
                )
            }
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn peer(&self) -> &str {
        &self.peer
    }

    pub fn generation(&self) -> TransportGeneration {
        self.generation
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
    };

    tokio::spawn(async move {
        let result = run_session(
            id,
            peer.clone(),
            channels,
            command_rx,
            options,
            events.clone(),
            metrics,
            ready_tx,
        )
        .await;
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
        Ok(Ok(Ok(()))) => Ok(handle),
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

#[allow(clippy::too_many_arguments)]
async fn run_session(
    session_id: u64,
    peer: String,
    mut channels: InputChannels,
    mut commands: mpsc::Receiver<SessionCommand>,
    options: SessionOptions,
    events: mpsc::Sender<SessionEvent>,
    metrics: Arc<Mutex<SessionMetrics>>,
    ready: oneshot::Sender<Result<(), String>>,
) -> Result<()> {
    let negotiated = match negotiate(&mut channels, &options.offer).await {
        Ok(negotiated) => negotiated,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return Err(error);
        }
    };
    channels
        .datagrams
        .configure_maximum(negotiated.maximum_datagram_size)?;
    let negotiated_lease = Duration::from_millis(u64::from(negotiated.receiver_lease_ms));
    let negotiated_checkpoint = Duration::from_millis(u64::from(negotiated.checkpoint_bound_ms));
    let sender_config = SenderConfig::new(negotiated_checkpoint, negotiated_lease)?;
    let receiver_config = ReceiverConfig::new(negotiated_lease)?;
    let _ = ready.send(Ok(()));

    let clock = MonotonicClock::new();
    let mut desktop_waiter: Option<(u64, oneshot::Sender<crate::desktop::DesktopResponse>)> = None;
    let mut desktop_incoming: Option<(u64, crate::desktop::DesktopRequest, Instant)> = None;
    let mut desktop_reply: Option<(
        u64,
        oneshot::Receiver<crate::desktop::DesktopResponse>,
        Instant,
    )> = None;
    let mut sender = None;
    let mut capture_merge = CaptureMerger::default();
    let mut receiver = Receiver::new(receiver_config, clock.now())?;
    let mut authorized = None;
    let mut clock_mapper = ClockMapper::new(ClockConfig::default())?;
    let mut playout = None;
    let mut response_sequence = ControlSequence(0);
    let mut next_probe_sequence = ProbeSequence(1);
    let mut pending_probes = BTreeMap::new();
    let mut pending_controls = VecDeque::new();
    let mut motion_received_at = BTreeMap::new();
    let mut probe_context = None;
    let mut control_rate = EventRate::new(MAX_CONTROL_MESSAGES_PER_SECOND);
    let mut datagram_rate = EventRate::new(MAX_DATAGRAMS_PER_SECOND);
    let mut next_probe_at = add_duration(clock.now(), PROBE_INTERVAL);
    let mut tick = tokio::time::interval(SESSION_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let run_result: Result<()> = async {
    loop {
        if let Some((id, receiver, started)) = desktop_reply.as_mut() {
            match receiver.try_recv() {
                Ok(response) => {
                    tracing::trace!(%peer, session_id, request_id = *id, outcome = desktop_response_kind(&response), elapsed_ms = started.elapsed().as_millis() as u64, "desktop reply sending to peer");
                    channels.control_send.send_desktop(crate::desktop::DesktopMessage::Response { id: *id, response }).await?;
                    desktop_reply = None;
                }
                Err(oneshot::error::TryRecvError::Closed) => bail!("Desktop receiver stopped"),
                Err(oneshot::error::TryRecvError::Empty) => {
                    anyhow::ensure!(started.elapsed() < Duration::from_millis(crate::desktop::REQUEST_TIMEOUT_MS), "Desktop receiver timed out");
                }
            }
        }
        if let Some((_, request, started)) = &desktop_incoming {
            anyhow::ensure!(started.elapsed() < Duration::from_millis(crate::desktop::REQUEST_TIMEOUT_MS), "Desktop operation ordering timed out");
            // Finish follows Leave on this stream. Wait through playout and the
            // daemon's backend receipt before acknowledging desktop cleanup.
            let ready = pending_controls.is_empty()
                && (!matches!(request, crate::desktop::DesktopRequest::Finish { .. }) || receiver.active_context().is_none());
            if ready && desktop_reply.is_none() {
                let (id, request, started) = desktop_incoming.take().unwrap();
                tracing::trace!(%peer, session_id, request_id = id, operation = desktop_operation(&request), elapsed_ms = started.elapsed().as_millis() as u64, "desktop request dispatching to receiver");
                let (reply, receipt) = oneshot::channel();
                emit_event(&events, SessionEvent { session_id, peer: peer.clone(), kind: SessionEventKind::Desktop {request, reply} })?;
                desktop_reply = Some((id, receipt, started));
            }
        }
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    break;
                };
                match command {
                    SessionCommand::Desktop {id,request,reply} => {
                        if desktop_waiter.is_some() {
                            tracing::debug!(%peer, session_id, request_id = id, operation = desktop_operation(&request), "desktop request rejected while previous reply pending");
                            let _ = reply.send(crate::desktop::DesktopResponse::unavailable("A desktop request is already pending"));
                        } else {
                            tracing::trace!(%peer, session_id, request_id = id, operation = desktop_operation(&request), "desktop request sending to peer");
                            channels.control_send.send_desktop(crate::desktop::DesktopMessage::Request {id,request}).await?;
                            desktop_waiter=Some((id,reply));
                        }
                    }
                    SessionCommand::BeginOutbound(context) => {
                        if sender.is_some() {
                            bail!("outbound activation is already open");
                        }
                        if context.protocol_version != negotiated.protocol_version {
                            bail!("outbound activation uses the wrong negotiated protocol");
                        }
                        let now = clock.now();
                        capture_merge.clear();
                        let mut next = Sender::new(sender_config, context, now)?;
                        let enter = next.enter(now)?;
                        channels.control_send.send_control(&enter).await?;
                        sender = Some(next);
                    }
                    SessionCommand::Capture(frame) => {
                        let Some(active) = sender.as_mut() else {
                            bail!("capture arrived without an outbound activation");
                        };
                        let captured_at = frame.captured_at;
                        let frame = capture_merge.merge(frame);
                        send_capture(
                            &mut channels,
                            active,
                            &negotiated,
                            frame,
                            clock.now(),
                        ).await?;
                        lock_metrics(&metrics)
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
                            emit_event(
                                &events,
                                SessionEvent {
                                    session_id,
                                    peer: peer.clone(),
                                    kind: SessionEventKind::OutboundEnded,
                                },
                            )?;
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
                        if let Some(mut active) = sender.take()
                            && active.is_remote()
                            && let Ok(close) = active.leave(reason, clock.now())
                        {
                            let _ = tokio::time::timeout(
                                CLOSE_SEND_TIMEOUT,
                                channels.control_send.send_control(&close),
                            )
                            .await;
                        }
                        capture_merge.clear();
                        break;
                    }
                }
            }
            received = channels.control_receive.receive() => {
                control_rate.observe()?;
                let received_at = Instant::now();
                let message = received?;
                match message {
                    InputControlMessage::Desktop(crate::desktop::DesktopMessage::Request {id,request}) => {
                        tracing::trace!(%peer, session_id, request_id = id, operation = desktop_operation(&request), "desktop request received from peer");
                        anyhow::ensure!(desktop_incoming.is_none() && desktop_reply.is_none(), "Overlapping desktop requests");
                        desktop_incoming=Some((id,request,Instant::now()));
                    }
                    InputControlMessage::Desktop(crate::desktop::DesktopMessage::Response {id,response}) => {
                        tracing::trace!(%peer, session_id, request_id = id, outcome = desktop_response_kind(&response), "desktop response received from peer");
                        let (expected,reply)=desktop_waiter.take().context("Unexpected desktop response")?;
                        anyhow::ensure!(id == expected, "Desktop response ID does not match request");
                        let _=reply.send(response);
                    }
                    InputControlMessage::NegotiationOffer(_) | InputControlMessage::NegotiatedSession(_) => {
                        bail!("peer repeated session negotiation");
                    }
                    InputControlMessage::Reliable(message) => {
                        validate_negotiated_control(&message.payload, &negotiated)?;
                        match message.payload {
                            ReliableControl::SnapshotAck(ack) => {
                                let Some(active) = sender.as_mut() else {
                                    bail!("snapshot acknowledgement arrived without an outbound activation");
                                };
                                if message.session != active.session() {
                                    bail!("snapshot acknowledgement named the wrong activation");
                                }
                                active.acknowledge_snapshot(ack)?;
                                let mut metrics = lock_metrics(&metrics);
                                metrics.snapshot_acknowledgements =
                                    metrics.snapshot_acknowledgements.saturating_add(1);
                            }
                            ReliableControl::TakeoverAccepted(accepted) => {
                                let Some(active) = sender.as_mut() else {
                                    bail!("takeover acknowledgement arrived without an outbound activation");
                                };
                                active.accept_takeover(accepted)?;
                            }
                            _ => {
                                let now = clock.now();
                                enqueue_pending_control(
                                    &mut pending_controls,
                                    message,
                                    received_at,
                                    now,
                                    options.playout.maximum_queued_frames,
                                )?;
                                drain_pending_controls(
                                    &mut pending_controls,
                                    &mut channels,
                                    &mut receiver,
                                    &mut authorized,
                                    &mut playout,
                                    &mut clock_mapper,
                                    &mut response_sequence,
                                    now,
                                    &options,
                                    &events,
                                    session_id,
                                    &peer,
                                    &metrics,
                                    &mut motion_received_at,
                                ).await?;
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
                        receive_motion(
                            frame,
                            clock.now(),
                            &mut playout,
                            &mut clock_mapper,
                            &metrics,
                            &mut motion_received_at,
                            received_at,
                        )?;
                    }
                    InputDatagram::Probe(probe) => {
                        handle_probe(
                            &channels,
                            probe,
                            sender.as_ref(),
                            receiver.active_context(),
                            &mut pending_probes,
                            &mut clock_mapper,
                            &mut playout,
                            clock.now(),
                            &metrics,
                        )?;
                    }
                }
            }
            scheduled_at = tick.tick() => {
                lock_metrics(&metrics)
                    .scheduler_lateness_us
                    .record(scheduled_at.elapsed().as_secs_f64() * 1_000_000.0);
                let now = clock.now();
                if let Some(active) = sender.as_mut() {
                    match active.tick(now)? {
                        SenderTick::Checkpoint(checkpoint) => {
                            channels.control_send.send_control(&checkpoint).await?;
                            let mut metrics = lock_metrics(&metrics);
                            metrics.lease_renewals = metrics.lease_renewals.saturating_add(1);
                        }
                        SenderTick::ExitRemote(_) => {
                            sender = None;
                            emit_event(
                                &events,
                                SessionEvent {
                                    session_id,
                                    peer: peer.clone(),
                                    kind: SessionEventKind::OutboundEnded,
                                },
                            )?;
                        }
                        SenderTick::Idle => {}
                    }
                }

                let active_before_tick = receiver.active_context();
                let effects = receiver.tick(now)?;
                emit_receiver_effects(
                    &mut channels,
                    effects,
                    &mut response_sequence,
                    &events,
                    session_id,
                    &peer,
                    &metrics,
                    Instant::now(),
                ).await?;
                if let Some(closed) = active_before_tick
                    && receiver.active_context() != Some(closed)
                {
                    discard_pending_context(&mut pending_controls, closed);
                }
                poll_playout(
                    &mut receiver,
                    &mut playout,
                    now,
                    &mut channels,
                    &mut response_sequence,
                    &events,
                    session_id,
                    &peer,
                    &metrics,
                    &mut motion_received_at,
                ).await?;
                drain_pending_controls(
                    &mut pending_controls,
                    &mut channels,
                    &mut receiver,
                    &mut authorized,
                    &mut playout,
                    &mut clock_mapper,
                    &mut response_sequence,
                    now,
                    &options,
                    &events,
                    session_id,
                    &peer,
                    &metrics,
                    &mut motion_received_at,
                ).await?;

                let active_context = receiver.active_context();
                if probe_context != active_context {
                    pending_probes.clear();
                    probe_context = active_context;
                    next_probe_at = add_duration(now, PROBE_INTERVAL);
                }
                let oldest_probe = now.0.saturating_sub(
                    u64::try_from(PROBE_MAX_AGE.as_micros()).unwrap_or(u64::MAX),
                );
                pending_probes.retain(|_, sent_at| sent_at.0 >= oldest_probe);
                if now >= next_probe_at
                    && pending_probes.len() < MAX_PENDING_PROBES
                    && let Some(context) = active_context
                {
                    let probe = ProbeMessage {
                        session: context,
                        payload: ProbePayload::Probe {
                            sequence: next_probe_sequence,
                            sent_at: now,
                        },
                    };
                    channels.datagrams.send_probe(&probe)?;
                    pending_probes.insert(next_probe_sequence, now);
                    next_probe_sequence = ProbeSequence(
                        next_probe_sequence
                            .0
                            .checked_add(1)
                            .context("probe sequence exhausted")?,
                    );
                    next_probe_at = add_duration(now, PROBE_INTERVAL);
                }
            }
        }
    }
    Ok(())
    }.await;
    let outbound_cleanup_result = if sender.as_ref().is_some_and(Sender::is_remote) {
        sender.take();
        capture_merge.clear();
        emit_event(
            &events,
            SessionEvent {
                session_id,
                peer: peer.clone(),
                kind: SessionEventKind::OutboundEnded,
            },
        )
    } else {
        Ok(())
    };
    let cleanup_result = close_receiver(
        &mut receiver,
        &events,
        session_id,
        &peer,
        clock.now(),
        ReceiverLifecycle::ConnectionLost,
        &metrics,
    )
    .await;
    outbound_cleanup_result?;
    cleanup_result?;
    run_result
}

async fn negotiate(
    channels: &mut InputChannels,
    local: &NegotiationOffer,
) -> Result<NegotiatedSession> {
    channels.control_send.send_negotiation_offer(local).await?;
    let remote = match channels.control_receive.receive().await? {
        InputControlMessage::NegotiationOffer(offer) => offer,
        _ => bail!("peer did not begin with a negotiation offer"),
    };
    let selected = select_negotiation(local, &remote)?;
    channels
        .control_send
        .send_negotiated_session(&selected)
        .await?;
    let peer_selected = match channels.control_receive.receive().await? {
        InputControlMessage::NegotiatedSession(session) => session,
        _ => bail!("peer did not finish session negotiation"),
    };
    if peer_selected != selected {
        bail!("peer selected a different session schema");
    }
    Ok(selected)
}

fn select_negotiation(
    left: &NegotiationOffer,
    right: &NegotiationOffer,
) -> Result<NegotiatedSession> {
    let version = left
        .protocol_versions
        .iter()
        .filter(|version| right.protocol_versions.contains(version))
        .max()
        .copied()
        .context("peers have no protocol version in common")?;
    let capabilities = InputCapabilities::new(
        left.supported_capabilities
            .iter()
            .filter(|capability| right.supported_capabilities.contains(*capability)),
    );
    if !capabilities.is_superset(&left.required_capabilities)
        || !capabilities.is_superset(&right.required_capabilities)
    {
        bail!("peer lacks a required input capability");
    }
    let pointer_unit = left
        .pointer_units
        .intersection(&right.pointer_units)
        .next()
        .copied();
    let scroll_fields = intersect_scroll_fields(left.scroll_fields, right.scroll_fields);
    let contact_limit = if capabilities.contains(InputCapability::Touch) {
        left.maximum_contacts.min(right.maximum_contacts)
    } else {
        0
    };
    let selected = NegotiatedSession {
        protocol_version: version,
        maximum_datagram_size: left.maximum_datagram_size.min(right.maximum_datagram_size),
        capabilities,
        pointer_unit,
        scroll_fields,
        contact_limit,
        receiver_lease_ms: left
            .maximum_receiver_lease_ms
            .min(right.maximum_receiver_lease_ms),
        checkpoint_bound_ms: left
            .maximum_checkpoint_bound_ms
            .min(right.maximum_checkpoint_bound_ms),
    };
    selected.validate_for(left)?;
    selected.validate_for(right)?;
    Ok(selected)
}

fn validate_negotiated_control(
    payload: &ReliableControl,
    negotiated: &NegotiatedSession,
) -> Result<()> {
    match payload {
        ReliableControl::Enter
        | ReliableControl::SnapshotAck(_)
        | ReliableControl::TakeoverAccepted(_) => {}
        ReliableControl::Leave { anchor } => validate_negotiated_anchor(anchor, negotiated)?,
        ReliableControl::KeyDown { key } | ReliableControl::KeyUp { key } => {
            validate_negotiated_usage(*key, negotiated)?;
        }
        ReliableControl::ButtonDown { anchor, .. } | ReliableControl::ButtonUp { anchor, .. } => {
            require_capability(negotiated, InputCapability::Pointer, "pointer button")?;
            validate_negotiated_anchor(anchor, negotiated)?;
        }
        ReliableControl::ScrollBegin { scroll } => {
            validate_negotiated_scroll(*scroll, negotiated)?;
        }
        ReliableControl::ScrollEnd { anchor, .. }
        | ReliableControl::ScrollCancel { anchor, .. } => {
            require_capability(negotiated, InputCapability::Scroll, "scroll lifecycle")?;
            if !negotiated.scroll_fields.phase {
                bail!("peer sent scroll phase without negotiating phase support");
            }
            validate_negotiated_anchor(anchor, negotiated)?;
        }
        ReliableControl::TouchBegin { initial_state } => {
            require_capability(negotiated, InputCapability::Touch, "touch control")?;
            validate_negotiated_touch(initial_state, negotiated)?;
        }
        ReliableControl::TouchEnd { anchor } | ReliableControl::TouchCancel { anchor } => {
            require_capability(negotiated, InputCapability::Touch, "touch control")?;
            validate_negotiated_anchor(anchor, negotiated)?;
        }
        ReliableControl::StateSnapshot(snapshot) => {
            validate_negotiated_held(&snapshot.held, negotiated)?;
            validate_negotiated_anchor(&snapshot.motion_anchor, negotiated)?;
        }
        ReliableControl::SessionTakeover(takeover) => {
            validate_negotiated_held(&takeover.authoritative_held_state, negotiated)?;
            validate_negotiated_anchor(&takeover.final_motion_anchor, negotiated)?;
        }
        ReliableControl::SessionClose { final_anchor, .. } => {
            if let Some(anchor) = final_anchor {
                validate_negotiated_anchor(anchor, negotiated)?;
            }
        }
    }
    Ok(())
}

fn validate_negotiated_motion(
    frame: &crate::core::MotionFrame,
    negotiated: &NegotiatedSession,
) -> Result<()> {
    let totals = frame.totals;
    if totals.total_dx() != 0 || totals.total_dy() != 0 {
        require_capability(negotiated, InputCapability::Pointer, "pointer motion")?;
    }
    if totals.total_scroll_x() != 0 || totals.total_scroll_y() != 0 {
        require_capability(negotiated, InputCapability::Scroll, "scroll motion")?;
        if !negotiated.scroll_fields.high_resolution {
            bail!("peer sent high-resolution scroll totals without negotiating them");
        }
    }
    if let Some(touch) = &frame.touch_snapshot {
        require_capability(negotiated, InputCapability::Touch, "touch snapshot")?;
        validate_negotiated_touch(touch, negotiated)?;
    }
    Ok(())
}

fn validate_negotiated_usage(usage: HidUsage, negotiated: &NegotiatedSession) -> Result<()> {
    let capability = match usage.page {
        HidUsagePage::KEYBOARD_KEYPAD => InputCapability::Keyboard,
        HidUsagePage::CONSUMER => InputCapability::ConsumerControls,
        _ => bail!("peer sent a key from an unsupported HID usage page"),
    };
    require_capability(negotiated, capability, "key usage")
}

fn validate_negotiated_held(state: &HeldState, negotiated: &NegotiatedSession) -> Result<()> {
    for key in &state.pressed_keys {
        validate_negotiated_usage(*key, negotiated)?;
    }
    if !state.modifiers.is_empty() {
        require_capability(negotiated, InputCapability::Keyboard, "modifier state")?;
    }
    if !state.pressed_buttons.is_empty() {
        require_capability(negotiated, InputCapability::Pointer, "pointer button state")?;
    }
    if let Some(scroll) = state.active_scroll {
        validate_negotiated_scroll(scroll, negotiated)?;
    }
    validate_negotiated_touch(&state.active_touch, negotiated)
}

fn validate_negotiated_anchor(anchor: &MotionAnchor, negotiated: &NegotiatedSession) -> Result<()> {
    let totals = anchor.totals;
    if totals.total_dx() != 0 || totals.total_dy() != 0 {
        require_capability(negotiated, InputCapability::Pointer, "pointer anchor")?;
    }
    if totals.total_scroll_x() != 0 || totals.total_scroll_y() != 0 {
        require_capability(negotiated, InputCapability::Scroll, "scroll anchor")?;
        if !negotiated.scroll_fields.high_resolution {
            bail!("peer sent high-resolution scroll totals without negotiating them");
        }
    }
    validate_negotiated_touch(&anchor.final_touch_state, negotiated)
}

fn validate_negotiated_scroll(scroll: ActiveScroll, negotiated: &NegotiatedSession) -> Result<()> {
    require_capability(negotiated, InputCapability::Scroll, "scroll lifecycle")?;
    if !negotiated.scroll_fields.phase {
        bail!("peer sent scroll phase without negotiating phase support");
    }
    if scroll.source.unit != ScrollUnit::Device && !negotiated.scroll_fields.source_unit {
        bail!("peer sent a scroll source unit without negotiating it");
    }
    if (scroll.source.resolution_x.is_some() || scroll.source.resolution_y.is_some())
        && !negotiated.scroll_fields.source_resolution
    {
        bail!("peer sent scroll resolution without negotiating it");
    }
    if scroll.momentum_phase.is_some() && !negotiated.scroll_fields.momentum_phase {
        bail!("peer sent scroll momentum without negotiating it");
    }
    Ok(())
}

fn require_capability(
    negotiated: &NegotiatedSession,
    capability: InputCapability,
    event: &str,
) -> Result<()> {
    if !negotiated.capabilities.contains(capability) {
        bail!("peer sent {event} without negotiating {capability:?}");
    }
    Ok(())
}

fn validate_negotiated_touch(state: &TouchState, negotiated: &NegotiatedSession) -> Result<()> {
    if state.len() > usize::from(negotiated.contact_limit) {
        bail!("peer exceeded the negotiated touch contact limit");
    }
    if !state.is_empty() && !negotiated.capabilities.contains(InputCapability::Touch) {
        bail!("peer sent touch state without negotiating touch support");
    }
    Ok(())
}

fn intersect_scroll_fields(
    left: crate::core::ScrollFields,
    right: crate::core::ScrollFields,
) -> crate::core::ScrollFields {
    crate::core::ScrollFields {
        high_resolution: left.high_resolution && right.high_resolution,
        source_unit: left.source_unit && right.source_unit,
        source_resolution: left.source_resolution && right.source_resolution,
        discrete_steps: left.discrete_steps && right.discrete_steps,
        phase: left.phase && right.phase,
        momentum_phase: left.momentum_phase && right.momentum_phase,
    }
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
    fn merge(&mut self, mut captured: CapturedDeviceFrame) -> CapturedDeviceFrame {
        let mut aggregate = Vec::with_capacity(captured.frame.transitions.len());
        for transition in captured.frame.transitions {
            let (member, state) = match transition {
                CaptureTransition::Key { usage, state } => (CaptureMember::Key(usage), state),
                CaptureTransition::Button { button, state } => {
                    (CaptureMember::Button(button), state)
                }
            };
            if state == KeyState::Repeat {
                continue;
            }
            let held_before = self
                .held_by_device
                .values()
                .any(|held| held.contains(&member));
            let device = self
                .held_by_device
                .entry(captured.device_path.clone())
                .or_default();
            let changed = match state {
                KeyState::Pressed => device.insert(member),
                KeyState::Released => device.remove(&member),
                KeyState::Repeat => unreachable!(),
            };
            if !changed {
                continue;
            }
            let held_after = self
                .held_by_device
                .values()
                .any(|held| held.contains(&member));
            if held_before != held_after {
                aggregate.push(match member {
                    CaptureMember::Key(usage) => CaptureTransition::Key { usage, state },
                    CaptureMember::Button(button) => CaptureTransition::Button { button, state },
                });
            }
        }
        self.held_by_device.retain(|_, held| !held.is_empty());
        captured.frame.transitions = aggregate;
        captured
    }

    fn clear(&mut self) {
        self.held_by_device.clear();
    }
}

async fn send_capture(
    channels: &mut InputChannels,
    sender: &mut Sender,
    negotiated: &NegotiatedSession,
    captured: CapturedDeviceFrame,
    now: MonotonicTimeMicros,
) -> Result<()> {
    let touch = negotiated
        .capabilities
        .contains(InputCapability::Touch)
        .then_some(captured.frame.touch_snapshot)
        .flatten();
    let motion = captured.frame.motion;
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
    for transition in captured.frame.transitions {
        let message = match transition {
            CaptureTransition::Key {
                usage,
                state: KeyState::Pressed,
            } => Some(sender.key_down(usage, now)?),
            CaptureTransition::Key {
                usage,
                state: KeyState::Released,
            } => Some(sender.key_up(usage, now)?),
            CaptureTransition::Key {
                state: KeyState::Repeat,
                ..
            } => None,
            CaptureTransition::Button {
                button,
                state: KeyState::Pressed,
            } => Some(sender.button_down(button, now)?),
            CaptureTransition::Button {
                button,
                state: KeyState::Released,
            } => Some(sender.button_up(button, now)?),
            CaptureTransition::Button {
                state: KeyState::Repeat,
                ..
            } => None,
        };
        if let Some(message) = message {
            channels.control_send.send_control(&message).await?;
        }
    }
    Ok(())
}

fn enqueue_pending_control(
    pending: &mut VecDeque<PendingControl>,
    message: ReliableControlMessage,
    received_at: Instant,
    receiver_received_at: MonotonicTimeMicros,
    maximum: usize,
) -> Result<()> {
    if pending.len() >= maximum {
        bail!("deferred reliable-control queue reached its configured bound");
    }
    pending.push_back(PendingControl {
        message,
        received_at,
        receiver_received_at,
    });
    Ok(())
}

fn anchor_targets_active_context(message: &ReliableControlMessage, active: SessionContext) -> bool {
    let Some(anchor) = message.payload.motion_anchor() else {
        return false;
    };
    if anchor.activation_id != active.activation_id
        || message.session.protocol_version != active.protocol_version
        || message.session.session_epoch != active.session_epoch
    {
        return false;
    }
    match &message.payload {
        ReliableControl::SessionTakeover(takeover) => {
            takeover.prior_generation == active.transport_generation
                && takeover.proposed_generation == message.session.transport_generation
        }
        _ => message.session == active,
    }
}

fn message_belongs_to_context(message: &ReliableControlMessage, context: SessionContext) -> bool {
    if message.session == context {
        return true;
    }
    matches!(
        &message.payload,
        ReliableControl::SessionTakeover(takeover)
            if message.session.protocol_version == context.protocol_version
                && message.session.session_epoch == context.session_epoch
                && message.session.activation_id == context.activation_id
                && takeover.prior_generation == context.transport_generation
    )
}

fn discard_pending_context(pending: &mut VecDeque<PendingControl>, context: SessionContext) {
    pending.retain(|item| !message_belongs_to_context(&item.message, context));
}

fn pending_control_is_ready(
    pending: &PendingControl,
    receiver: &Receiver,
    playout: &Option<ReceiverPlayout>,
    clock: &mut ClockMapper,
    now: MonotonicTimeMicros,
    metrics: &Arc<Mutex<SessionMetrics>>,
) -> Result<bool> {
    let Some(anchor) = pending.message.payload.motion_anchor() else {
        return Ok(true);
    };
    let Some(active) = receiver.active_context() else {
        return Ok(true);
    };
    if !anchor_targets_active_context(&pending.message, active) {
        return Ok(true);
    }
    let delay = playout
        .as_ref()
        .filter(|playout| playout.session() == active)
        .context("active receiver has no matching playout scheduler")?
        .current_delay();
    if !clock.is_ready() {
        clock.bootstrap_from_arrival(anchor.sender_capture_time, pending.receiver_received_at)?;
        update_clock_metrics(&mut lock_metrics(metrics), clock);
    }
    let mapped_capture_time = clock.map(anchor.sender_capture_time)?;
    Ok(add_duration(mapped_capture_time, delay) <= now)
}

#[allow(clippy::too_many_arguments)]
async fn drain_pending_controls(
    pending: &mut VecDeque<PendingControl>,
    channels: &mut InputChannels,
    receiver: &mut Receiver,
    authorized: &mut Option<(ProtocolVersion, SessionEpoch, TransportGeneration)>,
    playout: &mut Option<ReceiverPlayout>,
    clock_mapper: &mut ClockMapper,
    response_sequence: &mut ControlSequence,
    now: MonotonicTimeMicros,
    options: &SessionOptions,
    events: &mpsc::Sender<SessionEvent>,
    session_id: u64,
    peer: &str,
    metrics: &Arc<Mutex<SessionMetrics>>,
    motion_received_at: &mut BTreeMap<MotionSequence, Instant>,
) -> Result<()> {
    while let Some(front) = pending.front() {
        if !pending_control_is_ready(front, receiver, playout, clock_mapper, now, metrics)? {
            break;
        }
        let item = pending
            .pop_front()
            .expect("ready reliable control came from the queue");
        let active_before = receiver.active_context();
        apply_control(
            channels,
            receiver,
            authorized,
            playout,
            clock_mapper,
            response_sequence,
            item.message,
            now,
            options,
            events,
            session_id,
            peer,
            metrics,
            item.received_at,
            motion_received_at,
        )
        .await?;
        if let Some(previous) = active_before
            && receiver.active_context() != Some(previous)
        {
            discard_pending_context(pending, previous);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn apply_control(
    channels: &mut InputChannels,
    receiver: &mut Receiver,
    authorized: &mut Option<(ProtocolVersion, SessionEpoch, TransportGeneration)>,
    playout: &mut Option<ReceiverPlayout>,
    clock_mapper: &mut ClockMapper,
    response_sequence: &mut ControlSequence,
    message: ReliableControlMessage,
    now: MonotonicTimeMicros,
    options: &SessionOptions,
    events: &mpsc::Sender<SessionEvent>,
    session_id: u64,
    peer: &str,
    metrics: &Arc<Mutex<SessionMetrics>>,
    received_at: Instant,
    motion_received_at: &mut BTreeMap<MotionSequence, Instant>,
) -> Result<()> {
    let boundary = (
        message.session.protocol_version,
        message.session.session_epoch,
        message.session.transport_generation,
    );
    let takeover = matches!(message.payload, ReliableControl::SessionTakeover(_));
    if *authorized != Some(boundary) && !takeover {
        let effects = receiver.authorize_session(message.session, now)?;
        if authorized.is_some_and(|(_, epoch, _)| epoch != message.session.session_epoch) {
            clock_mapper.on_session_epoch_transition();
            let mut metrics = lock_metrics(metrics);
            metrics.epoch_changes = metrics.epoch_changes.saturating_add(1);
            update_clock_metrics(&mut metrics, clock_mapper);
        } else if authorized
            .is_some_and(|(_, _, generation)| generation != message.session.transport_generation)
        {
            let mut metrics = lock_metrics(metrics);
            metrics.generation_changes = metrics.generation_changes.saturating_add(1);
        }
        *authorized = Some(boundary);
        emit_receiver_effects(
            channels,
            effects,
            response_sequence,
            events,
            session_id,
            peer,
            metrics,
            received_at,
        )
        .await?;
    }

    let anchor = message.payload.motion_anchor().cloned();
    let sequence = message.sequence;
    let effects = receiver.receive_control(message, now)?;
    let takeover_accepted = takeover
        && effects
            .iter()
            .any(|effect| matches!(effect, ReceiverEffect::TakeoverAccepted { .. }));
    if takeover_accepted {
        if authorized.is_some_and(|(_, _, generation)| generation != boundary.2) {
            let mut metrics = lock_metrics(metrics);
            metrics.generation_changes = metrics.generation_changes.saturating_add(1);
        }
        *authorized = Some(boundary);
        *playout = Some(ReceiverPlayout::new(
            options.playout,
            receiver
                .active_context()
                .expect("takeover kept activation open"),
        )?);
    }
    let rejected = effects
        .iter()
        .any(|effect| matches!(effect, ReceiverEffect::Rejected { .. }));
    if !rejected {
        if effects
            .iter()
            .any(|effect| matches!(effect, ReceiverEffect::ActivationOpened(_)))
        {
            lock_metrics(metrics).begin_activation();
            *playout = Some(ReceiverPlayout::new(
                options.playout,
                receiver.active_context().expect("activation opened"),
            )?);
        }
        if let Some(active) = playout.as_mut() {
            active.advance_control_watermark(sequence)?;
            if let Some(anchor) = anchor {
                let rebase = active.rebase(anchor.through_motion_sequence, anchor.totals)?;
                motion_received_at.retain(|sequence, _| *sequence > anchor.through_motion_sequence);
                let mut metrics = lock_metrics(metrics);
                metrics.explicit_rebases = metrics.explicit_rebases.saturating_add(1);
                metrics.rebase_discarded_pointer_units = metrics
                    .rebase_discarded_pointer_units
                    .saturating_add(rebase.discarded_displacement.dx.unsigned_abs())
                    .saturating_add(rebase.discarded_displacement.dy.unsigned_abs());
                metrics.rebase_discarded_scroll_units = metrics
                    .rebase_discarded_scroll_units
                    .saturating_add(rebase.discarded_displacement.scroll_x.unsigned_abs())
                    .saturating_add(rebase.discarded_displacement.scroll_y.unsigned_abs());
            }
        }
    }
    emit_receiver_effects(
        channels,
        effects,
        response_sequence,
        events,
        session_id,
        peer,
        metrics,
        received_at,
    )
    .await
}

fn receive_motion(
    frame: crate::core::MotionFrame,
    now: MonotonicTimeMicros,
    playout: &mut Option<ReceiverPlayout>,
    clock: &mut ClockMapper,
    metrics: &Arc<Mutex<SessionMetrics>>,
    motion_received_at: &mut BTreeMap<MotionSequence, Instant>,
    received_at: Instant,
) -> Result<()> {
    let Some(playout) = playout.as_mut() else {
        let mut metrics = lock_metrics(metrics);
        metrics.stale_events_rejected = metrics.stale_events_rejected.saturating_add(1);
        return Ok(());
    };
    if playout.session() != frame.session {
        let mut metrics = lock_metrics(metrics);
        metrics.stale_events_rejected = metrics.stale_events_rejected.saturating_add(1);
        return Ok(());
    }
    lock_metrics(metrics).observe_motion_sequence(frame.motion_sequence.0);
    // One sample gives playout a bounded bootstrap mapping. Further arrival
    // times contain network jitter and must not train the affine clock model;
    // authenticated probe exchanges replace the bootstrap fit.
    if !clock.is_ready() {
        clock.bootstrap_from_arrival(frame.sender_capture_time, now)?;
        update_clock_metrics(&mut lock_metrics(metrics), clock);
    }
    match playout.ingest_frame(frame, now, clock)? {
        crate::core::EnqueueOutcome::Queued {
            motion_sequence, ..
        } => {
            motion_received_at.insert(motion_sequence, received_at);
        }
        crate::core::EnqueueOutcome::Duplicate => {}
        crate::core::EnqueueOutcome::RetiredByCumulativeTarget
        | crate::core::EnqueueOutcome::RetiredByRebase => {
            let mut metrics = lock_metrics(metrics);
            metrics.stale_events_rejected = metrics.stale_events_rejected.saturating_add(1);
        }
    }
    let stats = playout.stats();
    let mut session_metrics = lock_metrics(metrics);
    if let Some(delay) = stats.last_packet_delay {
        session_metrics.observe_packet_delay(u64::try_from(delay.as_micros()).unwrap_or(u64::MAX));
    }
    if let Some(variation) = stats.packet_delay_variation_percentile {
        session_metrics
            .adaptive_delay_variation_percentile_us
            .record(variation.as_secs_f64() * 1_000_000.0);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn poll_playout(
    receiver: &mut Receiver,
    playout: &mut Option<ReceiverPlayout>,
    now: MonotonicTimeMicros,
    channels: &mut InputChannels,
    response_sequence: &mut ControlSequence,
    events: &mpsc::Sender<SessionEvent>,
    session_id: u64,
    peer: &str,
    metrics: &Arc<Mutex<SessionMetrics>>,
    motion_received_at: &mut BTreeMap<MotionSequence, Instant>,
) -> Result<()> {
    let Some(playout) = playout.as_mut() else {
        return Ok(());
    };
    let before = playout.stats();
    let Some(step) = playout.poll(now)? else {
        return Ok(());
    };
    let after = playout.stats();
    let new_scheduler_late = after
        .scheduler_late_count
        .saturating_sub(before.scheduler_late_count);
    let new_catch_up = after
        .catch_up_step_count
        .saturating_sub(before.catch_up_step_count);
    {
        let mut session_metrics = lock_metrics(metrics);
        session_metrics
            .playout_delay_us
            .record(after.current_delay.as_secs_f64() * 1_000_000.0);
        session_metrics.scheduler_late_events = session_metrics
            .scheduler_late_events
            .saturating_add(new_scheduler_late);
        if new_scheduler_late > 0
            && let Some(lateness) = after.last_scheduler_lateness
        {
            session_metrics
                .scheduler_lateness_us
                .record(lateness.as_secs_f64() * 1_000_000.0);
        }
        session_metrics.catch_up_steps =
            session_metrics.catch_up_steps.saturating_add(new_catch_up);
        if new_catch_up > 0 {
            if step.pointer_catch_up_limited {
                session_metrics.catch_up_pointer_units = session_metrics
                    .catch_up_pointer_units
                    .saturating_add(step.delta.dx.unsigned_abs())
                    .saturating_add(step.delta.dy.unsigned_abs());
            }
            if step.scroll_catch_up_limited {
                session_metrics.catch_up_scroll_units = session_metrics
                    .catch_up_scroll_units
                    .saturating_add(step.delta.scroll_x.unsigned_abs())
                    .saturating_add(step.delta.scroll_y.unsigned_abs());
            }
        }
    }
    let received_at = motion_received_at
        .get(&step.through_sequence)
        .copied()
        .unwrap_or_else(Instant::now);
    if step.target_reached {
        motion_received_at.retain(|sequence, _| *sequence > step.through_sequence);
    }
    let session = playout.session();
    let effects = receiver.receive_playout_step(session, step, now)?;
    emit_receiver_effects(
        channels,
        effects,
        response_sequence,
        events,
        session_id,
        peer,
        metrics,
        received_at,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
fn handle_probe(
    channels: &InputChannels,
    probe: ProbeMessage,
    sender: Option<&Sender>,
    receiver_context: Option<SessionContext>,
    pending: &mut BTreeMap<ProbeSequence, MonotonicTimeMicros>,
    clock: &mut ClockMapper,
    playout: &mut Option<ReceiverPlayout>,
    now: MonotonicTimeMicros,
    metrics: &Arc<Mutex<SessionMetrics>>,
) -> Result<()> {
    match probe.payload {
        ProbePayload::Probe { sequence, sent_at }
            if sender.is_some_and(|sender| sender.session() == probe.session) =>
        {
            channels.datagrams.send_probe(&ProbeMessage {
                session: probe.session,
                payload: ProbePayload::ProbeEcho {
                    sequence,
                    probe_sent_at: sent_at,
                    received_at: now,
                    echoed_at: now,
                },
            })?;
        }
        ProbePayload::ProbeEcho {
            sequence,
            probe_sent_at,
            received_at,
            echoed_at,
        } if receiver_context == Some(probe.session)
            && pending.remove(&sequence) == Some(probe_sent_at) =>
        {
            let replacing_arrival_bootstrap = clock.uses_arrival_bootstrap();
            clock.ingest_probe(ProbeExchange {
                receiver_sent_at: probe_sent_at,
                sender_received_at: received_at,
                sender_echoed_at: echoed_at,
                receiver_received_at: now,
            })?;
            if replacing_arrival_bootstrap && let Some(playout) = playout.as_mut() {
                playout.remap_clock(clock)?;
            }
            lock_metrics(metrics)
                .rtt_us
                .record(now.0.saturating_sub(probe_sent_at.0) as f64);
            if let Some(residual) = clock.stats().residual_error_micros {
                lock_metrics(metrics).clock_residual_us.record(residual);
            }
            update_clock_metrics(&mut lock_metrics(metrics), clock);
        }
        _ => {
            let mut metrics = lock_metrics(metrics);
            metrics.stale_events_rejected = metrics.stale_events_rejected.saturating_add(1);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn emit_receiver_effects(
    channels: &mut InputChannels,
    effects: Vec<ReceiverEffect>,
    response_sequence: &mut ControlSequence,
    events: &mpsc::Sender<SessionEvent>,
    session_id: u64,
    peer: &str,
    metrics: &Arc<Mutex<SessionMetrics>>,
    received_at: Instant,
) -> Result<()> {
    let mut backend = Vec::new();
    let mut responses = Vec::new();
    for effect in effects {
        match effect {
            ReceiverEffect::SnapshotAck { session, ack } => {
                responses.push((session, ReliableControl::SnapshotAck(ack)));
            }
            ReceiverEffect::TakeoverAccepted { session, accepted } => {
                responses.push((session, ReliableControl::TakeoverAccepted(accepted)));
            }
            ReceiverEffect::Rejected { reason, .. } => {
                let mut metrics = lock_metrics(metrics);
                metrics.stale_events_rejected = metrics.stale_events_rejected.saturating_add(1);
                if matches!(
                    reason,
                    RejectionReason::ControlGap
                        | RejectionReason::InvalidTransition
                        | RejectionReason::InvalidTakeover
                        | RejectionReason::AnchorActivationMismatch
                        | RejectionReason::AnchorMovedBackwards
                        | RejectionReason::WrongDirection
                ) {
                    bail!("peer sent an invalid input state transition");
                }
            }
            effect => backend.push(effect),
        }
    }
    if !backend.is_empty() {
        let synthetic_releases = backend
            .iter()
            .filter(|effect| is_synthetic_release(effect))
            .count();
        if synthetic_releases > 0 {
            let mut metrics = lock_metrics(metrics);
            metrics.synthetic_releases = metrics
                .synthetic_releases
                .saturating_add(synthetic_releases as u64);
        }
        let (applied, receipt) = oneshot::channel();
        emit_event(
            events,
            SessionEvent {
                session_id,
                peer: peer.to_owned(),
                kind: SessionEventKind::ReceiverEffects {
                    effects: backend,
                    received_at,
                    applied,
                },
            },
        )?;
        receipt
            .await
            .map_err(|_| anyhow!("daemon dropped the receiver backend apply receipt"))?
            .map_err(anyhow::Error::msg)?;
    }
    for (session, payload) in responses {
        response_sequence.0 = response_sequence
            .0
            .checked_add(1)
            .context("control response sequence exhausted")?;
        channels
            .control_send
            .send_control(&ReliableControlMessage {
                session,
                sequence: *response_sequence,
                payload,
            })
            .await?;
    }
    Ok(())
}

async fn close_receiver(
    receiver: &mut Receiver,
    events: &mpsc::Sender<SessionEvent>,
    session_id: u64,
    peer: &str,
    now: MonotonicTimeMicros,
    lifecycle: ReceiverLifecycle,
    metrics: &Arc<Mutex<SessionMetrics>>,
) -> Result<()> {
    let effects = receiver.lifecycle(lifecycle, now)?;
    if !effects.is_empty() {
        let synthetic_releases = effects
            .iter()
            .filter(|effect| is_synthetic_release(effect))
            .count();
        if synthetic_releases > 0 {
            let mut metrics = lock_metrics(metrics);
            metrics.synthetic_releases = metrics
                .synthetic_releases
                .saturating_add(synthetic_releases as u64);
        }
        let (applied, receipt) = oneshot::channel();
        events
            .send(SessionEvent {
                session_id,
                peer: peer.to_owned(),
                kind: SessionEventKind::ReceiverEffects {
                    effects,
                    received_at: Instant::now(),
                    applied,
                },
            })
            .await
            .map_err(|_| anyhow!("daemon session event router stopped during cleanup"))?;
        receipt
            .await
            .map_err(|_| anyhow!("daemon dropped the receiver cleanup apply receipt"))?
            .map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

fn emit_event(sender: &mpsc::Sender<SessionEvent>, event: SessionEvent) -> Result<()> {
    sender
        .try_send(event)
        .map_err(|error| anyhow!("daemon session event queue is unavailable: {error}"))
}

fn is_synthetic_release(effect: &ReceiverEffect) -> bool {
    matches!(
        effect,
        ReceiverEffect::Key {
            pressed: false,
            synthetic: true,
            ..
        } | ReceiverEffect::Button {
            pressed: false,
            synthetic: true,
            ..
        } | ReceiverEffect::Modifier {
            pressed: false,
            synthetic: true,
            ..
        } | ReceiverEffect::ScrollEnded {
            synthetic: true,
            ..
        } | ReceiverEffect::TouchReplaced {
            synthetic: true,
            ..
        }
    )
}

fn update_clock_metrics(metrics: &mut SessionMetrics, clock: &ClockMapper) {
    let stats = clock.stats();
    metrics.clock_offset_us = stats.offset_micros;
    metrics.clock_skew = stats.skew;
    metrics.clock_skew_ppm = stats.skew_ppm;
    metrics.clock_reset_count = stats.reset_count;
}

fn lock_metrics(metrics: &Arc<Mutex<SessionMetrics>>) -> std::sync::MutexGuard<'_, SessionMetrics> {
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

fn add_duration(time: MonotonicTimeMicros, duration: Duration) -> MonotonicTimeMicros {
    MonotonicTimeMicros(
        time.0
            .saturating_add(u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)),
    )
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
        MonotonicTimeMicros(u64::try_from(self.0.elapsed().as_micros()).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use tempfile::TempDir;

    use super::*;
    use crate::{
        capture::CaptureFrame,
        core::{
            ActivationId, AnchorKind, CumulativeMotion, HidUsage, MotionDelta, PointerButton,
            SnapshotAck,
        },
        identity::Identity,
        transport::{
            TransportError, accept_input, connect_input, input_client_config, input_server_config,
        },
    };

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
            protocol_version: CURRENT_PROTOCOL_VERSION,
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
            .authorize_session(context, MonotonicTimeMicros(0))
            .unwrap();
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
                protocol_version: CURRENT_PROTOCOL_VERSION,
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

        assert!(
            validate_negotiated_control(
                &ReliableControl::ScrollBegin {
                    scroll: ActiveScroll {
                        id: crate::core::ScrollId(1),
                        source: crate::core::ScrollSource {
                            unit: ScrollUnit::Device,
                            resolution_x: None,
                            resolution_y: None,
                        },
                        phase: crate::core::ScrollPhase::Begin,
                        momentum_phase: None,
                    },
                },
                &selected,
            )
            .is_err()
        );
    }

    #[test]
    fn capture_merge_keeps_overlapping_devices_held_until_the_last_release() {
        let key = HidUsage::keyboard(0xe0);
        let mut merge = CaptureMerger::default();
        let frame = |device: &str, state| CapturedDeviceFrame {
            device_path: device.into(),
            frame: CaptureFrame {
                transitions: vec![CaptureTransition::Key { usage: key, state }],
                motion: MotionDelta::default(),
                touch_snapshot: None,
                event_count: 1,
            },
            captured_at: Instant::now(),
        };

        assert_eq!(
            merge
                .merge(frame("/dev/input/one", KeyState::Pressed))
                .frame
                .transitions
                .len(),
            1
        );
        assert!(
            merge
                .merge(frame("/dev/input/two", KeyState::Pressed))
                .frame
                .transitions
                .is_empty()
        );
        assert!(
            merge
                .merge(frame("/dev/input/one", KeyState::Released))
                .frame
                .transitions
                .is_empty()
        );
        assert_eq!(
            merge
                .merge(frame("/dev/input/two", KeyState::Released))
                .frame
                .transitions
                .len(),
            1
        );
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
            let mut response_sequence = ControlSequence(0);
            emit_receiver_effects(
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
                &mut response_sequence,
                &event_tx,
                1,
                "peer",
                &metrics,
                Instant::now(),
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
            protocol_version: CURRENT_PROTOCOL_VERSION,
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

    #[test]
    fn protocol_version_is_current() {
        assert_eq!(CURRENT_PROTOCOL_VERSION, ProtocolVersion(1));
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
        ) -> (SessionHandle, oneshot::Receiver<Result<(), String>>) {
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
            };
            tokio::spawn(async move {
                let _ = run_session(
                    id,
                    "test".into(),
                    channels,
                    command_rx,
                    options(),
                    events,
                    metrics,
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
}
