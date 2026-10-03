//! Arrange to pair on Linux: hellos in and out, the computers found around
//! this one, the fresh install's pairing window, and trusting a computer a
//! person placed. Nothing here trusts a key on a hello's word alone.

use std::sync::PoisonError;

use super::*;
use crate::{
    discovery::UntrustedCandidate,
    hello::{Notice, NoticeKind, peer_name},
    neighbors::{Stranger, Unplaced, UnplacedState, fingerprint},
    transport::{HelloConnection, connect_hello},
    wire::Hello,
};

/// Hellos this computer has out at once.
pub(super) const HELLO_SLOTS: usize = 4;
/// How often found computers expire, due hellos go out and the pairing
/// window looks around.
const TICK: Duration = Duration::from_secs(1);
/// Recent notices kept for the settings window.
const MAX_NOTICES: usize = 8;

/// Where a person dropped a tile: its top left corner, in layout units,
/// and how far it may snap.
#[derive(Debug, Clone, Copy)]
pub(super) struct Spot {
    pub x: i32,
    pub y: i32,
    pub tolerance: u32,
}

/// Expires found computers, says hello to new ones and lets the pairing
/// window take a lone stranger, once a second.
pub(super) fn start(shared: Arc<Shared>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            shared.look_around().await;
        }
    })
}

