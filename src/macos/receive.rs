//! Lets paired computers control this Mac, and keeps its input going one
//! way at a time: out to a peer while this Mac sends, or in from one peer's
//! session while that peer controls it.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use tokio::sync::{mpsc, oneshot};

use super::{
    CursorPosition, awdl,
    clipboard::{Clipboard, MacPasteboard, Pasteboard},
    handoff_server::{Desk, HandoffServer},
    inject::{self, Injector},
};
use crate::{
    clipboard::Clip,
    config::{PeerConfig, PeerPermissions},
    core::{KeyboardMode, ReceiverEffect, SessionCloseReason},
    desktop::{DesktopRequest, DesktopResponse, Geometry, Point, SharedLayout},
    session::{SessionEvent, SessionEventKind, SessionHandle},
};

// The Linux daemon's words for the same refusals.
pub(crate) const SENDING: &str = "This computer is sending its own input";
pub(crate) const OWNED: &str = "Another computer owns input";
pub(crate) const HANDOFF_ACTIVE: &str = "A desktop handoff is already active";
const NOT_ALLOWED: &str =
    "Desktop control requires the active unlocked local session and an authorized paired peer";
const REJECTED: &str = "receiver effects were rejected before backend application";
/// How often the screen lock is checked while a peer controls this Mac.
const LOCK_CHECK: Duration = Duration::from_millis(250);
/// How long taking AWDL down may take while a peer controls this Mac.
const AWDL_LIMIT: Duration = Duration::from_secs(5);

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

    /// Gives this Mac's input to `peer`'s session for an activation, or
    /// says why not: this Mac sends, or another peer or session controls it
    /// or holds its desktop. The same session may claim again, and the
    /// newest claim holds it.
    pub fn claim_inbound(&self, peer: &str, session_id: u64) -> Result<InboundClaim, &'static str> {
        let mut state = self.state();
        state.check(peer, session_id)?;
        state.claims += 1;
        let claim = state.claims;
        state.inbound = Some((peer.to_owned(), session_id, claim));
        Ok(InboundClaim {
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
/// OutboundEnded that trails the previous crossing's release. A peer's
/// layout goes to `layout`, and its clipboard to `clip`.
pub(super) fn answer_waiting_events(
    events: &mut mpsc::Receiver<SessionEvent>,
    mut layout: impl FnMut(SharedLayout),
    mut clip: impl FnMut(Clip),
) -> Result<()> {
    loop {
        match events.try_recv() {
            Ok(event) => match event.kind {
                SessionEventKind::Closed { reason } => bail!("input session closed: {reason}"),
                SessionEventKind::Layout { layout: shared } => layout(shared),
                SessionEventKind::Clipboard { clip: shared } => clip(shared),
                kind => answer_while_sending(kind),
            },
            Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::error::TryRecvError::Disconnected) => bail!("input session closed"),
        }
    }
}

/// Turns a peer's scrolling around, as the daemon does for a Mac with
/// natural scrolling against a desktop without it. Pointer motion stays.
fn reverse_scrolling(effects: &mut [ReceiverEffect]) {
    for effect in effects {
        if let ReceiverEffect::Motion { delta, .. } = effect {
            delta.scroll_x = -delta.scroll_x;
            delta.scroll_y = -delta.scroll_y;
        }
    }
}

/// What receiving reads from this Mac. Tests use `FakeBackend`.
pub(crate) trait Screen: Send + Sync + 'static {
    fn geometry(&self) -> Result<Geometry>;
    /// Changes whenever macOS reconfigures a display.
    fn generation(&self) -> u32;
    fn cursor(&self) -> Result<CursorPosition>;
    /// macOS lets zflow post events.
    fn post_allowed(&self) -> bool;
    /// The screen lock is up, or another user has the console.
    fn locked(&self) -> bool;
    /// Wakes the display, as local input would.
    fn wake(&self);
}

pub(crate) struct MacScreen;

impl Screen for MacScreen {
    fn geometry(&self) -> Result<Geometry> {
        super::desktop_geometry()
    }

