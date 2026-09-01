use std::{
    collections::BTreeMap,
    fs,
    net::SocketAddr,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use quinn::Endpoint;
use tokio::{
    net::UnixListener,
    sync::{Mutex, RwLock, Semaphore, mpsc},
};

use crate::{
    config::{Config, PeerConfig, PeerPermissions},
    control::{
        DaemonStatus, OwnershipStatus, Request, Response, authorize_peer, read_message,
        write_message,
    },
    core::{
        ActivationId, InputCapability, ReceiverEffect, SessionCloseReason, SessionContext,
        SessionEpoch, TransportGeneration,
    },
    discovery::{Advertisement, Discovery, DiscoveryEvent, local_unicast_addresses},
    identity::Identity,
    linux::{InjectionGate, OwnershipPhase, query_primary_seat},
    runtime::{
        LinuxRuntime, LinuxRuntimeConfig, LinuxRuntimeControl, RuntimeCloseReason, RuntimeCommand,
        RuntimeEvent,
    },
    session::{SessionEvent, SessionEventKind, SessionHandle, SessionOptions, start_session},
    transport::{
        InputConnection, InputServerConfig, accept_input, connect_input, input_client_config,
        input_server_config_for_peers,
    },
    wire::CURRENT_PROTOCOL_VERSION,
};

const SESSION_EVENT_CAPACITY: usize = 1_024;
const ACCEPT_EVENT_CAPACITY: usize = 64;
const RUNTIME_DRAIN_INTERVAL: Duration = Duration::from_millis(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const LOCAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const TERMINAL_SEND_TIMEOUT: Duration = Duration::from_millis(250);
const DISCOVERY_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const DISCOVERY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_METRICS_HISTORY: usize = 128;
const MAX_PENDING_HANDSHAKES: usize = 64;
const MAX_LOCAL_CLIENTS: usize = 64;

pub fn run(config_path: PathBuf) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zflow=info".into()),
        )
        .init();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run_async(config_path))
}

async fn run_async(config_path: PathBuf) -> Result<()> {
    let config = Config::load(&config_path)
        .with_context(|| format!("failed to load {}", config_path.display()))?;
    validate_peer_identities(&config)?;
    SessionOptions::from_config(&config)?;
    fs::create_dir_all(&config.daemon.state_dir)?;
    fs::set_permissions(&config.daemon.state_dir, fs::Permissions::from_mode(0o700))?;
    let identity = Arc::new(Identity::load_or_create(&config.daemon.state_dir)?);
    let process_epoch = random_epoch()?;
    let runtime = LinuxRuntime::spawn(LinuxRuntimeConfig::from_config(&config))?;

    let endpoint = Endpoint::client(config.transport.listen).with_context(|| {
        format!(
            "failed to bind input QUIC socket at {}",
            config.transport.listen
        )
    })?;
    let server_config = build_server_config(&identity, &config)?;
    endpoint.set_server_config(server_config.as_ref().map(InputServerConfig::quinn_config));

    let socket_path = config.daemon.control_socket.clone();
    prepare_socket_path(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o660))?;
    let _socket_guard = SocketGuard(socket_path.clone());

    let (session_events, mut session_event_rx) = mpsc::channel(SESSION_EVENT_CAPACITY);
    let (accepted_tx, mut accepted_rx) = mpsc::channel(ACCEPT_EVENT_CAPACITY);
    let initial_seat = tokio::task::spawn_blocking(query_primary_seat)
        .await
        .context("initial active-seat query failed")?;
    let shared = Arc::new(Shared {
        config: RwLock::new(config.clone()),
        config_mutation: Mutex::new(()),
        policy: Mutex::new(()),
        policy_generation: AtomicU64::new(1),
        config_path,
        identity_fingerprint: identity.fingerprint_hex(),
        identity,
        process_epoch,
        next_generation: AtomicU64::new(1),
        next_activation: AtomicU64::new(1),
        endpoint,
        server_config: RwLock::new(server_config),
        runtime: runtime.control(),
        sessions: Mutex::new(BTreeMap::new()),
        metrics_history: Mutex::new(BTreeMap::new()),
        arming_started: Mutex::new(None),
        active_outbound: Mutex::new(None),
        inbound_owner: Mutex::new(None),
        seat_gate: RwLock::new(initial_seat.injection_gate()),
        session_events,
    });
    let mut discovery = start_discovery(&config, shared.endpoint.local_addr()?);
    let daemon_uid = nix::unistd::geteuid().as_raw();
    let handshake_slots = Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES));
    let session_setup_slots = Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES));
    let local_slots = Arc::new(Semaphore::new(MAX_LOCAL_CLIENTS));
    let seat_watcher = tokio::spawn(watch_seat(shared.clone()));
    let mut runtime_tick = tokio::time::interval(RUNTIME_DRAIN_INTERVAL);
    runtime_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut discovery_retry = tokio::time::interval(DISCOVERY_RETRY_INTERVAL);
    discovery_retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    tracing::info!(
        identity = %shared.identity_fingerprint,
        control_socket = %socket_path.display(),
        input_listen = %shared.endpoint.local_addr()?,
        "headless input daemon started"
    );

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let Ok(permit) = local_slots.clone().try_acquire_owned() else {
                    tracing::warn!("local control connection limit reached");
                    continue;
                };
                let shared = shared.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(error) = tokio::time::timeout(
                        LOCAL_REQUEST_TIMEOUT,
                        handle_client(stream, shared, daemon_uid),
                    )
                    .await
                    .map_err(|_| anyhow!("local control request timed out"))
                    .and_then(|result| result)
                    {
                        tracing::warn!(%error, "local control request failed");
                    }
                });
            }
            incoming = shared.endpoint.accept() => {
                let Some(incoming) = incoming else {
                    bail!("input QUIC endpoint stopped accepting connections");
                };
                let Ok(permit) = handshake_slots.clone().try_acquire_owned() else {
                    incoming.refuse();
                    tracing::warn!("input handshake limit reached");
                    continue;
                };
                let config = shared.server_config.read().await.clone();
                let accepted_tx = accepted_tx.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let result = match config {
                        Some(config) => tokio::time::timeout(
                            CONNECT_TIMEOUT,
                            accept_input(incoming, &config),
                        )
                        .await
                        .map_err(|_| "input handshake timed out".to_owned())
                        .and_then(|result| result.map_err(|error| error.to_string())),
                        None => Err("input listener has no authorized peers".to_owned()),
                    };
                    let _ = accepted_tx.send(result).await;
                });
            }
            accepted = accepted_rx.recv() => {
                if let Some(accepted) = accepted {
                    match accepted {
                        Ok(connection) => {
                            let Ok(permit) = session_setup_slots.clone().try_acquire_owned() else {
                                connection.close();
                                tracing::warn!("authenticated session setup limit reached");
                                continue;
                            };
                            let shared = shared.clone();
                            tokio::spawn(async move {
                                let _permit = permit;
                                if let Err(error) = tokio::time::timeout(
                                    CONNECT_TIMEOUT,
                                    shared.accept_connection(connection),
                                )
                                .await
                                .map_err(|_| anyhow!("authenticated session negotiation timed out"))
                                .and_then(|result| result)
                                {
                                    tracing::warn!(%error, "authenticated input connection was rejected");
                                }
                            });
                        }
                        Err(error) => tracing::warn!(%error, "input handshake failed"),
                    }
                }
            }
            event = session_event_rx.recv() => {
                let Some(event) = event else {
                    bail!("session event router stopped");
                };
                shared.handle_session_event(event).await?;
            }
            _ = runtime_tick.tick() => drain_runtime(&runtime, &shared).await?,
            signal = next_discovery_signal(discovery.as_ref()), if discovery.is_some() => {
                match signal {
                    DiscoverySignal::Event(Ok(DiscoveryEvent::Candidate(candidate))) => {
                        tracing::trace!(
                            candidates = candidate.socket_addresses().len(),
                            "received untrusted discovery hint"
                        );
                    }
                    DiscoverySignal::Event(Ok(DiscoveryEvent::Removed(_))) => {}
                    DiscoverySignal::Event(Ok(DiscoveryEvent::Stopped)) => {
                        stop_discovery(&mut discovery).await;
                    }
                    DiscoverySignal::Event(Err(error)) => {
                        tracing::warn!(%error, "mDNS browsing stopped");
                        stop_discovery(&mut discovery).await;
                    }
                    DiscoverySignal::Daemon(Ok(error)) => {
                        tracing::warn!(%error, "mDNS daemon error");
                        stop_discovery(&mut discovery).await;
                    }
                    DiscoverySignal::Daemon(Err(error)) => {
                        tracing::warn!(%error, "mDNS monitoring stopped");
                        stop_discovery(&mut discovery).await;
                    }
                }
            }
            _ = discovery_retry.tick(), if discovery.is_none() => {
                let config = shared.config.read().await.clone();
                discovery = start_discovery(&config, shared.endpoint.local_addr()?);
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                break;
            }
        }
    }

    shared
        .close_all(SessionCloseReason::BackendUnavailable)
        .await;
    seat_watcher.abort();
    shared.endpoint.close(0_u32.into(), b"daemon shutdown");
    stop_discovery(&mut discovery).await;
    runtime.shutdown()?;
    Ok(())
}

