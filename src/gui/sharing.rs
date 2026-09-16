use std::{
    path::{Path, PathBuf},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use eguicn::{Button, ButtonVariant, Card, egui};
use tokio::sync::{mpsc, watch};
use tracing::Instrument;

use crate::{
    config::Config,
    desktop::{Geometry, Point, Rect},
    macos::{self, SourceOptions, SourceStatus},
};

use super::{
    background::Background,
    handoff::{self, Handoff},
    layout_model::{Layout, LayoutDocument},
};

struct Running {
    stop: watch::Sender<bool>,
    events: mpsc::UnboundedReceiver<SourceStatus>,
    thread: JoinHandle<()>,
    handoff: Handoff,
    returned: Option<u32>,
    failed: bool,
    cancelled: bool,
    capturing: bool,
    entry_region: macos::DesktopRect,
    span: tracing::Span,
    started: Instant,
}

#[derive(Default)]
pub(super) struct Sharing {
    worker: Option<Background>,
    notices: Option<watch::Receiver<String>>,
    notice: String,
    reduce_wifi_latency: bool,
}

impl Sharing {
    pub fn is_active(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
    }

    pub fn stop(&mut self) {
        if let Some(worker) = &self.worker {
            worker.stop();
            self.notice = "Returning input to the Mac…".into();
        }
    }

    fn refresh(&mut self) {
        let finished = self.worker.as_ref().is_some_and(Background::is_finished);
        let panicked = finished && self.worker.take().unwrap().join().is_err();
        if let Some(notices) = &self.notices {
            self.notice = notices.borrow().clone();
        } else if self.notice.is_empty() {
            self.notice = "Sharing is off.".into();
        }
        if finished {
            self.notices = None;
        }
        if panicked {
            self.notice = "The sharing observer stopped unexpectedly. Sharing is off.".into();
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, path: &Path, config: &Config, editable: bool) {
        self.refresh();
        Card::new().padding(16).show(ui, |ui| {
            ui.set_width(ui.available_width());
            Card::header(ui, "Share from this Mac", &self.notice);
            if config.peers.is_empty() {
                ui.label("Pair a computer below, then arrange your displays in Layout.");
                return;
            }
            if self.is_active() {
                if ui.add(Button::new("Stop sharing").variant(ButtonVariant::Destructive)).clicked() { self.stop(); }
                ui.label("Ctrl+Cmd+Backspace returns input and turns sharing off.");
            } else {
                let permitted = macos::accessibility_authorized(false);
                if !permitted {
                    ui.label("Allow zflow in System Settings → Privacy & Security → Accessibility, then return here.");
                    if ui.add(Button::new("Request Accessibility access").variant(ButtonVariant::Outline)).clicked() {
                        macos::accessibility_authorized(true);
                    }
                }
                ui.add(eguicn::Switch::new(&mut self.reduce_wifi_latency, "Reduce Wi-Fi latency"));
                if self.reduce_wifi_latency {
                    ui.label("Requires the installed AWDL helper. AirDrop and Continuity may pause during sharing.");
                }
                if ui.add_enabled(permitted && editable && !config.peers.is_empty(), Button::new("Enable edge sharing")).clicked()
                    && let Err(error) = self.enable(path, config, ui.ctx()) { self.notice = format!("{error:#}"); }
                ui.label("On Ubuntu, enable desktop handoff. Save the computer layout here, then enable sharing. Keep both apps open.");
                if !editable { ui.label("Save your changes and finish pairing before enabling sharing."); }
            }
        });
    }

    fn enable(&mut self, path: &Path, config: &Config, ctx: &egui::Context) -> Result<()> {
        let mut observer = Observer {
            reduce_wifi_latency: self.reduce_wifi_latency,
            ..Default::default()
        };
        observer.enable(path, config)?;
        let (notices, updates) = watch::channel(observer.notice.clone());
        let ctx = ctx.clone();
        let worker =
            Background::spawn("zflow-edges", Duration::from_millis(12), move |stopping| {
                if stopping && observer.enabled {
                    observer.stop();
                }
                observer.tick();
                let active = observer.is_active();
                if notices.send_if_modified(|notice| {
                    if *notice == observer.notice {
                        return false;
                    }
                    notice.clone_from(&observer.notice);
                    true
                }) || !active
                {
                    ctx.request_repaint();
                }
                active
            })?;
        self.worker = Some(worker);
        self.notices = Some(updates);
        self.refresh();
        Ok(())
    }
}

struct Observer {
    enabled: bool,
    layout: Option<Layout>,
    path: PathBuf,
    running: Option<Running>,
    previous: Option<Point>,
    notice: String,
    reduce_wifi_latency: bool,
}

impl Default for Observer {
    fn default() -> Self {
        Self {
            enabled: false,
            layout: None,
            path: PathBuf::new(),
            running: None,
            previous: None,
            notice: "Sharing is off.".into(),
            reduce_wifi_latency: false,
        }
    }
}

impl Observer {
    fn is_active(&self) -> bool {
        self.enabled || self.running.is_some()
    }

    fn stop(&mut self) {
        self.enabled = false;
        self.previous = None;
        if let Some(running) = &self.running {
            tracing::info!(parent: &running.span, "stop requested by window or user");
            let _ = running.stop.send(true);
            self.notice = "Returning input to the Mac…".into();
        } else {
            self.notice = "Sharing is off.".into();
        }
    }

    fn enable(&mut self, path: &Path, config: &Config) -> Result<()> {
        let document = LayoutDocument::open(path)?;
        anyhow::ensure!(
            !document.is_new(),
            "Save the computer layout before enabling sharing"
        );
        let geometry = local_geometry()?;
        handoff::validate(&document.draft, &geometry)?;
        for transition in document.draft.transitions() {
            if document.draft.monitors[transition.source].peer.is_none() {
                let name = document.draft.monitors[transition.target]
                    .peer
                    .as_ref()
                    .context("Missing target computer")?;
                let peer = config
                    .peers
                    .get(name)
                    .with_context(|| format!("Pair {name} before enabling sharing"))?;
                anyhow::ensure!(
                    peer.permissions.connect && peer.permissions.receive_normal,
                    "{name} does not allow this Mac to send input"
                );
            }
        }
        self.layout = Some(document.draft);
        self.path = path.into();
        self.previous = None;
        self.enabled = true;
        tracing::info!("edge sharing enabled");
        self.notice = "Ready on the Mac. Move through a configured edge to connect.".into();
        Ok(())
    }

    fn tick(&mut self) {
        if let Some(running) = &mut self.running {
            while let Ok(status) = running.events.try_recv() {
                match status {
                    SourceStatus::Connecting => {
                        self.notice = format!("Connecting to {}…", running.handoff.peer)
                    }
                    SourceStatus::Sharing => {
                        running.capturing = true;
                        tracing::info!(parent: &running.span, elapsed_ms = running.started.elapsed().as_millis() as u64, "edge observer saw capture start");
                        self.notice = format!(
                            "Sharing with {}. Cross back or press Ctrl+Cmd+Backspace to return.",
                            running.handoff.peer
                        );
                    }
                    SourceStatus::Returned { position } => running.returned = Some(position),
                    SourceStatus::LocalInputRestored => {
                        self.notice = "Back on the Mac. Finishing connection cleanup…".into();
                    }
                    SourceStatus::Stopped => {}
                    SourceStatus::Cancelled(reason) => {
                        if !running.failed {
                            running.cancelled = true;
                            self.notice = format!("Crossing cancelled: {reason}");
                        }
                    }
                    SourceStatus::Failed(error) => {
                        tracing::warn!(parent: &running.span, %error, "edge observer received crossing failure");
                        running.failed = true;
                        self.notice = error;
                    }
                }
            }
            if !local_geometry().is_ok_and(|geometry| running.handoff.matches_geometry(&geometry)) {
                if !running.failed {
                    tracing::warn!(parent: &running.span, "crossing cancelled: Mac display geometry changed");
                }
                let _ = running.stop.send(true);
                self.enabled = false;
                running.failed = true;
                self.notice =
                    "The Mac displays changed. Sharing stopped; refresh and save the layout."
                        .into();
            }
            if !running.capturing
                && !running.failed
                && !running.cancelled
                && let Ok(position) = macos::cursor_position()
                && !running.entry_region.contains(position)
            {
                tracing::warn!(parent: &running.span,
                    elapsed_ms = running.started.elapsed().as_millis() as u64,
                    entry_region = ?running.entry_region,
                    current_x = position.x, current_y = position.y,
                    "crossing cancelled: Mac cursor left the configured edge during preparation");
                let _ = running.stop.send(true);
                running.cancelled = true;
                self.notice =
                    "Crossing cancelled because the Mac cursor left the configured edge.".into();
            }
            if running.thread.is_finished() {
                let mut running = self.running.take().unwrap();
                let span = running.span.clone();
                let _entered = span.enter();
                let joined = running.thread.join();
                while let Ok(status) = running.events.try_recv() {
                    match status {
                        SourceStatus::Returned { position } => running.returned = Some(position),
                        SourceStatus::Cancelled(reason) => {
                            if !running.failed {
                                running.cancelled = true;
                                self.notice = format!("Crossing cancelled: {reason}");
                            }
                        }
                        SourceStatus::Failed(error) => {
                            running.failed = true;
                            self.notice = error;
                        }
                        _ => {}
                    }
                }
                self.previous = None;
                if joined.is_err() {
                    tracing::error!("crossing worker panicked");
                    self.enabled = false;
                    self.notice = "The sharing worker stopped unexpectedly. Sharing is off.".into();
                } else if running.failed {
                    self.enabled = false;
                } else if running.cancelled && self.enabled {
                    tracing::info!("crossing cancelled; edge sharing remains armed");
                } else if running.returned.is_some() && self.enabled {
                    tracing::info!(
                        elapsed_ms = running.started.elapsed().as_millis() as u64,
                        "connection cleanup completed; edge sharing rearmed"
                    );
                    self.notice = "Back on the Mac. Edge sharing is ready.".into();
                } else {
                    self.enabled = false;
                    self.notice = "Sharing is off. Input is on the Mac.".into();
                }
                tracing::info!(enabled = self.enabled, failed = running.failed, cancelled = running.cancelled, returned = running.returned.is_some(), notice = %self.notice, "crossing worker finished");
            }
        }
        if self.enabled
            && self.running.is_none()
            && let Err(error) = self.observe()
        {
            tracing::warn!(error = %format!("{error:#}"), "edge observation failed; sharing disabled");
            self.enabled = false;
            self.previous = None;
            self.notice = format!("Sharing stopped: {error:#}");
        }
    }

    fn observe(&mut self) -> Result<()> {
        let layout = self.layout.as_ref().context("No saved layout")?;
        let geometry = local_geometry()?;
        handoff::validate(layout, &geometry)?;
        let position = macos::cursor_position()?;
        let current = Point {
            x: position.x.floor() as i32,
            y: position.y.floor() as i32,
        };
        let previous = self.previous.replace(current);
        if !macos::input_is_neutral() {
            return Ok(());
        }
        let Some(handoff) =
            previous.and_then(|previous| handoff::crossing(layout, &geometry, previous, current))
        else {
            return Ok(());
        };
        static NEXT_CROSSING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let span = tracing::info_span!("crossing",
            crossing = NEXT_CROSSING.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            peer = %handoff.peer);
        let started = Instant::now();
        tracing::info!(parent: &span, remote_entry_edge = ?handoff.edge, "configured edge reached");
        tracing::debug!(parent: &span, x = position.x, y = position.y,
            start = handoff.start, end = handoff.end, position = handoff.position,
            remote_width = handoff.expected_width, remote_height = handoff.expected_height,
            "crossing geometry");
        let entry_region = macos::DesktopRect {
            x: f64::from(handoff.entry_region.x),
            y: f64::from(handoff.entry_region.y),
            width: f64::from(handoff.entry_region.width),
            height: f64::from(handoff.entry_region.height),
        };
        tracing::debug!(parent: &span, ?entry_region, "capture admission region");
        let options = SourceOptions {
            config_path: self.path.clone(),
            peer: handoff.peer.clone(),
            address: None,
            raw_touch: true,
            reduce_wifi_latency: self.reduce_wifi_latency,
            handoff: Some(macos::HandoffOptions {
                return_mapping: handoff.return_mapping.clone(),
                entry_region,
                edge: handoff.edge,
                start: handoff.start,
                end: handoff.end,
                position: handoff.position,
                expected_width: handoff.expected_width,
                expected_height: handoff.expected_height,
                entry_position: macos::CursorPosition {
                    x: position.x,
                    y: position.y,
                },
            }),
        };
        let (stop, stopped) = watch::channel(false);
        let (status, events) = mpsc::unbounded_channel();
        let worker_span = span.clone();
        let thread = std::thread::Builder::new()
            .name("zflow-sharing".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => {
                        let _ = runtime.block_on(
                            macos::run_controlled(options, stopped, status).instrument(worker_span),
                        );
                    }
                    Err(error) => {
                        let _ = status.send(SourceStatus::Failed(format!(
                            "Could not start sharing: {error}"
                        )));
                    }
                }
            })?;
        self.notice = format!("Connecting to {}…", handoff.peer);
        self.running = Some(Running {
            stop,
            events,
            thread,
            handoff,
            returned: None,
            failed: false,
            cancelled: false,
            capturing: false,
            entry_region,
            span,
            started,
        });
        Ok(())
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.stop();
        if let Some(running) = self.running.take() {
            let _ = running.thread.join();
        }
    }
}

fn local_geometry() -> Result<Geometry> {
    let geometry = Geometry {
        monitors: macos::active_desktop_rectangles()?
            .into_iter()
            .map(|r| Rect {
                x: r.x.round() as i32,
                y: r.y.round() as i32,
                width: r.width.round() as u32,
                height: r.height.round() as u32,
            })
            .collect(),
    };
    geometry.validate()?;
    Ok(geometry)
}
