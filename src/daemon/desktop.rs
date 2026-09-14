use super::*;
use crate::desktop::{DesktopRequest, DesktopResponse};
use anyhow::ensure;
use tokio::net::UnixStream;

struct Job {
    request: DesktopRequest,
    origin: Option<(String, u64)>,
    reply: tokio::sync::oneshot::Sender<DesktopResponse>,
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
        let broker = self
            .broker
            .lock()
            .await
            .as_ref()
            .map(|(_, sender)| sender.clone());
        let Some(broker) = broker else {
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
            })
            .is_err()
        {
            return DesktopResponse::unavailable("The desktop receiver is busy or stopped");
        }
        match tokio::time::timeout(Duration::from_millis(700), receipt).await {
            Ok(Ok(response)) => response,
            _ => DesktopResponse::unavailable("The desktop receiver did not respond"),
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
    if let DesktopRequest::Prepare { token, .. } = request
        && !matches!(response, DesktopResponse::Prepared { .. })
    {
        // A timed-out compositor call may still complete. Queue cleanup after it
        // on the same local stream before releasing this reservation.
        let _ = shared.desktop.call(DesktopRequest::Finish { token }).await;
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
    let result=async {
        write_message(&mut stream,&DesktopResponse::Finished).await?;
        let mut interval=tokio::time::interval(Duration::from_millis(250));
        loop {
            tokio::select! {
                job=jobs.recv() => {
                    let Some(job)=job else {break;};
                    if job.reply.is_closed() { continue; }
                    let seat=tokio::task::spawn_blocking(query_primary_seat).await?;
                    authorize_peer(&stream,daemon_uid,seat.active_authenticated_uid())?;
                    if let Some((peer, session_id)) = &job.origin {
                        let _policy = shared.policy.lock().await;
                        let config = shared.config.read().await;
                        let gate = seat.injection_gate();
                        let current = matches!(gate,InjectionGate::Normal{..})
                            && receiver_authorized(&config,peer,gate)
                            && shared.sessions.lock().await.get(peer).is_some_and(|s|s.id()==*session_id)
                            && shared.desktop.allows_session(peer,*session_id).await;
                        if !current {
                            let _=job.reply.send(DesktopResponse::unavailable("Desktop authorization changed before the operation"));
                            continue;
                        }
                    }
                    write_message(&mut stream,&job.request).await?;
                    let response:DesktopResponse=tokio::time::timeout(Duration::from_millis(600),read_message(&mut stream)).await
                        .context("Desktop bridge response timed out")??;
                    response.validate()?;
                    let seat=tokio::task::spawn_blocking(query_primary_seat).await?;
                    authorize_peer(&stream,daemon_uid,seat.active_authenticated_uid())?;
                    if job.reply.send(response).is_err()
                        && let DesktopRequest::Prepare {token,..}=job.request {
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
    {
        let mut broker = shared.desktop.broker.lock().await;
        if broker.as_ref().is_some_and(|(current, _)| *current == id) {
            *broker = None;
        }
    }
    if let Some(lease) = shared.desktop.lease.lock().await.take()
        && let Some(session) = shared
            .sessions
            .lock()
            .await
            .get(&lease.peer)
            .filter(|s| s.id() == lease.session_id)
    {
        session.close(SessionCloseReason::BackendUnavailable);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
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
