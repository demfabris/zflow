//! Pure sender-side capture and checkpoint state machine.
//!
//! The caller owns clocks and I/O. Every method either returns a logical wire
//! message or a deadline; nothing in this module opens a socket or touches an
//! input device.

use std::{collections::BTreeMap, time::Duration};

use thiserror::Error;

use super::{
    ActiveScroll, AnchorKind, ControlSequence, CumulativeMotion, HeldState, HidUsage, Modifier,
    MonotonicTimeMicros, MotionAnchor, MotionDelta, MotionFrame, MotionOverflow, MotionSequence,
    PointerButton, ReliableControl, ReliableControlMessage, ScrollId, SessionCloseReason,
    SessionContext, SessionTakeover, SnapshotAck, StateSnapshot, TakeoverAccepted, TakeoverNonce,
    TouchState, TransportGeneration,
};

const MAX_CHECKPOINT_INTERVAL: Duration = Duration::from_millis(250);
const MAX_RECEIVER_LEASE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderConfig {
    pub checkpoint_interval: Duration,
    pub receiver_lease: Duration,
}

impl SenderConfig {
    pub fn new(
        checkpoint_interval: Duration,
        receiver_lease: Duration,
    ) -> Result<Self, SenderError> {
        let config = Self {
            checkpoint_interval,
            receiver_lease,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(self) -> Result<(), SenderError> {
        if self.checkpoint_interval.is_zero() || self.checkpoint_interval > MAX_CHECKPOINT_INTERVAL
        {
            return Err(SenderError::InvalidCheckpointInterval);
        }
        if self.receiver_lease.is_zero() || self.receiver_lease > MAX_RECEIVER_LEASE {
            return Err(SenderError::InvalidReceiverLease);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenderExitReason {
    LocalRelease,
    SnapshotAckTimedOut,
    MotionOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SenderTick {
    Idle,
    Checkpoint(Box<ReliableControlMessage>),
    ExitRemote(SenderExitReason),
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum SenderError {
    #[error("checkpoint interval must be in 1..=250 ms")]
    InvalidCheckpointInterval,
    #[error("receiver lease must be in 1..=1000 ms")]
    InvalidReceiverLease,
    #[error("the activation is not remote")]
    NotRemote,
    #[error("the activation is already remote")]
    AlreadyRemote,
    #[error("a closed activation cannot be reopened")]
    ClosedActivation,
    #[error("capture requested an invalid input transition")]
    InvalidTransition,
    #[error("sender monotonic time moved backwards")]
    ClockMovedBackwards,
    #[error("control or motion sequence exhausted")]
    SequenceExhausted,
    #[error("snapshot acknowledgement came from the wrong generation")]
    WrongAckGeneration,
    #[error("snapshot acknowledgement does not name an emitted snapshot")]
    UnknownSnapshotAck,
    #[error("transport generation must advance exactly by one")]
    InvalidGenerationAdvance,
    #[error("takeover acknowledgement did not match the pending proposal")]
    UnexpectedTakeoverAcceptance,
    #[error("capture is paused until the pending takeover is accepted or abandoned")]
    TakeoverPending,
    #[error(transparent)]
    MotionOverflow(#[from] MotionOverflow),
}

#[derive(Debug, Clone)]
struct PendingSnapshot {
    snapshot: StateSnapshot,
    /// Only held-state renewals can force the sender out of Remote.
    ack_deadline: Option<MonotonicTimeMicros>,
}

#[derive(Debug, Clone, Copy)]
struct PendingTakeover {
    generation: TransportGeneration,
    nonce: TakeoverNonce,
    proposed_at: MonotonicTimeMicros,
}

/// Sender state for one activation.
#[derive(Debug, Clone)]
pub struct Sender {
    config: SenderConfig,
    session: SessionContext,
    started: bool,
    remote: bool,
    held: HeldState,
    totals: CumulativeMotion,
    last_control_sequence: ControlSequence,
    last_motion_sequence: MotionSequence,
    last_observed_time: MonotonicTimeMicros,
    last_snapshot_at: MonotonicTimeMicros,
    dirty_since: Option<MonotonicTimeMicros>,
    pending_snapshots: BTreeMap<ControlSequence, PendingSnapshot>,
    last_acknowledged: Option<(ControlSequence, StateSnapshot)>,
    pending_takeover: Option<PendingTakeover>,
}

impl Sender {
    pub fn new(
        config: SenderConfig,
        session: SessionContext,
        now: MonotonicTimeMicros,
    ) -> Result<Self, SenderError> {
        config.validate()?;
        Ok(Self {
            config,
            session,
            started: false,
            remote: false,
            held: HeldState::default(),
            totals: CumulativeMotion::ZERO,
            last_control_sequence: ControlSequence(0),
            last_motion_sequence: MotionSequence(0),
            last_observed_time: now,
            last_snapshot_at: now,
            dirty_since: None,
            pending_snapshots: BTreeMap::new(),
            last_acknowledged: None,
            pending_takeover: None,
        })
    }

    pub fn session(&self) -> SessionContext {
        self.session
    }

    pub fn is_remote(&self) -> bool {
        self.remote
    }

    pub fn held_state(&self) -> &HeldState {
        &self.held
    }

    pub fn totals(&self) -> CumulativeMotion {
        self.totals
    }

    pub fn last_motion_sequence(&self) -> MotionSequence {
        self.last_motion_sequence
    }

    pub fn last_control_sequence(&self) -> ControlSequence {
        self.last_control_sequence
    }

    pub fn last_acknowledged_checkpoint(&self) -> Option<(ControlSequence, &StateSnapshot)> {
        self.last_acknowledged
            .as_ref()
            .map(|(sequence, snapshot)| (*sequence, snapshot))
    }

    pub fn enter(
        &mut self,
        now: MonotonicTimeMicros,
    ) -> Result<ReliableControlMessage, SenderError> {
        self.observe_time(now)?;
        if self.remote {
            return Err(SenderError::AlreadyRemote);
        }
        if self.started {
            return Err(SenderError::ClosedActivation);
        }
        self.started = true;
        self.remote = true;
        self.held.release_all();
        self.totals = CumulativeMotion::ZERO;
        self.last_motion_sequence = MotionSequence(0);
        self.last_snapshot_at = now;
        self.dirty_since = None;
        self.pending_snapshots.clear();
        self.control(ReliableControl::Enter)
    }

    pub fn capture_motion(
        &mut self,
        delta: MotionDelta,
        touch_snapshot: Option<TouchState>,
        now: MonotonicTimeMicros,
    ) -> Result<MotionFrame, SenderError> {
        self.require_capture_ready()?;
        self.observe_time(now)?;
        let next_totals = match self.totals.checked_add(delta) {
            Ok(totals) => totals,
            Err(error) => {
                self.remote = false;
                self.held.release_all();
                return Err(SenderError::MotionOverflow(error));
            }
        };
        let next_sequence = self.next_motion_sequence()?;

        self.totals = next_totals;
        self.last_motion_sequence = next_sequence;
        if let Some(touch) = &touch_snapshot {
            self.held.replace_touch(touch.clone());
        }
        if delta != MotionDelta::default() || touch_snapshot.is_some() {
            self.dirty_since.get_or_insert(now);
        }

        Ok(MotionFrame {
            session: self.session,
            motion_sequence: next_sequence,
            control_watermark: self.last_control_sequence,
            sender_capture_time: now,
            totals: self.totals,
            touch_snapshot,
        })
    }

    pub fn key_down(
        &mut self,
        key: HidUsage,
        now: MonotonicTimeMicros,
    ) -> Result<ReliableControlMessage, SenderError> {
        self.before_control(now)?;
        if !self.held.press_key(key) {
            return Err(SenderError::InvalidTransition);
        }
        self.control(ReliableControl::KeyDown { key })
    }

    pub fn key_up(
        &mut self,
        key: HidUsage,
        now: MonotonicTimeMicros,
    ) -> Result<ReliableControlMessage, SenderError> {
        self.before_control(now)?;
        if !self.held.release_key(key) {
            return Err(SenderError::InvalidTransition);
        }
        self.control(ReliableControl::KeyUp { key })
    }

    pub fn set_modifier(
        &mut self,
        modifier: Modifier,
        held: bool,
        now: MonotonicTimeMicros,
    ) -> Result<ReliableControlMessage, SenderError> {
        self.before_control(now)?;
        self.held.set_modifier(modifier, held);
        self.snapshot(now)
    }

    pub fn button_down(
        &mut self,
        button: PointerButton,
        now: MonotonicTimeMicros,
    ) -> Result<ReliableControlMessage, SenderError> {
        self.before_control(now)?;
        let anchor = self.anchor(now, AnchorKind::Checkpoint);
        if !self.held.press_button(button) {
            return Err(SenderError::InvalidTransition);
        }
        self.control(ReliableControl::ButtonDown { button, anchor })
    }

    pub fn button_up(
        &mut self,
        button: PointerButton,
        now: MonotonicTimeMicros,
    ) -> Result<ReliableControlMessage, SenderError> {
        self.before_control(now)?;
        let anchor = self.anchor(now, AnchorKind::Checkpoint);
        if !self.held.release_button(button) {
            return Err(SenderError::InvalidTransition);
        }
        self.control(ReliableControl::ButtonUp { button, anchor })
    }

    pub fn scroll_begin(
        &mut self,
        scroll: ActiveScroll,
        now: MonotonicTimeMicros,
    ) -> Result<ReliableControlMessage, SenderError> {
        self.before_control(now)?;
        if self.held.active_scroll.is_some() {
            return Err(SenderError::InvalidTransition);
        }
        self.held.begin_scroll(scroll);
        self.control(ReliableControl::ScrollBegin { scroll })
    }

    pub fn scroll_end(
        &mut self,
        scroll_id: ScrollId,
        cancelled: bool,
        now: MonotonicTimeMicros,
    ) -> Result<ReliableControlMessage, SenderError> {
        self.before_control(now)?;
        let anchor = self.anchor(now, AnchorKind::Checkpoint);
        if !self.held.end_scroll(scroll_id) {
            return Err(SenderError::InvalidTransition);
        }
        let payload = if cancelled {
            ReliableControl::ScrollCancel { scroll_id, anchor }
        } else {
            ReliableControl::ScrollEnd { scroll_id, anchor }
        };
        self.control(payload)
    }

    pub fn touch_begin(
        &mut self,
        initial_state: TouchState,
        now: MonotonicTimeMicros,
    ) -> Result<ReliableControlMessage, SenderError> {
        self.before_control(now)?;
        if !self.held.active_touch.is_empty() || initial_state.is_empty() {
            return Err(SenderError::InvalidTransition);
        }
        self.held.replace_touch(initial_state.clone());
        self.control(ReliableControl::TouchBegin { initial_state })
    }

    pub fn touch_end(
        &mut self,
        cancelled: bool,
        now: MonotonicTimeMicros,
    ) -> Result<ReliableControlMessage, SenderError> {
        self.before_control(now)?;
        if self.held.active_touch.is_empty() {
            return Err(SenderError::InvalidTransition);
        }
        let anchor = self.anchor(now, AnchorKind::Checkpoint);
        self.held.clear_touch();
        self.control(if cancelled {
            ReliableControl::TouchCancel { anchor }
        } else {
            ReliableControl::TouchEnd { anchor }
        })
    }

    pub fn snapshot(
        &mut self,
        now: MonotonicTimeMicros,
    ) -> Result<ReliableControlMessage, SenderError> {
        self.require_capture_ready()?;
        self.observe_time(now)?;
        let snapshot = StateSnapshot {
            held: self.held.clone(),
            motion_anchor: self.anchor(now, AnchorKind::Checkpoint),
        };
        let message = self.control(ReliableControl::StateSnapshot(snapshot.clone()))?;
        let ack_deadline =
            (!snapshot.held.is_neutral()).then(|| add_duration(now, self.config.receiver_lease));
        self.pending_snapshots
            .retain(|_, pending| pending.ack_deadline.is_some());
        self.pending_snapshots.insert(
            message.sequence,
            PendingSnapshot {
                snapshot,
                ack_deadline,
            },
        );
        self.last_snapshot_at = now;
        self.dirty_since = None;
        Ok(message)
    }

    /// Returns the next instant at which the caller must tick the engine.
    pub fn next_deadline(&self) -> Option<MonotonicTimeMicros> {
        if !self.remote {
            return None;
        }
        let periodic = add_duration(self.last_snapshot_at, self.config.checkpoint_interval);
        let dirty = self
            .dirty_since
            .map(|since| add_duration(since, self.config.checkpoint_interval));
        let renewal = (!self.held.is_neutral()).then(|| {
            add_duration(
                self.last_snapshot_at,
                one_third_rounded_down(self.config.receiver_lease),
            )
        });
        let ack_timeout = self
            .pending_snapshots
            .values()
            .filter_map(|pending| pending.ack_deadline)
            .min();
        [Some(periodic), dirty, renewal, ack_timeout]
            .into_iter()
            .flatten()
            .min()
    }

    pub fn tick(&mut self, now: MonotonicTimeMicros) -> Result<SenderTick, SenderError> {
        self.require_remote()?;
        self.observe_time(now)?;
        if self
            .pending_snapshots
            .values()
            .filter_map(|pending| pending.ack_deadline)
            .any(|deadline| deadline <= now)
        {
            self.remote = false;
            self.held.release_all();
            return Ok(SenderTick::ExitRemote(
                SenderExitReason::SnapshotAckTimedOut,
            ));
        }

        let periodic_due =
            add_duration(self.last_snapshot_at, self.config.checkpoint_interval) <= now;
        let dirty_due = self
            .dirty_since
            .is_some_and(|since| add_duration(since, self.config.checkpoint_interval) <= now);
        let renewal_due = !self.held.is_neutral()
            && add_duration(
                self.last_snapshot_at,
                one_third_rounded_down(self.config.receiver_lease),
            ) <= now;
        if periodic_due || dirty_due || renewal_due {
            return self.snapshot(now).map(Box::new).map(SenderTick::Checkpoint);
        }
        Ok(SenderTick::Idle)
    }

    pub fn acknowledge_snapshot(&mut self, ack: SnapshotAck) -> Result<(), SenderError> {
        if ack.accepted_generation != self.session.transport_generation {
            return Err(SenderError::WrongAckGeneration);
        }
        let Some(pending) = self.pending_snapshots.get(&ack.snapshot_sequence) else {
            if self
                .last_acknowledged
                .as_ref()
                .is_some_and(|(sequence, _)| *sequence >= ack.snapshot_sequence)
            {
                return Ok(());
            }
            return Err(SenderError::UnknownSnapshotAck);
        };
        self.last_acknowledged = Some((ack.snapshot_sequence, pending.snapshot.clone()));
        // The reliable stream is ordered. Acknowledging a later snapshot proves
        // every earlier snapshot on that stream was applied too.
        self.pending_snapshots
            .retain(|sequence, _| *sequence > ack.snapshot_sequence);
        Ok(())
    }

    pub fn propose_takeover(
        &mut self,
        generation: TransportGeneration,
        nonce: TakeoverNonce,
        now: MonotonicTimeMicros,
    ) -> Result<ReliableControlMessage, SenderError> {
        self.before_control(now)?;
        if self.session.transport_generation.0.checked_add(1) != Some(generation.0)
            || self.pending_takeover.is_some()
        {
            return Err(SenderError::InvalidGenerationAdvance);
        }
        let prior_control_sequence = self.last_control_sequence;
        let sequence = self.next_control_sequence()?;
        self.last_control_sequence = sequence;
        self.pending_takeover = Some(PendingTakeover {
            generation,
            nonce,
            proposed_at: now,
        });
        Ok(ReliableControlMessage {
            session: SessionContext {
                transport_generation: generation,
                ..self.session
            },
            sequence,
            payload: ReliableControl::SessionTakeover(SessionTakeover {
                prior_generation: self.session.transport_generation,
                proposed_generation: generation,
                proposal_nonce: nonce,
                last_control_sequence: prior_control_sequence,
                final_motion_anchor: self.anchor(now, AnchorKind::Checkpoint),
                authoritative_held_state: self.held.clone(),
            }),
        })
    }

    pub fn accept_takeover(&mut self, accepted: TakeoverAccepted) -> Result<(), SenderError> {
        let Some(pending) = self.pending_takeover else {
            return Err(SenderError::UnexpectedTakeoverAcceptance);
        };
        if pending.generation != accepted.accepted_generation
            || pending.nonce != accepted.proposal_nonce
            || u128::from(accepted.receiver_lease_ms) * 1_000
                != self.config.receiver_lease.as_micros()
        {
            return Err(SenderError::UnexpectedTakeoverAcceptance);
        }
        self.session.transport_generation = accepted.accepted_generation;
        self.last_snapshot_at = pending.proposed_at;
        self.dirty_since = None;
        self.pending_takeover = None;
        self.pending_snapshots.clear();
        Ok(())
    }

    pub fn leave(
        &mut self,
        reason: SessionCloseReason,
        now: MonotonicTimeMicros,
    ) -> Result<ReliableControlMessage, SenderError> {
        self.before_control(now)?;
        let final_anchor = self.anchor(now, AnchorKind::Terminal);
        let message = self.control(ReliableControl::SessionClose {
            reason,
            final_anchor: Some(final_anchor),
        })?;
        self.remote = false;
        self.held.release_all();
        self.pending_snapshots.clear();
        self.pending_takeover = None;
        Ok(message)
    }

    fn before_control(&mut self, now: MonotonicTimeMicros) -> Result<(), SenderError> {
        self.require_capture_ready()?;
        self.observe_time(now)
    }

    fn control(&mut self, payload: ReliableControl) -> Result<ReliableControlMessage, SenderError> {
        let sequence = self.next_control_sequence()?;
        self.last_control_sequence = sequence;
        Ok(ReliableControlMessage {
            session: self.session,
            sequence,
            payload,
        })
    }

    fn anchor(&self, now: MonotonicTimeMicros, kind: AnchorKind) -> MotionAnchor {
        MotionAnchor {
            activation_id: self.session.activation_id,
            through_motion_sequence: self.last_motion_sequence,
            sender_capture_time: now,
            totals: self.totals,
            final_touch_state: self.held.active_touch.clone(),
            kind,
        }
    }

    fn next_control_sequence(&self) -> Result<ControlSequence, SenderError> {
        self.last_control_sequence
            .0
            .checked_add(1)
            .map(ControlSequence)
            .ok_or(SenderError::SequenceExhausted)
    }

    fn next_motion_sequence(&self) -> Result<MotionSequence, SenderError> {
        self.last_motion_sequence
            .0
            .checked_add(1)
            .map(MotionSequence)
            .ok_or(SenderError::SequenceExhausted)
    }

    fn require_remote(&self) -> Result<(), SenderError> {
        self.remote.then_some(()).ok_or(SenderError::NotRemote)
    }

    fn require_capture_ready(&self) -> Result<(), SenderError> {
        self.require_remote()?;
        if self.pending_takeover.is_some() {
            return Err(SenderError::TakeoverPending);
        }
        Ok(())
    }

    fn observe_time(&mut self, now: MonotonicTimeMicros) -> Result<(), SenderError> {
        if now < self.last_observed_time {
            return Err(SenderError::ClockMovedBackwards);
        }
        self.last_observed_time = now;
        Ok(())
    }
}

fn add_duration(time: MonotonicTimeMicros, duration: Duration) -> MonotonicTimeMicros {
    let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
    MonotonicTimeMicros(time.0.saturating_add(micros))
}

fn one_third_rounded_down(duration: Duration) -> Duration {
    // At least one microsecond keeps very short negotiated leases live while
    // still scheduling strictly no later than one third for normal values.
    Duration::from_micros(
        u64::try_from(duration.as_micros().saturating_sub(1) / 3)
            .unwrap_or(u64::MAX)
            .max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ActivationId, ProtocolVersion, SessionEpoch};

    fn context() -> SessionContext {
        SessionContext {
            protocol_version: ProtocolVersion(1),
            session_epoch: SessionEpoch([1; 16]),
            transport_generation: TransportGeneration(1),
            activation_id: ActivationId(1),
        }
    }

    fn sender() -> Sender {
        Sender::new(
            SenderConfig::new(Duration::from_millis(250), Duration::from_millis(900)).unwrap(),
            context(),
            MonotonicTimeMicros(0),
        )
        .unwrap()
    }

    #[test]
    fn cumulative_frames_and_anchor_share_one_capture_order() {
        let mut sender = sender();
        sender.enter(MonotonicTimeMicros(0)).unwrap();
        let frame = sender
            .capture_motion(
                MotionDelta {
                    dx: 9,
                    ..MotionDelta::default()
                },
                None,
                MonotonicTimeMicros(10),
            )
            .unwrap();
        let click = sender
            .button_down(PointerButton::PRIMARY, MonotonicTimeMicros(11))
            .unwrap();

        assert_eq!(frame.motion_sequence, MotionSequence(1));
        assert_eq!(frame.control_watermark, ControlSequence(1));
        let anchor = click.payload.motion_anchor().unwrap();
        assert_eq!(anchor.through_motion_sequence, MotionSequence(1));
        assert_eq!(anchor.totals, CumulativeMotion::new(9, 0, 0, 0));
    }

    #[test]
    fn changed_totals_force_checkpoint_by_configured_bound() {
        let mut sender = sender();
        sender.enter(MonotonicTimeMicros(0)).unwrap();
        sender
            .capture_motion(
                MotionDelta {
                    dx: 1,
                    ..MotionDelta::default()
                },
                None,
                MonotonicTimeMicros(10),
            )
            .unwrap();

        assert_eq!(
            sender.tick(MonotonicTimeMicros(249_999)).unwrap(),
            SenderTick::Idle
        );
        assert!(matches!(
            sender.tick(MonotonicTimeMicros(250_000)).unwrap(),
            SenderTick::Checkpoint(_)
        ));
    }

    #[test]
    fn held_state_renews_before_one_third_lease_and_requires_ack() {
        let mut sender = sender();
        sender.enter(MonotonicTimeMicros(0)).unwrap();
        sender
            .key_down(HidUsage::keyboard(4), MonotonicTimeMicros(1))
            .unwrap();
        let SenderTick::Checkpoint(snapshot) = sender.tick(MonotonicTimeMicros(250_000)).unwrap()
        else {
            panic!("checkpoint was not enqueued");
        };
        sender
            .acknowledge_snapshot(SnapshotAck {
                snapshot_sequence: snapshot.sequence,
                accepted_generation: TransportGeneration(1),
            })
            .unwrap();
        assert_eq!(
            sender.last_acknowledged_checkpoint().unwrap().0,
            snapshot.sequence
        );
    }

    #[test]
    fn renewal_deadline_is_strictly_before_one_third_of_lease() {
        let mut sender = Sender::new(
            SenderConfig::new(Duration::from_millis(250), Duration::from_millis(750)).unwrap(),
            context(),
            MonotonicTimeMicros(0),
        )
        .unwrap();
        sender.enter(MonotonicTimeMicros(0)).unwrap();
        sender
            .key_down(HidUsage::keyboard(4), MonotonicTimeMicros(1))
            .unwrap();

        assert_eq!(sender.next_deadline(), Some(MonotonicTimeMicros(249_999)));
    }

    #[test]
    fn missing_snapshot_ack_exits_remote_at_advertised_deadline() {
        let mut sender = sender();
        sender.enter(MonotonicTimeMicros(0)).unwrap();
        sender
            .key_down(HidUsage::keyboard(4), MonotonicTimeMicros(1))
            .unwrap();
        sender.tick(MonotonicTimeMicros(250_000)).unwrap();

        assert_eq!(
            sender.tick(MonotonicTimeMicros(1_150_000)).unwrap(),
            SenderTick::ExitRemote(SenderExitReason::SnapshotAckTimedOut)
        );
        assert!(!sender.is_remote());
    }

    #[test]
    fn overflow_closes_activation_and_it_cannot_be_reopened() {
        let mut sender = sender();
        sender.enter(MonotonicTimeMicros(0)).unwrap();
        sender
            .capture_motion(
                MotionDelta {
                    dx: i64::MAX,
                    ..MotionDelta::default()
                },
                None,
                MonotonicTimeMicros(1),
            )
            .unwrap();
        let error = sender
            .capture_motion(
                MotionDelta {
                    dx: 1,
                    ..MotionDelta::default()
                },
                None,
                MonotonicTimeMicros(2),
            )
            .unwrap_err();

        assert!(matches!(error, SenderError::MotionOverflow(_)));
        assert!(!sender.is_remote());
        assert_eq!(
            sender.enter(MonotonicTimeMicros(3)).unwrap_err(),
            SenderError::ClosedActivation
        );
    }

    #[test]
    fn takeover_consumes_one_control_sequence_and_requires_matching_acceptance() {
        let mut sender = sender();
        sender.enter(MonotonicTimeMicros(0)).unwrap();
        let takeover = sender
            .propose_takeover(
                TransportGeneration(2),
                TakeoverNonce([8; 16]),
                MonotonicTimeMicros(1),
            )
            .unwrap();
        assert_eq!(takeover.sequence, ControlSequence(2));
        assert_eq!(
            takeover.session.transport_generation,
            TransportGeneration(2)
        );
        sender
            .accept_takeover(TakeoverAccepted {
                accepted_generation: TransportGeneration(2),
                proposal_nonce: TakeoverNonce([8; 16]),
                receiver_lease_ms: 900,
            })
            .unwrap();
        assert_eq!(
            sender.session().transport_generation,
            TransportGeneration(2)
        );
        assert_eq!(
            sender
                .key_down(HidUsage::keyboard(4), MonotonicTimeMicros(2))
                .unwrap()
                .sequence,
            ControlSequence(3)
        );
    }
}
