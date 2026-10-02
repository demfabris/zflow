use super::*;
use crate::app::layout_model::Layout;
use crate::clipboard::Clip;
use crate::control::{ControlError, read_message_within, write_message_within};
use crate::desktop::{DesktopRequest, DesktopResponse};
use crate::desktop::{Edge, FRACTION_MAX};
use crate::linux::SeatState;
use crate::peer_view::{
    AgentReply, AgentRequest, ClipData, ClipboardContents, LocalRequest, MAX_AGENT_MESSAGE,
    OutboundEdge,
};
use crate::session::{desktop_operation, desktop_response_kind};
use anyhow::ensure;
use tokio::net::UnixStream;

struct Job {
    request: AgentRequest,
    origin: Option<(String, u64)>,
    reply: tokio::sync::oneshot::Sender<AgentReply>,
    queued_at: Instant,
}

/// How long the agent has to answer. GNOME waits up to a second for the
/// app that copied to hand the clipboard over, and the agent a little
/// longer (src/app/desktop.rs), so clipboard requests get more time.
fn reply_limit(request: &AgentRequest) -> Duration {
    match request {
        AgentRequest::Local(LocalRequest::ReadClipboard | LocalRequest::WriteClipboard { .. }) => {
            Duration::from_secs(2)
        }
        _ => Duration::from_millis(600),
    }
}

struct Lease {
    peer: String,
    session_id: u64,
    token: u64,
    renewed: Instant,
}

impl Lease {
    fn permits(&self, peer: &str, session_id: u64, token: u64) -> bool {
        self.peer == peer
            && self.session_id == session_id
            && self.token == token
            && self.renewed.elapsed() < Duration::from_millis(crate::desktop::LEASE_MS)
    }
}

/// What GNOME Shell shows for this computer's own input.
#[derive(Default)]
pub(super) struct LocalState {
    /// Edges that lead to another computer, with that computer's name.
    edges: Vec<(OutboundEdge, String)>,
    /// Whether the pointer has to rest against an edge before it crosses.
    pause: bool,
    sending: bool,
}

#[derive(Default)]
pub(super) struct Hub {
    /// One task sends every change to the agent, in order.
    local: watch::Sender<LocalState>,
    broker: Mutex<Option<(u64, mpsc::Sender<Job>)>>,
    lease: Mutex<Option<Lease>>,
    next: AtomicU64,
}

impl Hub {
    async fn disconnected(
        &self,
        id: u64,
        sessions: &Mutex<BTreeMap<String, SessionHandle>>,
        runtime: &LinuxRuntimeControl,
    ) {
        let lease = {
            let mut broker = self.broker.lock().await;
            if broker.as_ref().is_none_or(|(current, _)| *current != id) {
                return;
            }
            let lease = self.lease.lock().await.take();
            *broker = None;
            // No agent is left to correct a lost reset, so it waits out a full
            // queue. Sending it before the broker slot frees up keeps it ahead
            // of the next agent's first report.
            if let Err(error) = runtime
                .send_critical(
                    RuntimeCommand::DesktopFocus { terminal: false },
                    TERMINAL_SEND_TIMEOUT,
                )
                .await
            {
                tracing::warn!(%error, "desktop focus reset not delivered");
            }
            lease
        };
        if let Some(lease) = lease
            && let Some(session) = sessions
                .lock()
                .await
                .get(&lease.peer)
                .filter(|s| s.id() == lease.session_id)
        {
            session.close(SessionCloseReason::BackendUnavailable);
        }
    }

    /// Passes the desktop's terminal focus to the input runtime while a
    /// desktop agent is connected.
    pub async fn focus(&self, runtime: &LinuxRuntimeControl, terminal: bool) {
        // Holding the lock keeps a late report from landing after the reset
        // in `disconnected`.
        let broker = self.broker.lock().await;
        // A dropped update only affects shortcuts pressed until the next one.
        if broker.is_some()
            && let Err(error) = runtime.send(RuntimeCommand::DesktopFocus { terminal })
        {
            tracing::debug!(%error, "desktop focus not delivered");
        }
    }

    pub async fn allows_session(&self, peer: &str, session_id: u64) -> bool {
        self.lease
            .lock()
            .await
            .as_ref()
            .is_none_or(|lease| lease.peer == peer && lease.session_id == session_id)
    }

    async fn call(&self, request: DesktopRequest) -> DesktopResponse {
        self.call_scoped(AgentRequest::Handoff(request), None).await
    }

