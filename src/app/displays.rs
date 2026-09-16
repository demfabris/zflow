use std::{
    collections::BTreeMap,
    net::IpAddr,
    sync::{Arc, Mutex},
};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

#[cfg(any(target_os = "macos", test))]
use crate::config::Config;
use crate::discovery::local_unicast_addresses;

mod desktop;
pub(super) use desktop::DesktopDetector;

const SERVICE: &str = "_zflow-display._udp.local.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Desktop {
    pub width: u32,
    pub height: u32,
}

impl Desktop {
    pub fn valid(&self) -> bool {
        (1..=16384).contains(&self.width) && (1..=16384).contains(&self.height)
    }
}

#[derive(Clone)]
struct Report {
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    addresses: Vec<IpAddr>,
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    desktop: Desktop,
}

#[derive(Default)]
struct State {
    reports: BTreeMap<String, Report>,
    error: Option<String>,
}

#[derive(Default)]
pub(super) struct DisplayDiscovery {
    state: Arc<Mutex<State>>,
    stop: Option<oneshot::Sender<()>>,
    pub local: Option<Desktop>,
}

impl DisplayDiscovery {
    pub fn update(&mut self, local: Option<Desktop>, enabled: bool) {
        let local = local.filter(Desktop::valid);
        let should_run = enabled && local.is_some();
        if self.local == local && self.stop.is_some() == should_run {
            return;
        }
        self.stop();
        self.local = local;
        if !enabled {
            return;
        }
        let Some(local) = local else { return };
        let (sender, receiver) = oneshot::channel();
        self.stop = Some(sender);
        let state = self.state.clone();

        if let Err(error) = std::thread::Builder::new()
            .name("zflow-displays".into())
            .spawn(move || {
                let result = (|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?
                        .block_on(browse(state.clone(), local, receiver))
                })();
                if let Err(error) = result {
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    state.reports.clear();
                    state.error = Some(format!("{error:#}"));
                }
            })
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.error = Some(format!("Could not start desktop discovery: {error}"));
        }
    }

    pub(super) fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.state = Arc::new(Mutex::new(State::default()));
    }

    #[cfg(target_os = "macos")]
    pub fn remote(&self, config: &Config) -> BTreeMap<String, Desktop> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match_reports(&state.reports, config)
    }

    pub fn error(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .clone()
    }
}

impl Drop for DisplayDiscovery {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(any(target_os = "macos", test))]
fn match_reports(reports: &BTreeMap<String, Report>, config: &Config) -> BTreeMap<String, Desktop> {
    let mut matches = BTreeMap::new();
    for (name, peer) in &config.peers {
        let candidates: Vec<_> = reports
            .values()
            .filter(|report| {
                has_peer_address(report, peer)
                    && config
                        .peers
                        .values()
                        .filter(|other| has_peer_address(report, other))
                        .count()
                        == 1
            })
            .collect();
        if let [report] = candidates.as_slice() {
            matches.insert(name.clone(), report.desktop);
        }
    }
    matches
}

#[cfg(any(target_os = "macos", test))]
fn has_peer_address(report: &Report, peer: &crate::config::PeerConfig) -> bool {
    peer.addresses.iter().any(|address| {
        report
            .addresses
            .iter()
            .any(|candidate| candidate.to_canonical() == address.ip().to_canonical())
    })
}

fn parse_report(service: &mdns_sd::ResolvedService) -> Option<Report> {
    let properties = service.get_properties();
    if !service.get_fullname().ends_with(&format!(".{SERVICE}"))
        || properties.len() != 2
        || properties
            .iter()
            .map(|p| p.key().len() + p.val().map_or(0, <[u8]>::len))
            .sum::<usize>()
            > 128
        || service.get_property_val_str("v")? != "2"
    {
        return None;
    }
    let desktop: Desktop = serde_json::from_str(service.get_property_val_str("desktop")?).ok()?;
    if !desktop.valid() {
        return None;
    }
    Some(Report {
        addresses: service
            .get_addresses()
            .iter()
            .take(16)
            .map(|address| address.to_ip_addr())
            .collect(),
        desktop,
    })
}

