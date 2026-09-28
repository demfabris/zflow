use super::*;
use crate::app::layout_model::Layout;
use crate::control::ControlError;
use crate::desktop::{DesktopRequest, DesktopResponse};
use crate::desktop::{Edge, FRACTION_MAX};
use crate::linux::SeatState;
use crate::peer_view::{AgentRequest, LocalRequest, OutboundEdge};
use crate::session::{desktop_operation, desktop_response_kind};
use anyhow::ensure;
use tokio::net::UnixStream;

struct Job {
    request: AgentRequest,
    origin: Option<(String, u64)>,
    reply: tokio::sync::oneshot::Sender<DesktopResponse>,
    queued_at: Instant,
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
struct LocalState {
    /// Edges that lead to another computer, with that computer's name.
    edges: Vec<(OutboundEdge, String)>,
    sending: bool,
}

#[derive(Default)]
pub(super) struct Hub {
    local: std::sync::Mutex<LocalState>,
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

    fn local_state(&self) -> std::sync::MutexGuard<'_, LocalState> {
        self.local.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The computer whose edge the pointer pushed against, if any.
    pub(super) fn edge_peer(&self, edge: Edge, position: u32) -> Option<String> {
        self.local_state()
            .edges
            .iter()
            .find(|(range, _)| range.edge == edge && (range.start..=range.end).contains(&position))
            .map(|(_, peer)| peer.clone())
    }

    async fn call_scoped(
        &self,
        request: AgentRequest,
        origin: Option<(String, u64)>,
    ) -> DesktopResponse {
        let started = Instant::now();
        let operation = request.operation();
        let broker = self
            .broker
            .lock()
            .await
            .as_ref()
            .map(|(_, sender)| sender.clone());
        let Some(broker) = broker else {
            tracing::debug!(operation, "desktop request has no connected desktop agent");
            return DesktopResponse::unavailable(
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
            return DesktopResponse::unavailable("The desktop receiver is busy or stopped");
        }
        match tokio::time::timeout(Duration::from_millis(700), receipt).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => {
                tracing::warn!(
                    operation,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "desktop broker reply channel closed"
                );
                DesktopResponse::unavailable("The desktop did not respond")
            }
            Err(_) => {
                tracing::warn!(
                    operation,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    timeout_ms = 700,
                    "desktop broker request timed out"
                );
                DesktopResponse::unavailable("The desktop did not respond")
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

pub(super) fn set_edges(shared: &Arc<Shared>, edges: Vec<(OutboundEdge, String)>) {
    shared.desktop.local_state().edges = edges;
    sync_local(shared);
}

pub(super) fn set_sending(shared: &Arc<Shared>, sending: bool) {
    shared.desktop.local_state().sending = sending;
    sync_local(shared);
}

/// Sends the agent the current edges and sending state. These tasks can run
/// out of order, so each sends whatever is current when it runs.
fn sync_local(shared: &Arc<Shared>) {
    let shared = shared.clone();
    tokio::spawn(async move {
        let (edges, active) = {
            let state = shared.desktop.local_state();
            (
                state.edges.iter().map(|(range, _)| *range).collect(),
                state.sending,
            )
        };
        for request in [
            LocalRequest::Edges { edges },
            LocalRequest::Sending { active },
        ] {
            let response = shared
                .desktop
                .call_scoped(AgentRequest::Local(request), None)
                .await;
            if let DesktopResponse::Unavailable { reason } = response {
                tracing::debug!(%reason, "desktop agent did not take local state");
                break;
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
    sync_local(&shared);
    let mut latest = shared.seat.clone();
    let mut seat = SeatGrace::new(latest.borrow_and_update().clone(), Instant::now());
    let result = async {
        write_message(&mut stream, &DesktopResponse::Finished).await?;
        loop {
            authorize_peer(&stream, daemon_uid, seat.state.active_authenticated_uid())?;
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
            let _ = job.reply.send(DesktopResponse::unavailable(
                "Desktop authorization changed before the operation",
            ));
            return Ok(());
        }
    }
    let compositor_started = Instant::now();
    tracing::trace!(broker_id = id, %peer, ?session_id, operation, "desktop broker calling desktop compositor bridge");
    write_message(stream, &job.request).await?;
    let response: DesktopResponse =
        tokio::time::timeout(Duration::from_millis(600), read_message(stream))
            .await
            .context("Desktop bridge response timed out")??;
    response.validate()?;
    let compositor_ms = compositor_started.elapsed().as_millis() as u64;
    let outcome = desktop_response_kind(&response);
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
        write_message(stream, &DesktopRequest::Finish { token }).await?;
        let _: DesktopResponse =
            tokio::time::timeout(Duration::from_millis(600), read_message(stream)).await??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        hub.local_state().edges = edges;
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
