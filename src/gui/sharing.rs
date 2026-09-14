use std::{
    path::{Path, PathBuf},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use eguicn::{Button, ButtonVariant, Card, egui};
use tokio::sync::{mpsc, watch};

use crate::{
    config::Config,
    desktop::{Geometry, Point, Rect},
    macos::{self, SourceOptions, SourceStatus},
};

use super::{
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
    capturing: bool,
    entry_point: Point,
}

pub(super) struct Sharing {
    enabled: bool,
    layout: Option<Layout>,
    path: PathBuf,
    running: Option<Running>,
    previous: Option<Point>,
    last_poll: Instant,
    notice: String,
    reduce_wifi_latency: bool,
}

impl Default for Sharing {
    fn default() -> Self {
        Self {
            enabled: false,
            layout: None,
            path: PathBuf::new(),
            running: None,
            previous: None,
            last_poll: Instant::now(),
            notice: "Sharing is off.".into(),
            reduce_wifi_latency: false,
        }
    }
}

impl Sharing {
    pub fn is_active(&self) -> bool {
        self.enabled || self.running.is_some()
    }

    pub fn stop(&mut self) {
        self.enabled = false;
        self.previous = None;
        if let Some(running) = &self.running {
            let _ = running.stop.send(true);
            self.notice = "Returning input to the Mac…".into();
        } else {
            self.notice = "Sharing is off.".into();
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, path: &Path, config: &Config, editable: bool) {
        self.tick(ui.ctx());
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
                    && let Err(error) = self.enable(path, config) { self.notice = format!("{error:#}"); }
                ui.label("On Ubuntu, enable desktop handoff. Save the computer layout here, then enable sharing. Keep both apps open.");
                if !editable { ui.label("Save your changes and finish pairing before enabling sharing."); }
            }
        });
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
        self.notice = "Ready on the Mac. Move through a configured edge to connect.".into();
        Ok(())
    }

    fn tick(&mut self, ctx: &egui::Context) {
        if let Some(running) = &mut self.running {
            while let Ok(status) = running.events.try_recv() {
                match status {
                    SourceStatus::Connecting => {
                        self.notice = format!("Connecting to {}…", running.handoff.peer)
                    }
                    SourceStatus::Sharing => {
                        running.capturing = true;
                        self.notice = format!(
                            "Sharing with {}. Cross back or press Ctrl+Cmd+Backspace to return.",
                            running.handoff.peer
                        );
                    }
                    SourceStatus::Returned { position } => running.returned = Some(position),
                    SourceStatus::Stopped => {}
                    SourceStatus::Failed(error) => {
                        running.failed = true;
                        self.notice = error;
                    }
                }
            }
            if !local_geometry().is_ok_and(|geometry| running.handoff.matches_geometry(&geometry)) {
                let _ = running.stop.send(true);
                self.enabled = false;
                running.failed = true;
                self.notice =
                    "The Mac displays changed. Sharing stopped; refresh and save the layout."
                        .into();
            }
            if !running.capturing
                && let Ok(position) = macos::cursor_position()
                && ((position.x - f64::from(running.entry_point.x)).abs() > 8.0
                    || (position.y - f64::from(running.entry_point.y)).abs() > 8.0)
            {
                let _ = running.stop.send(true);
                self.enabled = false;
                self.notice =
                    "Crossing cancelled because the Mac cursor moved. Sharing is off.".into();
            }
            if running.thread.is_finished() {
                let mut running = self.running.take().unwrap();
                let joined = running.thread.join();
                while let Ok(status) = running.events.try_recv() {
                    match status {
                        SourceStatus::Returned { position } => running.returned = Some(position),
                        SourceStatus::Failed(error) => {
                            running.failed = true;
                            self.notice = error;
                        }
                        _ => {}
                    }
                }
                self.previous = None;
                if joined.is_err() {
                    self.enabled = false;
                    self.notice = "The sharing worker stopped unexpectedly. Sharing is off.".into();
                } else if running.failed {
                    self.enabled = false;
                } else if let Some(position) = running.returned.filter(|_| self.enabled) {
                    match running.handoff.return_position(position).and_then(|point| {
                        macos::warp_cursor(macos::CursorPosition {
                            x: f64::from(point.x),
                            y: f64::from(point.y),
                        })
                    }) {
                        Ok(()) => self.notice = "Back on the Mac. Edge sharing is ready.".into(),
                        Err(error) => {
                            self.enabled = false;
                            self.notice = format!("Returned input to the Mac: {error:#}");
                        }
                    }
                } else {
                    self.enabled = false;
                    self.notice = "Sharing is off. Input is on the Mac.".into();
                }
            }
        }
        if self.enabled
            && self.running.is_none()
            && self.last_poll.elapsed() >= Duration::from_millis(12)
        {
            self.last_poll = Instant::now();
            if let Err(error) = self.observe(ctx) {
                self.enabled = false;
                self.previous = None;
                self.notice = format!("Sharing stopped: {error:#}");
            }
        }
        if self.is_active() {
            ctx.request_repaint_after(Duration::from_millis(12));
        }
    }

    fn observe(&mut self, ctx: &egui::Context) -> Result<()> {
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
        let options = SourceOptions {
            config_path: self.path.clone(),
            peer: handoff.peer.clone(),
            address: None,
            raw_touch: true,
            reduce_wifi_latency: self.reduce_wifi_latency,
            handoff: Some(macos::HandoffOptions {
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
        let ctx = ctx.clone();
        let thread = std::thread::Builder::new()
            .name("zflow-sharing".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => {
                        let _ = runtime.block_on(macos::run_controlled(options, stopped, status));
                    }
                    Err(error) => {
                        let _ = status.send(SourceStatus::Failed(format!(
                            "Could not start sharing: {error}"
                        )));
                    }
                }
                ctx.request_repaint();
            })?;
        self.notice = format!("Connecting to {}…", handoff.peer);
        self.running = Some(Running {
            stop,
            events,
            thread,
            handoff,
            returned: None,
            failed: false,
            capturing: false,
            entry_point: current,
        });
        Ok(())
    }
}

impl Drop for Sharing {
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
