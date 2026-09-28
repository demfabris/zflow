//! Keeps this Mac's input going one way at a time: out to a peer while this
//! Mac sends, or in from one peer's session while that peer controls it.

// Nothing takes input from a peer yet. Remove once receiving is wired in.
#![cfg_attr(not(test), allow(dead_code))]

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Result, bail};
use tokio::sync::mpsc;

use crate::{
    config::PeerPermissions,
    core::ReceiverEffect,
    desktop::DesktopResponse,
    session::{SessionEvent, SessionEventKind},
};

// The Linux daemon's words for the same refusals.
pub(crate) const SENDING: &str = "This computer is sending its own input";
pub(crate) const OWNED: &str = "Another computer owns input";
pub(crate) const HANDOFF_ACTIVE: &str = "A desktop handoff is already active";

#[derive(Default)]
struct State {
    /// A crossing to a peer holds this Mac's input.
    outbound: bool,
    /// The peer and session controlling this Mac, and the claim that holds it.
    inbound: Option<(String, u64, u64)>,
    /// The peer and session holding a desktop handoff.
    lease: Option<(String, u64)>,
    claims: u64,
}

impl State {
    /// Another peer or session controls this Mac or holds its desktop.
    fn taken(&self, peer: &str, session_id: u64) -> bool {
        let other = |owner: &str, id: u64| owner != peer || id != session_id;
        self.inbound
            .as_ref()
            .is_some_and(|(owner, id, _)| other(owner, *id))
            || self
                .lease
                .as_ref()
                .is_some_and(|(owner, id)| other(owner, *id))
    }

    fn check(&self, peer: &str, session_id: u64) -> Result<(), &'static str> {
        if self.outbound {
            Err(SENDING)
        } else if self.taken(peer, session_id) {
            Err(OWNED)
        } else {
            Ok(())
        }
    }
}

/// Who owns this Mac's input. Link tasks, the handoff server and the app
/// thread share it, and nobody holds it across an await.
#[derive(Clone, Default)]
pub(crate) struct Ownership(Arc<Mutex<State>>);

impl Ownership {
    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reserves this Mac's input for a crossing, from the edge until input
    /// is back. None while a peer controls this Mac or holds a desktop
    /// handoff, or another crossing runs.
    pub fn begin_outbound(&self) -> Option<OutboundGuard> {
        let mut state = self.state();
        if state.outbound || state.inbound.is_some() || state.lease.is_some() {
            return None;
        }
        state.outbound = true;
        Some(OutboundGuard(self.clone()))
    }

    /// Gives this Mac's input to `peer`'s session for an activation. None
    /// while this Mac sends, or another peer or session controls it or holds
    /// its desktop. The same session may claim again, and the newest claim
    /// holds it.
    pub fn claim_inbound(&self, peer: &str, session_id: u64) -> Option<InboundClaim> {
        let mut state = self.state();
        if state.check(peer, session_id).is_err() {
            return None;
        }
        state.claims += 1;
        let claim = state.claims;
        state.inbound = Some((peer.to_owned(), session_id, claim));
        Some(InboundClaim {
            ownership: self.clone(),
            claim,
        })
    }

    /// Reserves this Mac's desktop for `peer`'s handoff, or says why not.
    pub fn begin_lease(&self, peer: &str, session_id: u64) -> Result<LeaseGuard, &'static str> {
        let mut state = self.state();
        state.check(peer, session_id)?;
        if state.lease.is_some() {
            return Err(HANDOFF_ACTIVE);
        }
        state.lease = Some((peer.to_owned(), session_id));
        Ok(LeaseGuard(self.clone()))
    }

    /// Whether `peer`'s session may ask about this Mac's desktop, or why not.
    pub fn desktop_allowed(&self, peer: &str, session_id: u64) -> Result<(), &'static str> {
        self.state().check(peer, session_id)
    }

    /// The peer controlling this Mac, or about to.
    pub fn controller(&self) -> Option<String> {
        let state = self.state();
        let inbound = state.inbound.as_ref().map(|(peer, ..)| peer);
        inbound
            .or(state.lease.as_ref().map(|(peer, _)| peer))
            .cloned()
    }
}

