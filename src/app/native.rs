use super::{
    api::{self, Action, Health, Level, Peer, PeerState, Request, Shortcut, Status},
    handoff,
    layout_model::{self, Layout, LayoutDocument, Monitor},
    model::{ConfigDocument, Contents, Document},
    nearby::{BrowserStatus, NearbyBrowser},
    sharing::Observer,
};
use crate::{
    config::Config,
    desktop::{MAX_SHARED_TILES, SharedLayout},
    discovery::UntrustedCandidate,
    hello::{self, Notice, NoticeKind, peer_name},
    identity::Identity,
    macos::{self, Advertiser, Found, Greeting, Heard, LinkState, Links, LocalNetwork},
    neighbors::{Neighbors, UnplacedState, fingerprint, mark},
    pairing_window::PairingWindow,
    transport::{HelloClientConfig, hello_client_config},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const MAINTENANCE: Duration = Duration::from_secs(2);
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5);
/// Once allowed, the probe only watches for access being turned off, and each
/// probe that goes through is a packet on the network.
const LOCAL_NETWORK_RECHECK: Duration = Duration::from_secs(60);
/// Each refused tap leaks a Mach port inside CoreGraphics.
const TAP_RECHECK: Duration = Duration::from_secs(10);
/// A paired computer's tile until it writes its own size, as the Linux
/// daemon starts one.
const PEER_TILE_SIZE: (u32, u32) = (1920, 1080);
/// How often the computers around are weighed: who to say hello to, where
/// paired ones are now, and whether the pairing window lets one join.
const NEIGHBORS_TICK: Duration = Duration::from_secs(1);
/// Hellos this Mac sends at once.
const MAX_HELLOS: usize = 4;
/// Joined notices kept for the window to post.
const MAX_NOTICES: usize = 8;
/// Present in the state directory once setup asked to look for computers,
/// so a Mac with nothing paired yet keeps listening after a restart.
const DISCOVER_FILE: &str = "discover";

/// The layout every paired computer keeps, saved beside the configuration
/// as `NAME.shared-layout.toml`. Empty until this Mac first connects.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Kept {
    #[serde(skip_serializing_if = "Option::is_none")]
    layout: Option<SharedLayout>,
}

impl Contents for Kept {
    const NAME: &'static str = "shared layout";

    fn missing(_: &Path) -> Self {
        Self::default()
    }

    fn check_read(&self) -> Result<()> {
        self.layout.as_ref().map_or(Ok(()), SharedLayout::validate)
    }
}

fn shared_layout_path(config_path: &Path) -> Result<PathBuf> {
    let mut file_name = config_path
        .file_name()
        .context("Configuration path needs a filename")?
        .to_os_string();
    file_name.push(".shared-layout.toml");
    Ok(config_path.with_file_name(file_name))
}

/// The rows under the shared ones in the Mac settings window. The app adds
/// what only it knows: the helper, the login item and where it runs from.
#[derive(Serialize)]
pub(super) struct MacPlatform {
    accessibility: bool,
    local_network: LocalNetwork,
    block_awdl: bool,
}

pub(crate) struct NativeApp {
    document: ConfigDocument,
    /// What the window arranges. Once a shared layout exists, this Mac's
    /// view of it.
    layout: LayoutDocument,
    shared: Document<Kept>,
    // The observer drops first, so a live crossing is told to return input
    // before the links wait for their sessions to close.
    observer: Observer,
    links: Links,
    /// Tells the local network where peers connect, while this Mac listens.
    advertiser: Option<Advertiser>,
    nearby: NearbyBrowser,
    /// The computers around and what their hellos said.
    neighbors: Neighbors,
    /// The records last given to `neighbors`.
    instances: BTreeSet<String>,
    neighbors_at: Instant,
    /// This Mac's key, read once, and the hello client made with it.
    identity: Option<Identity>,
    hello_client: Option<HelloClientConfig>,
    window: PairingWindow,
    /// Computers that joined, newest last, for the window to post once.
    notices: Vec<Notice>,
    /// Receiver desktop sizes read over each peer's session.
    desktops: BTreeMap<String, (u32, u32)>,
    accessibility: bool,
    tap_refused: Option<Instant>,
    /// Set once setup asks to look for computers, and kept across restarts.
    discover: bool,
    /// Whether the links were last told to answer hellos.
    welcome: bool,
    local_network: LocalNetwork,
    local_network_checked: Instant,
    helper_ready: bool,
    emergency_paused: bool,
    config_error: Option<String>,
    layout_error: Option<String>,
    crossing_error: Option<String>,
    maintenance: Instant,
    retry_at: Instant,
}

impl NativeApp {
    pub fn open(path: PathBuf) -> Result<Self> {
        let mut document = ConfigDocument::open(path)?;
        document.validate()?;
        let state_dir = document.saved().daemon.state_dir.clone();
        if document.is_new() {
            document.save()?;
            // A fresh install may let one computer join on its own, once.
            if let Err(error) = PairingWindow::mark_eligible(&state_dir) {
                tracing::warn!(%error, "the pairing window cannot open");
            }
        }
        let layout = LayoutDocument::beside(&document.path)?;
        let shared = Document::open(shared_layout_path(&document.path)?)?;
        let identity = Identity::load_or_create(&state_dir)
            .inspect_err(|error| tracing::warn!(%error, "could not read this Mac's key"))
            .ok();
        let mut app = Self {
            document,
            layout,
            shared,
            observer: Observer::default(),
            links: Links::new()?,
            advertiser: None,
            nearby: NearbyBrowser::default(),
            neighbors: Neighbors::new(identity.as_ref().map_or(&[][..], Identity::spki)),
            instances: BTreeSet::new(),
            neighbors_at: Instant::now(),
            hello_client: identity
                .as_ref()
                .and_then(|identity| hello_client_config(identity).ok()),
            identity,
            window: PairingWindow::load(&state_dir),
            notices: Vec::new(),
            desktops: BTreeMap::new(),
            accessibility: crate::macos::accessibility_authorized(false),
            tap_refused: None,
            discover: state_dir.join(DISCOVER_FILE).exists(),
            welcome: false,
            local_network: LocalNetwork::Unknown,
            local_network_checked: Instant::now(),
            helper_ready: false,
            emergency_paused: false,
            config_error: None,
            layout_error: None,
            crossing_error: None,
            maintenance: Instant::now() - MAINTENANCE,
            retry_at: Instant::now(),
        };
        if let Some(kept) = &app.shared.draft.layout {
            app.links.share_layout(kept.clone());
        }
        // A computer paired while zflow was not running has no tile yet.
        app.place_new_peers();
        app.sync_links();
        Ok(app)
    }

    pub fn request(&mut self, request: Request) -> Result<Value> {
        match request {
            Request::Snapshot => {}
            Request::Reload => self.reload(),
            Request::SetSharing { enabled } => {
                if !enabled {
                    self.emergency_paused = true;
                    self.observer.stop();
                }
                self.document.draft.macos.sharing = enabled;
                self.save_config()?;
                self.emergency_paused = !enabled;
                self.restart();
            }
            Request::SetAwdl { enabled } => {
                self.document.draft.macos.block_awdl = enabled;
                self.save_config()?;
                self.restart();
            }
            // Readiness only gates the next arming in tick(). One failed
            // helper check must not pull input back from a live session.
            Request::HelperReady { ready } => self.helper_ready = ready,
            Request::MoveTile {
                id,
                x,
                y,
                tolerance,
            } => {
                let mut moved = self.layout.draft.clone();
                let index = moved
                    .monitors
                    .iter()
                    .position(|m| m.id == id)
                    .context("Unknown computer")?;
                let (x, y) = moved
                    .snap_move(index, x, y, tolerance.min(2048) as i32)
                    .context("Computers cannot overlap")?;
                moved.monitors[index].x = x;
                moved.monitors[index].y = y;
                self.rearrange(moved)?;
                tracing::info!(%id, "tile moved");
                self.restart();
            }
            Request::Place {
                id,
                x,
                y,
                tolerance,
            } => self.place(&id, x, y, tolerance)?,
            Request::AddAddress { address } => {
                let address = input_address(&address)?;
                self.neighbors.add_address(address);
                // Says hello on the next tick rather than in a second.
                self.neighbors_at = Instant::now() - NEIGHBORS_TICK;
            }
            Request::Forget { name } => {
                self.document.draft.peers.remove(&name);
                self.save_config()?;
                self.restart();
                self.place_new_peers();
            }
            Request::AllowAccessibility => {
                crate::macos::accessibility_authorized(true);
                self.accessibility = self.accessibility_granted();
            }
            Request::Retry => {
                self.crossing_error = None;
                self.advertiser = None;
                self.links.retry();
                self.restart();
            }
            Request::CheckAccessibility => self.accessibility = self.accessibility_granted(),
            Request::Discover => {
                if !self.discover {
                    self.discover = true;
                    let state_dir = &self.document.saved().daemon.state_dir;
                    let path = state_dir.join(DISCOVER_FILE);
                    if let Err(error) = crate::config::save_text(&path, "") {
                        tracing::warn!(%error, "looking for computers stops at restart");
                    }
                }
                self.sync_discovery();
                if self.discovers() {
                    self.check_local_network();
                }
            }
            Request::SetPeer {
                name,
                allow_control,
                keyboard,
                reverse_scroll,
            } => {
                let peer = self
                    .document
                    .draft
                    .peers
                    .get_mut(&name)
                    .with_context(|| format!("Unknown computer {name}"))?;
                crate::peer_view::set_peer(peer, allow_control, keyboard, reverse_scroll);
                self.save_config()?;
                // Links take who may control this Mac without reconnecting,
                // so a crossing in progress carries on.
                self.sync_links();
            }
            Request::SetSwitching { pause_at_edges } => {
                self.document.draft.switching.pause_at_edges = pause_at_edges;
                self.save_config()?;
                // The observer reads it on its next tick, so nothing restarts.
            }
            Request::SetClipboard { share } => {
                self.document.draft.clipboard.share = share;
                self.save_config()?;
                // Links take it without reconnecting, like peer settings.
                self.sync_links();
            }
            // The app keeps the login item.
            Request::SetAutostart { .. }
            | Request::OpenSettings
            | Request::OpenLogs
            | Request::InstallExtension => bail!("Not available on this computer"),
        }
        Ok(serde_json::to_value(self.snapshot())?)
    }