struct Shared {
    config: RwLock<Config>,
    config_mutation: Mutex<()>,
    /// Serializes authorization commits with session insertion and injection.
    policy: Mutex<()>,
    policy_generation: AtomicU64,
    config_path: PathBuf,
    identity: Arc<Identity>,
    identity_fingerprint: String,
    process_epoch: SessionEpoch,
    next_generation: AtomicU64,
    next_activation: AtomicU64,
    endpoint: Endpoint,
    server_config: RwLock<Option<InputServerConfig>>,
    runtime: LinuxRuntimeControl,
    sessions: Mutex<BTreeMap<String, SessionHandle>>,
    metrics_history: Mutex<BTreeMap<String, crate::metrics::SessionMetricsSnapshot>>,
    arming_started: Mutex<Option<(String, Instant)>>,
    active_outbound: Mutex<Option<ActiveOutbound>>,
    inbound_owner: Mutex<Option<(String, u64)>>,
    seat_gate: RwLock<InjectionGate>,
    session_events: mpsc::Sender<SessionEvent>,
}

#[derive(Clone)]
struct ActiveOutbound {
    peer: String,
    session_id: u64,
    context: SessionContext,
}

impl Shared {
    async fn status(&self) -> DaemonStatus {
        let runtime = self.runtime.status();
        let active = self.active_outbound.lock().await.clone();
        let mut metrics = self.metrics_history.lock().await.clone();
        metrics.extend(
            self.sessions
                .lock()
                .await
                .iter()
                .map(|(peer, session)| (peer.clone(), session.metrics_snapshot())),
        );
        DaemonStatus {
            identity: self.identity_fingerprint.clone(),
            ownership: ownership_status(runtime.ownership),
            selected_peer: runtime
                .selected_peer
                .or_else(|| active.as_ref().map(|active| active.peer.clone())),
            session_epoch: active
                .as_ref()
                .map(|active| encode_hex(&active.context.session_epoch.0)),
            transport_generation: active
                .as_ref()
                .map(|active| active.context.transport_generation.0),
            activation_id: active.as_ref().map(|active| active.context.activation_id.0),
            metrics,
        }
    }