impl Shared {
    async fn look_around(self: &Arc<Self>) {
        let now = tokio::time::Instant::now();
        let room = self.hello_slots.available_permits();
        let open = self.window().is_open();
        let mut due = Vec::new();
        self.neighbors.send_modify(|neighbors| {
            neighbors.expire(now);
            if open {
                neighbors.keep_fresh(now);
            }
            due = neighbors.take_due_hellos(room, now);
        });
        for (instance, addresses) in due {
            let Ok(permit) = self.hello_slots.clone().try_acquire_owned() else {
                break;
            };
            let shared = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                shared.say_hello(&instance, &addresses).await;
            });
        }
        let strangers = {
            let config = self.config.read().await;
            self.neighbors.borrow().strangers(&config, now)
        };
        let joining = self.window().tick(&strangers, now);
        if let Some(stranger) = joining {
            self.join(&stranger).await;
        }
    }

    /// Trusts the lone stranger the pairing window settled on. Saving it
    /// closes the window as accepted; if it cannot be saved, the window
    /// closes as failed and a person places it.
    async fn join(self: &Arc<Self>, stranger: &Stranger) {
        let _mutation = self.config_mutation.lock().await;
        // A person placed a computer while this waited, which used it up.
        if !self.window().is_open() {
            return;
        }
        let config = self.config.read().await.clone();
        let id = format!("key:{}", stranger.key);
        match self.trust(config, &id, None).await {
            Ok(name) => {
                tracing::info!(peer = %name, mark = %stranger.mark, "computer joined while the pairing window was open");
            }
            Err(error) => {
                self.window().not_added();
                tracing::warn!(
                    peer = %stranger.name,
                    mark = %stranger.mark,
                    error = %format_args!("{error:#}"),
                    "pairing window could not add a computer and closed; place it by hand"
                );
            }
        }
    }

    /// Says hello at every address of a record at once and keeps the first
    /// computer that answers. A record nothing answers at is asked again.
    async fn say_hello(&self, instance: &str, addresses: &[SocketAddr]) {
        let mut attempts = tokio::task::JoinSet::new();
        for &address in addresses {
            let (endpoint, config) = (self.endpoint.clone(), self.hello_config.clone());
            attempts.spawn(async move {
                let connecting = connect_hello(&endpoint, address, &config);
                match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
                    Ok(Ok(connection)) => Some((address, connection)),
                    _ => None,
                }
            });
        }
        let mut answered = None;
        while let Some(attempt) = attempts.join_next().await {
            if let Ok(Some(found)) = attempt {
                answered = Some(found);
                break;
            }
        }
        drop(attempts);
        let heard = match answered {
            Some((address, connection)) => self.exchange(connection, address, Some(instance)).await,
            None => false,
        };
        if !heard {
            tracing::debug!(instance, "no hello came back");
            let now = tokio::time::Instant::now();
            self.neighbors.send_if_modified(|neighbors| {
                neighbors.no_answer(instance, now);
                false
            });
        }
    }

    /// Answers a hello that came to the input port, from any key, unless
    /// it came from this computer itself.
    pub(super) async fn answer_hello(&self, connection: HelloConnection) {
        let remote = connection.remote_address();
        let now = tokio::time::Instant::now();
        let local = crate::discovery::this_host_addresses();
        let mut allowed = false;
        self.neighbors.send_if_modified(|neighbors| {
            allowed = neighbors.allow_hello(remote.ip(), &local, now);
            false
        });
        if !allowed {
            connection.close();
            return;
        }
        self.exchange(connection, remote, None).await;
    }

    /// Trades hellos on `connection` and returns whether one came back.
    async fn exchange(
        &self,
        connection: HelloConnection,
        remote: SocketAddr,
        instance: Option<&str>,
    ) -> bool {
        let spki = connection.peer_spki().to_vec();
        let ours = {
            let config = self.config.read().await;
            let trusts_you = peer_keys(&config)
                .values()
                .any(|key| *key == fingerprint(&spki));
            crate::hello::local_hello(config.transport.listen.port(), trusts_you)
        };
        match tokio::time::timeout(CONNECT_TIMEOUT, connection.exchange(&ours)).await {
            Ok(Ok(hello)) => {
                self.heard(&spki, remote, &hello, instance);
                return true;
            }
            Ok(Err(error)) => tracing::debug!(%error, %remote, "hello failed"),
            Err(_) => tracing::debug!(%remote, "hello timed out"),
        }
        false
    }

    fn heard(&self, spki: &[u8], remote: SocketAddr, hello: &Hello, instance: Option<&str>) {
        let now = tokio::time::Instant::now();
        self.neighbors.send_modify(|neighbors| {
            neighbors.hello(spki, remote, hello, instance, now);
        });
    }

    /// Browsing started, and finds this computer's own record too.
    pub(super) fn own_record(&self, discovery: Option<&crate::discovery::Discovery>) {
        let own = discovery.map(|discovery| discovery.instance_id().to_string());
        self.neighbors.send_if_modified(|neighbors| {
            neighbors.own_record(own);
            false
        });
    }

    /// Browsing stopped, so the records it found are gone.
    pub(super) fn forget_records(&self, records: &mut std::collections::BTreeSet<String>) {
        let now = tokio::time::Instant::now();
        self.neighbors.send_modify(|neighbors| {
            for instance in std::mem::take(records) {
                neighbors.instance_gone(&instance, now);
            }
        });
    }

    /// Says hello to an address a person typed, as to an mDNS record.
    pub(super) fn add_address(&self, address: SocketAddr) -> Result<()> {
        UntrustedCandidate::explicit(address)
            .map_err(|error| anyhow!("{address} cannot be added: {error}"))?;
        self.neighbors
            .send_modify(|neighbors| neighbors.add_address(address));
        Ok(())
    }

    /// Opens a fresh install's pairing window, now that a person is at this
    /// computer. Only the first call after setup does anything.
    pub(super) async fn open_pairing_window(&self) {
        let has_peers = !self.config.read().await.peers.is_empty();
        self.window().open(has_peers, tokio::time::Instant::now());
    }

    pub(super) fn window(&self) -> std::sync::MutexGuard<'_, PairingWindow> {
        self.window.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn notices(&self) -> Vec<Notice> {
        self.notices
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The found computers that are not trusted here.
    pub(super) fn unplaced(&self, config: &Config) -> Vec<Unplaced> {
        self.neighbors.borrow().unplaced(config)
    }

    /// The found computer with this name or mark, for the command line. A
    /// name two computers share needs the mark.
    pub(super) fn find_unplaced(&self, config: &Config, computer: &str) -> Result<Unplaced> {
        let tiles = self.unplaced(config);
        let matches: Vec<&Unplaced> = tiles
            .iter()
            .filter(|tile| {
                tile.name == computer
                    || tile
                        .mark
                        .as_deref()
                        .is_some_and(|mark| mark.eq_ignore_ascii_case(computer))
            })
            .collect();
        match matches[..] {
            [] => bail!("No computer called {computer} was found; run zflow nearby"),
            [tile] => match tile.state {
                UnplacedState::Ready | UnplacedState::DuplicateName => Ok(tile.clone()),
                UnplacedState::Identifying => bail!("{computer} has not answered yet; try again"),
                UnplacedState::DifferentVersion => {
                    bail!("{computer} runs another zflow version. Update both computers")
                }
            },
            _ => bail!("Several computers are called {computer}; use the mark zflow nearby shows"),
        }
    }

    /// Trusts a found computer, as a person dropping its tile on the board
    /// or the pairing window does, and returns the name it is saved under.
    /// Dropped on the tile of a saved computer with its name, it is that
    /// computer reinstalled: it keeps the name, settings and tile with its
    /// new key. Otherwise its tile goes where it was dropped, or beside this
    /// computer. The caller holds `config_mutation`; `config` is current.
    pub(super) async fn trust(
        self: &Arc<Self>,
        mut config: Config,
        id: &str,
        spot: Option<Spot>,
    ) -> Result<String> {
        let key = id
            .strip_prefix("key:")
            .context("zflow is still looking up that computer")?;
        let neighbor = self
            .neighbors
            .borrow()
            .neighbor(key)
            .cloned()
            .context("That computer is no longer on the network")?;
        if let Some(name) = peer_keys(&config)
            .into_iter()
            .find_map(|(name, known)| (known == key).then_some(name))
        {
            bail!("{name} is already added");
        }
        let full = self
            .layout
            .lock()
            .await
            .as_ref()
            .is_some_and(|layout| layout.tiles.len() >= crate::desktop::MAX_SHARED_TILES);
        ensure!(!full, "The arrangement has no room for another computer");
        let namesake = match spot {
            Some(spot) => self.namesake_under(&neighbor.name, spot).await,
            None => None,
        };
        let name = match &namesake {
            Some(name) => {
                crate::hello::replace_key(&mut config, name, &neighbor.spki, &neighbor.addresses)?;
                name.clone()
            }
            None => crate::hello::trust_peer(
                &mut config,
                &neighbor.spki,
                &neighbor.name,
                &neighbor.addresses,
            )?,
        };
        self.apply_config_locked(config, true).await?;
        self.window().placed();
        // A new tile starts beside this computer; a reinstalled one already
        // has its old tile.
        if let (None, Some(spot)) = (&namesake, spot)
            && let Err(error) = self
                .move_tile(&format!("peer:{name}"), spot.x, spot.y, spot.tolerance)
                .await
        {
            tracing::debug!(%error, peer = %name, "placed beside this computer instead");
        }
        self.post_joined(&name, &neighbor.mark);
        Ok(name)
    }

    /// The saved computer called `name` whose tile is under the middle of a
    /// new tile dropped at `spot`.
    async fn namesake_under(&self, name: &str, spot: Spot) -> Option<String> {
        let layout = self.layout_status().await?;
        let (width, height) = (i64::from(PEER_TILE_SIZE.0), i64::from(PEER_TILE_SIZE.1));
        let (x, y) = (
            i64::from(spot.x) + width / 2,
            i64::from(spot.y) + height / 2,
        );
        layout.monitors.into_iter().find_map(|monitor| {
            let (left, top) = (i64::from(monitor.x), i64::from(monitor.y));
            let under = (left..left + i64::from(monitor.width)).contains(&x)
                && (top..top + i64::from(monitor.height)).contains(&y);
            monitor
                .peer
                .filter(|peer| under && peer_name(Some(peer)) == name)
        })
    }

    /// Tells the person here that `name` joined, with its key's `mark`: a
    /// notification now, and a notice the settings window shows with a way
    /// to forget it.
    fn post_joined(self: &Arc<Self>, name: &str, mark: &str) {
        {
            let mut notices = self.notices.lock().unwrap_or_else(PoisonError::into_inner);
            let id = notices.last().map_or(1, |last| last.id + 1);
            notices.push(Notice {
                id,
                kind: NoticeKind::Joined,
                name: name.to_owned(),
                mark: mark.to_owned(),
            });
            if notices.len() > MAX_NOTICES {
                notices.remove(0);
            }
        }
        let message = joined_message(name, mark);
        let shared = self.clone();
        tokio::spawn(async move { shared.desktop.notify(message).await });
    }
}

