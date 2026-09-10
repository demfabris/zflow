use std::{
    collections::BTreeMap,
    net::IpAddr,
    sync::{Arc, Mutex},
};

use eguicn::egui;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::{config::Config, discovery::local_unicast_addresses};

const SERVICE: &str = "_zflow-display._udp.local.";
const MAX_DISPLAYS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Display {
    pub width: u32,
    pub height: u32,
    pub scale_milli: u32,
}

impl Display {
    pub fn valid(&self) -> bool {
        (1..=16384).contains(&self.width)
            && (1..=16384).contains(&self.height)
            && (1000..=8000).contains(&self.scale_milli)
    }

    pub fn logical_size(&self) -> (u32, u32) {
        (
            (self.width * 1000 / self.scale_milli).max(1),
            (self.height * 1000 / self.scale_milli).max(1),
        )
    }
}

#[derive(Clone)]
struct Report {
    addresses: Vec<IpAddr>,
    displays: Vec<Display>,
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
    pub local: Vec<Display>,
}

impl DisplayDiscovery {
    pub fn update(&mut self, ctx: &egui::Context, local: Vec<Display>, enabled: bool) {
        let local: Vec<_> = local
            .into_iter()
            .filter(Display::valid)
            .take(MAX_DISPLAYS)
            .collect();
        if self.local == local && self.stop.is_some() == enabled {
            return;
        }
        self.stop();
        self.local = local.clone();
        if !enabled || local.is_empty() {
            return;
        }
        let (sender, receiver) = oneshot::channel();
        self.stop = Some(sender);
        let state = self.state.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = (|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(browse(state.clone(), ctx.clone(), local, receiver))
            })();
            if let Err(error) = result {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                state.reports.clear();
                state.error = Some(format!("{error:#}"));
                ctx.request_repaint();
            }
        });
    }

    pub(super) fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.state = Arc::new(Mutex::new(State::default()));
    }

    pub fn remote(&self, config: &Config) -> BTreeMap<String, Vec<Display>> {
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

fn match_reports(
    reports: &BTreeMap<String, Report>,
    config: &Config,
) -> BTreeMap<String, Vec<Display>> {
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
            matches.insert(name.clone(), report.displays.clone());
        }
    }
    matches
}

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
        || properties.len() < 2
        || properties.len() > MAX_DISPLAYS + 1
        || properties
            .iter()
            .map(|p| p.key().len() + p.val().map_or(0, <[u8]>::len))
            .sum::<usize>()
            > 768
        || service.get_property_val_str("v")? != "1"
    {
        return None;
    }
    let mut displays = Vec::new();
    for index in 0..properties.len() - 1 {
        let display: Display =
            serde_json::from_str(service.get_property_val_str(&format!("d{index}"))?).ok()?;
        if !display.valid() {
            return None;
        }
        displays.push(display);
    }
    Some(Report {
        addresses: service
            .get_addresses()
            .iter()
            .take(16)
            .map(|address| address.to_ip_addr())
            .collect(),
        displays,
    })
}

async fn browse(
    state: Arc<Mutex<State>>,
    ctx: egui::Context,
    local: Vec<Display>,
    mut stop: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let addresses = local_unicast_addresses()?;
    let daemon = ServiceDaemon::new()?;
    let result = async {
        let mut entropy = [0u8; 16];
        getrandom::fill(&mut entropy).map_err(|error| anyhow::anyhow!("Display discovery entropy: {error}"))?;
        let instance: String = entropy.iter().map(|b| format!("{b:02x}")).collect();
        let mut properties = std::collections::HashMap::from([("v".to_owned(), "1".to_owned())]);
        for (index, display) in local.iter().enumerate() {
            properties.insert(format!("d{index}"), serde_json::to_string(display)?);
        }
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
                            if let Some(report) = parse_report(&service)
                                && !report.addresses.iter().any(|address| addresses.contains(address))
                                && (state.reports.len() < 64 || state.reports.contains_key(service.get_fullname())) {
                                    state.reports.insert(service.get_fullname().to_owned(), report);
                                }
                        }
                        ServiceEvent::ServiceRemoved(_, name) => { state.reports.remove(&name); }
                        _ => {}
                    }
                    ctx.request_repaint();
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
    fn display_records_reject_extra_fields_invalid_sizes_and_too_many_displays() {
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
        let display = r#"{"width":3840,"height":2160,"scale_milli":2000}"#.to_owned();
        let valid = vec![("v".into(), "1".into()), ("d0".into(), display.clone())];
        assert_eq!(
            parse_report(&make(valid.clone())).unwrap().displays[0].logical_size(),
            (1920, 1080)
        );
        let mut extra = valid.clone();
        extra.push(("identity".into(), "not-allowed".into()));
        assert!(parse_report(&make(extra)).is_none());
        assert!(
            parse_report(&make(vec![
                ("v".into(), "1".into()),
                ("d0".into(), display.replace("3840", "0"))
            ]))
            .is_none()
        );
        let mut many = vec![("v".into(), "1".into())];
        many.extend((0..9).map(|i| (format!("d{i}"), display.clone())));
        assert!(parse_report(&make(many)).is_none());
    }

    #[test]
    fn retina_displays_use_logical_size_without_losing_resolution() {
        let display = Display {
            width: 3840,
            height: 2160,
            scale_milli: 2000,
        };
        assert_eq!(display.logical_size(), (1920, 1080));
        assert_eq!(display.width, 3840);
        assert!(
            !Display {
                scale_milli: 0,
                ..display
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
                displays: vec![Display {
                    width: 3000,
                    height: 2000,
                    scale_milli: 2000,
                }],
            },
        )]);
        assert_eq!(match_reports(&reports, &config).len(), 1);
        config.peers.get_mut("mac").unwrap().addresses =
            vec!["[::ffff:192.0.2.1]:43119".parse().unwrap()];
        assert_eq!(match_reports(&reports, &config).len(), 1);
        config.peers.insert("ambiguous".into(), record);
        assert!(match_reports(&reports, &config).is_empty());
        assert!(match_reports(&reports, &Config::default()).is_empty());
    }
}
