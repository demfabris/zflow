//! One authenticated input session per paired computer while sharing is on.
//! The desktop snapshot and every crossing reuse it, so a crossing costs a
//! Prepare round trip instead of a QUIC handshake and session negotiation.
//! The same session carries the peer's input when it controls this Mac,
//! and the shared layout both ways. Peers may also connect first; the Mac
//! keeps one session per peer.
//!
//! The same port answers hellos from computers that do not trust this Mac
//! yet, and links say hello to the ones the app asks about. What the hellos
//! tell goes back to the app, which decides who is trusted.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use quinn::{Endpoint, Incoming};
use tokio::{
    runtime::Runtime,
    sync::{Semaphore, mpsc, watch},
    task::JoinHandle,
};

use crate::{
    app::handoff::Handoff,
    config::{Config, PeerConfig},
    core::{
        ActivationId, InputCapability, SessionCloseReason, SessionContext, SessionEpoch,
        TransportGeneration,
    },
    desktop::{DesktopRequest, DesktopResponse, Geometry, SharedLayout},
    hello::local_hello,
    identity::{Identity, wins_simultaneous_dial},
    link::{Fix, STABLE_SESSION, retry_delay},
    session::{SessionEvent, SessionEventKind, SessionHandle, SessionOptions, start_session},
    transport::{
        Accepted, HelloClientConfig, HelloConnection, InputClientConfig, InputConnection,
        InputServerConfig, accept, connect_hello, connect_input, input_client_config,
        input_server_config_for_peers,
    },
    wire::Hello,
};

use super::{
    Activation, CursorPosition, SourceStatus,
    receive::{Inbound, OutboundGuard, Receiving},
    run_crossing,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const SNAPSHOT_RETRY: Duration = Duration::from_secs(5);
// A receiver still replacing this peer's previous session refuses the first
// snapshot for a few milliseconds, so retry quickly before backing off.
const FIRST_SNAPSHOT_RETRY: Duration = Duration::from_millis(250);
// Long enough for the close to leave; the receiver refuses a second session
// from this Mac while the old one is open.
const CLOSE_GRACE: Duration = Duration::from_millis(100);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
/// Handshakes the listener runs at once, as the Linux daemon allows.
const MAX_PENDING_ACCEPTS: usize = 8;

#[derive(Clone, Debug, PartialEq)]
pub enum LinkState {
    Connecting,
    /// Connected, with the receiver's current desktop. Only this state crosses.
    Ready(Geometry),
    /// Connected to a peer that does not take input from this Mac, so it
    /// can only control it.
    Connected,
    /// Connected, but the peer takes no input from this Mac for now, and
    /// says why. It can still control this Mac, and the link keeps asking.
    Refused(String),
    /// Not usable right now. The link keeps retrying.
    Down(String),
}

/// A crossing handed to a link. Setting or dropping `stop` returns input to
/// the Mac. `status` closes once cleanup has finished.
pub struct Crossing {
    pub stop: watch::Sender<bool>,
    pub status: mpsc::UnboundedReceiver<SourceStatus>,
}

/// What this Mac says in a hello: its key, the port it takes input on, and
/// the keys it trusts, so each hello says whether it trusts the other side.
#[derive(Clone)]
pub struct Greeting {
    pub client: HelloClientConfig,
    pub port: u16,
    pub trusted: Arc<BTreeSet<Vec<u8>>>,
}

impl Greeting {
    fn hello(&self, peer_spki: &[u8]) -> Hello {
        local_hello(self.port, self.trusted.contains(peer_spki))
    }
}

/// What came of hellos and connections, for the app to weigh. Only a
/// session's address was proven by the key it trusts.
pub enum Heard {
    /// A computer is saying hello. Answer it with [`Links::answer`], or
    /// drop it to hang up.
    Knock(HelloConnection),
    /// A computer's hello: the answer to one this Mac sent to the record
    /// `instance`, or one that came in.
    Hello {
        instance: Option<String>,
        spki: Vec<u8>,
        remote: SocketAddr,
        hello: Box<Hello>,
    },
    /// A connection was turned away because too many were being set up at
    /// once. It could have been a computer not seen otherwise.
    TurnedAway,
    /// A trusted computer's input session came up: this Mac dialed it at
    /// `address`, or it connected from there. Unlike a hello's word, TLS
    /// proved the key at that address, so it is worth saving.
    Connected {
        /// The computer's `spki_der_hex`.
        key: String,
        address: SocketAddr,
        dialed: bool,
    },
}

/// Where each paired computer was found just now, by its key
/// (`spki_der_hex`). Links try these before the saved addresses.
pub type Found = BTreeMap<String, Vec<SocketAddr>>;

/// A session a peer opened to this Mac.
struct Opened {
    handle: SessionHandle,
    events: mpsc::Receiver<SessionEvent>,
    /// The address this Mac dialed, or the one the peer connected from.
    address: SocketAddr,
}

impl Opened {
    fn close(self, reason: SessionCloseReason) {
        self.handle.close(reason);
    }
}

/// Carries the shared layout between the app and one link's sessions.
/// The app keeps one layout for every peer; each session gets it when it
/// starts and whenever it changes, and hands back a newer one.
pub(super) struct LayoutRoute {
    peer: String,
    kept: watch::Receiver<Option<SharedLayout>>,
    received: mpsc::UnboundedSender<(String, SharedLayout)>,
    /// The newest layout the current session's peer is known to hold.
    peer_has: Option<SharedLayout>,
}

impl LayoutRoute {
    /// Sends the kept layout if the peer does not hold it or a newer one.
    fn offer(&mut self, session: &SessionHandle) {
        let Some(kept) = self.kept.borrow_and_update().clone() else {
            return;
        };
        if self
            .peer_has
            .as_ref()
            .is_some_and(|has| !kept.is_newer_than(has))
        {
            return;
        }
        match session.send_layout(kept.clone()) {
            Ok(()) => self.peer_has = Some(kept),
            Err(error) => tracing::debug!(peer = %self.peer, %error, "layout not sent"),
        }
    }

    /// Takes the peer's layout: a newer one goes to the app, and an older
    /// one gets the kept layout back.
    pub(super) fn take(&mut self, session: &SessionHandle, layout: SharedLayout) {
        self.peer_has = Some(layout.clone());
        let newer = self
            .kept
            .borrow()
            .as_ref()
            .is_none_or(|kept| layout.is_newer_than(kept));
        if newer {
            let _ = self.received.send((self.peer.clone(), layout));
        } else {
            self.offer(session);
        }
    }
}

enum Command {
    Cross {
        handoff: Handoff,
        entry_position: CursorPosition,
        reduce_wifi_latency: bool,
        stop: watch::Receiver<bool>,
        status: mpsc::UnboundedSender<SourceStatus>,
        /// Keeps a peer from taking control until input is back on the Mac.
        guard: OutboundGuard,
    },
    Inbound(Opened),
    Retry,
}

/// What a link's session depends on. The rest of a peer's record, such as
/// whether it may control this Mac, reaches the link without a reconnect.
/// So do its addresses: a link finds its computer by key, and an address
/// learned while connected must not drop the session.
#[derive(Clone, PartialEq)]
struct LinkKey {
    spki_der_hex: String,
    receive_normal: bool,
}

impl LinkKey {
    fn of(peer: &PeerConfig) -> Self {
        Self {
            spki_der_hex: peer.spki_der_hex.clone(),
            receive_normal: peer.permissions.receive_normal,
        }
    }
}

struct Link {
    key: LinkKey,
    config: Config,
    commands: mpsc::UnboundedSender<Command>,
    state: watch::Receiver<LinkState>,
    task: JoinHandle<()>,
}

/// What the accept loop needs, kept current by `Links::sync`.
#[derive(Default)]
struct Accepting {
    server: Option<InputServerConfig>,
    options: Option<SessionOptions>,
    /// Each link's peer and commands, by the peer's key.
    routes: BTreeMap<Vec<u8>, (String, mpsc::UnboundedSender<Command>)>,
}

/// Takes connections on `transport.listen`: input from paired computers,
/// and hellos from any. It has its own endpoint, so a port in use only
/// stops connections coming in.
struct Listener {
    address: SocketAddr,
    endpoint: Endpoint,
    accepting: Arc<Mutex<Accepting>>,
    task: JoinHandle<()>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Sessions already accepted keep running on the endpoint.
        self.task.abort();
    }
}

pub struct Links {
    runtime: Option<Runtime>,
    links: BTreeMap<String, Link>,
    /// Replaced links still returning input or closing their session.
    closing: BTreeMap<String, JoinHandle<()>>,
    found: watch::Sender<Found>,
    changed: bool,
    receiving: Arc<Receiving>,
    listener: Option<Listener>,
    listen_error: Option<String>,
    /// How long after this Mac's dial a peer's connection counts as dialed
    /// at the same moment.
    dial_window: Duration,
    /// The layout this Mac keeps, which every session gets.
    layout: watch::Sender<Option<SharedLayout>>,
    /// Newer layouts peers sent, by peer, until the app takes them.
    received: mpsc::UnboundedReceiver<(String, SharedLayout)>,
    received_sender: mpsc::UnboundedSender<(String, SharedLayout)>,
    /// Hellos and refusals, until the app takes them.
    heard: mpsc::UnboundedReceiver<Heard>,
    heard_sender: mpsc::UnboundedSender<Heard>,
    /// Hellos this Mac is sending.
    dialing: Vec<JoinHandle<()>>,
}

impl Links {
    pub fn new() -> Result<Self> {
        Self::with_receiving(Receiving::mac()?)
    }

    /// Links that post through `backend` instead of on this Mac, with
    /// motion that is not accelerated, and a pasteboard of their own.
    #[cfg(test)]
    pub(crate) fn with_backend(backend: super::inject::FakeBackend) -> Result<Self> {
        Self::with_fakes(backend, Default::default())
    }

    /// Links that also share `pasteboard` instead of this Mac's.
    #[cfg(test)]
    pub(crate) fn with_fakes(
        backend: super::inject::FakeBackend,
        pasteboard: super::clipboard::FakePasteboard,
    ) -> Result<Self> {
        let profile = super::pointer::Profile::Flat { speed: 0.0 };
        let injector = super::inject::Injector::start(backend.clone(), profile)?;
        let receiving = Receiving::new(injector, Arc::new(backend), Arc::new(pasteboard));
        Self::with_receiving(receiving)
    }