async fn browse(
    state: Arc<Mutex<State>>,
    local: Desktop,
    mut stop: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let addresses = local_unicast_addresses()?;
    let daemon = ServiceDaemon::new()?;
    let result = async {
        let mut entropy = [0u8; 16];
        getrandom::fill(&mut entropy).map_err(|error| anyhow::anyhow!("Display discovery entropy: {error}"))?;
        let instance: String = entropy.iter().map(|b| format!("{b:02x}")).collect();
        let properties = std::collections::HashMap::from([
            ("v".to_owned(), "2".to_owned()),
            ("desktop".to_owned(), serde_json::to_string(&local)?),
        ]);
        // This service carries TXT data only. It exposes no input listener.
        let info = ServiceInfo::new(SERVICE, &instance, &format!("{instance}.local."), addresses.as_slice(), 9, properties)?;
        daemon.register(info)?;
        let events = daemon.browse(SERVICE)?;
        let errors = daemon.monitor()?;
        loop {
            tokio::select! {
                _ = &mut stop => break,
                error = errors.recv_async() => {
                    match error? {
                        mdns_sd::DaemonEvent::Error(error) => anyhow::bail!("{error}"),
                        _ => continue,
                    }
                }
                event = events.recv_async() => {
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    match event? {
                        ServiceEvent::ServiceResolved(service) => {
                            state.reports.remove(service.get_fullname());
                            if let Some(report) = parse_report(&service)
                                && !report.addresses.iter().any(|address| addresses.contains(address))
                                && state.reports.len() < 64 {
                                    state.reports.insert(service.get_fullname().to_owned(), report);
                                }
                        }
                        ServiceEvent::ServiceRemoved(_, name) => { state.reports.remove(&name); }
                        _ => {}
                    }

                }
            }
        }
        Ok::<_, anyhow::Error>(())
    }.await;
    if let Ok(stopped) = daemon.shutdown() {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), stopped.recv_async()).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_records_reject_old_versions_extra_fields_and_invalid_sizes() {
        let make = |entries: Vec<(String, String)>| {
            ServiceInfo::new(
                SERVICE,
                "random",
                "random.local.",
                "192.0.2.1",
                9,
                entries.as_slice(),
            )
            .unwrap()
            .as_resolved_service()
        };
        let display = r#"{"width":2880,"height":1620}"#.to_owned();
        let valid = vec![
            ("v".into(), "2".into()),
            ("desktop".into(), display.clone()),
        ];
        assert_eq!(
            parse_report(&make(valid.clone())).unwrap().desktop,
            Desktop {
                width: 2880,
                height: 1620
            }
        );
        let mut extra = valid.clone();
        extra.push(("identity".into(), "not-allowed".into()));
        assert!(parse_report(&make(extra)).is_none());
        for invalid in [
            display.replace("2880", "0"),
            display.replace("2880", "16385"),
            display.replace("2880", "-1"),
            display.replace('}', ",\"identity\":\"nope\"}"),
            " ".repeat(129) + &display,
        ] {
            assert!(
                parse_report(&make(vec![
                    ("v".into(), "2".into()),
                    ("desktop".into(), invalid)
                ]))
                .is_none()
            );
        }
        assert!(
            parse_report(&make(vec![
                ("v".into(), "1".into()),
                ("d0".into(), display.clone())
            ]))
            .is_none()
        );
        let mut many = vec![("v".into(), "1".into())];
        many.extend((0..9).map(|i| (format!("d{i}"), display.clone())));
        assert!(parse_report(&make(many)).is_none());
    }

    #[test]
    fn desktop_bounds_match_layout_limits() {
        assert!(
            Desktop {
                width: 16384,
                height: 16384
            }
            .valid()
        );
        assert!(
            !Desktop {
                width: 16385,
                height: 16384
            }
            .valid()
        );
        assert!(
            !Desktop {
                width: 1,
                height: 0
            }
            .valid()
        );
    }

    #[test]
    fn discovery_matches_only_one_saved_peer_and_never_adds_pairing() {
        let address = "192.0.2.1:43119".parse().unwrap();
        let mut config = Config::default();
        let record =
            crate::config::PeerConfig::from_spki(b"test", vec![address], Default::default())
                .unwrap();
        config.peers.insert("mac".into(), record.clone());
        let reports = BTreeMap::from([(
            "random".into(),
            Report {
                addresses: vec![address.ip()],
                desktop: Desktop {
                    width: 3000,
                    height: 2000,
                },
            },
        )]);
        assert_eq!(match_reports(&reports, &config).len(), 1);
        let mut duplicate_reports = reports.clone();
        duplicate_reports.insert("another-instance".into(), reports["random"].clone());
        assert!(match_reports(&duplicate_reports, &config).is_empty());
        config.peers.get_mut("mac").unwrap().addresses =
            vec!["[::ffff:192.0.2.1]:43119".parse().unwrap()];
        assert_eq!(match_reports(&reports, &config).len(), 1);
        config.peers.insert("ambiguous".into(), record);
        assert!(match_reports(&reports, &config).is_empty());
        assert!(match_reports(&reports, &Config::default()).is_empty());
    }
}