/// The notification for a computer that joined. Its mark is what tells it
/// from another computer with its name.
fn joined_message(name: &str, mark: &str) -> String {
    format!(
        "{name} joined, with mark {mark}. It can share this computer's keyboard and mouse. Not yours? Forget it in zflow settings."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> (tempfile::TempDir, Identity) {
        let directory = tempfile::tempdir().unwrap();
        let identity = Identity::load_or_create(directory.path()).unwrap();
        (directory, identity)
    }

    /// A computer called `name` said hello; returns its shelf id. It takes
    /// input where nothing listens, so its link dials nothing real.
    fn found(shared: &Shared, spki: &[u8], name: &str) -> String {
        let hello = crate::hello::make_hello(name, 9, Vec::new(), false);
        let remote = "127.0.0.1:9".parse().unwrap();
        shared.heard(spki, remote, &hello, None);
        format!("key:{}", fingerprint(spki))
    }

    /// The layout before anyone arranged it: this computer alone.
    async fn arranged(shared: &Shared) {
        let own = &shared.identity_fingerprint;
        let layout = initial_layout(own, 2560, 1440, &BTreeMap::new()).unwrap();
        *shared.layout.lock().await = Some(layout);
    }

    async fn place(shared: &Arc<Shared>, id: &str, spot: Option<Spot>) -> Result<String> {
        let _mutation = shared.config_mutation.lock().await;
        let config = shared.config.read().await.clone();
        shared.trust(config, id, spot).await
    }

    fn tile_at(layout: &crate::desktop::SharedLayout, id: &str) -> Option<(i32, i32)> {
        let key = id.strip_prefix("key:").unwrap();
        let tile = layout.tiles.iter().find(|tile| tile.key == key)?;
        Some((tile.x, tile.y))
    }

    #[tokio::test]
    async fn placing_a_found_computer_trusts_it_where_it_was_dropped() {
        let (shared, _kept) = test_daemon();
        let (_directory, desk) = identity();
        arranged(&shared).await;
        let id = found(&shared, desk.spki(), "desk");
        assert_eq!(shared.unplaced(&*shared.config.read().await).len(), 1);

        let left = Spot {
            x: -1920,
            y: 0,
            tolerance: 0,
        };
        assert_eq!(place(&shared, &id, Some(left)).await.unwrap(), "desk");
        let config = shared.config.read().await.clone();
        assert_eq!(config.peers["desk"].spki_der().unwrap(), desk.spki());
        assert_eq!(Config::load(&shared.config_path).unwrap(), config);
        let layout = shared.layout.lock().await.clone().unwrap();
        assert_eq!(tile_at(&layout, &id), Some((-1920, 0)));
        assert!(shared.unplaced(&config).is_empty());
        let notice = &shared.notices()[0];
        let mark = crate::neighbors::mark(desk.spki());
        assert_eq!((notice.name.as_str(), &notice.mark), ("desk", &mark));
        let message = joined_message("desk", &mark);
        assert!(message.starts_with(&format!("desk joined, with mark {mark}.")));

        // Once only, and only computers that said hello.
        assert!(place(&shared, &id, None).await.is_err());
        let (_other_directory, other) = identity();
        let unknown = format!("key:{}", fingerprint(other.spki()));
        assert!(place(&shared, &unknown, None).await.is_err());
        assert!(place(&shared, "instance:zf-desk", None).await.is_err());
        assert_eq!(shared.config.read().await.peers.len(), 1);
    }

    #[tokio::test]
    async fn a_drop_with_no_room_still_trusts_it_beside_this_computer() {
        let (shared, _kept) = test_daemon();
        let (_directory, desk) = identity();
        arranged(&shared).await;
        let id = found(&shared, desk.spki(), "desk");
        // Right on top of this computer, too far from an edge to snap.
        let onto = Spot {
            x: 0,
            y: 0,
            tolerance: 0,
        };
        place(&shared, &id, Some(onto)).await.unwrap();
        let layout = shared.layout.lock().await.clone().unwrap();
        assert_eq!(tile_at(&layout, &id), Some((2560, 0)));
    }

    #[tokio::test]
    async fn dropping_a_reinstalled_computer_on_its_tile_gives_it_the_new_key() {
        let (shared, _kept) = test_daemon();
        let (_old_directory, old) = identity();
        let (_new_directory, new) = identity();
        arranged(&shared).await;
        let before = found(&shared, old.spki(), "desk");
        place(&shared, &before, None).await.unwrap();
        {
            let _mutation = shared.config_mutation.lock().await;
            let mut config = shared.config.read().await.clone();
            config.peers.get_mut("desk").unwrap().keyboard = crate::core::KeyboardMode::Mac;
            shared.apply_config_locked(config, true).await.unwrap();
        }
        let layout = shared.layout.lock().await.clone().unwrap();
        let (x, y) = tile_at(&layout, &before).unwrap();

        // The same name with a new key, dropped on the old tile.
        let after = found(&shared, new.spki(), "desk");
        let state = shared.unplaced(&*shared.config.read().await)[0].state;
        assert_eq!(state, UnplacedState::DuplicateName);
        let spot = Spot { x, y, tolerance: 0 };
        assert_eq!(place(&shared, &after, Some(spot)).await.unwrap(), "desk");
        let config = shared.config.read().await.clone();
        assert_eq!(config.peers.len(), 1);
        assert_eq!(config.peers["desk"].spki_der().unwrap(), new.spki());
        assert_eq!(
            config.peers["desk"].keyboard,
            crate::core::KeyboardMode::Mac
        );
        let layout = shared.layout.lock().await.clone().unwrap();
        assert_eq!(tile_at(&layout, &after), Some((x, y)));
        assert_eq!(tile_at(&layout, &before), None);
    }

    #[tokio::test]
    async fn a_computer_the_window_cannot_save_closes_it_as_failed() {
        use crate::pairing_window::{Reason, State};
        let (shared, _kept) = test_daemon();
        let state_dir = shared.config.read().await.daemon.state_dir.clone();
        PairingWindow::mark_eligible(&state_dir).unwrap();
        *shared.window() = PairingWindow::load(&state_dir);
        shared.open_pairing_window().await;
        let (_directory, desk) = identity();
        found(&shared, desk.spki(), "desk");
        let now = tokio::time::Instant::now();
        let config = shared.config.read().await.clone();
        let stranger = shared.neighbors.borrow().strangers(&config, now).present[0].clone();
        // Someone edited the file meanwhile, so saving it refuses.
        let mut edited = config.clone();
        edited.clipboard.share = !edited.clipboard.share;
        edited.save(&shared.config_path).unwrap();

        shared.join(&stranger).await;
        let window = shared.window().view(now);
        assert_eq!(
            (window.state, window.reason),
            (State::Closed, Some(Reason::Failed))
        );
        assert!(shared.config.read().await.peers.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_record_nothing_answers_at_is_asked_again_soon() {
        let (shared, _kept) = test_daemon();
        let now = tokio::time::Instant::now();
        let take = |at| {
            let mut due = Vec::new();
            shared
                .neighbors
                .send_modify(|neighbors| due = neighbors.take_due_hellos(4, at));
            due
        };
        shared.neighbors.send_modify(|neighbors| {
            neighbors.instance_seen("zf-desk", Vec::new(), true, None);
        });
        assert_eq!(take(now).len(), 1);
        shared.say_hello("zf-desk", &[]).await;
        assert!(take(now + Duration::from_millis(1999)).is_empty());
        assert_eq!(take(now + Duration::from_secs(2)).len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hello_from_this_computer_is_hung_up() {
        let (shared, _kept) = test_daemon();
        let (_server_directory, server_key) = identity();
        let (_local_directory, local_key) = identity();
        let loopback = SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0));
        let server =
            input_server_config_for_peers(&server_key, std::iter::empty::<&[u8]>()).unwrap();
        let listener = Endpoint::server(server.quinn_config(), loopback).unwrap();
        let address = listener.local_addr().unwrap();
        // A process on this computer, such as another user's, knocks.
        let client = Endpoint::client(loopback).unwrap();
        let config = hello_client_config(&local_key).unwrap();
        let knocking = tokio::spawn(async move {
            let connection = connect_hello(&client, address, &config).await?;
            let hello = crate::hello::make_hello("desk", 9, Vec::new(), false);
            connection.exchange(&hello).await
        });
        let incoming = listener.accept().await.unwrap();
        let Accepted::Hello(knock) = accept(incoming, &server).await.unwrap() else {
            panic!("expected a hello");
        };
        shared.answer_hello(knock).await;
        let answer = tokio::time::timeout(CONNECT_TIMEOUT, knocking).await;
        assert!(answer.unwrap().unwrap().is_err(), "nobody answered it");
        assert!(shared.unplaced(&*shared.config.read().await).is_empty());
        listener.close(0_u32.into(), b"test finished");
    }

    #[tokio::test]
    async fn the_command_line_picks_a_computer_by_name_or_by_mark() {
        let (shared, _kept) = test_daemon();
        let (_one_directory, one) = identity();
        let (_two_directory, two) = identity();
        let first = found(&shared, one.spki(), "desk");
        found(&shared, two.spki(), "desk");
        let config = shared.config.read().await.clone();
        let error = shared.find_unplaced(&config, "desk").unwrap_err();
        assert!(error.to_string().contains("mark"), "{error}");
        assert!(shared.find_unplaced(&config, "laptop").is_err());
        let mark = crate::neighbors::mark(one.spki()).to_uppercase();
        assert_eq!(shared.find_unplaced(&config, &mark).unwrap().id, first);
    }
}
