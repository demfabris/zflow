use super::{
    displays::{Desktop, DesktopDetector, DisplayDiscovery},
    handoff,
    layout_model::{LayoutDocument, Monitor},
    model::ConfigDocument,
    nearby::NearbyBrowser,
    pairing::Pairing,
    probe::Probe,
    sharing::Observer,
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

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
        address: Option<std::net::SocketAddr>,
    },
    PairConfirm {
        name: String,
        code: String,
    },
    PairCancel,
    Forget {
        name: String,
    },
    AllowAccessibility,
    Retry,
}

pub(crate) struct NativeApp {
    document: ConfigDocument,
    layout: LayoutDocument,
    observer: Observer,
    detector: DesktopDetector,
    displays: DisplayDiscovery,
    nearby: NearbyBrowser,
    pairing: Pairing,
    probe: Option<Probe>,
    desktops: BTreeMap<String, Desktop>,
    checked: bool,
    helper_ready: bool,
    restart: bool,
    emergency_paused: bool,
    config_error: Option<String>,
    layout_error: Option<String>,
    receiver_error: Option<String>,
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
        let layout = LayoutDocument::open(&document.path)?;
        Ok(Self {
            document,
            layout,
            observer: Observer::default(),
            detector: DesktopDetector::default(),
            displays: DisplayDiscovery::default(),
            nearby: NearbyBrowser::default(),
            pairing: Pairing::default(),
            probe: None,
            desktops: BTreeMap::new(),
            checked: false,
            helper_ready: false,
            restart: true,
            emergency_paused: false,
            config_error: None,
            layout_error: None,
            receiver_error: None,
            maintenance: Instant::now() - Duration::from_secs(3),
            retry_at: Instant::now(),
        })
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
            Request::HelperReady { ready } => {
                if ready != self.helper_ready {
                    self.helper_ready = ready;
                    if self.document.saved().macos.block_awdl {
                        self.restart();
                    }
                }
            }
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
            Request::PairStart { address } => {
                if let Some(address) = address {
                    ensure!(address.port() != 0, "Enter an IP address and port");
                }
                self.pairing.start(self.document.path.clone(), address)?;
            }
            Request::PairConfirm { name, code } => self.pairing.confirm(name, code)?,
            Request::PairCancel => self.pairing.cancel(),
            Request::Forget { name } => {
                self.document.draft.peers.remove(&name);
                self.save_config()?;
                self.restart();
            }
            Request::AllowAccessibility => {
                crate::macos::accessibility_authorized(true);
            }
            Request::Retry => self.restart(),
        }
        Ok(self.snapshot())
    }

    fn save_config(&mut self) -> Result<()> {
        if let Err(error) = self.document.save() {
            self.document.draft = self.document.saved().clone();
            return Err(error);
        }
        self.config_error = None;
        Ok(())
    }

    fn restart(&mut self) {
        self.observer.stop();
        self.restart = true;
        self.checked = false;
        self.retry_at = Instant::now();
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
        match LayoutDocument::open(&self.document.path) {
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
        self.observer.tick();
        if was_enabled && !self.observer.is_enabled() {
            self.checked = false;
            if self.observer.pause_requested {
                self.emergency_paused = true;
                self.document.draft.macos.sharing = false;
                // A conflicting external edit must never re-arm an emergency pause.
                if self.save_config().is_err() {
                    self.document.draft.macos.sharing = false;
                }
                self.restart = false;
            } else {
                self.receiver_error = Some(self.observer.notice.clone());
                self.retry_at = Instant::now() + Duration::from_secs(5);
            }
        }
        if self.maintenance.elapsed() >= Duration::from_secs(2) {
            self.maintenance = Instant::now();
            self.reload();
            self.detector.refresh();
            let config = self.document.saved();
            self.displays
                .update(self.detector.snapshot().0, config.transport.discovery);
            if config.transport.discovery {
                self.nearby.start();
            } else {
                self.nearby.stop();
            }
            if let Err(error) = self.sync_layout() {
                self.layout_error = Some(format!("{error:#}"));
            }
        }
        if self.probe.as_ref().is_some_and(Probe::finished) {
            match self.probe.take().unwrap().finish() {
                Ok(result) => {
                    // Results from an older configuration are discarded after a restart.
                    if !self.restart {
                        self.desktops = result.desktops;
                        self.checked = !self.desktops.is_empty();
                        self.receiver_error =
                            (!result.errors.is_empty()).then(|| result.errors.join("\n"));
                        self.retry_at = Instant::now() + Duration::from_secs(5);
                        if let Err(error) = self.sync_layout() {
                            self.layout_error = Some(format!("{error:#}"));
                        }
                    }
                }
                Err(error) => {
                    self.receiver_error = Some(format!("{error:#}"));
                    self.retry_at = Instant::now() + Duration::from_secs(5);
                }
            }
        }
        if self.observer.has_session() || self.probe.is_some() {
            return;
        }
        if self.restart {
            self.restart = false;
        }
        let config = self.document.saved();
        if self.emergency_paused || !config.macos.sharing || self.pairing.active() {
            return;
        }
        if config.peers.is_empty() || !crate::macos::accessibility_authorized(false) {
            self.observer.stop();
            self.checked = false;
            return;
        }
        if config.macos.block_awdl && !self.helper_ready {
            return;
        }
        if !self.checked && !self.observer.is_enabled() && Instant::now() >= self.retry_at {
            match Probe::start(config.clone()) {
                Ok(probe) => self.probe = Some(probe),
                Err(error) => {
                    self.receiver_error = Some(error.to_string());
                    self.retry_at = Instant::now() + Duration::from_secs(5);
                }
            }
        }
        if self.checked && !self.observer.is_enabled() {
            self.observer.reduce_wifi_latency = config.macos.block_awdl;
            let mut available = self.layout.draft.clone();
            available.monitors.retain(|m| {
                m.peer
                    .as_ref()
                    .is_none_or(|peer| self.desktops.contains_key(peer))
            });
            if let Err(error) = self
                .observer
                .enable(&self.document.path, config, &available)
            {
                self.layout_error = Some(format!("{error:#}"));
                if self.receiver_error.is_some() {
                    self.checked = false;
                }
            }
        }
    }

    fn sync_layout(&mut self) -> Result<()> {
        if self.layout_error.is_some() {
            return Ok(());
        }
        let Some(local) = self.detector.snapshot().0 else {
            return Ok(());
        };
        let mut sizes = self.displays.remote(self.document.saved());
        sizes.extend(self.desktops.clone());
        let previous = self.layout.draft.clone();
        let mut monitors = Vec::new();
        let owners = std::iter::once(None).chain(self.document.saved().peers.keys().map(Some));
        for owner in owners {
            let old = previous.monitors.iter().find(|m| m.peer.as_ref() == owner);
            let size = if let Some(name) = owner {
                sizes.get(name).copied().or_else(|| {
                    old.map(|m| Desktop {
                        width: m.width,
                        height: m.height,
                    })
                })
            } else {
                Some(local)
            };
            let Some(size) = size else {
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
                width: size.width,
                height: size.height,
            };
            monitors.push(monitor.clone());
            if (super::layout_model::Layout {
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
        let accessibility = crate::macos::accessibility_authorized(false);
        let layout_issue = super::sharing::local_geometry()
            .and_then(|g| handoff::validate(&self.layout.draft, &g))
            .err()
            .map(|e| e.to_string());
        let sharing = config.macos.sharing && !self.emergency_paused;
        let status = if !sharing {
            "paused"
        } else if config.peers.is_empty() {
            "setup"
        } else if !accessibility
            || (config.macos.block_awdl && !self.helper_ready)
            || self.receiver_error.is_some()
            || self.config_error.is_some()
            || self.layout_error.is_some()
        {
            "attention"
        } else if self.observer.has_session() {
            "sharing"
        } else if self.observer.is_enabled() {
            "ready"
        } else if self.probe.is_some() {
            "checking"
        } else {
            "attention"
        };
        json!({
            "config_path":self.document.path,"layout_path":self.layout.path,"status":status,
            "sharing":sharing,"block_awdl":config.macos.block_awdl,"accessibility":accessibility,
            "notice":self.observer.notice,"peers":config.peers.keys().collect::<Vec<_>>(),
            "layout":self.layout.draft,"pairing":self.pairing.snapshot(),
            "nearby":self.nearby.snapshot().records.values().collect::<Vec<_>>(),
            "config_error":self.config_error,"layout_error":self.layout_error.as_ref().or(layout_issue.as_ref()),
            "receiver_error":self.receiver_error,"receiver_checked":self.checked && self.receiver_error.is_none(),"checking":self.probe.is_some(),
            "discovery_error":self.displays.error(),"desktop_error":self.detector.snapshot().1,
        })
    }
}