    async fn activate(self: &Arc<Self>, peer: &str) -> Result<()> {
        if self.runtime.status().ownership != OwnershipPhase::Idle {
            bail!("local input ownership is not idle");
        }
        let record = self
            .config
            .read()
            .await
            .peers
            .get(peer)
            .cloned()
            .with_context(|| format!("unknown peer {peer}"))?;
        require_outbound_permission(peer, &record)?;
        let session = self.ensure_session(peer, &record).await?;
        let _policy = self.policy.lock().await;
        let current = self
            .config
            .read()
            .await
            .peers
            .get(peer)
            .cloned()
            .with_context(|| format!("peer {peer} was revoked during connection setup"))?;
        require_outbound_permission(peer, &current)?;
        if current.spki_der_hex != record.spki_der_hex
            || !self
                .sessions
                .lock()
                .await
                .get(peer)
                .is_some_and(|current| current.id() == session.id())
        {
            session.close(SessionCloseReason::PermissionRevoked);
            bail!("peer {peer} authorization changed during connection setup");
        }
        self.runtime
            .send(RuntimeCommand::Activate {
                peer: peer.to_owned(),
            })
            .map_err(|error| anyhow!(error))
    }

    async fn ensure_session(
        self: &Arc<Self>,
        peer: &str,
        record: &PeerConfig,
    ) -> Result<SessionHandle> {
        if let Some(existing) = self.sessions.lock().await.get(peer).cloned() {
            return Ok(existing);
        }
        if record.addresses.is_empty() {
            bail!("peer {peer} has no configured input address");
        }
        let policy_generation = self.policy_generation.load(Ordering::Acquire);
        let connection = race_connect(
            &self.endpoint,
            &self.identity,
            record.spki_der()?,
            &record.addresses,
        )
        .await?;
        let generation = self.allocate_generation()?;
        let options = {
            let config = self.config.read().await;
            SessionOptions::from_config(&config)?
        };
        let session = start_session(
            connection,
            peer.to_owned(),
            generation,
            options,
            self.session_events.clone(),
        )
        .await?;
        let _policy = self.policy.lock().await;
        let current = self.config.read().await.peers.get(peer).cloned();
        if self.policy_generation.load(Ordering::Acquire) != policy_generation
            || current.as_ref() != Some(record)
            || current
                .as_ref()
                .is_none_or(|current| require_outbound_permission(peer, current).is_err())
        {
            session.close(SessionCloseReason::PermissionRevoked);
            bail!("peer {peer} authorization changed during connection negotiation");
        }
        let mut sessions = self.sessions.lock().await;
        if let Some(existing) = sessions.get(peer).cloned() {
            session.close(SessionCloseReason::Superseded);
            return Ok(existing);
        }
        sessions.insert(peer.to_owned(), session.clone());
        Ok(session)
    }

    async fn accept_connection(self: &Arc<Self>, connection: InputConnection) -> Result<()> {
        let peer_spki = connection.peer_spki().to_vec();
        let policy_generation = self.policy_generation.load(Ordering::Acquire);
        let peer = {
            let config = self.config.read().await;
            peer_name_for_spki(&config, connection.peer_spki())?
        };
        if self.sessions.lock().await.contains_key(&peer) {
            connection.close();
            bail!("peer {peer} already has an established input connection");
        }
        let generation = self.allocate_generation()?;
        let options = {
            let config = self.config.read().await;
            SessionOptions::from_config(&config)?
        };
        let session = start_session(
            connection,
            peer.clone(),
            generation,
            options,
            self.session_events.clone(),
        )
        .await?;
        let _policy = self.policy.lock().await;
        let current_peer = {
            let config = self.config.read().await;
            peer_name_for_spki(&config, &peer_spki)
        };
        if self.policy_generation.load(Ordering::Acquire) != policy_generation
            || !matches!(current_peer.as_deref(), Ok(current) if current == peer)
        {
            session.close(SessionCloseReason::PermissionRevoked);
            bail!("peer authorization changed during inbound connection negotiation");
        }
        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(&peer) {
            session.close(SessionCloseReason::Superseded);
            bail!("peer {peer} established another input connection during negotiation");
        }
        sessions.insert(peer, session);
        Ok(())
    }

    fn allocate_generation(&self) -> Result<TransportGeneration> {
        let value = self.next_generation.fetch_add(1, Ordering::AcqRel);
        if value == 0 || value == u64::MAX {
            bail!("transport generation exhausted");
        }
        Ok(TransportGeneration(value))
    }

    fn allocate_activation(&self) -> Result<ActivationId> {
        let value = self.next_activation.fetch_add(1, Ordering::AcqRel);
        if value == 0 || value == u64::MAX {
            bail!("activation identifier exhausted");
        }
        Ok(ActivationId(value))
    }

    async fn begin_outbound(&self, peer: &str) -> Result<()> {
        let _policy = self.policy.lock().await;
        let record = self
            .config
            .read()
            .await
            .peers
            .get(peer)
            .cloned()
            .with_context(|| format!("peer {peer} was revoked before capture armed"))?;
        require_outbound_permission(peer, &record)?;
        let session = self
            .sessions
            .lock()
            .await
            .get(peer)
            .cloned()
            .with_context(|| format!("peer {peer} disconnected before capture armed"))?;
        let context = SessionContext {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            session_epoch: self.process_epoch,
            transport_generation: session.generation(),
            activation_id: self.allocate_activation()?,
        };
        session.begin_outbound(context)?;
        *self.active_outbound.lock().await = Some(ActiveOutbound {
            peer: peer.to_owned(),
            session_id: session.id(),
            context,
        });
        Ok(())
    }

    async fn end_outbound(&self, reason: SessionCloseReason) -> Result<()> {
        let active = self.active_outbound.lock().await.take();
        let Some(active) = active else {
            bail!("runtime requested a terminal state without an active outbound session");
        };
        let session = self
            .sessions
            .lock()
            .await
            .get(&active.peer)
            .filter(|session| session.id() == active.session_id)
            .cloned()
            .context("active outbound session disconnected before terminal state")?;
        session.end_outbound(reason).await
    }