    fn with_receiving(receiving: Arc<Receiving>) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("zflow-sharing")
            .enable_all()
            .build()
            .context("Could not start the sharing runtime")?;
        runtime.spawn({
            let receiving = receiving.clone();
            async move { receiving.watch().await }
        });
        let (received_sender, received) = mpsc::unbounded_channel();
        let (heard_sender, heard) = mpsc::unbounded_channel();
        Ok(Self {
            runtime: Some(runtime),
            links: BTreeMap::new(),
            closing: BTreeMap::new(),
            found: watch::channel(Found::new()).0,
            changed: false,
            receiving,
            listener: None,
            listen_error: None,
            dial_window: CONNECT_TIMEOUT,
            layout: watch::channel(None).0,
            received,
            received_sender,
            heard,
            heard_sender,
            dialing: Vec::new(),
        })
    }

    /// While `sharing`, keeps one link per peer that may connect and closes
    /// the others; while paused, closes every link. Either way it listens
    /// while any peer may connect, or while `welcome` lets computers that do
    /// not trust this Mac yet say hello. A peer whose key or permission to
    /// receive changed reconnects, and so does every peer when the session
    /// settings change.
    pub fn sync(&mut self, config: &Config, sharing: bool, welcome: bool) {
        // Made here once, so link tasks and the listener never race to
        // create it.
        if let Err(error) = Identity::load_or_create(&config.daemon.state_dir) {
            tracing::warn!(%error, "could not read this Mac's key");
        }
        self.sync_links(sharing.then_some(config));
        self.listen(config, welcome);
    }

    fn sync_links(&mut self, config: Option<&Config>) {
        let peers = config.map(|config| config.peers.clone());
        self.receiving.set_peers(peers.unwrap_or_default());
        let share = config.is_some_and(|config| config.clipboard.share);
        self.receiving.clipboard().set_share(share);
        let settings = config.map(session_settings);
        let wanted: BTreeMap<&String, &PeerConfig> = config
            .into_iter()
            .flat_map(|config| &config.peers)
            .filter(|(_, peer)| peer.permissions.connect)
            .collect();
        let stale: Vec<String> = self
            .links
            .iter()
            .filter(|(name, link)| {
                wanted.get(name).map(|peer| LinkKey::of(peer)) != Some(link.key.clone())
                    || settings.as_ref() != Some(&link.config)
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
            let remote = Remote {
                name: name.clone(),
                peer: peer.clone(),
                config: settings.clone(),
                window: self.dial_window,
                heard: self.heard_sender.clone(),
            };
            let layouts = LayoutRoute {
                peer: name.clone(),
                kept: self.layout.subscribe(),
                received: self.received_sender.clone(),
                peer_has: None,
            };
            let task = runtime.spawn(run(
                remote,
                self.receiving.clone(),
                receiver,
                state_sender,
                self.found.subscribe(),
                layouts,
                self.closing.remove(name),
            ));
            self.links.insert(
                name.clone(),
                Link {
                    key: LinkKey::of(peer),
                    config: settings.clone(),
                    commands,
                    state,
                    task,
                },
            );
            self.changed = true;
        }
    }

    /// Listens on `transport.listen` for every peer that may connect, and
    /// for hellos while `welcome`, or on nothing. The Mac still dials when
    /// this fails. While sharing is paused there are no links, so input
    /// from a peer is closed once it arrives. The allowlist keeps its key,
    /// or the peer would hear that this Mac never added it.
    fn listen(&mut self, config: &Config, welcome: bool) {
        let peers: Vec<(&String, Vec<u8>)> = config
            .peers
            .iter()
            .filter(|(_, peer)| peer.permissions.connect)
            .filter_map(|(name, peer)| Some((name, peer.spki_der().ok()?)))
            .collect();
        if peers.is_empty() && !welcome {
            self.listener = None;
            self.listen_error = None;
            return;
        }
        let accepting = (|| {
            let identity = Identity::load_or_create(&config.daemon.state_dir)?;
            let server =
                input_server_config_for_peers(&identity, peers.iter().map(|(_, spki)| spki))?;
            let routes = peers
                .iter()
                .filter_map(|(name, spki)| {
                    let link = self.links.get(*name)?;
                    Some((spki.clone(), ((*name).clone(), link.commands.clone())))
                })
                .collect();
            Ok::<_, anyhow::Error>(Accepting {
                server: Some(server),
                options: Some(SessionOptions::from_config(config)?),
                routes,
            })
        })();
        let accepting = match accepting {
            Ok(accepting) => accepting,
            Err(error) => {
                self.listener = None;
                self.listen_error = Some(format!(
                    "Other computers cannot connect to this Mac: {error:#}"
                ));
                return;
            }
        };
        let address = config.transport.listen;
        if let Some(listener) = self
            .listener
            .as_ref()
            .filter(|listener| listener.address == address)
        {
            let server = accepting
                .server
                .as_ref()
                .map(InputServerConfig::quinn_config);
            listener.endpoint.set_server_config(server);
            *listener
                .accepting
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = accepting;
            return;
        }
        self.listener = None;
        let Some(runtime) = &self.runtime else {
            return;
        };
        // Quinn needs the runtime to bind.
        let _entered = runtime.enter();
        match Endpoint::client(address) {
            Ok(endpoint) => {
                endpoint.set_server_config(
                    accepting
                        .server
                        .as_ref()
                        .map(InputServerConfig::quinn_config),
                );
                let accepting = Arc::new(Mutex::new(accepting));
                let task = runtime.spawn(take_connections(
                    endpoint.clone(),
                    accepting.clone(),
                    self.heard_sender.clone(),
                ));
                tracing::info!(address = %endpoint.local_addr().unwrap_or(address), "listening for other computers");
                self.listener = Some(Listener {
                    address,
                    endpoint,
                    accepting,
                    task,
                });
                self.listen_error = None;
            }
            Err(error) => {
                let message = format!(
                    "Other computers cannot connect to this Mac: could not listen on {address}: {error}"
                );
                // The app tries again every few seconds.
                if self.listen_error.as_ref() != Some(&message) {
                    tracing::warn!(%address, %error, "could not listen for other computers");
                }
                self.listen_error = Some(message);
            }
        }
    }

    /// Why the clipboard last went nowhere, while that still holds.
    pub fn clipboard_notice(&self) -> Option<String> {
        self.receiving.clipboard().notice()
    }

    /// Why peers cannot connect to this Mac, if they cannot.
    pub fn listen_error(&self) -> Option<&str> {
        self.listen_error.as_deref()
    }

    /// The port peers connect to, while this Mac listens.
    pub fn listen_port(&self) -> Option<u16> {
        let listener = self.listener.as_ref()?;
        Some(listener.endpoint.local_addr().ok()?.port())
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

    /// Hands a crossing to its peer's link, if that link is ready and no
    /// peer controls this Mac. `entry_position` is where the cursor reached
    /// the edge.
    pub(crate) fn cross(
        &self,
        handoff: Handoff,
        entry_position: CursorPosition,
        reduce_wifi_latency: bool,
    ) -> Option<Crossing> {
        let link = self.links.get(&handoff.peer)?;
        if !matches!(*link.state.borrow(), LinkState::Ready(_)) {
            return None;
        }
        let guard = self.receiving.ownership().begin_outbound()?;
        let (stop, stopped) = watch::channel(false);
        let (status, events) = mpsc::unbounded_channel();
        link.commands
            .send(Command::Cross {
                handoff,
                entry_position,
                reduce_wifi_latency,
                stop: stopped,
                status,
                guard,
            })
            .ok()?;
        Some(Crossing {
            stop,
            status: events,
        })
    }

    /// The peer controlling this Mac, or about to.
    pub fn controller(&self) -> Option<String> {
        self.receiving.ownership().controller()
    }

    /// Whether Accessibility lets peers control this Mac, and whether AWDL
    /// goes down while one does. The app sets both on every tick; turning
    /// Accessibility off ends control at once.
    pub fn set_receive_policy(&self, accessibility: bool, reduce_wifi_latency: bool) {
        self.receiving.set_accessibility(accessibility);
        self.receiving.set_reduce_wifi_latency(reduce_wifi_latency);
    }

    /// Reconnects waiting links now and rechecks connected receivers.
    pub fn retry(&self) {
        for link in self.links.values() {
            let _ = link.commands.send(Command::Retry);
        }
    }

    /// Makes `layout` the one every peer gets: each session is sent it
    /// unless its peer already holds it or a newer one.
    pub fn share_layout(&self, layout: SharedLayout) {
        self.layout.send_if_modified(|current| {
            let changed = current.as_ref() != Some(&layout);
            *current = Some(layout);
            changed
        });
    }

    /// Layouts peers sent that are newer than the shared one, oldest first.
    pub fn take_layouts(&mut self) -> Vec<(String, SharedLayout)> {
        std::iter::from_fn(|| self.received.try_recv().ok()).collect()
    }

    /// Where each paired computer was found just now. A link that waits to
    /// retry dials at once when its computer's addresses change.
    pub fn set_found(&self, found: Found) {
        self.found.send_if_modified(|current| {
            let changed = *current != found;
            *current = found;
            changed
        });
    }

    /// Says hello to the record `instance` at `addresses`, trying each in
    /// turn. The answer comes back from [`Self::take_heard`]; a record that
    /// never answers comes back as nothing.
    pub fn say_hello(&mut self, instance: String, addresses: Vec<SocketAddr>, greeting: Greeting) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let heard = self.heard_sender.clone();
        self.dialing.push(runtime.spawn(async move {
            for remote in addresses {
                let said = tokio::time::timeout(CONNECT_TIMEOUT, dial_hello(remote, &greeting));
                match said.await.unwrap_or_else(|_| Err(anyhow!("timed out"))) {
                    Ok((spki, hello)) => {
                        let instance = Some(instance);
                        let _ = heard.send(Heard::Hello {
                            instance,
                            spki,
                            remote,
                            hello: Box::new(hello),
                        });
                        return;
                    }
                    Err(error) => tracing::debug!(%instance, %remote, error = %format!("{error:#}"), "no hello"),
                }
            }
        }));
    }

    /// Hellos this Mac is still waiting on.
    pub fn hellos_in_flight(&mut self) -> usize {
        self.dialing.retain(|task| !task.is_finished());
        self.dialing.len()
    }

    /// Answers a computer that knocked, with this Mac's hello.
    pub fn answer(&self, knock: HelloConnection, greeting: Greeting) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let heard = self.heard_sender.clone();
        runtime.spawn(async move {
            let (spki, remote) = (knock.peer_spki().to_vec(), knock.remote_address());
            let local = greeting.hello(&spki);
            match tokio::time::timeout(CONNECT_TIMEOUT, knock.exchange(&local)).await {
                Ok(Ok(hello)) => {
                    let _ = heard.send(Heard::Hello {
                        instance: None,
                        spki,
                        remote,
                        hello: Box::new(hello),
                    });
                }
                Ok(Err(error)) => tracing::debug!(%remote, %error, "hello not answered"),
                Err(_) => tracing::debug!(%remote, "hello timed out"),
            }
        });
    }

    /// Hellos and refusals since the last call, oldest first.
    pub fn take_heard(&mut self) -> Vec<Heard> {
        std::iter::from_fn(|| self.heard.try_recv().ok()).collect()
    }
}

