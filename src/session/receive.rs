//! The receive side of one input session: control ordering, playout, clock
//! probes, and the handoff of effects to the input backend.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::oneshot;

use super::{MonotonicClock, Reporter, SessionEventKind, lock_metrics};
use crate::{
    core::{
        ClockConfig, ClockMapper, ControlSequence, MonotonicTimeMicros, MotionSequence,
        PlayoutConfig, ProbeExchange, ProbeMessage, ProbePayload, ProbeSequence, Receiver,
        ReceiverConfig, ReceiverEffect, ReceiverPlayout, RejectionReason, ReliableControl,
        ReliableControlMessage, Sender, SessionContext,
    },
    metrics::SessionMetrics,
    transport::InputChannels,
};

const SESSION_TICK: Duration = Duration::from_millis(1);
const PROBE_INTERVAL: Duration = Duration::from_millis(100);
const PROBE_MAX_AGE: Duration = Duration::from_secs(2);
const MAX_PENDING_PROBES: usize = 64;

pub(super) struct PendingControl {
    message: ReliableControlMessage,
    received_at: Instant,
    receiver_received_at: MonotonicTimeMicros,
}

pub(super) struct Inbound {
    reporter: Reporter,
    playout_config: PlayoutConfig,
    receiver: Receiver,
    clock: ClockMapper,
    playout: Option<ReceiverPlayout>,
    pending: VecDeque<PendingControl>,
    motion_received_at: BTreeMap<MotionSequence, Instant>,
    response_sequence: ControlSequence,
    probes: BTreeMap<ProbeSequence, MonotonicTimeMicros>,
    next_probe_sequence: ProbeSequence,
    probe_context: Option<SessionContext>,
    next_probe_at: MonotonicTimeMicros,
}

impl Inbound {
    pub(super) fn new(
        reporter: Reporter,
        config: ReceiverConfig,
        playout_config: PlayoutConfig,
        now: MonotonicTimeMicros,
    ) -> Result<Self> {
        Ok(Self {
            reporter,
            playout_config,
            receiver: Receiver::new(config, now)?,
            clock: ClockMapper::new(ClockConfig::default())?,
            playout: None,
            pending: VecDeque::new(),
            motion_received_at: BTreeMap::new(),
            response_sequence: ControlSequence(0),
            probes: BTreeMap::new(),
            next_probe_sequence: ProbeSequence(1),
            probe_context: None,
            next_probe_at: now.saturating_add(PROBE_INTERVAL),
        })
    }

    pub(super) fn active_context(&self) -> Option<SessionContext> {
        self.receiver.active_context()
    }

