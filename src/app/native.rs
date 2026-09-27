use super::{
    handoff,
    layout_model::{self, LayoutDocument, Monitor},
    model::ConfigDocument,
    nearby::NearbyBrowser,
    pairing::Pairing,
    sharing::{self, Observer},
};
use crate::macos::{LinkState, Links, LocalNetwork};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, Instant},
};

const MAINTENANCE: Duration = Duration::from_secs(2);
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5);
/// Once allowed, the probe only watches for access being turned off, and each
/// probe that goes through is a packet on the network.
const LOCAL_NETWORK_RECHECK: Duration = Duration::from_secs(60);
/// Each refused tap leaks a Mach port inside CoreGraphics.
const TAP_RECHECK: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Request {
    Snapshot,
    Reload,
    SetSharing {
        enabled: bool,
    },
    SetAwdl {
        enabled: bool,
    },
    HelperReady {
        ready: bool,
    },
    Move {
        id: String,
        x: i32,
        y: i32,
        tolerance: u32,
    },
    PairStart {
        /// Absent to listen; otherwise an IP address, with the port optional.
        address: Option<String>,
        /// The code shown on the other computer, when connecting.
        code: Option<String>,
    },
    PairCancel,
    Forget {
        name: String,
    },
    AllowAccessibility,
    Retry,
    /// Rechecks Accessibility now instead of at the next maintenance tick.
    CheckAccessibility,
    /// Starts looking for computers and checks Local Network access now.
    Discover,
}

