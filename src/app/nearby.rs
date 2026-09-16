use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::sync::oneshot;

use crate::{
    discovery::{Discovery, DiscoveryEvent, UntrustedCandidate, local_unicast_addresses},
    wire::CURRENT_PROTOCOL_VERSION,
};

const MAX_NEARBY: usize = 64;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum BrowserStatus {
    #[default]
    Paused,
    Starting,
    Browsing,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(super) struct NearbyRecord {
    pub instance: String,
    pub addresses: Vec<SocketAddr>,
    pub compatible: bool,
}

impl NearbyRecord {
    fn from_candidate(candidate: UntrustedCandidate) -> Option<Self> {
        Some(Self {
            instance: candidate.ephemeral_instance_id()?.to_string(),
            addresses: candidate.socket_addresses().to_vec(),
            compatible: candidate
                .protocol_versions()
                .contains(&CURRENT_PROTOCOL_VERSION),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct NearbySnapshot {
    pub status: BrowserStatus,
    pub records: BTreeMap<String, NearbyRecord>,
}

impl NearbySnapshot {
    fn insert(&mut self, record: NearbyRecord, local: &BTreeSet<IpAddr>) {
        if record.addresses.is_empty()
            || record
                .addresses
                .iter()
                .all(|address| address.ip().is_loopback() || local.contains(&address.ip()))
        {
            self.records.remove(&record.instance);
            return;
        }
        if self.records.len() < MAX_NEARBY || self.records.contains_key(&record.instance) {
            self.records.insert(record.instance.clone(), record);
        }
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.status = BrowserStatus::Failed(message.into().chars().take(256).collect());
        self.records.clear();
    }
}

#[derive(Default)]
pub(super) struct NearbyBrowser {
    shared: Arc<Mutex<NearbySnapshot>>,
    stop: Option<oneshot::Sender<()>>,
}

impl NearbyBrowser {
    pub fn snapshot(&self) -> NearbySnapshot {
        self.shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn status(&self) -> BrowserStatus {
        self.snapshot().status
    }

    pub fn is_running(&self) -> bool {
        matches!(
            self.status(),
            BrowserStatus::Starting | BrowserStatus::Browsing
        )
    }

    pub fn start(&mut self) {
        if self.is_running() {
            return;
        }
        self.stop();
        self.shared = Arc::new(Mutex::new(NearbySnapshot {
            status: BrowserStatus::Starting,
            ..NearbySnapshot::default()
        }));
        let (stop, cancelled) = oneshot::channel();
        self.stop = Some(stop);
        let shared = Arc::clone(&self.shared);

        if let Err(error) = std::thread::Builder::new()
            .name("zflow-nearby".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => runtime.block_on(browse(shared, cancelled)),
                    Err(error) => update(&shared, |state| {
                        state.fail(format!("Could not start discovery: {error}"));
                    }),
                }
            })
        {
            self.stop.take();
            update(&self.shared, |state| {
                state.fail(format!("Could not start discovery: {error}"));
            });
        }
    }

    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.shared = Arc::new(Mutex::new(NearbySnapshot::default()));
    }
}

impl Drop for NearbyBrowser {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn update(shared: &Mutex<NearbySnapshot>, change: impl FnOnce(&mut NearbySnapshot)) {
    change(&mut shared.lock().unwrap_or_else(|error| error.into_inner()));
}

async fn browse(shared: Arc<Mutex<NearbySnapshot>>, mut stop: oneshot::Receiver<()>) {
    if !matches!(stop.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
        return;
    }
    let setup = (|| {
        let local = local_unicast_addresses()?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut discovery = Discovery::new()?;
        discovery.browse()?;
        Ok::<_, crate::discovery::DiscoveryError>((discovery, local))
    })();
    let (discovery, local) = match setup {
        Ok(value) => value,
        Err(error) => {
            update(&shared, |state| {
                state.fail(format!("Discovery unavailable: {error}"))
            });
            return;
        }
    };
    update(&shared, |state| state.status = BrowserStatus::Browsing);
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => break,
            event = discovery.next_event() => match event {
                Ok(DiscoveryEvent::Candidate(candidate)) => {
                    if let Some(record) = NearbyRecord::from_candidate(candidate) {
                        update(&shared, |state| state.insert(record, &local));
                    }
                }
                Ok(DiscoveryEvent::Removed(instance)) => {
                    update(&shared, |state| { state.records.remove(&instance.to_string()); });
                }
                Ok(DiscoveryEvent::Stopped) => {
                    update(&shared, |state| state.fail("Network discovery stopped. Choose Resume to retry."));
                    break;
                }
                Err(error) => {
                    update(&shared, |state| state.fail(format!("Discovery unavailable: {error}")));
                    break;
                }
            },
            error = discovery.next_daemon_error() => {
                update(&shared, |state| state.fail(match error {
                    Ok(error) => format!("Discovery unavailable: {error}"),
                    Err(error) => format!("Discovery unavailable: {error}"),
                }));
                break;
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), discovery.shutdown()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(index: usize) -> NearbyRecord {
        NearbyRecord {
            instance: format!("zf-{index:032x}"),
            addresses: vec![
                format!("192.0.2.{}:43119", index % 250 + 1)
                    .parse()
                    .unwrap(),
            ],
            compatible: true,
        }
    }

    #[test]
    fn default_browser_is_offline_and_stop_clears_records() {
        let mut browser = NearbyBrowser::default();
        assert!(!browser.is_running());
        assert!(browser.stop.is_none());
        browser
            .shared
            .lock()
            .unwrap()
            .insert(record(1), &BTreeSet::new());
        browser.stop();
        assert!(browser.snapshot().records.is_empty());
        assert_eq!(browser.status(), BrowserStatus::Paused);
    }

    #[test]
    fn records_are_bounded_but_existing_instances_can_update() {
        let mut snapshot = NearbySnapshot::default();
        for index in 0..100 {
            snapshot.insert(record(index), &BTreeSet::new());
        }
        assert_eq!(snapshot.records.len(), MAX_NEARBY);
        let mut changed = record(0);
        changed.addresses = vec!["192.0.2.250:43119".parse().unwrap()];
        snapshot.insert(changed.clone(), &BTreeSet::new());
        assert_eq!(snapshot.records[&changed.instance], changed);
        snapshot.records.remove(&changed.instance);
        assert_eq!(snapshot.records.len(), MAX_NEARBY - 1);
        snapshot.fail("test error");
        assert!(snapshot.records.is_empty());
        assert_eq!(snapshot.status, BrowserStatus::Failed("test error".into()));
    }

    #[test]
    fn only_fully_local_records_are_hidden() {
        let mut snapshot = NearbySnapshot::default();
        let mut candidate = record(0);
        let local = BTreeSet::from([candidate.addresses[0].ip()]);
        snapshot.insert(candidate.clone(), &local);
        assert!(snapshot.records.is_empty());
        candidate.addresses.push("192.0.2.9:43119".parse().unwrap());
        snapshot.insert(candidate, &local);
        assert_eq!(snapshot.records.len(), 1);
    }

    #[tokio::test]
    async fn cancelled_before_start_opens_no_discovery() {
        let shared = Arc::new(Mutex::new(NearbySnapshot::default()));
        let (stop, cancelled) = oneshot::channel();
        drop(stop);
        browse(Arc::clone(&shared), cancelled).await;
        assert_eq!(shared.lock().unwrap().status, BrowserStatus::Paused);
    }

    #[test]
    fn stale_worker_cannot_restore_records_after_stop() {
        let mut browser = NearbyBrowser::default();
        let previous = Arc::clone(&browser.shared);
        browser.stop();
        previous.lock().unwrap().insert(record(0), &BTreeSet::new());
        assert!(browser.snapshot().records.is_empty());
    }

    #[test]
    fn stop_and_drop_cancel_without_waiting_for_a_worker() {
        let mut browser = NearbyBrowser::default();
        let (sender, mut receiver) = oneshot::channel();
        browser.stop = Some(sender);
        browser.stop();
        assert_eq!(receiver.try_recv(), Ok(()));
        let (sender, mut receiver) = oneshot::channel();
        browser.stop = Some(sender);
        drop(browser);
        assert_eq!(receiver.try_recv(), Ok(()));
    }

    #[test]
    fn protocol_compatibility_comes_from_the_validated_record() {
        for (version, compatible) in [(CURRENT_PROTOCOL_VERSION.0, true), (u16::MAX, false)] {
            let instance = "zf-0123456789abcdef0123456789abcdef";
            let version = version.to_string();
            let service = mdns_sd::ServiceInfo::new(
                crate::discovery::SERVICE_TYPE,
                instance,
                &format!("{instance}.local."),
                "192.0.2.1",
                43119,
                &[("v", version.as_str()), ("cap", "keyboard,pointer")][..],
            )
            .unwrap()
            .as_resolved_service();
            let candidate = crate::discovery::parse_resolved_service(&service).unwrap();
            let record = NearbyRecord::from_candidate(candidate).unwrap();
            assert_eq!(record.compatible, compatible);
        }
    }
}
