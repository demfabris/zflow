use super::*;
use crate::desktop::{DesktopRequest, DesktopResponse};
use crate::session::{desktop_operation, desktop_response_kind};
use anyhow::ensure;
use tokio::net::UnixStream;

struct Job {
    request: DesktopRequest,
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

#[derive(Default)]
pub(super) struct Hub {
    broker: Mutex<Option<(u64, mpsc::Sender<Job>)>>,
    lease: Mutex<Option<Lease>>,
    next: AtomicU64,
}

impl Hub {
    async fn disconnected(&self, id: u64, sessions: &Mutex<BTreeMap<String, SessionHandle>>) {
        let lease = {
            let mut broker = self.broker.lock().await;
            if broker.as_ref().is_none_or(|(current, _)| *current != id) {
                return;
            }
            let lease = self.lease.lock().await.take();
            *broker = None;
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

    pub async fn allows_session(&self, peer: &str, session_id: u64) -> bool {
        self.lease
            .lock()
            .await
            .as_ref()
            .is_none_or(|lease| lease.peer == peer && lease.session_id == session_id)
    }

    async fn call(&self, request: DesktopRequest) -> DesktopResponse {
        self.call_scoped(request, None).await
    }

    async fn call_scoped(
        &self,
        request: DesktopRequest,
        origin: Option<(String, u64)>,
    ) -> DesktopResponse {
        let started = Instant::now();
        let operation = desktop_operation(&request);
        let broker = self
            .broker
            .lock()
            .await
            .as_ref()
            .map(|(_, sender)| sender.clone());
        let Some(broker) = broker else {
            tracing::debug!(operation, "desktop request has no connected GUI broker");
            return DesktopResponse::unavailable(
                "Start receiving in the Linux zflow window and enable its GNOME integration",
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
                DesktopResponse::unavailable("The desktop receiver did not respond")
            }
            Err(_) => {
                tracing::warn!(
                    operation,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    timeout_ms = 700,
                    "desktop broker request timed out"
                );
                DesktopResponse::unavailable("The desktop receiver did not respond")
            }
        }
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
        let gate = *shared.seat_gate.read().await;
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
        .call_scoped(request.clone(), Some((peer.clone(), session_id)))
        .await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let outcome = desktop_response_kind(&response);
    if operation != "poll" || elapsed_ms >= 150 || outcome != "active" {
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

/// A GUI explicitly opts in by keeping this credential-checked stream open.
/// Every RPC and idle interval rechecks the active seat. The compositor barrier
/// and daemon reservation both expire after two seconds without a source Poll.
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
    tracing::info!(broker_id = id, "desktop GUI broker connected");
    let result=async {
        write_message(&mut stream,&DesktopResponse::Finished).await?;
        let mut interval=tokio::time::interval(Duration::from_millis(250));
        loop {
            tokio::select! {
                job=jobs.recv() => {
                    let Some(job)=job else {break;};
                    let operation = desktop_operation(&job.request);
                    let peer = job.origin.as_ref().map(|(peer,_)|peer.as_str()).unwrap_or("local_cleanup");
                    let session_id = job.origin.as_ref().map(|(_,id)|*id);
                    if job.reply.is_closed() {
                        tracing::debug!(broker_id = id, %peer, ?session_id, operation, "abandoned desktop broker job skipped");
                        continue;
                    }
                    let queue_ms = job.queued_at.elapsed().as_millis() as u64;
                    if operation != "poll" {
                        tracing::debug!(broker_id = id, %peer, ?session_id, operation, queue_ms, "desktop broker operation started");
                    }
                    let seat_started = Instant::now();
                    let seat=tokio::task::spawn_blocking(query_primary_seat).await?;
                    let seat_before_ms = seat_started.elapsed().as_millis() as u64;
                    if seat_before_ms >= 150 {
                        tracing::debug!(broker_id = id, %peer, ?session_id, operation, seat_before_ms, "slow desktop seat check before operation");
                    }
                    tracing::trace!(broker_id = id, %peer, ?session_id, operation, queue_ms, seat_before_ms, "desktop broker seat checked before operation");
                    authorize_peer(&stream,daemon_uid,seat.active_authenticated_uid())?;
                    if let Some((peer, session_id)) = &job.origin {
                        let _policy = shared.policy.lock().await;
                        let config = shared.config.read().await;
                        let gate = seat.injection_gate();
                        let current_session = shared.sessions.lock().await.get(peer).is_some_and(|s|s.id()==*session_id);
                        let current = matches!(gate,InjectionGate::Normal{..})
                            && receiver_authorized(&config,peer,gate)
                            && current_session
                            && shared.desktop.allows_session(peer,*session_id).await;
                        if !current {
                            tracing::warn!(broker_id = id, %peer, session_id, operation, "desktop authorization changed before compositor call");
                            let _=job.reply.send(DesktopResponse::unavailable("Desktop authorization changed before the operation"));
                            continue;
                        }
                    }
                    let compositor_started = Instant::now();
                    tracing::trace!(broker_id = id, %peer, ?session_id, operation, "desktop broker calling GUI compositor bridge");
                    write_message(&mut stream,&job.request).await?;
                    let response:DesktopResponse=tokio::time::timeout(Duration::from_millis(600),read_message(&mut stream)).await
                        .context("Desktop bridge response timed out")??;
                    response.validate()?;
                    let compositor_ms = compositor_started.elapsed().as_millis() as u64;
                    let outcome = desktop_response_kind(&response);
                    let seat_started = Instant::now();
                    let seat=tokio::task::spawn_blocking(query_primary_seat).await?;
                    let seat_after_ms = seat_started.elapsed().as_millis() as u64;
                    authorize_peer(&stream,daemon_uid,seat.active_authenticated_uid())?;
                    let elapsed_ms = job.queued_at.elapsed().as_millis() as u64;
                    if operation != "poll" || elapsed_ms >= 150 || outcome != "active" {
                        tracing::debug!(broker_id = id, %peer, ?session_id, operation, outcome, queue_ms, seat_before_ms, compositor_ms, seat_after_ms, elapsed_ms, "desktop broker operation completed");
                    } else {
                        tracing::trace!(broker_id = id, %peer, ?session_id, operation, outcome, queue_ms, seat_before_ms, compositor_ms, seat_after_ms, elapsed_ms, "desktop broker operation completed");
                    }
                    if job.reply.send(response).is_err()
                        && let DesktopRequest::Prepare {token,..}=job.request {
                        tracing::debug!(broker_id = id, operation, "desktop prepare reply abandoned; cleaning compositor lease");
                        write_message(&mut stream,&DesktopRequest::Finish{token}).await?;
                        let _:DesktopResponse=tokio::time::timeout(Duration::from_millis(600),read_message(&mut stream)).await??;
                    }
                }
                _=interval.tick() => {
                    let seat=tokio::task::spawn_blocking(query_primary_seat).await?;
                    authorize_peer(&stream,daemon_uid,seat.active_authenticated_uid())?;
                    let expired={
                        let mut lease=shared.desktop.lease.lock().await;
                        if lease.as_ref().is_some_and(|l|l.renewed.elapsed() >= Duration::from_millis(crate::desktop::LEASE_MS)) {lease.take()} else {None}
                    };
                    if let Some(expired)=expired {
                        tracing::warn!(broker_id = id, peer = %expired.peer, session_id = expired.session_id, elapsed_ms = expired.renewed.elapsed().as_millis() as u64, "desktop handoff lease expired");
                        if let Some(session)=shared.sessions.lock().await.get(&expired.peer).filter(|s|s.id()==expired.session_id) {
                            session.close(SessionCloseReason::BackendUnavailable);
                        }
                    }
                    // Unix stream readiness detects a closed GUI without creating
                    // another reader that could consume a response frame.
                    let mut byte=[0u8;1];
                    match stream.try_read(&mut byte) {
                        Ok(0)=>bail!("Desktop window disconnected"),
                        Ok(_)=>bail!("Unexpected desktop bridge data"),
                        Err(error) if error.kind()==std::io::ErrorKind::WouldBlock=>{},
                        Err(error)=>return Err(error.into()),
                    }
                }
            }
        }
        Ok(())
    }.await;
    if let Err(error) = &result {
        tracing::warn!(broker_id = id, error = %format_args!("{error:#}"), "desktop GUI broker stopped");
    } else {
        tracing::info!(broker_id = id, "desktop GUI broker stopped");
    }
    shared.desktop.disconnected(id, &shared.sessions).await;
    result
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
        let cleanup = hub.disconnected(1, &sessions);
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
        hub.disconnected(1, &Mutex::new(BTreeMap::new())).await;
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
