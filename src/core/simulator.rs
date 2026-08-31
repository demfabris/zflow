//! Deterministic protocol failure simulator used by the headless CLI.
//!
//! This is intentionally a small executable specification, not a network
//! emulator. Each named fault is injected with fixed timing and the protocol
//! invariants are checked against receiver effects and sender deadlines.

use std::{collections::BTreeSet, time::Duration};

use thiserror::Error;

use super::{
    ActivationId, ActiveScroll, ContactId, ControlSequence, CumulativeMotion, HidUsage,
    MonotonicTimeMicros, MotionDelta, MotionFrame, MotionSequence, PointerButton, ProtocolVersion,
    Receiver, ReceiverConfig, ReceiverEffect, ReceiverError, ReceiverLifecycle, ReliableControl,
    ReliableControlMessage, ScrollId, ScrollPhase, ScrollSource, ScrollUnit, Sender, SenderConfig,
    SenderError, SenderTick, SessionContext, SessionEpoch, TakeoverNonce, TouchContact, TouchState,
    TouchTool, TransportGeneration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InjectedFault {
    IsolatedDatagramLoss,
    BurstDatagramLoss,
    DuplicateDatagram,
    ReorderedDatagram,
    JitterBurst40To150Ms,
    ControlDatagramCrossOrdering,
    FinalDatagramLossThenIdle,
    DelayedSnapshotAfterLeaseExpiry,
    SenderProcessDeath,
    ReceiverProcessDeath,
    SuspendResume,
    ConnectionReplacement,
    StaleEpoch,
    StaleGeneration,
    HeldKeyAndButtonFailureCleanup,
    BulkAndInputIsolation,
}

impl InjectedFault {
    const ALL: [Self; 16] = [
        Self::IsolatedDatagramLoss,
        Self::BurstDatagramLoss,
        Self::DuplicateDatagram,
        Self::ReorderedDatagram,
        Self::JitterBurst40To150Ms,
        Self::ControlDatagramCrossOrdering,
        Self::FinalDatagramLossThenIdle,
        Self::DelayedSnapshotAfterLeaseExpiry,
        Self::SenderProcessDeath,
        Self::ReceiverProcessDeath,
        Self::SuspendResume,
        Self::ConnectionReplacement,
        Self::StaleEpoch,
        Self::StaleGeneration,
        Self::HeldKeyAndButtonFailureCleanup,
        Self::BulkAndInputIsolation,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulationReport {
    pub exercised_faults: BTreeSet<InjectedFault>,
    pub assertions: usize,
    pub maximum_checkpoint_delay: Duration,
}

#[derive(Debug, Error)]
pub enum SimulationError {
    #[error("sender engine failed: {0}")]
    Sender(#[from] SenderError),
    #[error("receiver engine failed: {0}")]
    Receiver(#[from] ReceiverError),
    #[error("simulator invariant failed: {0}")]
    Invariant(&'static str),
    #[error("simulator did not exercise required fault: {0:?}")]
    MissingFault(InjectedFault),
}

#[derive(Debug, Default)]
struct Ledger {
    faults: BTreeSet<InjectedFault>,
    assertions: usize,
    maximum_checkpoint_delay: Duration,
}

impl Ledger {
    fn inject(&mut self, fault: InjectedFault) {
        self.faults.insert(fault);
    }

    fn ensure(&mut self, condition: bool, message: &'static str) -> Result<(), SimulationError> {
        self.assertions += 1;
        condition
            .then_some(())
            .ok_or(SimulationError::Invariant(message))
    }

    fn checkpoint_delay(&mut self, delay: Duration) {
        self.maximum_checkpoint_delay = self.maximum_checkpoint_delay.max(delay);
    }
}

pub fn run_prototype_scenario() -> Result<SimulationReport, SimulationError> {
    let mut ledger = Ledger::default();
    cumulative_loss_reordering_and_checkpoint(&mut ledger)?;
    lease_and_delayed_snapshot(&mut ledger)?;
    lifecycle_failures(&mut ledger)?;
    connection_and_epoch_replacement(&mut ledger)?;
    bulk_isolation(&mut ledger)?;

    for fault in InjectedFault::ALL {
        if !ledger.faults.contains(&fault) {
            return Err(SimulationError::MissingFault(fault));
        }
    }
    ledger.ensure(
        ledger.maximum_checkpoint_delay <= Duration::from_millis(250),
        "a cumulative checkpoint exceeded 250 ms",
    )?;

    Ok(SimulationReport {
        exercised_faults: ledger.faults,
        assertions: ledger.assertions,
        maximum_checkpoint_delay: ledger.maximum_checkpoint_delay,
    })
}

fn cumulative_loss_reordering_and_checkpoint(ledger: &mut Ledger) -> Result<(), SimulationError> {
    let session = context(1, 1, 1);
    let mut sender = Sender::new(
        SenderConfig::new(Duration::from_millis(250), Duration::from_millis(900))?,
        session,
        time(0),
    )?;
    let mut receiver = Receiver::new(ReceiverConfig::new(Duration::from_millis(900))?, time(0))?;
    receiver.authorize_session(session, time(0))?;
    let enter = sender.enter(time(0))?;
    receiver.receive_control(enter, time(0))?;

    // seq 1 is isolated loss; seq 2 and 3 arrive in reverse order after fixed
    // delays at the edges of the required 40-150 ms jitter burst.
    let lost_one = sender.capture_motion(delta_x(10), None, time(1_000))?;
    ledger.inject(InjectedFault::IsolatedDatagramLoss);
    let reordered_two = sender.capture_motion(delta_x(10), None, time(2_000))?;
    let newest_three = sender.capture_motion(delta_x(10), None, time(3_000))?;
    ledger.inject(InjectedFault::ReorderedDatagram);
    ledger.inject(InjectedFault::JitterBurst40To150Ms);
    let effects = receiver.receive_motion(newest_three.clone(), time(43_000))?;
    ledger.ensure(
        motion_dx(&effects) == 30,
        "latest cumulative datagram did not repair isolated loss",
    )?;
    let effects = receiver.receive_motion(reordered_two.clone(), time(152_000))?;
    ledger.ensure(
        !effects.iter().any(ReceiverEffect::is_injection),
        "reordered older datagram injected after a newer cumulative frame",
    )?;
    ledger.ensure(
        newest_three.sender_capture_time.0 + 40_000 == 43_000
            && reordered_two.sender_capture_time.0 + 150_000 == 152_000
            && lost_one.motion_sequence == MotionSequence(1),
        "jitter schedule does not span the deterministic 40-150 ms edges",
    )?;

    ledger.inject(InjectedFault::DuplicateDatagram);
    let duplicate = receiver.receive_motion(newest_three, time(153_000))?;
    ledger.ensure(
        !duplicate.iter().any(ReceiverEffect::is_injection),
        "duplicate datagram injected twice",
    )?;

    // Frames 4 and 5 disappear as a burst. Frame 6 recovers their totals.
    let _lost_four = sender.capture_motion(delta_x(10), None, time(154_000))?;
    let _lost_five = sender.capture_motion(delta_x(10), None, time(155_000))?;
    ledger.inject(InjectedFault::BurstDatagramLoss);
    let six = sender.capture_motion(delta_x(10), None, time(156_000))?;
    let effects = receiver.receive_motion(six, time(246_000))?;
    ledger.ensure(
        motion_dx(&effects) == 30,
        "latest cumulative datagram did not repair burst loss",
    )?;

    // A post-key motion sample reaches the datagram path before its control
    // watermark reaches the reliable path. It must remain queued.
    let key = sender.key_down(HidUsage::keyboard(4), time(247_000))?;
    let seven = sender.capture_motion(delta_x(10), None, time(248_000))?;
    let SenderTick::Checkpoint(periodic_checkpoint) = sender.tick(time(250_000))? else {
        return Err(SimulationError::Invariant(
            "periodic checkpoint was not enqueued at 250 ms",
        ));
    };
    ledger.checkpoint_delay(Duration::from_millis(249));
    ledger.inject(InjectedFault::ControlDatagramCrossOrdering);
    let early = receiver.receive_motion(seven, time(288_000))?;
    ledger.ensure(
        !early.iter().any(ReceiverEffect::is_injection),
        "motion overtook its control watermark",
    )?;
    let effects = receiver.receive_control(key, time(289_000))?;
    ledger.ensure(
        effects
            .iter()
            .position(|effect| matches!(effect, ReceiverEffect::Key { pressed: true, .. }))
            < effects
                .iter()
                .position(|effect| matches!(effect, ReceiverEffect::Motion { .. })),
        "cross-ordered motion did not wait for its semantic watermark",
    )?;
    let checkpoint_effects = receiver.receive_control(*periodic_checkpoint, time(289_500))?;
    acknowledge_effects(&mut sender, &checkpoint_effects)?;

    // The click's datagram is lost. Its reliable anchor must repair position
    // before the button transition.
    let _lost_eight = sender.capture_motion(delta_x(10), None, time(290_000))?;
    let click = sender.button_down(PointerButton::PRIMARY, time(291_000))?;
    let click_effects = receiver.receive_control(click, time(331_000))?;
    let motion = click_effects
        .iter()
        .position(|effect| matches!(effect, ReceiverEffect::Motion { .. }));
    let button = click_effects
        .iter()
        .position(|effect| matches!(effect, ReceiverEffect::Button { pressed: true, .. }));
    ledger.ensure(
        motion.is_some() && motion < button,
        "button transition overtook its motion anchor",
    )?;

    // The final datagram is lost and the source becomes idle. The periodic
    // reliable snapshot is the only repair path.
    let final_capture_at = time(332_000);
    let _lost_final = sender.capture_motion(delta_x(7), None, final_capture_at)?;
    ledger.inject(InjectedFault::FinalDatagramLossThenIdle);
    let checkpoint_at = sender.next_deadline().ok_or(SimulationError::Invariant(
        "sender exposed no checkpoint deadline",
    ))?;
    let delay = Duration::from_micros(checkpoint_at.0 - final_capture_at.0);
    ledger.checkpoint_delay(delay);
    let SenderTick::Checkpoint(checkpoint) = sender.tick(checkpoint_at)? else {
        return Err(SimulationError::Invariant(
            "idle final loss did not enqueue a checkpoint",
        ));
    };
    let checkpoint_effects = receiver.receive_control(*checkpoint, checkpoint_at)?;
    ledger.ensure(
        receiver.injected_totals() == Some(sender.totals()),
        "reliable checkpoint did not repair final datagram loss",
    )?;
    acknowledge_effects(&mut sender, &checkpoint_effects)?;
    ledger.ensure(
        sender.last_acknowledged_checkpoint().is_some(),
        "sender did not preserve its last acknowledged checkpoint",
    )?;
    Ok(())
}

fn lease_and_delayed_snapshot(ledger: &mut Ledger) -> Result<(), SimulationError> {
    let session = context(2, 1, 1);
    let mut sender = Sender::new(
        SenderConfig::new(Duration::from_millis(30), Duration::from_millis(100))?,
        session,
        time(0),
    )?;
    let mut receiver = Receiver::new(ReceiverConfig::new(Duration::from_millis(100))?, time(0))?;
    receiver.authorize_session(session, time(0))?;
    receiver.receive_control(sender.enter(time(0))?, time(0))?;
    receiver.receive_control(
        sender.key_down(HidUsage::keyboard(5), time(1_000))?,
        time(1_000),
    )?;
    receiver.receive_control(
        sender.button_down(PointerButton::SECONDARY, time(2_000))?,
        time(2_000),
    )?;
    let scroll = ActiveScroll {
        id: ScrollId(1),
        source: ScrollSource {
            unit: ScrollUnit::Pixel,
            resolution_x: None,
            resolution_y: None,
        },
        phase: ScrollPhase::Begin,
        momentum_phase: None,
    };
    receiver.receive_control(sender.scroll_begin(scroll, time(3_000))?, time(3_000))?;
    let touch = one_finger();
    receiver.receive_control(sender.touch_begin(touch, time(4_000))?, time(4_000))?;

    let delayed = sender.snapshot(time(30_000))?;
    let expired = receiver.tick(time(104_000))?;
    ledger.inject(InjectedFault::HeldKeyAndButtonFailureCleanup);
    ledger.ensure(
        expired.iter().any(|effect| {
            matches!(
                effect,
                ReceiverEffect::Key {
                    pressed: false,
                    synthetic: true,
                    ..
                }
            )
        }) && expired.iter().any(|effect| {
            matches!(
                effect,
                ReceiverEffect::Button {
                    pressed: false,
                    synthetic: true,
                    ..
                }
            )
        }) && expired
            .iter()
            .any(|effect| matches!(effect, ReceiverEffect::ScrollEnded { .. }))
            && expired.iter().any(|effect| {
                matches!(
                    effect,
                    ReceiverEffect::TouchReplaced { state, .. } if state.is_empty()
                )
            }),
        "lease expiry did not clean up every held input class",
    )?;
    ledger.ensure(
        receiver.is_closed(session.session_epoch, session.activation_id),
        "lease expiry did not tombstone the activation",
    )?;

    ledger.inject(InjectedFault::DelayedSnapshotAfterLeaseExpiry);
    let late = receiver.receive_control(delayed, time(105_000))?;
    ledger.ensure(
        !late.iter().any(ReceiverEffect::is_injection),
        "delayed snapshot resurrected a lease-expired activation",
    )?;
    Ok(())
}

fn lifecycle_failures(ledger: &mut Ledger) -> Result<(), SimulationError> {
    // Sender process death is observed by the receiver as connection loss.
    let session = context(3, 1, 1);
    let (mut sender, mut receiver) = active_pair(session, Duration::from_millis(900))?;
    hold_key_and_button(&mut sender, &mut receiver, 1_000)?;
    drop(sender);
    ledger.inject(InjectedFault::SenderProcessDeath);
    let effects = receiver.lifecycle(ReceiverLifecycle::ConnectionLost, time(3_000))?;
    ensure_key_button_release(ledger, &effects, "sender death left receiver input held")?;

    // Receiver death/back-end teardown must synthesize cleanup before the
    // fresh receiver process accepts anything.
    let session = context(4, 1, 1);
    let (mut sender, mut receiver) = active_pair(session, Duration::from_millis(900))?;
    hold_key_and_button(&mut sender, &mut receiver, 1_000)?;
    ledger.inject(InjectedFault::ReceiverProcessDeath);
    let effects = receiver.lifecycle(ReceiverLifecycle::ProcessDeath, time(3_000))?;
    ensure_key_button_release(ledger, &effects, "receiver death left backend input held")?;
    drop(receiver);
    let replacement = Receiver::new(
        ReceiverConfig::new(Duration::from_millis(900))?,
        time(3_000),
    )?;
    ledger.ensure(
        replacement.active_context().is_none(),
        "replacement receiver inherited dead process state",
    )?;

    // Both suspend and resume close ownership. Resume is idempotent and the old
    // activation remains tombstoned.
    let session = context(5, 1, 1);
    let (mut sender, mut receiver) = active_pair(session, Duration::from_millis(900))?;
    hold_key_and_button(&mut sender, &mut receiver, 1_000)?;
    ledger.inject(InjectedFault::SuspendResume);
    let effects = receiver.lifecycle(ReceiverLifecycle::Suspend, time(3_000))?;
    ensure_key_button_release(ledger, &effects, "suspend left receiver input held")?;
    let resumed = receiver.lifecycle(ReceiverLifecycle::Resume, time(4_000))?;
    ledger.ensure(
        resumed.is_empty(),
        "resume emitted effects for a closed activation",
    )?;
    let stale = receiver.receive_motion(
        sender.capture_motion(delta_x(1), None, time(5_000))?,
        time(5_000),
    )?;
    ledger.ensure(
        !stale.iter().any(ReceiverEffect::is_injection),
        "resume accepted traffic from the pre-suspend activation",
    )?;
    Ok(())
}

fn connection_and_epoch_replacement(ledger: &mut Ledger) -> Result<(), SimulationError> {
    let old = context(6, 1, 1);
    let (mut sender, mut receiver) = active_pair(old, Duration::from_millis(900))?;
    hold_key_and_button(&mut sender, &mut receiver, 1_000)?;

    let next = SessionContext {
        transport_generation: TransportGeneration(2),
        ..old
    };
    let takeover = sender.propose_takeover(
        next.transport_generation,
        TakeoverNonce([9; 16]),
        time(4_000),
    )?;
    ledger.inject(InjectedFault::ConnectionReplacement);
    let accepted = receiver.receive_control(takeover, time(4_000))?;
    ledger.ensure(
        accepted
            .iter()
            .any(|effect| matches!(effect, ReceiverEffect::TakeoverAccepted { .. })),
        "exact next-generation takeover was not accepted",
    )?;
    let takeover_ack = accepted.iter().find_map(|effect| match effect {
        ReceiverEffect::TakeoverAccepted { accepted, .. } => Some(*accepted),
        _ => None,
    });
    sender.accept_takeover(takeover_ack.ok_or(SimulationError::Invariant(
        "takeover acceptance effect did not carry its acknowledgement",
    ))?)?;

    ledger.inject(InjectedFault::StaleGeneration);
    let stale_generation = receiver.receive_motion(
        MotionFrame {
            session: old,
            motion_sequence: MotionSequence(1),
            control_watermark: ControlSequence(1),
            sender_capture_time: time(5_000),
            totals: CumulativeMotion::new(999, 0, 0, 0),
            touch_snapshot: None,
        },
        time(5_000),
    )?;
    ledger.ensure(
        !stale_generation.iter().any(ReceiverEffect::is_injection),
        "old transport generation injected after replacement",
    )?;

    let new_epoch = context(7, 1, 1);
    ledger.inject(InjectedFault::StaleEpoch);
    let replacement_effects = receiver.authorize_session(new_epoch, time(6_000))?;
    ensure_key_button_release(
        ledger,
        &replacement_effects,
        "epoch replacement left receiver input held",
    )?;
    receiver.receive_control(
        ReliableControlMessage {
            session: new_epoch,
            sequence: ControlSequence(1),
            payload: ReliableControl::Enter,
        },
        time(6_000),
    )?;
    let stale_epoch = receiver.receive_motion(
        MotionFrame {
            session: next,
            motion_sequence: MotionSequence(2),
            control_watermark: ControlSequence(1),
            sender_capture_time: time(7_000),
            totals: CumulativeMotion::new(999, 0, 0, 0),
            touch_snapshot: None,
        },
        time(7_000),
    )?;
    ledger.ensure(
        !stale_epoch.iter().any(ReceiverEffect::is_injection),
        "old epoch injected after authenticated epoch replacement",
    )?;
    Ok(())
}

fn bulk_isolation(ledger: &mut Ledger) -> Result<(), SimulationError> {
    // Two independent deterministic queues model the architectural invariant:
    // input has its own connection/socket, so a blocked bulk serializer and
    // congestion window never become predecessors of input work.
    #[derive(Debug, Clone, Copy)]
    struct Queue {
        available_at: u64,
    }
    impl Queue {
        fn submit(&mut self, at: u64, service_time: u64) -> u64 {
            let starts = self.available_at.max(at);
            self.available_at = starts + service_time;
            self.available_at
        }
    }

    let mut bulk = Queue { available_at: 0 };
    let mut input = Queue { available_at: 0 };
    let bulk_done = bulk.submit(0, 10_000_000);
    let control_done = input.submit(1_000, 100);
    let datagram_done = input.submit(1_100, 1);
    ledger.inject(InjectedFault::BulkAndInputIsolation);
    ledger.ensure(
        control_done < bulk_done && datagram_done < bulk_done,
        "bulk serialization or congestion blocked the input connection",
    )?;
    Ok(())
}

fn active_pair(
    session: SessionContext,
    lease: Duration,
) -> Result<(Sender, Receiver), SimulationError> {
    let mut sender = Sender::new(
        SenderConfig::new(Duration::from_millis(250), lease)?,
        session,
        time(0),
    )?;
    let mut receiver = Receiver::new(ReceiverConfig::new(lease)?, time(0))?;
    receiver.authorize_session(session, time(0))?;
    receiver.receive_control(sender.enter(time(0))?, time(0))?;
    Ok((sender, receiver))
}

fn hold_key_and_button(
    sender: &mut Sender,
    receiver: &mut Receiver,
    at: u64,
) -> Result<(), SimulationError> {
    receiver.receive_control(sender.key_down(HidUsage::keyboard(4), time(at))?, time(at))?;
    receiver.receive_control(
        sender.button_down(PointerButton::PRIMARY, time(at + 1))?,
        time(at + 1),
    )?;
    Ok(())
}

fn ensure_key_button_release(
    ledger: &mut Ledger,
    effects: &[ReceiverEffect],
    message: &'static str,
) -> Result<(), SimulationError> {
    ledger.ensure(
        effects.iter().any(|effect| {
            matches!(
                effect,
                ReceiverEffect::Key {
                    pressed: false,
                    synthetic: true,
                    ..
                }
            )
        }) && effects.iter().any(|effect| {
            matches!(
                effect,
                ReceiverEffect::Button {
                    pressed: false,
                    synthetic: true,
                    ..
                }
            )
        }),
        message,
    )
}

fn acknowledge_effects(
    sender: &mut Sender,
    effects: &[ReceiverEffect],
) -> Result<(), SimulationError> {
    for effect in effects {
        if let ReceiverEffect::SnapshotAck { ack, .. } = effect {
            sender.acknowledge_snapshot(*ack)?;
        }
    }
    Ok(())
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

fn context(epoch: u8, generation: u64, activation: u64) -> SessionContext {
    SessionContext {
        protocol_version: ProtocolVersion(1),
        session_epoch: SessionEpoch([epoch; 16]),
        transport_generation: TransportGeneration(generation),
        activation_id: ActivationId(activation),
    }
}

fn time(micros: u64) -> MonotonicTimeMicros {
    MonotonicTimeMicros(micros)
}

fn delta_x(dx: i64) -> MotionDelta {
    MotionDelta {
        dx,
        ..MotionDelta::default()
    }
}

fn one_finger() -> TouchState {
    TouchState::new([TouchContact {
        id: ContactId(1),
        x: 100,
        y: 200,
        pressure: Some(400),
        major: Some(10),
        minor: Some(8),
        orientation_millidegrees: Some(0),
        tool: TouchTool::Finger,
        source_dimensions: None,
    }])
    .expect("one contact has a unique id")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{AnchorKind, HeldState, MotionAnchor, SessionCloseReason, StateSnapshot};

    #[test]
    fn prototype_scenario_exercises_every_required_fault() {
        let report = run_prototype_scenario().unwrap();
        assert_eq!(report.exercised_faults.len(), InjectedFault::ALL.len());
        assert!(report.assertions >= 20);
        assert!(report.maximum_checkpoint_delay <= Duration::from_millis(250));
    }

    #[test]
    fn sender_ack_timeout_is_also_deterministic() {
        let session = context(8, 1, 1);
        let mut sender = Sender::new(
            SenderConfig::new(Duration::from_millis(250), Duration::from_millis(900)).unwrap(),
            session,
            time(0),
        )
        .unwrap();
        sender.enter(time(0)).unwrap();
        sender.key_down(HidUsage::keyboard(4), time(1_000)).unwrap();
        let SenderTick::Checkpoint(snapshot) = sender.tick(time(250_000)).unwrap() else {
            panic!("renewal snapshot missing");
        };
        assert!(matches!(
            snapshot.payload,
            ReliableControl::StateSnapshot(_)
        ));
        assert_eq!(
            sender.tick(time(1_150_000)).unwrap(),
            SenderTick::ExitRemote(super::super::SenderExitReason::SnapshotAckTimedOut)
        );
    }

    #[test]
    fn terminal_close_message_cleans_up_without_waiting_for_lease() {
        let session = context(9, 1, 1);
        let (mut sender, mut receiver) = active_pair(session, Duration::from_millis(900)).unwrap();
        hold_key_and_button(&mut sender, &mut receiver, 1_000).unwrap();
        let close = sender
            .leave(SessionCloseReason::LocalRelease, time(2_000))
            .unwrap();
        let effects = receiver.receive_control(close, time(2_000)).unwrap();
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, ReceiverEffect::ActivationClosed { .. }))
        );
        assert!(receiver.active_context().is_none());
    }

    #[test]
    fn stale_manual_snapshot_has_no_ack_or_injection() {
        let session = context(10, 1, 1);
        let mut receiver = Receiver::new(
            ReceiverConfig::new(Duration::from_millis(100)).unwrap(),
            time(0),
        )
        .unwrap();
        receiver.authorize_session(session, time(0)).unwrap();
        receiver
            .receive_control(
                ReliableControlMessage {
                    session,
                    sequence: ControlSequence(1),
                    payload: ReliableControl::Enter,
                },
                time(0),
            )
            .unwrap();
        receiver
            .lifecycle(ReceiverLifecycle::ConnectionLost, time(1))
            .unwrap();
        let effects = receiver
            .receive_control(
                ReliableControlMessage {
                    session,
                    sequence: ControlSequence(2),
                    payload: ReliableControl::StateSnapshot(StateSnapshot {
                        held: HeldState::default(),
                        motion_anchor: MotionAnchor {
                            activation_id: session.activation_id,
                            through_motion_sequence: MotionSequence(0),
                            sender_capture_time: time(0),
                            totals: CumulativeMotion::ZERO,
                            final_touch_state: TouchState::default(),
                            kind: AnchorKind::Checkpoint,
                        },
                    }),
                },
                time(2),
            )
            .unwrap();
        assert!(!effects.iter().any(ReceiverEffect::is_injection));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ReceiverEffect::SnapshotAck { .. }))
        );
    }
}