/// Says hello to whatever answers at `remote`, from an endpoint of its own,
/// and returns its key and hello.
async fn dial_hello(remote: SocketAddr, greeting: &Greeting) -> Result<(Vec<u8>, Hello)> {
    let endpoint = Endpoint::client(unspecified_like(remote))?;
    let said = async {
        let connection = connect_hello(&endpoint, remote, &greeting.client).await?;
        let spki = connection.peer_spki().to_vec();
        let hello = connection.exchange(&greeting.hello(&spki)).await?;
        Ok::<_, anyhow::Error>((spki, hello))
    }
    .await;
    endpoint.close(0_u32.into(), b"");
    said
}

/// Any port on the unspecified address of `remote`'s family.
fn unspecified_like(remote: SocketAddr) -> SocketAddr {
    if remote.is_ipv4() {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    }
}

impl Drop for Links {
    fn drop(&mut self) {
        self.listener = None;
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
        switching: Default::default(),
        clipboard: Default::default(),
        ..config.clone()
    }
}

/// One link's peer, and the settings its sessions use.
struct Remote {
    name: String,
    peer: PeerConfig,
    config: Config,
    /// How long after this Mac's dial a peer's connection counts as dialed
    /// at the same moment.
    window: Duration,
    /// Where each session's address goes, for the app to save.
    heard: mpsc::UnboundedSender<Heard>,
}

impl Remote {
    /// Whether this Mac's connection stays when both computers dial at once.
    /// Both sides keep the one dialed by the computer whose key sorts first.
    fn wins(&self) -> bool {
        let local = Identity::load_or_create(&self.config.daemon.state_dir);
        let peer = self.peer.fingerprint_hex();
        local
            .ok()
            .zip(peer.ok())
            .is_some_and(|(local, peer)| wins_simultaneous_dial(&local.fingerprint_hex(), &peer))
    }

    /// Where to dial the peer: where its key was found just now, then the
    /// addresses saved with it. Each dial pins the key, so a stale or wrong
    /// address only fails to connect.
    fn addresses(&self, found: &Found) -> Vec<SocketAddr> {
        let found = found.get(&self.peer.spki_der_hex).into_iter().flatten();
        let mut addresses = Vec::new();
        for address in found.chain(&self.peer.addresses) {
            if !addresses.contains(address) {
                addresses.push(*address);
            }
        }
        addresses
    }
}

/// How a link's session ended.
enum Served {
    /// The link was closed.
    Stopped,
    Lost(String),
    /// The peer connected again, and its session replaces this one.
    Replaced(Opened),
}

async fn run(
    remote: Remote,
    receiving: Arc<Receiving>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    state: watch::Sender<LinkState>,
    mut found: watch::Receiver<Found>,
    mut layouts: LayoutRoute,
    previous: Option<JoinHandle<()>>,
) {
    // The receiver refuses a second session from this Mac while the old one is open.
    if let Some(previous) = previous {
        let _ = previous.await;
    }
    let name = &remote.name;
    let wins = remote.wins();
    let mut failures = 0_u32;
    let mut adopted = None;
    let mut tried = Vec::new();
    loop {
        let opened = match adopted.take() {
            Some(theirs) => Session::adopt(theirs, &remote),
            None => {
                tried = remote.addresses(&found.borrow_and_update());
                let dial = Session::open(name, &remote.peer, &remote.config, &tried);
                let Some((ours, theirs)) = opening(&mut commands, dial).await else {
                    return;
                };
                keep_one(ours, theirs, wins, &remote).await
            }
        };
        match opened {
            Ok(mut session) => {
                let _ = remote.heard.send(Heard::Connected {
                    key: remote.peer.spki_der_hex.clone(),
                    address: session.address,
                    dialed: session.endpoint.is_some(),
                });
                let opened_at = Instant::now();
                let mut inbound = receiving.inbound(name, &session.handle);
                let served = session
                    .serve(
                        &mut commands,
                        &state,
                        &mut inbound,
                        &mut layouts,
                        wins,
                        remote.window,
                    )
                    .await;
                // Lets go of whatever the peer held before the session goes.
                drop(inbound);
                let reason = match served {
                    Served::Stopped => {
                        session.close(SessionCloseReason::LocalRelease).await;
                        return;
                    }
                    Served::Replaced(theirs) => {
                        tracing::info!(peer = %name, "the other computer connected again; using its connection");
                        session.close(SessionCloseReason::Superseded).await;
                        adopted = Some(theirs);
                        failures = 0;
                        continue;
                    }
                    Served::Lost(reason) => reason,
                };
                state.send_replace(LinkState::Down(reason.clone()));
                session.close(SessionCloseReason::LocalRelease).await;
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
                tracing::warn!(peer = %name, error = %format_args!("{error:#}"), failures,
                    "input link could not connect");
                state.send_replace(LinkState::Down(crate::link::reason(&error)));
            }
        }
        let delay = retry_delay(failures);
        match wait_to_retry(&mut commands, &mut found, &remote, &tried, delay).await {
            Next::Dial => {}
            Next::Adopt(theirs) => adopted = Some(theirs),
            Next::Stop => return,
        }
    }
}

/// Picks one session when the peer connected while this Mac dialed it:
/// this Mac's if its dial wins, otherwise the peer's. A failed dial takes
/// the peer's.
async fn keep_one(
    ours: Result<Session>,
    theirs: Option<Opened>,
    wins: bool,
    remote: &Remote,
) -> Result<Session> {
    let Some(theirs) = theirs else {
        return ours;
    };
    match ours {
        Ok(ours) if wins => {
            tracing::info!(peer = %remote.name, "both computers dialed at once; keeping this Mac's connection");
            theirs.close(SessionCloseReason::Superseded);
            Ok(ours)
        }
        Ok(ours) => {
            tracing::info!(peer = %remote.name, "both computers dialed at once; keeping theirs");
            ours.close(SessionCloseReason::Superseded).await;
            Session::adopt(theirs, remote)
        }
        Err(_) => Session::adopt(theirs, remote),
    }
}

fn refuse(command: Command, reason: &str) {
    match command {
        Command::Cross { status, guard, .. } => {
            drop(guard);
            let _ = status.send(SourceStatus::Cancelled(reason.into()));
        }
        Command::Inbound(opened) => opened.close(SessionCloseReason::Superseded),
        Command::Retry => {}
    }
}

/// Runs `work`, a dial, while refusing crossings, and keeps the newest
/// session the peer opened meanwhile. None means the link was closed.
async fn opening<T>(
    commands: &mut mpsc::UnboundedReceiver<Command>,
    work: impl Future<Output = T>,
) -> Option<(T, Option<Opened>)> {
    tokio::pin!(work);
    let mut theirs: Option<Opened> = None;
    loop {
        tokio::select! {
            output = &mut work => return Some((output, theirs)),
            command = commands.recv() => match command {
                Some(Command::Inbound(opened)) => {
                    if let Some(older) = theirs.replace(opened) {
                        older.close(SessionCloseReason::Superseded);
                    }
                }
                Some(command) => refuse(command, "the other computer is not connected"),
                None => {
                    if let Some(theirs) = theirs {
                        theirs.close(SessionCloseReason::LocalRelease);
                    }
                    return None;
                }
            },
        }
    }
}

/// What a link does after waiting.
enum Next {
    Dial,
    /// Use the session the peer opened meanwhile.
    Adopt(Opened),
    Stop,
}

/// Waits before the next attempt. Retry or addresses for the peer other
/// than the ones `tried` end the wait early, and so does the peer connecting.
async fn wait_to_retry(
    commands: &mut mpsc::UnboundedReceiver<Command>,
    found: &mut watch::Receiver<Found>,
    remote: &Remote,
    tried: &[SocketAddr],
    delay: Duration,
) -> Next {
    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    if remote.addresses(&found.borrow_and_update()) != tried {
        return Next::Dial;
    }
    loop {
        tokio::select! {
            () = &mut sleep => return Next::Dial,
            Ok(()) = found.changed() => {
                if remote.addresses(&found.borrow_and_update()) != tried {
                    return Next::Dial;
                }
            }
            command = commands.recv() => match command {
                Some(Command::Retry) => return Next::Dial,
                Some(Command::Inbound(opened)) => return Next::Adopt(opened),
                Some(command) => refuse(command, "the other computer is not connected"),
                None => return Next::Stop,
            },
        }
    }
}

/// Takes connections until the listener is dropped. Hellos and refusals go
/// to `heard`.
async fn take_connections(
    endpoint: Endpoint,
    accepting: Arc<Mutex<Accepting>>,
    heard: mpsc::UnboundedSender<Heard>,
) {
    let slots = Arc::new(Semaphore::new(MAX_PENDING_ACCEPTS));
    while let Some(incoming) = endpoint.accept().await {
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            tracing::warn!("too many computers connecting at once");
            incoming.refuse();
            let _ = heard.send(Heard::TurnedAway);
            continue;
        };
        let (accepting, heard) = (accepting.clone(), heard.clone());
        tokio::spawn(async move {
            let _slot = slot;
            let opened = open_inbound(incoming, &accepting, &heard);
            let opened = tokio::time::timeout(CONNECT_TIMEOUT, opened)
                .await
                .context("handshake timed out")
                .and_then(|opened| opened);
            if let Err(error) = opened {
                tracing::warn!(error = %format!("{error:#}"), "connection refused");
            }
        });
    }
}

