//! One authenticated input session per paired receiver while sharing is on.
//! The desktop snapshot and every crossing reuse it, so a crossing costs a
//! Prepare round trip instead of a QUIC handshake and session negotiation.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use quinn::Endpoint;
use tokio::{
    runtime::Runtime,
    sync::{mpsc, watch},
    task::JoinHandle,
};

use crate::{
    config::{Config, PeerConfig},
    core::{
        ActivationId, InputCapability, SessionCloseReason, SessionContext, SessionEpoch,
        TransportGeneration,
    },
    desktop::{DesktopRequest, DesktopResponse, Geometry},
    identity::Identity,
    session::{SessionEvent, SessionEventKind, SessionHandle, SessionOptions, start_session},
    transport::{InputClientConfig, InputConnection, connect_input, input_client_config},
};

use super::{Activation, HandoffOptions, SourceStatus, refuse_inbound, run_crossing};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
// A working pinned address wins before other hosts on the network are tried.
const NEARBY_DELAY: Duration = Duration::from_millis(300);
const SNAPSHOT_RETRY: Duration = Duration::from_secs(5);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(15);
const STABLE_SESSION: Duration = Duration::from_secs(10);
// Long enough for the close to leave; the receiver refuses a second session
// from this Mac while the old one is open.
const CLOSE_GRACE: Duration = Duration::from_millis(100);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq)]
pub enum LinkState {
    Connecting,
    /// Connected, with the receiver's current desktop. Only this state crosses.
    Ready(Geometry),
    /// Not usable right now. The link keeps retrying.
    Down(String),
}

/// A crossing handed to a link. Setting or dropping `stop` returns input to
/// the Mac. `status` closes once cleanup has finished.
pub struct Crossing {
    pub stop: watch::Sender<bool>,
    pub status: mpsc::UnboundedReceiver<SourceStatus>,
}

enum Command {
    Cross {
        handoff: HandoffOptions,
        reduce_wifi_latency: bool,
        stop: watch::Receiver<bool>,
        status: mpsc::UnboundedSender<SourceStatus>,
    },
    Retry,
}

struct Link {
    peer: PeerConfig,
    config: Config,
    commands: mpsc::UnboundedSender<Command>,
    state: watch::Receiver<LinkState>,
    task: JoinHandle<()>,
}

pub struct Links {
    runtime: Option<Runtime>,
    links: BTreeMap<String, Link>,
    /// Replaced links still returning input or closing their session.
    closing: BTreeMap<String, JoinHandle<()>>,
    nearby: watch::Sender<Vec<SocketAddr>>,
    changed: bool,
}

