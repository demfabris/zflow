//! Pure receiver-side ordering, reconciliation, and failure recovery.
//!
//! Authentication and playout scheduling live outside this type. Each input
//! connection gets its own receiver. The caller supplies monotonic time; the
//! receiver only emits backend-neutral effects.

use std::{collections::BTreeSet, time::Duration};

use thiserror::Error;

use super::{
    ActivationId, ActiveScroll, AnchorKind, ControlSequence, CumulativeMotion, HeldState, HidUsage,
    Modifier, MonotonicTimeMicros, MotionAnchor, MotionDelta, MotionSequence, PlayoutStep,
    PointerButton, ProtocolVersion, ReliableControl, ReliableControlMessage, ScrollId,
    SessionCloseReason, SessionContext, SessionEpoch, SnapshotAck, TouchState, TransportGeneration,
};

const MAX_HELD_STATE_LEASE: Duration = Duration::from_secs(1);

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
    ClosedActivation,
    StaleActivation,
    NoOpenActivation,
    DuplicateControl,
    ControlGap,
    DuplicateOrReorderedMotion,
    AnchorActivationMismatch,
    AnchorMovedBackwards,
    InvalidTransition,
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
    #[error("the peer changed protocol, epoch, or generation within one connection")]
    ContextChanged,
}

#[derive(Debug, Clone)]
struct ActivationState {
    session: SessionContext,
    last_control_sequence: ControlSequence,
    held: HeldState,
    last_injected_motion: MotionSequence,
    injected_totals: CumulativeMotion,
    lease_deadline: Option<MonotonicTimeMicros>,
}