/// Sorts a connection by what it asks for. Input from a peer is
/// negotiated into a session and handed to that peer's link; the session
/// starts here, so it is ready even while the link runs a crossing. A
/// hello, or input from a key not trusted here, goes to `heard`.
async fn open_inbound(
    incoming: Incoming,
    accepting: &Mutex<Accepting>,
    heard: &mpsc::UnboundedSender<Heard>,
) -> Result<()> {
    let lock = || accepting.lock().unwrap_or_else(PoisonError::into_inner);
    let (server, options) = {
        let accepting = lock();
        (accepting.server.clone(), accepting.options.clone())
    };
    let (server, options) = server
        .zip(options)
        .context("This Mac takes no connections")?;
    let connection = match accept(incoming, &server).await? {
        Accepted::Input(connection) => connection,
        Accepted::Hello(knock) => {
            let _ = heard.send(Heard::Knock(knock));
            return Ok(());
        }
        Accepted::NotTrusted { remote_address, .. } => {
            tracing::info!(%remote_address, "a computer not added here asked for input");
            return Ok(());
        }
    };
    let route = lock().routes.get(connection.peer_spki()).cloned();
    let Some((name, commands)) = route else {
        connection.close();
        bail!("the computer that connected has no link here, as while sharing is paused");
    };
    let address = connection.remote_address();
    let (sender, events) = mpsc::channel(128);
    let handle = start_session(
        connection,
        name.clone(),
        TransportGeneration(1),
        options,
        sender,
    )
    .await?;
    tracing::info!(peer = %name, %address, session_id = handle.id(), "input link accepted");
    if let Err(mpsc::error::SendError(command)) = commands.send(Command::Inbound(Opened {
        handle,
        events,
        address,
    })) {
        refuse(command, "the link closed");
    }
    Ok(())
}

struct Session {
    /// The endpoint this Mac dialed from. None for a session the peer opened.
    endpoint: Option<Endpoint>,
    /// When this Mac's dial finished.
    dialed_at: Option<Instant>,
    /// Where the peer is: the address this Mac dialed, or the one it
    /// connected from.
    address: SocketAddr,
    handle: SessionHandle,
    events: mpsc::Receiver<SessionEvent>,
    epoch: SessionEpoch,
    activations: u64,
    raw_touch: bool,
    /// The peer takes input from this Mac, so the link reads its desktop.
    sends: bool,
}

impl Session {
    async fn open(
        name: &str,
        peer: &PeerConfig,
        config: &Config,
        addresses: &[SocketAddr],
    ) -> Result<Self> {
        let identity = Identity::load_or_create(&config.daemon.state_dir)?;
        let client = input_client_config(&identity, &peer.spki_der()?)?;
        let options = SessionOptions::from_config(config)?;
        // One endpoint dials every address of its family, and most
        // networks give each computer an IPv4 address.
        let family = addresses
            .iter()
            .find(|address| address.is_ipv4())
            .or(addresses.first())
            .context("Computer has no input address")?;
        let endpoint = Endpoint::client(unspecified_like(*family))?;
        let started = Instant::now();
        let opened = async {
            let (address, connection) = connect_peer(&endpoint, &client, addresses).await?;
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
            Ok::<_, anyhow::Error>(Opened {
                handle,
                events,
                address,
            })
        }
        .await;
        let opened = match opened {
            Ok(opened) => opened,
            Err(error) => {
                endpoint.close(0_u32.into(), b"input link failed");
                return Err(error);
            }
        };
        let session = Self::new(opened, Some(endpoint), peer, config)?;
        Ok(Self {
            dialed_at: Some(Instant::now()),
            ..session
        })
    }

    /// Takes over a session the peer opened.
    fn adopt(opened: Opened, remote: &Remote) -> Result<Self> {
        let session = Self::new(opened, None, &remote.peer, &remote.config)?;
        tracing::info!(peer = %remote.name, session_id = session.handle.id(), "using the other computer's connection");
        Ok(session)
    }