impl Links {
    pub fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("zflow-sharing")
            .enable_all()
            .build()
            .context("Could not start the sharing runtime")?;
        Ok(Self {
            runtime: Some(runtime),
            links: BTreeMap::new(),
            closing: BTreeMap::new(),
            nearby: watch::channel(Vec::new()).0,
            changed: false,
        })
    }

    /// Keeps one link per peer that may receive input from this Mac, and
    /// closes the others. `None` closes every link. A peer whose record or
    /// session settings changed reconnects.
    pub fn sync(&mut self, config: Option<&Config>) {
        let settings = config.map(session_settings);
        let wanted: BTreeMap<&String, &PeerConfig> = config
            .into_iter()
            .flat_map(|config| &config.peers)
            .filter(|(_, peer)| peer.permissions.connect && peer.permissions.receive_normal)
            .collect();
        let stale: Vec<String> = self
            .links
            .iter()
            .filter(|(name, link)| {
                wanted.get(name) != Some(&&link.peer) || settings.as_ref() != Some(&link.config)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in stale {
            let link = self.links.remove(&name).expect("stale link exists");
            tracing::info!(peer = %name, "closing input link");
            // Dropping the command sender stops the task after any crossing.
            self.closing.insert(name, link.task);
            self.changed = true;
        }
        self.closing.retain(|_, task| !task.is_finished());
        let (Some(runtime), Some(settings)) = (&self.runtime, settings) else {
            return;
        };
        for (name, peer) in wanted {
            if self.links.contains_key(name) {
                continue;
            }
            let (commands, receiver) = mpsc::unbounded_channel();
            let (state_sender, state) = watch::channel(LinkState::Connecting);
            let task = runtime.spawn(run(
                name.clone(),
                peer.clone(),
                settings.clone(),
                receiver,
                state_sender,
                self.nearby.subscribe(),
                self.closing.remove(name),
            ));
            self.links.insert(
                name.clone(),
                Link {
                    peer: peer.clone(),
                    config: settings.clone(),
                    commands,
                    state,
                    task,
                },
            );
            self.changed = true;
        }
    }

    /// True when a link came, went, or changed state since the last call.
    pub fn changed(&mut self) -> bool {
        let mut changed = std::mem::take(&mut self.changed);
        for link in self.links.values_mut() {
            if link.state.has_changed().unwrap_or(false) {
                link.state.mark_unchanged();
                changed = true;
            }
        }
        changed
    }

    pub fn states(&self) -> impl Iterator<Item = (&str, LinkState)> + '_ {
        self.links
            .iter()
            .map(|(name, link)| (name.as_str(), link.state.borrow().clone()))
    }

    /// Hands a crossing to `peer`'s link, if that link is ready.
    pub fn cross(
        &self,
        peer: &str,
        handoff: HandoffOptions,
        reduce_wifi_latency: bool,
    ) -> Option<Crossing> {
        let link = self.links.get(peer)?;
        if !matches!(*link.state.borrow(), LinkState::Ready(_)) {
            return None;
        }
        let (stop, stopped) = watch::channel(false);
        let (status, events) = mpsc::unbounded_channel();
        link.commands
            .send(Command::Cross {
                handoff,
                reduce_wifi_latency,
                stop: stopped,
                status,
            })
            .ok()?;
        Some(Crossing {
            stop,
            status: events,
        })
    }

    /// Reconnects waiting links now and rechecks connected receivers.
    pub fn retry(&self) {
        for link in self.links.values() {
            let _ = link.commands.send(Command::Retry);
        }
    }

    /// Discovered receivers, tried after each peer's own addresses.
    pub fn set_nearby(&self, nearby: Vec<SocketAddr>) {
        self.nearby.send_if_modified(|current| {
            let changed = *current != nearby;
            *current = nearby;
            changed
        });
    }
}

impl Drop for Links {
    fn drop(&mut self) {
        let tasks: Vec<_> = std::mem::take(&mut self.links)
            .into_values()
            .map(|link| link.task)
            .chain(std::mem::take(&mut self.closing).into_values())
            .collect();
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        // Links return input and close their sessions once their commands close.
        runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + SHUTDOWN_TIMEOUT;
            for task in tasks {
                let _ = tokio::time::timeout_at(deadline, task).await;
            }
        });
        runtime.shutdown_timeout(Duration::from_millis(100));
    }
}

/// The settings a session was negotiated with. Other edits leave links alone.
fn session_settings(config: &Config) -> Config {
    Config {
        peers: BTreeMap::new(),
        macos: Default::default(),
        ..config.clone()
    }
}

async fn run(
    name: String,
    peer: PeerConfig,
    config: Config,
    mut commands: mpsc::UnboundedReceiver<Command>,
    state: watch::Sender<LinkState>,
    mut nearby: watch::Receiver<Vec<SocketAddr>>,
    previous: Option<JoinHandle<()>>,
) {
    // The receiver refuses a second session from this Mac while the old one is open.
    if let Some(previous) = previous {
        let _ = previous.await;
    }
    let mut failures = 0_u32;
    loop {
        let addresses = nearby.borrow_and_update().clone();
        let opening = Session::open(&name, &peer, &config, &addresses);
        let Some(opened) = refusing_crossings(&mut commands, opening).await else {
            return;
        };
        match opened {
            Ok(mut session) => {
                let opened_at = Instant::now();
                let lost = session.serve(&mut commands, &state).await;
                if let Some(reason) = &lost {
                    state.send_replace(LinkState::Down(reason.clone()));
                }
                session.close().await;
                let Some(reason) = lost else {
                    return;
                };
                // A session that keeps dropping right after it opens backs off
                // like a failed attempt.
                failures = if opened_at.elapsed() >= STABLE_SESSION {
                    0
                } else {
                    failures + 1
                };
                tracing::warn!(peer = %name, %reason, failures, "input link lost; reconnecting");
            }
            Err(error) => {
                failures += 1;
                let error = format!("{error:#}");
                tracing::warn!(peer = %name, %error, failures, "input link could not connect");
                state.send_replace(LinkState::Down(error));
            }
        }
        if !wait_to_retry(&mut commands, &mut nearby, retry_delay(failures)).await {
            return;
        }
    }
}