    /// AXIsProcessTrusted can stay true after zflow is removed from the
    /// Accessibility list. A crossing's tap then blocks the Mac's input, new
    /// crossings fail, and a peer's input is silently dropped. The window
    /// server knows, so ask it for a tap while sharing is armed, while a peer
    /// controls this Mac, and until access comes back.
    fn accessibility_granted(&mut self) -> bool {
        if !crate::macos::accessibility_authorized(false) {
            return false;
        }
        if self.accessibility && !self.observer.is_active() && self.links.controller().is_none() {
            return true;
        }
        if self
            .tap_refused
            .is_some_and(|refused| refused.elapsed() < TAP_RECHECK)
        {
            return false;
        }
        let allowed = crate::macos::event_tap_allowed();
        self.tap_refused = (!allowed).then(Instant::now);
        allowed
    }

    /// macOS asks for Local Network access on the first send to the network,
    /// so nothing is sent until the setup step that explains it, or until a
    /// paired computer needs the network anyway.
    fn discovers(&self) -> bool {
        let config = self.document.saved();
        config.transport.discovery && (self.discover || !config.peers.is_empty())
    }

    fn sync_discovery(&mut self) {
        if self.discovers() {
            self.nearby.start();
        } else {
            self.nearby.stop();
            self.local_network = LocalNetwork::Unknown;
        }
        // While it looks for computers, this Mac also answers their hellos.
        if self.discovers() != self.welcome {
            self.sync_links();
        } else {
            self.sync_advertiser();
        }
    }

    /// Advertises the port computers connect to while this Mac listens, and
    /// only once it may use the network. One that failed waits for Retry or
    /// a new port.
    fn sync_advertiser(&mut self) {
        let port = self.links.listen_port().filter(|_| self.discovers());
        if self.advertiser.as_ref().map(Advertiser::port) != port {
            self.advertiser = port.map(Advertiser::start);
        }
    }

    fn check_local_network(&mut self) {
        let previous = std::mem::replace(
            &mut self.local_network,
            crate::macos::local_network_access(),
        );
        self.local_network_checked = Instant::now();
        // Queries sent while access was off were dropped, and the browser
        // waits longer before each retry. A new one asks again at once.
        if previous == LocalNetwork::Blocked && self.local_network == LocalNetwork::Allowed {
            self.nearby.stop();
            self.sync_discovery();
        }
    }

    fn save_config(&mut self) -> Result<()> {
        if let Err(error) = self.document.save() {
            self.document.draft = self.document.saved().clone();
            return Err(error);
        }
        self.config_error = None;
        Ok(())
    }

    /// Disarms and rearms on the next tick with the saved settings. Links
    /// reconnect only when their peer or session settings changed.
    fn restart(&mut self) {
        self.observer.stop();
        self.retry_at = Instant::now();
        self.sync_links();
    }

    fn sync_links(&mut self) {
        self.welcome = self.discovers();
        let config = self.document.saved();
        let sharing = config.macos.sharing && !self.emergency_paused;
        self.links.sync(config, sharing, self.welcome);
        self.sync_advertiser();
    }

    fn reload(&mut self) {
        match ConfigDocument::open(self.document.path.clone()).and_then(|doc| {doc.validate()?; Ok(doc)}) {
            Ok(document) if !document.is_new() => {
                // A computer added to the file by hand gets its tile once the
                // change is read. Links find each computer by key, so new
                // addresses alone change nothing.
                if without_addresses(document.saved())!=without_addresses(self.document.saved()) { self.document=document; self.restart(); self.place_new_peers(); }
                else { self.document=document; }
                self.config_error=None;
            },
            Ok(_) => self.config_error=Some("Configuration was deleted. Restore it to apply changes. The last valid settings are still in use.".into()),
            Err(error) => self.config_error=Some(format!("{error:#}. The last valid settings are still in use.")),
        }
        match LayoutDocument::beside(&self.document.path) {
            Ok(layout) if !layout.is_new() || self.layout.is_new() => {
                if layout.draft != self.layout.draft {
                    self.layout = layout;
                    self.restart();
                } else {
                    self.layout = layout;
                }
                self.layout_error = None;
            }
            Ok(_) => {
                self.layout_error =
                    Some("The layout file was deleted. Restore it to apply changes.".into())
            }
            Err(error) => self.layout_error = Some(format!("{error:#}")),
        }
    }

    pub fn tick(&mut self) {
        // A missing helper only costs Wi-Fi latency, as when sending.
        let reduce_wifi_latency = self.document.saved().macos.block_awdl && self.helper_ready;
        self.links
            .set_receive_policy(self.accessibility, reduce_wifi_latency);
        let adopted = self.take_layouts();
        let was_enabled = self.observer.is_enabled();
        self.observer.tick(&self.links);
        if was_enabled && !self.observer.is_enabled() {
            if self.observer.pause_requested {
                self.emergency_paused = true;
                self.document.draft.macos.sharing = false;
                // A conflicting external edit must never re-arm an emergency pause.
                if self.save_config().is_err() {
                    self.document.draft.macos.sharing = false;
                }
                self.sync_links();
            } else {
                self.crossing_error = Some(self.observer.notice.clone());
                self.retry_at = Instant::now() + RETRY_AFTER_FAILURE;
            }
        }
        if self.maintenance.elapsed() >= MAINTENANCE {
            self.maintenance = Instant::now();
            self.reload();
            // The port can be held for a moment, as by a session still
            // closing after sharing went off and on, so listen again.
            if self.links.listen_error().is_some() {
                self.sync_links();
            }
            // Also a fallback for display and permission changes that sent no callback.
            macos::forget_desktop_geometry();
            let accessibility = self.accessibility_granted();
            if accessibility != self.accessibility {
                tracing::info!(accessibility, "Accessibility changed");
            }
            self.accessibility = accessibility;
            self.sync_discovery();
            if self.discovers()
                && (self.local_network != LocalNetwork::Allowed
                    || self.local_network_checked.elapsed() >= LOCAL_NETWORK_RECHECK)
            {
                self.check_local_network();
            }
            if let Err(error) = self.sync_layout() {
                self.layout_error = Some(format!("{error:#}"));
            }
        }
        self.take_heard();
        if self.neighbors_at.elapsed() >= NEIGHBORS_TICK {
            self.neighbors_at = Instant::now();
            self.tick_neighbors(tokio::time::Instant::now());
        }
        let changed = self.links.changed();
        if changed {
            self.desktops = self
                .links
                .states()
                .filter_map(|(name, state)| match state {
                    LinkState::Ready(geometry) => geometry
                        .bounds()
                        .ok()
                        .map(|bounds| (name.to_owned(), (bounds.width, bounds.height))),
                    _ => None,
                })
                .collect();
        }
        if (changed || adopted)
            && let Err(error) = self.sync_layout()
        {
            self.layout_error = Some(format!("{error:#}"));
        }
        if self.observer.has_session() {
            // Without Accessibility the crossing's tap stalls the Mac's input,
            // so end the crossing, which removes the tap.
            if !self.accessibility && self.observer.is_enabled() {
                self.observer.stop();
            }
            return;
        }
        let config = self.document.saved();
        if self.emergency_paused || !config.macos.sharing {
            return;
        }
        if config.peers.is_empty() || !self.accessibility {
            self.observer.stop();
            return;
        }
        // A missing helper only costs Wi-Fi latency, so it never holds sharing back.
        // Crossings read this when they start, so a later helper needs no restart.
        self.observer.reduce_wifi_latency = config.macos.block_awdl && self.helper_ready;
        self.observer.pause_at_edges = config.switching.pause_at_edges;
        if !self.observer.is_enabled() && Instant::now() >= self.retry_at && self.any_ready() {
            match self.observer.enable(config, &self.layout.draft) {
                Ok(()) => self.crossing_error = None,
                Err(error) => {
                    self.layout_error = Some(format!("{error:#}"));
                    self.retry_at = Instant::now() + Duration::from_secs(1);
                }
            }
        }
    }