    fn generation(&self) -> u32 {
        super::display_generation()
    }

    fn cursor(&self) -> Result<CursorPosition> {
        super::cursor_position()
    }

    fn post_allowed(&self) -> bool {
        inject::post_allowed()
    }

    fn locked(&self) -> bool {
        inject::session_locked()
    }

    fn wake(&self) {
        if let Err(error) = inject::declare_user_activity() {
            tracing::debug!(%error, "display not woken");
        }
    }
}

#[cfg(test)]
impl Screen for inject::FakeBackend {
    fn geometry(&self) -> Result<Geometry> {
        let monitors = self
            .state()
            .displays
            .iter()
            .map(|display| crate::desktop::Rect {
                x: display.x as i32,
                y: display.y as i32,
                width: display.width as u32,
                height: display.height as u32,
            })
            .collect();
        Ok(Geometry {
            monitors,
            displays: Vec::new(),
        })
    }

    fn generation(&self) -> u32 {
        self.state().generation
    }

    fn cursor(&self) -> Result<CursorPosition> {
        self.state()
            .cursor
            .ok_or_else(|| anyhow::anyhow!("no cursor"))
    }

    fn post_allowed(&self) -> bool {
        true
    }

    fn locked(&self) -> bool {
        self.state().locked
    }

    fn wake(&self) {
        self.state().wakes += 1;
    }
}

/// Each peer's current session, so a revoked peer or a lapsed handoff can
/// end it.
#[derive(Clone, Default)]
struct Sessions(Arc<Mutex<BTreeMap<String, SessionHandle>>>);

impl Sessions {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, SessionHandle>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Closes `peer`'s session, if it is still `session_id` when one is given.
    fn close(&self, peer: &str, session_id: Option<u64>, reason: SessionCloseReason) {
        let sessions = self.lock();
        if let Some(session) = sessions.get(peer)
            && session_id.is_none_or(|id| id == session.id())
        {
            session.close(reason);
        }
    }
}

/// This Mac as the handoff server sees it.
pub(crate) struct MacDesk {
    screen: Arc<dyn Screen>,
    injector: Arc<Injector>,
    sessions: Sessions,
}

impl Desk for MacDesk {
    fn geometry(&self) -> Result<Geometry> {
        self.screen.geometry()
    }

    fn generation(&self) -> u32 {
        self.screen.generation()
    }

    fn cursor(&self) -> Result<CursorPosition> {
        self.screen.cursor()
    }

    fn move_to(&self, point: Point) {
        self.injector.move_to(CursorPosition {
            x: f64::from(point.x),
            y: f64::from(point.y),
        });
    }

    fn release_all(&self) {
        self.injector.release_all();
    }

    fn wake(&self) {
        self.screen.wake();
    }

    fn close(&self, peer: &str, session_id: u64) {
        self.sessions
            .close(peer, Some(session_id), SessionCloseReason::LeaseExpired);
    }

    fn current(&self, peer: &str, session_id: u64) -> bool {
        self.sessions
            .lock()
            .get(peer)
            .is_some_and(|session| session.id() == session_id)
    }
}

/// Who may control this Mac, and how.
#[derive(Default)]
struct Policy {
    /// Every paired computer, while sharing is on.
    peers: BTreeMap<String, PeerConfig>,
    /// Accessibility, as the app last saw it.
    accessibility: bool,
    /// AWDL goes down while a peer controls this Mac.
    reduce_wifi_latency: bool,
}

/// Lets paired computers control this Mac. The links share one, with the
/// injector and the handoff server.
pub(crate) struct Receiving {
    ownership: Ownership,
    screen: Arc<dyn Screen>,
    injector: Arc<Injector>,
    handoff: Arc<HandoffServer<MacDesk>>,
    policy: Mutex<Policy>,
    sessions: Sessions,
    clipboard: Arc<Clipboard>,
}

