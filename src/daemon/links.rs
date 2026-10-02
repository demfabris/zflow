//! One live session to each paired computer this one may send to, so the
//! first crossing does not wait for a handshake. A session the other
//! computer dialed counts, and while one is up nothing is dialed.
//!
//! A link dials only where a hello proved the peer's key: an mDNS record or
//! an added address that answered with it, whatever address it has now. A
//! computer that does not listen is never dialed, and its own link to this
//! computer is the session. When both computers dial at once, both keep the
//! same session. Crossings and the chord still dial on demand when no
//! session is up.

use super::*;
use crate::{
    link::{STABLE_SESSION, retry_delay},
    peer_view::LinkStatus,
};

/// What a link uses from the daemon, so tests can stand in for it.
pub(super) trait Sessions: Send + Sync + 'static {
    /// The id of the peer's session, whichever computer dialed it.
    fn current(&self, peer: &str) -> impl Future<Output = Option<u64>> + Send;
    /// Dials the peer and returns the session it keeps, which is the
    /// peer's own if its dial wins.
    fn dial(
        &self,
        peer: &str,
        record: &PeerConfig,
        addresses: &[SocketAddr],
    ) -> impl Future<Output = Result<u64>> + Send;
}

impl Sessions for Shared {
    async fn current(&self, peer: &str) -> Option<u64> {
        self.sessions.lock().await.get(peer).map(SessionHandle::id)
    }

    async fn dial(&self, peer: &str, record: &PeerConfig, addresses: &[SocketAddr]) -> Result<u64> {
        Ok(self.connect(peer, record, addresses).await?.id())
    }
}

pub(super) struct Link {
    record: PeerConfig,
    /// Dropping it stops the link at its next wait.
    retry: mpsc::UnboundedSender<()>,
    /// None while the peer has a session or the link has nothing to dial.
    status: watch::Receiver<Option<LinkStatus>>,
}

impl Shared {
    /// Keeps one link per peer this computer may send to, and stops the
    /// others. A peer whose record changed starts over, so a computer paired
    /// again is dialed at once; one that only learned an address carries on.
    /// Pausing sharing stops every link.
    pub(super) async fn sync_links(self: &Arc<Self>) {
        let wanted = linked_peers(&*self.config.read().await);
        let mut links = self.links.lock().await;
        links.retain(|name, link| {
            wanted.get(name).map(without_addresses) == Some(without_addresses(&link.record))
        });
        for (name, record) in wanted {
            if let std::collections::btree_map::Entry::Vacant(slot) = links.entry(name) {
                let link = start(
                    self.clone(),
                    slot.key().clone(),
                    record,
                    self.session_changes.subscribe(),
                    self.neighbors.subscribe(),
                );
                slot.insert(link);
            }
        }
    }

    /// Ends each link's wait, so it dials now.
    pub(super) async fn retry_links(&self) {
        for link in self.links.lock().await.values() {
            let _ = link.retry.send(());
        }
    }

    /// The links that have no session yet, for the settings window.
    pub(super) async fn link_status(&self) -> BTreeMap<String, LinkStatus> {
        self.links
            .lock()
            .await
            .iter()
            .filter_map(|(name, link)| Some((name.clone(), link.status.borrow().clone()?)))
            .collect()
    }
}

/// The peers that get a link: those this computer may send to, as for the
/// chord. Empty while sharing is paused.
fn linked_peers(config: &Config) -> BTreeMap<String, PeerConfig> {
    eligible_outbound_peers(config)
        .into_iter()
        .filter_map(|name| {
            let record = config.peers.get(&name)?.clone();
            Some((name, record))
        })
        .collect()
}

fn start<S: Sessions>(
    sessions: Arc<S>,
    peer: String,
    record: PeerConfig,
    changes: watch::Receiver<()>,
    neighbors: watch::Receiver<Neighbors>,
) -> Link {
    let (retry, retries) = mpsc::unbounded_channel();
    let (status_sender, status) = watch::channel(None);
    let task = Task {
        sessions,
        peer,
        key: record.fingerprint_hex().unwrap_or_default(),
        record: record.clone(),
        retries,
        changes,
        neighbors,
    };
    tokio::spawn(run(task, status_sender));
    Link {
        record,
        retry,
        status,
    }
}

/// What a link acts on: the peer's session while it has one, or else where
/// it can be dialed.
#[derive(Debug, PartialEq)]
enum View {
    Session(u64),
    Dial(Vec<SocketAddr>),
}