    /// This desktop's monitors and pointer, from GNOME.
    pub(super) async fn snapshot(&self) -> DesktopResponse {
        self.call(DesktopRequest::Snapshot).await
    }

    /// Puts the pointer at `position` on this desktop.
    pub(super) async fn warp(&self, position: crate::desktop::Point) -> DesktopResponse {
        self.call_scoped(AgentRequest::Local(LocalRequest::Warp { position }), None)
            .await
    }

    /// The computer whose edge the pointer pushed against, if any.
    pub(super) fn edge_peer(&self, edge: Edge, position: u32) -> Option<String> {
        self.local
            .borrow()
            .edges
            .iter()
            .find(|(range, _)| range.edge == edge && (range.start..=range.end).contains(&position))
            .map(|(_, peer)| peer.clone())
    }

    /// What this desktop's clipboard holds, read through GNOME.
    pub(super) async fn read_clipboard(&self) -> Result<ClipboardContents> {
        match self
            .ask(AgentRequest::Local(LocalRequest::ReadClipboard), None)
            .await
        {
            AgentReply::Clipboard(contents) => Ok(contents),
            AgentReply::Desktop(DesktopResponse::Unavailable { reason }) => bail!("{reason}"),
            AgentReply::Desktop(_) => bail!("The desktop agent did not read the clipboard"),
        }
    }

    /// Puts a clip from another computer on this desktop's clipboard.
    pub(super) async fn write_clipboard(&self, clip: Clip) -> Result<()> {
        let request = LocalRequest::WriteClipboard {
            kind: clip.kind(),
            data: ClipData(clip.into_data()),
        };
        match self.call_scoped(AgentRequest::Local(request), None).await {
            DesktopResponse::Finished => Ok(()),
            DesktopResponse::Unavailable { reason } => bail!("{reason}"),
            _ => bail!("The desktop agent did not write the clipboard"),
        }
    }

    /// Shows the person here a short notice.
    pub(super) async fn notify(&self, message: String) {
        let request = AgentRequest::Local(LocalRequest::Notify { message });
        if let DesktopResponse::Unavailable { reason } = self.call_scoped(request, None).await {
            tracing::debug!(%reason, "desktop notice not shown");
        }
    }

    /// Asks the agent for anything but the clipboard. A clip is never an
    /// answer to these, so one never reaches a peer as a handoff reply.
    async fn call_scoped(
        &self,
        request: AgentRequest,
        origin: Option<(String, u64)>,
    ) -> DesktopResponse {
        match self.ask(request, origin).await {
            AgentReply::Desktop(response) => response,
            AgentReply::Clipboard(_) => {
                DesktopResponse::unavailable("The desktop agent answered with a clip")
            }
        }
    }

    async fn ask(&self, request: AgentRequest, origin: Option<(String, u64)>) -> AgentReply {
        let started = Instant::now();
        let operation = request.operation();
        let limit = reply_limit(&request) + Duration::from_millis(100);
        let unavailable = |reason: &str| AgentReply::Desktop(DesktopResponse::unavailable(reason));
        let broker = self
            .broker
            .lock()
            .await
            .as_ref()
            .map(|(_, sender)| sender.clone());
        let Some(broker) = broker else {
            tracing::debug!(operation, "desktop request has no connected desktop agent");
            return unavailable(
                "Run zflow desktop-agent in the Linux desktop session and enable the zflow GNOME extension",
            );
        };
        let (reply, receipt) = tokio::sync::oneshot::channel();
        if broker
            .try_send(Job {
                request,
                origin,
                reply,
                queued_at: started,
            })
            .is_err()
        {
            tracing::warn!(operation, "desktop broker request queue unavailable");
            return unavailable("The desktop receiver is busy or stopped");
        }
        match tokio::time::timeout(limit, receipt).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => {
                tracing::warn!(
                    operation,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "desktop broker reply channel closed"
                );
                unavailable("The desktop did not respond")
            }
            Err(_) => {
                tracing::warn!(
                    operation,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    timeout_ms = limit.as_millis() as u64,
                    "desktop broker request timed out"
                );
                unavailable("The desktop did not respond")
            }
        }
    }

    /// When the handoff lease runs out unless a Poll renews it first.
    async fn lease_deadline(&self) -> Option<Instant> {
        self.lease
            .lock()
            .await
            .as_ref()
            .map(|lease| lease.renewed + Duration::from_millis(crate::desktop::LEASE_MS))
    }

    pub async fn closed(&self, peer: &str, session_id: u64) {
        let token = {
            let mut lease = self.lease.lock().await;
            if lease
                .as_ref()
                .is_some_and(|l| l.peer == peer && l.session_id == session_id)
            {
                lease.take().map(|l| l.token)
            } else {
                None
            }
        };
        if let Some(token) = token {
            tracing::debug!(%peer, session_id, "closed input session cleaning desktop reservation");
            let _ = self.call(DesktopRequest::Finish { token }).await;
        }
    }
}