    fn any_ready(&self) -> bool {
        self.links
            .states()
            .any(|(_, state)| matches!(state, LinkState::Ready(_)))
    }

    /// What this Mac says in a hello, or None without its key.
    fn greeting(&self) -> Option<Greeting> {
        let config = self.document.saved();
        let trusted = config
            .peers
            .values()
            .filter_map(|peer| peer.spki_der().ok());
        Some(Greeting {
            client: self.hello_client.clone()?,
            port: self
                .links
                .listen_port()
                .unwrap_or(config.transport.listen.port()),
            trusted: Arc::new(trusted.collect()),
        })
    }

    fn take_heard(&mut self) {
        for heard in self.links.take_heard() {
            self.hear(heard, tokio::time::Instant::now());
        }
    }

    /// Answers a knock, within its address's share, keeps what a hello or
    /// refusal told, and saves where a trusted computer's session came from.
    fn hear(&mut self, heard: Heard, now: tokio::time::Instant) {
        match heard {
            Heard::Knock(knock) => {
                let local = crate::discovery::this_host_addresses();
                let ip = knock.remote_address().ip();
                let allowed = self.neighbors.allow_hello(ip, &local, now);
                match self.greeting().filter(|_| allowed) {
                    Some(greeting) => self.links.answer(knock, greeting),
                    None => knock.close(),
                }
            }
            Heard::Hello {
                instance,
                spki,
                remote,
                hello,
            } => {
                self.neighbors
                    .hello(&spki, remote, &hello, instance.as_deref(), now);
            }
            Heard::Refused { spki } => self.neighbors.refused_input(&spki),
            Heard::TurnedAway => self.neighbors.turned_one_away(),
            Heard::Connected {
                key,
                address,
                dialed,
            } => self.learn(&key, address, dialed),
        }
    }

    /// Saves where a paired computer's session came from, to dial it there
    /// next, as the Linux service does. A hello's addresses are only its
    /// word and are never saved. A computer that connected here dialed from
    /// any port, so its address gets the input port its hello named, else
    /// the one saved with it. Links find it by key, so nothing reconnects.
    fn learn(&mut self, key: &str, address: SocketAddr, dialed: bool) {
        let listen = self.document.saved().transport.listen.port();
        let mut peers = self.document.draft.peers.values_mut();
        let Some(peer) = peers.find(|peer| peer.spki_der_hex == key) else {
            return;
        };
        let address = if dialed {
            address
        } else {
            let heard = peer
                .fingerprint_hex()
                .ok()
                .and_then(|key| self.neighbors.neighbor(&key)?.addresses.first().copied());
            let port = heard
                .or(peer.addresses.first().copied())
                .map_or(listen, |known| known.port());
            SocketAddr::new(address.ip().to_canonical(), port)
        };
        if peer.learn_addresses([address])
            && let Err(error) = self.save_config()
        {
            tracing::warn!(error = %format!("{error:#}"), "a computer's new address was not saved");
        }
    }

    /// Feeds the records found to `neighbors`, says hello to the ones that
    /// need it, tells the links where paired computers are, and lets the
    /// pairing window weigh who is around.
    fn tick_neighbors(&mut self, now: tokio::time::Instant) {
        let nearby = self.nearby.snapshot();
        if nearby.turned_away {
            self.neighbors.turned_one_away();
        }
        let records = nearby.records;
        for gone in self
            .instances
            .iter()
            .filter(|id| !records.contains_key(*id))
        {
            self.neighbors.instance_gone(gone, now);
        }
        for record in records.values() {
            let addresses = record.addresses.clone();
            let name = record.name.clone();
            self.neighbors
                .instance_seen(&record.instance, addresses, record.compatible, name);
        }
        self.instances = records.into_keys().collect();
        self.neighbors.expire(now);
        let room = MAX_HELLOS.saturating_sub(self.links.hellos_in_flight());
        if let Some(greeting) = self.greeting().filter(|_| room > 0) {
            for (instance, addresses) in self.neighbors.take_due_hellos(room, now) {
                self.links.say_hello(instance, addresses, greeting.clone());
            }
        }
        let found: Found = self
            .document
            .saved()
            .peers
            .values()
            .filter_map(|peer| {
                let addresses = self
                    .neighbors
                    .addresses_for_key(&peer.fingerprint_hex().ok()?);
                (!addresses.is_empty()).then(|| (peer.spki_der_hex.clone(), addresses))
            })
            .collect();
        self.links.set_found(found);
        self.tick_window(now);
    }

    /// Opens a fresh install's pairing window once this Mac can reach the
    /// network, and trusts the lone computer it settles on.
    fn tick_window(&mut self, now: tokio::time::Instant) {
        if self.discovers() && self.local_network == LocalNetwork::Allowed {
            self.window
                .open(!self.document.saved().peers.is_empty(), now);
        }
        let strangers = self.neighbors.strangers(self.document.saved(), now);
        let Some(stranger) = self.window.tick(&strangers, now) else {
            return;
        };
        match self.trust(&stranger.key) {
            Ok(name) => {
                tracing::info!(%name, "joined while the pairing window was open");
                self.restart();
                self.place_new_peers();
            }
            Err(error) => {
                let error = format!("{error:#}");
                tracing::warn!(%error, "could not add the computer that joined");
            }
        }
    }

    /// Saves the computer whose hello proved `key` as trusted, and tells
    /// people it joined. Returns its name here.
    fn trust(&mut self, key: &str) -> Result<String> {
        let neighbor = self.neighbors.neighbor(key).context("That computer left")?;
        let addresses = self.neighbors.addresses_for_key(key);
        let config = &mut self.document.draft;
        let name = hello::trust_peer(config, &neighbor.spki, &neighbor.name, &addresses)?;
        self.save_config()?;
        self.joined(&name);
        Ok(name)
    }

    fn joined(&mut self, name: &str) {
        let id = self.notices.last().map_or(1, |notice| notice.id + 1);
        self.notices.push(Notice {
            id,
            kind: NoticeKind::Joined,
            name: name.to_owned(),
        });
        if self.notices.len() > MAX_NOTICES {
            self.notices.remove(0);
        }
    }

    /// Trusts the computer dropped from the shelf, with its tile where it
    /// landed: `(x, y)` is the tile's corner at the size a new tile gets.
    /// Dropped onto the tile of a computer with its name, it takes that
    /// computer's place, as one that was reset or reinstalled.
    fn place(&mut self, id: &str, x: i32, y: i32, tolerance: u32) -> Result<()> {
        let config = self.document.saved();
        let tile = self
            .neighbors
            .unplaced(config)
            .into_iter()
            .find(|tile| tile.id == id);
        let tile = tile.context("That computer is no longer around")?;
        match tile.state {
            UnplacedState::Ready | UnplacedState::DuplicateName => {}
            UnplacedState::Identifying => bail!("{} is still being identified", tile.name),
            UnplacedState::DifferentVersion => {
                bail!(
                    "{} runs another zflow version. Update both computers.",
                    tile.name
                )
            }
        }
        let key = id.strip_prefix("key:").context("Unknown computer")?;
        let neighbor = self.neighbors.neighbor(key).context("That computer left")?;
        let (width, height) = PEER_TILE_SIZE;
        let center = (
            i64::from(x) + i64::from(width / 2),
            i64::from(y) + i64::from(height / 2),
        );
        let namesake = self.layout.draft.monitors.iter().find(|monitor| {
            let (left, top) = (i64::from(monitor.x), i64::from(monitor.y));
            (left..left + i64::from(monitor.width)).contains(&center.0)
                && (top..top + i64::from(monitor.height)).contains(&center.1)
                && monitor
                    .peer
                    .as_ref()
                    .is_some_and(|peer| peer_name(Some(peer)) == neighbor.name)
        });
        if let Some(name) = namesake.and_then(|monitor| monitor.peer.clone()) {
            let old = config.peers.get(&name).context("Unknown computer")?;
            let old = old.fingerprint_hex()?;
            let spki = neighbor.spki.clone();
            let addresses = self.neighbors.addresses_for_key(key);
            hello::replace_key(&mut self.document.draft, &name, &spki, &addresses)?;
            self.save_config()?;
            let kept = self.shared.draft.layout.as_ref();
            let own = self.own_key();
            if let Some(next) = kept
                .zip(own)
                .and_then(|(kept, own)| kept.with_key_replaced(&own, &old, &fingerprint(&spki)))
            {
                self.keep_layout(next);
                self.show_shared()?;
            }
            tracing::info!(%name, "placed onto its old tile with a new key");
            self.joined(&name);
        } else {
            ensure!(
                config.peers.len() + 1 < MAX_SHARED_TILES,
                "The arrangement holds at most {MAX_SHARED_TILES} computers"
            );
            let name = self.trust(key)?;
            self.put_tile(&name, x, y, tolerance);
            tracing::info!(%name, "placed");
        }
        self.window.placed();
        self.restart();
        self.place_new_peers();
        Ok(())
    }