struct Task<S> {
    sessions: Arc<S>,
    peer: String,
    /// The fingerprint of the peer's key, which finds it among the neighbors.
    key: String,
    record: PeerConfig,
    retries: mpsc::UnboundedReceiver<()>,
    changes: watch::Receiver<()>,
    neighbors: watch::Receiver<Neighbors>,
}

impl<S: Sessions> Task<S> {
    /// Without a session, the link dials wherever a hello proved the key.
    /// That is nowhere while the peer is off, out of mDNS reach, or a Mac
    /// that is not listening.
    async fn view(&self) -> View {
        match self.sessions.current(&self.peer).await {
            Some(id) => View::Session(id),
            None => View::Dial(self.neighbors.borrow().addresses_for_key(&self.key)),
        }
    }

    /// Waits until the view differs from `seen`, a retry asks for an attempt
    /// now, or `delay` passes. False once the link is stopped.
    async fn wait(&mut self, seen: &View, delay: Option<Duration>) -> bool {
        let sleep = async {
            match delay {
                Some(delay) => tokio::time::sleep(delay).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(sleep);
        loop {
            if self.view().await != *seen {
                return true;
            }
            tokio::select! {
                () = &mut sleep => return true,
                retry = self.retries.recv() => match retry {
                    None => return false,
                    // A live session has nothing to retry.
                    Some(()) if matches!(seen, View::Session(_)) => {}
                    Some(()) => return true,
                },
                changed = self.changes.changed() => if changed.is_err() {
                    return false;
                },
                changed = self.neighbors.changed() => if changed.is_err() {
                    return false;
                },
            }
        }
    }
}

/// Keeps the peer's session up, with the Mac app's retry schedule.
async fn run<S: Sessions>(mut task: Task<S>, status: watch::Sender<Option<LinkStatus>>) {
    let mut failures = 0_u32;
    loop {
        let view = task.view().await;
        let session = match &view {
            View::Session(id) => *id,
            // No hello found its key: it is off, out of mDNS reach, or a
            // Mac that is not listening. A crossing can still dial it.
            View::Dial(addresses) if addresses.is_empty() => {
                status.send_replace(None);
                if !task.wait(&view, None).await {
                    return;
                }
                continue;
            }
            View::Dial(addresses) => {
                // Retries keep showing why the last attempt failed.
                if !matches!(*status.borrow(), Some(LinkStatus::Unreachable { .. })) {
                    status.send_replace(Some(LinkStatus::Connecting));
                }
                match task
                    .sessions
                    .dial(&task.peer, &task.record, addresses)
                    .await
                {
                    Ok(id) => id,
                    // The peer's own dial landed meanwhile and won.
                    Err(_) if task.sessions.current(&task.peer).await.is_some() => continue,
                    Err(error) => {
                        failures += 1;
                        let down = Some(LinkStatus::Unreachable {
                            reason: crate::link::reason(&error),
                            needs_fix: crate::link::Fix::of(&error).is_some(),
                        });
                        // Repeats of the same failure stay out of the journal.
                        if *status.borrow() != down {
                            tracing::info!(peer = %task.peer, error = %format_args!("{error:#}"),
                                failures, "input link could not connect");
                        }
                        status.send_replace(down);
                        if !task.wait(&view, Some(retry_delay(failures))).await {
                            return;
                        }
                        continue;
                    }
                }
            }
        };
        status.send_replace(None);
        let opened = tokio::time::Instant::now();
        if !task.wait(&View::Session(session), None).await {
            return;
        }
        // A session that keeps dropping right after it opens backs off like
        // a failed attempt.
        failures = if opened.elapsed() >= STABLE_SESSION {
            0
        } else {
            failures + 1
        };
        let view = task.view().await;
        if failures > 0
            && matches!(view, View::Dial(_))
            && !task.wait(&view, Some(retry_delay(failures))).await
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::TransportError;
    use tokio::{sync::oneshot, time::Instant as Clock};

    const SAVED: &str = "192.0.2.7:43119";
    const DESK: &[u8] = &[1, 2, 3];
    const OTHER: &[u8] = &[9, 9, 9];

    fn record() -> PeerConfig {
        PeerConfig::from_spki(
            DESK,
            vec![SAVED.parse().unwrap()],
            PeerPermissions {
                connect: true,
                send_normal: true,
                receive_normal: true,
                inject_prelogin: false,
            },
        )
        .unwrap()
    }

    /// The computers around: each mDNS record, with the key its hello proved.
    fn around(records: &[(&str, &[u8], &str)]) -> Neighbors {
        let mut neighbors = Neighbors::new(&[0]);
        let hello = crate::hello::make_hello("desk", 43119, Vec::new(), false);
        for &(instance, spki, address) in records {
            let address: SocketAddr = address.parse().unwrap();
            neighbors.instance_seen(instance, vec![address], true, None);
            let now = tokio::time::Instant::now();
            neighbors.hello(spki, address, &hello, Some(instance), now);
        }
        neighbors
    }

    type Dial = (Vec<SocketAddr>, oneshot::Sender<Result<u64>>);

    /// Stands in for the daemon: the test answers each dial, and sets the
    /// peer's session the way an inbound connection would.
    struct Fake {
        session: std::sync::Mutex<Option<u64>>,
        changes: watch::Sender<()>,
        dials: mpsc::UnboundedSender<Dial>,
    }

    impl Fake {
        fn set_session(&self, id: Option<u64>) {
            *self.session.lock().unwrap() = id;
            self.changes.send_replace(());
        }
    }

    impl Sessions for Fake {
        async fn current(&self, _peer: &str) -> Option<u64> {
            *self.session.lock().unwrap()
        }

        async fn dial(&self, _: &str, _: &PeerConfig, addresses: &[SocketAddr]) -> Result<u64> {
            let (answer, result) = oneshot::channel();
            self.dials.send((addresses.to_vec(), answer)).unwrap();
            let id = result.await.unwrap()?;
            self.set_session(Some(id));
            Ok(id)
        }
    }

    struct Harness {
        fake: Arc<Fake>,
        link: Link,
        neighbors: watch::Sender<Neighbors>,
        dials: mpsc::UnboundedReceiver<Dial>,
    }

    fn harness(neighbors: Neighbors, session: Option<u64>) -> Harness {
        let (dial_sender, dials) = mpsc::unbounded_channel();
        let fake = Arc::new(Fake {
            session: std::sync::Mutex::new(session),
            changes: watch::Sender::new(()),
            dials: dial_sender,
        });
        let neighbors = watch::Sender::new(neighbors);
        let link = start(
            fake.clone(),
            "desk".into(),
            record(),
            fake.changes.subscribe(),
            neighbors.subscribe(),
        );
        Harness {
            fake,
            link,
            neighbors,
            dials,
        }
    }

    fn unreachable(reason: &str, needs_fix: bool) -> Option<LinkStatus> {
        Some(LinkStatus::Unreachable {
            reason: reason.into(),
            needs_fix,
        })
    }

    #[test]
    fn paused_sharing_and_forgotten_peers_get_no_link() {
        let mut config = Config::default();
        config.peers.insert("desk".into(), record());
        let mut one_way = record();
        one_way.permissions.receive_normal = false;
        config.peers.insert("watcher".into(), one_way);
        assert_eq!(
            linked_peers(&config).into_keys().collect::<Vec<_>>(),
            ["desk"]
        );
        config.daemon.sharing = false;
        assert!(linked_peers(&config).is_empty());
        config.daemon.sharing = true;
        config.peers.remove("desk");
        assert!(linked_peers(&config).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_link_backs_off_retries_on_request_and_redials_a_lost_session_at_once() {
        let mut test = harness(around(&[("zf-desk", DESK, SAVED)]), None);
        let (addresses, answer) = test.dials.recv().await.unwrap();
        assert_eq!(addresses, [SAVED.parse().unwrap()]);
        assert_eq!(*test.link.status.borrow(), Some(LinkStatus::Connecting));
        answer
            .send(Err(TransportError::InvalidAlpn.into()))
            .unwrap();

        let failed = Clock::now();
        let (_, answer) = test.dials.recv().await.unwrap();
        assert_eq!(failed.elapsed(), Duration::from_secs(1));
        // The retry keeps showing why the last attempt failed.
        assert_eq!(
            *test.link.status.borrow(),
            unreachable("Different zflow version. Update both computers.", true)
        );
        answer.send(Err(anyhow!("timed out"))).unwrap();

        // Retry skips the two seconds the second failure waits.
        let failed = Clock::now();
        test.link.retry.send(()).unwrap();
        let (_, answer) = test.dials.recv().await.unwrap();
        assert_eq!(failed.elapsed(), Duration::ZERO);
        assert_eq!(*test.link.status.borrow(), unreachable("timed out", false));
        answer.send(Ok(7)).unwrap();
        test.link.status.wait_for(Option::is_none).await.unwrap();

        // A session that worked for a while is redialed at once.
        tokio::time::sleep(STABLE_SESSION).await;
        test.fake.set_session(None);
        let lost = Clock::now();
        let (_, answer) = test.dials.recv().await.unwrap();
        assert_eq!(lost.elapsed(), Duration::ZERO);
        answer.send(Ok(8)).unwrap();

        // One that drops right away waits like a failed attempt.
        test.link.status.wait_for(Option::is_none).await.unwrap();
        test.fake.set_session(None);
        let lost = Clock::now();
        let (_, _answer) = test.dials.recv().await.unwrap();
        assert_eq!(lost.elapsed(), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_link_dials_its_key_wherever_it_is_and_nowhere_else() {
        // Another computer took the peer's saved address, and the peer, a
        // Mac that is not listening, advertises nothing.
        let mut test = harness(around(&[("zf-other", OTHER, SAVED)]), None);
        test.link.retry.send(()).unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(
            test.dials.try_recv().is_err(),
            "neither a computer that is not listening nor its namesake address is dialed"
        );
        assert_eq!(*test.link.status.borrow(), None);

        // The Mac's own link arrives; it is the session.
        test.fake.set_session(Some(3));
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(test.dials.try_recv().is_err());

        // Without a session, the peer is dialed where its hello says it is
        // now, an address it was never saved at.
        test.fake.set_session(None);
        let moved = "192.0.2.70:43119";
        test.neighbors.send_replace(around(&[
            ("zf-other", OTHER, SAVED),
            ("zf-desk", DESK, moved),
        ]));
        let (addresses, answer) = test.dials.recv().await.unwrap();
        assert_eq!(addresses, [moved.parse().unwrap()]);
        answer.send(Ok(4)).unwrap();
    }

    #[tokio::test]
    async fn learning_an_address_restarts_neither_the_link_nor_sessions() {
        let (shared, _kept) = test_daemon();
        let directory = tempfile::tempdir().unwrap();
        let desk = Identity::load_or_create(directory.path()).unwrap();
        // Saved where nothing listens, so the link dials nothing real.
        let saved: SocketAddr = "127.0.0.1:9".parse().unwrap();
        {
            let _mutation = shared.config_mutation.lock().await;
            let mut config = shared.config.read().await.clone();
            crate::hello::trust_peer(&mut config, desk.spki(), "desk", &[saved]).unwrap();
            shared.apply_config_locked(config, true).await.unwrap();
        }
        let link = shared.links.lock().await["desk"].retry.clone();
        let generation = shared.policy_generation.load(Ordering::Acquire);

        let moved: SocketAddr = "127.0.0.2:9".parse().unwrap();
        shared.learn_address("desk", moved).await;
        let config = shared.config.read().await.clone();
        assert_eq!(config.peers["desk"].addresses, [moved, saved]);
        assert_eq!(Config::load(&shared.config_path).unwrap(), config);
        assert_eq!(shared.policy_generation.load(Ordering::Acquire), generation);
        // The next change of something else syncs the links again.
        let mut paused = config;
        paused.switching.pause_at_edges = true;
        let _mutation = shared.config_mutation.lock().await;
        shared.apply_config_locked(paused, true).await.unwrap();
        assert!(shared.links.lock().await["desk"].retry.same_channel(&link));
    }

    #[tokio::test(start_paused = true)]
    async fn a_stopped_link_stops_dialing() {
        let mut test = harness(around(&[("zf-desk", DESK, SAVED)]), None);
        let (_, answer) = test.dials.recv().await.unwrap();
        answer.send(Err(anyhow!("timed out"))).unwrap();
        let down =
            |status: &Option<LinkStatus>| matches!(status, Some(LinkStatus::Unreachable { .. }));
        test.link.status.wait_for(down).await.unwrap();
        // As when the peer is forgotten or sharing pauses, mid-backoff. The
        // task ends, which drops its side of the status.
        let Link {
            retry, mut status, ..
        } = test.link;
        drop(retry);
        while status.changed().await.is_ok() {}
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(test.dials.try_recv().is_err());
    }
}