impl ActivationState {
    fn new(session: SessionContext) -> Self {
        Self {
            session,
            last_control_sequence: ControlSequence(0),
            held: HeldState::default(),
            last_injected_motion: MotionSequence(0),
            injected_totals: CumulativeMotion::ZERO,
            lease_deadline: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Receiver {
    config: ReceiverConfig,
    /// Taken from the first control message. One connection carries one.
    context: Option<(ProtocolVersion, SessionEpoch, TransportGeneration)>,
    highest_activation: Option<ActivationId>,
    activation: Option<ActivationState>,
    closed_activations: BTreeSet<ActivationId>,
    last_observed_time: MonotonicTimeMicros,
}

impl Receiver {
    pub fn new(config: ReceiverConfig, now: MonotonicTimeMicros) -> Result<Self, ReceiverError> {
        let config = ReceiverConfig::new(config.held_state_lease)?;
        Ok(Self {
            config,
            context: None,
            highest_activation: None,
            activation: None,
            closed_activations: BTreeSet::new(),
            last_observed_time: now,
        })
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

    pub fn receive_control(
        &mut self,
        message: ReliableControlMessage,
        now: MonotonicTimeMicros,
    ) -> Result<Vec<ReceiverEffect>, ReceiverError> {
        self.observe_time(now)?;
        // Checked before the lease: an error must leave held state for the
        // caller's connection-loss cleanup.
        self.check_context(message.session)?;
        let mut effects = Vec::new();
        self.expire_lease(now, &mut effects);

        if matches!(message.payload, ReliableControl::Enter) {
            self.receive_enter(message, now, &mut effects);
            return Ok(effects);
        }

        if self
            .closed_activations
            .contains(&message.session.activation_id)
        {
            effects.push(rejected(message.session, RejectionReason::ClosedActivation));
            return Ok(effects);
        }

        let Some(state) = self.activation.as_ref() else {
            effects.push(rejected(message.session, RejectionReason::NoOpenActivation));
            return Ok(effects);
        };
        if state.session.activation_id != message.session.activation_id {
            effects.push(rejected(message.session, RejectionReason::StaleActivation));
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
                    effects.push(ReceiverEffect::SnapshotAck {
                        session: state.session,
                        ack: SnapshotAck {
                            snapshot_sequence: message.sequence,
                            accepted_generation: state.session.transport_generation,
                        },
                    });
                }
                ReliableControl::SessionClose { reason, .. } => close = Some(*reason),
                ReliableControl::SnapshotAck(_) => {
                    effects.push(rejected(message.session, RejectionReason::WrongDirection));
                    return Ok(effects);
                }
                ReliableControl::Enter => unreachable!(),
            }

            state.last_control_sequence = message.sequence;
            refresh_lease(state, now, self.config.held_state_lease);
        }

        if let Some(reason) = close {
            self.close_activation(reason, &mut effects);
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
        if self.closed_activations.contains(&session.activation_id) {
            effects.push(rejected(session, RejectionReason::ClosedActivation));
            return Ok(effects);
        }
        let Some(state) = self.activation.as_mut() else {
            effects.push(rejected(session, RejectionReason::NoOpenActivation));
            return Ok(effects);
        };
        if state.session != session {
            effects.push(rejected(session, RejectionReason::StaleActivation));
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
            .contains(&message.session.activation_id)
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
            ReliableControl::Enter | ReliableControl::SnapshotAck(_) => false,
        }
    }

    fn check_context(&mut self, session: SessionContext) -> Result<(), ReceiverError> {
        let context = (
            session.protocol_version,
            session.session_epoch,
            session.transport_generation,
        );
        match self.context {
            None => self.context = Some(context),
            Some(pinned) if pinned != context => return Err(ReceiverError::ContextChanged),
            Some(_) => {}
        }
        Ok(())
    }

    fn close_activation(&mut self, reason: SessionCloseReason, effects: &mut Vec<ReceiverEffect>) {
        let Some(mut state) = self.activation.take() else {
            return;
        };
        release_all(&mut state, effects);
        self.closed_activations.insert(state.session.activation_id);
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
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;
    use crate::core::{Sender, SenderConfig, SenderTick, StateSnapshot};

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
            prop_assert!(receiver.closed_activations.contains(&session.activation_id));
        }

        /// Required property: a closed activation never injects again, and a
        /// connection cannot switch epoch or generation.
        #[test]
        fn epoch_generation_activation_ordering_rejects_stale(
            epochs in (any::<u8>(), any::<u8>()).prop_filter(
                "different epochs",
                |(old, new)| old != new,
            ),
            generation in 1_u64..u64::MAX,
            activation in 1_u64..u64::MAX,
        ) {
            let (epoch, other_epoch) = epochs;
            let old = context(epoch, generation, activation);
            let new = SessionContext { activation_id: ActivationId(activation + 1), ..old };
            let mut receiver = receiver(900);
            enter(&mut receiver, old, 0);
            receiver.receive_control(
                ReliableControlMessage {
                    session: old,
                    sequence: ControlSequence(2),
                    payload: ReliableControl::SessionClose {
                        reason: SessionCloseReason::LocalRelease,
                        final_anchor: None,
                    },
                },
                MonotonicTimeMicros(1),
            ).unwrap();
            enter(&mut receiver, new, 1);

            let effects = receiver
                .receive_playout_step(old, step(1, 99), MonotonicTimeMicros(2))
                .unwrap();
            prop_assert!(!effects.iter().any(ReceiverEffect::is_injection));
            let effects = receiver.receive_control(
                ReliableControlMessage {
                    session: old,
                    sequence: ControlSequence(3),
                    payload: ReliableControl::KeyDown { key: HidUsage::keyboard(4) },
                },
                MonotonicTimeMicros(2),
            ).unwrap();
            prop_assert!(!effects.iter().any(ReceiverEffect::is_injection));

            for changed in [
                SessionContext { session_epoch: SessionEpoch([other_epoch; 16]), ..new },
                SessionContext { transport_generation: TransportGeneration(generation - 1), ..new },
                SessionContext { transport_generation: TransportGeneration(generation + 1), ..new },
            ] {
                let result = receiver.receive_control(
                    ReliableControlMessage {
                        session: changed,
                        sequence: ControlSequence(2),
                        payload: ReliableControl::KeyDown { key: HidUsage::keyboard(4) },
                    },
                    MonotonicTimeMicros(3),
                );
                prop_assert_eq!(result, Err(ReceiverError::ContextChanged));
                prop_assert_eq!(receiver.active_context(), Some(new));
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
    fn context_error_at_deadline_preserves_state_for_cleanup() {
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
                .receive_control(
                    ReliableControlMessage {
                        session: invalid,
                        sequence: ControlSequence(3),
                        payload: ReliableControl::KeyUp { key },
                    },
                    MonotonicTimeMicros(10_000),
                )
                .unwrap_err(),
            ReceiverError::ContextChanged
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
    fn adversarial_motion_counter_overflow_closes_activation() {
        let session = context(1, 1, 1);
        let mut receiver = receiver(900);
        enter(&mut receiver, session, 0);
        receiver
            .receive_playout_step(session, step(1, i64::MAX), MonotonicTimeMicros(1))
            .unwrap();
        let effects = receiver
            .receive_playout_step(session, step(2, 1), MonotonicTimeMicros(2))
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
    fn progressive_playout_updates_the_authoritative_injected_position() {
        let session = context(1, 1, 1);
        let mut receiver = receiver(900);
        enter(&mut receiver, session, 0);

        let first = receiver
            .receive_playout_step(
                session,
                PlayoutStep {
                    mapped_capture_time: MonotonicTimeMicros(0),
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
                    mapped_capture_time: MonotonicTimeMicros(0),
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
        let effects = receiver
            .receive_playout_step(
                session,
                PlayoutStep {
                    touch_snapshot: Some(one_finger()),
                    ..step(1, 0)
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
    fn lease_expiry_releases_keys_buttons_and_touch() {
        let session = context(1, 1, 1);
        let mut receiver = receiver(100);
        enter(&mut receiver, session, 0);
        for (sequence, payload) in [
            (
                2,
                ReliableControl::KeyDown {
                    key: HidUsage::keyboard(5),
                },
            ),
            (
                3,
                ReliableControl::ButtonDown {
                    button: PointerButton::SECONDARY,
                    anchor: anchor(session, 0, 0),
                },
            ),
            (
                4,
                ReliableControl::TouchBegin {
                    initial_state: one_finger(),
                },
            ),
        ] {
            receiver
                .receive_control(
                    ReliableControlMessage {
                        session,
                        sequence: ControlSequence(sequence),
                        payload,
                    },
                    MonotonicTimeMicros(0),
                )
                .unwrap();
        }

        let effects = receiver.tick(MonotonicTimeMicros(100_000)).unwrap();
        assert!(effects.iter().any(|effect| matches!(
            effect,
            ReceiverEffect::Key {
                pressed: false,
                synthetic: true,
                ..
            }
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            ReceiverEffect::Button {
                pressed: false,
                synthetic: true,
                ..
            }
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            ReceiverEffect::TouchReplaced { state, synthetic: true } if state.is_empty()
        )));
        assert!(receiver.active_context().is_none());
    }

    #[test]
    fn idle_checkpoint_repairs_a_lost_final_datagram() {
        let session = context(1, 1, 1);
        let mut sender = Sender::new(
            SenderConfig::new(Duration::from_millis(250), Duration::from_millis(900)).unwrap(),
            session,
            MonotonicTimeMicros(0),
        )
        .unwrap();
        let mut receiver = receiver(900);
        enter(&mut receiver, session, 0);
        sender.enter(MonotonicTimeMicros(0)).unwrap();

        // The only datagram is lost and the source goes idle.
        let captured_at = MonotonicTimeMicros(1_000);
        sender
            .capture_motion(
                MotionDelta {
                    dx: 7,
                    ..MotionDelta::default()
                },
                None,
                captured_at,
            )
            .unwrap();
        let checkpoint_at = sender.next_deadline().unwrap();
        assert!(checkpoint_at.0 - captured_at.0 <= 250_000);
        let SenderTick::Checkpoint(checkpoint) = sender.tick(checkpoint_at).unwrap() else {
            panic!("idle loss did not enqueue a checkpoint");
        };

        let effects = receiver
            .receive_control(*checkpoint, checkpoint_at)
            .unwrap();
        assert_eq!(motion_dx(&effects), 7);
        assert_eq!(
            receiver.activation.as_ref().unwrap().injected_totals,
            CumulativeMotion::new(7, 0, 0, 0)
        );
    }

    fn one_finger() -> TouchState {
        TouchState::new([crate::core::TouchContact {
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
        .unwrap()
    }

    fn step(through: u64, dx: i64) -> PlayoutStep {
        PlayoutStep {
            mapped_capture_time: MonotonicTimeMicros(0),
            through_sequence: MotionSequence(through),
            delta: MotionDelta {
                dx,
                ..MotionDelta::default()
            },
            touch_snapshot: None,
            target_reached: true,
            pointer_catch_up_limited: false,
            scroll_catch_up_limited: false,
        }
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