    /// Gives a computer just placed its tile at `(x, y)`, snapped to an edge
    /// nearby. Where it does not fit, it gets one beside this Mac as any new
    /// computer does.
    fn put_tile(&mut self, name: &str, x: i32, y: i32, tolerance: u32) {
        let (width, height) = PEER_TILE_SIZE;
        let mut next = self.layout.draft.clone();
        next.monitors.push(Monitor {
            id: format!("peer:{name}"),
            label: name.to_owned(),
            peer: Some(name.to_owned()),
            x,
            y,
            width,
            height,
        });
        let index = next.monitors.len() - 1;
        let Some((x, y)) = next.snap_move(index, x, y, tolerance.min(2048) as i32) else {
            return;
        };
        next.monitors[index].x = x;
        next.monitors[index].y = y;
        if let Err(error) = self.rearrange(next) {
            tracing::warn!(error = %format!("{error:#}"), "the new tile was not kept where it landed");
        }
    }

    /// Uses `next` as the arrangement: this Mac's own until it first
    /// connects, then the next version of the shared layout.
    fn rearrange(&mut self, next: Layout) -> Result<()> {
        match self.shared.draft.layout.clone() {
            None => {
                let previous = std::mem::replace(&mut self.layout.draft, next);
                if let Err(error) = self.layout.save() {
                    self.layout.draft = previous;
                    return Err(error);
                }
            }
            Some(kept) => {
                let own = self.own_key().context("Could not read this Mac's key")?;
                let keys = peer_keys(self.document.saved());
                let version =
                    next_version(Some(&kept)).context("The layout cannot change again")?;
                let shared = next.to_shared(version, &own, &keys);
                shared.validate()?;
                self.keep_layout(shared);
                self.show_shared()?;
            }
        }
        Ok(())
    }

    /// Takes the layouts peers sent. Returns whether one was kept.
    fn take_layouts(&mut self) -> bool {
        let mut adopted = false;
        for (peer, layout) in self.links.take_layouts() {
            adopted |= self.take_layout(&peer, layout);
        }
        adopted
    }

    /// Keeps a peer's layout if it is newer than the kept one.
    fn take_layout(&mut self, peer: &str, layout: SharedLayout) -> bool {
        let kept = self.shared.draft.layout.as_ref();
        if kept.is_some_and(|kept| !layout.is_newer_than(kept)) {
            return false;
        }
        tracing::info!(%peer, version = layout.version, "layout adopted");
        self.keep_layout(layout);
        // A computer paired here but not there has no tile in it yet.
        self.place_new_peers();
        true
    }

    /// Uses `layout` from now on, saves it, and gives it to every peer.
    fn keep_layout(&mut self, layout: SharedLayout) {
        self.links.share_layout(layout.clone());
        self.shared.draft.layout = Some(layout);
        if let Err(error) = self.shared.save() {
            let error = format!("{error:#}");
            tracing::warn!(%error, "layout not saved; still using it until restart");
        }
    }

    /// This Mac's key fingerprint, which names its tile in the shared layout.
    fn own_key(&self) -> Option<String> {
        let state = &self.document.saved().daemon.state_dir;
        Some(Identity::load_or_create(state).ok()?.fingerprint_hex())
    }

    /// Gives each paired computer the kept layout lacks a tile beside this
    /// Mac, such as one paired after the computers were arranged, and tells
    /// the other computers, as the Linux daemon does. Without a kept layout
    /// there is nothing to add to: the first one places every computer.
    fn place_new_peers(&mut self) {
        let Some(kept) = &self.shared.draft.layout else {
            return;
        };
        let Some(own) = self.own_key() else {
            return;
        };
        let keys = peer_keys(self.document.saved());
        let Some(placed) =
            kept.with_tiles_for(&own, keys.values().map(String::as_str), PEER_TILE_SIZE)
        else {
            return;
        };
        tracing::info!(
            version = placed.version,
            "paired computer placed in the layout"
        );
        self.keep_layout(placed);
        if let Err(error) = self.show_shared() {
            self.layout_error = Some(format!("{error:#}"));
        }
    }

    /// Arranges the computers on this Mac until it first connects. The
    /// arrangement then becomes the shared layout, and from there on this
    /// Mac only writes its own tile's size, adding its tile if a peer's
    /// layout lacks it.
    fn sync_layout(&mut self) -> Result<()> {
        if self.layout_error.is_some() {
            return Ok(());
        }
        let Ok(local) = macos::desktop_geometry().and_then(|geometry| geometry.bounds()) else {
            return Ok(());
        };
        if !fits(local.width, local.height) {
            return Ok(());
        }
        let size = (local.width, local.height);
        let connected = self.links.states().any(|(_, state)| {
            matches!(
                state,
                LinkState::Ready(_) | LinkState::Connected | LinkState::Refused(_)
            )
        });
        if self.shared.draft.layout.is_none() {
            self.arrange(size)?;
            if !connected {
                return Ok(());
            }
        }
        let Some(own) = self.own_key() else {
            return Ok(());
        };
        let next = match &self.shared.draft.layout {
            Some(kept) => kept.with_own_size(&own, size.0, size.1),
            None => next_version(None).and_then(|version| {
                let keys = peer_keys(self.document.saved());
                let first = self.layout.draft.to_shared(version, &own, &keys);
                // Computers whose desktop is not known yet were not arranged.
                let first = first
                    .with_tiles_for(&own, keys.values().map(String::as_str), PEER_TILE_SIZE)
                    .unwrap_or(first);
                first.validate().is_ok().then_some(first)
            }),
        };
        if let Some(next) = next {
            tracing::info!(version = next.version, "layout updated on this Mac");
            self.keep_layout(next);
        }
        self.show_shared()
    }

    /// Shows this Mac's view of the kept layout, and rearms sharing with it.
    fn show_shared(&mut self) -> Result<()> {
        let (Some(kept), Some(own)) = (&self.shared.draft.layout, self.own_key()) else {
            return Ok(());
        };
        let keys = peer_keys(self.document.saved());
        let previous = std::mem::replace(
            &mut self.layout.draft,
            Layout::from_shared(kept, &own, "This Mac", &keys),
        );
        if self.layout.is_dirty() || self.layout.is_new() {
            if let Err(error) = self.layout.save() {
                self.layout.draft = previous;
                return Err(error);
            }
            self.rearm();
        }
        Ok(())
    }

    /// Arms sharing again with a changed layout. A crossing in progress,
    /// perhaps the one a peer's edit arrived during, finishes first.
    fn rearm(&mut self) {
        if self.observer.is_active() {
            self.observer.disarm();
            self.retry_at = Instant::now();
        }
    }

    /// Places this Mac and each paired computer with a known desktop size,
    /// keeping where each already was.
    fn arrange(&mut self, (width, height): (u32, u32)) -> Result<()> {
        let previous = self.layout.draft.clone();
        let mut monitors = Vec::new();
        let owners = std::iter::once(None).chain(self.document.saved().peers.keys().map(Some));
        for owner in owners {
            let old = previous.monitors.iter().find(|m| m.peer.as_ref() == owner);
            let size = if let Some(name) = owner {
                self.desktops
                    .get(name)
                    .copied()
                    .filter(|&(width, height)| fits(width, height))
                    .or_else(|| old.map(|m| (m.width, m.height)))
            } else {
                Some((width, height))
            };
            let Some((width, height)) = size else {
                continue;
            };
            let right = monitors
                .iter()
                .map(|m: &Monitor| m.x + m.width as i32)
                .max()
                .unwrap_or(0);
            let mut monitor = Monitor {
                id: old
                    .map(|m| m.id.clone())
                    .unwrap_or_else(|| owner.map_or("local".into(), |name| format!("peer:{name}"))),
                label: owner.map_or("This Mac".into(), Clone::clone),
                peer: owner.cloned(),
                x: old.map_or(right, |m| m.x),
                y: old.map_or(0, |m| m.y),
                width,
                height,
            };
            monitors.push(monitor.clone());
            if (Layout {
                monitors: monitors.clone(),
            })
            .validate()
            .is_err()
            {
                monitors.pop();
                monitor.x = right;
                monitor.y = 0;
                monitors.push(monitor);
            }
        }
        self.layout.draft.monitors = monitors;
        if self.layout.is_dirty() || self.layout.is_new() {
            if let Err(error) = self.layout.save() {
                self.layout.draft = previous;
                return Err(error);
            }
            self.rearm();
        }
        Ok(())
    }