/// Reconnects at once after a stable session drops, then backs off while
/// attempts fail.
fn retry_delay(failures: u32) -> Duration {
    match failures {
        0 => Duration::ZERO,
        failures => Duration::from_secs(1 << (failures - 1).min(4)).min(MAX_RETRY_DELAY),
    }
}

fn refuse(command: Command, reason: &str) {
    if let Command::Cross { status, .. } = command {
        let _ = status.send(SourceStatus::Cancelled(reason.into()));
    }
}

/// Runs `work` while refusing crossings. None means the link was closed.
async fn refusing_crossings<T>(
    commands: &mut mpsc::UnboundedReceiver<Command>,
    work: impl Future<Output = T>,
) -> Option<T> {
    tokio::pin!(work);
    loop {
        tokio::select! {
            output = &mut work => return Some(output),
            command = commands.recv() => match command {
                Some(command) => refuse(command, "the receiver is not connected"),
                None => return None,
            },
        }
    }
}

/// Waits before the next attempt. Retry or new nearby addresses end the wait
/// early. False means the link was closed.
async fn wait_to_retry(
    commands: &mut mpsc::UnboundedReceiver<Command>,
    nearby: &mut watch::Receiver<Vec<SocketAddr>>,
    delay: Duration,
) -> bool {
    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            () = &mut sleep => return true,
            Ok(()) = nearby.changed() => return true,
            command = commands.recv() => match command {
                Some(Command::Retry) => return true,
                Some(command) => refuse(command, "the receiver is not connected"),
                None => return false,
            },
        }
    }
}

struct Session {
    endpoint: Endpoint,
    handle: SessionHandle,
    events: mpsc::Receiver<SessionEvent>,
    epoch: SessionEpoch,
    activations: u64,
    raw_touch: bool,
}

