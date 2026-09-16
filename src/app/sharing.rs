use std::{
    path::{Path, PathBuf},
    thread::JoinHandle,
    time::Instant,
};

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch};
use tracing::Instrument;

use crate::{
    config::Config,
    desktop::{Geometry, Point, Rect},
    macos::{self, SourceOptions, SourceStatus},
};

use super::{
    handoff::{self, Handoff},
    layout_model::Layout,
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

pub(super) struct Observer {
    enabled: bool,
    layout: Option<Layout>,
    path: PathBuf,
    config: Option<Config>,
    running: Option<Running>,
    previous: Option<Point>,
    pub notice: String,
    pub reduce_wifi_latency: bool,
    pub pause_requested: bool,
}

impl Default for Observer {
    fn default() -> Self {
        Self {
            enabled: false,
            layout: None,
            path: PathBuf::new(),
            config: None,
            running: None,
            previous: None,
            notice: "Sharing is off.".into(),
            reduce_wifi_latency: false,
            pause_requested: false,
        }
    }
}

impl Observer {
    pub fn has_session(&self) -> bool {
        self.running.is_some()
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn is_active(&self) -> bool {
        self.enabled || self.running.is_some()
    }

    pub fn stop(&mut self) {
        self.enabled = false;
        self.previous = None;
        if let Some(running) = &self.running {
            tracing::info!(parent: &running.span, "sharing stop requested");
            let _ = running.stop.send(true);
            self.notice = "Returning input to the Mac…".into();
        } else {
            self.notice = "Sharing is off.".into();
        }
    }

    pub fn enable(&mut self, path: &Path, config: &Config, layout: &Layout) -> Result<()> {
        let geometry = local_geometry()?;
        handoff::validate(layout, &geometry)?;
        for transition in layout.transitions() {
            if layout.monitors[transition.source].peer.is_none() {
                let name = layout.monitors[transition.target]
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
        self.layout = Some(layout.clone());
        self.path = path.into();
        self.config = Some(config.clone());
        self.previous = None;
        self.enabled = true;
        self.pause_requested = false;
        tracing::info!("edge sharing enabled");
        self.notice = "Ready on the Mac. Move through a configured edge to connect.".into();
        Ok(())
    }

    pub fn tick(&mut self) {
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
                    SourceStatus::PauseRequested => {
                        self.pause_requested = true;
                        self.enabled = false;
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
                    "The Mac displays changed. Checking the new layout before sharing resumes."
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
                        SourceStatus::PauseRequested => {
                            self.pause_requested = true;
                            self.enabled = false;
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
                    self.pause_requested |= self.enabled;
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
            config: self.config.clone(),
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

pub(super) fn local_geometry() -> Result<Geometry> {
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