    async fn finish_runtime_terminal(&self, reason: SessionCloseReason) -> Result<()> {
        let active_session = if let Some(active) = self.active_outbound.lock().await.clone() {
            self.sessions
                .lock()
                .await
                .get(&active.peer)
                .filter(|session| session.id() == active.session_id)
                .cloned()
        } else {
            None
        };
        let result = tokio::time::timeout(TERMINAL_SEND_TIMEOUT, self.end_outbound(reason)).await;
        let transport_live = matches!(result, Ok(Ok(())));
        if !transport_live {
            if let Ok(Err(error)) = &result {
                tracing::warn!(%error, "outbound terminal state could not be sent");
            } else {
                tracing::warn!("outbound terminal state timed out");
            }
            // Closing the transport is the authoritative fallback: the peer's
            // receiver lifecycle releases held state before local ungrab.
            if let Some(session) = active_session {
                session.close(SessionCloseReason::LocalRelease);
            }
        }
        self.runtime
            .send_critical(
                RuntimeCommand::TerminalSent { transport_live },
                TERMINAL_SEND_TIMEOUT,
            )
            .map_err(|error| anyhow!(error))
    }

    async fn route_capture(&self, frame: crate::linux::CapturedDeviceFrame) -> Result<()> {
        let _policy = self.policy.lock().await;
        let active = self
            .active_outbound
            .lock()
            .await
            .clone()
            .context("captured input has no active outbound session")?;
        let record = self
            .config
            .read()
            .await
            .peers
            .get(&active.peer)
            .cloned()
            .with_context(|| format!("peer {} was revoked during capture", active.peer))?;
        require_outbound_permission(&active.peer, &record)?;
        self.sessions
            .lock()
            .await
            .get(&active.peer)
            .filter(|session| session.id() == active.session_id)
            .cloned()
            .with_context(|| format!("peer {} disconnected during capture", active.peer))?
            .capture(frame)
    }
}

async fn handle_client(
    mut stream: tokio::net::UnixStream,
    shared: Arc<Shared>,
    daemon_uid: u32,
) -> Result<()> {
    let seat = tokio::task::spawn_blocking(query_primary_seat)
        .await
        .context("active-seat query task failed")?;
    authorize_peer(&stream, daemon_uid, seat.active_authenticated_uid())?;
    let request: Request = read_message(&mut stream).await?;
    let response = match dispatch_result(request, &shared).await {
        Ok(response) => response,
        Err(error) => Response::Error {
            message: error.to_string(),
        },
    };
    write_message(&mut stream, &response).await?;
    Ok(())
}

async fn dispatch_result(request: Request, shared: &Arc<Shared>) -> Result<Response> {
    match request {
        Request::Status => Ok(Response::Status(Box::new(shared.status().await))),
        Request::ReloadConfig => {
            let _mutation = shared.config_mutation.lock().await;
            shared
                .apply_config_locked(Config::load(&shared.config_path)?, false)
                .await?;
            Ok(Response::Ack)
        }
        Request::ListPeers => Ok(Response::Peers {
            peers: shared
                .config
                .read()
                .await
                .peers
                .iter()
                .map(|(name, peer)| (name.clone(), peer.permissions))
                .collect(),
        }),
        Request::AddPeer { peer, record } => {
            let _mutation = shared.config_mutation.lock().await;
            let mut config = shared.config.read().await.clone();
            config.peers.insert(peer, record);
            shared.apply_config_locked(config, true).await?;
            Ok(Response::Ack)
        }
        Request::RevokePeer { peer } => {
            let _mutation = shared.config_mutation.lock().await;
            let mut config = shared.config.read().await.clone();
            if config.peers.remove(&peer).is_none() {
                bail!("unknown peer {peer}");
            }
            shared.apply_config_locked(config, true).await?;
            Ok(Response::Ack)
        }
        Request::SetPeerPermissions { peer, permissions } => {
            let _mutation = shared.config_mutation.lock().await;
            let mut config = shared.config.read().await.clone();
            config
                .peers
                .get_mut(&peer)
                .with_context(|| format!("unknown peer {peer}"))?
                .permissions = permissions;
            shared.apply_config_locked(config, true).await?;
            Ok(Response::Ack)
        }
        Request::Local => {
            shared
                .runtime
                .send(RuntimeCommand::Release {
                    transport_live: true,
                })
                .map_err(|error| anyhow!(error))?;
            Ok(Response::Ack)
        }
        Request::Activate { peer } => {
            shared.activate(&peer).await?;
            Ok(Response::Ack)
        }
    }
}

async fn drain_runtime(runtime: &LinuxRuntime, shared: &Arc<Shared>) -> Result<()> {
    for _ in 0..1_024 {
        match runtime.events().try_recv() {
            Ok(event) => handle_runtime_event(event, shared).await?,
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                bail!("Linux input runtime event channel closed")
            }
        }
    }
    for _ in 0..4_096 {
        match runtime.captured_frames().try_recv() {
            Ok(frame) => {
                if let Err(error) = shared.route_capture(frame).await {
                    tracing::warn!(%error, "captured input could not reach its peer");
                    shared
                        .runtime
                        .send_critical(
                            RuntimeCommand::Release {
                                transport_live: false,
                            },
                            TERMINAL_SEND_TIMEOUT,
                        )
                        .map_err(|error| anyhow!(error))?;
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                bail!("Linux input runtime capture channel closed")
            }
        }
    }
    Ok(())
}