    /// What the settings window and the menu show.
    fn snapshot(&self) -> api::Snapshot<MacPlatform> {
        let config = self.document.saved();
        let sharing = config.macos.sharing && !self.emergency_paused;
        let controller = self.links.controller();
        let peers = peer_rows(
            config,
            self.links.states(),
            self.observer.session_peer(),
            controller.as_deref(),
        );
        let connected = peers.iter().any(|p| {
            matches!(
                p.state,
                PeerState::Connected | PeerState::ControlledFromHere | PeerState::ControllingThis
            )
        });
        let checking = !connected && peers.iter().any(|p| p.state == PeerState::Connecting);
        let mut health = vec![self.sharing_health(sharing)];
        if sharing {
            let refused = refusals(self.links.states(), controller.as_deref());
            health.extend(link_health(&peers, &refused));
            // The Mac still dials, so peers can control it over that.
            if let Some(error) = self.links.listen_error() {
                health.push(Health::new(
                    "listen",
                    Level::Warning,
                    "Incoming connections",
                    error,
                ));
            }
        }
        // A computer gets its tile once it connects, so until then there is
        // nothing to drag.
        let layout_issue = macos::desktop_geometry()
            .and_then(|g| handoff::validate(&self.layout.draft, &g))
            .err()
            .filter(|_| connected)
            .map(|e| e.to_string());
        if let Some(error) = self.layout_error.clone().or(layout_issue) {
            health.push(Health::new(
                "layout",
                Level::Error,
                "Computer layout",
                error,
            ));
        }
        if let Some(error) = &self.config_error {
            health.push(Health {
                action: Some(Action {
                    label: "Open Configuration…".into(),
                    command: "open_config".into(),
                }),
                ..Health::new("config", Level::Error, "Configuration", error.clone())
            });
        }
        if let Some(notice) = self.links.clipboard_notice() {
            health.push(Health::new(
                "clipboard",
                Level::Warning,
                "Clipboard",
                notice,
            ));
        }
        if let BrowserStatus::Failed(error) = self.nearby.status() {
            health.push(Health::new(
                "discovery",
                Level::Warning,
                "Nearby computers",
                error,
            ));
        }
        api::Snapshot {
            status: Status::new(Some(sharing), &peers, &health, checking),
            sharing: Some(sharing),
            health,
            layout: Some(self.layout.draft.clone()),
            peers,
            pause_at_edges: Some(config.switching.pause_at_edges),
            shortcuts: vec![Shortcut {
                title: "Return input to this computer".into(),
                keys: "⌃⌘⌫".into(),
            }],
            share_clipboard: Some(config.clipboard.share),
            autostart: None,
            config_path: self.document.path.clone(),
            pairing_window: self.window.view(tokio::time::Instant::now()),
            own_mark: self.identity.as_ref().map(|identity| mark(identity.spki())),
            unplaced: self.neighbors.unplaced(config),
            notices: self.notices.clone(),
            platform: MacPlatform {
                accessibility: self.accessibility,
                local_network: self.local_network,
                block_awdl: config.macos.block_awdl,
            },
        }
    }

    /// Whether this Mac can send its input right now. The window shows a row
    /// that is not fine as a banner, so this one carries the Allow button.
    fn sharing_health(&self, sharing: bool) -> Health {
        let row = |level, detail: &str| Health::new("sharing", level, "Sharing", detail);
        if !self.accessibility {
            return Health {
                action: Some(Action {
                    label: "Allow…".into(),
                    command: "allow_accessibility".into(),
                }),
                ..row(
                    Level::Error,
                    "Allow Accessibility so zflow can share the keyboard and pointer, and so paired computers can control this Mac.",
                )
            };
        }
        if let Some(error) = self.crossing_error.as_ref().filter(|_| sharing) {
            return Health {
                action: Some(retry()),
                ..row(Level::Error, error)
            };
        }
        if sharing && !self.observer.is_active() {
            return row(Level::Ok, "Sharing starts once a paired computer is ready.");
        }
        let level = if sharing && self.observer.waiting_for_secure_input() {
            Level::Warning
        } else {
            Level::Ok
        };
        row(level, &self.observer.notice)
    }
}

fn fits(width: u32, height: u32) -> bool {
    (1..=layout_model::MAX_DIMENSION).contains(&width)
        && (1..=layout_model::MAX_DIMENSION).contains(&height)
}

/// Each paired computer's key fingerprint, by its name here.
fn peer_keys(config: &Config) -> BTreeMap<String, String> {
    config
        .peers
        .iter()
        .filter_map(|(name, peer)| Some((name.clone(), peer.fingerprint_hex().ok()?)))
        .collect()
}

/// The version of this Mac's next edit: one past the newest it has seen.
/// A layout newer than the kept one would have been kept, so that is the
/// kept one's.
fn next_version(kept: Option<&SharedLayout>) -> Option<u64> {
    kept.map_or(0, |kept| kept.version).checked_add(1)
}

/// The configuration without the addresses saved for each computer, which
/// change as computers move and are learned without reconnecting.
fn without_addresses(config: &Config) -> Config {
    let mut config = config.clone();
    for peer in config.peers.values_mut() {
        peer.addresses.clear();
    }
    config
}

/// Where a computer typed in by address takes input. A bare IP address
/// means the port zflow listens on unless told otherwise.
fn input_address(input: &str) -> Result<SocketAddr> {
    let input = input.trim();
    let address = match input.parse::<SocketAddr>() {
        Ok(address) => address,
        Err(_) => {
            let ip = input.trim_start_matches('[').trim_end_matches(']');
            let ip: IpAddr = ip
                .parse()
                .with_context(|| format!("{input:?} is not an IP address"))?;
            SocketAddr::new(ip, Config::default().transport.listen.port())
        }
    };
    UntrustedCandidate::explicit(address)
        .with_context(|| format!("{address} cannot be reached"))?;
    Ok(address)
}

fn retry() -> Action {
    Action {
        label: "Retry".into(),
        command: "retry".into(),
    }
}

/// Each paired computer, from its link, the crossing in progress, and the
/// peer controlling this Mac. A computer without a link may not connect, or
/// sharing is off.
fn peer_rows<'a>(
    config: &Config,
    links: impl Iterator<Item = (&'a str, LinkState)>,
    controlled: Option<&str>,
    controller: Option<&str>,
) -> Vec<Peer> {
    let mut links: BTreeMap<_, _> = links.collect();
    config
        .peers
        .iter()
        .map(|(name, record)| {
            let (state, error) = match links.remove(name.as_str()) {
                _ if controlled == Some(name) => (PeerState::ControlledFromHere, None),
                _ if controller == Some(name) => (PeerState::ControllingThis, None),
                Some(LinkState::Ready(_) | LinkState::Connected | LinkState::Refused(_)) => {
                    (PeerState::Connected, None)
                }
                Some(LinkState::Connecting) => (PeerState::Connecting, None),
                Some(LinkState::Down(error)) => (PeerState::Unreachable, Some(error)),
                None => (PeerState::Paired, None),
            };
            let mut peer = Peer::new(name, record, state);
            if let Some(error) = error {
                peer.detail = error;
            }
            peer
        })
        .collect()
}

/// Why connected computers take no input from this Mac right now. The one
/// controlling this Mac refuses while it sends, so it is left out.
fn refusals<'a>(
    links: impl Iterator<Item = (&'a str, LinkState)>,
    controller: Option<&str>,
) -> Vec<String> {
    links
        .filter(|(name, _)| Some(*name) != controller)
        .filter_map(|(name, state)| match state {
            LinkState::Refused(reason) => {
                Some(format!("{name} takes no input from this Mac: {reason}"))
            }
            _ => None,
        })
        .collect()
}

