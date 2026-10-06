use super::{clipboard, desktop, input, ipc};
use crate::{
    app::{handoff, layout_model::Layout, model::ConfigDocument},
    config::Config,
    core::{
        ActivationId, InputCapability, ReceiverEffect, SessionCloseReason, SessionContext,
        SessionEpoch, TransportGeneration,
    },
    desktop::{DesktopRequest, DesktopResponse, Geometry, SharedLayout, Tile},
    discovery::{Advertisement, Discovery, DiscoveryEvent},
    hello::{local_hello, trust_peer},
    identity::Identity,
    neighbors::Neighbors,
    session::{SessionEvent, SessionEventKind, SessionHandle, SessionOptions, start_session},
    transport::{self, Accepted, InputConnection, InputServerConfig},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

pub fn setup(path: &Path) -> Result<Config> {
    if path.exists() {
        return Ok(Config::load(path)?);
    }
    let mut config = Config::default();
    config.daemon.state_dir = path
        .parent()
        .context("Configuration needs a parent directory")?
        .join("state");
    config.daemon.control_socket = PathBuf::from(ipc::pipe_name(path)?);
    config.save(path)?;
    Identity::load_or_create(&config.daemon.state_dir)?;
    Ok(config)
}

enum Network {
    Accepted(Accepted, OwnedSemaphorePermit),
    Hello {
        spki: Vec<u8>,
        remote: SocketAddr,
        hello: Box<crate::wire::Hello>,
        instance: Option<String>,
        reply: Option<oneshot::Sender<Value>>,
    },
    Opened {
        peer: String,
        handle: SessionHandle,
        dialed: bool,
        spki: Vec<u8>,
        events: mpsc::Receiver<SessionEvent>,
    },
    Failed {
        peer: String,
        reason: String,
    },
    ProbeFailed {
        instance: Option<String>,
        reason: String,
        reply: Option<oneshot::Sender<Value>>,
    },
    Prepared {
        peer: String,
        session: u64,
        token: u64,
        handoff: Option<handoff::Handoff>,
        result: Result<DesktopResponse>,
    },
    Polled {
        peer: String,
        session: u64,
        result: Result<DesktopResponse>,
    },
    Discovery(DiscoveryEvent),
}
struct Link {
    handle: SessionHandle,
    dialed: bool,
}
struct Outbound {
    peer: String,
    session: u64,
    token: u64,
    handoff: Option<handoff::Handoff>,
    polling: bool,
    next_poll: Instant,
}
struct Engine {
    config: Config,
    document: ConfigDocument,
    identity: Identity,
    server: InputServerConfig,
    endpoint: quinn::Endpoint,
    neighbors: Neighbors,
    layout: SharedLayout,
    links: BTreeMap<String, Link>,
    connecting: BTreeSet<String>,
    failures: BTreeMap<String, (u32, Instant, String)>,
    network: mpsc::Sender<Network>,
    events: mpsc::Sender<SessionEvent>,
    permits: Arc<Semaphore>,
    injector: input::Injector,
    inbound: Option<(String, u64)>,
    lease: Option<desktop::Lease>,
    outbound: Option<Outbound>,
    arming: Option<(String, u64)>,
    geometry: Geometry,
    desktop_available: bool,
    boundaries: Vec<input::Boundary>,
    previous: crate::desktop::Point,
    next_geometry: Instant,
    next_discovery: Instant,
    edge_since: Option<(handoff::Handoff, Instant)>,
    pending_activation: Option<(String, Instant)>,
    clip_echo: BTreeMap<String, crate::clipboard::Echo>,
    notice: Option<String>,
    generation: u64,
}

pub async fn run(path: PathBuf) -> Result<()> {
    let path = std::path::absolute(path)?;
    let config = setup(&path)?;
    let document = ConfigDocument::open(path.clone())?;
    ensure!(
        !config.input.experimental_touchpad,
        "Windows does not support experimental raw touchpad forwarding"
    );
    ensure!(
        !config.input.allow_prelogin_input,
        "Windows input sharing runs only in an unlocked desktop session"
    );
    let identity = Identity::load_or_create(&config.daemon.state_dir)?;
    let server = server_config(&identity, &config)?;
    // Quinn's server helper uses the OS's IPv6-only default on Windows.
    // Its client helper explicitly enables dual stack; install our listener
    // on that same socket so Tailscale's IPv4 addresses work in both directions.
    let endpoint = quinn::Endpoint::client(config.transport.listen)
        .context("Cannot listen on the zflow UDP port")?;
    endpoint.set_server_config(Some(server.quinn_config()));
    let (mut commands, ipc_task) = ipc::listen(&path)?;
    let (_capture, mut captured) = input::Capture::start().await?;
    let (network, mut net) = mpsc::channel(128);
    let (events, mut received) = mpsc::channel(512);
    let geometry = input::geometry()?;
    let bounds = geometry.bounds()?;
    let key = identity.fingerprint_hex();
    let layout_path = config.daemon.state_dir.join("layout.json");
    let layout = if layout_path.exists() {
        let l: SharedLayout = serde_json::from_slice(&std::fs::read(&layout_path)?)?;
        l.validate()?;
        l
    } else {
        SharedLayout {
            version: 1,
            editor: key.clone(),
            tiles: vec![Tile {
                display: None,
                key,
                x: 0,
                y: 0,
                width: bounds.width,
                height: bounds.height,
            }],
        }
    };
    let mut engine = Engine {
        neighbors: Neighbors::new(identity.spki()),
        config,
        document,
        identity,
        server,
        endpoint,
        layout,
        links: BTreeMap::new(),
        connecting: BTreeSet::new(),
        failures: BTreeMap::new(),
        network: network.clone(),
        events,
        permits: Arc::new(Semaphore::new(8)),
        injector: input::Injector::new(),
        inbound: None,
        lease: None,
        outbound: None,
        arming: None,
        geometry,
        desktop_available: input::available(),
        boundaries: Vec::new(),
        previous: input::cursor()?,
        next_geometry: Instant::now(),
        next_discovery: Instant::now(),
        edge_since: None,
        pending_activation: None,
        clip_echo: BTreeMap::new(),
        notice: None,
        generation: 0,
    };
    let discovery = engine.discovery();
    let mut tick = tokio::time::interval(Duration::from_millis(8));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tracing::info!(name=%crate::hello::local_name(),address=%engine.endpoint.local_addr()?,"Windows input engine ready");
    loop {
        tokio::select! {
            _=tokio::signal::ctrl_c()=>break,
            request=commands.recv()=>{
                let Some(request)=request else {break};
                if matches!(request.command,ipc::Command::Quit){engine.local();let _=request.reply.send(json!({"ok":true}));break;}
                engine.command(request).await;
            },
            incoming=engine.endpoint.accept()=>if let Some(incoming)=incoming {
                if let Ok(permit)=engine.permits.clone().try_acquire_owned(){
                    let tx=network.clone();let server=engine.server.clone();
                    tokio::spawn(async move {
                        if let Ok(Ok(accepted))=tokio::time::timeout(Duration::from_secs(6),transport::accept(incoming,&server)).await {let _=tx.send(Network::Accepted(accepted,permit)).await;}
                    });
                }else{incoming.refuse();}
            },
            Some(message)=net.recv()=>{if let Err(error)=engine.network(message).await {engine.notice=Some(format!("{error:#}"));}},
            Some(event)=received.recv()=>engine.session(event),
            capture=captured.recv()=>match capture {
                Some(input::Event::Escape)=>{engine.local();engine.config.daemon.sharing=false;engine.save_config()?;},
                Some(input::Event::Activate)=>{
                    if engine.outbound.is_some(){engine.local();}else if let Some(peer)=engine.links.keys().next().cloned(){engine.pending_activation=Some((peer,Instant::now()+Duration::from_secs(3)));}
                },
                Some(input::Event::Frame(frame))=>if let Some(out)=&engine.outbound
                    && let Some(link)=engine.links.get(&out.peer) && link.handle.capture(crate::capture::CapturedDeviceFrame{device_path:"windows".into(),frame,captured_at:Instant::now()}).is_err(){engine.local();},
                Some(input::Event::EdgeHit(point, edge))=>if let Err(error)=engine.edge_hit(point, edge){engine.local();engine.notice=Some(format!("{error:#}"));},
                Some(input::Event::Stopped)|None=>{engine.local();bail!("Windows input capture stopped");},
                _=>{},
            },
            _=tick.tick()=>{if let Err(error)=engine.tick(){engine.local();engine.notice=Some(format!("{error:#}"));}},
        }
        engine.refresh_boundaries();
    }
    engine.local();
    engine.endpoint.close(0u32.into(), b"Windows app stopped");
    if let Some(task) = discovery {
        task.abort();
    }
    // Give the Quit reply and final releases a chance to leave before the runtime stops.
    tokio::time::sleep(Duration::from_millis(100)).await;
    ipc_task.abort();
    Ok(())
}
fn server_config(identity: &Identity, config: &Config) -> Result<InputServerConfig> {
    let keys: Result<Vec<_>, _> = config
        .peers
        .values()
        .filter(|p| p.permissions.connect)
        .map(|p| p.spki_der())
        .collect();
    Ok(transport::input_server_config_for_peers(identity, keys?)?)
}

impl Engine {
    fn keys(&self) -> BTreeMap<String, String> {
        self.config
            .peers
            .iter()
            .filter_map(|(n, p)| Some((n.clone(), p.fingerprint_hex().ok()?)))
            .collect()
    }
    fn local_layout(&self) -> Layout {
        Layout::from_shared(
            &self.layout,
            &self.identity.fingerprint_hex(),
            "This computer",
            &self.keys(),
        )
    }
    fn snapshot(&self) -> Value {
        let peers:Vec<_>=self.config.peers.iter().map(|(name,p)|json!({"name":name,"key":p.fingerprint_hex().ok(),"mark":p.spki_der().ok().map(|s|crate::neighbors::mark(&s)),
            "connected":self.links.get(name).is_some_and(|l|!l.handle.is_closed()),"status":self.failures.get(name).map(|(_,_,reason)|reason),"keyboard":p.keyboard,"reverse_scroll":p.reverse_scroll,"addresses":p.addresses})).collect();
        json!({"name":crate::hello::local_name(),"key":self.identity.fingerprint_hex(),"mark":crate::neighbors::mark(self.identity.spki()),
            "pid":std::process::id(),"elevated":input::elevated(),
            "sharing":self.config.daemon.sharing,"clipboard":self.config.clipboard.share,"available":input::available(),"peers":peers,
            "nearby":self.neighbors.unplaced(&self.config),"layout":self.local_layout(),"sending":self.outbound.as_ref().map(|o|&o.peer),"receiving":self.inbound.as_ref().map(|(p,_)|p),
            "notice":self.notice,"layout_version":self.layout.version,"pause_at_edges":self.config.switching.pause_at_edges,"listen":self.config.transport.listen,"addresses":crate::discovery::this_host_addresses()})
    }
    fn save_config(&mut self) -> Result<()> {
        self.document.draft = self.config.clone();
        if let Err(error) = self.document.save() {
            self.config = self.document.saved().clone();
            return Err(error);
        }
        self.server = server_config(&self.identity, &self.config)?;
        self.endpoint
            .set_server_config(Some(self.server.quinn_config()));
        Ok(())
    }
    fn save_layout(&mut self) -> Result<()> {
        self.layout.validate()?;
        crate::config::save_text(
            &self.config.daemon.state_dir.join("layout.json"),
            &serde_json::to_string_pretty(&self.layout)?,
        )?;
        for link in self.links.values() {
            let _ = link.handle.send_layout(self.layout.clone());
        }
        Ok(())
    }
    async fn command(&mut self, request: ipc::Request) {
        let ipc::Request { command, reply } = request;
        if let ipc::Command::Nearby { address } = command {
            let Ok(permit) = self.permits.clone().try_acquire_owned() else {
                let _ = reply.send(json!({"error":"Discovery is busy. Try again shortly."}));
                return;
            };
            let tx = self.network.clone();
            let endpoint = self.endpoint.clone();
            let config = transport::hello_client_config(&self.identity);
            let hello = local_hello(self.config.transport.listen.port(), false);
            tokio::spawn(async move {
                let _permit = permit;
                let result = async {
                    let address = resolve(&address).await?;
                    let connection = transport::connect_hello(&endpoint, address, &config?).await?;
                    let spki = connection.peer_spki().to_vec();
                    let remote = connection.remote_address();
                    let hello = connection.exchange(&hello).await?;
                    Ok::<_, anyhow::Error>((spki, remote, hello))
                };
                match tokio::time::timeout(Duration::from_secs(8), result).await {
                    Ok(Ok((spki, remote, hello))) => {
                        let _ = tx
                            .send(Network::Hello {
                                spki,
                                remote,
                                hello: Box::new(hello),
                                instance: None,
                                reply: Some(reply),
                            })
                            .await;
                    }
                    result => {
                        let reason=match result {Ok(Err(e))=>crate::link::reason(&e).to_string(),_=>"No zflow answer. Check the address, the other app, and UDP port 43119 in its firewall.".into()};
                        let _ = reply.send(json!({"error":reason}));
                    }
                }
            });
            return;
        }
        let result = (|| -> Result<Value> {
            match command {
                ipc::Command::Status => {}
                ipc::Command::Trust { key } => {
                    let unplaced = self.neighbors.unplaced(&self.config);
                    let matches: Vec<_> = unplaced
                        .iter()
                        .filter(|p| {
                            p.id == format!("key:{key}")
                                || p.id == key
                                || p.mark.as_deref() == Some(key.as_str())
                        })
                        .collect();
                    ensure!(
                        matches.len() == 1,
                        "Choose one computer's full fingerprint or unique mark from Nearby"
                    );
                    let neighbor = self
                        .neighbors
                        .neighbor(
                            matches[0]
                                .id
                                .strip_prefix("key:")
                                .context("Wait for the computer to identify itself")?,
                        )
                        .context("Computer is no longer nearby")?;
                    let key = crate::neighbors::fingerprint(&neighbor.spki);
                    ensure!(
                        self.neighbors
                            .strangers(&self.config, Instant::now().into())
                            .present
                            .iter()
                            .any(|s| s.key == key && s.fresh),
                        "Say hello again before trusting this computer"
                    );
                    trust_peer(
                        &mut self.config,
                        &neighbor.spki,
                        &neighbor.name,
                        &neighbor.addresses,
                    )?;
                    self.save_config()?;
                    if let Some(layout) = self.layout.with_tiles_for(
                        &self.identity.fingerprint_hex(),
                        self.keys().values().map(String::as_str),
                        (1920, 1080),
                    ) {
                        self.layout = layout;
                        self.save_layout()?;
                    }
                }
                ipc::Command::Forget { peer } => {
                    self.local();
                    if let Some(link) = self.links.remove(&peer) {
                        link.handle.close(SessionCloseReason::PermissionRevoked);
                    }
                    self.config.peers.remove(&peer);
                    self.failures.remove(&peer);
                    self.save_config()?;
                }
                ipc::Command::Sharing { enabled } => {
                    if !enabled {
                        self.local();
                        for (_, link) in std::mem::take(&mut self.links) {
                            link.handle.close(SessionCloseReason::LocalRelease);
                        }
                    }
                    self.config.daemon.sharing = enabled;
                    self.save_config()?;
                }
                ipc::Command::Clipboard { enabled } => {
                    self.config.clipboard.share = enabled;
                    self.save_config()?;
                }
                ipc::Command::PauseAtEdges { enabled } => {
                    self.config.switching.pause_at_edges = enabled;
                    self.save_config()?;
                }
                ipc::Command::Keyboard { peer, mode } => {
                    self.config
                        .peers
                        .get_mut(&peer)
                        .context("Unknown computer")?
                        .keyboard = mode;
                    self.save_config()?;
                }
                ipc::Command::ReverseScroll { peer, enabled } => {
                    self.config
                        .peers
                        .get_mut(&peer)
                        .context("Unknown computer")?
                        .reverse_scroll = enabled;
                    self.save_config()?;
                }
                ipc::Command::Arrange { layout, version } => {
                    ensure!(
                        version == self.layout.version,
                        "The arrangement changed on another computer. Try your move again."
                    );
                    layout.validate()?;
                    ensure!(
                        layout
                            .monitors
                            .iter()
                            .any(|m| m.peer.is_none() && m.active()),
                        "Arrangement needs an active local monitor"
                    );
                    ensure!(
                        layout.monitors.iter().all(|m| m
                            .peer
                            .as_ref()
                            .is_none_or(|p| self.config.peers.contains_key(p))),
                        "Only paired computers can be arranged"
                    );
                    self.local();
                    let own = self.identity.fingerprint_hex();
                    let keys = self.keys();
                    self.layout = layout
                        .to_shared(self.layout.version + 1, &own, &keys)
                        .with_others_from(&self.layout, |key| {
                            key == own || keys.values().any(|known| known == key)
                        });
                    self.save_layout()?;
                }
                ipc::Command::Activate { peer } => {
                    ensure!(self.links.contains_key(&peer), "Computer is not connected");
                    ensure!(self.config.daemon.sharing, "Turn Sharing on first");
                    ensure!(
                        self.config
                            .peers
                            .get(&peer)
                            .is_some_and(|p| p.permissions.receive_normal),
                        "This computer does not have permission to receive input"
                    );
                    ensure!(
                        self.inbound.is_none() && self.lease.is_none(),
                        "Return input on the other computer first"
                    );
                    self.pending_activation = Some((peer, Instant::now() + Duration::from_secs(3)));
                }
                ipc::Command::Local => self.local(),
                ipc::Command::Quit | ipc::Command::Nearby { .. } => unreachable!(),
            }
            Ok(self.snapshot())
        })();
        let _ = reply.send(result.unwrap_or_else(|e| json!({"error":format!("{e:#}")})));
    }
    fn discovery(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        if !self.config.transport.discovery {
            return None;
        }
        let result = (|| -> Result<Discovery> {
            let mut d = Discovery::new()?;
            d.register(
                Advertisement::new(
                    self.config.transport.listen.port(),
                    [
                        InputCapability::Keyboard,
                        InputCapability::Pointer,
                        InputCapability::Scroll,
                    ],
                )?
                .with_name(&crate::hello::local_name()),
            )?;
            d.browse()?;
            Ok(d)
        })();
        match result {
            Ok(d) => {
                self.neighbors.own_record(Some(d.instance_id().to_string()));
                let tx = self.network.clone();
                Some(tokio::spawn(async move {
                    while let Ok(event) = d.next_event().await {
                        if tx.send(Network::Discovery(event)).await.is_err() {
                            break;
                        }
                    }
                }))
            }
            Err(e) => {
                self.notice = Some(format!("Network discovery unavailable: {e}"));
                None
            }
        }
    }
    fn probe(&self, instance: Option<String>, addresses: Vec<SocketAddr>) {
        let Ok(permit) = self.permits.clone().try_acquire_owned() else {
            return;
        };
        let Ok(config) = transport::hello_client_config(&self.identity) else {
            return;
        };
        let endpoint = self.endpoint.clone();
        let tx = self.network.clone();
        let port = self.config.transport.listen.port();
        let trusted: Vec<_> = self
            .config
            .peers
            .values()
            .filter_map(|p| p.spki_der().ok())
            .collect();
        tokio::spawn(async move {
            let _permit = permit;
            let result = tokio::time::timeout(Duration::from_secs(6), async {
                for address in addresses {
                    if let Ok(Ok(c)) = tokio::time::timeout(
                        Duration::from_millis(1500),
                        transport::connect_hello(&endpoint, address, &config),
                    )
                    .await
                    {
                        let spki = c.peer_spki().to_vec();
                        let remote = c.remote_address();
                        let local = local_hello(port, trusted.contains(&spki));
                        if let Ok(hello) = c.exchange(&local).await {
                            return Some((spki, remote, hello));
                        }
                    }
                }
                None
            })
            .await;
            let message = match result {
                Ok(Some((spki, remote, hello))) => Network::Hello {
                    spki,
                    remote,
                    hello: Box::new(hello),
                    instance,
                    reply: None,
                },
                _ => Network::ProbeFailed {
                    instance,
                    reason: "No hello answer".into(),
                    reply: None,
                },
            };
            let _ = tx.send(message).await;
        });
    }
    fn open(
        &mut self,
        peer: String,
        connection: InputConnection,
        dialed: bool,
        permit: OwnedSemaphorePermit,
    ) -> Result<()> {
        let spki = connection.peer_spki().to_vec();
        self.generation += 1;
        let generation = TransportGeneration(self.generation);
        let options = SessionOptions::from_config(&self.config)?;
        let (events, session_events) = mpsc::channel(512);
        let tx = self.network.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let message =
                match start_session(connection, peer.clone(), generation, options, events).await {
                    Ok(handle) => Network::Opened {
                        peer,
                        handle,
                        dialed,
                        spki,
                        events: session_events,
                    },
                    Err(e) => Network::Failed {
                        peer,
                        reason: crate::link::reason(&e),
                    },
                };
            let _ = tx.send(message).await;
        });
        Ok(())
    }
    fn dial(&mut self, peer: String) {
        let record = &self.config.peers[&peer];
        let Ok(spki) = record.spki_der() else {
            return;
        };
        let Ok(config) = transport::input_client_config(&self.identity, &spki) else {
            return;
        };
        let mut addresses = self
            .neighbors
            .addresses_for_key(&record.fingerprint_hex().unwrap_or_default());
        addresses.extend(record.addresses.clone());
        self.connecting.insert(peer.clone());
        self.generation += 1;
        let generation = TransportGeneration(self.generation);
        let endpoint = self.endpoint.clone();
        let tx = self.network.clone();
        let (events, session_events) = mpsc::channel(512);
        let options = SessionOptions::from_config(&self.config);
        tokio::spawn(async move {
            let result = tokio::time::timeout(Duration::from_secs(8), async {
                let mut error = anyhow::anyhow!("No saved address; use Add by address");
                for address in addresses {
                    match tokio::time::timeout(
                        Duration::from_secs(2),
                        transport::connect_input(&endpoint, address, &config),
                    )
                    .await
                    {
                        Ok(Ok(connection)) => {
                            return start_session(
                                connection,
                                peer.clone(),
                                generation,
                                options?,
                                events,
                            )
                            .await;
                        }
                        Ok(Err(e)) => error = e.into(),
                        Err(e) => error = e.into(),
                    }
                }
                Err(error)
            })
            .await;
            let message = match result {
                Ok(Ok(handle)) => Network::Opened {
                    peer,
                    handle,
                    dialed: true,
                    spki,
                    events: session_events,
                },
                Ok(Err(e)) => Network::Failed {
                    peer,
                    reason: crate::link::reason(&e),
                },
                Err(_) => Network::Failed {
                    peer,
                    reason: "Connection timed out".into(),
                },
            };
            let _ = tx.send(message).await;
        });
    }
    async fn network(&mut self, message: Network) -> Result<()> {
        match message {
            Network::Accepted(Accepted::Hello(c), permit) => {
                let spki = c.peer_spki().to_vec();
                let remote = c.remote_address();
                if !self.neighbors.allow_hello(
                    remote.ip(),
                    &crate::discovery::this_host_addresses(),
                    Instant::now().into(),
                ) {
                    c.close();
                    return Ok(());
                }
                let trusts = self
                    .config
                    .peers
                    .values()
                    .any(|p| p.permissions.connect && p.spki_der().is_ok_and(|k| k == spki));
                let hello = local_hello(self.config.transport.listen.port(), trusts);
                let tx = self.network.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Ok(Ok(hello)) =
                        tokio::time::timeout(Duration::from_secs(5), c.exchange(&hello)).await
                    {
                        let _ = tx
                            .send(Network::Hello {
                                spki,
                                remote,
                                hello: Box::new(hello),
                                instance: None,
                                reply: None,
                            })
                            .await;
                    }
                });
            }
            Network::Accepted(Accepted::Input(c), permit) => {
                let peer = self
                    .config
                    .peers
                    .iter()
                    .find(|(_, p)| {
                        p.permissions.connect && p.spki_der().is_ok_and(|k| k == c.peer_spki())
                    })
                    .map(|(n, _)| n.clone());
                if self.config.daemon.sharing
                    && let Some(peer) = peer
                {
                    self.open(peer, c, false, permit)?;
                } else {
                    c.close();
                }
            }
            Network::Accepted(Accepted::NotTrusted { .. }, _) => {}
            Network::Hello {
                spki,
                remote,
                hello,
                instance,
                reply,
            } => {
                self.neighbors.hello(
                    &spki,
                    remote,
                    &hello,
                    instance.as_deref(),
                    Instant::now().into(),
                );
                if let Some((_, p)) = self
                    .config
                    .peers
                    .iter_mut()
                    .find(|(_, p)| p.spki_der().is_ok_and(|k| k == spki))
                    && p.learn_addresses(crate::hello::hello_addresses(remote, &hello))
                {
                    self.save_config()?;
                }
                if let Some(reply) = reply {
                    let _ = reply.send(self.snapshot());
                }
            }
            Network::ProbeFailed {
                instance,
                reason,
                reply,
            } => {
                if let Some(i) = instance {
                    self.neighbors.no_answer(&i, Instant::now().into());
                }
                if let Some(reply) = reply {
                    let _ = reply.send(json!({"error":reason}));
                }
            }
            Network::Opened {
                peer,
                handle,
                dialed,
                spki,
                mut events,
            } => {
                self.connecting.remove(&peer);
                let Some(record) = self.config.peers.get(&peer) else {
                    handle.close(SessionCloseReason::PermissionRevoked);
                    return Ok(());
                };
                if !session_allowed(self.config.daemon.sharing, Some(record), &spki) {
                    handle.close(SessionCloseReason::PermissionRevoked);
                    return Ok(());
                }
                if let Some(old) = self.links.get(&peer) {
                    let prefer_dial = crate::identity::wins_simultaneous_dial(
                        &self.identity.fingerprint_hex(),
                        &record.fingerprint_hex()?,
                    );
                    if !old.handle.is_closed()
                        && (old.dialed == prefer_dial || dialed != prefer_dial)
                    {
                        handle.close(SessionCloseReason::Superseded);
                        return Ok(());
                    }
                    old.handle.close(SessionCloseReason::Superseded);
                    if self.inbound.as_ref().is_some_and(|(p, _)| p == &peer)
                        || self.outbound.as_ref().is_some_and(|o| o.peer == peer)
                        || self.lease.as_ref().is_some_and(|l| l.peer == peer)
                        || self.arming.as_ref().is_some_and(|(p, _)| p == &peer)
                    {
                        self.local();
                    }
                }
                self.failures.remove(&peer);
                let _ = handle.send_layout(self.layout.clone());
                let h = handle.clone();
                let tx = self.network.clone();
                let name = peer.clone();
                // A snapshot learns the receiver's actual dimensions before any crossing.
                tokio::spawn(async move {
                    let result = h.desktop_request(DesktopRequest::Snapshot).await;
                    let _ = tx
                        .send(Network::Prepared {
                            peer: name,
                            session: h.id(),
                            token: 0,
                            handoff: None,
                            result,
                        })
                        .await;
                });
                self.links.insert(peer, Link { handle, dialed });
                let tx = self.events.clone();
                tokio::spawn(async move {
                    while let Some(event) = events.recv().await {
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                });
            }
            Network::Failed { peer, reason } => {
                self.connecting.remove(&peer);
                if !self.links.contains_key(&peer) {
                    let failures = self.failures.get(&peer).map_or(1, |(n, _, _)| n + 1);
                    self.failures.insert(
                        peer,
                        (
                            failures,
                            Instant::now() + crate::link::retry_delay(failures),
                            reason,
                        ),
                    );
                }
            }
            Network::Discovery(DiscoveryEvent::Candidate(c)) => {
                if let Some(id) = c.ephemeral_instance_id() {
                    self.neighbors.instance_seen(
                        &id.to_string(),
                        crate::discovery::remote_addresses(
                            c.socket_addresses(),
                            &crate::discovery::this_host_addresses(),
                        ),
                        c.is_compatible(),
                        c.name().map(str::to_owned),
                    );
                }
            }
            Network::Discovery(DiscoveryEvent::Removed(id)) => self
                .neighbors
                .instance_gone(&id.to_string(), Instant::now().into()),
            Network::Discovery(DiscoveryEvent::Stopped) => {
                self.notice = Some("Network discovery stopped; Add by address still works".into())
            }
            Network::Prepared {
                peer,
                session,
                token,
                handoff,
                result,
            } => {
                if !self
                    .links
                    .get(&peer)
                    .is_some_and(|l| l.handle.id() == session)
                {
                    return Ok(());
                }
                if token == 0 {
                    if let Ok(DesktopResponse::Snapshot { geometry, .. }) = result {
                        let key = self.config.peers[&peer].fingerprint_hex()?;
                        if let Some(layout) = self.layout.with_geometry(&key, &geometry) {
                            self.layout = layout;
                            self.save_layout()?;
                        }
                    }
                    return Ok(());
                }
                if self.arming.as_ref() != Some(&(peer.clone(), session)) {
                    self.links[&peer]
                        .handle
                        .close(SessionCloseReason::LocalRelease);
                    return Ok(());
                }
                // Keep arming reserved until all validation succeeds. Any error
                // cancels the remote lease by closing this session below.
                let checked = (|| -> Result<()> {
                    let response = result?;
                    if let Some(h) = &handoff {
                        h.check_prepared(response)?;
                        ensure!(
                            h.matches_geometry(&input::geometry()?)
                                && h.entry_region.contains(input::cursor()?),
                            "The pointer left the edge while the other computer was preparing"
                        );
                    } else {
                        ensure!(
                            matches!(response, DesktopResponse::Prepared { .. }),
                            "Other computer did not prepare its desktop"
                        );
                    }
                    ensure!(
                        self.config.daemon.sharing && input::available() && input::clean(),
                        "Release keys and mouse buttons before switching"
                    );
                    let h = &self.links[&peer].handle;
                    let mut random = [0u8; 16];
                    getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("{e}"))?;
                    h.begin_outbound(SessionContext {
                        session_epoch: SessionEpoch(random),
                        transport_generation: h.generation(),
                        activation_id: ActivationId(token),
                    })?;
                    input::grab()?;
                    Ok(())
                })();
                self.arming = None;
                if let Err(error) = checked {
                    self.links[&peer]
                        .handle
                        .close(SessionCloseReason::BackendUnavailable);
                    return Err(error);
                }
                self.send_clip(&peer);
                self.outbound = Some(Outbound {
                    peer,
                    session,
                    token,
                    handoff,
                    polling: false,
                    next_poll: Instant::now(),
                });
            }
            Network::Polled {
                peer,
                session,
                result,
            } => {
                if let Some(out) = self.outbound.as_mut()
                    && out.peer == peer
                    && out.session == session
                {
                    out.polling = false;
                    out.next_poll = Instant::now() + Duration::from_millis(30);
                    match result {
                        Ok(DesktopResponse::Active) => {}
                        Ok(DesktopResponse::Exited { exit, position }) => {
                            // Windows lists only the ways home, so every exit
                            // brings input back here.
                            let back = out
                                .handoff
                                .as_ref()
                                .map(|h| match h.next(exit, position)? {
                                    handoff::Next::Home(point) => Ok(point),
                                    handoff::Next::Hop(_) => {
                                        bail!("the other computer left toward another one")
                                    }
                                })
                                .transpose();
                            self.local();
                            if let Some(back) = back? {
                                input::move_to(back)?;
                                self.previous = back;
                            }
                        }
                        result => {
                            self.local();
                            bail!("Desktop handoff ended: {result:?}");
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn local(&mut self) {
        input::boundaries(&[]);
        self.boundaries.clear();
        input::remote(false);
        self.pending_activation = None;
        // Returning locally must also stop the remote sender. Releasing only
        // the injected state would let its next frame take control again.
        let incoming = self
            .inbound
            .take()
            .or_else(|| self.lease.as_ref().map(|l| (l.peer.clone(), l.session)));
        if let Some((peer, session)) = incoming
            && let Some(link) = self.links.get(&peer)
            && link.handle.id() == session
        {
            link.handle.close(SessionCloseReason::LocalRelease);
        }
        if let Some((peer, session)) = self.arming.take()
            && let Some(link) = self.links.get(&peer)
            && link.handle.id() == session
        {
            link.handle.close(SessionCloseReason::LocalRelease);
        }
        if let Some(out) = self.outbound.take()
            && let Some(link) = self.links.get(&out.peer)
        {
            let handle = link.handle.clone();
            tokio::spawn(async move {
                let _ = handle.end_outbound(SessionCloseReason::LocalRelease).await;
                let _ = handoff::check_finished(
                    handle
                        .desktop_request(DesktopRequest::Finish { token: out.token })
                        .await,
                );
            });
        }
        self.injector.release();
        self.inbound = None;
        self.lease = None;
        self.edge_since = None;
        if let Ok(p) = input::cursor() {
            self.previous = p;
        }
    }
    fn arm(&mut self, peer: String, handoff: Option<handoff::Handoff>) {
        if !self.config.daemon.sharing
            || !input::available()
            || self.inbound.is_some()
            || self.lease.is_some()
            || self.outbound.is_some()
            || self.arming.is_some()
        {
            return;
        }
        let Some(link) = self.links.get(&peer) else {
            return;
        };
        if !self
            .config
            .peers
            .get(&peer)
            .is_some_and(|p| p.permissions.receive_normal)
        {
            return;
        }
        let Ok(token) = handoff::token() else {
            return;
        };
        let session = link.handle.id();
        self.arming = Some((peer.clone(), session));
        // Input comes back from the other computer, but Windows does not
        // route it on to a third one.
        let handoff = handoff.map(|mut h| {
            h.keep_routes(|_| false);
            h
        });
        let request = handoff.as_ref().map_or(
            DesktopRequest::Prepare {
                monitor: None,
                token,
                edge: crate::desktop::Edge::Left,
                start: 0,
                end: crate::desktop::FRACTION_MAX,
                position: 500_000,
                exits: vec![crate::desktop::Exit {
                    monitor: None,
                    edge: crate::desktop::Edge::Left,
                    start: 0,
                    end: crate::desktop::FRACTION_MAX,
                }],
            },
            |h| h.prepare(token),
        );
        let handle = link.handle.clone();
        let tx = self.network.clone();
        tokio::spawn(async move {
            let result = handle.desktop_request(request).await;
            let _ = tx
                .send(Network::Prepared {
                    peer,
                    session,
                    token,
                    handoff,
                    result,
                })
                .await;
        });
    }
    fn session(&mut self, event: SessionEvent) {
        let SessionEvent {
            peer,
            session_id,
            kind,
        } = event;
        if !self
            .links
            .get(&peer)
            .is_some_and(|l| l.handle.id() == session_id)
        {
            if let SessionEventKind::ReceiverEffects { applied, .. } = kind {
                let _ = applied.send(Err("Session was replaced".into()));
            }
            return;
        }
        match kind {
            SessionEventKind::Closed { reason } => {
                if self.outbound.as_ref().is_some_and(|o| o.peer == peer)
                    || self.inbound.as_ref().is_some_and(|(p, _)| p == &peer)
                    || self.arming.as_ref().is_some_and(|(p, _)| p == &peer)
                    || self
                        .lease
                        .as_ref()
                        .is_some_and(|l| l.belongs(&peer, session_id))
                {
                    self.local();
                }
                self.links.remove(&peer);
                self.failures
                    .insert(peer, (1, Instant::now() + Duration::from_secs(1), reason));
            }
            SessionEventKind::ReceiverEffects {
                effects, applied, ..
            } => {
                let allowed = self.config.daemon.sharing
                    && input::available()
                    && self.outbound.is_none()
                    && self.arming.is_none()
                    && self
                        .config
                        .peers
                        .get(&peer)
                        .is_some_and(|p| p.permissions.connect && p.permissions.send_normal)
                    && self.inbound.as_ref().map_or_else(
                        || {
                            effects
                                .iter()
                                .any(|e| matches!(e, ReceiverEffect::ActivationOpened(_)))
                                || effects
                                    .iter()
                                    .all(|e| matches!(e, ReceiverEffect::ActivationClosed { .. }))
                        },
                        |(p, s)| p == &peer && *s == session_id,
                    )
                    && self
                        .lease
                        .as_ref()
                        .is_none_or(|l| l.belongs(&peer, session_id) && !l.expired());
                let owns = self.inbound.as_ref() == Some(&(peer.clone(), session_id));
                let result = if allowed {
                    if effects
                        .iter()
                        .any(|e| matches!(e, ReceiverEffect::ActivationOpened(_)))
                    {
                        let p = &self.config.peers[&peer];
                        self.injector.configure(p.keyboard, p.reverse_scroll);
                        self.inbound = Some((peer.clone(), session_id));
                    }
                    let closed = effects
                        .iter()
                        .any(|e| matches!(e, ReceiverEffect::ActivationClosed { .. }));
                    // The sender learns about a return on its next Poll. Ignore
                    // in-flight frames until then; rejecting them would close
                    // the session before it receives the return coordinates.
                    let result = if self.lease.as_ref().is_some_and(|l| l.exited()) {
                        self.injector.release();
                        Ok(())
                    } else {
                        self.injector.apply(effects).map_err(|e| format!("{e:#}"))
                    };
                    if closed {
                        self.inbound = None;
                    }
                    result
                } else {
                    if owns {
                        self.injector.release();
                        self.inbound = None;
                    }
                    Err("Windows is paused, locked, or already sharing input".into())
                };
                if let Err(error) = &result {
                    self.notice = Some(error.clone());
                }
                let _ = applied.send(result);
            }
            SessionEventKind::Desktop { request, reply } => {
                let result = self.desktop(&peer, session_id, request);
                let _ = reply
                    .send(result.unwrap_or_else(|e| DesktopResponse::unavailable(e.to_string())));
            }
            SessionEventKind::Layout { layout } => {
                if layout.is_newer_than(&self.layout) {
                    self.layout = layout;
                    if let Err(e) = self.save_layout() {
                        self.notice = Some(e.to_string());
                    }
                }
            }
            SessionEventKind::OutboundEnded => {
                if self.outbound.as_ref().is_some_and(|o| o.peer == peer) {
                    self.local();
                }
            }
            SessionEventKind::Clipboard { clip } => {
                if self.config.daemon.sharing && self.config.clipboard.share && input::available() {
                    match clipboard::write(&clip) {
                        Ok(()) => self.clip_echo.entry(peer).or_default().written(&clip),
                        Err(e) => self.notice = Some(e.to_string()),
                    }
                }
            }
        }
    }
    fn desktop(
        &mut self,
        peer: &str,
        session: u64,
        request: DesktopRequest,
    ) -> Result<DesktopResponse> {
        ensure!(
            self.config.daemon.sharing && input::available(),
            "Windows sharing is paused or the desktop is locked"
        );
        ensure!(
            self.config
                .peers
                .get(peer)
                .is_some_and(|p| p.permissions.send_normal),
            "This computer may not control Windows"
        );
        ensure!(
            self.outbound.is_none() && self.arming.is_none(),
            "Windows is sending input"
        );
        ensure!(
            self.inbound
                .as_ref()
                .is_none_or(|(p, s)| p == peer && *s == session),
            "Another computer owns input"
        );
        ensure!(
            self.lease.as_ref().is_none_or(|l| l.belongs(peer, session)),
            "Another computer owns the desktop"
        );
        request.validate()?;
        if matches!(request, DesktopRequest::Snapshot) {
            return Ok(DesktopResponse::Snapshot {
                geometry: input::geometry()?,
                position: input::cursor()?,
            });
        }
        if matches!(request, DesktopRequest::Prepare { .. }) {
            ensure!(self.lease.is_none(), "A desktop handoff is already active");
            input::boundaries(&[]);
            self.boundaries.clear();
            let (lease, response) = desktop::prepare(peer.into(), session, request)?;
            self.lease = Some(lease);
            return Ok(response);
        }
        let finish = matches!(request, DesktopRequest::Finish { .. });
        let response = self
            .lease
            .as_mut()
            .context("Desktop handoff expired or ended")?
            .request(request)?;
        if finish {
            self.send_clip(peer);
            self.injector.release();
            self.inbound = None;
            self.lease = None;
        }
        Ok(response)
    }
    fn tick(&mut self) -> Result<()> {
        let now = Instant::now();
        self.desktop_available = input::available();
        if !self.desktop_available {
            if self.outbound.is_some()
                || self.inbound.is_some()
                || self.lease.is_some()
                || self.arming.is_some()
            {
                self.local();
            }
            self.injector.release();
            return Ok(());
        }
        if self.outbound.is_some() && !input::is_remote() {
            self.local();
            self.notice = Some("Input returned locally after capture stopped responding".into());
        }
        input::pulse();
        self.injector.tick()?;
        if let Some((peer, expires)) = self.pending_activation.take() {
            if now >= expires {
                self.notice = Some("Release keys and mouse buttons, then try Control again".into());
            } else if input::clean() {
                self.arm(peer, None);
            } else {
                self.pending_activation = Some((peer, expires));
            }
        }
        if let Some(lease) = &mut self.lease
            && (lease.expired() || lease.sample().is_err())
        {
            let peer = lease.peer.clone();
            self.local();
            if let Some(link) = self.links.get(&peer) {
                link.handle.close(SessionCloseReason::BackendUnavailable);
            }
        }
        if now >= self.next_geometry {
            self.next_geometry = now + Duration::from_secs(2);
            let geometry = input::geometry()?;
            if geometry != self.geometry {
                self.local();
                self.geometry = geometry;
            }

            if let Some(layout) = self
                .layout
                .with_geometry(&self.identity.fingerprint_hex(), &self.geometry)
            {
                self.layout = layout;
                self.save_layout()?;
            }
            let to_dial: Vec<_> = self
                .config
                .peers
                .iter()
                .filter(|(name, p)| {
                    self.config.daemon.sharing
                        && p.permissions.connect
                        && !self.links.contains_key(*name)
                        && !self.connecting.contains(*name)
                        && self.failures.get(*name).is_none_or(|(_, at, _)| now >= *at)
                })
                .map(|(name, _)| name.clone())
                .collect();
            for peer in to_dial {
                self.dial(peer);
            }
        }
        if now >= self.next_discovery {
            self.next_discovery = now + Duration::from_secs(1);
            self.neighbors.keep_fresh(now.into());
            self.neighbors.expire(now.into());
            for (instance, addresses) in self
                .neighbors
                .take_due_hellos(self.permits.available_permits(), now.into())
            {
                self.probe(Some(instance), addresses);
            }
        }
        if let Some(out) = &mut self.outbound {
            if !out.polling && now >= out.next_poll {
                out.polling = true;
                let peer = out.peer.clone();
                let session = out.session;
                let token = out.token;
                let handle = self.links[&peer].handle.clone();
                let tx = self.network.clone();
                tokio::spawn(async move {
                    let result = handle.desktop_request(DesktopRequest::Poll { token }).await;
                    let _ = tx
                        .send(Network::Polled {
                            peer,
                            session,
                            result,
                        })
                        .await;
                });
            }
        } else if self.arming.is_none()
            && self.inbound.is_none()
            && self.lease.is_none()
            && self.config.daemon.sharing
        {
            let current = input::cursor()?;
            let layout = self.local_layout();
            if input::clean()
                && let Some(h) = handoff::crossing(&layout, &self.geometry, self.previous, current)
            {
                if !self.config.switching.pause_at_edges {
                    self.arm(h.peer.clone(), Some(h));
                } else {
                    self.edge_since = Some((h, now));
                }
            }
            if let Some((held, since)) = self.edge_since.take()
                && let Some(h) =
                    handoff::on_edge(&layout, &self.geometry, held.return_mapping.edge, current)
                && h.peer == held.peer
                && h.monitor == held.monitor
                && h.return_mapping == held.return_mapping
                && input::clean()
            {
                if now.duration_since(since) >= Duration::from_millis(250) {
                    self.arm(h.peer.clone(), Some(h));
                } else {
                    self.edge_since = Some((held, since));
                }
            }
            self.previous = current;
        }
        Ok(())
    }
    fn edge_hit(&mut self, point: crate::desktop::Point, edge: crate::desktop::Edge) -> Result<()> {
        if self.outbound.is_some()
            || self.arming.is_some()
            || self.inbound.is_some()
            || self.lease.is_some()
            || !self.config.daemon.sharing
            || !input::clean()
            || input::cursor()? != point
        {
            return Ok(());
        }
        if let Some(h) = handoff::on_edge(&self.local_layout(), &self.geometry, edge, point) {
            if !self.config.switching.pause_at_edges {
                self.arm(h.peer.clone(), Some(h));
            } else if self.edge_since.as_ref().is_none_or(|(held, _)| {
                held.peer != h.peer
                    || held.monitor != h.monitor
                    || held.return_mapping != h.return_mapping
            }) {
                self.edge_since = Some((h, Instant::now()));
            }
        }
        Ok(())
    }
    fn refresh_boundaries(&mut self) {
        let mut boundaries = Vec::new();
        if self.desktop_available && self.config.daemon.sharing && self.outbound.is_none() {
            if let Some(lease) = &self.lease {
                boundaries.extend(lease.boundaries());
            } else if self.inbound.is_none() {
                let layout = self.local_layout();
                for t in layout.transitions() {
                    let source = &layout.monitors[t.source];
                    let Some(peer) = &layout.monitors[t.target].peer else {
                        continue;
                    };
                    if source.peer.is_some()
                        || !self.links.get(peer).is_some_and(|l| !l.handle.is_closed())
                        || !self
                            .config
                            .peers
                            .get(peer)
                            .is_some_and(|p| p.permissions.connect && p.permissions.receive_normal)
                    {
                        continue;
                    }
                    let Ok(selected) = self
                        .geometry
                        .for_monitor(source.display.as_ref().map(|d| d.id.as_str()))
                    else {
                        continue;
                    };
                    let Ok(bounds) = selected.bounds() else {
                        continue;
                    };
                    if source.display.as_ref().is_some_and(|d| d.bounds != bounds) {
                        continue;
                    }
                    boundaries.extend(input::Boundary::new(
                        bounds,
                        t.edge,
                        (t.source_start * f64::from(crate::desktop::FRACTION_MAX)).round() as u32,
                        (t.source_end * f64::from(crate::desktop::FRACTION_MAX)).round() as u32,
                        false,
                    ));
                }
            }
        }
        if boundaries != self.boundaries {
            input::boundaries(&boundaries);
            self.boundaries = boundaries;
        }
    }
    fn send_clip(&mut self, peer: &str) {
        if !self.config.clipboard.share {
            return;
        }
        match clipboard::read() {
            Ok(Some(clip)) => {
                if self
                    .clip_echo
                    .entry(peer.to_owned())
                    .or_default()
                    .should_send(&clip)
                    && let Some(link) = self.links.get(peer)
                {
                    link.handle.send_clipboard(clip);
                }
            }
            Ok(None) => {}
            Err(e) => self.notice = Some(e.to_string()),
        }
    }
}

async fn resolve(address: &str) -> Result<SocketAddr> {
    if let Ok(ip) = address.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, 43119));
    }
    if let Ok(socket) = address.parse() {
        return Ok(socket);
    }
    let host = if address.contains(':') {
        address.to_owned()
    } else {
        format!("{address}:43119")
    };
    let addresses: Vec<_> = tokio::net::lookup_host(host).await?.collect();
    addresses
        .iter()
        .find(|a| a.is_ipv4())
        .or(addresses.first())
        .copied()
        .context("Computer name did not resolve to an IP address")
}