/// Holds this Mac's input for one crossing. Dropping it lets a peer take
/// control again.
pub(crate) struct OutboundGuard(Ownership);

impl Drop for OutboundGuard {
    fn drop(&mut self) {
        self.0.state().outbound = false;
    }
}

/// Holds this Mac's input for a peer's activation.
pub(crate) struct InboundClaim {
    ownership: Ownership,
    claim: u64,
}

impl Drop for InboundClaim {
    fn drop(&mut self) {
        let mut state = self.ownership.state();
        if state
            .inbound
            .as_ref()
            .is_some_and(|(.., claim)| *claim == self.claim)
        {
            state.inbound = None;
        }
    }
}

/// Holds this Mac's desktop for a peer's handoff.
pub(crate) struct LeaseGuard(Ownership);

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.0.state().lease = None;
    }
}

/// Whether a peer's input reaches this Mac. Unlike Linux, there is no hold
/// while the seat is unknown: the Mac always knows whether it is locked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    Inject,
    Refuse,
}

/// A peer may control this Mac if its record lets it connect and send,
/// macOS lets zflow post events, and the screen is unlocked. Its session
/// only exists while sharing is on.
pub(crate) fn admit(
    permissions: Option<&PeerPermissions>,
    post_allowed: bool,
    locked: bool,
) -> Admission {
    let allowed = permissions.is_some_and(|peer| peer.connect && peer.send_normal);
    if allowed && post_allowed && !locked {
        Admission::Inject
    } else {
        Admission::Refuse
    }
}

/// Splits a batch into what reaches this Mac and whether any of it was
/// refused. A refused peer's safety releases still get through, so nothing
/// stays held.
pub(crate) fn admitted_effects(
    effects: Vec<ReceiverEffect>,
    admission: Admission,
) -> (Vec<ReceiverEffect>, bool) {
    let mut deliver = Vec::new();
    let mut refused = false;
    for effect in effects {
        if !effect.is_injection() || admission == Admission::Inject || is_safety_release(&effect) {
            deliver.push(effect);
        } else {
            refused = true;
        }
    }
    (deliver, refused)
}

pub(crate) fn is_safety_release(effect: &ReceiverEffect) -> bool {
    // A synthetic touch replacement can carry peer-supplied contacts; only a
    // replacement with no contacts is a release.
    if let ReceiverEffect::TouchReplaced { state, synthetic } = effect {
        return *synthetic && state.is_empty();
    }
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
        } | ReceiverEffect::ActivationClosed { .. }
    )
}

/// Answers a peer while this Mac sends its own input. Its desktop is not
/// available and no activation may open, which closes the peer's session.
/// A batch that only lets go of input succeeds: nothing is held, because
/// a claim cannot exist while this Mac sends and dropping one released it.
pub(super) fn answer_while_sending(kind: SessionEventKind) {
    match kind {
        SessionEventKind::Desktop { reply, .. } => {
            let _ = reply.send(DesktopResponse::unavailable(SENDING));
        }
        SessionEventKind::ReceiverEffects {
            effects, applied, ..
        } => {
            let opens = effects
                .iter()
                .any(|effect| matches!(effect, ReceiverEffect::ActivationOpened(_)));
            let (_, refused) = admitted_effects(effects, Admission::Refuse);
            let _ = applied.send(if opens || refused {
                Err(SENDING.into())
            } else {
                Ok(())
            });
        }
        SessionEventKind::OutboundEnded
        | SessionEventKind::Closed { .. }
        | SessionEventKind::Layout { .. }
        | SessionEventKind::Clipboard { .. } => {}
    }
}