/// One row for the links to paired computers, with a retry when one is down
/// or `refused` input from this Mac. Called only while sharing is on, so a
/// computer without a link is one that does not take input from this Mac.
fn link_health(peers: &[Peer], refused: &[String]) -> Option<Health> {
    if peers.is_empty() {
        return None;
    }
    let down: Vec<_> = peers
        .iter()
        .filter(|peer| peer.state == PeerState::Unreachable)
        .map(|peer| format!("{}: {}", peer.name, peer.detail))
        .collect();
    let title = "Paired computers";
    Some(if !down.is_empty() {
        Health {
            action: Some(retry()),
            ..Health::new("computers", Level::Error, title, down.join("\n"))
        }
    } else if !refused.is_empty() {
        // Connected all the same, so they can still control this Mac.
        Health {
            action: Some(retry()),
            ..Health::new("computers", Level::Warning, title, refused.join("\n"))
        }
    } else if peers.iter().all(|peer| peer.state == PeerState::Paired) {
        Health::new(
            "computers",
            Level::Error,
            title,
            "No computer takes input from this Mac. Forget one and drag it back into place to fix this.",
        )
    } else if peers.iter().any(|peer| peer.state == PeerState::Connecting) {
        Health::new("computers", Level::Ok, title, "Checking…")
    } else {
        Health::new("computers", Level::Ok, title, "No problems found.")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::Tile;

    #[test]
    fn helper_readiness_does_not_restart_sharing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        std::fs::write(
            &path,
            "[transport]\ndiscovery = false\n[macos]\nblock_awdl = true\n",
        )
        .unwrap();
        let mut app = NativeApp::open(path).unwrap();
        let armed_at = app.retry_at;
        for ready in [true, false, true] {
            app.request(Request::HelperReady { ready }).unwrap();
            assert_eq!(app.helper_ready, ready);
            assert_eq!(app.retry_at, armed_at);
        }
    }

    #[test]
    fn network_waits_for_setup_or_a_paired_computer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        // Discovery is on by default; only the missing peers hold it back.
        std::fs::write(&path, "[macos]\nsharing = true\n").unwrap();
        let mut app = NativeApp::open(path).unwrap();
        app.tick();
        assert!(!app.discovers());
        assert!(!app.nearby.is_running());
        assert_eq!(app.local_network, LocalNetwork::Unknown);
        // With discovery off, setup's request sends nothing either.
        app.document.draft.transport.discovery = false;
        app.save_config().unwrap();
        let snapshot = app.request(Request::Discover).unwrap();
        assert!(app.discover && !app.discovers());
        assert!(!app.nearby.is_running());
        assert_eq!(snapshot["platform"]["local_network"], "unknown");
    }

    #[test]
    fn the_mac_snapshot_lists_the_shared_rows_in_window_order() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        std::fs::write(
            &path,
            "[transport]\ndiscovery = false\n[macos]\nsharing = false\n",
        )
        .unwrap();
        let mut app = NativeApp::open(path.clone()).unwrap();
        let text = serde_json::to_string(&app.snapshot()).unwrap();
        let at = |key| text.find(&format!("\"{key}\":")).unwrap();
        let order = [
            "status",
            "sharing",
            "health",
            "pairing_window",
            "layout",
            "own_mark",
            "unplaced",
            "peers",
            "pause_at_edges",
            "shortcuts",
            "share_clipboard",
            "autostart",
            "config_path",
            "notices",
            "platform",
        ];
        assert!(
            order.windows(2).all(|pair| at(pair[0]) < at(pair[1])),
            "{text}"
        );
        let value = app.request(Request::Snapshot).unwrap();
        assert_eq!(value.as_object().unwrap().len(), order.len());
        assert_eq!(value["status"]["state"], "paused");
        assert_eq!(value["status"]["title"], "Paused");
        assert_eq!(value["sharing"], false);
        assert_eq!(value["health"][0]["id"], "sharing");
        assert_eq!(value["shortcuts"][0]["keys"], "⌃⌘⌫");
        assert_eq!(value["pause_at_edges"], false);
        assert_eq!(value["share_clipboard"], false);
        assert_eq!(value["autostart"], Value::Null);
        assert_eq!(value["config_path"], path.to_str().unwrap());
        assert_eq!(
            value["platform"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["accessibility", "block_awdl", "local_network"]
        );
        let error = app
            .request(Request::SetPeer {
                name: "desk".into(),
                allow_control: Some(true),
                keyboard: None,
                reverse_scroll: None,
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "Unknown computer desk");
    }

    #[test]
    fn peer_settings_are_saved_without_restarting_sharing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let peer =
            crate::identity::Identity::load_or_create(&directory.path().join("desk")).unwrap();
        let mut config = Config::default();
        config.transport.discovery = false;
        config.macos.sharing = false;
        let permissions = crate::config::PeerPermissions {
            connect: true,
            ..Default::default()
        };
        let record = crate::config::PeerConfig::from_spki(peer.spki(), Vec::new(), permissions);
        config.peers.insert("desk".into(), record.unwrap());
        config.save(&path).unwrap();
        let mut app = NativeApp::open(path.clone()).unwrap();
        let armed_at = app.retry_at;
        let value = app
            .request(Request::SetPeer {
                name: "desk".into(),
                allow_control: Some(true),
                keyboard: Some(crate::core::KeyboardMode::PcPositions),
                reverse_scroll: Some(true),
            })
            .unwrap();
        assert_eq!(app.retry_at, armed_at, "sharing did not restart");
        let saved = &Config::load(&path).unwrap().peers["desk"];
        assert!(saved.permissions.send_normal && saved.permissions.receive_normal);
        assert_eq!(saved.keyboard, crate::core::KeyboardMode::PcPositions);
        assert!(saved.reverse_scroll);
        let row = &value["peers"][0];
        assert_eq!(row["allow_control"], true);
        assert_eq!(row["keyboard"], "pc_positions");
        assert_eq!(row["reverse_scroll"], true);

        // The clipboard and pause switches apply without a restart too.
        let value = app.request(Request::SetClipboard { share: true }).unwrap();
        assert_eq!(app.retry_at, armed_at, "sharing did not restart");
        assert_eq!(value["share_clipboard"], true);
        assert!(Config::load(&path).unwrap().clipboard.share);
        let pause = Request::SetSwitching {
            pause_at_edges: true,
        };
        let value = app.request(pause).unwrap();
        assert_eq!(app.retry_at, armed_at, "sharing did not restart");
        assert_eq!(value["pause_at_edges"], true);
        assert!(Config::load(&path).unwrap().switching.pause_at_edges);
    }

    #[test]
    fn this_mac_is_advertised_only_while_it_listens_and_may_use_the_network() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let desk = Identity::load_or_create(&directory.path().join("desk")).unwrap();
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("state");
        config.transport.discovery = false;
        // A free port, since a saved configuration cannot ask for any.
        let free = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        config.transport.listen = free.local_addr().unwrap();
        drop(free);
        config.macos.sharing = true;
        let permissions = crate::config::PeerPermissions {
            connect: true,
            ..Default::default()
        };
        let record = crate::config::PeerConfig::from_spki(desk.spki(), Vec::new(), permissions);
        config.peers.insert("desk".into(), record.unwrap());
        config.save(&path).unwrap();
        let mut app = NativeApp::open(path).unwrap();
        assert_eq!(
            app.links.listen_port(),
            Some(config.transport.listen.port()),
            "{:?}",
            app.links.listen_error()
        );
        // Discovery is off, so nothing goes out even after setup asks.
        assert!(app.advertiser.is_none());
        app.request(Request::Discover).unwrap();
        assert!(!app.discovers() && app.advertiser.is_none());
        // Paused, it still listens, so its computers are not told it never
        // added them.
        app.request(Request::SetSharing { enabled: false }).unwrap();
        assert_eq!(
            app.links.listen_port(),
            Some(config.transport.listen.port())
        );
        assert!(app.advertiser.is_none());
    }

    #[test]
    fn missing_accessibility_comes_with_its_allow_button() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        std::fs::write(&path, "[transport]\ndiscovery = false\n").unwrap();
        let mut app = NativeApp::open(path).unwrap();
        app.accessibility = false;
        let row = app.sharing_health(true);
        assert_eq!(row.level, Level::Error);
        assert_eq!(row.action.unwrap().command, "allow_accessibility");
    }

    #[test]
    fn a_newer_shared_layout_is_kept_and_a_move_is_the_next_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let desk = Identity::load_or_create(&directory.path().join("desk")).unwrap();
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("state");
        config.transport.discovery = false;
        config.macos.sharing = false;
        let permissions = crate::config::PeerPermissions::default();
        let record = crate::config::PeerConfig::from_spki(desk.spki(), Vec::new(), permissions);
        config.peers.insert("desk".into(), record.unwrap());
        config.save(&path).unwrap();
        let mut app = NativeApp::open(path.clone()).unwrap();
        let (own, theirs) = (app.own_key().unwrap(), desk.fingerprint_hex());
        let layout = |version, desk_x| SharedLayout {
            version,
            editor: theirs.clone(),
            tiles: vec![
                Tile {
                    key: own.clone(),
                    x: 0,
                    y: 0,
                    width: 3008,
                    height: 1692,
                },
                Tile {
                    key: theirs.clone(),
                    x: desk_x,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            ],
        };
        assert!(app.take_layout("desk", layout(3, 3008)));
        assert!(!app.take_layout("desk", layout(2, -1920)), "older");
        app.show_shared().unwrap();
        let tile = |app: &NativeApp, id: &str| {
            let monitor = app.layout.draft.monitors.iter().find(|m| m.id == id);
            monitor.map(|m| (m.label.clone(), m.x)).unwrap()
        };
        assert_eq!(tile(&app, "local"), ("This Mac".into(), 0));
        assert_eq!(tile(&app, "peer:desk"), ("desk".into(), 3008));

        app.request(Request::MoveTile {
            id: "peer:desk".into(),
            x: -1920,
            y: 0,
            tolerance: 0,
        })
        .unwrap();
        assert_eq!(tile(&app, "peer:desk"), ("desk".into(), -1920));
        drop(app);
        // Kept beside the configuration, across a restart.
        let app = NativeApp::open(path).unwrap();
        let kept = app.shared.draft.layout.clone().unwrap();
        assert_eq!((kept.version, &kept.editor), (4, &own));
        assert_eq!(kept.tiles[1].x, -1920);
    }

    #[test]
    fn a_computer_paired_later_gets_a_tile_beside_this_mac() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let identity = |name: &str| Identity::load_or_create(&directory.path().join(name)).unwrap();
        let [desk, laptop, tablet] = ["desk", "laptop", "tablet"].map(identity);
        let record = |peer: &Identity| {
            let permissions = crate::config::PeerPermissions::default();
            crate::config::PeerConfig::from_spki(peer.spki(), Vec::new(), permissions).unwrap()
        };
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("state");
        config.transport.discovery = false;
        config.macos.sharing = false;
        config.peers.insert("desk".into(), record(&desk));
        config.save(&path).unwrap();
        let mut app = NativeApp::open(path.clone()).unwrap();
        let (own, theirs) = (app.own_key().unwrap(), desk.fingerprint_hex());
        let tile = |key: &str, x| Tile {
            key: key.into(),
            x,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let kept = |app: &NativeApp| app.shared.draft.layout.clone().unwrap();
        let arranged = SharedLayout {
            version: 3,
            editor: theirs.clone(),
            tiles: vec![tile(&own, 0), tile(&theirs, 1920)],
        };
        assert!(app.take_layout("desk", arranged.clone()));
        assert_eq!(kept(&app), arranged, "nothing missing");

        // A computer added to the file by hand reaches the app from there.
        config.peers.insert("laptop".into(), record(&laptop));
        config.save(&path).unwrap();
        app.reload();
        let placed = kept(&app);
        assert_eq!((placed.version, &placed.editor), (4, &own));
        // Desk is right of this Mac, so the laptop goes left.
        let laptop_tile = tile(&laptop.fingerprint_hex(), -1920);
        assert_eq!(placed.tiles[2], laptop_tile);
        let shown = app.layout.draft.monitors.iter();
        assert!(
            shown
                .map(|m| m.peer.as_deref())
                .any(|peer| peer == Some("laptop"))
        );

        // Desk's newer layout, with desk below, lacks the laptop, which
        // gets its tile back on the right.
        let below = SharedLayout {
            version: 9,
            tiles: vec![
                tile(&own, 0),
                Tile {
                    y: 1080,
                    ..tile(&theirs, 0)
                },
            ],
            ..arranged
        };
        assert!(app.take_layout("desk", below));
        let placed = kept(&app);
        assert_eq!((placed.version, &placed.editor), (10, &own));
        assert_eq!(
            placed.tiles[2],
            Tile {
                x: 1920,
                ..laptop_tile
            }
        );

        // A computer paired while the app was closed is placed at startup.
        drop(app);
        config.peers.insert("tablet".into(), record(&tablet));
        config.save(&path).unwrap();
        let app = NativeApp::open(path).unwrap();
        let placed = kept(&app);
        assert_eq!((placed.version, placed.tiles.len()), (11, 4));
        assert_eq!(placed.tiles[3], tile(&tablet.fingerprint_hex(), -1920));
    }

    /// A Mac on its own state directory with sharing off, so nothing
    /// listens, and with `peers` trusted.
    fn mac_with(directory: &Path, peers: &[(&str, &Identity)]) -> NativeApp {
        let mut config = Config::default();
        config.daemon.state_dir = directory.join("state");
        config.transport.discovery = false;
        config.macos.sharing = false;
        for (name, peer) in peers {
            let permissions = crate::config::PeerPermissions::default();
            let record = crate::config::PeerConfig::from_spki(peer.spki(), Vec::new(), permissions);
            config.peers.insert((*name).into(), record.unwrap());
        }
        let path = directory.join("zflow.toml");
        config.save(&path).unwrap();
        NativeApp::open(path).unwrap()
    }

    /// `identity` says hello to `app` as `name`, and returns its shelf id.
    fn says_hello(app: &mut NativeApp, identity: &Identity, name: &str) -> String {
        let hello = hello::make_hello(name, 43119, Vec::new(), false);
        let remote = "192.0.2.7:50000".parse().unwrap();
        let now = tokio::time::Instant::now();
        let key = app
            .neighbors
            .hello(identity.spki(), remote, &hello, None, now);
        format!("key:{}", key.unwrap())
    }

    /// `identity` answers the hello this Mac sent to its mDNS record.
    fn answers_hello(app: &mut NativeApp, identity: &Identity, name: &str) {
        let (record, remote) = (format!("zf-{name}"), "192.0.2.7:43119".parse().unwrap());
        let now = tokio::time::Instant::now();
        app.neighbors
            .instance_seen(&record, vec![remote], true, Some(name.into()));
        app.neighbors.take_due_hellos(MAX_HELLOS, now);
        let hello = hello::make_hello(name, 43119, Vec::new(), false);
        app.neighbors
            .hello(identity.spki(), remote, &hello, Some(&record), now);
    }

    #[test]
    fn a_computer_dropped_from_the_shelf_is_trusted_where_it_landed() {
        let directory = tempfile::tempdir().unwrap();
        let desk = Identity::load_or_create(&directory.path().join("desk")).unwrap();
        let mut app = mac_with(directory.path(), &[]);
        let id = says_hello(&mut app, &desk, "desk");
        let value = app.request(Request::Snapshot).unwrap();
        assert_eq!(value["unplaced"][0]["id"], id.as_str());
        assert_eq!(value["unplaced"][0]["state"], "ready");
        assert_eq!(value["unplaced"][0]["mark"], mark(desk.spki()));
        let own = Identity::load_or_create(&directory.path().join("state")).unwrap();
        assert_eq!(value["own_mark"], mark(own.spki()));

        // One still being identified, or gone, cannot be placed.
        app.request(Request::AddAddress {
            address: "100.64.0.7".into(),
        })
        .unwrap();
        let place = |id: &str, x| Request::Place {
            id: id.into(),
            x,
            y: 0,
            tolerance: 0,
        };
        let error = app
            .request(place("instance:address:100.64.0.7:43119", 0))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "100.64.0.7:43119 is still being identified"
        );
        assert!(app.request(place("key:00", 0)).is_err());

        let value = app.request(place(&id, 4000)).unwrap();
        let saved = Config::load(&directory.path().join("zflow.toml")).unwrap();
        let record = &saved.peers["desk"];
        assert_eq!(record.spki_der().unwrap(), desk.spki());
        assert!(record.permissions.connect && record.permissions.receive_normal);
        assert_eq!(record.addresses, ["192.0.2.7:43119".parse().unwrap()]);
        let tile = app
            .layout
            .draft
            .monitors
            .iter()
            .find(|m| m.id == "peer:desk");
        assert_eq!(tile.map(|m| (m.x, m.y)), Some((4000, 0)));
        assert_eq!(
            value["unplaced"].as_array().unwrap().len(),
            1,
            "only the address"
        );
        assert_eq!(value["peers"][0]["mark"], mark(desk.spki()));
        assert_eq!(value["notices"][0]["kind"], "joined");
        assert_eq!(value["notices"][0]["name"], "desk");
        assert_eq!(value["notices"][0]["id"], 1);

        // Forgotten, it is back on the shelf at once.
        let value = app
            .request(Request::Forget {
                name: "desk".into(),
            })
            .unwrap();
        let shelf = value["unplaced"].as_array().unwrap();
        assert!(shelf.iter().any(|tile| tile["id"] == id.as_str()));
    }

    #[test]
    fn dropped_onto_its_namesake_a_computer_takes_over_its_tile_and_settings() {
        let directory = tempfile::tempdir().unwrap();
        let identity = |name: &str| Identity::load_or_create(&directory.path().join(name)).unwrap();
        let (desk, reinstalled) = (identity("desk"), identity("desk-new"));
        let mut app = mac_with(directory.path(), &[("desk", &desk)]);
        app.request(Request::SetPeer {
            name: "desk".into(),
            allow_control: Some(true),
            keyboard: Some(crate::core::KeyboardMode::PcPositions),
            reverse_scroll: None,
        })
        .unwrap();
        let (own, old) = (app.own_key().unwrap(), desk.fingerprint_hex());
        let tile = |key: &str, x| Tile {
            key: key.into(),
            x,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let arranged = SharedLayout {
            version: 3,
            editor: old.clone(),
            tiles: vec![tile(&own, 0), tile(&old, 1920)],
        };
        assert!(app.take_layout("desk", arranged));
        app.show_shared().unwrap();
        let id = says_hello(&mut app, &reinstalled, "desk");
        let value = app.request(Request::Snapshot).unwrap();
        assert_eq!(value["unplaced"][0]["state"], "duplicate_name");

        // Its corner a little off the old tile's, so its middle is on it.
        let request = Request::Place {
            id,
            x: 2000,
            y: 100,
            tolerance: 0,
        };
        app.request(request).unwrap();
        let saved = Config::load(&directory.path().join("zflow.toml")).unwrap();
        assert_eq!(saved.peers.len(), 1);
        let record = &saved.peers["desk"];
        assert_eq!(record.spki_der().unwrap(), reinstalled.spki());
        assert_eq!(record.keyboard, crate::core::KeyboardMode::PcPositions);
        let kept = app.shared.draft.layout.clone().unwrap();
        assert_eq!(kept.tiles[1], tile(&reinstalled.fingerprint_hex(), 1920));
        assert_eq!((kept.version, &kept.editor), (4, &own));
        let shown = app
            .layout
            .draft
            .monitors
            .iter()
            .find(|m| m.id == "peer:desk");
        assert_eq!(shown.map(|m| m.x), Some(1920));
    }

    #[test]
    fn a_fresh_mac_lets_the_lone_computer_around_join_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let mut app = NativeApp::open(path.clone()).unwrap();
        // Nothing listens on the real port while this runs.
        app.document.draft.macos.sharing = false;
        app.save_config().unwrap();
        let start = tokio::time::Instant::now();
        let window = |app: &NativeApp| app.snapshot().pairing_window;
        // It waits for Local Network access before it opens.
        app.tick_window(start);
        assert_eq!(window(&app).state, crate::pairing_window::State::Eligible);
        app.discover = true;
        app.local_network = LocalNetwork::Allowed;
        let desk = Identity::load_or_create(&directory.path().join("desk")).unwrap();
        // A hello that only came in, maybe from off the network, is not
        // enough to join.
        says_hello(&mut app, &desk, "desk");
        app.tick_window(start);
        let open = window(&app);
        assert_eq!(open.state, crate::pairing_window::State::Open);
        assert_eq!(open.holding, None);
        answers_hello(&mut app, &desk, "desk");
        app.tick_window(start);
        assert_eq!(window(&app).holding.unwrap().name, "desk");
        app.tick_window(start + crate::pairing_window::HOLD_OFF);
        assert!(Config::load(&path).unwrap().peers.contains_key("desk"));
        assert_eq!(app.notices.len(), 1);
        assert_eq!(
            window(&app).reason,
            Some(crate::pairing_window::Reason::Accepted)
        );
        drop(app);

        // Once only, even across a restart.
        let app = NativeApp::open(path).unwrap();
        assert_eq!(window(&app).state, crate::pairing_window::State::Closed);
    }

    #[test]
    fn a_connection_turned_away_closes_a_fresh_macs_window() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let mut app = NativeApp::open(path).unwrap();
        app.document.draft.macos.sharing = false;
        app.save_config().unwrap();
        (app.discover, app.local_network) = (true, LocalNetwork::Allowed);
        let desk = Identity::load_or_create(&directory.path().join("desk")).unwrap();
        answers_hello(&mut app, &desk, "desk");
        let start = tokio::time::Instant::now();
        app.tick_window(start);
        assert_eq!(app.snapshot().pairing_window.holding.unwrap().name, "desk");
        // Too many handshakes at once: the one refused could be a rival.
        app.hear(Heard::TurnedAway, start);
        app.tick_window(start + crate::pairing_window::HOLD_OFF);
        let window = app.snapshot().pairing_window;
        assert_eq!(window.reason, Some(crate::pairing_window::Reason::Rival));
        assert!(app.document.saved().peers.is_empty());
    }

    #[test]
    fn looking_for_computers_is_remembered_across_restarts() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = mac_with(directory.path(), &[]);
        assert!(!app.discover);
        app.request(Request::Discover).unwrap();
        drop(app);
        let app = NativeApp::open(directory.path().join("zflow.toml")).unwrap();
        assert!(app.discover);
        // Discovery is off in this configuration, so nothing goes out.
        assert!(!app.discovers() && app.links.listen_port().is_none());
    }

    #[test]
    fn only_a_sessions_address_is_saved_and_it_changes_nothing_else() {
        let directory = tempfile::tempdir().unwrap();
        let desk = Identity::load_or_create(&directory.path().join("desk")).unwrap();
        let mut app = mac_with(directory.path(), &[("desk", &desk)]);
        let path = directory.path().join("zflow.toml");
        let saved = || Config::load(&path).unwrap().peers["desk"].addresses.clone();
        let armed_at = app.retry_at;
        let now = tokio::time::Instant::now();
        // Its hello names addresses, which are only its word.
        let claimed = vec!["198.51.100.9:43200".parse().unwrap()];
        let hello = hello::make_hello("desk", 43200, claimed, true);
        let heard = Heard::Hello {
            instance: None,
            spki: desk.spki().to_vec(),
            remote: "192.0.2.7:50000".parse().unwrap(),
            hello: Box::new(hello),
        };
        app.hear(heard, now);
        assert!(saved().is_empty());

        // A session this Mac dialed saves the address that answered.
        let key = app.document.saved().peers["desk"].spki_der_hex.clone();
        let connected = |address: &str, dialed| Heard::Connected {
            key: key.clone(),
            address: address.parse().unwrap(),
            dialed,
        };
        app.hear(connected("192.0.2.70:43119", true), now);
        let moved: SocketAddr = "192.0.2.70:43119".parse().unwrap();
        assert_eq!(saved(), [moved]);
        // One it opened from a passing port gets the port its hello named.
        app.hear(connected("[::ffff:192.0.2.7]:50000", false), now);
        let inbound: SocketAddr = "192.0.2.7:43200".parse().unwrap();
        assert_eq!(saved(), [inbound, moved]);
        // Another computer's key teaches nothing.
        let other = Heard::Connected {
            key: "01".into(),
            address: "192.0.2.9:43119".parse().unwrap(),
            dialed: true,
        };
        app.hear(other, now);
        assert_eq!(saved(), [inbound, moved]);
        // Nor does an edit of the file that only moves it.
        let mut config = Config::load(&path).unwrap();
        config.peers.get_mut("desk").unwrap().addresses = vec!["192.0.2.71:43119".parse().unwrap()];
        config.save(&path).unwrap();
        app.reload();
        assert_eq!(app.document.saved(), &config);
        assert_eq!(app.retry_at, armed_at, "sharing did not restart");
    }

    #[test]
    fn computers_are_added_by_ip_address_with_an_optional_port() {
        let parse = |input| input_address(input).map(|address| address.to_string());
        assert_eq!(parse(" 100.64.0.7 ").unwrap(), "100.64.0.7:43119");
        assert_eq!(parse("[2001:db8::1]").unwrap(), "[2001:db8::1]:43119");
        assert_eq!(parse("100.64.0.7:5000").unwrap(), "100.64.0.7:5000");
        for wrong in ["desk.local", "100.64.0.7:0", "0.0.0.0", ""] {
            assert!(parse(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn links_become_peer_states_and_one_health_row() {
        let mut config = Config::default();
        for name in ["busy", "desk", "down", "new", "off"] {
            config.peers.insert(
                name.into(),
                crate::config::PeerConfig {
                    spki_der_hex: "01".into(),
                    addresses: Vec::new(),
                    permissions: crate::config::PeerPermissions {
                        connect: true,
                        send_normal: false,
                        receive_normal: name != "off",
                        inject_prelogin: false,
                    },
                    keyboard: crate::core::KeyboardMode::Standard,
                    reverse_scroll: false,
                },
            );
        }
        let ready = || {
            LinkState::Ready(crate::desktop::Geometry {
                monitors: vec![crate::desktop::Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                }],
            })
        };
        let links = [
            ("busy", ready()),
            ("desk", ready()),
            ("down", LinkState::Down("connection refused".into())),
            ("new", LinkState::Connecting),
        ];
        let peers = peer_rows(&config, links.into_iter(), Some("busy"), None);
        let rows: Vec<_> = peers
            .iter()
            .map(|peer| (peer.name.as_str(), peer.state, peer.detail.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                (
                    "busy",
                    PeerState::ControlledFromHere,
                    "Controlled from here"
                ),
                ("desk", PeerState::Connected, "Connected"),
                ("down", PeerState::Unreachable, "connection refused"),
                ("new", PeerState::Connecting, "Connecting…"),
                ("off", PeerState::Paired, "Paired"),
            ]
        );
        let row = link_health(&peers, &[]).unwrap();
        assert_eq!(
            (row.level, row.detail.as_str()),
            (Level::Error, "down: connection refused")
        );
        assert_eq!(row.action.as_ref().unwrap().command, "retry");
        let status = Status::new(Some(true), &peers, &[row], false);
        assert_eq!(status.title, "Controlling busy");
        let fine = &peers[..2];
        assert_eq!(link_health(fine, &[]).unwrap().level, Level::Ok);
        assert!(link_health(&[], &[]).is_none());
        // Sharing is on, but no computer takes input from this Mac.
        let unlinked = link_health(&peers[4..], &[]).unwrap();
        assert_eq!(unlinked.level, Level::Error);
        assert!(unlinked.action.is_none());

        // One peer controls this Mac, and another only may.
        let links = [("busy", ready()), ("desk", LinkState::Connected)];
        let peers = peer_rows(&config, links.into_iter(), None, Some("busy"));
        let states: Vec<_> = peers.iter().map(|peer| peer.state).collect();
        assert_eq!(
            states[..2],
            [PeerState::ControllingThis, PeerState::Connected]
        );
        let status = Status::new(Some(true), &peers[..2], &[], false);
        assert_eq!(status.title, "Controlled by busy");

        // A computer that takes no input from this Mac can still control it.
        let links = [
            ("busy", LinkState::Refused("sending".into())),
            ("desk", LinkState::Refused("locked".into())),
        ];
        let refused = refusals(links.clone().into_iter(), Some("busy"));
        assert_eq!(refused, ["desk takes no input from this Mac: locked"]);
        let peers = peer_rows(&config, links.into_iter(), None, Some("busy"));
        assert_eq!(peers[1].state, PeerState::Connected);
        let row = link_health(&peers[..2], &refused).unwrap();
        assert_eq!(row.level, Level::Warning);
        assert_eq!(row.action.as_ref().unwrap().command, "retry");
        let status = Status::new(Some(true), &peers[1..2], &[row], false);
        assert_eq!(status.title, "Ready");
    }
}