fn session_allowed(sharing: bool, record: Option<&crate::config::PeerConfig>, spki: &[u8]) -> bool {
    sharing
        && record
            .is_some_and(|p| p.permissions.connect && p.spki_der().is_ok_and(|key| key == spki))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_inflight_connection_cannot_survive_pause_revoke_or_key_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(&dir.path().join("a")).unwrap();
        let b = Identity::load_or_create(&dir.path().join("b")).unwrap();
        let mut record = crate::config::PeerConfig::from_spki(
            a.spki(),
            vec![],
            crate::config::PeerPermissions {
                connect: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(session_allowed(true, Some(&record), a.spki()));
        assert!(!session_allowed(false, Some(&record), a.spki()));
        assert!(!session_allowed(true, None, a.spki()));
        assert!(!session_allowed(true, Some(&record), b.spki()));
        record.permissions.connect = false;
        assert!(!session_allowed(true, Some(&record), a.spki()));
    }
    #[tokio::test]
    async fn tailscale_names_ip_literals_and_ports() {
        assert_eq!(
            resolve("100.114.101.60").await.unwrap(),
            "100.114.101.60:43119".parse().unwrap()
        );
        assert_eq!(
            resolve("[::1]:43120").await.unwrap(),
            "[::1]:43120".parse().unwrap()
        );
        assert_eq!(
            resolve("::1").await.unwrap(),
            "[::1]:43119".parse().unwrap()
        );
        assert!(resolve("localhost").await.is_ok());
        assert!(resolve("invalid host name:invalid").await.is_err());
    }
}
