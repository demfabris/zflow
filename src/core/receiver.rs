//! Pure receiver-side ordering, reconciliation, and failure recovery.
//!
//! Authentication and playout scheduling live outside this type. The caller
//! explicitly authorizes epochs and supplies monotonic time; the receiver only
//! emits backend-neutral effects.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use thiserror::Error;

use super::{
    ActivationId, ActiveScroll, AnchorKind, ControlSequence, CumulativeMotion, HeldState, HidUsage,
    Modifier, MonotonicTimeMicros, MotionAnchor, MotionDelta, MotionFrame, MotionSequence,
    PlayoutStep, PointerButton, ReliableControl, ReliableControlMessage, ScrollId,
    SessionCloseReason, SessionContext, SessionEpoch, SnapshotAck, TakeoverAccepted, TouchState,
    TransportGeneration,
};

const MAX_HELD_STATE_LEASE: Duration = Duration::from_secs(1);
const MAX_PENDING_MOTION_FRAMES: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiverConfig {
    pub held_state_lease: Duration,
}

impl ReceiverConfig {
    pub fn new(held_state_lease: Duration) -> Result<Self, ReceiverError> {
        if held_state_lease.is_zero() || held_state_lease > MAX_HELD_STATE_LEASE {
            return Err(ReceiverError::InvalidLease);
        }
        Ok(Self { held_state_lease })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverLifecycle {
    ConnectionLost,
    StreamReset,
    BackendTeardown,
    ProcessDeath,
    Suspend,
    Resume,
}

impl ReceiverLifecycle {
    fn close_reason(self) -> SessionCloseReason {
        match self {
            Self::Suspend | Self::Resume => SessionCloseReason::Suspend,
            Self::BackendTeardown | Self::ProcessDeath => SessionCloseReason::BackendUnavailable,
            Self::ConnectionLost | Self::StreamReset => SessionCloseReason::LeaseExpired,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionReason {
    NoAuthorizedSession,
    StaleEpoch,
    StaleGeneration,
    FutureGeneration,
    ClosedActivation,
    StaleActivation,
    NoOpenActivation,
    DuplicateControl,
    ControlGap,
    DuplicateOrReorderedMotion,
    MotionPastTerminalCutoff,
    AnchorActivationMismatch,
    AnchorMovedBackwards,
    InvalidTransition,
    InvalidTakeover,
    WrongDirection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiverEffect {
    ActivationOpened(SessionContext),
    Motion {
        delta: MotionDelta,
        through_sequence: MotionSequence,
    },
    Key {
        key: HidUsage,
        pressed: bool,
        synthetic: bool,
    },
    Button {
        button: PointerButton,
        pressed: bool,
        synthetic: bool,
    },
    Modifier {
        modifier: Modifier,
        pressed: bool,
        synthetic: bool,
    },
    ScrollBegan(ActiveScroll),
    ScrollEnded {
        id: ScrollId,
        cancelled: bool,
        synthetic: bool,
    },
    TouchReplaced {
        state: TouchState,
        synthetic: bool,
    },
    SnapshotAck {
        session: SessionContext,
        ack: SnapshotAck,
    },
    TakeoverAccepted {
        session: SessionContext,
        accepted: TakeoverAccepted,
    },
    ActivationClosed {
        session: SessionContext,
        reason: SessionCloseReason,
    },
    Rejected {
        session: SessionContext,
        reason: RejectionReason,
    },
}

impl ReceiverEffect {
    /// True only for an effect that a platform input backend must apply.
    pub fn is_injection(&self) -> bool {
        matches!(
            self,
            Self::Motion { .. }
                | Self::Key { .. }
                | Self::Button { .. }
                | Self::Modifier { .. }
                | Self::ScrollBegan(_)
                | Self::ScrollEnded { .. }
                | Self::TouchReplaced { .. }
        )
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverError {
    #[error("held-state lease must be in 1..=1000 ms")]
    InvalidLease,
    #[error("receiver monotonic time moved backwards")]
    ClockMovedBackwards,
    #[error("a generation change for an open activation requires SessionTakeover")]
    TakeoverRequired,
    #[error("accepted transport generations must increase exactly by one")]
    InvalidGenerationAdvance,
    #[error("the protocol version cannot change within one session epoch")]
    ProtocolChangedWithinEpoch,
}

#[derive(Debug, Clone)]
struct ActivationState {
    session: SessionContext,
    last_control_sequence: ControlSequence,
    held: HeldState,
    highest_seen_motion: MotionSequence,
    last_injected_motion: MotionSequence,
    injected_totals: CumulativeMotion,
    pending_motion: BTreeMap<MotionSequence, MotionFrame>,
    terminal_cutoff: Option<MotionSequence>,
    lease_deadline: Option<MonotonicTimeMicros>,
}

impl ActivationState {
    fn new(session: SessionContext) -> Self {
        Self {
            session,
            last_control_sequence: ControlSequence(0),
            held: HeldState::default(),
            highest_seen_motion: MotionSequence(0),
            last_injected_motion: MotionSequence(0),
            injected_totals: CumulativeMotion::ZERO,
            pending_motion: BTreeMap::new(),
            terminal_cutoff: None,
            lease_deadline: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Receiver {
    config: ReceiverConfig,
    accepted_protocol: Option<super::ProtocolVersion>,
    accepted_epoch: Option<SessionEpoch>,
    accepted_generation: Option<TransportGeneration>,
    highest_activation: Option<ActivationId>,
    activation: Option<ActivationState>,
    closed_activations: BTreeSet<(SessionEpoch, ActivationId)>,
    accepted_takeovers: BTreeSet<(SessionEpoch, TransportGeneration)>,
    last_observed_time: MonotonicTimeMicros,
}

impl Receiver {
    pub fn new(config: ReceiverConfig, now: MonotonicTimeMicros) -> Result<Self, ReceiverError> {
        let config = ReceiverConfig::new(config.held_state_lease)?;
        Ok(Self {
            config,
            accepted_protocol: None,
            accepted_epoch: None,
            accepted_generation: None,
            highest_activation: None,
            activation: None,
            closed_activations: BTreeSet::new(),
            accepted_takeovers: BTreeSet::new(),
            last_observed_time: now,
        })
    }

    /// Authorizes an authenticated session boundary.
    ///
    /// Epoch bytes are random identifiers, not counters. Consequently a packet
    /// can never replace an epoch by comparing byte values; only this explicit
    /// lifecycle input can do so.
    pub fn authorize_session(
        &mut self,
        session: SessionContext,
        now: MonotonicTimeMicros,
    ) -> Result<Vec<ReceiverEffect>, ReceiverError> {
        self.observe_time(now)?;
        let mut effects = Vec::new();
        match self.accepted_epoch {
            None => {
                self.accepted_protocol = Some(session.protocol_version);
                self.accepted_epoch = Some(session.session_epoch);
                self.accepted_generation = Some(session.transport_generation);
                self.highest_activation = None;
            }
            Some(epoch) if epoch != session.session_epoch => {
                self.close_activation(SessionCloseReason::Superseded, &mut effects);
                self.closed_activations.clear();
                self.accepted_takeovers.clear();
                self.accepted_protocol = Some(session.protocol_version);
                self.accepted_epoch = Some(session.session_epoch);
                self.accepted_generation = Some(session.transport_generation);
                self.highest_activation = None;
            }
            Some(_) => {
                if self.accepted_protocol != Some(session.protocol_version) {
                    return Err(ReceiverError::ProtocolChangedWithinEpoch);
                }
                let accepted = self.accepted_generation.expect("epoch has a generation");
                if self.activation.is_some() && session.transport_generation != accepted {
                    return Err(ReceiverError::TakeoverRequired);
                }
                if session.transport_generation != accepted {
                    let expected = accepted.0.checked_add(1);
                    if expected != Some(session.transport_generation.0) {
                        return Err(ReceiverError::InvalidGenerationAdvance);
                    }
                    self.accepted_generation = Some(session.transport_generation);
                }
                self.accepted_protocol = Some(session.protocol_version);
            }
        }
        Ok(effects)
    }

    pub fn active_context(&self) -> Option<SessionContext> {
        self.activation.as_ref().map(|state| state.session)
    }

    pub fn held_state(&self) -> Option<&HeldState> {
        self.activation.as_ref().map(|state| &state.held)
    }

    pub fn injected_totals(&self) -> Option<CumulativeMotion> {
        self.activation.as_ref().map(|state| state.injected_totals)
    }

    pub fn lease_deadline(&self) -> Option<MonotonicTimeMicros> {
        self.activation
            .as_ref()
            .and_then(|state| state.lease_deadline)
    }

    pub fn is_closed(&self, epoch: SessionEpoch, activation: ActivationId) -> bool {
        self.closed_activations.contains(&(epoch, activation))
    }

    pub fn receive_control(
        &mut self,
        message: ReliableControlMessage,
        now: MonotonicTimeMicros,
    ) -> Result<Vec<ReceiverEffect>, ReceiverError> {
        self.observe_time(now)?;
        let mut effects = Vec::new();
        self.expire_lease(now, &mut effects);
        if matches!(message.payload, ReliableControl::SessionTakeover(_)) {
            effects.extend(self.receive_takeover(message, now));
            return Ok(effects);
        }

        if let Some(reason) = self.session_rejection(message.session, false) {
            effects.push(rejected(message.session, reason));
            return Ok(effects);
        }

        if matches!(message.payload, ReliableControl::Enter) {
            self.receive_enter(message, now, &mut effects);
            return Ok(effects);
        }

        if self
            .closed_activations
            .contains(&(message.session.session_epoch, message.session.activation_id))
        {
            effects.push(rejected(message.session, RejectionReason::ClosedActivation));
            return Ok(effects);
        }

        let Some(state) = self.activation.as_ref() else {
            effects.push(rejected(message.session, RejectionReason::NoOpenActivation));
            return Ok(effects);
        };
        if state.session.activation_id != message.session.activation_id {
            let reason = if self
                .closed_activations
                .contains(&(message.session.session_epoch, message.session.activation_id))
            {
                RejectionReason::ClosedActivation
            } else {
                RejectionReason::StaleActivation
            };
            effects.push(rejected(message.session, reason));
            return Ok(effects);
        }
        if let Some(reason) = sequence_rejection(state.last_control_sequence, message.sequence) {
            effects.push(rejected(message.session, reason));
            return Ok(effects);
        }

        if !self.validate_transition(&message.payload) {
            effects.push(rejected(
                message.session,
                RejectionReason::InvalidTransition,
            ));
            return Ok(effects);
        }

        let anchor_delta = if let Some(anchor) = message.payload.motion_anchor() {
            let state = self.activation.as_ref().expect("checked above");
            match validate_anchor(state, anchor) {
                Ok(delta) => Some(delta),
                Err(AnchorFailure::Rejected(reason)) => {
                    effects.push(rejected(message.session, reason));
                    return Ok(effects);
                }
                Err(AnchorFailure::MotionOverflow) => {
                    self.close_activation(SessionCloseReason::MotionOverflow, &mut effects);
                    return Ok(effects);
                }
            }
        } else {
            None
        };

        let mut close = None;
        let mut snapshot_ack = None;
        let mut drain_failure = None;
        {
            let state = self.activation.as_mut().expect("checked above");
            if let (Some(anchor), Some(delta)) = (message.payload.motion_anchor(), anchor_delta) {
                apply_anchor(state, anchor, delta, &mut effects);
            }

            match &message.payload {
                ReliableControl::Leave { .. } => close = Some(SessionCloseReason::LocalRelease),
                ReliableControl::KeyDown { key } => {
                    state.held.press_key(*key);
                    effects.push(ReceiverEffect::Key {
                        key: *key,
                        pressed: true,
                        synthetic: false,
                    });
                }
                ReliableControl::KeyUp { key } => {
                    state.held.release_key(*key);
                    effects.push(ReceiverEffect::Key {
                        key: *key,
                        pressed: false,
                        synthetic: false,
                    });
                }
                ReliableControl::ButtonDown { button, .. } => {
                    state.held.press_button(*button);
                    effects.push(ReceiverEffect::Button {
                        button: *button,
                        pressed: true,
                        synthetic: false,
                    });
                }
                ReliableControl::ButtonUp { button, .. } => {
                    state.held.release_button(*button);
                    effects.push(ReceiverEffect::Button {
                        button: *button,
                        pressed: false,
                        synthetic: false,
                    });
                }
                ReliableControl::ScrollBegin { scroll } => {
                    state.held.begin_scroll(*scroll);
                    effects.push(ReceiverEffect::ScrollBegan(*scroll));
                }
                ReliableControl::ScrollEnd { scroll_id, .. } => {
                    state.held.end_scroll(*scroll_id);
                    effects.push(ReceiverEffect::ScrollEnded {
                        id: *scroll_id,
                        cancelled: false,
                        synthetic: false,
                    });
                }
                ReliableControl::ScrollCancel { scroll_id, .. } => {
                    state.held.end_scroll(*scroll_id);
                    effects.push(ReceiverEffect::ScrollEnded {
                        id: *scroll_id,
                        cancelled: true,
                        synthetic: false,
                    });
                }
                ReliableControl::TouchBegin { initial_state } => {
                    state.held.replace_touch(initial_state.clone());
                    effects.push(ReceiverEffect::TouchReplaced {
                        state: initial_state.clone(),
                        synthetic: false,
                    });
                }
                ReliableControl::TouchEnd { .. } | ReliableControl::TouchCancel { .. } => {
                    state.held.clear_touch();
                    effects.push(ReceiverEffect::TouchReplaced {
                        state: TouchState::default(),
                        synthetic: false,
                    });
                }
                ReliableControl::StateSnapshot(snapshot) => {
                    reconcile_held(state, &snapshot.held, &mut effects);
                    snapshot_ack = Some(SnapshotAck {
                        snapshot_sequence: message.sequence,
                        accepted_generation: state.session.transport_generation,
                    });
                }
                ReliableControl::SessionClose { reason, .. } => close = Some(*reason),
                ReliableControl::SnapshotAck(_) | ReliableControl::TakeoverAccepted(_) => {
                    effects.push(rejected(message.session, RejectionReason::WrongDirection));
                    return Ok(effects);
                }
                ReliableControl::Enter | ReliableControl::SessionTakeover(_) => unreachable!(),
            }

            state.last_control_sequence = message.sequence;
            refresh_lease(state, now, self.config.held_state_lease);
            if close.is_none() {
                drain_failure = drain_mature_motion(state, &mut effects).err();
            }
            if drain_failure.is_none()
                && let Some(ack) = snapshot_ack
            {
                effects.push(ReceiverEffect::SnapshotAck {
                    session: state.session,
                    ack,
                });
            }
        }

        if let Some(failure) = drain_failure {
            self.close_activation(failure.close_reason(), &mut effects);
            return Ok(effects);
        }
        if let Some(reason) = close {
            self.close_activation(reason, &mut effects);
        }
        Ok(effects)
    }

    pub fn receive_motion(
        &mut self,
        frame: MotionFrame,
        now: MonotonicTimeMicros,
    ) -> Result<Vec<ReceiverEffect>, ReceiverError> {
        self.observe_time(now)?;
        let mut effects = Vec::new();
        self.expire_lease(now, &mut effects);
        if let Some(reason) = self.session_rejection(frame.session, false) {
            effects.push(rejected(frame.session, reason));
            return Ok(effects);
        }
        if self
            .closed_activations
            .contains(&(frame.session.session_epoch, frame.session.activation_id))
        {
            effects.push(rejected(frame.session, RejectionReason::ClosedActivation));
            return Ok(effects);
        }
        let Some(state) = self.activation.as_mut() else {
            effects.push(rejected(frame.session, RejectionReason::NoOpenActivation));
            return Ok(effects);
        };
        if state.session.activation_id != frame.session.activation_id {
            let reason = if self
                .closed_activations
                .contains(&(frame.session.session_epoch, frame.session.activation_id))
            {
                RejectionReason::ClosedActivation
            } else {
                RejectionReason::StaleActivation
            };
            effects.push(rejected(frame.session, reason));
            return Ok(effects);
        }
        if state
            .terminal_cutoff
            .is_some_and(|cutoff| frame.motion_sequence <= cutoff)
        {
            effects.push(rejected(
                frame.session,
                RejectionReason::MotionPastTerminalCutoff,
            ));
            return Ok(effects);
        }
        if frame.motion_sequence <= state.last_injected_motion
            || state.pending_motion.contains_key(&frame.motion_sequence)
        {
            effects.push(rejected(
                frame.session,
                RejectionReason::DuplicateOrReorderedMotion,
            ));
            return Ok(effects);
        }

        state.highest_seen_motion = state.highest_seen_motion.max(frame.motion_sequence);
        state.pending_motion.insert(frame.motion_sequence, frame);
        let drain_failure = drain_mature_motion(state, &mut effects).err();
        if let Some(failure) = drain_failure {
            self.close_activation(failure.close_reason(), &mut effects);
        } else if self
            .activation
            .as_ref()
            .is_some_and(|state| state.pending_motion.len() > MAX_PENDING_MOTION_FRAMES)
        {
            self.close_activation(SessionCloseReason::ProtocolViolation, &mut effects);
        }
        Ok(effects)
    }

    /// Applies one bounded playout step while keeping anchor reconciliation
    /// based on the displacement that the backend has actually received.
    /// Several steps may name the same cumulative target until it is reached.
    pub fn receive_playout_step(
        &mut self,
        session: SessionContext,
        step: PlayoutStep,
        now: MonotonicTimeMicros,
    ) -> Result<Vec<ReceiverEffect>, ReceiverError> {
        self.observe_time(now)?;
        let mut effects = Vec::new();
        self.expire_lease(now, &mut effects);
        if let Some(reason) = self.session_rejection(session, false) {
            effects.push(rejected(session, reason));
            return Ok(effects);
        }
        if self
            .closed_activations
            .contains(&(session.session_epoch, session.activation_id))
        {
            effects.push(rejected(session, RejectionReason::ClosedActivation));
            return Ok(effects);
        }
        let Some(state) = self.activation.as_mut() else {
            effects.push(rejected(session, RejectionReason::NoOpenActivation));
            return Ok(effects);
        };
        if state.session.activation_id != session.activation_id {
            effects.push(rejected(session, RejectionReason::StaleActivation));
            return Ok(effects);
        }
        if state
            .terminal_cutoff
            .is_some_and(|cutoff| step.through_sequence <= cutoff)
        {
            effects.push(rejected(session, RejectionReason::MotionPastTerminalCutoff));
            return Ok(effects);
        }
        if step.through_sequence <= state.last_injected_motion {
            effects.push(rejected(
                session,
                RejectionReason::DuplicateOrReorderedMotion,
            ));
            return Ok(effects);
        }
        if step
            .touch_snapshot
            .as_ref()
            .is_some_and(|touch| state.held.active_touch.is_empty() && !touch.is_empty())
        {
            self.close_activation(SessionCloseReason::ProtocolViolation, &mut effects);
            return Ok(effects);
        }

        let totals = match state.injected_totals.checked_add(step.delta) {
            Ok(totals) => totals,
            Err(_) => {
                self.close_activation(SessionCloseReason::MotionOverflow, &mut effects);
                return Ok(effects);
            }
        };
        state.injected_totals = totals;
        state.highest_seen_motion = state.highest_seen_motion.max(step.through_sequence);
        if step.delta != MotionDelta::default() {
            effects.push(ReceiverEffect::Motion {
                delta: step.delta,
                through_sequence: step.through_sequence,
            });
        }
        if let Some(touch) = step.touch_snapshot
            && state.held.active_touch != touch
        {
            state.held.replace_touch(touch.clone());
            effects.push(ReceiverEffect::TouchReplaced {
                state: touch,
                synthetic: false,
            });
        }
        if step.target_reached {
            state.last_injected_motion = step.through_sequence;
            state
                .pending_motion
                .retain(|sequence, _| *sequence > step.through_sequence);
        }
        Ok(effects)
    }

    pub fn tick(&mut self, now: MonotonicTimeMicros) -> Result<Vec<ReceiverEffect>, ReceiverError> {
        self.observe_time(now)?;
        let mut effects = Vec::new();
        self.expire_lease(now, &mut effects);
        Ok(effects)
    }

    fn expire_lease(&mut self, now: MonotonicTimeMicros, effects: &mut Vec<ReceiverEffect>) {
        if self
            .activation
            .as_ref()
            .and_then(|state| state.lease_deadline)
            .is_some_and(|deadline| deadline <= now)
        {
            self.close_activation(SessionCloseReason::LeaseExpired, effects);
        }
    }

    pub fn lifecycle(
        &mut self,
        event: ReceiverLifecycle,
        now: MonotonicTimeMicros,
    ) -> Result<Vec<ReceiverEffect>, ReceiverError> {
        self.observe_time(now)?;
        let mut effects = Vec::new();
        self.close_activation(event.close_reason(), &mut effects);
        Ok(effects)
    }

    fn receive_enter(
        &mut self,
        message: ReliableControlMessage,
        now: MonotonicTimeMicros,
        effects: &mut Vec<ReceiverEffect>,
    ) {
        if self
            .closed_activations
            .contains(&(message.session.session_epoch, message.session.activation_id))
        {
            effects.push(rejected(message.session, RejectionReason::ClosedActivation));
            return;
        }
        if self
            .highest_activation
            .is_some_and(|highest| message.session.activation_id <= highest)
        {
            effects.push(rejected(message.session, RejectionReason::StaleActivation));
            return;
        }
        if message.sequence != ControlSequence(1) {
            effects.push(rejected(message.session, RejectionReason::ControlGap));
            return;
        }
        if self.activation.is_some() {
            self.close_activation(SessionCloseReason::Superseded, effects);
        }
        let mut state = ActivationState::new(message.session);
        state.last_control_sequence = message.sequence;
        refresh_lease(&mut state, now, self.config.held_state_lease);
        self.highest_activation = Some(message.session.activation_id);
        self.activation = Some(state);
        effects.push(ReceiverEffect::ActivationOpened(message.session));
    }

    fn receive_takeover(
        &mut self,
        message: ReliableControlMessage,
        now: MonotonicTimeMicros,
    ) -> Vec<ReceiverEffect> {
        let mut effects = Vec::new();
        let ReliableControl::SessionTakeover(takeover) = &message.payload else {
            unreachable!();
        };
        let Some(state) = self.activation.as_ref() else {
            effects.push(rejected(message.session, RejectionReason::NoOpenActivation));
            return effects;
        };
        let next_generation = state.session.transport_generation.0.checked_add(1);
        let valid = message.session.protocol_version == state.session.protocol_version
            && message.session.session_epoch == state.session.session_epoch
            && message.session.activation_id == state.session.activation_id
            && takeover.prior_generation == state.session.transport_generation
            && takeover.proposed_generation == message.session.transport_generation
            && next_generation == Some(takeover.proposed_generation.0)
            && takeover.last_control_sequence == state.last_control_sequence
            && message.sequence.0 == state.last_control_sequence.0.saturating_add(1)
            && takeover.final_motion_anchor.kind == AnchorKind::Checkpoint
            && takeover.final_motion_anchor.final_touch_state
                == takeover.authoritative_held_state.active_touch
            && !self
                .accepted_takeovers
                .contains(&(message.session.session_epoch, takeover.proposed_generation));
        if !valid {
            effects.push(rejected(message.session, RejectionReason::InvalidTakeover));
            return effects;
        }
        let anchor_delta = match validate_anchor(state, &takeover.final_motion_anchor) {
            Ok(delta) => delta,
            Err(AnchorFailure::Rejected(reason)) => {
                effects.push(rejected(message.session, reason));
                return effects;
            }
            Err(AnchorFailure::MotionOverflow) => {
                self.close_activation(SessionCloseReason::MotionOverflow, &mut effects);
                return effects;
            }
        };
        let state = self.activation.as_mut().expect("validated above");
        apply_anchor(
            state,
            &takeover.final_motion_anchor,
            anchor_delta,
            &mut effects,
        );
        reconcile_held(state, &takeover.authoritative_held_state, &mut effects);
        state.session.transport_generation = takeover.proposed_generation;
        state.last_control_sequence = message.sequence;
        state.pending_motion.clear();
        refresh_lease(state, now, self.config.held_state_lease);
        self.accepted_generation = Some(takeover.proposed_generation);
        self.accepted_takeovers
            .insert((message.session.session_epoch, takeover.proposed_generation));
        effects.push(ReceiverEffect::TakeoverAccepted {
            session: state.session,
            accepted: TakeoverAccepted {
                accepted_generation: takeover.proposed_generation,
                proposal_nonce: takeover.proposal_nonce,
                receiver_lease_ms: self.config.held_state_lease.as_millis() as u32,
            },
        });
        effects
    }

    fn validate_transition(&self, payload: &ReliableControl) -> bool {
        let Some(state) = self.activation.as_ref() else {
            return false;
        };
        match payload {
            ReliableControl::KeyDown { key } => !state.held.pressed_keys.contains(key),
            ReliableControl::KeyUp { key } => state.held.pressed_keys.contains(key),
            ReliableControl::ButtonDown { button, anchor } => {
                anchor.kind == AnchorKind::Checkpoint
                    && !state.held.pressed_buttons.contains(button)
            }
            ReliableControl::ButtonUp { button, anchor } => {
                anchor.kind == AnchorKind::Checkpoint && state.held.pressed_buttons.contains(button)
            }
            ReliableControl::ScrollBegin { .. } => state.held.active_scroll.is_none(),
            ReliableControl::ScrollEnd { scroll_id, anchor }
            | ReliableControl::ScrollCancel { scroll_id, anchor } => {
                anchor.kind == AnchorKind::Checkpoint
                    && state
                        .held
                        .active_scroll
                        .is_some_and(|scroll| scroll.id == *scroll_id)
            }
            ReliableControl::TouchBegin { initial_state } => {
                state.held.active_touch.is_empty() && !initial_state.is_empty()
            }
            ReliableControl::TouchEnd { anchor } | ReliableControl::TouchCancel { anchor } => {
                anchor.kind == AnchorKind::Checkpoint && !state.held.active_touch.is_empty()
            }
            ReliableControl::StateSnapshot(snapshot) => {
                snapshot.motion_anchor.kind == AnchorKind::Checkpoint
                    && snapshot.motion_anchor.final_touch_state == snapshot.held.active_touch
            }
            ReliableControl::Leave { anchor } => anchor.kind == AnchorKind::Terminal,
            ReliableControl::SessionClose { final_anchor, .. } => final_anchor
                .as_ref()
                .is_none_or(|anchor| anchor.kind == AnchorKind::Terminal),
            ReliableControl::Enter
            | ReliableControl::SnapshotAck(_)
            | ReliableControl::SessionTakeover(_)
            | ReliableControl::TakeoverAccepted(_) => false,
        }
    }

    fn session_rejection(
        &self,
        session: SessionContext,
        allow_next_generation: bool,
    ) -> Option<RejectionReason> {
        let Some(epoch) = self.accepted_epoch else {
            return Some(RejectionReason::NoAuthorizedSession);
        };
        if session.session_epoch != epoch {
            return Some(RejectionReason::StaleEpoch);
        }
        if self.accepted_protocol != Some(session.protocol_version) {
            return Some(RejectionReason::StaleEpoch);
        }
        let generation = self.accepted_generation.expect("epoch has generation");
        if session.transport_generation < generation {
            return Some(RejectionReason::StaleGeneration);
        }
        if session.transport_generation > generation
            && !(allow_next_generation
                && generation.0.checked_add(1) == Some(session.transport_generation.0))
        {
            return Some(RejectionReason::FutureGeneration);
        }
        None
    }

    fn close_activation(&mut self, reason: SessionCloseReason, effects: &mut Vec<ReceiverEffect>) {
        let Some(mut state) = self.activation.take() else {
            return;
        };
        release_all(&mut state, effects);
        self.closed_activations
            .insert((state.session.session_epoch, state.session.activation_id));
        effects.push(ReceiverEffect::ActivationClosed {
            session: state.session,
            reason,
        });
    }

    fn observe_time(&mut self, now: MonotonicTimeMicros) -> Result<(), ReceiverError> {
        if now < self.last_observed_time {
            return Err(ReceiverError::ClockMovedBackwards);
        }
        self.last_observed_time = now;
        Ok(())
    }
}

fn sequence_rejection(
    previous: ControlSequence,
    received: ControlSequence,
) -> Option<RejectionReason> {
    if received <= previous {
        Some(RejectionReason::DuplicateControl)
    } else if previous.0.checked_add(1) != Some(received.0) {
        Some(RejectionReason::ControlGap)
    } else {
        None
    }
}

fn rejected(session: SessionContext, reason: RejectionReason) -> ReceiverEffect {
    ReceiverEffect::Rejected { session, reason }
}

fn refresh_lease(state: &mut ActivationState, now: MonotonicTimeMicros, lease: Duration) {
    state.lease_deadline = (!state.held.is_neutral()).then(|| add_duration(now, lease));
}

fn add_duration(time: MonotonicTimeMicros, duration: Duration) -> MonotonicTimeMicros {
    let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
    MonotonicTimeMicros(time.0.saturating_add(micros))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnchorFailure {
    Rejected(RejectionReason),
    MotionOverflow,
}

fn validate_anchor(
    state: &ActivationState,
    anchor: &MotionAnchor,
) -> Result<MotionDelta, AnchorFailure> {
    if anchor.activation_id != state.session.activation_id {
        return Err(AnchorFailure::Rejected(
            RejectionReason::AnchorActivationMismatch,
        ));
    }
    if anchor.through_motion_sequence < state.last_injected_motion {
        return Err(AnchorFailure::Rejected(
            RejectionReason::AnchorMovedBackwards,
        ));
    }
    anchor
        .totals
        .checked_delta_from(state.injected_totals)
        .map_err(|_| AnchorFailure::MotionOverflow)
}

fn apply_anchor(
    state: &mut ActivationState,
    anchor: &MotionAnchor,
    delta: MotionDelta,
    effects: &mut Vec<ReceiverEffect>,
) {
    state
        .pending_motion
        .retain(|sequence, _| *sequence > anchor.through_motion_sequence);
    state.highest_seen_motion = state
        .highest_seen_motion
        .max(anchor.through_motion_sequence);
    state.last_injected_motion = anchor.through_motion_sequence;
    state.injected_totals = anchor.totals;
    if delta != MotionDelta::default() {
        effects.push(ReceiverEffect::Motion {
            delta,
            through_sequence: anchor.through_motion_sequence,
        });
    }
    if state.held.active_touch != anchor.final_touch_state {
        state.held.replace_touch(anchor.final_touch_state.clone());
        effects.push(ReceiverEffect::TouchReplaced {
            state: anchor.final_touch_state.clone(),
            synthetic: true,
        });
    }
    if anchor.kind == AnchorKind::Terminal {
        state.terminal_cutoff = Some(anchor.through_motion_sequence);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainFailure {
    MotionOverflow,
    InvalidTouchLifecycle,
}

impl DrainFailure {
    fn close_reason(self) -> SessionCloseReason {
        match self {
            Self::MotionOverflow => SessionCloseReason::MotionOverflow,
            Self::InvalidTouchLifecycle => SessionCloseReason::ProtocolViolation,
        }
    }
}

fn drain_mature_motion(
    state: &mut ActivationState,
    effects: &mut Vec<ReceiverEffect>,
) -> Result<(), DrainFailure> {
    let candidate = state
        .pending_motion
        .iter()
        .rev()
        .find(|(_, frame)| frame.control_watermark <= state.last_control_sequence)
        .map(|(sequence, _)| *sequence);
    let Some(sequence) = candidate else {
        return Ok(());
    };
    let frame = state
        .pending_motion
        .remove(&sequence)
        .expect("candidate came from map");
    state.pending_motion.retain(|queued, _| *queued > sequence);
    if sequence <= state.last_injected_motion {
        return Ok(());
    }
    if frame
        .touch_snapshot
        .as_ref()
        .is_some_and(|touch| state.held.active_touch.is_empty() && !touch.is_empty())
    {
        return Err(DrainFailure::InvalidTouchLifecycle);
    }
    let delta = frame
        .totals
        .checked_delta_from(state.injected_totals)
        .map_err(|_| DrainFailure::MotionOverflow)?;
    state.last_injected_motion = sequence;
    state.injected_totals = frame.totals;
    if delta != MotionDelta::default() {
        effects.push(ReceiverEffect::Motion {
            delta,
            through_sequence: sequence,
        });
    }
    if let Some(touch) = frame.touch_snapshot
        && state.held.active_touch != touch
    {
        state.held.replace_touch(touch.clone());
        effects.push(ReceiverEffect::TouchReplaced {
            state: touch,
            synthetic: false,
        });
    }
    Ok(())
}

fn reconcile_held(
    state: &mut ActivationState,
    authoritative: &HeldState,
    effects: &mut Vec<ReceiverEffect>,
) {
    for key in state
        .held
        .pressed_keys
        .difference(&authoritative.pressed_keys)
        .copied()
        .collect::<Vec<_>>()
    {
        state.held.release_key(key);
        effects.push(ReceiverEffect::Key {
            key,
            pressed: false,
            synthetic: true,
        });
    }
    for key in authoritative
        .pressed_keys
        .difference(&state.held.pressed_keys)
        .copied()
        .collect::<Vec<_>>()
    {
        state.held.press_key(key);
        effects.push(ReceiverEffect::Key {
            key,
            pressed: true,
            synthetic: true,
        });
    }
    for button in state
        .held
        .pressed_buttons
        .difference(&authoritative.pressed_buttons)
        .copied()
        .collect::<Vec<_>>()
    {
        state.held.release_button(button);
        effects.push(ReceiverEffect::Button {
            button,
            pressed: false,
            synthetic: true,
        });
    }
    for button in authoritative
        .pressed_buttons
        .difference(&state.held.pressed_buttons)
        .copied()
        .collect::<Vec<_>>()
    {
        state.held.press_button(button);
        effects.push(ReceiverEffect::Button {
            button,
            pressed: true,
            synthetic: true,
        });
    }
    for modifier in state
        .held
        .modifiers
        .difference(&authoritative.modifiers)
        .copied()
        .collect::<Vec<_>>()
    {
        state.held.set_modifier(modifier, false);
        effects.push(ReceiverEffect::Modifier {
            modifier,
            pressed: false,
            synthetic: true,
        });
    }
    for modifier in authoritative
        .modifiers
        .difference(&state.held.modifiers)
        .copied()
        .collect::<Vec<_>>()
    {
        state.held.set_modifier(modifier, true);
        effects.push(ReceiverEffect::Modifier {
            modifier,
            pressed: true,
            synthetic: true,
        });
    }
    if state.held.active_scroll != authoritative.active_scroll {
        if let Some(scroll) = state.held.active_scroll {
            effects.push(ReceiverEffect::ScrollEnded {
                id: scroll.id,
                cancelled: true,
                synthetic: true,
            });
        }
        state.held.active_scroll = authoritative.active_scroll;
        if let Some(scroll) = authoritative.active_scroll {
            effects.push(ReceiverEffect::ScrollBegan(scroll));
        }
    }
    if state.held.active_touch != authoritative.active_touch {
        state.held.replace_touch(authoritative.active_touch.clone());
        effects.push(ReceiverEffect::TouchReplaced {
            state: authoritative.active_touch.clone(),
            synthetic: true,
        });
    }
}

fn release_all(state: &mut ActivationState, effects: &mut Vec<ReceiverEffect>) {
    let neutral = HeldState::default();
    reconcile_held(state, &neutral, effects);
    state.lease_deadline = None;
    state.pending_motion.clear();
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;
    use crate::core::{
        ProtocolVersion, SessionEpoch, SessionTakeover, StateSnapshot, TakeoverNonce,
    };

    fn context(epoch: u8, generation: u64, activation: u64) -> SessionContext {
        SessionContext {
            protocol_version: ProtocolVersion(1),
            session_epoch: SessionEpoch([epoch; 16]),
            transport_generation: TransportGeneration(generation),
            activation_id: ActivationId(activation),
        }
    }

    fn receiver(lease_ms: u64) -> Receiver {
        Receiver::new(
            ReceiverConfig::new(Duration::from_millis(lease_ms)).unwrap(),
            MonotonicTimeMicros(0),
        )
        .unwrap()
    }

    fn enter(receiver: &mut Receiver, session: SessionContext, at: u64) {
        receiver
            .authorize_session(session, MonotonicTimeMicros(at))
            .unwrap();
        let effects = receiver
            .receive_control(
                ReliableControlMessage {
                    session,
                    sequence: ControlSequence(1),
                    payload: ReliableControl::Enter,
                },
                MonotonicTimeMicros(at),
            )
            .unwrap();
        assert!(matches!(
            effects.as_slice(),
            [ReceiverEffect::ActivationOpened(_)]
        ));
    }

    fn anchor(session: SessionContext, sequence: u64, x: i64) -> MotionAnchor {
        MotionAnchor {
            activation_id: session.activation_id,
            through_motion_sequence: MotionSequence(sequence),
            sender_capture_time: MonotonicTimeMicros(0),
            totals: CumulativeMotion::new(x, 0, 0, 0),
            final_touch_state: TouchState::default(),
            kind: AnchorKind::Checkpoint,
        }
    }

    proptest! {
        /// Required property: held input is gone at, never after, the negotiated lease.
        #[test]
        fn lease_expiry_releases_held_state(
            lease_ms in 1_u64..=1_000,
            pressed_for_ms in 0_u64..=20,
        ) {
            let session = context(1, 1, 1);
            let mut receiver = receiver(lease_ms);
            enter(&mut receiver, session, 0);
            receiver.receive_control(
                ReliableControlMessage {
                    session,
                    sequence: ControlSequence(2),
                    payload: ReliableControl::KeyDown { key: HidUsage::keyboard(4) },
                },
                MonotonicTimeMicros(pressed_for_ms * 1_000),
            ).unwrap();
            let deadline = (pressed_for_ms + lease_ms) * 1_000;
            if deadline > 0 {
                prop_assert!(receiver.tick(MonotonicTimeMicros(deadline - 1)).unwrap().is_empty());
            }
            let effects = receiver.tick(MonotonicTimeMicros(deadline)).unwrap();
            let released_key = effects.iter().any(|effect| matches!(
                effect,
                ReceiverEffect::Key { pressed: false, synthetic: true, .. }
            ));
            prop_assert!(released_key);
            prop_assert!(receiver.active_context().is_none());
            prop_assert!(receiver.is_closed(session.session_epoch, session.activation_id));
        }

        /// Required property: stale epoch, generation, and activation traffic never injects.
        #[test]
        fn epoch_generation_activation_ordering_rejects_stale(
            epochs in (any::<u8>(), any::<u8>()).prop_filter(
                "different epochs",
                |(old, new)| old != new,
            ),
            generation in 1_u64..u64::MAX,
            activation in 1_u64..u64::MAX,
        ) {
            let (old_epoch, new_epoch) = epochs;
            let old = context(old_epoch, generation, activation);
            let new = context(new_epoch, generation + 1, activation + 1);
            let mut receiver = receiver(900);
            enter(&mut receiver, old, 0);
            receiver.authorize_session(new, MonotonicTimeMicros(1)).unwrap();
            enter(&mut receiver, new, 1);

            for stale in [
                old,
                SessionContext { transport_generation: TransportGeneration(generation), ..new },
                SessionContext { activation_id: ActivationId(activation), ..new },
            ] {
                let effects = receiver.receive_motion(
                    MotionFrame {
                        session: stale,
                        motion_sequence: MotionSequence(1),
                        control_watermark: ControlSequence(1),
                        sender_capture_time: MonotonicTimeMicros(2),
                        totals: CumulativeMotion::new(99, 0, 0, 0),
                        touch_snapshot: None,
                    },
                    MonotonicTimeMicros(2),
                ).unwrap();
                prop_assert!(!effects.iter().any(ReceiverEffect::is_injection));
            }
        }

        /// Required property: the anchor displacement is emitted before its click.
        #[test]
        fn anchor_precedes_pointer_transition(distance in -1_000_i64..=1_000) {
            let session = context(1, 1, 1);
            let mut receiver = receiver(900);
            enter(&mut receiver, session, 0);
            let effects = receiver.receive_control(
                ReliableControlMessage {
                    session,
                    sequence: ControlSequence(2),
                    payload: ReliableControl::ButtonDown {
                        button: PointerButton::PRIMARY,
                        anchor: anchor(session, 1, distance),
                    },
                },
                MonotonicTimeMicros(1),
            ).unwrap();
            let button_index = effects.iter().position(|effect| matches!(effect, ReceiverEffect::Button { pressed: true, .. })).unwrap();
            if distance != 0 {
                let motion_index = effects.iter().position(|effect| matches!(effect, ReceiverEffect::Motion { .. })).unwrap();
                prop_assert!(motion_index < button_index);
            }
        }

        /// Required property: snapshots emit exactly the set difference.
        #[test]
        fn snapshot_reconciliation_is_by_difference(
            before in prop::collection::btree_set(4_u16..20, 0..8),
            after in prop::collection::btree_set(4_u16..20, 0..8),
        ) {
            let session = context(1, 1, 1);
            let mut receiver = receiver(900);
            enter(&mut receiver, session, 0);
            let state = receiver.activation.as_mut().unwrap();
            state.held.pressed_keys = before.iter().copied().map(HidUsage::keyboard).collect();
            let snapshot = StateSnapshot {
                held: HeldState {
                    pressed_keys: after.iter().copied().map(HidUsage::keyboard).collect(),
                    ..HeldState::default()
                },
                motion_anchor: anchor(session, 0, 0),
            };
            let effects = receiver.receive_control(
                ReliableControlMessage {
                    session,
                    sequence: ControlSequence(2),
                    payload: ReliableControl::StateSnapshot(snapshot),
                },
                MonotonicTimeMicros(1),
            ).unwrap();
            let changed: BTreeSet<_> = effects.iter().filter_map(|effect| match effect {
                ReceiverEffect::Key { key, .. } => Some(key.usage.0),
                _ => None,
            }).collect();
            let expected: BTreeSet<_> = before.symmetric_difference(&after).copied().collect();
            prop_assert_eq!(changed, expected);
            let matching_keys_unchanged = before.intersection(&after).all(|usage| !effects.iter().any(|effect| matches!(
                effect,
                ReceiverEffect::Key { key, .. } if key.usage.0 == *usage
            )));
            prop_assert!(matching_keys_unchanged);
        }
    }

    #[test]
    fn reliable_control_at_lease_deadline_expires_before_it_can_renew() {
        let session = context(1, 1, 1);
        let key = HidUsage::keyboard(4);
        let mut receiver = receiver(10);
        enter(&mut receiver, session, 0);
        receiver
            .receive_control(
                ReliableControlMessage {
                    session,
                    sequence: ControlSequence(2),
                    payload: ReliableControl::KeyDown { key },
                },
                MonotonicTimeMicros(0),
            )
            .unwrap();
        assert_eq!(receiver.lease_deadline(), Some(MonotonicTimeMicros(10_000)));

        let mut held = HeldState::default();
        held.press_key(key);
        let effects = receiver
            .receive_control(
                ReliableControlMessage {
                    session,
                    sequence: ControlSequence(3),
                    payload: ReliableControl::StateSnapshot(StateSnapshot {
                        held,
                        motion_anchor: anchor(session, 0, 0),
                    }),
                },
                MonotonicTimeMicros(10_000),
            )
            .unwrap();

        assert!(matches!(
            effects.as_slice(),
            [
                ReceiverEffect::Key {
                    key: released,
                    pressed: false,
                    synthetic: true,
                },
                ReceiverEffect::ActivationClosed {
                    reason: SessionCloseReason::LeaseExpired,
                    ..
                },
                ReceiverEffect::Rejected {
                    reason: RejectionReason::ClosedActivation,
                    ..
                }
            ] if *released == key
        ));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ReceiverEffect::SnapshotAck { .. }))
        );
        assert!(receiver.active_context().is_none());
        assert_eq!(receiver.lease_deadline(), None);
    }

    #[test]
    fn authorization_error_at_deadline_preserves_state_for_cleanup() {
        let session = context(1, 1, 1);
        let key = HidUsage::keyboard(4);
        let mut receiver = receiver(10);
        enter(&mut receiver, session, 0);
        receiver
            .receive_control(
                ReliableControlMessage {
                    session,
                    sequence: ControlSequence(2),
                    payload: ReliableControl::KeyDown { key },
                },
                MonotonicTimeMicros(0),
            )
            .unwrap();

        let invalid = SessionContext {
            protocol_version: ProtocolVersion(2),
            ..session
        };
        assert_eq!(
            receiver
                .authorize_session(invalid, MonotonicTimeMicros(10_000))
                .unwrap_err(),
            ReceiverError::ProtocolChangedWithinEpoch
        );
        assert_eq!(receiver.active_context(), Some(session));
        assert!(receiver.held_state().unwrap().pressed_keys.contains(&key));

        let effects = receiver
            .lifecycle(
                ReceiverLifecycle::ConnectionLost,
                MonotonicTimeMicros(10_000),
            )
            .unwrap();
        assert!(matches!(
            effects.as_slice(),
            [
                ReceiverEffect::Key {
                    key: released,
                    pressed: false,
                    synthetic: true,
                },
                ReceiverEffect::ActivationClosed { .. }
            ] if *released == key
        ));
        assert!(receiver.active_context().is_none());
    }

    #[test]
    fn latest_mature_cumulative_frame_repairs_lost_and_reordered_motion() {
        let session = context(1, 1, 1);
        let mut receiver = receiver(900);
        enter(&mut receiver, session, 0);
        let frame = |sequence, total| MotionFrame {
            session,
            motion_sequence: MotionSequence(sequence),
            control_watermark: ControlSequence(1),
            sender_capture_time: MonotonicTimeMicros(sequence),
            totals: CumulativeMotion::new(total, 0, 0, 0),
            touch_snapshot: None,
        };

        let effects = receiver
            .receive_motion(frame(3, 30), MonotonicTimeMicros(1))
            .unwrap();
        assert!(effects.iter().any(|effect| matches!(
            effect,
            ReceiverEffect::Motion { delta, .. } if delta.dx == 30
        )));
        let effects = receiver
            .receive_motion(frame(2, 20), MonotonicTimeMicros(2))
            .unwrap();
        assert!(!effects.iter().any(ReceiverEffect::is_injection));
    }

    #[test]
    fn reordered_older_frame_can_mature_while_newer_frame_waits_on_control() {
        let session = context(1, 1, 1);
        let mut receiver = receiver(900);
        enter(&mut receiver, session, 0);
        let frame = |sequence, watermark, total| MotionFrame {
            session,
            motion_sequence: MotionSequence(sequence),
            control_watermark: ControlSequence(watermark),
            sender_capture_time: MonotonicTimeMicros(sequence),
            totals: CumulativeMotion::new(total, 0, 0, 0),
            touch_snapshot: None,
        };

        assert!(
            receiver
                .receive_motion(frame(3, 2, 30), MonotonicTimeMicros(1))
                .unwrap()
                .is_empty()
        );
        let older = receiver
            .receive_motion(frame(2, 1, 20), MonotonicTimeMicros(2))
            .unwrap();
        assert_eq!(motion_dx(&older), 20);
        let control = receiver
            .receive_control(
                ReliableControlMessage {
                    session,
                    sequence: ControlSequence(2),
                    payload: ReliableControl::KeyDown {
                        key: HidUsage::keyboard(4),
                    },
                },
                MonotonicTimeMicros(3),
            )
            .unwrap();
        assert_eq!(motion_dx(&control), 10);
    }

    #[test]
    fn adversarial_motion_counter_overflow_closes_activation() {
        let session = context(1, 1, 1);
        let mut receiver = receiver(900);
        enter(&mut receiver, session, 0);
        let frame = |sequence, total| MotionFrame {
            session,
            motion_sequence: MotionSequence(sequence),
            control_watermark: ControlSequence(1),
            sender_capture_time: MonotonicTimeMicros(sequence),
            totals: CumulativeMotion::new(total, 0, 0, 0),
            touch_snapshot: None,
        };
        receiver
            .receive_motion(frame(1, i64::MIN), MonotonicTimeMicros(1))
            .unwrap();
        let effects = receiver
            .receive_motion(frame(2, i64::MAX), MonotonicTimeMicros(2))
            .unwrap();

        assert!(effects.iter().any(|effect| matches!(
            effect,
            ReceiverEffect::ActivationClosed {
                reason: SessionCloseReason::MotionOverflow,
                ..
            }
        )));
        assert!(receiver.active_context().is_none());
    }

    #[test]
    fn future_watermark_motion_is_bounded_and_closes_the_activation() {
        let session = context(1, 1, 1);
        let mut receiver = receiver(900);
        enter(&mut receiver, session, 0);
        let mut final_effects = Vec::new();

        for sequence in 1..=(MAX_PENDING_MOTION_FRAMES as u64 + 1) {
            final_effects = receiver
                .receive_motion(
                    MotionFrame {
                        session,
                        motion_sequence: MotionSequence(sequence),
                        control_watermark: ControlSequence(2),
                        sender_capture_time: MonotonicTimeMicros(sequence),
                        totals: CumulativeMotion::new(sequence as i64, 0, 0, 0),
                        touch_snapshot: None,
                    },
                    MonotonicTimeMicros(sequence),
                )
                .unwrap();
        }

        assert!(final_effects.iter().any(|effect| matches!(
            effect,
            ReceiverEffect::ActivationClosed {
                reason: SessionCloseReason::ProtocolViolation,
                ..
            }
        )));
        assert!(receiver.active_context().is_none());
        assert!(receiver.is_closed(session.session_epoch, session.activation_id));
    }

    #[test]
    fn replacing_an_epoch_discards_old_epoch_tombstones() {
        let mut receiver = receiver(900);

        for epoch in 1..=200 {
            let session = context(epoch, u64::from(epoch), 1);
            enter(&mut receiver, session, u64::from(epoch));
            receiver
                .accepted_takeovers
                .insert((session.session_epoch, session.transport_generation));
            receiver
                .receive_control(
                    ReliableControlMessage {
                        session,
                        sequence: ControlSequence(2),
                        payload: ReliableControl::SessionClose {
                            reason: SessionCloseReason::LocalRelease,
                            final_anchor: None,
                        },
                    },
                    MonotonicTimeMicros(u64::from(epoch)),
                )
                .unwrap();

            assert_eq!(receiver.closed_activations.len(), 1);
            assert_eq!(receiver.accepted_takeovers.len(), 1);
            assert!(receiver.is_closed(session.session_epoch, session.activation_id));
        }
    }

    #[test]
    fn progressive_playout_updates_the_authoritative_injected_position() {
        let session = context(1, 1, 1);
        let mut receiver = receiver(900);
        enter(&mut receiver, session, 0);

        let first = receiver
            .receive_playout_step(
                session,
                PlayoutStep {
                    through_sequence: MotionSequence(1),
                    delta: MotionDelta {
                        dx: 8,
                        ..MotionDelta::default()
                    },
                    touch_snapshot: None,
                    target_reached: false,
                    pointer_catch_up_limited: false,
                    scroll_catch_up_limited: false,
                },
                MonotonicTimeMicros(1),
            )
            .unwrap();
        assert_eq!(motion_dx(&first), 8);
        assert_eq!(receiver.injected_totals().unwrap().total_dx(), 8);

        let final_step = receiver
            .receive_playout_step(
                session,
                PlayoutStep {
                    through_sequence: MotionSequence(1),
                    delta: MotionDelta {
                        dx: 5,
                        ..MotionDelta::default()
                    },
                    touch_snapshot: None,
                    target_reached: true,
                    pointer_catch_up_limited: false,
                    scroll_catch_up_limited: false,
                },
                MonotonicTimeMicros(2),
            )
            .unwrap();
        assert_eq!(motion_dx(&final_step), 5);
        assert_eq!(receiver.injected_totals().unwrap().total_dx(), 13);

        let anchored_click = receiver
            .receive_control(
                ReliableControlMessage {
                    session,
                    sequence: ControlSequence(2),
                    payload: ReliableControl::ButtonDown {
                        button: PointerButton::PRIMARY,
                        anchor: anchor(session, 1, 13),
                    },
                },
                MonotonicTimeMicros(3),
            )
            .unwrap();
        assert_eq!(motion_dx(&anchored_click), 0);
        assert!(
            anchored_click
                .iter()
                .any(|effect| matches!(effect, ReceiverEffect::Button { pressed: true, .. }))
        );
    }

    #[test]
    fn touch_datagram_cannot_start_a_touch_lifecycle() {
        let session = context(1, 1, 1);
        let mut receiver = receiver(900);
        enter(&mut receiver, session, 0);
        let touch = TouchState::new([crate::core::TouchContact {
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
        let effects = receiver
            .receive_motion(
                MotionFrame {
                    session,
                    motion_sequence: MotionSequence(1),
                    control_watermark: ControlSequence(1),
                    sender_capture_time: MonotonicTimeMicros(1),
                    totals: CumulativeMotion::ZERO,
                    touch_snapshot: Some(touch),
                },
                MonotonicTimeMicros(1),
            )
            .unwrap();
        assert!(!effects.iter().any(ReceiverEffect::is_injection));
        assert!(matches!(
            effects.as_slice(),
            [ReceiverEffect::ActivationClosed {
                reason: SessionCloseReason::ProtocolViolation,
                ..
            }]
        ));
    }

    #[test]
    fn delayed_snapshot_after_expiry_is_tombstoned() {
        let session = context(1, 1, 1);
        let mut receiver = receiver(100);
        enter(&mut receiver, session, 0);
        receiver
            .receive_control(
                ReliableControlMessage {
                    session,
                    sequence: ControlSequence(2),
                    payload: ReliableControl::KeyDown {
                        key: HidUsage::keyboard(4),
                    },
                },
                MonotonicTimeMicros(0),
            )
            .unwrap();
        receiver.tick(MonotonicTimeMicros(100_000)).unwrap();
        let effects = receiver
            .receive_control(
                ReliableControlMessage {
                    session,
                    sequence: ControlSequence(3),
                    payload: ReliableControl::StateSnapshot(StateSnapshot {
                        held: HeldState::default(),
                        motion_anchor: anchor(session, 0, 0),
                    }),
                },
                MonotonicTimeMicros(100_001),
            )
            .unwrap();
        assert!(!effects.iter().any(ReceiverEffect::is_injection));
        assert!(matches!(
            effects.as_slice(),
            [ReceiverEffect::Rejected {
                reason: RejectionReason::ClosedActivation,
                ..
            }]
        ));
    }

    #[test]
    fn takeover_accepts_exact_next_generation_and_rejects_old_transport() {
        let session = context(1, 1, 1);
        let mut receiver = receiver(900);
        enter(&mut receiver, session, 0);
        let next = SessionContext {
            transport_generation: TransportGeneration(2),
            ..session
        };
        let effects = receiver
            .receive_control(
                ReliableControlMessage {
                    session: next,
                    sequence: ControlSequence(2),
                    payload: ReliableControl::SessionTakeover(SessionTakeover {
                        prior_generation: TransportGeneration(1),
                        proposed_generation: TransportGeneration(2),
                        proposal_nonce: TakeoverNonce([7; 16]),
                        last_control_sequence: ControlSequence(1),
                        final_motion_anchor: anchor(session, 0, 0),
                        authoritative_held_state: HeldState::default(),
                    }),
                },
                MonotonicTimeMicros(1),
            )
            .unwrap();
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, ReceiverEffect::TakeoverAccepted { .. }))
        );
        let stale = receiver
            .receive_motion(
                MotionFrame {
                    session,
                    motion_sequence: MotionSequence(1),
                    control_watermark: ControlSequence(1),
                    sender_capture_time: MonotonicTimeMicros(2),
                    totals: CumulativeMotion::new(1, 0, 0, 0),
                    touch_snapshot: None,
                },
                MonotonicTimeMicros(2),
            )
            .unwrap();
        assert!(!stale.iter().any(ReceiverEffect::is_injection));
    }

    fn motion_dx(effects: &[ReceiverEffect]) -> i64 {
        effects
            .iter()
            .filter_map(|effect| match effect {
                ReceiverEffect::Motion { delta, .. } => Some(delta.dx),
                _ => None,
            })
            .sum()
    }
}