/// Answers events that arrived while the link was idle, including the
/// OutboundEnded that trails the previous crossing's release.
pub(super) fn answer_waiting_events(events: &mut mpsc::Receiver<SessionEvent>) -> Result<()> {
    loop {
        match events.try_recv() {
            Ok(event) => {
                if let SessionEventKind::Closed { reason } = event.kind {
                    bail!("input session closed: {reason}");
                }
                answer_while_sending(event.kind);
            }
            Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::error::TryRecvError::Disconnected) => bail!("input session closed"),
        }
    }
}

/// The Mac does not take input from other computers yet. Refuse whatever a
/// receiver would handle.
pub(super) fn refuse_inbound(kind: SessionEventKind) {
    match kind {
        SessionEventKind::Desktop { reply, .. } => {
            let _ = reply.send(DesktopResponse::unavailable(
                "Mac source cannot receive desktop handoffs",
            ));
        }
        SessionEventKind::ReceiverEffects { applied, .. } => {
            let _ = applied.send(Err(
                "the Mac does not accept input from other computers".into()
            ));
        }
        SessionEventKind::OutboundEnded
        | SessionEventKind::Closed { .. }
        | SessionEventKind::Layout { .. }
        | SessionEventKind::Clipboard { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use tokio::sync::oneshot;

    use super::*;
    use crate::{
        core::{
            ActivationId, HidUsage, MotionDelta, MotionSequence, SessionCloseReason,
            SessionContext, SessionEpoch, TransportGeneration,
        },
        desktop::DesktopRequest,
    };

    #[test]
    fn sending_and_being_controlled_exclude_each_other() {
        let ownership = Ownership::default();
        let outbound = ownership.begin_outbound().unwrap();
        assert!(
            ownership.begin_outbound().is_none(),
            "one crossing at a time"
        );
        assert!(ownership.claim_inbound("linux", 1).is_none());
        assert_eq!(ownership.begin_lease("linux", 1).err(), Some(SENDING));
        assert_eq!(ownership.desktop_allowed("linux", 1), Err(SENDING));
        assert_eq!(ownership.controller(), None);
        drop(outbound);

        let claim = ownership.claim_inbound("linux", 1).unwrap();
        assert_eq!(ownership.controller().as_deref(), Some("linux"));
        assert!(ownership.begin_outbound().is_none());
        drop(claim);
        assert_eq!(ownership.controller(), None);
        assert!(ownership.begin_outbound().is_some());
    }

    #[test]
    fn one_session_controls_at_a_time_and_may_claim_again() {
        let ownership = Ownership::default();
        let first = ownership.claim_inbound("linux", 1).unwrap();
        assert!(
            ownership.claim_inbound("linux", 2).is_none(),
            "another session"
        );
        assert!(ownership.claim_inbound("desk", 1).is_none(), "another peer");
        assert_eq!(ownership.desktop_allowed("desk", 1), Err(OWNED));
        assert_eq!(ownership.begin_lease("desk", 1).err(), Some(OWNED));
        assert_eq!(ownership.desktop_allowed("linux", 1), Ok(()));

        // The newest claim holds input, so dropping an older one keeps it.
        let second = ownership.claim_inbound("linux", 1).unwrap();
        drop(first);
        assert_eq!(ownership.controller().as_deref(), Some("linux"));
        assert!(ownership.claim_inbound("linux", 2).is_none());
        drop(second);
        assert!(ownership.claim_inbound("linux", 2).is_some());
    }

    #[test]
    fn a_desktop_handoff_keeps_the_mac_for_its_session() {
        let ownership = Ownership::default();
        let lease = ownership.begin_lease("linux", 1).unwrap();
        assert_eq!(ownership.controller().as_deref(), Some("linux"));
        assert_eq!(
            ownership.begin_lease("linux", 1).err(),
            Some(HANDOFF_ACTIVE)
        );
        assert!(ownership.begin_outbound().is_none());
        assert!(ownership.claim_inbound("desk", 1).is_none());
        let claim = ownership.claim_inbound("linux", 1).unwrap();
        drop(lease);
        assert_eq!(ownership.controller().as_deref(), Some("linux"));
        drop(claim);
        assert_eq!(ownership.controller(), None);
        assert!(ownership.begin_lease("desk", 3).is_ok());
    }

    fn context() -> SessionContext {
        SessionContext {
            session_epoch: SessionEpoch([7; 16]),
            transport_generation: TransportGeneration(1),
            activation_id: ActivationId(1),
        }
    }

    fn key(pressed: bool, synthetic: bool) -> ReceiverEffect {
        ReceiverEffect::Key {
            key: HidUsage::keyboard(4),
            pressed,
            synthetic,
        }
    }

    fn closed() -> ReceiverEffect {
        ReceiverEffect::ActivationClosed {
            session: context(),
            reason: SessionCloseReason::LocalRelease,
        }
    }

    #[test]
    fn only_an_allowed_peer_on_an_unlocked_mac_injects() {
        let peer = PeerPermissions {
            connect: true,
            send_normal: true,
            ..PeerPermissions::default()
        };
        assert_eq!(admit(Some(&peer), true, false), Admission::Inject);
        assert_eq!(admit(Some(&peer), false, false), Admission::Refuse);
        assert_eq!(admit(Some(&peer), true, true), Admission::Refuse);
        assert_eq!(admit(None, true, false), Admission::Refuse);
        for peer in [
            PeerPermissions {
                send_normal: false,
                ..peer
            },
            PeerPermissions {
                connect: false,
                ..peer
            },
        ] {
            assert_eq!(admit(Some(&peer), true, false), Admission::Refuse);
        }

        let effects = || {
            vec![
                key(true, false),
                ReceiverEffect::Motion {
                    delta: MotionDelta::default(),
                    through_sequence: MotionSequence(1),
                },
                key(false, false),
                key(false, true),
                closed(),
            ]
        };
        let (denied, refused) = admitted_effects(effects(), Admission::Refuse);
        assert!(refused);
        assert_eq!(denied.len(), 2);
        assert!(denied.iter().all(is_safety_release));
        let (injected, refused) = admitted_effects(effects(), Admission::Inject);
        assert!(!refused);
        assert_eq!(injected.len(), 5);
    }

    fn apply(effects: Vec<ReceiverEffect>) -> Result<(), String> {
        let (applied, answer) = oneshot::channel();
        answer_while_sending(SessionEventKind::ReceiverEffects {
            effects,
            touch_captured_at: None,
            received_at: Instant::now(),
            applied,
        });
        answer.blocking_recv().unwrap()
    }

    #[test]
    fn while_sending_only_releases_succeed() {
        assert_eq!(apply(vec![key(false, true), closed()]), Ok(()));
        assert_eq!(apply(vec![key(true, false)]), Err(SENDING.into()));
        assert_eq!(
            apply(vec![ReceiverEffect::ActivationOpened(context())]),
            Err(SENDING.into()),
            "an activation cannot open, which closes the session"
        );
    }

    #[tokio::test]
    async fn idle_events_are_answered_and_a_closed_session_refuses_the_crossing() {
        let event = |kind| SessionEvent {
            session_id: 1,
            peer: "linux".into(),
            kind,
        };
        let (sender, mut events) = mpsc::channel(8);
        let (reply, desktop) = oneshot::channel();
        sender
            .send(event(SessionEventKind::OutboundEnded))
            .await
            .unwrap();
        sender
            .send(event(SessionEventKind::Desktop {
                request: DesktopRequest::Snapshot,
                reply,
            }))
            .await
            .unwrap();
        answer_waiting_events(&mut events).unwrap();
        assert_eq!(
            desktop.await.unwrap(),
            DesktopResponse::unavailable(SENDING)
        );
        sender
            .send(event(SessionEventKind::Closed {
                reason: "lost".into(),
            }))
            .await
            .unwrap();
        assert!(answer_waiting_events(&mut events).is_err());
        drop(sender);
        assert!(answer_waiting_events(&mut events).is_err());
    }
}