async fn handle_runtime_event(event: RuntimeEvent, shared: &Arc<Shared>) -> Result<()> {
    match event {
        RuntimeEvent::Ready => tracing::info!("Linux input runtime is ready"),
        RuntimeEvent::ActivationChord => {
            let eligible = {
                let config = shared.config.read().await;
                eligible_outbound_peers(&config)
            };
            if eligible.len() == 1 {
                let shared = shared.clone();
                let peer = eligible.into_iter().next().expect("one eligible peer");
                tokio::spawn(async move {
                    if let Err(error) = shared.activate(&peer).await {
                        tracing::warn!(%error, %peer, "activation chord could not select peer");
                    }
                });
            } else {
                tracing::warn!(
                    count = eligible.len(),
                    "activation chord requires exactly one eligible peer"
                );
            }
        }
        RuntimeEvent::OwnershipChanged {
            phase: OwnershipPhase::Arming,
            selected_peer: Some(peer),
            changed_at,
            arming_leakage_events: _,
        } => {
            *shared.arming_started.lock().await = Some((peer, changed_at));
        }
        RuntimeEvent::OwnershipChanged {
            phase: OwnershipPhase::Remote,
            selected_peer: Some(peer),
            changed_at,
            arming_leakage_events,
        } => {
            if let Some((arming_peer, started_at)) = shared.arming_started.lock().await.take()
                && arming_peer == peer
                && let Some(session) = shared.sessions.lock().await.get(&peer).cloned()
            {
                session.record_arming_to_grab(changed_at.saturating_duration_since(started_at));
                session.record_switch_time_leakage(arming_leakage_events);
            }
            if let Err(error) = shared.begin_outbound(&peer).await {
                tracing::warn!(%error, %peer, "outbound session could not start");
                shared
                    .runtime
                    .send_critical(
                        RuntimeCommand::Release {
                            transport_live: false,
                        },
                        TERMINAL_SEND_TIMEOUT,
                    )
                    .map_err(|error| anyhow!(error))?;
            }
        }
        RuntimeEvent::OwnershipChanged {
            phase: OwnershipPhase::Idle,
            ..
        } => {
            *shared.arming_started.lock().await = None;
            *shared.active_outbound.lock().await = None;
        }
        RuntimeEvent::OwnershipChanged { .. } => {}
        RuntimeEvent::TerminalRequested => {
            shared
                .finish_runtime_terminal(SessionCloseReason::LocalRelease)
                .await?;
        }
        RuntimeEvent::ActivationClosed(reason) => {
            let has_active = shared.active_outbound.lock().await.is_some();
            if has_active
                && let Err(error) = shared.end_outbound(runtime_close_reason(reason)).await
            {
                tracing::warn!(%error, "outbound terminal state could not be sent after runtime close");
            }
        }
        RuntimeEvent::ReceiverStateReleased(reason) => {
            if matches!(
                reason,
                RuntimeCloseReason::BackendFault
                    | RuntimeCloseReason::Suspend
                    | RuntimeCloseReason::Stop
            ) {
                shared.close_all(runtime_close_reason(reason)).await;
            }
        }
        RuntimeEvent::Diagnostic(diagnostic) => {
            tracing::warn!(?diagnostic, "Linux input runtime diagnostic");
        }
        RuntimeEvent::Stopped => bail!("Linux input runtime stopped"),
    }
    Ok(())
}

async fn race_connect(
    endpoint: &Endpoint,
    identity: &Identity,
    peer_spki: Vec<u8>,
    addresses: &[SocketAddr],
) -> Result<InputConnection> {
    let config = input_client_config(identity, &peer_spki)?;
    let mut addresses = addresses.to_vec();
    addresses.sort_unstable();
    addresses.dedup();
    let mut attempts = tokio::task::JoinSet::new();
    for address in addresses {
        let endpoint = endpoint.clone();
        let config = config.clone();
        attempts.spawn(async move {
            tokio::time::timeout(CONNECT_TIMEOUT, connect_input(&endpoint, address, &config))
                .await
                .map_err(|_| anyhow!("input connection to {address} timed out"))?
                .map_err(anyhow::Error::from)
        });
    }
    let mut last_error = None;
    while let Some(result) = attempts.join_next().await {
        match result {
            Ok(Ok(connection)) => {
                attempts.abort_all();
                return Ok(connection);
            }
            Ok(Err(error)) => last_error = Some(error),
            Err(error) => last_error = Some(anyhow!(error)),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("peer has no connection candidates")))
}

async fn watch_seat(shared: Arc<Shared>) {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let gate = match tokio::task::spawn_blocking(query_primary_seat).await {
            Ok(state) => state.injection_gate(),
            Err(_) => InjectionGate::Denied,
        };
        shared.update_seat_gate(gate).await;
    }
}

fn receiver_authorized(config: &Config, peer: &str, gate: InjectionGate) -> bool {
    let Some(peer) = config.peers.get(peer) else {
        return false;
    };
    if !peer.permissions.connect || !peer.permissions.send_normal {
        return false;
    }
    match gate {
        InjectionGate::Normal { .. } => true,
        InjectionGate::PreLogin => {
            config.input.allow_prelogin_input && peer.permissions.inject_prelogin
        }
        InjectionGate::Denied => false,
    }
}

fn claim_inbound(
    owner: &mut Option<(String, u64)>,
    peer: &str,
    session_id: u64,
    authorized: bool,
) -> bool {
    if !authorized
        || owner.as_ref().is_some_and(|(current_peer, current_id)| {
            current_peer != peer || *current_id != session_id
        })
    {
        return false;
    }
    *owner = Some((peer.to_owned(), session_id));
    true
}

fn is_safety_release(effect: &ReceiverEffect) -> bool {
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
        } | ReceiverEffect::Modifier {
            pressed: false,
            synthetic: true,
            ..
        } | ReceiverEffect::ScrollEnded {
            synthetic: true,
            ..
        } | ReceiverEffect::TouchReplaced {
            synthetic: true,
            ..
        } | ReceiverEffect::ActivationClosed { .. }
    )
}

fn require_outbound_permission(peer: &str, record: &PeerConfig) -> Result<()> {
    if !record.permissions.connect {
        bail!("peer {peer} is not allowed to connect");
    }
    if !record.permissions.receive_normal {
        bail!("peer {peer} is not allowed to receive input");
    }
    Ok(())
}

fn eligible_outbound_peers(config: &Config) -> Vec<String> {
    config
        .peers
        .iter()
        .filter(|(_, peer)| peer.permissions.connect && peer.permissions.receive_normal)
        .map(|(name, _)| name.clone())
        .collect()
}

