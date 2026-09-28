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

use anyhow::{Context, Result, anyhow, bail, ensure};
use quinn::Endpoint;
use tokio::{
    net::UnixListener,
    sync::{Mutex, RwLock, Semaphore, mpsc, watch},
};

use crate::{
    config::{Config, PeerConfig, PeerPermissions},
    control::{
        DaemonStatus, OwnershipStatus, Request, Response, authorize_peer, read_message,
        write_message,
    },
    core::{
        ActivationId, InputCapability, KeyboardMode, ReceiverEffect, SessionCloseReason,
        SessionContext, SessionEpoch, TransportGeneration,
    },
    discovery::{
        Advertisement, Discovery, DiscoveryError, DiscoveryEvent, local_unicast_addresses,
    },
    identity::{Identity, encode_hex},
    linux::{InjectionGate, OwnershipPhase, SeatState, watch_primary_seat},
    runtime::{
        LinuxRuntime, LinuxRuntimeConfig, LinuxRuntimeControl, RuntimeCloseReason, RuntimeCommand,
        RuntimeEvent,
    },
    session::{SessionEvent, SessionEventKind, SessionHandle, SessionOptions, start_session},
    transport::{
        InputConnection, InputServerConfig, accept_input, connect_input, input_client_config,
        input_server_config_for_peers,
    },
};

const SESSION_EVENT_CAPACITY: usize = 1_024;
mod clipboard;
mod crossing;
mod desktop;
mod peer_view;
const ACCEPT_EVENT_CAPACITY: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const LOCAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const TERMINAL_SEND_TIMEOUT: Duration = Duration::from_millis(250);
const DISCOVERY_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const DISCOVERY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_METRICS_HISTORY: usize = 128;
const MAX_PENDING_HANDSHAKES: usize = 64;
const MAX_LOCAL_CLIENTS: usize = 64;
/// Nearby zflow computers kept as extra dial candidates, like the Mac app's list.
const MAX_NEARBY: usize = 64;
/// The shared layout this computer keeps, in its state directory.
const LAYOUT_FILE: &str = "layout.json";
/// A paired computer's tile has this size until that computer writes its own.
const PEER_TILE_SIZE: (u32, u32) = (1920, 1080);
/// The largest snap distance a move may ask for, in layout units, as on the Mac.
const MAX_SNAP: u32 = 2048;
/// Logind answers Unknown when a reply is slow or races a property change.
/// Such a short Unknown holds injection and keeps the last definite state for
/// authorization, so it does not end a live crossing.
const SEAT_UNKNOWN_GRACE: Duration = Duration::from_secs(1);

pub fn run(config_path: PathBuf) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zflow=info".into()),
        )
        .init();
    // The unit caps threads with TasksMax=128, and input runs on its own
    // thread, so the control plane gets two workers rather than one per CPU.
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(run_async(config_path))
}