/// The local tile's edges that touch another computer's tile, as barrier
/// ranges along this desktop.
pub(super) fn outbound_edges(layout: &Layout) -> Vec<(OutboundEdge, String)> {
    let fraction = |value: f64| (value.clamp(0.0, 1.0) * f64::from(FRACTION_MAX)).round() as u32;
    layout
        .transitions()
        .into_iter()
        .filter(|transition| layout.monitors[transition.source].peer.is_none())
        .filter_map(|transition| {
            let peer = layout.monitors[transition.target].peer.clone()?;
            let range = OutboundEdge {
                edge: transition.edge,
                start: fraction(transition.source_start),
                end: fraction(transition.source_end),
            };
            (range.start < range.end).then_some((range, peer))
        })
        .collect()
}

/// How long the pointer rests against an edge when crossings pause there,
/// as Deskflow's switch delay does.
const PAUSE_MS: u32 = 250;

pub(super) fn set_edges(shared: &Shared, edges: Vec<(OutboundEdge, String)>, pause: bool) {
    shared.desktop.local.send_modify(|state| {
        state.edges = edges;
        state.pause = pause;
    });
}

pub(super) fn set_sending(shared: &Shared, sending: bool) {
    shared
        .desktop
        .local
        .send_modify(|state| state.sending = sending);
}