impl Session {
    async fn open(
        name: &str,
        peer: &PeerConfig,
        config: &Config,
        nearby: &[SocketAddr],
    ) -> Result<Self> {
        let identity = Identity::load_or_create(&config.daemon.state_dir)?;
        let client = input_client_config(&identity, &peer.spki_der()?)?;
        let options = SessionOptions::from_config(config)?;
        let mut epoch = [0_u8; 16];
        getrandom::fill(&mut epoch)
            .map_err(|error| anyhow!("could not create the source session epoch: {error}"))?;
        let first = peer
            .addresses
            .first()
            .context("Computer has no input address")?;
        // The endpoint is bound to the family of the first pinned address.
        let bind = if first.is_ipv4() {
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
        } else {
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
        };
        let endpoint = Endpoint::client(bind)?;
        let started = Instant::now();
        let opened = async {
            let (address, connection) =
                connect_peer(&endpoint, &client, &peer.addresses, nearby).await?;
            let (sender, events) = mpsc::channel(128);
            let handle = start_session(
                connection,
                name.to_owned(),
                TransportGeneration(1),
                options,
                sender,
            )
            .await?;
            tracing::info!(peer = name, %address, session_id = handle.id(),
                elapsed_ms = started.elapsed().as_millis() as u64, "input link connected");
            Ok::<_, anyhow::Error>((handle, events))
        }
        .await;
        let (handle, events) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                endpoint.close(0_u32.into(), b"input link failed");
                return Err(error);
            }
        };
        // A receiver without Touch drops contact snapshots, so keep pointer
        // and scroll instead of suppressing them while a finger is down.
        let raw_touch = config.input.experimental_touchpad
            && handle.capabilities().contains(InputCapability::Touch);
        Ok(Self {
            endpoint,
            handle,
            events,
            epoch: SessionEpoch(epoch),
            activations: 0,
            raw_touch,
        })
    }

    /// Each crossing is a new activation in this session's epoch.
    fn next_context(&mut self) -> SessionContext {
        self.activations += 1;
        SessionContext {
            session_epoch: self.epoch,
            transport_generation: self.handle.generation(),
            activation_id: ActivationId(self.activations),
        }
    }

    /// Serves crossings until the session ends, returning why, or until the
    /// link is closed, returning None.
    async fn serve(
        &mut self,
        commands: &mut mpsc::UnboundedReceiver<Command>,
        state: &watch::Sender<LinkState>,
    ) -> Option<String> {
        let mut ready = self.snapshot(state).await;
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    None => return None,
                    Some(Command::Retry) => ready = self.snapshot(state).await,
                    Some(command) if !ready => refuse(command, "the receiver is not ready"),
                    Some(Command::Cross { handoff, reduce_wifi_latency, stop, status }) => {
                        let context = self.next_context();
                        let activation = Activation {
                            session: &self.handle,
                            events: &mut self.events,
                            context,
                            raw_touch: self.raw_touch,
                        };
                        if run_crossing(activation, handoff, reduce_wifi_latency, stop, status).await {
                            // The receiver may have changed; check it before the next crossing.
                            ready = self.snapshot(state).await;
                        }
                    }
                },
                event = self.events.recv() => match event.map(|event| event.kind) {
                    Some(SessionEventKind::Closed { reason }) => return Some(reason),
                    Some(kind) => refuse_inbound(kind),
                    None => return Some("input session closed".into()),
                },
                () = tokio::time::sleep(SNAPSHOT_RETRY), if !ready => {
                    ready = self.snapshot(state).await;
                }
            }
        }
    }

    /// Reads the receiver's desktop into the link state.
    async fn snapshot(&mut self, state: &watch::Sender<LinkState>) -> bool {
        let geometry = self
            .handle
            .desktop_request(DesktopRequest::Snapshot)
            .await
            .and_then(|response| {
                response.validate()?;
                match response {
                    DesktopResponse::Snapshot { geometry, .. } => Ok(geometry),
                    DesktopResponse::Unavailable { reason } => bail!("{reason}"),
                    _ => bail!("Unexpected receiver response"),
                }
            });
        let ready = geometry.is_ok();
        state.send_replace(match geometry {
            Ok(geometry) => LinkState::Ready(geometry),
            Err(error) => LinkState::Down(format!("{error:#}")),
        });
        ready
    }

    async fn close(self) {
        self.handle.close(SessionCloseReason::LocalRelease);
        self.endpoint.close(0_u32.into(), b"sharing stopped");
        let _ = tokio::time::timeout(CLOSE_GRACE, self.endpoint.wait_idle()).await;
    }
}