impl Receiving {
    /// Posts on this Mac, and shares its pasteboard.
    pub fn mac() -> Result<Arc<Self>> {
        let pasteboard = Arc::new(MacPasteboard);
        Ok(Self::new(Injector::mac()?, Arc::new(MacScreen), pasteboard))
    }

    pub fn new(
        injector: Injector,
        screen: Arc<dyn Screen>,
        pasteboard: Arc<dyn Pasteboard>,
    ) -> Arc<Self> {
        let ownership = Ownership::default();
        let injector = Arc::new(injector);
        let sessions = Sessions::default();
        let desk = MacDesk {
            screen: screen.clone(),
            injector: injector.clone(),
            sessions: sessions.clone(),
        };
        let handoff = Arc::new(HandoffServer::new(desk, ownership.clone()));
        // Weak, so the injector thread does not keep the server alive, and
        // through it the injector itself.
        let server = Arc::downgrade(&handoff);
        injector.watch(Box::new(move |from, to| {
            server
                .upgrade()
                .is_some_and(|server| server.moved(from, to))
        }));
        Arc::new(Self {
            ownership,
            screen,
            injector,
            handoff,
            policy: Mutex::default(),
            sessions,
            clipboard: Clipboard::new(pasteboard),
        })
    }

    pub fn ownership(&self) -> &Ownership {
        &self.ownership
    }

    pub fn clipboard(&self) -> &Arc<Clipboard> {
        &self.clipboard
    }

    fn policy(&self) -> MutexGuard<'_, Policy> {
        self.policy.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Who may control this Mac, and how their keys and scrolling act. A
    /// controlling peer that may no longer loses control at once.
    pub fn set_peers(&self, peers: BTreeMap<String, PeerConfig>) {
        let mut policy = self.policy();
        if policy.peers != peers {
            policy.peers = peers;
            self.revoke(&policy);
        }
    }

    pub fn set_accessibility(&self, allowed: bool) {
        let mut policy = self.policy();
        if policy.accessibility != allowed {
            policy.accessibility = allowed;
            self.revoke(&policy);
        }
    }

    /// Takes effect the next time a peer takes control.
    pub fn set_reduce_wifi_latency(&self, reduce: bool) {
        self.policy().reduce_wifi_latency = reduce;
    }

    /// Ends the session of a controlling peer the policy no longer admits.
    fn revoke(&self, policy: &Policy) {
        let Some(peer) = self.ownership.controller() else {
            return;
        };
        let permissions = policy.peers.get(&peer).map(|record| &record.permissions);
        if admit(permissions, policy.accessibility, false) == Admission::Refuse {
            tracing::info!(%peer, "peer may no longer control this Mac");
            self.sessions
                .close(&peer, None, SessionCloseReason::PermissionRevoked);
        }
    }

    /// Whether `peer` may control this Mac, with how its keys act and
    /// whether its scrolling turns around. `fresh` also asks macOS whether
    /// zflow may post and whether the screen is locked, as a new activation
    /// or a desktop request does; the injector checks the lock on every
    /// batch.
    fn admission(&self, peer: &str, fresh: bool) -> (Admission, KeyboardMode, bool) {
        let (permissions, keyboard, reverse, accessibility) = {
            let policy = self.policy();
            let record = policy.peers.get(peer);
            (
                record.map(|record| record.permissions),
                record.map_or(KeyboardMode::Standard, |record| record.keyboard),
                record.is_some_and(|record| record.reverse_scroll),
                policy.accessibility,
            )
        };
        let post_allowed = accessibility && (!fresh || self.screen.post_allowed());
        let locked = fresh && self.screen.locked();
        let admission = admit(permissions.as_ref(), post_allowed, locked);
        (admission, keyboard, reverse)
    }

    /// Starts taking `peer`'s input over `session`.
    pub fn inbound(self: &Arc<Self>, peer: &str, session: &SessionHandle) -> Inbound {
        self.sessions
            .lock()
            .insert(peer.to_owned(), session.clone());
        Inbound {
            receiving: self.clone(),
            peer: peer.to_owned(),
            session: session.clone(),
            claim: None,
            wifi: None,
        }
    }