/// Sends the agent each change to the edges and sending state, in order,
/// and everything again when an agent connects.
pub(super) fn start_local_sync(shared: Arc<Shared>) {
    let mut changes = shared.desktop.local.subscribe();
    tokio::spawn(async move {
        loop {
            let (edges, pause_ms, active) = {
                let state = changes.borrow_and_update();
                (
                    state.edges.iter().map(|(range, _)| *range).collect(),
                    if state.pause { PAUSE_MS } else { 0 },
                    state.sending,
                )
            };
            // Showing the pointer again matters more than the barriers.
            for request in [
                LocalRequest::Sending { active },
                LocalRequest::Edges { edges, pause_ms },
            ] {
                let response = shared
                    .desktop
                    .call_scoped(AgentRequest::Local(request), None)
                    .await;
                if let DesktopResponse::Unavailable { reason } = response {
                    tracing::debug!(%reason, "desktop agent did not take local state");
                }
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    });
}

pub(super) async fn request(
    shared: Arc<Shared>,
    peer: String,
    session_id: u64,
    request: DesktopRequest,
) -> DesktopResponse {
    let started = Instant::now();
    let operation = desktop_operation(&request);
    tracing::trace!(%peer, session_id, operation, "desktop receiver authorization started");
    if let Err(error) = request.validate() {
        return DesktopResponse::unavailable(error.to_string());
    }
    {
        let _policy = shared.policy.lock().await;
        let config = shared.config.read().await;
        let gate = shared.seat_gate.read().await.gate;
        if !matches!(gate, InjectionGate::Normal { .. })
            || !receiver_authorized(&config, &peer, gate)
            || !shared
                .sessions
                .lock()
                .await
                .get(&peer)
                .is_some_and(|s| s.id() == session_id)
        {
            return DesktopResponse::unavailable(
                "Desktop control requires the active unlocked local session and an authorized paired peer",
            );
        }
        // Arming or sending, this computer's own input goes elsewhere.
        if shared.runtime.status().ownership != OwnershipPhase::Idle {
            return DesktopResponse::unavailable("This computer is sending its own input");
        }
        if shared.active_outbound.lock().await.is_some()
            || shared
                .inbound_owner
                .lock()
                .await
                .as_ref()
                .is_some_and(|(p, id)| p != &peer || *id != session_id)
        {
            return DesktopResponse::unavailable("Another computer owns input");
        }
        let mut lease = shared.desktop.lease.lock().await;
        match &request {
            DesktopRequest::Prepare { token, .. } => {
                if lease.is_some() {
                    return DesktopResponse::unavailable("A desktop handoff is already active");
                }
                *lease = Some(Lease {
                    peer: peer.clone(),
                    session_id,
                    token: *token,
                    renewed: Instant::now(),
                });
            }
            DesktopRequest::Poll { token } | DesktopRequest::Finish { token } => {
                let Some(active) = lease.as_mut() else {
                    return DesktopResponse::unavailable("Desktop handoff expired or ended");
                };
                if !active.permits(&peer, session_id, *token) {
                    return DesktopResponse::unavailable("Desktop handoff token is stale");
                }
                active.renewed = Instant::now();
            }
            DesktopRequest::Snapshot => {}
        }
    }
    let response = shared
        .desktop
        .call_scoped(
            AgentRequest::Handoff(request.clone()),
            Some((peer.clone(), session_id)),
        )
        .await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let outcome = desktop_response_kind(&response);
    if operation != "poll"
        || elapsed_ms >= crate::desktop::POLL_HOLD_MS + 150
        || outcome != "active"
    {
        tracing::debug!(%peer, session_id, operation, outcome, elapsed_ms, "desktop receiver operation completed");
    } else {
        tracing::trace!(%peer, session_id, operation, outcome, elapsed_ms, "desktop receiver operation completed");
    }
    if let DesktopResponse::Unavailable { reason } = &response {
        tracing::warn!(%peer, session_id, operation, %reason, "desktop receiver operation unavailable");
    }
    // A computer crossing into this one checks this desktop against its
    // layout, so keep this computer's tile the right size.
    if let DesktopResponse::Snapshot { geometry, .. } | DesktopResponse::Prepared { geometry, .. } =
        &response
    {
        let (shared, geometry) = (shared.clone(), geometry.clone());
        tokio::spawn(async move { shared.fit_own_tile(&geometry).await });
    }
    if let DesktopRequest::Prepare { token, .. } = request
        && !matches!(response, DesktopResponse::Prepared { .. })
    {
        // A timed-out compositor call may still complete. Queue cleanup after it
        // on the same local stream before releasing this reservation.
        tracing::debug!(%peer, session_id, operation, "failed desktop prepare waiting for ordered cleanup");
        let _ = shared.desktop.call(DesktopRequest::Finish { token }).await;
        tracing::debug!(%peer, session_id, elapsed_ms = started.elapsed().as_millis() as u64, "failed desktop prepare cleanup completed");
    }
    if matches!(request, DesktopRequest::Finish { .. })
        || (matches!(request, DesktopRequest::Prepare { .. })
            && !matches!(response, DesktopResponse::Prepared { .. }))
    {
        let mut lease = shared.desktop.lease.lock().await;
        if lease.as_ref().is_some_and(|l| {
            l.peer == peer && l.session_id == session_id && Some(l.token) == request.token()
        }) {
            *lease = None;
        }
    }
    response
}

/// A desktop agent opts in by keeping this credential-checked stream open.
/// Every seat change rechecks it. The compositor barrier and daemon
/// reservation both expire after two seconds without a source Poll.
pub(super) async fn serve(
    shared: Arc<Shared>,
    mut stream: UnixStream,
    daemon_uid: u32,
) -> Result<()> {
    let id = shared.desktop.next.fetch_add(1, Ordering::Relaxed);
    let (sender, mut jobs) = mpsc::channel::<Job>(4);
    {
        let mut broker = shared.desktop.broker.lock().await;
        ensure!(broker.is_none(), "A desktop receiver is already connected");
        *broker = Some((id, sender));
    }
    tracing::info!(broker_id = id, "desktop agent connected");
    // A new agent starts from a clean Shell: no barriers, pointer shown.
    shared.desktop.local.send_modify(|_| {});
    // The desktop may have changed size while no agent was connected.
    tokio::spawn({
        let shared = shared.clone();
        async move {
            if let DesktopResponse::Snapshot { geometry, .. } = shared.desktop.snapshot().await {
                shared.fit_own_tile(&geometry).await;
            }
        }
    });
    let mut latest = shared.seat.clone();
    let mut seat = SeatGrace::new(latest.borrow_and_update().clone(), Instant::now());
    let mut window_asked = false;
    let result = async {
        write_message(&mut stream, &DesktopResponse::Finished).await?;
        loop {
            authorize_peer(&stream, daemon_uid, seat.state.active_authenticated_uid())?;
            // Someone is at this unlocked desktop, so a fresh install's
            // pairing window may open. An ssh install alone never opens it.
            if !window_asked && matches!(seat.state, SeatState::Unlocked(_)) {
                window_asked = true;
                shared.open_pairing_window().await;
            }
            let lease_deadline = shared.desktop.lease_deadline().await;
            tokio::select! {
                job = jobs.recv() => {
                    let Some(job) = job else {
                        break;
                    };
                    broker_job(&shared, &mut stream, &mut latest, &mut seat, daemon_uid, id, job).await?;
                }
                changed = latest.changed() => {
                    changed.context("the seat watch stopped")?;
                    seat.observe(latest.borrow_and_update().clone(), Instant::now());
                }
                () = sleep_until(seat.deadline()) => {
                    seat.observe(latest.borrow().clone(), Instant::now());
                }
                () = sleep_until(lease_deadline) => {
                    let expired = {
                        let mut lease = shared.desktop.lease.lock().await;
                        if lease.as_ref().is_some_and(|lease| {
                            lease.renewed.elapsed() >= Duration::from_millis(crate::desktop::LEASE_MS)
                        }) {
                            lease.take()
                        } else {
                            None
                        }
                    };
                    if let Some(expired) = expired {
                        tracing::warn!(broker_id = id, peer = %expired.peer, session_id = expired.session_id, elapsed_ms = expired.renewed.elapsed().as_millis() as u64, "desktop handoff lease expired");
                        if let Some(session) = shared
                            .sessions
                            .lock()
                            .await
                            .get(&expired.peer)
                            .filter(|session| session.id() == expired.session_id)
                        {
                            session.close(SessionCloseReason::BackendUnavailable);
                        }
                    }
                }
                // Between jobs the agent sends nothing, so readiness means it
                // closed. No second reader can consume a response frame.
                ready = stream.readable() => {
                    ready?;
                    let mut byte = [0u8; 1];
                    match stream.try_read(&mut byte) {
                        Ok(0) => break,
                        Ok(_) => bail!("Unexpected desktop bridge data"),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    match &result {
        Ok(()) => tracing::info!(broker_id = id, "desktop agent stopped"),
        // Logout and user switches hand the seat to a greeter or another uid.
        Err(error)
            if matches!(
                error.downcast_ref::<ControlError>(),
                Some(ControlError::Unauthorized(_))
            ) && !matches!(seat.state, SeatState::Unknown { .. }) =>
        {
            tracing::info!(broker_id = id, error = %format_args!("{error:#}"), "desktop agent stopped after the seat changed hands");
        }
        Err(error) => {
            tracing::warn!(broker_id = id, error = %format_args!("{error:#}"), "desktop agent stopped");
        }
    }
    shared
        .desktop
        .disconnected(id, &shared.sessions, &shared.runtime)
        .await;
    result
}

/// Runs one compositor call for a peer or for local cleanup, checking the
/// seat and the peer's authorization right before and after it.
async fn broker_job(
    shared: &Shared,
    stream: &mut UnixStream,
    latest: &mut watch::Receiver<SeatState>,
    seat: &mut SeatGrace,
    daemon_uid: u32,
    id: u64,
    job: Job,
) -> Result<()> {
    let operation = job.request.operation();
    let peer = job
        .origin
        .as_ref()
        .map(|(peer, _)| peer.as_str())
        .unwrap_or("local_cleanup");
    let session_id = job.origin.as_ref().map(|(_, id)| *id);
    if job.reply.is_closed() {
        tracing::debug!(broker_id = id, %peer, ?session_id, operation, "abandoned desktop broker job skipped");
        return Ok(());
    }
    let queue_ms = job.queued_at.elapsed().as_millis() as u64;
    if operation != "poll" {
        tracing::debug!(broker_id = id, %peer, ?session_id, operation, queue_ms, "desktop broker operation started");
    }
    seat.observe(latest.borrow_and_update().clone(), Instant::now());
    authorize_peer(stream, daemon_uid, seat.state.active_authenticated_uid())?;
    if let Some((peer, session_id)) = &job.origin {
        let _policy = shared.policy.lock().await;
        let config = shared.config.read().await;
        let gate = seat.state.injection_gate();
        let current_session = shared
            .sessions
            .lock()
            .await
            .get(peer)
            .is_some_and(|session| session.id() == *session_id);
        let current = matches!(gate, InjectionGate::Normal { .. })
            && receiver_authorized(&config, peer, gate)
            && current_session
            && shared.desktop.allows_session(peer, *session_id).await;
        if !current {
            tracing::warn!(broker_id = id, %peer, session_id, operation, "desktop authorization changed before compositor call");
            let _ = job
                .reply
                .send(AgentReply::Desktop(DesktopResponse::unavailable(
                    "Desktop authorization changed before the operation",
                )));
            return Ok(());
        }
    }
    let compositor_started = Instant::now();
    tracing::trace!(broker_id = id, %peer, ?session_id, operation, "desktop broker calling desktop compositor bridge");
    let response = exchange(stream, &job.request).await?;
    let compositor_ms = compositor_started.elapsed().as_millis() as u64;
    let outcome = response.kind();
    seat.observe(latest.borrow_and_update().clone(), Instant::now());
    authorize_peer(stream, daemon_uid, seat.state.active_authenticated_uid())?;
    let elapsed_ms = job.queued_at.elapsed().as_millis() as u64;
    if operation != "poll"
        || elapsed_ms >= crate::desktop::POLL_HOLD_MS + 150
        || outcome != "active"
    {
        tracing::debug!(broker_id = id, %peer, ?session_id, operation, outcome, queue_ms, compositor_ms, elapsed_ms, "desktop broker operation completed");
    } else {
        tracing::trace!(broker_id = id, %peer, ?session_id, operation, outcome, queue_ms, compositor_ms, elapsed_ms, "desktop broker operation completed");
    }
    if job.reply.send(response).is_err()
        && let AgentRequest::Handoff(DesktopRequest::Prepare { token, .. }) = job.request
    {
        tracing::debug!(
            broker_id = id,
            operation,
            "desktop prepare reply abandoned; cleaning compositor lease"
        );
        exchange(
            stream,
            &AgentRequest::Handoff(DesktopRequest::Finish { token }),
        )
        .await?;
    }
    Ok(())
}

/// Sends the agent one request and reads its answer. Clips cross only this
/// stream, so only it takes messages up to [`MAX_AGENT_MESSAGE`].
async fn exchange(stream: &mut UnixStream, request: &AgentRequest) -> Result<AgentReply> {
    write_message_within(stream, request, MAX_AGENT_MESSAGE).await?;
    let reply: AgentReply = tokio::time::timeout(
        reply_limit(request),
        read_message_within(stream, MAX_AGENT_MESSAGE),
    )
    .await
    .context("Desktop bridge response timed out")??;
    reply.validate()?;
    Ok(reply)
}

#[cfg(test)]
impl Hub {
    /// Stands in for a connected desktop agent on one 2560 by 1440
    /// monitor, which answers every request at once.
    pub(super) async fn answer_everything(&self) -> tokio::task::JoinHandle<()> {
        let (sender, mut jobs) = mpsc::channel::<Job>(4);
        *self.broker.lock().await = Some((0, sender));
        tokio::spawn(async move {
            while let Some(job) = jobs.recv().await {
                let response = match job.request {
                    AgentRequest::Handoff(DesktopRequest::Snapshot) => DesktopResponse::Snapshot {
                        geometry: crate::desktop::Geometry {
                            monitors: vec![crate::desktop::Rect {
                                x: 0,
                                y: 0,
                                width: 2560,
                                height: 1440,
                            }],
                        },
                        position: crate::desktop::Point { x: 10, y: 10 },
                    },
                    _ => DesktopResponse::Finished,
                };
                let _ = job.reply.send(AgentReply::Desktop(response));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::{ClipKind, MAX_CLIP_BYTES};

    #[tokio::test]
    async fn only_the_agent_stream_takes_a_whole_clip() {
        let (mut service, mut agent) = UnixStream::pair().unwrap();
        let full = |kind| ClipboardContents::Clip {
            kind,
            data: ClipData(vec![b'a'; MAX_CLIP_BYTES]),
        };
        let read = AgentReply::Clipboard(full(ClipKind::Text));
        let answer = read.clone();
        let fake = tokio::spawn(async move {
            let request: AgentRequest = read_message_within(&mut agent, MAX_AGENT_MESSAGE)
                .await
                .unwrap();
            assert_eq!(request, AgentRequest::Local(LocalRequest::ReadClipboard));
            write_message_within(&mut agent, &answer, MAX_AGENT_MESSAGE)
                .await
                .unwrap();
            let request: AgentRequest = read_message_within(&mut agent, MAX_AGENT_MESSAGE)
                .await
                .unwrap();
            assert!(matches!(
                request,
                AgentRequest::Local(LocalRequest::WriteClipboard { ref data, .. })
                    if data.0.len() == MAX_CLIP_BYTES
            ));
            write_message_within(
                &mut agent,
                &AgentReply::Desktop(DesktopResponse::Finished),
                MAX_AGENT_MESSAGE,
            )
            .await
            .unwrap();
            // Every other local stream refuses a frame this size, and hangs up
            // before reading all of it.
            let _ = write_message_within(&mut agent, &answer, MAX_AGENT_MESSAGE).await;
        });
        let reply = exchange(
            &mut service,
            &AgentRequest::Local(LocalRequest::ReadClipboard),
        )
        .await
        .unwrap();
        assert_eq!(reply, read);
        let write = LocalRequest::WriteClipboard {
            kind: ClipKind::Text,
            data: ClipData(vec![b'a'; MAX_CLIP_BYTES]),
        };
        let reply = exchange(&mut service, &AgentRequest::Local(write))
            .await
            .unwrap();
        assert_eq!(reply, AgentReply::Desktop(DesktopResponse::Finished));
        assert!(matches!(
            read_message::<_, AgentReply>(&mut service).await,
            Err(ControlError::TooLarge(_))
        ));
        drop(service);
        fake.await.unwrap();
    }

    #[tokio::test]
    async fn clipboard_requests_reach_the_agent_and_a_clip_never_answers_a_handoff() {
        let hub = Hub::default();
        let error = hub.read_clipboard().await.unwrap_err();
        assert!(format!("{error:#}").contains("desktop-agent"), "{error:#}");
        let (sender, mut jobs) = mpsc::channel::<Job>(4);
        *hub.broker.lock().await = Some((1, sender));
        let agent = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(job) = jobs.recv().await {
                let reply = match &job.request {
                    AgentRequest::Local(LocalRequest::ReadClipboard) if seen.is_empty() => {
                        AgentReply::Clipboard(ClipboardContents::TooLarge { bytes: 9 })
                    }
                    AgentRequest::Local(LocalRequest::ReadClipboard) => AgentReply::Desktop(
                        DesktopResponse::unavailable("GNOME integration unavailable"),
                    ),
                    AgentRequest::Local(_) => AgentReply::Desktop(DesktopResponse::Finished),
                    // An agent that answers a handoff with a clip.
                    AgentRequest::Handoff(_) => AgentReply::Clipboard(ClipboardContents::Empty),
                };
                seen.push(job.request.clone());
                let _ = job.reply.send(reply);
            }
            seen
        });
        assert_eq!(
            hub.read_clipboard().await.unwrap(),
            ClipboardContents::TooLarge { bytes: 9 }
        );
        let error = hub.read_clipboard().await.unwrap_err();
        assert_eq!(format!("{error:#}"), "GNOME integration unavailable");
        let clip = Clip::new(ClipKind::Text, b"hi".to_vec()).unwrap();
        hub.write_clipboard(clip).await.unwrap();
        hub.notify("Clipboard not shared".into()).await;
        assert!(
            matches!(hub.snapshot().await, DesktopResponse::Unavailable { .. }),
            "a clip never becomes a handoff reply"
        );
        *hub.broker.lock().await = None;
        let local = |request| AgentRequest::Local(request);
        assert_eq!(
            agent.await.unwrap(),
            [
                local(LocalRequest::ReadClipboard),
                local(LocalRequest::ReadClipboard),
                local(LocalRequest::WriteClipboard {
                    kind: ClipKind::Text,
                    data: ClipData(b"hi".to_vec()),
                }),
                local(LocalRequest::Notify {
                    message: "Clipboard not shared".into(),
                }),
                AgentRequest::Handoff(DesktopRequest::Snapshot),
            ]
        );
    }

    #[tokio::test]
    async fn disconnected_broker_releases_lease_before_waiting_for_sessions() {
        let hub = Hub::default();
        let (sender, _jobs) = mpsc::channel(1);
        *hub.broker.lock().await = Some((1, sender));
        *hub.lease.lock().await = Some(Lease {
            peer: "mac".into(),
            session_id: 4,
            token: 8,
            renewed: Instant::now(),
        });
        let sessions = Mutex::new(BTreeMap::new());
        let held_sessions = sessions.lock().await;
        let (runtime, _commands, _poll) = LinuxRuntimeControl::queue(1);
        let cleanup = hub.disconnected(1, &sessions, &runtime);
        tokio::pin!(cleanup);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut cleanup)
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), hub.allows_session("other", 5))
                .await
                .unwrap()
        );
        assert!(hub.broker.try_lock().unwrap().is_none());
        drop(held_sessions);
        tokio::time::timeout(Duration::from_millis(100), cleanup)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stale_broker_cleanup_preserves_reconnected_receiver() {
        let hub = Hub::default();
        let (sender, _jobs) = mpsc::channel(1);
        *hub.broker.lock().await = Some((2, sender));
        *hub.lease.lock().await = Some(Lease {
            peer: "mac".into(),
            session_id: 5,
            token: 9,
            renewed: Instant::now(),
        });
        let (runtime, mut commands, _poll) = LinuxRuntimeControl::queue(1);
        hub.disconnected(1, &Mutex::new(BTreeMap::new()), &runtime)
            .await;
        assert!(commands.try_recv().is_err(), "the live agent's focus stays");
        assert_eq!(hub.broker.lock().await.as_ref().unwrap().0, 2);
        assert!(
            hub.lease
                .lock()
                .await
                .as_ref()
                .unwrap()
                .permits("mac", 5, 9)
        );
        assert!(!hub.allows_session("mac", 4).await);
    }

    #[tokio::test]
    async fn disconnected_broker_resets_focus_before_another_can_connect() {
        let hub = Hub::default();
        let (sender, _jobs) = mpsc::channel(1);
        *hub.broker.lock().await = Some((1, sender));
        let (runtime, mut commands, _poll) = LinuxRuntimeControl::queue(1);
        hub.focus(&runtime, true).await;
        let sessions = Mutex::new(BTreeMap::new());
        let cleanup = hub.disconnected(1, &sessions, &runtime);
        tokio::pin!(cleanup);
        // The reset waits out the full queue instead of being dropped, and
        // keeps the broker slot until it is queued.
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut cleanup)
                .await
                .is_err()
        );
        assert!(hub.broker.try_lock().is_err());
        assert!(matches!(
            commands.try_recv(),
            Ok(RuntimeCommand::DesktopFocus { terminal: true })
        ));
        tokio::time::timeout(Duration::from_millis(100), cleanup)
            .await
            .unwrap();
        assert!(matches!(
            commands.try_recv(),
            Ok(RuntimeCommand::DesktopFocus { terminal: false })
        ));
        assert!(hub.broker.try_lock().unwrap().is_none());
        hub.focus(&runtime, true).await;
        assert!(commands.try_recv().is_err(), "no agent, no focus reports");
    }

    #[test]
    fn only_the_local_tiles_touching_edges_become_barriers() {
        use crate::app::layout_model::Monitor;
        let tile = |id: &str, x, y, width, height| Monitor {
            id: id.into(),
            label: id.into(),
            peer: (id != "local").then(|| id.into()),
            x,
            y,
            width,
            height,
        };
        let layout = Layout {
            monitors: vec![
                tile("local", 0, 0, 2000, 1000),
                // Touches the lower half of the right edge.
                tile("mac", 2000, 500, 1000, 1000),
                // Touches nothing.
                tile("far", 5000, 0, 100, 100),
            ],
        };
        let edges = outbound_edges(&layout);
        assert_eq!(
            edges,
            [(
                OutboundEdge {
                    edge: Edge::Right,
                    start: FRACTION_MAX / 2,
                    end: FRACTION_MAX,
                },
                "mac".to_owned()
            )]
        );
        let hub = Hub::default();
        hub.local.send_modify(|state| state.edges = edges);
        assert_eq!(hub.edge_peer(Edge::Right, 750_000).as_deref(), Some("mac"));
        assert_eq!(hub.edge_peer(Edge::Right, 100_000), None);
        assert_eq!(hub.edge_peer(Edge::Left, 750_000), None);
    }

    #[test]
    fn handoff_lease_rejects_wrong_peer_session_token_and_age() {
        let mut lease = Lease {
            peer: "mac".into(),
            session_id: 4,
            token: 8,
            renewed: Instant::now(),
        };
        assert!(lease.permits("mac", 4, 8));
        assert!(!lease.permits("other", 4, 8));
        assert!(!lease.permits("mac", 5, 8));
        assert!(!lease.permits("mac", 4, 9));
        lease.renewed = Instant::now() - Duration::from_millis(crate::desktop::LEASE_MS + 1);
        assert!(!lease.permits("mac", 4, 8));
    }
    #[tokio::test]
    async fn receiver_opt_in_and_single_owner_are_required() {
        let hub = Hub::default();
        assert!(matches!(
            hub.call(DesktopRequest::Snapshot).await,
            DesktopResponse::Unavailable { .. }
        ));
        *hub.lease.lock().await = Some(Lease {
            peer: "mac".into(),
            session_id: 4,
            token: 8,
            renewed: Instant::now(),
        });
        assert!(hub.allows_session("mac", 4).await);
        assert!(!hub.allows_session("mac", 5).await);
        assert!(!hub.allows_session("other", 4).await);
    }
}