fn peer_name_for_spki(config: &Config, spki: &[u8]) -> Result<String> {
    let mut matches = config
        .peers
        .iter()
        .filter(|(_, peer)| {
            peer.permissions.connect && peer.spki_der().is_ok_and(|known| known == spki)
        })
        .map(|(name, _)| name.clone());
    let peer = matches
        .next()
        .context("authenticated peer is not configured")?;
    if matches.next().is_some() {
        bail!("authenticated peer identity has more than one configured name");
    }
    Ok(peer)
}

fn validate_peer_identities(config: &Config) -> Result<()> {
    let mut identities = BTreeMap::<Vec<u8>, &str>::new();
    for (name, peer) in &config.peers {
        if let Some(first) = identities.insert(peer.spki_der()?, name) {
            bail!("peers {first} and {name} use the same identity key");
        }
    }
    Ok(())
}

fn build_server_config(identity: &Identity, config: &Config) -> Result<Option<InputServerConfig>> {
    let peers = config
        .peers
        .values()
        .filter(|peer| peer.permissions.connect)
        .map(PeerConfig::spki_der)
        .collect::<Result<Vec<_>, _>>()?;
    if peers.is_empty() {
        Ok(None)
    } else {
        Ok(Some(input_server_config_for_peers(identity, &peers)?))
    }
}

fn permissions_reduced(old: Option<PeerPermissions>, new: Option<PeerPermissions>) -> bool {
    match (old, new) {
        (Some(old), Some(new)) => {
            (old.connect && !new.connect)
                || (old.send_normal && !new.send_normal)
                || (old.receive_normal && !new.receive_normal)
                || (old.inject_prelogin && !new.inject_prelogin)
        }
        (Some(_), None) => true,
        _ => false,
    }
}

fn remove_session_if_current(
    sessions: &mut BTreeMap<String, SessionHandle>,
    peer: &str,
    session_id: u64,
) -> bool {
    if sessions
        .get(peer)
        .is_some_and(|session| session.id() == session_id)
    {
        sessions.remove(peer);
        true
    } else {
        false
    }
}

fn runtime_close_reason(reason: RuntimeCloseReason) -> SessionCloseReason {
    match reason {
        RuntimeCloseReason::LocalRelease => SessionCloseReason::LocalRelease,
        RuntimeCloseReason::Suspend => SessionCloseReason::Suspend,
        RuntimeCloseReason::TransportLost => SessionCloseReason::LeaseExpired,
        RuntimeCloseReason::DeviceRemoved
        | RuntimeCloseReason::CaptureFault
        | RuntimeCloseReason::BackendFault
        | RuntimeCloseReason::Backpressure
        | RuntimeCloseReason::Stop => SessionCloseReason::BackendUnavailable,
    }
}

fn ownership_status(phase: OwnershipPhase) -> OwnershipStatus {
    match phase {
        OwnershipPhase::Idle => OwnershipStatus::Idle,
        OwnershipPhase::Arming => OwnershipStatus::Arming,
        OwnershipPhase::Remote => OwnershipStatus::Remote,
        OwnershipPhase::Releasing => OwnershipStatus::Releasing,
    }
}

fn random_epoch() -> Result<SessionEpoch> {
    let mut epoch = [0_u8; 16];
    getrandom::fill(&mut epoch)
        .map_err(|error| anyhow!("could not generate the process session epoch: {error}"))?;
    Ok(SessionEpoch(epoch))
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn start_discovery(config: &Config, listen: SocketAddr) -> Option<Discovery> {
    if !config.transport.discovery {
        return None;
    }
    let result = (|| {
        let addresses = local_unicast_addresses()?;
        if addresses.is_empty() {
            bail!("no usable local address is available for mDNS");
        }
        let mut discovery = Discovery::new()?;
        discovery.register(Advertisement::new(
            listen.port(),
            addresses,
            [
                InputCapability::Keyboard,
                InputCapability::Pointer,
                InputCapability::Scroll,
            ],
        )?)?;
        discovery.browse()?;
        Ok::<_, anyhow::Error>(discovery)
    })();
    match result {
        Ok(discovery) => Some(discovery),
        Err(error) => {
            tracing::warn!(%error, "mDNS discovery is unavailable");
            None
        }
    }
}

async fn stop_discovery(discovery: &mut Option<Discovery>) {
    if let Some(active) = discovery.take() {
        match tokio::time::timeout(DISCOVERY_SHUTDOWN_TIMEOUT, active.shutdown()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "mDNS shutdown failed"),
            Err(_) => tracing::warn!("mDNS shutdown timed out"),
        }
    }
}

enum DiscoverySignal {
    Event(Result<DiscoveryEvent, crate::discovery::DiscoveryError>),
    Daemon(Result<mdns_sd::Error, crate::discovery::DiscoveryError>),
}

async fn next_discovery_signal(discovery: Option<&Discovery>) -> DiscoverySignal {
    let discovery = discovery.expect("select guard requires discovery");
    tokio::select! {
        event = discovery.next_event() => DiscoverySignal::Event(event),
        error = discovery.next_daemon_error() => DiscoverySignal::Daemon(error),
    }
}