    /// Ends handoffs whose peer stopped polling or whose displays changed,
    /// and control when the screen locks. Runs until dropped.
    pub async fn watch(&self) {
        tokio::join!(self.handoff.watch(), self.watch_lock());
    }

    /// Ends control when the screen locks, as the Linux daemon does when its
    /// seat changes. A modifier the peer holds would otherwise stay down on
    /// the lock screen, and a peer that sends nothing would keep control
    /// through it.
    async fn watch_lock(&self) {
        let mut check = tokio::time::interval(LOCK_CHECK);
        check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            check.tick().await;
            let Some(peer) = self.ownership.controller() else {
                continue;
            };
            if self.screen.locked() {
                tracing::info!(%peer, "this Mac locked; ending control");
                self.injector.release_all();
                self.sessions
                    .close(&peer, None, SessionCloseReason::PermissionRevoked);
            }
        }
    }

    async fn desktop(
        &self,
        peer: &str,
        session_id: u64,
        request: DesktopRequest,
    ) -> DesktopResponse {
        if self.admission(peer, true).0 == Admission::Refuse {
            return DesktopResponse::unavailable(NOT_ALLOWED);
        }
        self.handoff.request(peer, session_id, request).await
    }
}

/// One session's input from its peer. Dropping it lets go of whatever the
/// peer still holds on this Mac.
pub(crate) struct Inbound {
    receiving: Arc<Receiving>,
    peer: String,
    session: SessionHandle,
    claim: Option<InboundClaim>,
    /// Keeps AWDL down while the claim lasts.
    wifi: Option<awdl::Wanted>,
}

impl Inbound {
    /// Answers a desktop request on its own task. A Poll can wait 200 ms,
    /// and the session's input must not wait behind it.
    pub fn desktop(&self, request: DesktopRequest, reply: oneshot::Sender<DesktopResponse>) {
        let (receiving, peer, id) = (self.receiving.clone(), self.peer.clone(), self.session.id());
        tokio::spawn(async move {
            let _ = reply.send(receiving.desktop(&peer, id, request).await);
        });
    }

    /// The clipboard this Mac shares, which the session's clips go to.
    pub fn clipboard(&self) -> Arc<Clipboard> {
        self.receiving.clipboard.clone()
    }

    /// Whether this session's peer controls this Mac, or holds its desktop
    /// for a handoff.
    pub fn controls(&self) -> bool {
        self.receiving
            .ownership
            .controller()
            .is_some_and(|peer| peer == self.peer)
    }

    /// Posts one batch of the peer's input, or says why not, which closes
    /// the session.
    pub async fn effects(
        &mut self,
        effects: Vec<ReceiverEffect>,
        received_at: Instant,
    ) -> Result<(), String> {
        let opens = effects
            .iter()
            .any(|effect| matches!(effect, ReceiverEffect::ActivationOpened(_)));
        // A batch can close one activation and open the next, so the last
        // one says whether the peer still holds this Mac afterwards.
        let closes = effects.iter().rev().find_map(|effect| match effect {
            ReceiverEffect::ActivationOpened(_) => Some(false),
            ReceiverEffect::ActivationClosed { .. } => Some(true),
            _ => None,
        }) == Some(true);
        let (mut admission, keyboard, reverse) = self.receiving.admission(&self.peer, opens);
        if opens {
            if admission == Admission::Refuse {
                self.release();
                return Err(NOT_ALLOWED.into());
            }
            let claim = self
                .receiving
                .ownership
                .claim_inbound(&self.peer, self.session.id());
            match claim {
                Ok(claim) => {
                    let first = self.claim.replace(claim).is_none();
                    // A revocation between the check above and the claim
                    // found no controller to end, so check again now that
                    // the claim is there for the next one to see.
                    if self.receiving.admission(&self.peer, false).0 == Admission::Refuse {
                        self.release();
                        return Err(NOT_ALLOWED.into());
                    }
                    if first {
                        tracing::info!(peer = %self.peer, "peer took control of this Mac");
                        self.take_control();
                    }
                }
                Err(reason) => {
                    self.release();
                    return Err(reason.into());
                }
            }
        } else if self.claim.is_none() {
            // Only an activation this session claimed reaches the Mac.
            admission = Admission::Refuse;
        }
        let (mut deliver, refused) = admitted_effects(effects, admission);
        if reverse {
            reverse_scrolling(&mut deliver);
        }
        let applied = if deliver.is_empty() {
            Ok(())
        } else {
            let keyboard = opens.then_some(keyboard);
            match self.receiving.injector.apply(deliver, keyboard).await {
                Ok(Ok(applied_at)) => {
                    self.session
                        .record_receive_to_inject(received_at, applied_at);
                    Ok(())
                }
                Ok(Err(error)) => Err(format!("{error:#}")),
                Err(_) => Err("the Mac input thread stopped".into()),
            }
        };
        if refused || applied.is_err() {
            self.release();
        } else if closes && self.claim.take().is_some() {
            // Closing the activation already let go of everything.
            self.wifi = None;
            tracing::info!(peer = %self.peer, "peer let go of this Mac");
            // The pointer went back to the peer, so the clipboard goes along.
            self.receiving.clipboard.share(&self.session);
        }
        if refused {
            return Err(REJECTED.into());
        }
        applied
    }

