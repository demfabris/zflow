use std::{sync::atomic::AtomicU64, time::Instant};

use anyhow::{Context, Result};

use crate::{
    config::Config,
    desktop::Point,
    macos::{self, Crossing, Links, SourceStatus},
};

use super::{
    handoff::{self, Handoff},
    layout_model::Layout,
};

const READY: &str = "Ready on the Mac. Move through a configured edge to connect.";
const SECURE_INPUT_ON: &str = "Secure keyboard entry is on in a Mac app (a password field or \
    Terminal's Secure Keyboard Entry). Input stays on the Mac until it turns off.";

struct Running {
    crossing: Crossing,
    handoff: Handoff,
    returned: Option<u32>,
    failed: bool,
    cancelled: bool,
    /// A final status arrived; without one the worker ended unexpectedly.
    finished: bool,
    span: tracing::Span,
    started: Instant,
}

pub(super) struct Observer {
    enabled: bool,
    layout: Option<Layout>,
    running: Option<Running>,
    previous: Option<Point>,
    secure_input_notice: bool,
    /// The notice says a peer controls this Mac.
    controlled: bool,
    pub notice: String,
    pub reduce_wifi_latency: bool,
    pub pause_requested: bool,
}

impl Default for Observer {
    fn default() -> Self {
        Self {
            enabled: false,
            layout: None,
            running: None,
            previous: None,
            secure_input_notice: false,
            controlled: false,
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

    /// The computer that input goes to during a crossing.
    pub fn session_peer(&self) -> Option<&str> {
        let running = self.running.as_ref()?;
        Some(&running.handoff.peer)
    }

    /// The notice says Secure Input keeps input on the Mac.
    pub fn waiting_for_secure_input(&self) -> bool {
        self.secure_input_notice && self.notice == SECURE_INPUT_ON
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn is_active(&self) -> bool {
        self.enabled || self.running.is_some()
    }

    pub fn stop(&mut self) {
        if self.enabled {
            tracing::info!("edge sharing disarmed");
        }
        self.enabled = false;
        self.previous = None;
        if let Some(running) = &self.running {
            tracing::info!(parent: &running.span, "sharing stop requested");
            let _ = running.crossing.stop.send(true);
            self.notice = "Returning input to the Mac…".into();
        } else {
            self.notice = "Sharing is off.".into();
        }
    }

    /// Stops arming with the current layout, and lets a crossing in progress
    /// finish with its own. The app arms again with the new layout.
    pub fn disarm(&mut self) {
        if self.enabled {
            tracing::info!("edge sharing disarmed for a new layout");
        }
        self.enabled = false;
        self.previous = None;
    }

    pub fn enable(&mut self, config: &Config, layout: &Layout) -> Result<()> {
        let geometry = macos::desktop_geometry()?;
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
        self.previous = None;
        self.enabled = true;
        self.pause_requested = false;
        tracing::info!("edge sharing enabled");
        self.notice = READY.into();
        Ok(())
    }

    pub fn tick(&mut self, links: &Links) {
        self.tick_with(links.controller(), links);
    }

    /// `controller` is the peer controlling this Mac, if any.
    fn tick_with(&mut self, controller: Option<String>, links: &Links) {
        if self.running.is_some() {
            self.watch_crossing();
        }
        if self.secure_input_notice && !macos::secure_input_enabled() {
            self.secure_input_notice = false;
            if self.enabled && self.running.is_none() {
                self.notice = READY.into();
            }
        }
        if let Some(peer) = controller {
            // Nothing crosses while a peer controls this Mac, and a cursor it
            // leaves resting on an edge does not cross once it lets go.
            self.previous = None;
            if self.enabled && self.running.is_none() {
                self.notice = format!("Controlled by {peer}");
                self.controlled = true;
            }
            return;
        }
        if std::mem::take(&mut self.controlled) && self.enabled && self.running.is_none() {
            self.notice = READY.into();
        }
        if self.enabled
            && self.running.is_none()
            && let Err(error) = self.observe(links)
        {
            tracing::warn!(error = %format!("{error:#}"), "edge observation failed; sharing disabled");
            self.enabled = false;
            self.previous = None;
            self.notice = format!("Sharing stopped: {error:#}");
        }
    }

    fn watch_crossing(&mut self) {
        let Some(running) = &mut self.running else {
            return;
        };
        let mut closed = false;
        let mut statuses = Vec::new();
        loop {
            match running.crossing.status.try_recv() {
                Ok(status) => statuses.push(status),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        for status in statuses {
            self.record(status);
        }
        let Some(running) = &mut self.running else {
            return;
        };
        if !macos::desktop_geometry()
            .is_ok_and(|geometry| running.handoff.matches_geometry(&geometry))
        {
            if !running.failed {
                tracing::warn!(parent: &running.span, "crossing cancelled: Mac display geometry changed");
            }
            let _ = running.crossing.stop.send(true);
            self.enabled = false;
            running.failed = true;
            self.notice =
                "The Mac displays changed. Checking the new layout before sharing resumes.".into();
        }
        if closed {
            self.finish();
        }
    }

    fn record(&mut self, status: SourceStatus) {
        let Some(running) = &mut self.running else {
            return;
        };
        match status {
            SourceStatus::Sharing => {
                tracing::info!(parent: &running.span, elapsed_ms = running.started.elapsed().as_millis() as u64, "edge observer saw capture start");
                self.notice = format!(
                    "Sharing with {}. Cross back or press Ctrl+Cmd+Backspace to return.",
                    running.handoff.peer
                );
            }
            SourceStatus::Returned { position } => running.returned = Some(position),
            SourceStatus::LocalInputRestored => {
                self.notice = "Back on the Mac. Finishing cleanup…".into();
            }
            SourceStatus::Stopped => running.finished = true,
            SourceStatus::Cancelled(reason) => {
                running.finished = true;
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
                running.finished = true;
                running.failed = true;
                self.notice = error;
            }
        }
    }

    fn finish(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        let _entered = running.span.enter();
        self.previous = None;
        if !running.finished {
            tracing::error!("crossing worker stopped without reporting");
            self.enabled = false;
            self.notice = "The sharing worker stopped unexpectedly. Sharing will retry.".into();
        } else if running.failed {
            self.enabled = false;
        } else if running.cancelled && self.enabled {
            tracing::info!("crossing cancelled; edge sharing remains armed");
        } else if running.returned.is_some() && self.enabled {
            tracing::info!(
                elapsed_ms = running.started.elapsed().as_millis() as u64,
                "crossing cleanup completed; edge sharing rearmed"
            );
            self.notice = "Back on the Mac. Edge sharing is ready.".into();
        } else if self.enabled {
            // Only a stop or Escape ends a crossing without a return, and both
            // disarm first. Anything else is a failure, never a user pause.
            self.enabled = false;
            self.notice =
                "Remote input ended without returning to the Mac. Sharing will retry.".into();
        } else {
            self.notice = "Sharing is off. Input is on the Mac.".into();
        }
        tracing::info!(enabled = self.enabled, failed = running.failed, cancelled = running.cancelled, returned = running.returned.is_some(), notice = %self.notice, "crossing worker finished");
    }

    fn observe(&mut self, links: &Links) -> Result<()> {
        let layout = self.layout.as_ref().context("No saved layout")?;
        let geometry = macos::desktop_geometry()?;
        handoff::validate(layout, &geometry)?;
        let position = macos::cursor_position()?;
        let current = Point {
            x: position.x.floor() as i32,
            y: position.y.floor() as i32,
        };
        let previous = self.previous.replace(current);
        let Some(handoff) =
            previous.and_then(|previous| handoff::crossing(layout, &geometry, previous, current))
        else {
            return Ok(());
        };
        // A drag or a held modifier keeps the pointer on the Mac. The bridge
        // checks again once its tap is installed.
        if !macos::input_is_neutral() {
            tracing::info!("edge reached with a button or modifier held; input stays on the Mac");
            return Ok(());
        }
        // Capture would refuse anyway; skip the receiver setup.
        if macos::secure_input_enabled() {
            self.notice = SECURE_INPUT_ON.into();
            self.secure_input_notice = true;
            return Ok(());
        }
        static NEXT_CROSSING: AtomicU64 = AtomicU64::new(1);
        let span = tracing::info_span!("crossing",
            crossing = NEXT_CROSSING.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            peer = %handoff.peer);
        tracing::info!(parent: &span, remote_entry_edge = ?handoff.edge, "configured edge reached");
        tracing::debug!(parent: &span, x = position.x, y = position.y,
            start = handoff.start, end = handoff.end, position = handoff.position,
            remote_width = handoff.expected_width, remote_height = handoff.expected_height,
            entry_region = ?handoff.entry_region, "crossing geometry");
        // The receiver status already says why a link is not ready.
        let Some(crossing) = links.cross(handoff.clone(), position, self.reduce_wifi_latency)
        else {
            tracing::info!(parent: &span, "receiver not ready; input stays on the Mac");
            return Ok(());
        };
        self.notice = format!("Switching to {}…", handoff.peer);
        self.running = Some(Running {
            crossing,
            handoff,
            returned: None,
            failed: false,
            cancelled: false,
            finished: false,
            span,
            started: Instant::now(),
        });
        Ok(())
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        // Dropping the crossing's stop sender also returns input to the Mac;
        // the link finishes cleanup before it closes.
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::{Edge, Geometry, Rect, ReturnMapping};
    use tokio::sync::{mpsc, watch};

    /// An armed observer running one crossing, the crossing's status
    /// sender, and whether it was told to stop.
    fn crossing() -> (
        Observer,
        mpsc::UnboundedSender<SourceStatus>,
        watch::Receiver<bool>,
    ) {
        let mut observer = Observer::default();
        observer.enabled = true;
        let (status, events) = mpsc::unbounded_channel();
        let (stop, stopped) = watch::channel(false);
        let geometry = Geometry {
            monitors: vec![Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            }],
        };
        observer.running = Some(Running {
            crossing: Crossing {
                stop,
                status: events,
            },
            handoff: Handoff {
                peer: "linux".into(),
                edge: Edge::Left,
                start: 0,
                end: crate::desktop::FRACTION_MAX,
                position: 0,
                expected_width: 100,
                expected_height: 100,
                entry_region: geometry.monitors[0],
                return_mapping: ReturnMapping {
                    geometry,
                    edge: Edge::Right,
                    local_start: 0.0,
                    local_end: 1.0,
                    remote_start: 0.0,
                    remote_end: 1.0,
                },
            },
            returned: None,
            failed: false,
            cancelled: false,
            finished: false,
            span: tracing::Span::none(),
            started: Instant::now(),
        });
        (observer, status, stopped)
    }

    /// An armed observer running one crossing, fed `statuses` before the
    /// crossing's channel closes.
    fn finished(statuses: &[SourceStatus], report_end: bool) -> Observer {
        let (mut observer, status, _stopped) = crossing();
        for status in statuses {
            observer.record(status.clone());
        }
        if report_end {
            observer.record(SourceStatus::Stopped);
        }
        drop(status);
        observer.finish();
        observer
    }

    #[test]
    fn a_crossing_that_ends_without_returning_fails_instead_of_pausing() {
        // For example, macOS invalidated the tap and the bridge ended capture.
        let observer = finished(&[SourceStatus::Failed("tap invalidated".into())], false);
        assert!(!observer.is_enabled() && !observer.pause_requested);
        let observer = finished(&[], true);
        assert!(!observer.is_enabled() && !observer.pause_requested);
        assert!(observer.notice.contains("without returning"));
        let observer = finished(&[], false);
        assert!(!observer.is_enabled() && !observer.pause_requested);
    }

    #[test]
    fn nothing_crosses_while_a_peer_controls_the_mac() {
        let links = Links::with_backend(Default::default()).unwrap();
        let mut observer = Observer::default();
        observer.enabled = true;
        observer.previous = Some(Point { x: 1, y: 540 });
        observer.tick_with(Some("linux".into()), &links);
        // Observing would have disarmed this observer, which has no layout.
        assert!(observer.is_enabled());
        assert_eq!(observer.previous, None);
        assert_eq!(observer.notice, "Controlled by linux");
    }

    #[test]
    fn a_new_layout_lets_the_crossing_in_progress_finish() {
        let (mut observer, status, stopped) = crossing();
        observer.disarm();
        assert!(observer.has_session() && !observer.is_enabled());
        assert!(!*stopped.borrow(), "the crossing goes on");
        observer.record(SourceStatus::Returned { position: 0 });
        observer.record(SourceStatus::Stopped);
        drop(status);
        observer.finish();
        // Not a failure or a pause: the app arms again with the new layout.
        assert!(!observer.has_session() && !observer.is_enabled());
        assert!(!observer.pause_requested);
    }

    #[test]
    fn returns_and_cancellations_stay_armed_and_escape_pauses() {
        let observer = finished(&[SourceStatus::Returned { position: 0 }], true);
        assert!(observer.is_enabled() && !observer.has_session());
        let observer = finished(&[SourceStatus::Cancelled("cursor left".into())], false);
        assert!(observer.is_enabled());
        let observer = finished(&[SourceStatus::PauseRequested], true);
        assert!(!observer.is_enabled() && observer.pause_requested);
    }
}