/// Race the peer's pinned addresses, then discovered receivers after a short
/// delay, all within one timeout. SPKI pinning rejects every host that is not
/// this peer, so unverified addresses are safe to try.
async fn connect_peer(
    endpoint: &Endpoint,
    client: &InputClientConfig,
    pinned: &[SocketAddr],
    nearby: &[SocketAddr],
) -> Result<(SocketAddr, InputConnection)> {
    let ipv4 = pinned
        .first()
        .context("Computer has no input address")?
        .is_ipv4();
    let mut tried = BTreeSet::new();
    let mut attempts = tokio::task::JoinSet::new();
    let candidates = pinned.iter().map(|address| (*address, Duration::ZERO));
    let candidates = candidates.chain(nearby.iter().map(|address| (*address, NEARBY_DELAY)));
    // The endpoint is bound to the family of the first pinned address.
    for (address, delay) in candidates.filter(|(address, _)| address.is_ipv4() == ipv4) {
        if !tried.insert(address) {
            continue;
        }
        let (endpoint, client) = (endpoint.clone(), client.clone());
        attempts.spawn(async move {
            tokio::time::sleep(delay).await;
            let connection = connect_input(&endpoint, address, &client)
                .await
                .with_context(|| format!("could not connect to {address}"))?;
            Ok::<_, anyhow::Error>((address, connection))
        });
    }
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        let mut failure = anyhow!("no input address to try");
        while let Some(attempt) = attempts.join_next().await {
            match attempt
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
            {
                Ok(connected) => return Ok(connected),
                Err(error) => failure = error,
            }
        }
        Err(failure)
    })
    .await
    .context("input connection timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        capture::{CaptureFrame, CaptureTransition, CapturedDeviceFrame, KeyState},
        config::PeerPermissions,
        core::{HidUsage, ReceiverEffect},
        desktop::{Edge, FRACTION_MAX, Point, Rect},
        transport::{accept_input, input_server_config},
    };

    fn geometry() -> Geometry {
        Geometry {
            monitors: vec![Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }],
        }
    }

    fn answer(request: DesktopRequest) -> DesktopResponse {
        let position = Point { x: 0, y: 540 };
        match request {
            DesktopRequest::Snapshot => DesktopResponse::Snapshot {
                geometry: geometry(),
                position,
            },
            DesktopRequest::Prepare { .. } => DesktopResponse::Prepared {
                geometry: geometry(),
                position,
            },
            DesktopRequest::Poll { .. } => DesktopResponse::Active,
            DesktopRequest::Finish { .. } => DesktopResponse::Finished,
        }
    }

    /// A receiver that answers desktop requests and applies input, the way
    /// the Linux daemon does, without touching any device.
    struct Receiver {
        address: SocketAddr,
        sessions: mpsc::UnboundedReceiver<SessionHandle>,
        effects: mpsc::UnboundedReceiver<(u64, ReceiverEffect)>,
        closed: mpsc::UnboundedReceiver<u64>,
        _endpoint: Endpoint,
    }

    async fn receiver(directory: &std::path::Path, mac: &[u8], touch: bool) -> Receiver {
        let identity = Identity::load_or_create(directory).unwrap();
        let config = input_server_config(&identity, mac).unwrap();
        let endpoint =
            Endpoint::server(config.quinn_config(), "127.0.0.1:0".parse().unwrap()).unwrap();
        let mut settings = Config::default();
        settings.input.experimental_touchpad = touch;
        let options = SessionOptions::from_config(&settings).unwrap();
        let (sessions_tx, sessions) = mpsc::unbounded_channel();
        let (effects_tx, effects) = mpsc::unbounded_channel();
        let (closed_tx, closed) = mpsc::unbounded_channel();
        let accepting = endpoint.clone();
        tokio::spawn(async move {
            while let Some(incoming) = accepting.accept().await {
                let Ok(connection) = accept_input(incoming, &config).await else {
                    continue;
                };
                let (events_tx, mut events) = mpsc::channel(64);
                let Ok(session) = start_session(
                    connection,
                    "mac".into(),
                    TransportGeneration(1),
                    options.clone(),
                    events_tx,
                )
                .await
                else {
                    continue;
                };
                let _ = sessions_tx.send(session);
                let (effects_tx, closed_tx) = (effects_tx.clone(), closed_tx.clone());
                tokio::spawn(async move {
                    while let Some(event) = events.recv().await {
                        match event.kind {
                            SessionEventKind::Desktop { request, reply } => {
                                let _ = reply.send(answer(request));
                            }
                            SessionEventKind::ReceiverEffects {
                                effects, applied, ..
                            } => {
                                for effect in effects {
                                    let _ = effects_tx.send((event.session_id, effect));
                                }
                                let _ = applied.send(Ok(()));
                            }
                            SessionEventKind::Closed { .. } => {
                                let _ = closed_tx.send(event.session_id);
                            }
                            SessionEventKind::OutboundEnded => {}
                        }
                    }
                });
            }
        });
        Receiver {
            address: endpoint.local_addr().unwrap(),
            sessions,
            effects,
            closed,
            _endpoint: endpoint,
        }
    }

    fn mac_config(
        state: &std::path::Path,
        receiver: &std::path::Path,
        address: SocketAddr,
    ) -> Config {
        let receiver = Identity::load_or_create(receiver).unwrap();
        let mut config = Config::default();
        config.daemon.state_dir = state.to_owned();
        config.input.experimental_touchpad = true;
        let permissions = PeerPermissions {
            connect: true,
            receive_normal: true,
            ..PeerPermissions::default()
        };
        config.peers.insert(
            "linux".into(),
            PeerConfig::from_spki(receiver.spki(), vec![address], permissions).unwrap(),
        );
        config
    }

    fn key(state: KeyState) -> CapturedDeviceFrame {
        CapturedDeviceFrame {
            device_path: "test".into(),
            captured_at: Instant::now(),
            frame: CaptureFrame {
                transitions: vec![CaptureTransition::Key {
                    usage: HidUsage::keyboard(4),
                    state,
                }],
                ..CaptureFrame::default()
            },
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn crossings_reuse_one_session_with_a_new_activation_each() {
        let (mac, linux) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let spki = Identity::load_or_create(mac.path())
            .unwrap()
            .spki()
            .to_vec();
        let mut receiver = receiver(linux.path(), &spki, false).await;
        let config = mac_config(mac.path(), linux.path(), receiver.address);
        let mut session = Session::open("linux", &config.peers["linux"], &config, &[])
            .await
            .unwrap();
        let state = watch::channel(LinkState::Connecting).0;
        assert!(session.snapshot(&state).await);
        assert_eq!(*state.borrow(), LinkState::Ready(geometry()));
        let mut opened = Vec::new();
        let mut sessions = BTreeSet::new();
        for token in 1..=2 {
            // A crossing's steps on the link's session, without native capture.
            let prepare = DesktopRequest::Prepare {
                token,
                edge: Edge::Left,
                start: 0,
                end: FRACTION_MAX,
                position: 500_000,
            };
            let prepared = session.handle.desktop_request(prepare).await.unwrap();
            assert!(matches!(prepared, DesktopResponse::Prepared { .. }));
            // The previous release left an OutboundEnded behind.
            super::super::refuse_waiting_events(&mut session.events).unwrap();
            let context = session.next_context();
            session.handle.begin_outbound(context).unwrap();
            session.handle.capture(key(KeyState::Pressed)).unwrap();
            session.handle.capture(key(KeyState::Released)).unwrap();
            loop {
                let (id, effect) =
                    tokio::time::timeout(Duration::from_secs(2), receiver.effects.recv())
                        .await
                        .expect("the receiver applied the key")
                        .unwrap();
                sessions.insert(id);
                match effect {
                    ReceiverEffect::ActivationOpened(context) => opened.push(context),
                    ReceiverEffect::Key { pressed: false, .. } => break,
                    _ => {}
                }
            }
            session
                .handle
                .end_outbound(SessionCloseReason::LocalRelease)
                .await
                .unwrap();
            let finished = session
                .handle
                .desktop_request(DesktopRequest::Finish { token })
                .await
                .unwrap();
            assert_eq!(finished, DesktopResponse::Finished);
        }
        let activations: Vec<_> = opened.iter().map(|context| context.activation_id).collect();
        assert_eq!(activations, [ActivationId(1), ActivationId(2)]);
        assert_eq!(opened[0].session_epoch, opened[1].session_epoch);
        assert_eq!(
            sessions.len(),
            1,
            "both crossings used one receiver session"
        );
        receiver.sessions.recv().await.unwrap();
        assert!(
            receiver.sessions.try_recv().is_err(),
            "no second connection"
        );
        session.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn raw_touch_needs_a_receiver_that_negotiates_touch() {
        for touch in [false, true] {
            let (mac, linux) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let spki = Identity::load_or_create(mac.path())
                .unwrap()
                .spki()
                .to_vec();
            let receiver = receiver(linux.path(), &spki, touch).await;
            let config = mac_config(mac.path(), linux.path(), receiver.address);
            let session = Session::open("linux", &config.peers["linux"], &config, &[])
                .await
                .unwrap();
            assert_eq!(session.raw_touch, touch);
            session.close().await;
        }
    }

    fn wait_until(links: &mut Links, done: impl Fn(&Links) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(links) {
            assert!(Instant::now() < deadline, "link state did not settle");
            std::thread::sleep(Duration::from_millis(5));
        }
        links.changed();
    }

    fn ready(links: &Links) -> bool {
        links
            .states()
            .any(|(name, state)| name == "linux" && state == LinkState::Ready(geometry()))
    }

    fn handoff() -> HandoffOptions {
        HandoffOptions {
            entry_position: super::super::CursorPosition { x: 0.0, y: 50.0 },
            entry_region: Rect {
                x: 0,
                y: 0,
                width: 9,
                height: 100,
            },
            return_mapping: crate::desktop::ReturnMapping {
                geometry: geometry(),
                edge: Edge::Left,
                local_start: 0.0,
                local_end: 1.0,
                remote_start: 0.0,
                remote_end: 1.0,
            },
            edge: Edge::Right,
            start: 0,
            end: FRACTION_MAX,
            position: 500_000,
            expected_width: 1920,
            expected_height: 1080,
        }
    }

    // Links own a runtime, so this test drives them from a plain thread.
    #[test]
    fn a_link_reconnects_after_losing_its_session_and_closes_promptly() {
        let server = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (mac, linux) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let spki = Identity::load_or_create(mac.path())
            .unwrap()
            .spki()
            .to_vec();
        let mut receiver = server.block_on(receiver(linux.path(), &spki, false));
        let mut config = mac_config(mac.path(), linux.path(), receiver.address);
        let mut links = Links::new().unwrap();
        assert!(links.cross("linux", handoff(), false).is_none());
        links.sync(Some(&config));
        assert!(links.changed());
        wait_until(&mut links, ready);
        let first = server.block_on(receiver.sessions.recv()).unwrap();

        // Radio settings apply per crossing, so they leave the session alone.
        config.macos.block_awdl = true;
        let task = links.links["linux"].task.id();
        links.sync(Some(&config));
        assert_eq!(links.links["linux"].task.id(), task);

        // The receiver drops the session, as when its daemon restarts.
        first.close(SessionCloseReason::BackendUnavailable);
        let second = server
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), receiver.sessions.recv()).await
            })
            .expect("the link reconnected")
            .unwrap();
        assert_ne!(first.id(), second.id());
        wait_until(&mut links, ready);
        assert!(links.cross("other", handoff(), false).is_none());

        // Closing the link reaches the receiver now, not at its idle timeout.
        links.sync(None);
        assert!(links.states().next().is_none());
        server.block_on(async {
            loop {
                let id = tokio::time::timeout(Duration::from_secs(2), receiver.closed.recv())
                    .await
                    .expect("the receiver saw the close")
                    .unwrap();
                if id == second.id() {
                    break;
                }
            }
        });
        drop(links);
    }

    #[test]
    fn retries_start_at_once_and_back_off_to_a_cap() {
        let delays = [0, 1, 2, 3, 4, 5, 40].map(retry_delay);
        assert_eq!(delays, [0, 1, 2, 4, 8, 15, 15].map(Duration::from_secs));
    }

    #[tokio::test]
    async fn connection_finds_a_moved_peer_and_rejects_other_receivers() {
        let directories = [(); 3].map(|_| tempfile::tempdir().unwrap());
        let [mac, linux, stranger] =
            [0, 1, 2].map(|index| Identity::load_or_create(directories[index].path()).unwrap());
        let client = input_client_config(&mac, linux.spki()).unwrap();
        let mut servers = Vec::new();
        for identity in [&stranger, &linux] {
            let config = input_server_config(identity, mac.spki()).unwrap();
            let server =
                Endpoint::server(config.quinn_config(), "127.0.0.1:0".parse().unwrap()).unwrap();
            let accepting = server.clone();
            tokio::spawn(async move {
                let mut accepted = Vec::new();
                while let Some(incoming) = accepting.accept().await {
                    accepted.push(accept_input(incoming, &config).await);
                }
            });
            servers.push(server);
        }
        let [stranger, linux] = [&servers[0], &servers[1]].map(|s| s.local_addr().unwrap());
        // The pinned address stopped answering, as after a DHCP change.
        let stale = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let (address, _connection) = connect_peer(
            &endpoint,
            &client,
            &[stale.local_addr().unwrap()],
            &[stranger, linux],
        )
        .await
        .unwrap();
        assert_eq!(address, linux);
        assert!(
            connect_peer(&endpoint, &client, &[stranger], &[])
                .await
                .is_err()
        );
        endpoint.close(0_u32.into(), b"test finished");
        for server in servers {
            server.close(0_u32.into(), b"test finished");
        }
    }
}