    /// Wakes the display, since the chord sends no Prepare that would,
    /// and takes AWDL down when the app asks for less Wi-Fi lag.
    fn take_control(&mut self) {
        self.receiving.screen.wake();
        if self.receiving.policy().reduce_wifi_latency {
            self.wifi = Some(awdl::shared().want());
            let peer = self.peer.clone();
            // On its own task, so the peer's input does not wait for the helper.
            tokio::spawn(async move { awdl::shared().down(&peer, AWDL_LIMIT).await });
        }
    }

    /// Lets go of whatever the peer holds, and of its claim on this Mac.
    fn release(&mut self) {
        if let Some(claim) = self.claim.take() {
            // Sent before the claim goes, so it reaches the injector ahead
            // of the next peer's input.
            self.receiving.injector.release_all();
            drop(claim);
            self.wifi = None;
            tracing::info!(peer = %self.peer, "peer lost control of this Mac");
        }
    }
}

impl Drop for Inbound {
    fn drop(&mut self) {
        self.release();
        let id = self.session.id();
        {
            let mut sessions = self.receiving.sessions.lock();
            if sessions
                .get(&self.peer)
                .is_some_and(|session| session.id() == id)
            {
                sessions.remove(&self.peer);
            }
        }
        // After the session is gone, so a Prepare still on its way either
        // started its handoff before this ends it, or finds the session gone.
        self.receiving.handoff.closed(&self.peer, id);
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
        assert!(ownership.claim_inbound("linux", 1).is_err());
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
            ownership.claim_inbound("linux", 2).is_err(),
            "another session"
        );
        assert!(ownership.claim_inbound("desk", 1).is_err(), "another peer");
        assert_eq!(ownership.desktop_allowed("desk", 1), Err(OWNED));
        assert_eq!(ownership.begin_lease("desk", 1).err(), Some(OWNED));
        assert_eq!(ownership.desktop_allowed("linux", 1), Ok(()));

        // The newest claim holds input, so dropping an older one keeps it.
        let second = ownership.claim_inbound("linux", 1).unwrap();
        drop(first);
        assert_eq!(ownership.controller().as_deref(), Some("linux"));
        assert!(ownership.claim_inbound("linux", 2).is_err());
        drop(second);
        assert!(ownership.claim_inbound("linux", 2).is_ok());
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
        assert!(ownership.claim_inbound("desk", 1).is_err());
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
        answer_waiting_events(&mut events, drop, drop).unwrap();
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
        assert!(answer_waiting_events(&mut events, drop, drop).is_err());
        drop(sender);
        assert!(answer_waiting_events(&mut events, drop, drop).is_err());
    }
}