    pub(super) fn has_pending_controls(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Restarts probing whenever the open activation changes.
    pub(super) fn track_activation(&mut self, clock: &MonotonicClock) {
        let active_context = self.receiver.active_context();
        if self.probe_context != active_context {
            self.probes.clear();
            self.probe_context = active_context;
            self.next_probe_at = clock.now().saturating_add(PROBE_INTERVAL);
        }
    }

    pub(super) fn deadline(
        &self,
        sender: Option<&Sender>,
        last_tick_at: MonotonicTimeMicros,
    ) -> Option<MonotonicTimeMicros> {
        session_deadline(
            sender,
            &self.receiver,
            self.playout.as_ref(),
            !self.pending.is_empty(),
            last_tick_at,
            self.next_probe_at,
        )
    }

    pub(super) async fn receive_control(
        &mut self,
        channels: &mut InputChannels,
        message: ReliableControlMessage,
        received_at: Instant,
        now: MonotonicTimeMicros,
    ) -> Result<()> {
        enqueue_pending_control(
            &mut self.pending,
            message,
            received_at,
            now,
            self.playout_config.maximum_queued_frames,
        )?;
        self.drain_pending_controls(channels, now).await
    }

    pub(super) fn receive_motion(
        &mut self,
        frame: crate::core::MotionFrame,
        received_at: Instant,
        now: MonotonicTimeMicros,
    ) -> Result<()> {
        let Some(playout) = self.playout.as_mut() else {
            self.reporter.count_stale();
            return Ok(());
        };
        if playout.session() != frame.session {
            self.reporter.count_stale();
            return Ok(());
        }
        self.reporter
            .metrics()
            .observe_motion_sequence(frame.motion_sequence.0);
        // One sample gives playout a bounded bootstrap mapping. Further arrival
        // times contain network jitter and must not train the affine clock model;
        // authenticated probe exchanges replace the bootstrap fit.
        if !self.clock.is_ready() {
            self.clock
                .bootstrap_from_arrival(frame.sender_capture_time, now)?;
            update_clock_metrics(&mut self.reporter.metrics(), &self.clock);
        }
        match playout.ingest_frame(frame, now, &self.clock)? {
            crate::core::EnqueueOutcome::Queued { motion_sequence } => {
                self.motion_received_at.insert(motion_sequence, received_at);
            }
            crate::core::EnqueueOutcome::Duplicate => {}
            crate::core::EnqueueOutcome::RetiredByCumulativeTarget
            | crate::core::EnqueueOutcome::RetiredByRebase => self.reporter.count_stale(),
        }
        let stats = playout.stats();
        let mut session_metrics = self.reporter.metrics();
        if let Some(delay) = stats.last_packet_delay {
            session_metrics
                .observe_packet_delay(u64::try_from(delay.as_micros()).unwrap_or(u64::MAX));
        }
        if let Some(variation) = stats.packet_delay_variation_percentile {
            session_metrics
                .adaptive_delay_variation_percentile_us
                .record(variation.as_secs_f64() * 1_000_000.0);
        }
        Ok(())
    }

    /// Echoes a probe for our outbound activation, or feeds an echo of our
    /// own probe to the clock model.
    pub(super) fn receive_probe(
        &mut self,
        channels: &InputChannels,
        probe: ProbeMessage,
        sender: Option<&Sender>,
        now: MonotonicTimeMicros,
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
            } if self.receiver.active_context() == Some(probe.session)
                && self.probes.remove(&sequence) == Some(probe_sent_at) =>
            {
                let replacing_arrival_bootstrap = self.clock.uses_arrival_bootstrap();
                self.clock.ingest_probe(ProbeExchange {
                    receiver_sent_at: probe_sent_at,
                    sender_received_at: received_at,
                    sender_echoed_at: echoed_at,
                    receiver_received_at: now,
                })?;
                if replacing_arrival_bootstrap && let Some(playout) = self.playout.as_mut() {
                    playout.remap_clock(&self.clock)?;
                }
                self.reporter
                    .metrics()
                    .rtt_us
                    .record(now.0.saturating_sub(probe_sent_at.0) as f64);
                if let Some(residual) = self.clock.stats().residual_error_micros {
                    self.reporter.metrics().clock_residual_us.record(residual);
                }
                update_clock_metrics(&mut self.reporter.metrics(), &self.clock);
            }
            _ => self.reporter.count_stale(),
        }
        Ok(())
    }

    /// Expires the lease, plays due motion, applies due controls, and sends
    /// the next clock probe.
    pub(super) async fn tick(
        &mut self,
        channels: &mut InputChannels,
        clock: &MonotonicClock,
        now: MonotonicTimeMicros,
    ) -> Result<()> {
        let active_before_tick = self.receiver.active_context();
        let effects = self.receiver.tick(now)?;
        self.emit(channels, effects, Instant::now(), None).await?;
        if let Some(closed) = active_before_tick
            && self.receiver.active_context() != Some(closed)
        {
            discard_pending_context(&mut self.pending, closed);
        }
        self.poll_playout(channels, now).await?;
        self.drain_pending_controls(channels, now).await?;

        let active_context = self.receiver.active_context();
        let oldest_probe = now
            .0
            .saturating_sub(u64::try_from(PROBE_MAX_AGE.as_micros()).unwrap_or(u64::MAX));
        self.probes.retain(|_, sent_at| sent_at.0 >= oldest_probe);
        if now >= self.next_probe_at
            && self.probes.len() < MAX_PENDING_PROBES
            && let Some(context) = active_context
        {
            // The backend awaits above can take a while. A stale stamp
            // would inflate RTT and bias the clock offset.
            let sent_at = clock.now();
            let probe = ProbeMessage {
                session: context,
                payload: ProbePayload::Probe {
                    sequence: self.next_probe_sequence,
                    sent_at,
                },
            };
            channels.datagrams.send_probe(&probe)?;
            self.probes.insert(self.next_probe_sequence, sent_at);
            self.next_probe_sequence = ProbeSequence(
                self.next_probe_sequence
                    .0
                    .checked_add(1)
                    .context("probe sequence exhausted")?,
            );
        }
        if now >= self.next_probe_at {
            self.next_probe_at = now.saturating_add(PROBE_INTERVAL);
        }
        Ok(())
    }

    /// Releases everything the closing connection held.
    pub(super) async fn close(
        &mut self,
        channels: &mut InputChannels,
        now: MonotonicTimeMicros,
    ) -> Result<()> {
        let effects = self.receiver.connection_lost(now)?;
        self.emit(channels, effects, Instant::now(), None).await
    }

    async fn drain_pending_controls(
        &mut self,
        channels: &mut InputChannels,
        now: MonotonicTimeMicros,
    ) -> Result<()> {
        while let Some(front) = self.pending.front() {
            if !pending_control_is_ready(
                front,
                &self.receiver,
                &self.playout,
                &mut self.clock,
                now,
                &self.reporter.metrics,
            )? {
                break;
            }
            let item = self
                .pending
                .pop_front()
                .expect("ready reliable control came from the queue");
            let active_before = self.receiver.active_context();
            self.apply_control(channels, item.message, now, item.received_at)
                .await?;
            if let Some(previous) = active_before
                && self.receiver.active_context() != Some(previous)
            {
                discard_pending_context(&mut self.pending, previous);
            }
        }
        Ok(())
    }

    async fn apply_control(
        &mut self,
        channels: &mut InputChannels,
        message: ReliableControlMessage,
        now: MonotonicTimeMicros,
        received_at: Instant,
    ) -> Result<()> {
        let anchor = message.payload.motion_anchor().cloned();
        let touch_captured_at = anchor
            .as_ref()
            .filter(|anchor| !anchor.final_touch_state.is_empty() && self.clock.is_ready())
            .map(|anchor| self.clock.map(anchor.sender_capture_time))
            .transpose()?
            .map(|capture| capture_instant(capture, now));
        let sequence = message.sequence;
        let effects = self.receiver.receive_control(message, now)?;
        let rejected = effects
            .iter()
            .any(|effect| matches!(effect, ReceiverEffect::Rejected { .. }));
        if !rejected {
            if effects
                .iter()
                .any(|effect| matches!(effect, ReceiverEffect::ActivationOpened(_)))
            {
                self.reporter.metrics().begin_activation();
                // Motion sequences restart with each activation.
                self.motion_received_at.clear();
                // Probes only run during an activation, so an old fit may have
                // drifted, or missed a Mac sleep, while the session sat idle.
                self.clock = ClockMapper::new(ClockConfig::default())?;
                self.playout = Some(ReceiverPlayout::new(
                    self.playout_config,
                    self.receiver.active_context().expect("activation opened"),
                )?);
            }
            if let Some(active) = self.playout.as_mut() {
                active.advance_control_watermark(sequence)?;
                if let Some(anchor) = anchor {
                    // The receiver injects whatever the anchor adds, so this only
                    // moves playout's baseline. Nothing is discarded.
                    active.rebase(anchor.through_motion_sequence, anchor.totals)?;
                    self.motion_received_at
                        .retain(|sequence, _| *sequence > anchor.through_motion_sequence);
                }
            }
        }
        self.emit(channels, effects, received_at, touch_captured_at)
            .await
    }

    async fn poll_playout(
        &mut self,
        channels: &mut InputChannels,
        now: MonotonicTimeMicros,
    ) -> Result<()> {
        let Some(playout) = self.playout.as_mut() else {
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
            let mut session_metrics = self.reporter.metrics();
            session_metrics
                .playout_delay_us
                .record(after.current_delay.as_secs_f64() * 1_000_000.0);
            session_metrics.scheduler_late_events = session_metrics
                .scheduler_late_events
                .saturating_add(new_scheduler_late);
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
        let received_at = self
            .motion_received_at
            .get(&step.through_sequence)
            .copied()
            .unwrap_or_else(Instant::now);
        if step.target_reached {
            self.motion_received_at
                .retain(|sequence, _| *sequence > step.through_sequence);
        }
        let touch_captured_at = step
            .touch_snapshot
            .as_ref()
            .map(|_| capture_instant(step.mapped_capture_time, now));
        let session = playout.session();
        let effects = self.receiver.receive_playout_step(session, step, now)?;
        self.emit(channels, effects, received_at, touch_captured_at)
            .await
    }

    /// Hands injections to the backend, waits until they are applied, and
    /// only then answers the peer.
    pub(super) async fn emit(
        &mut self,
        channels: &mut InputChannels,
        effects: Vec<ReceiverEffect>,
        received_at: Instant,
        touch_captured_at: Option<Instant>,
    ) -> Result<()> {
        let mut backend = Vec::new();
        let mut responses = Vec::new();
        for effect in effects {
            match effect {
                ReceiverEffect::SnapshotAck { session, ack } => {
                    responses.push((session, ReliableControl::SnapshotAck(ack)));
                }
                ReceiverEffect::Rejected { reason, .. } => {
                    self.reporter.count_stale();
                    if matches!(
                        reason,
                        RejectionReason::ControlGap
                            | RejectionReason::InvalidTransition
                            | RejectionReason::AnchorActivationMismatch
                            | RejectionReason::AnchorMovedBackwards
                            | RejectionReason::WrongDirection
                    ) {
                        bail!("peer sent an invalid input state transition");
                    }
                }
                effect if !backend_supports(&effect) => {
                    let mut metrics = self.reporter.metrics();
                    metrics.unsupported_inputs_dropped =
                        metrics.unsupported_inputs_dropped.saturating_add(1);
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
                let mut metrics = self.reporter.metrics();
                metrics.synthetic_releases = metrics
                    .synthetic_releases
                    .saturating_add(synthetic_releases as u64);
            }
            let (applied, receipt) = oneshot::channel();
            // Wait for queue space: this also runs during cleanup, where
            // dropping synthetic releases would leave keys held.
            self.reporter
                .send(SessionEventKind::ReceiverEffects {
                    effects: backend,
                    touch_captured_at,
                    received_at,
                    applied,
                })
                .await?;
            receipt
                .await
                .map_err(|_| anyhow!("daemon dropped the receiver backend apply receipt"))?
                .map_err(anyhow::Error::msg)?;
        }
        for (session, payload) in responses {
            self.response_sequence.0 = self
                .response_sequence
                .0
                .checked_add(1)
                .context("control response sequence exhausted")?;
            channels
                .control_send
                .send_control(&ReliableControlMessage {
                    session,
                    sequence: self.response_sequence,
                    payload,
                })
                .await?;
        }
        Ok(())
    }
}

// Tick only when an engine needs progress. Catch-up and deferred controls keep
// their 1ms cadence; quiet connections sleep until a checkpoint, lease, or probe.
pub(super) fn session_deadline(
    sender: Option<&Sender>,
    receiver: &Receiver,
    playout: Option<&ReceiverPlayout>,
    pending_controls: bool,
    last_tick_at: MonotonicTimeMicros,
    next_probe_at: MonotonicTimeMicros,
) -> Option<MonotonicTimeMicros> {
    let catch_up = playout.is_some_and(|playout| {
        let stats = playout.stats();
        stats.selected_target_sequence > stats.completed_sequence
    });
    [
        sender.and_then(Sender::next_deadline),
        receiver.lease_deadline(),
        playout.and_then(ReceiverPlayout::next_deadline),
        (catch_up || pending_controls).then(|| last_tick_at.saturating_add(SESSION_TICK)),
        receiver.active_context().map(|_| next_probe_at),
    ]
    .into_iter()
    .flatten()
    .min()
}

pub(super) fn enqueue_pending_control(
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

pub(super) fn discard_pending_context(
    pending: &mut VecDeque<PendingControl>,
    context: SessionContext,
) {
    pending.retain(|item| item.message.session != context);
}

pub(super) fn pending_control_is_ready(
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
    if pending.message.session != active {
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
    Ok(mapped_capture_time.saturating_add(delay) <= now)
}

fn capture_instant(capture: MonotonicTimeMicros, now: MonotonicTimeMicros) -> Instant {
    let instant = Instant::now();
    instant
        .checked_sub(Duration::from_micros(now.0.saturating_sub(capture.0)))
        .unwrap_or(instant)
}

/// The receiver keeps tracking keys and buttons the local backend cannot
/// inject, so held state and control sequences stay in step with the sender.
/// Only their injection is dropped.
#[cfg(target_os = "linux")]
fn backend_supports(effect: &ReceiverEffect) -> bool {
    match effect {
        ReceiverEffect::Key { key, .. } => crate::linux::hid_to_evdev_key(*key).is_ok(),
        ReceiverEffect::Button { button, .. } => {
            crate::linux::pointer_button_to_evdev(*button).is_ok()
        }
        _ => true,
    }
}

/// The Mac posts only the keys and buttons it has codes for. Touch still
/// reaches its injector, which ignores it.
#[cfg(target_os = "macos")]
fn backend_supports(effect: &ReceiverEffect) -> bool {
    match effect {
        ReceiverEffect::Key { key, .. } => crate::macos::supports_key(*key),
        ReceiverEffect::Button { button, .. } => (1..=32).contains(&button.0),
        _ => true,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn backend_supports(_: &ReceiverEffect) -> bool {
    true
}

pub(super) fn is_synthetic_release(effect: &ReceiverEffect) -> bool {
    match effect {
        ReceiverEffect::Key {
            pressed: false,
            synthetic: true,
            ..
        }
        | ReceiverEffect::Button {
            pressed: false,
            synthetic: true,
            ..
        } => true,
        // A synthetic replacement can carry contacts; only an empty one lifts.
        ReceiverEffect::TouchReplaced {
            state,
            synthetic: true,
        } => state.is_empty(),
        _ => false,
    }
}

fn update_clock_metrics(metrics: &mut SessionMetrics, clock: &ClockMapper) {
    let stats = clock.stats();
    metrics.clock_offset_us = stats.offset_micros;
    metrics.clock_skew = stats.skew;
    metrics.clock_skew_ppm = stats.skew_ppm;
}