fn prepare_socket_path(path: &Path) -> Result<()> {
    let parent = path.parent().context("control socket path has no parent")?;
    fs::create_dir_all(parent)?;
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() {
        bail!("refusing to replace non-socket {}", path.display());
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => bail!("another daemon is listening on {}", path.display()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            fs::remove_file(path)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl Shared {
    async fn handle_session_event(self: &Arc<Self>, event: SessionEvent) -> Result<()> {
        match event.kind {
            SessionEventKind::ReceiverEffects {
                effects,
                received_at,
                applied,
            } => {
                match self
                    .route_receiver_effects(&event.peer, event.session_id, effects, received_at)
                    .await
                {
                    Ok(true) => {
                        let _ = applied.send(Ok(()));
                    }
                    Ok(false) => {
                        let _ = applied.send(Err(
                            "receiver effects were rejected before backend application".into(),
                        ));
                    }
                    Err(error) => {
                        let _ = applied.send(Err(error.to_string()));
                        return Err(error);
                    }
                }
            }
            SessionEventKind::OutboundEnded => {
                let active = self.active_outbound.lock().await.clone();
                if active.as_ref().is_some_and(|active| {
                    active.peer == event.peer && active.session_id == event.session_id
                }) {
                    *self.active_outbound.lock().await = None;
                    if let Some(session) = self
                        .sessions
                        .lock()
                        .await
                        .get(&event.peer)
                        .filter(|session| session.id() == event.session_id)
                        .cloned()
                    {
                        session.close(SessionCloseReason::LeaseExpired);
                    }
                    self.runtime
                        .send_critical(
                            RuntimeCommand::Release {
                                transport_live: false,
                            },
                            TERMINAL_SEND_TIMEOUT,
                        )
                        .map_err(|error| anyhow!(error))?;
                }
            }
            SessionEventKind::Closed { reason } => {
                let final_metrics = {
                    let mut sessions = self.sessions.lock().await;
                    let final_metrics = sessions
                        .get(&event.peer)
                        .filter(|session| session.id() == event.session_id)
                        .map(SessionHandle::metrics_snapshot);
                    remove_session_if_current(&mut sessions, &event.peer, event.session_id);
                    final_metrics
                };
                if let Some(final_metrics) = final_metrics {
                    let mut history = self.metrics_history.lock().await;
                    if !history.contains_key(&event.peer) && history.len() == MAX_METRICS_HISTORY {
                        let eviction_key = history.keys().next().cloned();
                        if let Some(eviction_key) = eviction_key {
                            history.remove(&eviction_key);
                        }
                    }
                    history.insert(event.peer.clone(), final_metrics);
                }
                let active = self.active_outbound.lock().await.clone();
                if active.as_ref().is_some_and(|active| {
                    active.peer == event.peer && active.session_id == event.session_id
                }) {
                    *self.active_outbound.lock().await = None;
                    self.runtime
                        .send_critical(
                            RuntimeCommand::Release {
                                transport_live: false,
                            },
                            TERMINAL_SEND_TIMEOUT,
                        )
                        .map_err(|error| anyhow!(error))?;
                }
                let mut inbound = self.inbound_owner.lock().await;
                if inbound
                    .as_ref()
                    .is_some_and(|(peer, id)| peer == &event.peer && *id == event.session_id)
                {
                    *inbound = None;
                }
                tracing::warn!(peer = %event.peer, %reason, "input session closed");
            }
        }
        Ok(())
    }

    async fn route_receiver_effects(
        &self,
        peer: &str,
        session_id: u64,
        effects: Vec<ReceiverEffect>,
        received_at: std::time::Instant,
    ) -> Result<bool> {
        let _policy = self.policy.lock().await;
        let session = self
            .sessions
            .lock()
            .await
            .get(peer)
            .filter(|session| session.id() == session_id)
            .cloned();
        let Some(session) = session else {
            return Ok(false);
        };

        let opens = effects
            .iter()
            .any(|effect| matches!(effect, ReceiverEffect::ActivationOpened(_)));
        let gate = *self.seat_gate.read().await;
        let permitted = {
            let config = self.config.read().await;
            receiver_authorized(&config, peer, gate)
        };
        if opens {
            if !permitted {
                self.close_session(peer, session_id, SessionCloseReason::PermissionRevoked)
                    .await;
                return Ok(false);
            }
            let mut owner = self.inbound_owner.lock().await;
            if !claim_inbound(&mut owner, peer, session_id, permitted) {
                drop(owner);
                self.close_session(peer, session_id, SessionCloseReason::Superseded)
                    .await;
                return Ok(false);
            }
        }

        let mut deliver = Vec::new();
        let mut rejected = false;
        let mut closed = false;
        for effect in effects {
            closed |= matches!(effect, ReceiverEffect::ActivationClosed { .. });
            if !effect.is_injection() || permitted || is_safety_release(&effect) {
                deliver.push(effect);
            } else {
                rejected = true;
            }
        }
        if !deliver.is_empty() {
            let safety_release = deliver.iter().all(is_safety_release);
            let (applied_tx, applied_rx) = tokio::sync::oneshot::channel();
            let command = RuntimeCommand::ReceiverEffects {
                effects: deliver,
                applied: Some(applied_tx),
            };
            let result = if safety_release {
                self.runtime.send_critical(command, TERMINAL_SEND_TIMEOUT)
            } else {
                self.runtime.send(command)
            };
            if let Err(error) = result {
                if safety_release {
                    return Err(anyhow!(error)
                        .context("runtime backpressure prevented authoritative receiver cleanup"));
                }
                rejected = true;
            } else {
                session.record_receive_to_runtime_dispatch(received_at);
                // Once accepted by the runtime queue, a local timeout cannot
                // distinguish "not applied" from "will apply after timeout".
                // Hold the policy barrier for the definitive ACK instead. The
                // packaged service watchdog runs on this same runtime thread,
                // so a wedged backend is terminated by the service manager.
                match applied_rx.await {
                    Ok(applied_at) => {
                        session.record_receive_to_inject(received_at, applied_at);
                    }
                    Err(_) if safety_release => {
                        return Err(anyhow!(
                            "runtime did not acknowledge authoritative receiver cleanup"
                        ));
                    }
                    Err(_) => rejected = true,
                }
            }
        }
        if closed {
            let mut owner = self.inbound_owner.lock().await;
            if owner
                .as_ref()
                .is_some_and(|(owner_peer, owner_id)| owner_peer == peer && *owner_id == session_id)
            {
                *owner = None;
            }
        }
        if rejected {
            self.close_session(peer, session_id, SessionCloseReason::PermissionRevoked)
                .await;
        }
        Ok(!rejected)
    }

    async fn close_session(&self, peer: &str, session_id: u64, reason: SessionCloseReason) {
        if let Some(session) = self.sessions.lock().await.get(peer).cloned()
            && session.id() == session_id
        {
            session.close(reason);
        }
    }

    async fn apply_config_locked(&self, config: Config, persist: bool) -> Result<()> {
        config.validate()?;
        validate_peer_identities(&config)?;
        // SessionOptions performs the core duration/capability conversions
        // that Config's schema-level validation deliberately does not.
        SessionOptions::from_config(&config)?;
        let runtime_config = LinuxRuntimeConfig::from_config(&config);
        runtime_config.validate()?;
        let replacement = build_server_config(&self.identity, &config)?;
        let old = self.config.read().await.clone();
        if old.daemon.state_dir != config.daemon.state_dir {
            bail!("changing daemon.state_dir requires a daemon restart");
        }
        if old.daemon.control_socket != config.daemon.control_socket {
            bail!("changing daemon.control_socket requires a daemon restart");
        }
        if old.transport.listen != config.transport.listen {
            bail!("changing transport.listen requires a daemon restart");
        }
        if old.transport.discovery != config.transport.discovery {
            bail!("changing transport.discovery requires a daemon restart");
        }
        let runtime_changed = old.input.capture_devices != config.input.capture_devices
            || old.input.activation_chord != config.input.activation_chord
            || old.input.escape_chord != config.input.escape_chord;
        if runtime_changed {
            self.reload_runtime(runtime_config).await?;
        }
        if persist && let Err(error) = config.save(&self.config_path) {
            // Runtime reload is acknowledged before persistence so a full or
            // stopped command queue can never make disk claim a rejected
            // configuration. Roll back the acknowledged runtime mutation if
            // the atomic file replacement itself fails.
            if runtime_changed
                && let Err(rollback) = self
                    .reload_runtime(LinuxRuntimeConfig::from_config(&old))
                    .await
            {
                return Err(anyhow!(error).context(format!(
                    "runtime rollback also failed after config persistence error: {rollback}"
                )));
            }
            return Err(error.into());
        }
        let _policy = self.policy.lock().await;
        self.endpoint
            .set_server_config(replacement.as_ref().map(InputServerConfig::quinn_config));
        *self.server_config.write().await = replacement;
        *self.config.write().await = config.clone();
        self.policy_generation.fetch_add(1, Ordering::AcqRel);

        let sessions = self.sessions.lock().await.clone();
        let session_policy_changed = old.transport.checkpoint_ms != config.transport.checkpoint_ms
            || old.transport.lease_ms != config.transport.lease_ms
            || old.playout != config.playout
            || (old.input.allow_prelogin_input && !config.input.allow_prelogin_input);
        for (peer, session) in sessions {
            let old_record = old.peers.get(&peer);
            let new_record = config.peers.get(&peer);
            let identity_changed = match (old_record, new_record) {
                (Some(old), Some(new)) => old.spki_der_hex != new.spki_der_hex,
                (Some(_), None) => true,
                _ => false,
            };
            if identity_changed
                || session_policy_changed
                || permissions_reduced(
                    old_record.map(|peer| peer.permissions),
                    new_record.map(|peer| peer.permissions),
                )
                || !new_record.is_some_and(|peer| peer.permissions.connect)
            {
                session.close(SessionCloseReason::PermissionRevoked);
            }
        }
        Ok(())
    }

    async fn reload_runtime(&self, config: LinuxRuntimeConfig) -> Result<()> {
        let (applied, receipt) = tokio::sync::oneshot::channel();
        self.runtime
            .send(RuntimeCommand::Reload { config, applied })
            .map_err(|error| anyhow!(error))?;
        receipt
            .await
            .map_err(|_| anyhow!("Linux runtime stopped before acknowledging reload"))?
            .map_err(anyhow::Error::from)
    }

    async fn update_seat_gate(&self, gate: InjectionGate) {
        let _policy = self.policy.lock().await;
        let old = std::mem::replace(&mut *self.seat_gate.write().await, gate);
        if old == gate {
            return;
        }
        let owner = self.inbound_owner.lock().await.clone();
        if let Some((peer, session_id)) = owner {
            let allowed = {
                let config = self.config.read().await;
                receiver_authorized(&config, &peer, gate)
            };
            if !allowed {
                self.close_session(&peer, session_id, SessionCloseReason::PermissionRevoked)
                    .await;
            }
        }
    }

    async fn close_all(&self, reason: SessionCloseReason) {
        let sessions = std::mem::take(&mut *self.sessions.lock().await);
        for session in sessions.into_values() {
            session.close(reason);
        }
        *self.active_outbound.lock().await = None;
        *self.inbound_owner.lock().await = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denied_activation_cannot_claim_the_inbound_owner() {
        let mut owner = None;
        assert!(!claim_inbound(&mut owner, "denied", 1, false));
        assert_eq!(owner, None);
        assert!(claim_inbound(&mut owner, "authorized", 2, true));
        assert_eq!(owner, Some(("authorized".to_owned(), 2)));
        assert!(!claim_inbound(&mut owner, "denied", 1, true));
    }

    #[test]
    fn receiver_permission_is_fail_closed_and_prelogin_is_explicit() {
        let mut config = Config::default();
        config.peers.insert(
            "peer".to_owned(),
            PeerConfig {
                spki_der_hex: "01".to_owned(),
                addresses: Vec::new(),
                permissions: PeerPermissions {
                    connect: true,
                    send_normal: true,
                    receive_normal: false,
                    inject_prelogin: false,
                },
            },
        );
        assert!(receiver_authorized(
            &config,
            "peer",
            InjectionGate::Normal { uid: 1000 }
        ));
        assert!(!receiver_authorized(&config, "peer", InjectionGate::Denied));
        assert!(!receiver_authorized(
            &config,
            "peer",
            InjectionGate::PreLogin
        ));
        config.input.allow_prelogin_input = true;
        config
            .peers
            .get_mut("peer")
            .unwrap()
            .permissions
            .inject_prelogin = true;
        assert!(receiver_authorized(
            &config,
            "peer",
            InjectionGate::PreLogin
        ));
    }
}