    fn new(
        opened: Opened,
        endpoint: Option<Endpoint>,
        peer: &PeerConfig,
        config: &Config,
    ) -> Result<Self> {
        let Opened {
            handle,
            events,
            address,
        } = opened;
        let mut epoch = [0_u8; 16];
        if let Err(error) = getrandom::fill(&mut epoch) {
            handle.close(SessionCloseReason::LocalRelease);
            if let Some(endpoint) = endpoint {
                endpoint.close(0_u32.into(), b"input link failed");
            }
            bail!("could not create the source session epoch: {error}");
        }
        // A receiver without Touch drops contact snapshots, so keep pointer
        // and scroll instead of suppressing them while a finger is down.
        let raw_touch = config.input.experimental_touchpad
            && handle.capabilities().contains(InputCapability::Touch);
        Ok(Self {
            endpoint,
            dialed_at: None,
            address,
            handle,
            events,
            epoch: SessionEpoch(epoch),
            activations: 0,
            raw_touch,
            sends: peer.permissions.receive_normal,
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

    /// Serves crossings and the peer's input until the session ends, the
    /// link is closed, or the peer's newer session replaces this one. A
    /// session the peer opens while this Mac's dial is `window` old or
    /// younger counts as dialed at the same moment, and `wins` picks one.
    async fn serve(
        &mut self,
        commands: &mut mpsc::UnboundedReceiver<Command>,
        state: &watch::Sender<LinkState>,
        inbound: &mut Inbound,
        layouts: &mut LayoutRoute,
        wins: bool,
        window: Duration,
    ) -> Served {
        // A new session holds nothing yet.
        layouts.peer_has = None;
        layouts.offer(&self.handle);
        // The peer's input keeps flowing while its desktop is read, so the
        // answer is awaited next to it. Only crossings wait for it.
        let mut snapshot = self.ask(inbound);
        let mut ready = false;
        if !self.sends {
            state.send_replace(LinkState::Connected);
        }
        let mut retry = FIRST_SNAPSHOT_RETRY;
        loop {
            if ready {
                retry = FIRST_SNAPSHOT_RETRY;
            }
            tokio::select! {
                command = commands.recv() => match command {
                    None => return Served::Stopped,
                    Some(Command::Retry) => {
                        ready = false;
                        snapshot = snapshot.or_else(|| self.ask(inbound));
                    }
                    Some(Command::Inbound(theirs)) => {
                        // A peer that connects later lost its connection,
                        // so its new one replaces this one.
                        let simultaneous = self.dialed_at.is_some_and(|at| at.elapsed() < window);
                        if !(simultaneous && wins) {
                            return Served::Replaced(theirs);
                        }
                        tracing::info!(peer = %self.handle.peer(), "both computers dialed at once; keeping this Mac's connection");
                        theirs.close(SessionCloseReason::Superseded);
                    }
                    Some(command) if !ready => refuse(command, "the other computer is not ready"),
                    Some(Command::Cross {
                        handoff,
                        entry_position,
                        reduce_wifi_latency,
                        stop,
                        status,
                        guard,
                    }) => {
                        let context = self.next_context();
                        let clipboard = inbound.clipboard();
                        let activation = Activation {
                            session: &self.handle,
                            events: &mut self.events,
                            layouts: &mut *layouts,
                            clipboard: &clipboard,
                            context,
                            raw_touch: self.raw_touch,
                        };
                        let failed = run_crossing(
                            activation,
                            handoff,
                            entry_position,
                            reduce_wifi_latency,
                            stop,
                            status.clone(),
                        )
                        .await;
                        // Input is back on the Mac. Let a peer take control
                        // before the observer hears the crossing ended.
                        drop(guard);
                        drop(status);
                        if failed {
                            // The receiver may have changed; check it before the next crossing.
                            ready = false;
                            snapshot = snapshot.or_else(|| self.ask(inbound));
                        }
                    }
                },
                response = answered(&mut snapshot) => {
                    snapshot = None;
                    ready = show(state, response);
                }
                event = self.events.recv() => match event.map(|event| event.kind) {
                    Some(SessionEventKind::Closed { reason }) => return Served::Lost(reason),
                    Some(SessionEventKind::Desktop { request, reply }) => {
                        inbound.desktop(request, reply);
                    }
                    Some(SessionEventKind::ReceiverEffects {
                        effects,
                        received_at,
                        applied,
                        ..
                    }) => {
                        let _ = applied.send(inbound.effects(effects, received_at).await);
                    }
                    Some(SessionEventKind::Layout { layout }) => layouts.take(&self.handle, layout),
                    Some(SessionEventKind::Clipboard { clip }) => {
                        inbound.clipboard().keep(self.handle.peer(), clip);
                    }
                    Some(SessionEventKind::OutboundEnded) => {}
                    None => return Served::Lost("input session closed".into()),
                },
                Ok(()) = layouts.kept.changed() => layouts.offer(&self.handle),
                () = tokio::time::sleep(retry), if !ready && snapshot.is_none() && self.sends => {
                    // Nothing is asked while the peer controls this Mac, so
                    // the next try comes as soon as it lets go.
                    snapshot = self.ask(inbound);
                    if snapshot.is_some() {
                        retry = (retry * 2).min(SNAPSHOT_RETRY);
                    }
                }
            }
        }
    }

    /// Asks for the receiver's desktop, unless it takes no input from this
    /// Mac, or controls it and so refuses while it sends. Dropping the
    /// answer before it comes closes the session.
    fn ask(&self, inbound: &Inbound) -> Option<Snapshot> {
        if !self.sends || inbound.controls() {
            return None;
        }
        let handle = self.handle.clone();
        Some(Box::pin(async move {
            handle.desktop_request(DesktopRequest::Snapshot).await
        }))
    }

    /// Closes the session and waits briefly for the close to leave.
    async fn close(mut self, reason: SessionCloseReason) {
        self.handle.close(reason);
        let closed = async {
            match &self.endpoint {
                Some(endpoint) => {
                    endpoint.close(0_u32.into(), b"sharing stopped");
                    endpoint.wait_idle().await;
                }
                // The peer's connection lives on the listener's endpoint.
                None => {
                    while let Some(event) = self.events.recv().await {
                        if matches!(event.kind, SessionEventKind::Closed { .. }) {
                            break;
                        }
                    }
                }
            }
        };
        let _ = tokio::time::timeout(CLOSE_GRACE, closed).await;
    }
}

/// A desktop snapshot on its way back from the receiver.
type Snapshot = Pin<Box<dyn Future<Output = Result<DesktopResponse>> + Send>>;

/// The answer to the snapshot in flight. Never ready without one.
async fn answered(snapshot: &mut Option<Snapshot>) -> Result<DesktopResponse> {
    match snapshot {
        Some(snapshot) => snapshot.await,
        None => std::future::pending().await,
    }
}

/// Puts the receiver's desktop into the link state. True if it can be
/// crossed into.
fn show(state: &watch::Sender<LinkState>, response: Result<DesktopResponse>) -> bool {
    let response = response.and_then(|response| {
        response.validate()?;
        Ok(response)
    });
    let (ready, shown) = match response {
        Ok(DesktopResponse::Snapshot { geometry, .. }) => (true, LinkState::Ready(geometry)),
        // The session works, so the peer can still control this Mac.
        Ok(DesktopResponse::Unavailable { reason }) => (false, LinkState::Refused(reason)),
        Ok(_) => (
            false,
            LinkState::Down("Unexpected response from the other computer".into()),
        ),
        Err(error) => (false, LinkState::Down(format!("{error:#}"))),
    };
    state.send_replace(shown);
    ready
}

/// Races the peer's addresses of the endpoint's family within one timeout.
/// SPKI pinning rejects every host that is not this peer, so unverified
/// addresses are safe to try.
async fn connect_peer(
    endpoint: &Endpoint,
    client: &InputClientConfig,
    addresses: &[SocketAddr],
) -> Result<(SocketAddr, InputConnection)> {
    let ipv4 = endpoint.local_addr()?.is_ipv4();
    let mut tried = BTreeSet::new();
    let mut attempts = tokio::task::JoinSet::new();
    for &address in addresses.iter().filter(|address| address.is_ipv4() == ipv4) {
        if !tried.insert(address) {
            continue;
        }
        let (endpoint, client) = (endpoint.clone(), client.clone());
        attempts.spawn(async move {
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
                // Only the peer's own key can say it has not added this
                // Mac, so that says more than another address failing.
                Err(error) if Fix::of(&failure) == Some(Fix::WaitingForThem) => {
                    tracing::debug!(error = %format!("{error:#}"), "input address failed");
                }
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
        clipboard::{Clip, ClipKind},
        config::PeerPermissions,
        core::{HidUsage, ReceiverEffect},
        desktop::{Edge, FRACTION_MAX, Point, Rect},
        macos::{
            clipboard::FakePasteboard,
            inject::FakeBackend,
            receive::{self, Ownership},
        },
        transport::{accept_input, input_server_config},
    };
    use tokio::sync::oneshot;

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
        layouts: mpsc::UnboundedReceiver<SharedLayout>,
        clips: mpsc::UnboundedReceiver<Clip>,
        /// While set, desktop requests wait in `held` for the test to answer.
        hold: Arc<std::sync::atomic::AtomicBool>,
        held: mpsc::UnboundedReceiver<(DesktopRequest, oneshot::Sender<DesktopResponse>)>,
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
        let (layouts_tx, layouts) = mpsc::unbounded_channel();
        let (clips_tx, clips) = mpsc::unbounded_channel();
        let hold = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (held_tx, held) = mpsc::unbounded_channel();
        let holding = hold.clone();
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
                let (effects_tx, closed_tx, layouts_tx, clips_tx, held_tx) = (
                    effects_tx.clone(),
                    closed_tx.clone(),
                    layouts_tx.clone(),
                    clips_tx.clone(),
                    held_tx.clone(),
                );
                let holding = holding.clone();
                tokio::spawn(async move {
                    while let Some(event) = events.recv().await {
                        match event.kind {
                            SessionEventKind::Desktop { request, reply }
                                if holding.load(std::sync::atomic::Ordering::SeqCst) =>
                            {
                                let _ = held_tx.send((request, reply));
                            }
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
                            SessionEventKind::Layout { layout } => {
                                let _ = layouts_tx.send(layout);
                            }
                            SessionEventKind::Clipboard { clip } => {
                                let _ = clips_tx.send(clip);
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
            layouts,
            clips,
            hold,
            held,
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
        config.transport.listen = "127.0.0.1:0".parse().unwrap();
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

    fn captured(frame: CaptureFrame) -> CapturedDeviceFrame {
        CapturedDeviceFrame {
            device_path: "test".into(),
            captured_at: Instant::now(),
            frame,
        }
    }

    fn key(state: KeyState) -> CapturedDeviceFrame {
        captured(CaptureFrame {
            transitions: vec![CaptureTransition::Key {
                usage: HidUsage::keyboard(4),
                state,
            }],
            ..CaptureFrame::default()
        })
    }

    fn button(state: KeyState) -> CapturedDeviceFrame {
        captured(CaptureFrame {
            transitions: vec![CaptureTransition::Button {
                button: crate::core::PointerButton(1),
                state,
            }],
            ..CaptureFrame::default()
        })
    }

    fn motion(dx: i64, scroll_y: i64) -> CapturedDeviceFrame {
        captured(CaptureFrame {
            motion: crate::core::MotionDelta {
                dx,
                scroll_y,
                ..Default::default()
            },
            event_count: 1,
            ..CaptureFrame::default()
        })
    }

    fn left_alt(state: KeyState) -> CapturedDeviceFrame {
        captured(CaptureFrame {
            transitions: vec![CaptureTransition::Key {
                usage: HidUsage::keyboard(0xe2),
                state,
            }],
            ..CaptureFrame::default()
        })
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
        let mut session = Session::open(
            "linux",
            &config.peers["linux"],
            &config,
            &[receiver.address],
        )
        .await
        .unwrap();
        let state = watch::channel(LinkState::Connecting).0;
        let response = session
            .handle
            .desktop_request(DesktopRequest::Snapshot)
            .await;
        assert!(show(&state, response));
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
            receive::answer_waiting_events(&mut session.events, drop, drop).unwrap();
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
        session.close(SessionCloseReason::LocalRelease).await;
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
            let session = Session::open(
                "linux",
                &config.peers["linux"],
                &config,
                &[receiver.address],
            )
            .await
            .unwrap();
            assert_eq!(session.raw_touch, touch);
            session.close(SessionCloseReason::LocalRelease).await;
        }
    }

    const ENTRY: CursorPosition = CursorPosition { x: 0.0, y: 50.0 };

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

    fn handoff(peer: &str) -> Handoff {
        Handoff {
            peer: peer.into(),
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
        let mut links = Links::with_backend(FakeBackend::default()).unwrap();
        assert!(links.cross(handoff("linux"), ENTRY, false).is_none());
        links.sync(&config, true, false);
        assert!(links.changed());
        wait_until(&mut links, ready);
        let first = server.block_on(receiver.sessions.recv()).unwrap();

        // Radio, clipboard and edge settings leave the session alone.
        config.macos.block_awdl = true;
        config.clipboard.share = true;
        config.switching.pause_at_edges = true;
        let task = links.links["linux"].task.id();
        links.sync(&config, true, false);
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
        assert!(links.cross(handoff("other"), ENTRY, false).is_none());

        // While a peer controls this Mac, nothing crosses.
        let claim = links
            .receiving
            .ownership()
            .claim_inbound("linux", 1)
            .unwrap();
        assert_eq!(links.controller().as_deref(), Some("linux"));
        assert!(links.cross(handoff("linux"), ENTRY, false).is_none());
        drop(claim);
        assert_eq!(links.controller(), None);
        assert!(
            links.receiving.ownership().begin_outbound().is_some(),
            "no crossing kept input"
        );

        // Pausing closes the link, and that reaches the receiver now, not
        // at its idle timeout.
        links.sync(&config, false, false);
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

    /// A fake Linux computer on its own runtime, and this Mac's links to it,
    /// ready, posting into `fake` and sharing `pasteboard`.
    struct Pair {
        links: Links,
        receiver: Receiver,
        config: Config,
        fake: FakeBackend,
        pasteboard: FakePasteboard,
        server: tokio::runtime::Runtime,
        directories: [tempfile::TempDir; 2],
    }

    fn pair(record: impl FnOnce(&mut PeerConfig)) -> Pair {
        let server = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let directories = [(); 2].map(|_| tempfile::tempdir().unwrap());
        let [mac, linux] = [0, 1].map(|index| directories[index].path());
        let spki = Identity::load_or_create(mac).unwrap().spki().to_vec();
        let receiver = server.block_on(receiver(linux, &spki, false));
        let mut config = mac_config(mac, linux, receiver.address);
        let peer = config.peers.get_mut("linux").unwrap();
        peer.permissions.send_normal = true;
        record(peer);
        let fake = FakeBackend::default();
        let pasteboard = FakePasteboard::default();
        let mut links = Links::with_fakes(fake.clone(), pasteboard.clone()).unwrap();
        links.set_receive_policy(true, false);
        links.sync(&config, true, false);
        wait_until(&mut links, |links| {
            links
                .states()
                .any(|(_, state)| matches!(state, LinkState::Ready(_) | LinkState::Connected))
        });
        Pair {
            links,
            receiver,
            config,
            fake,
            pasteboard,
            server,
            directories,
        }
    }

    /// The first activation of the fake computer's session.
    fn activation(session: &SessionHandle) -> SessionContext {
        activation_number(session, 1)
    }

    fn activation_number(session: &SessionHandle, id: u64) -> SessionContext {
        SessionContext {
            session_epoch: SessionEpoch([9; 16]),
            transport_generation: session.generation(),
            activation_id: ActivationId(id),
        }
    }

    /// Waits until the Mac posted `line`, and returns what it posted so far.
    fn posted(fake: &FakeBackend, line: &str, within: Duration) -> Vec<String> {
        let deadline = Instant::now() + within;
        loop {
            let log = fake.state().log.clone();
            if log.iter().any(|posted| posted == line) {
                return log;
            }
            assert!(Instant::now() < deadline, "never posted {line}: {log:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Waits until the Mac closes the fake computer's session `id`.
    fn closed(server: &tokio::runtime::Runtime, receiver: &mut Receiver, id: u64) {
        server.block_on(async {
            loop {
                let closed = tokio::time::timeout(Duration::from_secs(2), receiver.closed.recv())
                    .await
                    .expect("the Mac closed the session")
                    .unwrap();
                if closed == id {
                    break;
                }
            }
        });
    }

    #[test]
    fn a_peer_controls_this_mac_over_the_session_this_mac_dialed() {
        let Pair {
            links,
            mut receiver,
            fake,
            server,
            directories: _directories,
            ..
        } = pair(|peer| {
            peer.keyboard = crate::core::KeyboardMode::PcPositions;
            peer.reverse_scroll = true;
        });
        let linux = server.block_on(receiver.sessions.recv()).unwrap();
        let ask = |request| server.block_on(linux.desktop_request(request)).unwrap();
        assert_eq!(
            ask(DesktopRequest::Snapshot),
            DesktopResponse::Snapshot {
                geometry: geometry(),
                position: Point { x: 960, y: 540 },
            }
        );
        let token = 7;
        let prepare = DesktopRequest::Prepare {
            token,
            edge: Edge::Left,
            start: 0,
            end: FRACTION_MAX,
            position: 500_000,
        };
        assert_eq!(
            ask(prepare),
            DesktopResponse::Prepared {
                geometry: geometry(),
                position: Point { x: 3, y: 540 },
            }
        );
        assert_eq!(links.controller().as_deref(), Some("linux"));

        linux.begin_outbound(activation(&linux)).unwrap();
        for frame in [
            key(KeyState::Pressed),
            key(KeyState::Released),
            left_alt(KeyState::Pressed),
            left_alt(KeyState::Released),
            motion(0, 120),
            button(KeyState::Pressed),
            button(KeyState::Released),
        ] {
            linux.capture(frame).unwrap();
        }
        let log = posted(
            &fake,
            "button 0 up at 3,540 click 1",
            Duration::from_secs(2),
        );
        assert_eq!(
            log[0], "move 3,540 by -957,0",
            "Prepare put the cursor at the entry"
        );
        for line in ["key 0 down", "key 0 up", "button 0 down at 3,540 click 1"] {
            assert!(log.iter().any(|posted| posted == line), "{line}: {log:?}");
        }
        assert!(
            log.iter().any(|line| line.starts_with("modifier 55 down")),
            "the peer's Alt is Cmd in PC positions: {log:?}"
        );
        assert!(
            log.iter().any(|line| line == "scroll 0,-30"),
            "the peer's scrolling is turned around: {log:?}"
        );

        // The pointer leaves through the edge it came in by.
        linux.capture(motion(-100, 0)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let returned = loop {
            match ask(DesktopRequest::Poll { token }) {
                DesktopResponse::Active => {
                    assert!(Instant::now() < deadline, "the pointer never left");
                }
                response => break response,
            }
        };
        assert_eq!(returned, DesktopResponse::Returned { position: 500_000 });
        // Motion after that stays off the Mac.
        fake.take_log();
        for frame in [
            motion(50, 0),
            button(KeyState::Pressed),
            button(KeyState::Released),
        ] {
            linux.capture(frame).unwrap();
        }
        let log = posted(
            &fake,
            "button 0 down at 3,540 click 2",
            Duration::from_secs(2),
        )
        .into_iter()
        .chain(fake.take_log())
        .collect::<Vec<_>>();
        assert!(log.iter().all(|line| !line.starts_with("move")), "{log:?}");

        server
            .block_on(linux.end_outbound(SessionCloseReason::LocalRelease))
            .unwrap();
        assert_eq!(
            ask(DesktopRequest::Finish { token }),
            DesktopResponse::Finished
        );
        assert_eq!(links.controller(), None);
        assert!(links.receiving.ownership().begin_outbound().is_some());
        drop(links);
    }

    /// The fake computer takes control of this Mac with activation `id`,
    /// types a key, and lets go.
    fn control_once(pair: &mut Pair, linux: &SessionHandle, id: u64) {
        linux.begin_outbound(activation_number(linux, id)).unwrap();
        linux.capture(key(KeyState::Pressed)).unwrap();
        linux.capture(key(KeyState::Released)).unwrap();
        posted(&pair.fake, "key 0 up", Duration::from_secs(2));
        pair.fake.take_log();
        pair.server
            .block_on(linux.end_outbound(SessionCloseReason::LocalRelease))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while pair.links.controller().is_some() {
            assert!(Instant::now() < deadline, "the peer kept control");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The next clip the fake computer gets within `within`, if one comes.
    fn clip(pair: &mut Pair, within: Duration) -> Option<Clip> {
        let clips = &mut pair.receiver.clips;
        pair.server
            .block_on(async { tokio::time::timeout(within, clips.recv()).await })
            .ok()
            .flatten()
    }

    #[test]
    fn the_clipboard_follows_the_pointer_back_to_a_peer_and_never_bounces() {
        let mut pair = pair(|_| {});
        let linux = pair.server.block_on(pair.receiver.sessions.recv()).unwrap();
        pair.pasteboard.copy(ClipKind::Text, b"copied on the mac");

        // Sharing is off, so the pasteboard is not even read.
        control_once(&mut pair, &linux, 1);
        assert!(clip(&mut pair, Duration::from_millis(200)).is_none());
        assert_eq!(pair.pasteboard.board().reads, 0);

        pair.config.clipboard.share = true;
        pair.links.sync(&pair.config, true, false);
        control_once(&mut pair, &linux, 2);
        let sent = clip(&mut pair, Duration::from_secs(2)).expect("the clipboard went along");
        assert_eq!(sent.kind(), ClipKind::Text);
        assert_eq!(sent.data(), b"copied on the mac");

        // A clip from the peer lands on the pasteboard, and does not go back.
        let theirs = Clip::new(ClipKind::Text, b"copied on linux".to_vec()).unwrap();
        pair.server
            .block_on(async { linux.send_clipboard(theirs.clone()) });
        let deadline = Instant::now() + Duration::from_secs(2);
        while pair.pasteboard.board().writes.is_empty() {
            assert!(Instant::now() < deadline, "the clip was not kept");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(pair.pasteboard.board().writes, [theirs]);
        let reads = pair.pasteboard.board().reads;
        control_once(&mut pair, &linux, 3);
        assert!(clip(&mut pair, Duration::from_millis(300)).is_none());
        assert_eq!(pair.pasteboard.board().reads, reads + 1);
        drop(pair.links);
    }

    #[test]
    fn keys_a_vanished_peer_held_are_let_go_within_its_lease() {
        let Pair {
            links,
            mut receiver,
            fake,
            server,
            directories: _directories,
            ..
        } = pair(|_| {});
        let linux = server.block_on(receiver.sessions.recv()).unwrap();
        linux.begin_outbound(activation(&linux)).unwrap();
        linux.capture(key(KeyState::Pressed)).unwrap();
        posted(&fake, "key 0 down", Duration::from_secs(2));
        assert_eq!(links.controller().as_deref(), Some("linux"));
        assert_eq!(fake.state().wakes, 1, "taking control wakes the display");
        // The peer stops without a word, as when its process is killed.
        let vanished = Instant::now();
        drop(linux);
        server.shutdown_background();
        posted(&fake, "key 0 up", Duration::from_millis(1200));
        assert!(vanished.elapsed() < Duration::from_millis(1200));
        let deadline = Instant::now() + Duration::from_secs(2);
        while links.controller().is_some() {
            assert!(Instant::now() < deadline, "the peer kept control");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(links);
        drop(receiver);
    }

    /// The next desktop request the fake computer holds, if one comes.
    fn held(
        server: &tokio::runtime::Runtime,
        receiver: &mut Receiver,
        within: Duration,
    ) -> Option<(DesktopRequest, oneshot::Sender<DesktopResponse>)> {
        server
            .block_on(async { tokio::time::timeout(within, receiver.held.recv()).await })
            .ok()
            .flatten()
    }

    #[test]
    fn the_peers_input_does_not_wait_for_its_desktop() {
        let Pair {
            mut links,
            mut receiver,
            fake,
            server,
            directories: _directories,
            ..
        } = pair(|_| {});
        let linux = server.block_on(receiver.sessions.recv()).unwrap();
        receiver
            .hold
            .store(true, std::sync::atomic::Ordering::SeqCst);
        links.retry();
        let (request, reply) = held(&server, &mut receiver, Duration::from_secs(2))
            .expect("the Mac asked for the desktop");
        assert!(matches!(request, DesktopRequest::Snapshot));
        // The peer takes control before it answers, as when it dials and
        // activates at once. The Mac gives up on an answer after 1 s.
        linux.begin_outbound(activation(&linux)).unwrap();
        linux.capture(key(KeyState::Pressed)).unwrap();
        posted(&fake, "key 0 down", Duration::from_millis(800));
        let smaller = Geometry {
            monitors: vec![Rect {
                x: 0,
                y: 0,
                width: 1280,
                height: 720,
            }],
        };
        let _ = reply.send(DesktopResponse::Snapshot {
            geometry: smaller.clone(),
            position: Point { x: 0, y: 0 },
        });
        wait_until(&mut links, |links| {
            links
                .states()
                .any(|(_, state)| state == LinkState::Ready(smaller.clone()))
        });

        // The peer refuses snapshots while it sends, so none is asked.
        links.retry();
        assert!(
            held(&server, &mut receiver, Duration::from_millis(300)).is_none(),
            "asked while controlled"
        );
        linux.capture(key(KeyState::Released)).unwrap();
        server
            .block_on(linux.end_outbound(SessionCloseReason::LocalRelease))
            .unwrap();
        let (request, reply) = held(&server, &mut receiver, Duration::from_secs(2))
            .expect("asked again once the peer let go");
        let _ = reply.send(answer(request));
        wait_until(&mut links, ready);
        assert!(!linux.is_closed());
        assert!(receiver.closed.try_recv().is_err());
        drop(links);
    }

    #[test]
    fn a_peer_that_takes_no_input_from_this_mac_stays_connected() {
        let Pair {
            mut links,
            mut receiver,
            server,
            directories: _directories,
            ..
        } = pair(|_| {});
        let linux = server.block_on(receiver.sessions.recv()).unwrap();
        receiver
            .hold
            .store(true, std::sync::atomic::Ordering::SeqCst);
        links.retry();
        let (_, reply) = held(&server, &mut receiver, Duration::from_secs(2))
            .expect("the Mac asked for the desktop");
        receiver
            .hold
            .store(false, std::sync::atomic::Ordering::SeqCst);
        // Linux's answer when its switch for this Mac is off, or it is locked.
        let reason = "Desktop control requires the active unlocked local session and an authorized paired peer";
        let _ = reply.send(DesktopResponse::unavailable(reason));
        wait_until(&mut links, |links| {
            links
                .states()
                .any(|(_, state)| state == LinkState::Refused(reason.into()))
        });
        // The link keeps asking, and crosses again once the peer takes input.
        wait_until(&mut links, ready);
        assert!(!linux.is_closed());
        drop(links);
    }

    #[test]
    fn locking_this_mac_ends_control() {
        let Pair {
            links,
            mut receiver,
            fake,
            server,
            directories: _directories,
            ..
        } = pair(|_| {});
        let linux = server.block_on(receiver.sessions.recv()).unwrap();
        linux.begin_outbound(activation(&linux)).unwrap();
        linux.capture(left_alt(KeyState::Pressed)).unwrap();
        posted(
            &fake,
            "modifier 58 down flags 0x80020",
            Duration::from_secs(2),
        );
        // The peer holds Option and sends nothing more, so no batch finds
        // the lock.
        fake.state().locked = true;
        posted(&fake, "modifier 58 up", Duration::from_secs(1));
        closed(&server, &mut receiver, linux.id());
        let deadline = Instant::now() + Duration::from_secs(2);
        while links.controller().is_some() {
            assert!(Instant::now() < deadline, "the peer kept control");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(links);
    }

    #[test]
    fn a_peer_may_not_control_this_mac_without_leave_or_while_it_sends() {
        // Not allowed to control this Mac.
        let Pair {
            links,
            mut receiver,
            fake,
            server,
            directories: _directories,
            ..
        } = pair(|peer| peer.permissions.send_normal = false);
        let linux = server.block_on(receiver.sessions.recv()).unwrap();
        let snapshot = server.block_on(linux.desktop_request(DesktopRequest::Snapshot));
        assert!(matches!(
            snapshot.unwrap(),
            DesktopResponse::Unavailable { .. }
        ));
        linux.begin_outbound(activation(&linux)).unwrap();
        linux.capture(key(KeyState::Pressed)).unwrap();
        closed(&server, &mut receiver, linux.id());
        assert!(fake.take_log().iter().all(|line| line != "key 0 down"));
        assert_eq!(links.controller(), None);
        drop(links);

        // This Mac is sending its own input.
        let Pair {
            links,
            mut receiver,
            fake,
            server,
            directories: _directories,
            ..
        } = pair(|_| {});
        let sending = links.receiving.ownership().begin_outbound().unwrap();
        let linux = server.block_on(receiver.sessions.recv()).unwrap();
        linux.begin_outbound(activation(&linux)).unwrap();
        linux.capture(key(KeyState::Pressed)).unwrap();
        closed(&server, &mut receiver, linux.id());
        assert!(fake.take_log().iter().all(|line| line != "key 0 down"));
        drop(sending);
        drop(links);

        // Control taken away while the peer holds a key.
        let Pair {
            mut links,
            mut receiver,
            mut config,
            fake,
            server,
            directories: _directories,
            ..
        } = pair(|_| {});
        let linux = server.block_on(receiver.sessions.recv()).unwrap();
        linux.begin_outbound(activation(&linux)).unwrap();
        linux.capture(key(KeyState::Pressed)).unwrap();
        posted(&fake, "key 0 down", Duration::from_secs(2));
        let task = links.links["linux"].task.id();
        config
            .peers
            .get_mut("linux")
            .unwrap()
            .permissions
            .send_normal = false;
        links.sync(&config, true, false);
        assert_eq!(links.links["linux"].task.id(), task, "the link stays");
        posted(&fake, "key 0 up", Duration::from_secs(1));
        closed(&server, &mut receiver, linux.id());
        assert_eq!(links.controller(), None);
        drop(links);
    }

    #[test]
    fn every_session_gets_the_kept_layout_and_a_newer_one_comes_back() {
        let key = format!("{:064x}", 1);
        let layout = |version| SharedLayout {
            version,
            editor: key.clone(),
            tiles: vec![crate::desktop::Tile {
                key: key.clone(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }],
        };
        let Pair {
            mut links,
            mut receiver,
            server,
            directories: _directories,
            ..
        } = pair(|_| {});
        let next_layout = |receiver: &mut Receiver| {
            let layout = server.block_on(async {
                tokio::time::timeout(Duration::from_secs(2), receiver.layouts.recv()).await
            });
            layout.expect("the Mac sent its layout").unwrap()
        };
        links.share_layout(layout(2));
        assert_eq!(next_layout(&mut receiver), layout(2));

        // A newer one goes to the app, and once kept it is not sent back.
        let linux = server.block_on(receiver.sessions.recv()).unwrap();
        linux.send_layout(layout(3)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let taken = loop {
            let taken = links.take_layouts();
            if !taken.is_empty() {
                break taken;
            }
            assert!(Instant::now() < deadline, "the newer layout never came");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(taken, [("linux".to_owned(), layout(3))]);
        links.share_layout(layout(3));
        std::thread::sleep(Duration::from_millis(200));
        assert!(receiver.layouts.try_recv().is_err(), "no echo");

        // An older one gets the kept one back.
        linux.send_layout(layout(1)).unwrap();
        assert_eq!(next_layout(&mut receiver), layout(3));
        assert!(links.take_layouts().is_empty());

        // So does the next session.
        linux.close(SessionCloseReason::BackendUnavailable);
        assert_eq!(next_layout(&mut receiver), layout(3));
        drop(links);
    }

    #[test]
    fn a_peer_that_only_controls_this_mac_is_connected_without_a_desktop() {
        let Pair {
            mut links,
            mut receiver,
            mut config,
            server,
            directories: _directories,
            ..
        } = pair(|peer| peer.permissions.receive_normal = false);
        assert!(
            links
                .states()
                .any(|(name, state)| name == "linux" && state == LinkState::Connected)
        );
        assert!(links.cross(handoff("linux"), ENTRY, false).is_none());
        let first = server.block_on(receiver.sessions.recv()).unwrap();

        // Whether it may control this Mac, and how, keeps the session.
        let task = links.links["linux"].task.id();
        let peer = config.peers.get_mut("linux").unwrap();
        peer.permissions.send_normal = false;
        peer.keyboard = crate::core::KeyboardMode::Mac;
        peer.reverse_scroll = true;
        links.sync(&config, true, false);
        assert_eq!(links.links["linux"].task.id(), task);

        // Taking input from this Mac needs the desktop, so it reconnects.
        config
            .peers
            .get_mut("linux")
            .unwrap()
            .permissions
            .receive_normal = true;
        links.sync(&config, true, false);
        assert_ne!(links.links["linux"].task.id(), task);
        wait_until(&mut links, ready);
        let second = server.block_on(receiver.sessions.recv()).unwrap();
        assert_ne!(first.id(), second.id());
        drop(links);
    }

    /// Key directories for this Mac and a fake computer, where this Mac's
    /// dial `wins` when both dial at once.
    fn identities(wins: bool) -> (tempfile::TempDir, tempfile::TempDir) {
        let mac = tempfile::tempdir().unwrap();
        let local = Identity::load_or_create(mac.path())
            .unwrap()
            .fingerprint_hex();
        loop {
            let linux = tempfile::tempdir().unwrap();
            let peer = Identity::load_or_create(linux.path())
                .unwrap()
                .fingerprint_hex();
            if wins_simultaneous_dial(&local, &peer) == wins {
                return (mac, linux);
            }
        }
    }

    /// The fake computer dials this Mac, as the Linux daemon does. The
    /// channel says "snapshot" for each desktop the Mac reads through the
    /// session, and "closed" once it closes.
    async fn dial_mac(
        linux: &std::path::Path,
        mac: &[u8],
        address: SocketAddr,
    ) -> (
        SessionHandle,
        mpsc::UnboundedReceiver<&'static str>,
        Endpoint,
    ) {
        let identity = Identity::load_or_create(linux).unwrap();
        let client = input_client_config(&identity, mac).unwrap();
        let endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let connection = connect_input(&endpoint, address, &client).await.unwrap();
        let options = SessionOptions::from_config(&Config::default()).unwrap();
        let (sender, mut events) = mpsc::channel(64);
        let session = start_session(
            connection,
            "mac".into(),
            TransportGeneration(1),
            options,
            sender,
        )
        .await
        .unwrap();
        let (seen, seen_events) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                match event.kind {
                    SessionEventKind::Desktop { request, reply } => {
                        let _ = reply.send(answer(request));
                        let _ = seen.send("snapshot");
                    }
                    SessionEventKind::Closed { .. } => break,
                    _ => {}
                }
            }
            let _ = seen.send("closed");
        });
        (session, seen_events, endpoint)
    }

    fn listening(links: &Links) -> SocketAddr {
        links
            .listener
            .as_ref()
            .unwrap()
            .endpoint
            .local_addr()
            .unwrap()
    }

    fn next(server: &tokio::runtime::Runtime, seen: &mut mpsc::UnboundedReceiver<&str>) -> String {
        server
            .block_on(async { tokio::time::timeout(Duration::from_secs(2), seen.recv()).await })
            .expect("the Mac answered")
            .unwrap()
            .to_owned()
    }

    #[test]
    fn computers_that_dial_each_other_at_once_keep_one_connection() {
        for wins in [true, false] {
            let server = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let (mac, linux) = identities(wins);
            let spki = Identity::load_or_create(mac.path())
                .unwrap()
                .spki()
                .to_vec();
            let mut receiver = server.block_on(receiver(linux.path(), &spki, false));
            let config = mac_config(mac.path(), linux.path(), receiver.address);
            let mut links = Links::with_backend(FakeBackend::default()).unwrap();
            links.sync(&config, true, false);
            wait_until(&mut links, ready);
            let ours = server.block_on(receiver.sessions.recv()).unwrap();
            let (theirs, mut seen, _endpoint) =
                server.block_on(dial_mac(linux.path(), &spki, listening(&links)));
            if wins {
                assert_eq!(
                    next(&server, &mut seen),
                    "closed",
                    "the Mac kept its own dial"
                );
                assert!(!ours.is_closed());
            } else {
                closed(&server, &mut receiver, ours.id());
                assert_eq!(next(&server, &mut seen), "snapshot", "the Mac uses theirs");
                assert!(!theirs.is_closed());
            }
            wait_until(&mut links, ready);
            drop(links);
        }
    }

    #[test]
    fn a_computer_that_connects_later_replaces_the_connection() {
        let server = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        // Even the dial that would win at once gives way once it is older.
        let (mac, linux) = identities(true);
        let spki = Identity::load_or_create(mac.path())
            .unwrap()
            .spki()
            .to_vec();
        let mut receiver = server.block_on(receiver(linux.path(), &spki, false));
        let mut config = mac_config(mac.path(), linux.path(), receiver.address);
        let mut links = Links::with_backend(FakeBackend::default()).unwrap();
        links.dial_window = Duration::ZERO;
        links.sync(&config, true, false);
        wait_until(&mut links, ready);
        let ours = server.block_on(receiver.sessions.recv()).unwrap();
        let (theirs, mut seen, _endpoint) =
            server.block_on(dial_mac(linux.path(), &spki, listening(&links)));
        closed(&server, &mut receiver, ours.id());
        assert_eq!(next(&server, &mut seen), "snapshot");
        assert!(!theirs.is_closed());
        drop(links);

        // A computer this Mac has no address for connects on its own, and
        // the app hears where from.
        config.peers.get_mut("linux").unwrap().addresses.clear();
        let mut links = Links::with_backend(FakeBackend::default()).unwrap();
        links.sync(&config, true, false);
        wait_until(&mut links, |links| {
            links
                .states()
                .any(|(_, state)| matches!(state, LinkState::Down(_)))
        });
        let (theirs, mut seen, endpoint) =
            server.block_on(dial_mac(linux.path(), &spki, listening(&links)));
        assert_eq!(next(&server, &mut seen), "snapshot");
        wait_until(&mut links, ready);
        assert!(!theirs.is_closed());
        let Heard::Connected {
            key,
            address,
            dialed,
        } = heard(&mut links)
        else {
            panic!("expected the session's address");
        };
        assert_eq!(key, config.peers["linux"].spki_der_hex);
        assert_eq!((address, dialed), (endpoint.local_addr().unwrap(), false));
        drop(links);
    }

    #[test]
    fn a_link_dials_where_its_key_was_found_and_learning_an_address_keeps_it() {
        let server = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (mac, linux) = identities(true);
        let spki = Identity::load_or_create(mac.path())
            .unwrap()
            .spki()
            .to_vec();
        let receiver = server.block_on(receiver(linux.path(), &spki, false));
        // Nothing saved, as for a computer placed before it ever answered.
        let mut config = mac_config(mac.path(), linux.path(), receiver.address);
        let peer = config.peers.get_mut("linux").unwrap();
        peer.addresses.clear();
        let key = peer.spki_der_hex.clone();
        let mut links = Links::with_backend(FakeBackend::default()).unwrap();
        links.sync(&config, true, false);
        wait_until(&mut links, |links| {
            links
                .states()
                .any(|(_, state)| matches!(state, LinkState::Down(_)))
        });
        links.set_found(Found::from([(key.clone(), vec![receiver.address])]));
        wait_until(&mut links, ready);
        // The app hears the address that answered, to save it.
        let Heard::Connected {
            key: connected,
            address,
            dialed,
        } = heard(&mut links)
        else {
            panic!("expected the session's address");
        };
        assert_eq!((connected, address, dialed), (key, receiver.address, true));

        // Saving where it was found does not reconnect.
        let task = links.links["linux"].task.id();
        let peer = config.peers.get_mut("linux").unwrap();
        assert!(peer.learn_addresses([receiver.address]));
        links.sync(&config, true, false);
        assert_eq!(links.links["linux"].task.id(), task);
        drop(links);
    }

    /// Waits for the next thing heard.
    fn heard(links: &mut Links) -> Heard {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(heard) = links.take_heard().into_iter().next() {
                return heard;
            }
            assert!(Instant::now() < deadline, "nothing was heard");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn computers_that_do_not_trust_each_other_trade_hellos_on_the_input_port() {
        use crate::{
            hello::make_hello,
            transport::{TransportError, hello_client_config},
        };
        let server = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (mac, linux) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mac_key = Identity::load_or_create(mac.path()).unwrap();
        let linux_key = Arc::new(Identity::load_or_create(linux.path()).unwrap());
        let mut config = Config::default();
        config.daemon.state_dir = mac.path().to_owned();
        config.transport.listen = "127.0.0.1:0".parse().unwrap();
        let mut links = Links::with_backend(FakeBackend::default()).unwrap();
        // Nobody is trusted, so only a welcome listens.
        links.sync(&config, true, false);
        assert!(links.listener.is_none());
        links.sync(&config, true, true);
        let address = listening(&links);
        let greeting = Greeting {
            client: hello_client_config(&mac_key).unwrap(),
            port: address.port(),
            trusted: Arc::default(),
        };

        // A computer knocks, and the app answers.
        let knocking = linux_key.clone();
        let theirs = server.spawn(async move {
            let endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let client = hello_client_config(&knocking).unwrap();
            let connection = connect_hello(&endpoint, address, &client).await.unwrap();
            let hello = make_hello("linux", 43119, Vec::new(), false);
            connection.exchange(&hello).await.unwrap()
        });
        let Heard::Knock(knock) = heard(&mut links) else {
            panic!("expected a knock");
        };
        links.answer(knock, greeting.clone());
        let Heard::Hello {
            instance,
            spki,
            hello,
            ..
        } = heard(&mut links)
        else {
            panic!("expected its hello");
        };
        assert_eq!((instance, spki.as_slice()), (None, linux_key.spki()));
        assert_eq!(hello.name, "linux");
        let ours = server.block_on(theirs).unwrap();
        assert_eq!(ours.input_port, address.port());
        assert!(!ours.trusts_you);
        assert!(ours.candidates.is_empty(), "a stranger hears no addresses");

        // Input from a key not trusted here is refused. That it asked says
        // nothing anyone could not claim, so the app hears nothing.
        let refused = server.block_on(async {
            let endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let client = input_client_config(&linux_key, mac_key.spki()).unwrap();
            match connect_input(&endpoint, address, &client).await {
                Ok(connection) => {
                    let closed = tokio::time::timeout(Duration::from_secs(2), connection.closed());
                    TransportError::from(closed.await.expect("the Mac closed the input"))
                }
                Err(error) => error,
            }
        });
        assert!(matches!(refused, TransportError::NotTrusted), "{refused}");
        assert!(links.take_heard().is_empty(), "nothing was heard");

        // The Mac says hello to a record, and hears back from it.
        let answering = linux_key.clone();
        let endpoint = server.block_on(async move {
            let config =
                input_server_config_for_peers(&answering, std::iter::empty::<&[u8]>()).unwrap();
            let endpoint =
                Endpoint::server(config.quinn_config(), "127.0.0.1:0".parse().unwrap()).unwrap();
            let accepting = endpoint.clone();
            tokio::spawn(async move {
                while let Some(incoming) = accepting.accept().await {
                    if let Ok(Accepted::Hello(knock)) = accept(incoming, &config).await {
                        let _ = knock
                            .exchange(&make_hello("linux", 43119, Vec::new(), true))
                            .await;
                    }
                }
            });
            endpoint
        });
        let record = endpoint.local_addr().unwrap();
        links.say_hello("zf-linux".into(), vec![record], greeting);
        let Heard::Hello {
            instance,
            remote,
            hello,
            ..
        } = heard(&mut links)
        else {
            panic!("expected the record's hello");
        };
        assert_eq!((instance.as_deref(), remote), (Some("zf-linux"), record));
        assert!(hello.trusts_you);
        let deadline = Instant::now() + Duration::from_secs(2);
        while links.hellos_in_flight() > 0 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(links);
    }

    #[test]
    fn a_paused_mac_answers_hellos_and_closes_a_peers_input_without_disowning_it() {
        use crate::{
            hello::make_hello,
            transport::{TransportError, hello_client_config},
        };
        let server = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (mac, linux) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mac_spki = Identity::load_or_create(mac.path())
            .unwrap()
            .spki()
            .to_vec();
        let linux_key = Arc::new(Identity::load_or_create(linux.path()).unwrap());
        let config = mac_config(mac.path(), linux.path(), "127.0.0.1:9".parse().unwrap());
        let mut links = Links::with_backend(FakeBackend::default()).unwrap();
        links.sync(&config, false, false);
        assert!(
            links.states().next().is_none(),
            "nothing dials while paused"
        );
        let address = listening(&links);

        // A hello still reaches the app.
        let knocking = linux_key.clone();
        server.spawn(async move {
            let endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let client = hello_client_config(&knocking).unwrap();
            let connection = connect_hello(&endpoint, address, &client).await.unwrap();
            let _ = connection
                .exchange(&make_hello("linux", 43119, Vec::new(), true))
                .await;
        });
        let Heard::Knock(knock) = heard(&mut links) else {
            panic!("expected a knock");
        };
        knock.close();

        // The peer's input is closed, but not as from a key never added.
        let closed = server.block_on(async move {
            let endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let client = input_client_config(&linux_key, &mac_spki).unwrap();
            let connection = connect_input(&endpoint, address, &client).await.unwrap();
            let closed = tokio::time::timeout(Duration::from_secs(2), connection.closed());
            TransportError::from(closed.await.expect("the Mac closed the input"))
        });
        assert!(!matches!(closed, TransportError::NotTrusted), "{closed}");
        assert!(links.take_heard().is_empty(), "no refusal was heard");
    }

    #[test]
    fn a_port_in_use_only_stops_connections_coming_in() {
        let busy = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let (mac, linux) = identities(true);
        let mut config = mac_config(mac.path(), linux.path(), busy.local_addr().unwrap());
        config.transport.listen = busy.local_addr().unwrap();
        let mut links = Links::with_backend(FakeBackend::default()).unwrap();
        links.sync(&config, true, false);
        let error = links.listen_error().unwrap();
        assert!(
            error.starts_with("Other computers cannot connect"),
            "{error}"
        );
        assert_eq!(links.states().count(), 1, "the link still dials");
        // Once the port is free, the next sync listens on it.
        drop(busy);
        links.sync(&config, true, false);
        assert_eq!(links.listen_error(), None);
        assert!(links.listener.is_some());
        config.transport.listen = "127.0.0.1:0".parse().unwrap();
        links.sync(&config, true, false);
        assert_eq!(links.listen_error(), None);
        assert!(links.listener.is_some());
        // Paused, it still listens, so peers hear it is there.
        links.sync(&config, false, false);
        assert!(links.listener.is_some() && links.states().next().is_none());
        // Nobody who may connect, and no welcome, listens on nothing.
        config.peers.get_mut("linux").unwrap().permissions.connect = false;
        links.sync(&config, true, false);
        assert!(links.listener.is_none() && links.listen_error().is_none());
    }

    #[test]
    fn a_refused_crossing_gives_input_back() {
        let ownership = Ownership::default();
        let (status, mut statuses) = mpsc::unbounded_channel();
        let command = Command::Cross {
            handoff: handoff("linux"),
            entry_position: ENTRY,
            reduce_wifi_latency: false,
            stop: watch::channel(false).1,
            status,
            guard: ownership.begin_outbound().unwrap(),
        };
        assert!(ownership.claim_inbound("linux", 1).is_err());
        refuse(command, "the other computer is not connected");
        assert!(matches!(
            statuses.try_recv(),
            Ok(SourceStatus::Cancelled(_))
        ));
        assert!(ownership.claim_inbound("linux", 1).is_ok());
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
        // The saved address stopped answering, as after a DHCP change.
        let stale = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let addresses = [stale.local_addr().unwrap(), stranger, linux];
        let (address, _connection) = connect_peer(&endpoint, &client, &addresses).await.unwrap();
        assert_eq!(address, linux);
        assert!(connect_peer(&endpoint, &client, &[stranger]).await.is_err());
        endpoint.close(0_u32.into(), b"test finished");
        for server in servers {
            server.close(0_u32.into(), b"test finished");
        }
    }
}