pub(crate) struct NativeApp {
    document: ConfigDocument,
    layout: LayoutDocument,
    // The observer drops first, so a live crossing is told to return input
    // before the links wait for their sessions to close.
    observer: Observer,
    links: Links,
    nearby: NearbyBrowser,
    pairing: Pairing,
    /// Receiver desktop sizes read over each peer's session.
    desktops: BTreeMap<String, (u32, u32)>,
    accessibility: bool,
    tap_refused: Option<Instant>,
    /// Set once setup asks to look for computers.
    discover: bool,
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
        if document.is_new() {
            document.save()?;
        }
        let layout = LayoutDocument::beside(&document.path)?;
        let mut app = Self {
            document,
            layout,
            observer: Observer::default(),
            links: Links::new()?,
            nearby: NearbyBrowser::default(),
            pairing: Pairing::default(),
            desktops: BTreeMap::new(),
            accessibility: crate::macos::accessibility_authorized(false),
            tap_refused: None,
            discover: false,
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
            Request::Move {
                id,
                x,
                y,
                tolerance,
            } => {
                let previous = self.layout.draft.clone();
                let index = self
                    .layout
                    .draft
                    .monitors
                    .iter()
                    .position(|m| m.id == id)
                    .context("Unknown computer")?;
                let (x, y) = self
                    .layout
                    .draft
                    .snap_move(index, x, y, tolerance.min(2048) as i32)
                    .context("Computers cannot overlap")?;
                self.layout.draft.monitors[index].x = x;
                self.layout.draft.monitors[index].y = y;
                if let Err(error) = self.layout.save() {
                    self.layout.draft = previous;
                    return Err(error);
                }
                self.restart();
            }
            Request::PairStart { address, code } => {
                let remote = address
                    .as_deref()
                    .map(crate::pairing::parse_pairing_address)
                    .transpose()?;
                if let Some(remote) = remote {
                    ensure!(remote.port() != 0, "Enter the other computer's IP address");
                }
                self.pairing
                    .start(self.document.path.clone(), remote, code)?;
            }
            Request::PairCancel => self.pairing.cancel(),
            Request::Forget { name } => {
                self.document.draft.peers.remove(&name);
                self.save_config()?;
                self.restart();
            }
            Request::AllowAccessibility => {
                crate::macos::accessibility_authorized(true);
                self.accessibility = self.accessibility_granted();
            }
            Request::Retry => {
                self.crossing_error = None;
                self.links.retry();
                self.restart();
            }
            Request::CheckAccessibility => self.accessibility = self.accessibility_granted(),
            Request::Discover => {
                self.discover = true;
                self.sync_discovery();
                if self.discovers() {
                    self.check_local_network();
                }
            }
        }
        Ok(self.snapshot())
    }

    /// AXIsProcessTrusted can stay true after zflow is removed from the
    /// Accessibility list. A crossing's tap then blocks the Mac's input, and
    /// new crossings fail. The window server knows, so ask it for a tap while
    /// sharing is armed and until access comes back.
    fn accessibility_granted(&mut self) -> bool {
        if !crate::macos::accessibility_authorized(false) {
            return false;
        }
        if self.accessibility && !self.observer.is_active() {
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
        self.links.set_nearby(self.nearby_addresses());
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
        let config = self.document.saved();
        let sharing = config.macos.sharing && !self.emergency_paused;
        self.links.sync(sharing.then_some(config));
    }

    fn reload(&mut self) {
        match ConfigDocument::open(self.document.path.clone()).and_then(|doc| {doc.validate()?; Ok(doc)}) {
            Ok(document) if !document.is_new() => {
                if document.saved()!=self.document.saved() { self.document=document; self.restart(); }
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
            // Also a fallback for display and permission changes that sent no callback.
            sharing::forget_geometry();
            self.accessibility = self.accessibility_granted();
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
        if self.links.changed() {
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
            if let Err(error) = self.sync_layout() {
                self.layout_error = Some(format!("{error:#}"));
            }
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
        if self.emergency_paused || !config.macos.sharing || self.pairing.active() {
            return;
        }
        if config.peers.is_empty() || !self.accessibility {
            self.observer.stop();
            return;
        }
        // A missing helper only costs Wi-Fi latency, so it never holds sharing back.
        // Crossings read this when they start, so a later helper needs no restart.
        self.observer.reduce_wifi_latency = config.macos.block_awdl && self.helper_ready;
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

    /// Discovered receivers let a connection find a peer whose address changed.
    /// Each connection still pins the peer's key, so other hosts are rejected.
    fn nearby_addresses(&self) -> Vec<SocketAddr> {
        let records = self.nearby.snapshot().records.into_values();
        records
            .filter(|record| record.compatible)
            .flat_map(|record| record.addresses)
            .collect()
    }

    fn sync_layout(&mut self) -> Result<()> {
        if self.layout_error.is_some() {
            return Ok(());
        }
        let Ok(local) = sharing::local_geometry().and_then(|geometry| geometry.bounds()) else {
            return Ok(());
        };
        let fits = |width: u32, height: u32| {
            (1..=layout_model::MAX_DIMENSION).contains(&width)
                && (1..=layout_model::MAX_DIMENSION).contains(&height)
        };
        if !fits(local.width, local.height) {
            return Ok(());
        }
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
                Some((local.width, local.height))
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
            if (layout_model::Layout {
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
            if self.observer.is_active() {
                self.restart();
            }
        }
        Ok(())
    }

    fn snapshot(&self) -> Value {
        let config = self.document.saved();
        let layout_issue = sharing::local_geometry()
            .and_then(|g| handoff::validate(&self.layout.draft, &g))
            .err()
            .map(|e| e.to_string());
        let mut ready = false;
        let mut connecting = false;
        let mut link_errors = Vec::new();
        for (name, state) in self.links.states() {
            match state {
                LinkState::Ready(_) => ready = true,
                LinkState::Connecting => connecting = true,
                LinkState::Down(error) => link_errors.push(format!("{name}: {error}")),
            }
        }
        let receiver_error = self
            .crossing_error
            .clone()
            .or_else(|| (!link_errors.is_empty()).then(|| link_errors.join("\n")));
        let checking = !ready && connecting;
        let sharing = config.macos.sharing && !self.emergency_paused;
        let notice = if sharing && !self.observer.is_active() {
            "Sharing starts when the checks above pass."
        } else {
            &self.observer.notice
        };
        let status = if !sharing {
            "paused"
        } else if config.peers.is_empty() {
            "setup"
        } else if !self.accessibility
            || (config.macos.block_awdl && !self.helper_ready)
            || receiver_error.is_some()
            || self.config_error.is_some()
            || self.layout_error.is_some()
        {
            "attention"
        } else if self.observer.has_session() {
            "sharing"
        } else if self.observer.is_enabled() && ready {
            "ready"
        } else if checking {
            "checking"
        } else {
            "attention"
        };
        json!({
            "config_path":self.document.path,"layout_path":self.layout.path,"status":status,
            "sharing":sharing,"block_awdl":config.macos.block_awdl,"accessibility":self.accessibility,
            "notice":notice,"peers":config.peers.keys().collect::<Vec<_>>(),
            "layout":self.layout.draft,"pairing":self.pairing.snapshot(),
            "nearby":self.nearby.snapshot().records.values().collect::<Vec<_>>(),
            "config_error":self.config_error,"layout_error":self.layout_error.as_ref().or(layout_issue.as_ref()),
            "receiver_error":receiver_error,"receiver_checked":ready && receiver_error.is_none(),"checking":checking,
            "local_network":self.local_network,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(snapshot["local_network"], "unknown");
    }
}