async fn run_async(config_path: PathBuf) -> Result<()> {
    // systemctl stop and the sleep hook send SIGTERM. Handle it from the start
    // so it always reaches the graceful shutdown below.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let config = Config::load(&config_path)
        .with_context(|| format!("failed to load {}", config_path.display()))?;
    validate_peer_identities(&config)?;
    SessionOptions::from_config(&config)?;
    fs::create_dir_all(&config.daemon.state_dir)?;
    fs::set_permissions(&config.daemon.state_dir, fs::Permissions::from_mode(0o700))?;
    let identity = Arc::new(Identity::load_or_create(&config.daemon.state_dir)?);
    let process_epoch = random_epoch()?;
    let mut seat = watch_primary_seat();
    let mut runtime = LinuxRuntime::spawn(LinuxRuntimeConfig::from_config(&config))?;

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
    // Give the first logind answer a moment so early requests see a real seat.
    let _ = tokio::time::timeout(SEAT_UNKNOWN_GRACE, seat.changed()).await;
    let initial_seat = SeatGrace::new(seat.borrow().clone(), Instant::now()).gate();
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
        dialed: Mutex::new(BTreeMap::new()),
        nearby: Mutex::new(BTreeMap::new()),
        layout: Mutex::new(None),
        crossing: Mutex::new(()),
        metrics_history: Mutex::new(BTreeMap::new()),
        arming_started: Mutex::new(None),
        active_outbound: Mutex::new(None),
        inbound_owner: Mutex::new(None),
        clipboard_echo: Mutex::new(BTreeMap::new()),
        desktop: desktop::Hub::default(),
        seat,
        seat_gate: RwLock::new(initial_seat),
        session_events,
    });
    desktop::start_local_sync(shared.clone());
    if let Err(error) = peer_view::start(shared.clone()) {
        tracing::warn!(%error, "desktop metadata API unavailable");
    }
    shared.restore_layout().await;
    let mut discovery = start_discovery(&config, shared.endpoint.local_addr()?);
    let daemon_uid = nix::unistd::geteuid().as_raw();
    let handshake_slots = Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES));
    let session_setup_slots = Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES));
    let local_slots = Arc::new(Semaphore::new(MAX_LOCAL_CLIENTS));
    let seat_watcher = tokio::spawn(watch_seat(shared.clone()));
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
            event = runtime.events.recv() => {
                let Some(event) = event else {
                    bail!("Linux input runtime event channel closed");
                };
                handle_runtime_event(event, &shared).await?;
            }
            frame = runtime.captured.recv() => {
                let Some(frame) = frame else {
                    bail!("Linux input runtime capture channel closed");
                };
                // The input thread sends the switch to Remote before the frames
                // it covers, so handle queued events first.
                while let Ok(event) = runtime.events.try_recv() {
                    handle_runtime_event(event, &shared).await?;
                }
                shared.forward_capture(frame).await?;
            }
            error = next_discovery_error(discovery.as_ref()), if discovery.is_some() => {
                match error {
                    Ok(error) => tracing::warn!(%error, "mDNS daemon error"),
                    Err(error) => tracing::warn!(%error, "mDNS monitoring stopped"),
                }
                stop_discovery(&mut discovery).await;
                shared.nearby.lock().await.clear();
            }
            event = next_discovery_event(discovery.as_ref()), if discovery.is_some() => {
                match event {
                    Ok(DiscoveryEvent::Candidate(candidate)) => {
                        let Some(instance) = candidate.ephemeral_instance_id() else {
                            continue;
                        };
                        let local = local_unicast_addresses().unwrap_or_default();
                        let addresses = if candidate.is_compatible() {
                            remote_addresses(candidate.socket_addresses(), &local)
                        } else {
                            Vec::new()
                        };
                        let mut nearby = shared.nearby.lock().await;
                        let instance = instance.to_string();
                        if addresses.is_empty() {
                            nearby.remove(&instance);
                        } else if nearby.len() < MAX_NEARBY || nearby.contains_key(&instance) {
                            nearby.insert(instance, addresses);
                        }
                    }
                    Ok(DiscoveryEvent::Removed(instance)) => {
                        shared.nearby.lock().await.remove(&instance.to_string());
                    }
                    Ok(DiscoveryEvent::Stopped) | Err(_) => {
                        tracing::warn!("mDNS browsing stopped");
                        stop_discovery(&mut discovery).await;
                        shared.nearby.lock().await.clear();
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
            _ = terminate.recv() => break,
        }
    }

    shared
        .close_all(SessionCloseReason::BackendUnavailable)
        .await;
    // Nothing reads session events any more, so release injected input now
    // rather than after the slower network teardown; a key held from the
    // peer would autorepeat until then.
    let runtime_result = runtime.shutdown();
    seat_watcher.abort();
    shared.endpoint.close(0_u32.into(), b"daemon shutdown");
    stop_discovery(&mut discovery).await;
    runtime_result?;
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
    /// Sessions this computer dialed, by id, with when the dial finished.
    dialed: Mutex<BTreeMap<u64, Instant>>,
    /// Addresses of compatible zflow computers on the network, by mDNS
    /// instance. A dial also tries these, with the peer's pinned key, so a
    /// peer whose address changed is still found.
    nearby: Mutex<BTreeMap<String, Vec<SocketAddr>>>,
    /// The newest layout this computer has seen from any peer.
    layout: Mutex<Option<crate::desktop::SharedLayout>>,
    /// Held while a crossing started from an edge runs.
    crossing: Mutex<()>,
    metrics_history: Mutex<BTreeMap<String, crate::metrics::SessionMetricsSnapshot>>,
    arming_started: Mutex<Option<(String, Instant)>>,
    active_outbound: Mutex<Option<ActiveOutbound>>,
    inbound_owner: Mutex<Option<(String, u64)>>,
    /// Per peer, the clip last sent to it and the one it last gave here.
    clipboard_echo: Mutex<BTreeMap<String, crate::clipboard::Echo>>,
    desktop: desktop::Hub,
    seat: watch::Receiver<SeatState>,
    /// The inbound injection gate, kept current by `watch_seat`.
    seat_gate: RwLock<SeatGate>,
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
        require_outbound_permission(&*self.config.read().await, peer, &record)?;
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
        require_outbound_permission(&*self.config.read().await, peer, &current)?;
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
        self.require_not_controlled().await?;
        self.runtime
            .send(RuntimeCommand::Activate {
                peer: peer.to_owned(),
            })
            .map_err(|error| anyhow!(error))
    }

    /// Reads the layout this computer kept last time.
    async fn restore_layout(self: &Arc<Self>) {
        let path = self.config.read().await.daemon.state_dir.join(LAYOUT_FILE);
        match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<crate::desktop::SharedLayout>(&bytes)
                .map_err(anyhow::Error::from)
                .and_then(|layout| layout.validate().map(|()| layout))
            {
                Ok(layout) => *self.layout.lock().await = Some(layout),
                Err(error) => tracing::warn!(%error, "saved layout ignored"),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(%error, "saved layout not read"),
        }
        self.apply_layout().await;
    }

    /// Places a barrier on each edge of this computer's tile that touches a
    /// paired computer. Without a layout, crossings start only from the chord.
    async fn apply_layout(&self) {
        let edges = self
            .local_layout()
            .await
            .map(|layout| desktop::outbound_edges(&layout))
            .unwrap_or_default();
        tracing::info!(edges = edges.len(), "outbound edges placed");
        let pause = self.config.read().await.switching.pause_at_edges;
        desktop::set_edges(self, edges, pause);
    }

    /// Sends a peer the layout this computer keeps, if it has one.
    async fn offer_layout(&self, session: &SessionHandle) {
        let layout = self.layout.lock().await.clone();
        if let Some(layout) = layout
            && let Err(error) = session.send_layout(layout)
        {
            tracing::debug!(%error, peer = %session.peer(), "layout not sent");
        }
    }

    /// Keeps whichever of this computer's layout and a peer's is newer. A
    /// newer one is saved and passed to the other peers; a peer with an older
    /// one gets this computer's back.
    async fn merge_layout(
        self: &Arc<Self>,
        from: &str,
        session_id: u64,
        layout: crate::desktop::SharedLayout,
    ) {
        let session = self
            .sessions
            .lock()
            .await
            .get(from)
            .filter(|session| session.id() == session_id)
            .cloned();
        let current = self.layout.lock().await.clone();
        if let Some(current) = &current
            && !layout.is_newer_than(current)
        {
            if current.is_newer_than(&layout)
                && let Some(session) = session
            {
                self.offer_layout(&session).await;
            }
            return;
        }
        let version = layout.version;
        if !self.keep_layout(layout, Some(session_id)).await {
            return;
        }
        tracing::info!(peer = %from, version, "layout adopted");
        // The other computer may not know this desktop's size yet.
        let shared = self.clone();
        tokio::spawn(async move {
            if let crate::desktop::DesktopResponse::Snapshot { geometry, .. } =
                shared.desktop.snapshot().await
            {
                shared.fit_own_tile(&geometry).await;
            }
        });
    }

    /// Uses `layout` from now on if it is newer than the one kept, saves it,
    /// and sends it to every peer except the session it came from. Returns
    /// whether it was kept: another task may have kept a newer one first.
    async fn keep_layout(&self, layout: crate::desktop::SharedLayout, origin: Option<u64>) -> bool {
        {
            let mut current = self.layout.lock().await;
            if current
                .as_ref()
                .is_some_and(|current| !layout.is_newer_than(current))
            {
                return false;
            }
            *current = Some(layout.clone());
        }
        self.save_layout(&layout).await;
        self.apply_layout().await;
        let others: Vec<_> = self
            .sessions
            .lock()
            .await
            .values()
            .filter(|session| Some(session.id()) != origin)
            .cloned()
            .collect();
        for session in others {
            self.offer_layout(&session).await;
        }
        true
    }

    /// Writes this computer's own tile size, from its GNOME desktop, into
    /// the shared layout, and tells the other computers.
    pub(super) async fn fit_own_tile(&self, geometry: &crate::desktop::Geometry) {
        let Ok(bounds) = geometry.bounds() else {
            return;
        };
        let current = self.layout.lock().await.clone();
        let Some(resized) = current.and_then(|layout| {
            layout.with_own_size(&self.identity_fingerprint, bounds.width, bounds.height)
        }) else {
            return;
        };
        let version = resized.version;
        if self.keep_layout(resized, None).await {
            tracing::info!(
                width = bounds.width,
                height = bounds.height,
                version,
                "this computer's tile resized"
            );
        }
    }

    /// Writes `layout` to the state directory, so it outlives a restart.
    async fn save_layout(&self, layout: &crate::desktop::SharedLayout) {
        let path = self.config.read().await.daemon.state_dir.join(LAYOUT_FILE);
        let saved = serde_json::to_string(layout)
            .map_err(anyhow::Error::from)
            .and_then(|text| crate::config::save_text(&path, &text).map_err(Into::into));
        if let Err(error) = saved {
            tracing::warn!(%error, "layout not saved; still using it until restart");
        }
    }

    /// The layout this computer keeps, or, before anyone arranged the
    /// computers, the one it would start from. Starting one asks GNOME for
    /// this desktop's size.
    async fn layout_or_initial(&self) -> Result<crate::desktop::SharedLayout> {
        if let Some(layout) = self.layout.lock().await.clone() {
            return Ok(layout);
        }
        let geometry = match self.desktop.snapshot().await {
            crate::desktop::DesktopResponse::Snapshot { geometry, .. } => geometry,
            crate::desktop::DesktopResponse::Unavailable { reason } => bail!("{reason}"),
            _ => bail!("GNOME did not describe this desktop"),
        };
        let bounds = geometry.bounds()?;
        let initial = initial_layout(
            &self.identity_fingerprint,
            bounds.width,
            bounds.height,
            &peer_keys(&*self.config.read().await),
        )?;
        // Keep it, so later requests do not ask GNOME again. Version 0 gives
        // way to any layout a peer arranged.
        self.keep_layout(initial, None).await;
        self.layout
            .lock()
            .await
            .clone()
            .context("the layout was not kept")
    }

    /// What the settings window arranges. None while there is no layout and
    /// GNOME cannot describe this desktop.
    pub(super) async fn layout_status(&self) -> Option<crate::app::layout_model::Layout> {
        let layout = self.layout_or_initial().await.ok()?;
        let keys = peer_keys(&*self.config.read().await);
        Some(layout_view(&layout, &self.identity_fingerprint, &keys))
    }

    /// Moves one computer, as the settings window asks, and tells the
    /// other computers. The first move also starts the layout.
    pub(super) async fn move_tile(
        self: &Arc<Self>,
        id: &str,
        x: i32,
        y: i32,
        tolerance: u32,
    ) -> Result<()> {
        let current = self.layout_or_initial().await?;
        let keys = peer_keys(&*self.config.read().await);
        let moved = with_tile_moved(
            &current,
            &self.identity_fingerprint,
            &keys,
            id,
            (x, y),
            tolerance,
        )?;
        let version = moved.version;
        ensure!(
            self.keep_layout(moved, None).await,
            "The layout changed on another computer; try again"
        );
        tracing::info!(%id, version, "tile moved");
        Ok(())
    }

    /// Starts a crossing toward the computer behind the edge the pointer
    /// pushed against. The usual activation checks apply.
    fn edge_hit(self: &Arc<Self>, edge: crate::desktop::Edge, position: u32) {
        let Some(peer) = self.desktop.edge_peer(edge, position) else {
            return;
        };
        let shared = self.clone();
        tokio::spawn(async move {
            if let Err(error) = shared.cross(edge, position).await {
                tracing::info!(error = %format_args!("{error:#}"), %peer, "edge crossing ended");
            }
        });
    }

    /// A computer that another one controls does not send its own input.
    async fn require_not_controlled(&self) -> Result<()> {
        match &*self.inbound_owner.lock().await {
            Some((peer, _)) => bail!("{peer} is controlling this computer"),
            None => Ok(()),
        }
    }

    async fn ensure_session(
        self: &Arc<Self>,
        peer: &str,
        record: &PeerConfig,
    ) -> Result<SessionHandle> {
        if let Some(existing) = self.sessions.lock().await.get(peer).cloned() {
            return Ok(existing);
        }
        let mut addresses = record.addresses.clone();
        addresses.extend(self.nearby.lock().await.values().flatten());
        if addresses.is_empty() {
            bail!("peer {peer} has no configured input address and none was found nearby");
        }
        let policy_generation = self.policy_generation.load(Ordering::Acquire);
        let connection = race_connect(
            &self.endpoint,
            &self.identity,
            record.spki_der()?,
            &addresses,
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
        let config = self.config.read().await;
        let current = config.peers.get(peer).cloned();
        if self.policy_generation.load(Ordering::Acquire) != policy_generation
            || current.as_ref() != Some(record)
            || current
                .as_ref()
                .is_none_or(|current| require_outbound_permission(&config, peer, current).is_err())
        {
            session.close(SessionCloseReason::PermissionRevoked);
            bail!("peer {peer} authorization changed during connection negotiation");
        }
        let wins = wins_dial(&config, &self.identity_fingerprint, peer);
        let mut sessions = self.sessions.lock().await;
        if let Some(existing) = sessions.get(peer).cloned() {
            // Either this computer dialed twice at once, or the peer dialed
            // while this computer did.
            let own = self.dialed.lock().await.contains_key(&existing.id());
            if own || !wins {
                session.close(SessionCloseReason::Superseded);
                return Ok(existing);
            }
            existing.close(SessionCloseReason::Superseded);
        }
        sessions.insert(peer.to_owned(), session.clone());
        self.dialed
            .lock()
            .await
            .insert(session.id(), Instant::now());
        drop(sessions);
        self.offer_layout(&session).await;
        Ok(session)
    }

    /// Whether this computer's own dial to `peer` just finished and wins
    /// over a connection the peer dialed at the same moment.
    async fn keeps_own_dial(&self, peer: &str) -> bool {
        let Some(session) = self.sessions.lock().await.get(peer).cloned() else {
            return false;
        };
        let dialed_at = self.dialed.lock().await.get(&session.id()).copied();
        keeps_own_dial(dialed_at, Instant::now())
            && wins_dial(&*self.config.read().await, &self.identity_fingerprint, peer)
    }

    async fn accept_connection(self: &Arc<Self>, connection: InputConnection) -> Result<()> {
        let peer_spki = connection.peer_spki().to_vec();
        let policy_generation = self.policy_generation.load(Ordering::Acquire);
        let peer = {
            let config = self.config.read().await;
            peer_name_for_spki(&config, connection.peer_spki())?
        };
        if self.keeps_own_dial(&peer).await {
            connection.close();
            bail!("{peer} dialed while this computer did; keeping this computer's connection");
        }
        // A peer that reconnects after a silent network loss would otherwise
        // wait for its old session's idle timeout. This connection is the same
        // authenticated peer, so the old session goes first, and its close
        // releases what it held before the new one is registered.
        self.retire_session(&peer).await;
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
        // Another reconnect can race this one; the last to finish wins.
        loop {
            {
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
                // A session that already ended has sent its Closed event, and
                // nothing would ever remove it from the map.
                if session.is_closed() {
                    bail!("peer {peer} input session ended during setup");
                }
                let own_dial = self.keeps_own_dial(&peer).await;
                let mut sessions = self.sessions.lock().await;
                match sessions.get(&peer) {
                    None => {
                        sessions.insert(peer, session.clone());
                        drop(sessions);
                        self.offer_layout(&session).await;
                        return Ok(());
                    }
                    Some(_) if own_dial => {
                        session.close(SessionCloseReason::Superseded);
                        return Ok(());
                    }
                    Some(old) => old.close(SessionCloseReason::Superseded),
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Closes the peer's current session and waits until its Closed event has
    /// run, which is where its held input is released.
    async fn retire_session(&self, peer: &str) {
        loop {
            let old = self.sessions.lock().await.get(peer).cloned();
            let Some(old) = old else {
                return;
            };
            old.close(SessionCloseReason::Superseded);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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
        require_outbound_permission(&*self.config.read().await, peer, &record)?;
        // The runtime may have been arming when a peer took control.
        self.require_not_controlled().await?;
        let session = self
            .sessions
            .lock()
            .await
            .get(peer)
            .cloned()
            .with_context(|| format!("peer {peer} disconnected before capture armed"))?;
        let context = SessionContext {
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
            .await
            .map_err(|error| anyhow!(error))
    }

    /// Sends a captured frame to its peer, or releases local input if it
    /// cannot go anywhere.
    async fn forward_capture(&self, frame: crate::linux::CapturedDeviceFrame) -> Result<()> {
        if let Err(error) = self.route_capture(frame).await {
            tracing::warn!(%error, "captured input could not reach its peer");
            self.runtime
                .send_critical(
                    RuntimeCommand::Release {
                        transport_live: false,
                    },
                    TERMINAL_SEND_TIMEOUT,
                )
                .await
                .map_err(|error| anyhow!(error))?;
        }
        Ok(())
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
        require_outbound_permission(&*self.config.read().await, &active.peer, &record)?;
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
    // The socket is mode 0660 for the service account, whose group has no
    // members, so only root and the service itself can reach it.
    authorize_peer(&stream, daemon_uid, None)?;
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
        Request::SetPeerKeyboard { peer, keyboard } => {
            let _mutation = shared.config_mutation.lock().await;
            let mut config = shared.config.read().await.clone();
            config
                .peers
                .get_mut(&peer)
                .with_context(|| format!("unknown peer {peer}"))?
                .keyboard = keyboard;
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
            desktop::set_sending(shared, true);
            match shared.begin_outbound(&peer).await {
                // The pointer left for `peer`, so the clipboard goes along.
                Ok(()) => shared.share_clipboard(&peer),
                Err(error) => {
                    tracing::warn!(%error, %peer, "outbound session could not start");
                    shared
                        .runtime
                        .send_critical(
                            RuntimeCommand::Release {
                                transport_live: false,
                            },
                            TERMINAL_SEND_TIMEOUT,
                        )
                        .await
                        .map_err(|error| anyhow!(error))?;
                }
            }
        }
        RuntimeEvent::OwnershipChanged {
            phase: OwnershipPhase::Idle,
            ..
        } => {
            *shared.arming_started.lock().await = None;
            *shared.active_outbound.lock().await = None;
            desktop::set_sending(shared, false);
        }
        RuntimeEvent::OwnershipChanged { .. } => {}
        RuntimeEvent::TerminalRequested => {
            shared
                .finish_runtime_terminal(SessionCloseReason::LocalRelease)
                .await?;
        }
        RuntimeEvent::ActivationClosed(reason) => {
            let active = shared.active_outbound.lock().await.clone();
            if let Some(active) = active {
                // The session may itself be waiting on this loop to apply input
                // from the peer, so bound the wait and close it instead; the
                // peer's receiver releases held state on connection loss.
                let result = tokio::time::timeout(
                    TERMINAL_SEND_TIMEOUT,
                    shared.end_outbound(runtime_close_reason(reason)),
                )
                .await;
                if !matches!(result, Ok(Ok(()))) {
                    tracing::warn!("outbound terminal state could not be sent after runtime close");
                    shared
                        .close_session(
                            &active.peer,
                            active.session_id,
                            SessionCloseReason::LocalRelease,
                        )
                        .await;
                }
            }
        }
        RuntimeEvent::ReceiverStateReleased(reason) => {
            if matches!(
                reason,
                RuntimeCloseReason::BackendFault | RuntimeCloseReason::Stop
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

/// Keeps the inbound gate in step with the seat watch and ends the inbound
/// session once the seat stops authorizing it.
async fn watch_seat(shared: Arc<Shared>) {
    let mut seat = shared.seat.clone();
    let mut grace = SeatGrace::new(seat.borrow_and_update().clone(), Instant::now());
    loop {
        shared.update_seat_gate(grace.gate()).await;
        tokio::select! {
            changed = seat.changed() => if changed.is_err() {
                shared.update_seat_gate(SeatGate::DENIED).await;
                return;
            },
            () = sleep_until(grace.deadline()) => {}
        }
        grace.observe(seat.borrow_and_update().clone(), Instant::now());
    }
}

/// Sleeps until `deadline`, or forever without one.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

/// The seat as the daemon judges it: the last definite state rides out a
/// short Unknown, then Unknown takes over.
struct SeatGrace {
    state: SeatState,
    /// Set while logind's latest answer is Unknown.
    unknown_since: Option<Instant>,
}

impl SeatGrace {
    fn new(current: SeatState, now: Instant) -> Self {
        let mut grace = Self {
            state: current.clone(),
            unknown_since: None,
        };
        grace.observe(current, now);
        grace
    }

    fn observe(&mut self, next: SeatState, now: Instant) {
        if !matches!(next, SeatState::Unknown { .. }) {
            self.state = next;
            self.unknown_since = None;
            return;
        }
        let since = *self.unknown_since.get_or_insert(now);
        if matches!(self.state, SeatState::Unknown { .. })
            || now.duration_since(since) >= SEAT_UNKNOWN_GRACE
        {
            self.state = next;
        }
    }

    /// When a short Unknown runs out and must be observed again.
    fn deadline(&self) -> Option<Instant> {
        if matches!(self.state, SeatState::Unknown { .. }) {
            return None;
        }
        self.unknown_since.map(|since| since + SEAT_UNKNOWN_GRACE)
    }

    fn gate(&self) -> SeatGate {
        SeatGate {
            gate: self.state.injection_gate(),
            held: self.unknown_since.is_some(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SeatGate {
    /// What the last definite seat state allows.
    gate: InjectionGate,
    /// Logind's latest answer is Unknown, so nothing is injected until it
    /// clears or the grace runs out.
    held: bool,
}

impl SeatGate {
    const DENIED: Self = Self {
        gate: InjectionGate::Denied,
        held: false,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    Inject,
    /// Deliver releases and drop the rest without ending the session.
    Hold,
    Refuse,
}

fn admit(config: &Config, peer: &str, seat: SeatGate) -> Admission {
    if !receiver_authorized(config, peer, seat.gate) {
        Admission::Refuse
    } else if seat.held {
        Admission::Hold
    } else {
        Admission::Inject
    }
}

/// Splits receiver effects into what reaches the backend and whether any
/// were refused outright.
fn admitted_effects(
    effects: Vec<ReceiverEffect>,
    admission: Admission,
) -> (Vec<ReceiverEffect>, bool) {
    let mut deliver = Vec::new();
    let mut refused = false;
    for effect in effects {
        let admitted = !effect.is_injection()
            || match admission {
                Admission::Inject => true,
                // A release can only return keys, buttons and contacts to rest.
                Admission::Hold => is_release(&effect),
                Admission::Refuse => is_safety_release(&effect),
            };
        if admitted {
            deliver.push(effect);
        } else {
            refused |= admission == Admission::Refuse;
        }
    }
    (deliver, refused)
}

fn receiver_authorized(config: &Config, peer: &str, gate: InjectionGate) -> bool {
    if !config.daemon.sharing {
        return false;
    }
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
    local: OwnershipPhase,
) -> bool {
    // Grabbed or arming devices mean this computer is sending, and it is
    // never controlled at the same time.
    if !authorized
        || local != OwnershipPhase::Idle
        || owner.as_ref().is_some_and(|(current_peer, current_id)| {
            current_peer != peer || *current_id != session_id
        })
    {
        return false;
    }
    *owner = Some((peer.to_owned(), session_id));
    true
}

/// Turns a peer's scrolling around, for a Mac with natural scrolling against
/// a desktop without it. Pointer motion stays as it is.
fn reverse_scrolling(effects: &mut [ReceiverEffect]) {
    for effect in effects {
        if let ReceiverEffect::Motion { delta, .. } = effect {
            delta.scroll_x = -delta.scroll_x;
            delta.scroll_y = -delta.scroll_y;
        }
    }
}

fn is_release(effect: &ReceiverEffect) -> bool {
    match effect {
        ReceiverEffect::Key { pressed, .. } | ReceiverEffect::Button { pressed, .. } => !pressed,
        ReceiverEffect::TouchReplaced { state, .. } => state.is_empty(),
        ReceiverEffect::ActivationClosed { .. } => true,
        _ => false,
    }
}

fn is_safety_release(effect: &ReceiverEffect) -> bool {
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

fn require_outbound_permission(config: &Config, peer: &str, record: &PeerConfig) -> Result<()> {
    anyhow::ensure!(config.daemon.sharing, "Input sharing is paused");
    if !record.permissions.connect {
        bail!("peer {peer} is not allowed to connect");
    }
    if !record.permissions.receive_normal {
        bail!("peer {peer} is not allowed to receive input");
    }
    Ok(())
}

/// A dial counts as simultaneous with the peer's while it is younger than a
/// connection attempt. A peer that dials in later lost its connection, so its
/// new one replaces whatever was there.
fn keeps_own_dial(dialed_at: Option<Instant>, now: Instant) -> bool {
    dialed_at.is_some_and(|at| now.saturating_duration_since(at) < CONNECT_TIMEOUT)
}

fn wins_dial(config: &Config, local_fingerprint: &str, peer: &str) -> bool {
    config
        .peers
        .get(peer)
        .and_then(|record| record.fingerprint_hex().ok())
        .is_some_and(|theirs| crate::identity::wins_simultaneous_dial(local_fingerprint, &theirs))
}

/// Each paired computer's key fingerprint, by its name here.
fn peer_keys(config: &Config) -> BTreeMap<String, String> {
    config
        .peers
        .iter()
        .filter_map(|(name, peer)| Some((name.clone(), peer.fingerprint_hex().ok()?)))
        .collect()
}

/// This computer's view of a shared layout: its own tile is "local", and
/// paired computers carry their names here.
fn layout_view(
    layout: &crate::desktop::SharedLayout,
    own: &str,
    keys: &BTreeMap<String, String>,
) -> crate::app::layout_model::Layout {
    crate::app::layout_model::Layout::from_shared(layout, own, "This computer", keys)
}

/// The layout to start from before anyone arranged the computers: this
/// computer's tile, then each paired computer to its right. It is version 0,
/// so the first edit makes version 1.
fn initial_layout(
    own: &str,
    width: u32,
    height: u32,
    keys: &BTreeMap<String, String>,
) -> Result<crate::desktop::SharedLayout> {
    let mut seen = std::collections::BTreeSet::from([own]);
    let peers = keys
        .values()
        .filter(|key| seen.insert(key.as_str()))
        .map(|key| (key.as_str(), PEER_TILE_SIZE));
    let mut x: i32 = 0;
    let tiles = std::iter::once((own, (width, height)))
        .chain(peers)
        .take(crate::desktop::MAX_SHARED_TILES)
        .map(|(key, (width, height))| {
            let tile = crate::desktop::Tile {
                key: key.to_owned(),
                x,
                y: 0,
                width,
                height,
            };
            x = x.saturating_add(i32::try_from(width).unwrap_or(i32::MAX));
            tile
        })
        .collect();
    let layout = crate::desktop::SharedLayout {
        version: 0,
        editor: own.to_owned(),
        tiles,
    };
    layout
        .validate()
        .context("This desktop is too large for the layout")?;
    Ok(layout)
}

/// A new version of `layout`, edited by this computer, with the tile `id` of
/// its view at (x, y), or against an edge within `tolerance` of there. Tiles
/// of computers not paired here are left out, as in any layout it writes.
fn with_tile_moved(
    layout: &crate::desktop::SharedLayout,
    own: &str,
    keys: &BTreeMap<String, String>,
    id: &str,
    (x, y): (i32, i32),
    tolerance: u32,
) -> Result<crate::desktop::SharedLayout> {
    let mut view = layout_view(layout, own, keys);
    let index = view
        .monitors
        .iter()
        .position(|monitor| monitor.id == id)
        .with_context(|| format!("The layout has no computer {id}"))?;
    let (x, y) = view
        .snap_move(index, x, y, tolerance.min(MAX_SNAP) as i32)
        .context("Computers cannot overlap")?;
    view.monitors[index].x = x;
    view.monitors[index].y = y;
    let moved = view.to_shared(layout.version.saturating_add(1), own, keys);
    moved.validate()?;
    Ok(moved)
}

fn eligible_outbound_peers(config: &Config) -> Vec<String> {
    if !config.daemon.sharing {
        return Vec::new();
    }
    config
        .peers
        .iter()
        .filter(|(_, peer)| peer.permissions.connect && peer.permissions.receive_normal)
        .map(|(name, _)| name.clone())
        .collect()
}

fn peer_name_for_spki(config: &Config, spki: &[u8]) -> Result<String> {
    anyhow::ensure!(config.daemon.sharing, "Input sharing is paused");
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
    if !config.daemon.sharing {
        return Ok(None);
    }
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

fn start_discovery(config: &Config, listen: SocketAddr) -> Option<Discovery> {
    if !config.transport.discovery {
        return None;
    }
    // Browsing finds a paired computer whose address changed.
    let result = (|| {
        let mut discovery = Discovery::new()?;
        discovery.register(Advertisement::new(
            listen.port(),
            advertised_capabilities(config),
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

fn advertised_capabilities(config: &Config) -> Vec<InputCapability> {
    let mut capabilities = vec![
        InputCapability::Keyboard,
        InputCapability::Pointer,
        InputCapability::Scroll,
    ];
    if config.input.experimental_touchpad {
        capabilities.push(InputCapability::Touch);
    }
    capabilities
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

async fn next_discovery_event(
    discovery: Option<&Discovery>,
) -> Result<DiscoveryEvent, DiscoveryError> {
    discovery
        .expect("select guard requires discovery")
        .next_event()
        .await
}

/// A record's addresses without this computer's own, which include its own
/// advertisement coming back.
fn remote_addresses(addresses: &[SocketAddr], local: &[std::net::IpAddr]) -> Vec<SocketAddr> {
    addresses
        .iter()
        .filter(|address| !address.ip().is_loopback() && !local.contains(&address.ip()))
        .copied()
        .collect()
}

async fn next_discovery_error(
    discovery: Option<&Discovery>,
) -> Result<mdns_sd::Error, DiscoveryError> {
    discovery
        .expect("select guard requires discovery")
        .next_daemon_error()
        .await
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
            SessionEventKind::Desktop { request, reply } => {
                let shared = self.clone();
                tokio::spawn(async move {
                    let response =
                        desktop::request(shared, event.peer, event.session_id, request).await;
                    let _ = reply.send(response);
                });
            }
            SessionEventKind::ReceiverEffects {
                effects,
                touch_captured_at,
                received_at,
                applied,
            } => {
                match self
                    .route_receiver_effects(
                        &event.peer,
                        event.session_id,
                        effects,
                        received_at,
                        touch_captured_at,
                    )
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
            SessionEventKind::Clipboard { clip } => self.keep_clipboard(event.peer, clip),
            SessionEventKind::Layout { layout } => {
                self.merge_layout(&event.peer, event.session_id, layout)
                    .await;
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
                        .await
                        .map_err(|error| anyhow!(error))?;
                }
            }
            SessionEventKind::Closed { reason } => {
                let shared = self.clone();
                let peer = event.peer.clone();
                tokio::spawn(async move {
                    shared.desktop.closed(&peer, event.session_id).await;
                });
                let final_metrics = {
                    let mut sessions = self.sessions.lock().await;
                    let final_metrics = sessions
                        .get(&event.peer)
                        .filter(|session| session.id() == event.session_id)
                        .map(SessionHandle::metrics_snapshot);
                    remove_session_if_current(&mut sessions, &event.peer, event.session_id);
                    final_metrics
                };
                self.dialed.lock().await.remove(&event.session_id);
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
                        .await
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
        self: &Arc<Self>,
        peer: &str,
        session_id: u64,
        effects: Vec<ReceiverEffect>,
        received_at: std::time::Instant,
        touch_captured_at: Option<Instant>,
    ) -> Result<bool> {
        let opens = effects
            .iter()
            .any(|effect| matches!(effect, ReceiverEffect::ActivationOpened(_)));
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

        let gate = *self.seat_gate.read().await;
        let (admission, keyboard, reverse_scroll) = {
            let config = self.config.read().await;
            let record = config.peers.get(peer);
            let keyboard = record.map_or(KeyboardMode::Standard, |record| record.keyboard);
            let reverse_scroll = record.is_some_and(|record| record.reverse_scroll);
            (admit(&config, peer, gate), keyboard, reverse_scroll)
        };
        let permitted = admission != Admission::Refuse;
        if opens {
            if !permitted || !self.desktop.allows_session(peer, session_id).await {
                self.close_session(peer, session_id, SessionCloseReason::PermissionRevoked)
                    .await;
                return Ok(false);
            }
            let mut owner = self.inbound_owner.lock().await;
            let local = self.runtime.status().ownership;
            if !claim_inbound(&mut owner, peer, session_id, permitted, local) {
                drop(owner);
                self.close_session(peer, session_id, SessionCloseReason::Superseded)
                    .await;
                return Ok(false);
            }
        }

        let closed = effects
            .iter()
            .any(|effect| matches!(effect, ReceiverEffect::ActivationClosed { .. }));
        let (mut deliver, mut rejected) = admitted_effects(effects, admission);
        if reverse_scroll {
            reverse_scrolling(&mut deliver);
        }
        if !deliver.is_empty() {
            let safety_release = deliver.iter().all(is_safety_release);
            let (applied_tx, applied_rx) = tokio::sync::oneshot::channel();
            let command = RuntimeCommand::ReceiverEffects {
                effects: deliver,
                // Read once per activation, so a change applies at the next one.
                keyboard: opens.then_some(keyboard),
                touch_captured_at,
                applied: Some(applied_tx),
            };
            let result = if safety_release {
                self.runtime
                    .send_critical(command, TERMINAL_SEND_TIMEOUT)
                    .await
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
                // The pointer went back to `peer`, so the clipboard goes along.
                self.share_clipboard(peer);
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
            || old.input.escape_chord != config.input.escape_chord
            || old.input.experimental_touchpad != config.input.experimental_touchpad;
        if runtime_changed {
            self.reload_runtime(runtime_config).await?;
        }
        if persist && let Err(error) = self.save_config(&old, &config) {
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
            return Err(error);
        }
        let _policy = self.policy.lock().await;
        self.endpoint
            .set_server_config(replacement.as_ref().map(InputServerConfig::quinn_config));
        *self.server_config.write().await = replacement;
        *self.config.write().await = config.clone();
        self.policy_generation.fetch_add(1, Ordering::AcqRel);

        let sessions = self.sessions.lock().await.clone();
        let session_policy_changed = old.daemon.sharing != config.daemon.sharing
            || old.transport.checkpoint_ms != config.transport.checkpoint_ms
            || old.transport.lease_ms != config.transport.lease_ms
            || old.playout != config.playout
            || old.input.experimental_touchpad != config.input.experimental_touchpad
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
        // Pairing, forgetting or renaming a computer changes which tiles have
        // a computer behind them here.
        self.apply_layout().await;
        Ok(())
    }

    fn save_config(&self, old: &Config, config: &Config) -> Result<()> {
        let mut document = crate::app::model::ConfigDocument::open(self.config_path.clone())?;
        anyhow::ensure!(
            document.saved() == old,
            "Configuration changed on disk; restart the service before saving"
        );
        document.draft = config.clone();
        document.save()
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

    async fn update_seat_gate(&self, gate: SeatGate) {
        let _policy = self.policy.lock().await;
        let old = std::mem::replace(&mut *self.seat_gate.write().await, gate);
        if old == gate {
            return;
        }
        let owner = self.inbound_owner.lock().await.clone();
        if let Some((peer, session_id)) = owner {
            let refused = {
                let config = self.config.read().await;
                admit(&config, &peer, gate) == Admission::Refuse
            };
            if refused {
                self.close_session(&peer, session_id, SessionCloseReason::PermissionRevoked)
                    .await;
            }
        }
    }

    /// The active desktop user's uid for desktop API callers; none while the
    /// seat is Unknown.
    fn active_uid(&self) -> Option<u32> {
        self.seat.borrow().active_authenticated_uid()
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
    fn pausing_blocks_both_directions_and_preserves_peer_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let identity = Identity::load_or_create(dir.path()).unwrap();
        let mut config = Config::default();
        let record = PeerConfig::from_spki(
            identity.spki(),
            vec![],
            PeerPermissions {
                connect: true,
                send_normal: true,
                receive_normal: true,
                inject_prelogin: true,
            },
        )
        .unwrap();
        config.peers.insert("mac".into(), record.clone());
        config.input.allow_prelogin_input = true;
        assert!(receiver_authorized(
            &config,
            "mac",
            InjectionGate::Normal { uid: 1000 }
        ));
        assert!(require_outbound_permission(&config, "mac", &record).is_ok());
        assert!(build_server_config(&identity, &config).unwrap().is_some());
        config.daemon.sharing = false;
        assert!(!receiver_authorized(
            &config,
            "mac",
            InjectionGate::Normal { uid: 1000 }
        ));
        assert!(!receiver_authorized(
            &config,
            "mac",
            InjectionGate::PreLogin
        ));
        assert!(require_outbound_permission(&config, "mac", &record).is_err());
        assert!(eligible_outbound_peers(&config).is_empty());
        assert!(peer_name_for_spki(&config, identity.spki()).is_err());
        assert!(build_server_config(&identity, &config).unwrap().is_none());
        assert_eq!(config.peers["mac"], record);
        config.daemon.sharing = true;
        assert_eq!(eligible_outbound_peers(&config), vec!["mac"]);
        assert_eq!(peer_name_for_spki(&config, identity.spki()).unwrap(), "mac");
    }

    #[test]
    fn discovery_advertises_touch_only_when_experiment_is_enabled() {
        let mut config = Config::default();
        let baseline = advertised_capabilities(&config);
        assert!(!baseline.contains(&InputCapability::Touch));

        config.input.experimental_touchpad = true;
        let experimental = advertised_capabilities(&config);
        assert!(experimental.contains(&InputCapability::Touch));
    }

    #[test]
    fn denied_activation_cannot_claim_the_inbound_owner() {
        let idle = OwnershipPhase::Idle;
        let mut owner = None;
        assert!(!claim_inbound(&mut owner, "denied", 1, false, idle));
        assert_eq!(owner, None);
        assert!(claim_inbound(&mut owner, "authorized", 2, true, idle));
        assert_eq!(owner, Some(("authorized".to_owned(), 2)));
        assert!(!claim_inbound(&mut owner, "denied", 1, true, idle));
    }

    #[test]
    fn nearby_records_drop_this_computers_own_addresses() {
        let own = "192.0.2.5".parse().unwrap();
        let addresses: Vec<SocketAddr> = ["192.0.2.5:43119", "127.0.0.1:43119", "192.0.2.9:43119"]
            .iter()
            .map(|address| address.parse().unwrap())
            .collect();
        assert_eq!(
            remote_addresses(&addresses, &[own]),
            ["192.0.2.9:43119".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn only_a_fresh_dial_by_the_lower_fingerprint_is_kept() {
        let now = Instant::now();
        assert!(keeps_own_dial(Some(now - Duration::from_secs(1)), now));
        assert!(!keeps_own_dial(Some(now - CONNECT_TIMEOUT), now));
        assert!(!keeps_own_dial(None, now));

        let dir = tempfile::tempdir().unwrap();
        let peer = Identity::load_or_create(dir.path()).unwrap();
        let mut config = Config::default();
        config.peers.insert(
            "mac".into(),
            PeerConfig::from_spki(peer.spki(), vec![], PeerPermissions::default()).unwrap(),
        );
        let theirs = peer.fingerprint_hex();
        let lower = "0".repeat(theirs.len());
        let higher = "f".repeat(theirs.len());
        assert!(wins_dial(&config, &lower, "mac"));
        assert!(!wins_dial(&config, &higher, "mac"));
        assert!(!wins_dial(&config, &lower, "unknown"));
    }

    #[test]
    fn reversed_scrolling_leaves_pointer_motion_alone() {
        use crate::core::{MotionDelta, MotionSequence};
        let motion = |dx, scroll_y| ReceiverEffect::Motion {
            delta: MotionDelta {
                dx,
                dy: 0,
                scroll_x: 3,
                scroll_y,
            },
            through_sequence: MotionSequence(1),
        };
        let mut effects = vec![motion(5, -120), motion(-2, 0)];
        reverse_scrolling(&mut effects);
        assert_eq!(
            effects,
            vec![
                ReceiverEffect::Motion {
                    delta: MotionDelta {
                        dx: 5,
                        dy: 0,
                        scroll_x: -3,
                        scroll_y: 120
                    },
                    through_sequence: MotionSequence(1),
                },
                ReceiverEffect::Motion {
                    delta: MotionDelta {
                        dx: -2,
                        dy: 0,
                        scroll_x: -3,
                        scroll_y: 0
                    },
                    through_sequence: MotionSequence(1),
                },
            ]
        );
    }

    #[test]
    fn a_computer_that_is_sending_refuses_to_be_controlled() {
        for local in [
            OwnershipPhase::Arming,
            OwnershipPhase::Remote,
            OwnershipPhase::Releasing,
        ] {
            let mut owner = None;
            assert!(!claim_inbound(&mut owner, "mac", 1, true, local));
            assert_eq!(owner, None);
        }
        let mut owner = None;
        assert!(claim_inbound(
            &mut owner,
            "mac",
            1,
            true,
            OwnershipPhase::Idle
        ));
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
                keyboard: KeyboardMode::Standard,
                reverse_scroll: false,
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

    #[test]
    fn only_an_empty_synthetic_touch_replacement_is_a_safety_release() {
        use crate::core::{ContactId, TouchContact, TouchState, TouchTool};
        let contact = TouchContact {
            id: ContactId(1),
            x: 10,
            y: 20,
            pressure: None,
            major: None,
            minor: None,
            orientation_millidegrees: None,
            tool: TouchTool::Finger,
            source_dimensions: None,
        };
        let touching = TouchState::new([contact]).unwrap();
        assert!(is_safety_release(&ReceiverEffect::TouchReplaced {
            state: TouchState::default(),
            synthetic: true,
        }));
        assert!(!is_safety_release(&ReceiverEffect::TouchReplaced {
            state: touching.clone(),
            synthetic: true,
        }));
        assert!(!is_safety_release(&ReceiverEffect::TouchReplaced {
            state: TouchState::default(),
            synthetic: false,
        }));
        assert!(!is_safety_release(&ReceiverEffect::TouchReplaced {
            state: touching,
            synthetic: false,
        }));
    }

    fn unlocked() -> SeatState {
        SeatState::Unlocked(crate::linux::AuthenticatedSession {
            id: "2".into(),
            uid: 1000,
            kind: crate::linux::AuthenticatedSessionKind::Wayland,
        })
    }

    fn unknown() -> SeatState {
        SeatState::Unknown {
            reason: "slow".into(),
        }
    }

    fn sending_peer() -> Config {
        let mut config = Config::default();
        config.peers.insert(
            "mac".to_owned(),
            PeerConfig {
                spki_der_hex: "01".to_owned(),
                addresses: Vec::new(),
                permissions: PeerPermissions {
                    connect: true,
                    send_normal: true,
                    receive_normal: false,
                    inject_prelogin: false,
                },
                keyboard: KeyboardMode::Standard,
                reverse_scroll: false,
            },
        );
        config
    }

    #[test]
    fn seat_grace_rides_out_a_short_unknown_but_not_a_definite_change() {
        use crate::linux::RestrictedSeatState;
        let start = Instant::now();
        let mut seat = SeatGrace::new(unlocked(), start);
        assert_eq!(seat.deadline(), None);

        seat.observe(unknown(), start);
        seat.observe(unknown(), start + Duration::from_millis(900));
        assert_eq!(seat.state, unlocked());
        assert_eq!(seat.deadline(), Some(start + SEAT_UNKNOWN_GRACE));
        seat.observe(unknown(), start + SEAT_UNKNOWN_GRACE);
        assert_eq!(seat.state.active_authenticated_uid(), None);
        assert_eq!(seat.deadline(), None);

        seat.observe(unlocked(), start + Duration::from_secs(2));
        let greeter = SeatState::Restricted(RestrictedSeatState::Greeter {
            session_id: "c1".into(),
        });
        seat.observe(greeter.clone(), start + Duration::from_millis(2_100));
        assert_eq!(seat.state, greeter);
        assert_eq!(seat.deadline(), None);

        // Nothing definite to fall back on: Unknown applies at once.
        let fresh = SeatGrace::new(unknown(), start);
        assert_eq!(
            fresh.gate(),
            SeatGate {
                gate: InjectionGate::Denied,
                held: true
            }
        );
    }

    #[test]
    fn inbound_injection_holds_through_a_short_unknown_and_closes_after_it() {
        let config = sending_peer();
        let start = Instant::now();
        let mut seat = SeatGrace::new(unlocked(), start);
        assert_eq!(admit(&config, "mac", seat.gate()), Admission::Inject);

        seat.observe(unknown(), start);
        assert_eq!(admit(&config, "mac", seat.gate()), Admission::Hold);
        seat.observe(unknown(), start + SEAT_UNKNOWN_GRACE);
        assert_eq!(admit(&config, "mac", seat.gate()), Admission::Refuse);

        // A definite answer ends the hold either way.
        seat.observe(unlocked(), start + Duration::from_secs(2));
        assert_eq!(admit(&config, "mac", seat.gate()), Admission::Inject);
        seat.observe(unknown(), start + Duration::from_secs(3));
        let locked = SeatState::Restricted(crate::linux::RestrictedSeatState::LockScreen {
            session_id: "c2".into(),
        });
        seat.observe(locked, start + Duration::from_millis(3_100));
        assert_eq!(admit(&config, "mac", seat.gate()), Admission::Refuse);

        // Base permissions still refuse during a hold.
        let mut paused = config.clone();
        paused.daemon.sharing = false;
        let held = SeatGate {
            gate: InjectionGate::Normal { uid: 1000 },
            held: true,
        };
        assert_eq!(admit(&paused, "mac", held), Admission::Refuse);
    }

    #[test]
    fn a_hold_drops_new_input_but_lets_releases_through() {
        use crate::core::{HidUsage, MotionDelta, MotionSequence, TouchState};
        let key = |pressed, synthetic| ReceiverEffect::Key {
            key: HidUsage::keyboard(4),
            pressed,
            synthetic,
        };
        let effects = || {
            vec![
                key(true, false),
                ReceiverEffect::Motion {
                    delta: MotionDelta::default(),
                    through_sequence: MotionSequence(1),
                },
                key(false, false),
                key(false, true),
                ReceiverEffect::TouchReplaced {
                    state: TouchState::default(),
                    synthetic: false,
                },
            ]
        };

        let (held, refused) = admitted_effects(effects(), Admission::Hold);
        assert!(!refused);
        assert_eq!(held.len(), 3);
        assert!(held.iter().all(is_release));

        let (denied, refused) = admitted_effects(effects(), Admission::Refuse);
        assert!(refused);
        assert_eq!(denied.len(), 1);
        assert!(is_safety_release(&denied[0]));

        let (injected, refused) = admitted_effects(effects(), Admission::Inject);
        assert!(!refused);
        assert_eq!(injected.len(), 5);
    }

    fn fingerprint(n: u8) -> String {
        format!("{n:064x}")
    }

    #[test]
    fn the_first_layout_puts_each_paired_computer_right_of_this_one() {
        let own = fingerprint(1);
        let keys = BTreeMap::from([
            ("mac".to_owned(), fingerprint(2)),
            ("desk".to_owned(), fingerprint(3)),
            // One computer paired twice gets one tile.
            ("mac again".to_owned(), fingerprint(2)),
        ]);
        let layout = initial_layout(&own, 2560, 1440, &keys).unwrap();
        assert_eq!((layout.version, &layout.editor), (0, &own));
        let tiles: Vec<_> = layout
            .tiles
            .iter()
            .map(|t| (t.key.clone(), t.x, t.y, t.width, t.height))
            .collect();
        assert_eq!(
            tiles,
            [
                (own.clone(), 0, 0, 2560, 1440),
                (fingerprint(3), 2560, 0, 1920, 1080),
                (fingerprint(2), 4480, 0, 1920, 1080),
            ]
        );
        assert_eq!(
            initial_layout(&own, 2560, 1440, &BTreeMap::new())
                .unwrap()
                .tiles
                .len(),
            1,
            "nothing paired yet"
        );
        let many: BTreeMap<_, _> = (2..40).map(|n| (n.to_string(), fingerprint(n))).collect();
        assert_eq!(
            initial_layout(&own, 2560, 1440, &many).unwrap().tiles.len(),
            crate::desktop::MAX_SHARED_TILES
        );
        assert!(
            initial_layout(
                &own,
                crate::app::layout_model::MAX_DIMENSION + 1,
                1440,
                &keys
            )
            .is_err()
        );
    }

    #[test]
    fn a_move_snaps_and_makes_a_version_this_computer_edited() {
        let own = fingerprint(1);
        let keys = BTreeMap::from([("mac".to_owned(), fingerprint(2))]);
        let mut layout = initial_layout(&own, 2560, 1440, &keys).unwrap();
        // A peer made the last version, and it still holds a computer that
        // is not paired here.
        layout.version = 7;
        layout.editor = fingerprint(9);
        layout.tiles.push(crate::desktop::Tile {
            key: fingerprint(5),
            x: 0,
            y: 5000,
            width: 100,
            height: 100,
        });
        let at = |layout: &crate::desktop::SharedLayout, key: &str| {
            let tile = layout.tiles.iter().find(|t| t.key == key).unwrap();
            (tile.x, tile.y)
        };

        // Dropped 20 units into this computer, it snaps against its left edge.
        let moved = with_tile_moved(&layout, &own, &keys, "peer:mac", (-1900, 40), 150).unwrap();
        assert_eq!((moved.version, &moved.editor), (8, &own));
        assert_eq!(at(&moved, &fingerprint(2)), (-1920, 40));
        assert_eq!(at(&moved, &own), (0, 0));
        assert_eq!(moved.tiles.len(), 2, "the unpaired computer is left out");
        // This computer's own tile moves by its view's id.
        let moved = with_tile_moved(&moved, &own, &keys, "local", (0, 1100), 0).unwrap();
        assert_eq!((moved.version, at(&moved, &own)), (9, (0, 1100)));

        // A huge tolerance is capped rather than read as a negative one.
        assert!(with_tile_moved(&layout, &own, &keys, "peer:mac", (-1900, 40), u32::MAX).is_ok());
        let error = with_tile_moved(&layout, &own, &keys, "peer:desk", (0, 0), 0).unwrap_err();
        assert!(
            error.to_string().contains("no computer peer:desk"),
            "{error}"
        );
        let error = with_tile_moved(&layout, &own, &keys, "peer:mac", (100, 100), 0).unwrap_err();
        assert!(error.to_string().contains("overlap"), "{error}");
    }
}
