//! macOS input source: native capture, and crossings over a receiver's session.

mod awdl;
mod inject;
mod keys;
mod link;
mod local_network;
mod pointer;

use std::{
    ffi::CStr,
    future::Future,
    os::raw::c_char,
    path::PathBuf,
    pin::Pin,
    sync::Once,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use tokio::sync::{Notify, mpsc, watch};

use crate::{
    capture::{
        CaptureFrame, CaptureTransition, CapturedDeviceFrame, KeyState, MAX_TOUCHPAD_CONTACTS,
    },
    core::{
        ContactId, MotionDelta, PointerButton, SessionCloseReason, SessionContext,
        SourceDimensions, TouchContact, TouchState, TouchTool,
    },
    desktop::{DesktopRequest, DesktopResponse, Edge, Point, Rect, ReturnMapping},
    session::{SessionEvent, SessionEventKind, SessionHandle},
};

pub use link::{Crossing, LinkState, Links};
pub use local_network::{LocalNetwork, local_network_access};

// Yield to the session between batches so a backlog after a stall cannot
// overflow its 512-command queue in one burst.
const MAX_EVENTS_PER_DRAIN: usize = 256;
const TOUCH_STALE_TIMEOUT: Duration = Duration::from_millis(150);
/// A Magic Trackpad 2, 160 x 115 mm, in hundredths of a millimetre.
const FALLBACK_TRACKPAD_SIZE: SourceDimensions = SourceDimensions {
    width: 16_000,
    height: 11_500,
};
/// 500 mm. A larger reported side is not a trackpad.
const MAX_TRACKPAD_EXTENT: i32 = 50_000;
const SECURE_INPUT_CHECK_INTERVAL: Duration = Duration::from_millis(100);
const SECURE_INPUT_RETURNED: &str = "Secure keyboard entry turned on in a Mac app, so typing \
    could not be shared. Input returned to the Mac";
const RELEASE_TIMEOUT: Duration = Duration::from_millis(200);
// Prepare starts the receiver's two-second handoff lease, which only polling
// renews, and polling starts once the AWDL lease is held.
const AWDL_ACQUIRE_TIMEOUT: Duration = Duration::from_millis(crate::desktop::LEASE_MS / 2);

/// Woken by the capture bridge whenever it queues an event or stops.
static CAPTURE_WAKE: Notify = Notify::const_new();

extern "C" fn wake_capture() {
    CAPTURE_WAKE.notify_one();
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceStatus {
    PauseRequested,
    Sharing,
    LocalInputRestored,
    Returned { position: u32 },
    Stopped,
    Failed(String),
    Cancelled(String),
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct AdmissionCancelled(String);

fn failure_status(error: &anyhow::Error) -> SourceStatus {
    if error.is::<AdmissionCancelled>() {
        SourceStatus::Cancelled(format!("{error:#}"))
    } else {
        SourceStatus::Failed(format!("{error:#}"))
    }
}

/// Keeps the first failure. A cleanup failure still replaces success or a
/// cancellation, because input may not be back on the Mac.
fn keep_first_failure<T>(result: &mut Result<T>, cleanup: Result<()>, operation: &str) {
    if let Err(error) = cleanup {
        tracing::warn!(operation, error = %format!("{error:#}"), "crossing cleanup failed");
        if result
            .as_ref()
            .map_or_else(|error| error.is::<AdmissionCancelled>(), |_| true)
        {
            *result = Err(error.context(format!("Could not finish {operation}")));
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CursorPosition {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DesktopRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// `prompt` asks macOS to show its normal Accessibility permission UI.
pub fn accessibility_authorized(prompt: bool) -> bool {
    // SAFETY: the bridge creates and releases its own permission options.
    unsafe { zflow_mac_accessibility_authorized(i32::from(prompt)) == 1 }
}

/// Asks the window server for an active event tap and releases it at once.
/// Unlike `accessibility_authorized`, this notices access removed while
/// zflow runs. A refused tap leaks a Mach port inside CoreGraphics, so
/// callers must not retry a refusal quickly.
pub fn event_tap_allowed() -> bool {
    // SAFETY: the bridge creates, disables and releases its own tap.
    unsafe { zflow_mac_event_tap_allowed() == 1 }
}

pub fn cursor_position() -> Result<CursorPosition> {
    let mut position = CursorPosition::default();
    // SAFETY: the output has the bridge's two-double C layout.
    if unsafe { zflow_mac_cursor_position(&mut position) } != 0 {
        bail!("could not read the Mac cursor position");
    }
    Ok(position)
}

pub fn active_desktop_rectangles() -> Result<Vec<DesktopRect>> {
    let mut rectangles = vec![DesktopRect::default(); 64];
    // SAFETY: the bridge receives writable storage for all 64 rectangles.
    let count = unsafe { zflow_mac_desktop_rectangles(rectangles.as_mut_ptr(), 64) };
    if !(1..=64).contains(&count) {
        bail!("could not read active Mac displays");
    }
    rectangles.truncate(count as usize);
    rectangles.sort_by(|left, right| {
        left.x
            .total_cmp(&right.x)
            .then(left.y.total_cmp(&right.y))
            .then(left.width.total_cmp(&right.width))
            .then(left.height.total_cmp(&right.height))
    });
    Ok(rectangles)
}

/// Changes whenever macOS reconfigures a display.
pub fn display_generation() -> u32 {
    // SAFETY: the bridge registers its reconfiguration callback once.
    unsafe { zflow_mac_display_generation() }
}

pub fn input_is_neutral() -> bool {
    // SAFETY: this reads physical key, button and modifier state without capture.
    unsafe { zflow_mac_input_is_neutral() == 1 }
}

/// True while any app holds Secure Event Input. The event tap then sees no
/// keys, so typing would reach the Mac while the pointer drives the peer.
pub fn secure_input_enabled() -> bool {
    // SAFETY: this reads a system-wide flag without capture.
    unsafe { zflow_mac_secure_input_enabled() == 1 }
}

#[derive(Clone, Debug)]
pub struct HandoffOptions {
    pub entry_position: CursorPosition,
    pub entry_region: Rect,
    pub return_mapping: ReturnMapping,
    pub edge: Edge,
    pub start: u32,
    pub end: u32,
    pub position: u32,
    pub expected_width: u32,
    pub expected_height: u32,
}

/// What one crossing borrows from its link.
struct Activation<'a> {
    session: &'a SessionHandle,
    events: &'a mut mpsc::Receiver<SessionEvent>,
    context: SessionContext,
    raw_touch: bool,
}

type DesktopPoll<'a> = Pin<Box<dyn Future<Output = Result<DesktopResponse>> + Send + 'a>>;

/// Runs one crossing and reports how it ended. The status sender drops only
/// after cleanup, so the observer rearms once the Mac owns input again.
/// Returns true when the crossing failed.
async fn run_crossing(
    activation: Activation<'_>,
    handoff: HandoffOptions,
    reduce_wifi_latency: bool,
    mut stop: watch::Receiver<bool>,
    status: mpsc::UnboundedSender<SourceStatus>,
) -> bool {
    let started = Instant::now();
    let result = cross(activation, handoff, reduce_wifi_latency, &mut stop, &status).await;
    let outcome = match &result {
        Ok(returned) => {
            if let Some(position) = returned {
                let _ = status.send(SourceStatus::Returned {
                    position: *position,
                });
            }
            SourceStatus::Stopped
        }
        Err(error) => {
            let outcome = failure_status(error);
            if matches!(outcome, SourceStatus::Cancelled(_)) {
                tracing::info!(error = %format!("{error:#}"), "crossing cancelled");
            } else {
                tracing::error!(error = %format!("{error:#}"), "crossing failed");
            }
            outcome
        }
    };
    let failed = matches!(outcome, SourceStatus::Failed(_));
    let _ = status.send(outcome);
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        success = result.is_ok(),
        "crossing completed"
    );
    failed
}

/// Prepares the receiver's desktop, forwards input until it returns, and
/// finishes the handoff. The session outlives the crossing, so every desktop
/// request is awaited: dropping one closes the transport.
async fn cross(
    mut activation: Activation<'_>,
    mut handoff: HandoffOptions,
    reduce_wifi_latency: bool,
    stop: &mut watch::Receiver<bool>,
    status: &mpsc::UnboundedSender<SourceStatus>,
) -> Result<Option<u32>> {
    refuse_waiting_events(activation.events)?;
    if stopped(stop) {
        return Ok(None);
    }
    let local_desktop = active_desktop_rectangles()?;
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow!("could not create a desktop handoff token: {error}"))?;
    let token = handoff_token(random);
    let prepare = prepare_request(&mut handoff, token)?;
    let preparing = Instant::now();
    // Await Prepare even after Stop so Finish can use the same session.
    let (lease, prepared) = tokio::join!(
        acquire_lease(reduce_wifi_latency),
        activation.session.desktop_request(prepare)
    );
    tracing::info!(
        elapsed_ms = preparing.elapsed().as_millis() as u64,
        "desktop preparation completed"
    );
    let (mut lease, mut result) = match lease {
        Ok(lease) => (lease, Ok(None)),
        Err(error) => (None, Err(error)),
    };
    if result.is_ok() {
        result = async {
            // The link's snapshot already showed the receiver handles desktop
            // requests, so a failure here is the session or the receiver.
            validate_prepared(
                &handoff,
                prepared.context("Could not prepare the other computer's desktop")?,
            )?;
            if stopped(stop) {
                return Ok(None);
            }
            ensure!(
                local_desktop == active_desktop_rectangles()?,
                "the Mac desktop changed while preparing the crossing; refresh and save its layout"
            );
            remote(
                &mut activation,
                &handoff,
                &local_desktop,
                token,
                &mut lease,
                stop,
                status,
            )
            .await
        }
        .await;
    }
    let finishing = Instant::now();
    let finished = validate_finished(
        activation
            .session
            .desktop_request(DesktopRequest::Finish { token })
            .await,
    );
    tracing::info!(
        elapsed_ms = finishing.elapsed().as_millis() as u64,
        success = finished.is_ok(),
        "desktop handoff cleanup completed"
    );
    keep_first_failure(&mut result, finished, "desktop handoff cleanup");
    if let Some(lease) = lease {
        keep_first_failure(&mut result, lease.release().await, "AWDL restoration");
    }
    result
}

async fn acquire_lease(reduce_wifi_latency: bool) -> Result<Option<awdl::HeldLease>> {
    if !reduce_wifi_latency {
        return Ok(None);
    }
    // Dropping a late acquisition closes its pipes, so the helper restores AWDL.
    let lease = tokio::time::timeout(AWDL_ACQUIRE_TIMEOUT, awdl::AwDlLease::acquire())
        .await
        .context("the AWDL helper did not grant a lease in time")??;
    Ok(Some(lease.hold()))
}

/// Samples the cursor again, since it kept moving after the edge was detected.
fn prepare_request(handoff: &mut HandoffOptions, token: u64) -> Result<DesktopRequest> {
    let current = cursor_position()?;
    let point = Point {
        x: current.x.floor() as i32,
        y: current.y.floor() as i32,
    };
    let original_position = handoff.position;
    let refreshed = handoff.entry_region.contains(point);
    if refreshed {
        handoff.position = handoff.return_mapping.fraction(point)?;
    }
    tracing::debug!(
        original_position,
        position = handoff.position,
        refreshed,
        current_x = current.x,
        current_y = current.y,
        displacement_x = current.x - handoff.entry_position.x,
        displacement_y = current.y - handoff.entry_position.y,
        "entry fraction sampled before desktop preparation"
    );
    let request = DesktopRequest::Prepare {
        token,
        edge: handoff.edge,
        start: handoff.start,
        end: handoff.end,
        position: handoff.position,
    };
    request.validate()?;
    Ok(request)
}

/// Owns input on the receiver: activation, native capture, and release.
async fn remote(
    activation: &mut Activation<'_>,
    handoff: &HandoffOptions,
    local_desktop: &[DesktopRect],
    token: u64,
    lease: &mut Option<awdl::HeldLease>,
    stop: &mut watch::Receiver<bool>,
    status: &mpsc::UnboundedSender<SourceStatus>,
) -> Result<Option<u32>> {
    let session = activation.session;
    session.begin_outbound(activation.context)?;
    let mut poll = None;
    let mut result = capture(
        activation,
        handoff,
        local_desktop,
        token,
        &mut poll,
        lease,
        stop,
        status,
    )
    .await;
    let releasing = Instant::now();
    let released = tokio::time::timeout(
        RELEASE_TIMEOUT,
        session.end_outbound(SessionCloseReason::LocalRelease),
    )
    .await
    .context("timed out releasing remote input")
    .and_then(|result| result);
    tracing::info!(
        elapsed_ms = releasing.elapsed().as_millis() as u64,
        success = released.is_ok(),
        "remote input release completed"
    );
    if released.is_err() {
        // Closing the session makes the receiver release everything it holds.
        session.close(SessionCloseReason::LocalRelease);
    }
    keep_first_failure(&mut result, released, "remote input release");
    // Keep the poll alive through Leave, then finish it before Finish. Late
    // responses cannot change the cursor placement or the capture result.
    if let Some(poll) = poll {
        tracing::debug!("waiting for desktop poll before cleanup");
        let response = poll.await;
        tracing::debug!(?response, "desktop poll wait completed");
    }
    result
}

/// Why forwarding stopped.
struct Ended {
    reason: &'static str,
    returned: Option<u32>,
    error: Option<anyhow::Error>,
    touch_active: bool,
    events: u64,
}

impl Ended {
    fn failed(mut self, reason: &'static str, error: anyhow::Error) -> Self {
        self.reason = reason;
        self.error = Some(error);
        self
    }
}

#[allow(clippy::too_many_arguments)]
async fn capture<'a>(
    activation: &mut Activation<'a>,
    handoff: &HandoffOptions,
    local_desktop: &[DesktopRect],
    token: u64,
    poll: &mut Option<DesktopPoll<'a>>,
    lease: &mut Option<awdl::HeldLease>,
    stop: &mut watch::Receiver<bool>,
    status: &mpsc::UnboundedSender<SourceStatus>,
) -> Result<Option<u32>> {
    let started = Instant::now();
    let mut capture =
        MacCapture::start(activation.raw_touch, handoff.entry_region).inspect_err(|error| {
            tracing::warn!(elapsed_ms = started.elapsed().as_millis() as u64,
                cursor = ?cursor_position().ok().map(|p| (p.x, p.y)),
                error = %error, "native capture start rejected");
        })?;
    if activation.raw_touch && !capture.raw_touch {
        tracing::info!(reason = %MacCapture::last_error(),
            "raw touch unavailable; forwarding pointer and scroll");
    }
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        raw_touch = capture.raw_touch,
        cursor = ?cursor_position().ok().map(|p| (p.x, p.y)),
        "native capture started"
    );
    let _ = status.send(SourceStatus::Sharing);
    // Polling renews the receiver's handoff and reports its return edge.
    *poll = Some(poll_desktop(activation.session, token));
    let ended = forward(
        &mut capture,
        activation,
        handoff,
        token,
        poll,
        lease,
        stop,
        status,
    )
    .await;
    tracing::info!(reason = ended.reason, captured_events = ended.events,
        active_ms = started.elapsed().as_millis() as u64,
        error = ?ended.error.as_ref().map(|error| format!("{error:#}")), "stopping native capture");
    let mut error = ended.error;
    let return_point = match ended
        .returned
        .map(|position| return_point(handoff, local_desktop, position))
    {
        Some(Ok(point)) => Some(point),
        Some(Err(failure)) => {
            error.get_or_insert(failure);
            None
        }
        None => None,
    };
    if let Err(failure) = capture.stop_at(return_point) {
        error.get_or_insert(failure);
    } else if return_point.is_some() {
        tracing::info!(point = ?return_point, "Mac cursor positioned before local input resumed");
        let _ = status.send(SourceStatus::LocalInputRestored);
    }
    // Escape must stay paused even when the event queue was full or cleanup failed.
    if capture.pause_requested() {
        let _ = status.send(SourceStatus::PauseRequested);
    }
    if ended.touch_active {
        let _ = activation
            .session
            .capture(touch_frame(TouchState::default()));
    }
    error.map_or(Ok(ended.returned), Err)
}

fn return_point(
    handoff: &HandoffOptions,
    local_desktop: &[DesktopRect],
    position: u32,
) -> Result<CursorPosition> {
    let point = handoff.return_mapping.position(position)?;
    ensure!(
        local_desktop == active_desktop_rectangles()?,
        "the Mac desktop changed before returning input"
    );
    Ok(CursorPosition {
        x: f64::from(point.x),
        y: f64::from(point.y),
    })
}

/// Forwards captured input until the crossing ends and reports why.
#[allow(clippy::too_many_arguments)]
async fn forward<'a>(
    capture: &mut MacCapture,
    activation: &mut Activation<'a>,
    handoff: &HandoffOptions,
    token: u64,
    poll: &mut Option<DesktopPoll<'a>>,
    lease: &mut Option<awdl::HeldLease>,
    stop: &mut watch::Receiver<bool>,
    status: &mpsc::UnboundedSender<SourceStatus>,
) -> Ended {
    let session = activation.session;
    let mut ended = Ended {
        reason: "stop requested",
        returned: None,
        error: None,
        touch_active: false,
        events: 0,
    };
    let mut secure_input_check = tokio::time::interval(SECURE_INPUT_CHECK_INTERVAL);
    secure_input_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_touch = Instant::now();
    loop {
        let touch_expiry = tokio::time::Instant::from_std(last_touch + TOUCH_STALE_TIMEOUT);
        tokio::select! {
            biased;
            _ = stop_requested(stop) => return ended,
            response = next_poll(poll) => match response {
                Ok(DesktopResponse::Active) => *poll = Some(poll_desktop(session, token)),
                Ok(DesktopResponse::Returned { position }) => {
                    tracing::info!(position, "receiver reported return edge");
                    ended.reason = "GNOME return edge";
                    if position < handoff.start || position > handoff.end {
                        ended.error =
                            Some(anyhow!("the pointer came back outside the configured edge range"));
                    } else {
                        ended.returned = Some(position);
                    }
                    return ended;
                }
                Ok(DesktopResponse::Unavailable { reason }) => {
                    let error = anyhow!("the other computer's desktop is unavailable: {reason}");
                    return ended.failed("desktop unavailable", error);
                }
                Ok(_) => {
                    let error = anyhow!("unexpected desktop response from the other computer");
                    return ended.failed("unexpected desktop response", error);
                }
                Err(error) => return ended.failed("desktop request failed", error),
            },
            error = lease_failed(lease) => {
                *lease = None;
                return ended.failed("AWDL renewal failed", error);
            }
            _ = secure_input_check.tick() => {
                if secure_input_enabled() {
                    let error = AdmissionCancelled(SECURE_INPUT_RETURNED.into()).into();
                    return ended.failed("secure keyboard entry", error);
                }
                // A stop the wake missed still ends capture.
                if capture.stop_requested() {
                    CAPTURE_WAKE.notify_one();
                }
            }
            () = CAPTURE_WAKE.notified() => {
                let mut drained = 0;
                while drained < MAX_EVENTS_PER_DRAIN {
                    let Some(event) = capture.poll() else { break };
                    drained += 1;
                    ended.events += 1;
                    if event.kind == NativeEventKind::Escape as u32 {
                        let _ = status.send(SourceStatus::PauseRequested);
                        ended.reason = "native escape event";
                        return ended;
                    }
                    if event.kind == NativeEventKind::Touch as u32 {
                        last_touch = Instant::now();
                        ended.touch_active = event.contact_count > 0;
                    }
                    if let Err(error) = native_frame(event)
                        .and_then(|frame| frame.map_or(Ok(()), |frame| session.capture(frame)))
                    {
                        return ended.failed("capture forwarding failed", error);
                    }
                }
                if drained == MAX_EVENTS_PER_DRAIN {
                    // More may be queued. Let the session send this batch first.
                    tokio::task::yield_now().await;
                    CAPTURE_WAKE.notify_one();
                } else if capture.stop_requested() {
                    ended.reason = "native capture requested stop";
                    return ended;
                }
            }
            () = tokio::time::sleep_until(touch_expiry), if ended.touch_active => {
                if let Err(error) = session.capture(touch_frame(TouchState::default())) {
                    return ended.failed("capture forwarding failed", error);
                }
                ended.touch_active = false;
            }
            event = activation.events.recv() => match event.map(|event| event.kind) {
                Some(SessionEventKind::OutboundEnded) => {
                    // The session ends remote control when the receiver stops
                    // acknowledging, for example after a Wi-Fi stall outlived its
                    // lease. Return at the entry point and keep sharing armed.
                    ended.reason = "remote ownership ended";
                    ended.returned = Some(handoff.position);
                    return ended;
                }
                Some(SessionEventKind::Closed { reason }) => {
                    let error = anyhow!("input session closed: {reason}");
                    return ended.failed("input session closed", error);
                }
                Some(kind) => refuse_inbound(kind),
                None => {
                    let error = anyhow!("input session event channel closed");
                    return ended.failed("input session closed", error);
                }
            },
        }
    }
}

async fn next_poll(poll: &mut Option<DesktopPoll<'_>>) -> Result<DesktopResponse> {
    let response = match poll.as_mut() {
        Some(pending) => pending.await,
        None => std::future::pending().await,
    };
    *poll = None;
    response
}

/// The Mac only sends input. Refuse whatever a receiver would handle.
fn refuse_inbound(kind: SessionEventKind) {
    match kind {
        SessionEventKind::Desktop { reply, .. } => {
            let _ = reply.send(DesktopResponse::unavailable(
                "Mac source cannot receive desktop handoffs",
            ));
        }
        SessionEventKind::ReceiverEffects { applied, .. } => {
            let _ = applied.send(Err(
                "the Mac does not accept input from other computers".into()
            ));
        }
        SessionEventKind::OutboundEnded | SessionEventKind::Closed { .. } => {}
    }
}

/// Answers events that arrived while the link was idle, including the
/// OutboundEnded that trails the previous crossing's release.
fn refuse_waiting_events(events: &mut mpsc::Receiver<SessionEvent>) -> Result<()> {
    loop {
        match events.try_recv() {
            Ok(event) => {
                if let SessionEventKind::Closed { reason } = event.kind {
                    bail!("input session closed: {reason}");
                }
                refuse_inbound(event.kind);
            }
            Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::error::TryRecvError::Disconnected) => bail!("input session closed"),
        }
    }
}

fn stopped(stop: &watch::Receiver<bool>) -> bool {
    *stop.borrow() || stop.has_changed().is_err()
}

async fn stop_requested(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() || stop.changed().await.is_err() {
            return;
        }
    }
}

fn validate_prepared(handoff: &HandoffOptions, response: DesktopResponse) -> Result<()> {
    response.validate()?;
    match response {
        DesktopResponse::Prepared { geometry, .. } => {
            let bounds = geometry.bounds()?;
            if bounds.width != handoff.expected_width || bounds.height != handoff.expected_height {
                bail!(
                    "the other computer's desktop changed size; refresh and save the computer layout before sharing"
                );
            }
            Ok(())
        }
        DesktopResponse::Unavailable { reason } => {
            bail!("the other computer's desktop is unavailable: {reason}")
        }
        _ => bail!("the other computer did not prepare its desktop for input"),
    }
}

fn handoff_token(random: [u8; 8]) -> u64 {
    (u64::from_ne_bytes(random) & crate::desktop::MAX_TOKEN).max(1)
}

fn validate_finished(response: Result<DesktopResponse>) -> Result<()> {
    match response? {
        DesktopResponse::Finished => Ok(()),
        DesktopResponse::Unavailable { reason } => {
            bail!("the other computer could not finish the desktop handoff: {reason}")
        }
        _ => bail!("the other computer did not confirm the desktop handoff cleanup"),
    }
}

/// Resolves only when a held AWDL lease fails to renew.
async fn lease_failed(lease: &mut Option<awdl::HeldLease>) -> anyhow::Error {
    match lease {
        Some(lease) => lease.failed().await,
        None => std::future::pending().await,
    }
}

fn poll_desktop(session: &SessionHandle, token: u64) -> DesktopPoll<'_> {
    Box::pin(async move {
        let started = Instant::now();
        let response = session
            .desktop_request(DesktopRequest::Poll { token })
            .await;
        // Older receivers reply immediately; limit their poll rate before repeating.
        if matches!(&response, Ok(DesktopResponse::Active)) {
            tokio::time::sleep_until((started + Duration::from_millis(50)).into()).await;
        }
        response
    })
}

fn native_frame(event: NativeEvent) -> Result<Option<CapturedDeviceFrame>> {
    let frame = match event.kind {
        kind if kind == NativeEventKind::Motion as u32 => CaptureFrame {
            motion: MotionDelta {
                dx: event.dx,
                dy: event.dy,
                scroll_x: event.scroll_x,
                scroll_y: event.scroll_y,
            },
            event_count: 1,
            ..CaptureFrame::default()
        },
        kind if kind == NativeEventKind::Key as u32 => {
            let Some(usage) = keys::mac_keycode_to_hid(event.code) else {
                return Ok(None);
            };
            CaptureFrame {
                transitions: vec![CaptureTransition::Key {
                    usage,
                    state: pressed_state(event.pressed),
                }],
                event_count: 1,
                ..CaptureFrame::default()
            }
        }
        kind if kind == NativeEventKind::Button as u32 => CaptureFrame {
            transitions: vec![CaptureTransition::Button {
                button: PointerButton(event.button),
                state: pressed_state(event.pressed),
            }],
            event_count: 1,
            ..CaptureFrame::default()
        },
        kind if kind == NativeEventKind::Touch as u32 => {
            let size = trackpad_size(event.surface_width, event.surface_height);
            let count = usize::from(event.contact_count).min(MAX_TOUCHPAD_CONTACTS);
            let contacts = event.contacts[..count].iter().filter_map(|contact| {
                let id = u32::try_from(contact.id).ok()?;
                let x = surface_axis(contact.x, size.width);
                // MultitouchSupport uses a bottom-left origin; Linux touchpads use top-left.
                let y = surface_axis(1.0 - contact.y, size.height);
                Some(TouchContact {
                    id: ContactId(id),
                    x,
                    y,
                    pressure: None,
                    major: None,
                    minor: None,
                    orientation_millidegrees: None,
                    tool: TouchTool::Finger,
                    source_dimensions: Some(size),
                })
            });
            let state = TouchState::new(contacts)
                .map_err(|id| anyhow!("duplicate raw touch contact id {}", id.0))?;
            return Ok(Some(touch_frame(state)));
        }
        _ => return Ok(None),
    };
    Ok(Some(CapturedDeviceFrame {
        device_path: PathBuf::from("macos:event-tap"),
        frame,
        captured_at: Instant::now(),
    }))
}

fn touch_frame(state: TouchState) -> CapturedDeviceFrame {
    CapturedDeviceFrame {
        device_path: PathBuf::from("macos:magic-trackpad"),
        frame: CaptureFrame {
            touch_snapshot: Some(state),
            event_count: 1,
            ..CaptureFrame::default()
        },
        captured_at: Instant::now(),
    }
}

/// The trackpad's size in hundredths of a millimetre, as MultitouchSupport
/// reported it, or a Magic Trackpad 2's when the report is missing or absurd.
fn trackpad_size(width: i32, height: i32) -> SourceDimensions {
    let plausible = |extent: i32| (1..=MAX_TRACKPAD_EXTENT).contains(&extent);
    if plausible(width) && plausible(height) {
        return SourceDimensions {
            width: width as u32,
            height: height as u32,
        };
    }
    static LOGGED: Once = Once::new();
    LOGGED.call_once(|| {
        tracing::warn!(
            width,
            height,
            "trackpad size unavailable; assuming a Magic Trackpad 2"
        );
    });
    FALLBACK_TRACKPAD_SIZE
}

/// Converts a 0..1 MultitouchSupport position to hundredths of a millimetre.
fn surface_axis(value: f32, extent: u32) -> i32 {
    (f64::from(value.clamp(0.0, 1.0)) * f64::from(extent)).round() as i32
}

fn pressed_state(pressed: u8) -> KeyState {
    if pressed == 0 {
        KeyState::Released
    } else {
        KeyState::Pressed
    }
}

#[repr(u32)]
enum NativeEventKind {
    Motion = 1,
    Key = 2,
    Button = 3,
    Touch = 4,
    Escape = 5,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NativeContact {
    id: i32,
    x: f32,
    y: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NativeEvent {
    kind: u32,
    dx: i64,
    dy: i64,
    scroll_x: i64,
    scroll_y: i64,
    code: u16,
    button: u16,
    pressed: u8,
    contact_count: u8,
    padding: [u8; 2],
    surface_width: i32,
    surface_height: i32,
    contacts: [NativeContact; MAX_TOUCHPAD_CONTACTS],
}

unsafe extern "C" {
    fn zflow_mac_accessibility_authorized(prompt: i32) -> i32;
    fn zflow_mac_event_tap_allowed() -> i32;
    fn zflow_mac_cursor_position(position: *mut CursorPosition) -> i32;
    fn zflow_mac_desktop_rectangles(rectangles: *mut DesktopRect, capacity: u32) -> i32;
    fn zflow_mac_display_generation() -> u32;
    fn zflow_mac_input_is_neutral() -> i32;
    fn zflow_mac_secure_input_enabled() -> i32;
    fn zflow_mac_capture_start(
        raw_touch: i32,
        entry: *const DesktopRect,
        wake: extern "C" fn(),
    ) -> i32;
    fn zflow_mac_capture_stop_at(position: *const CursorPosition) -> i32;
    fn zflow_mac_capture_poll(event: *mut NativeEvent) -> i32;
    fn zflow_mac_capture_stop_requested() -> i32;
    fn zflow_mac_capture_pause_requested() -> i32;
    fn zflow_mac_capture_last_error() -> *const c_char;
    #[cfg(test)]
    fn zflow_mac_modifier_pressed(keycode: u16, flags: u64) -> u8;
    #[cfg(test)]
    fn zflow_mac_should_forward_scroll(
        raw_touch: u8,
        raw_contact_active: u8,
        scroll_phase: i64,
        momentum_phase: i64,
    ) -> u8;
}

struct MacCapture {
    running: bool,
    raw_touch: bool,
}

impl MacCapture {
    /// Installs the event tap. The bridge then admits the crossing only if the
    /// cursor is still in `entry` and no key or button is held; checking after
    /// the tap is installed leaves no gap for a press to slip through.
    fn start(raw_touch: bool, entry: Rect) -> Result<Self> {
        let entry = DesktopRect {
            x: f64::from(entry.x),
            y: f64::from(entry.y),
            width: f64::from(entry.width),
            height: f64::from(entry.height),
        };
        // SAFETY: start initializes the native thread before returning; the
        // wake callback is a plain function that only touches a static Notify.
        let status = unsafe { zflow_mac_capture_start(i32::from(raw_touch), &entry, wake_capture) };
        if status < 0 {
            return Err(capture_start_error(status, Self::last_error()));
        }
        Ok(Self {
            running: true,
            raw_touch: status == 1,
        })
    }

    fn poll(&mut self) -> Option<NativeEvent> {
        let mut event = NativeEvent::default();
        // SAFETY: event points to writable storage matching the bridge's C layout.
        (unsafe { zflow_mac_capture_poll(&mut event) } == 1).then_some(event)
    }

    fn stop_requested(&self) -> bool {
        // SAFETY: the bridge exposes this flag atomically.
        unsafe { zflow_mac_capture_stop_requested() == 1 }
    }

    fn pause_requested(&self) -> bool {
        // SAFETY: the bridge exposes this flag atomically and preserves it after stop.
        unsafe { zflow_mac_capture_pause_requested() == 1 }
    }

    fn stop_at(&mut self, position: Option<CursorPosition>) -> Result<()> {
        if self.running {
            // SAFETY: stop is idempotent after a successful start and joins the native thread.
            let status = unsafe {
                zflow_mac_capture_stop_at(position.as_ref().map_or(std::ptr::null(), |point| point))
            };
            self.running = false;
            if status < 0 {
                bail!("macOS capture ended with an error: {}", Self::last_error());
            }
        }
        Ok(())
    }

    fn last_error() -> String {
        // SAFETY: the bridge returns a process-lifetime NUL-terminated buffer.
        let pointer = unsafe { zflow_mac_capture_last_error() };
        if pointer.is_null() {
            return "unknown error".to_owned();
        }
        // SAFETY: checked non-null and the bridge always terminates the buffer.
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    }
}

fn capture_start_error(status: i32, reason: String) -> anyhow::Error {
    // capture_bridge.c uses -2 only for admission cancellation after cleanup.
    if status == -2 {
        AdmissionCancelled(reason).into()
    } else {
        anyhow!("could not start macOS capture: {reason}")
    }
}

impl Drop for MacCapture {
    fn drop(&mut self) {
        if let Err(error) = self.stop_at(None) {
            tracing::warn!(error = %format!("{error:#}"), "macOS capture cleanup failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_cancellation_is_typed_and_survives_context() {
        let native = capture_start_error(-2, "held button".into()).context("while starting");
        assert!(matches!(
            failure_status(&native),
            SourceStatus::Cancelled(_)
        ));
        for error in [
            capture_start_error(-1, "crossing cancelled".into()),
            anyhow!("crossing cancelled"),
        ] {
            assert!(matches!(failure_status(&error), SourceStatus::Failed(_)));
        }
    }

    #[test]
    fn cleanup_failures_replace_success_and_cancellation_but_not_failure() {
        let cancelled = || -> Result<Option<u32>> {
            Err(AdmissionCancelled("cursor left the edge".into()).into())
        };
        for mut result in [Ok(Some(7)), cancelled()] {
            keep_first_failure(&mut result, Err(anyhow!("Finish lost")), "cleanup");
            let error = result.unwrap_err();
            assert!(!error.is::<AdmissionCancelled>());
            assert!(format!("{error:#}").contains("Finish lost"));
        }
        let mut failed: Result<Option<u32>> = Err(anyhow!("Prepare timed out"));
        keep_first_failure(&mut failed, Err(anyhow!("Finish lost")), "cleanup");
        assert_eq!(failed.unwrap_err().to_string(), "Prepare timed out");
        let mut kept = cancelled();
        keep_first_failure(&mut kept, Ok(()), "cleanup");
        assert!(kept.unwrap_err().is::<AdmissionCancelled>());
    }

    #[tokio::test]
    async fn idle_events_are_answered_and_a_closed_session_refuses_the_crossing() {
        let event = |kind| SessionEvent {
            session_id: 1,
            peer: "linux".into(),
            kind,
        };
        let (sender, mut events) = mpsc::channel(8);
        let (reply, desktop) = tokio::sync::oneshot::channel();
        sender
            .send(event(SessionEventKind::OutboundEnded))
            .await
            .unwrap();
        sender
            .send(event(SessionEventKind::Desktop {
                request: DesktopRequest::Snapshot,
                reply,
            }))
            .await
            .unwrap();
        refuse_waiting_events(&mut events).unwrap();
        assert!(matches!(
            desktop.await.unwrap(),
            DesktopResponse::Unavailable { .. }
        ));
        sender
            .send(event(SessionEventKind::Closed {
                reason: "lost".into(),
            }))
            .await
            .unwrap();
        assert!(refuse_waiting_events(&mut events).is_err());
        drop(sender);
        assert!(refuse_waiting_events(&mut events).is_err());
    }

    #[test]
    fn handoff_tokens_fit_the_receiver_javascript_integer_range() {
        assert_eq!(handoff_token([0; 8]), 1);
        assert_eq!(handoff_token([u8::MAX; 8]), crate::desktop::MAX_TOKEN);
        assert!(handoff_token([0x80; 8]) <= crate::desktop::MAX_TOKEN);
    }

    #[test]
    fn handoff_requires_prepared_geometry_to_match_saved_target() {
        let handoff = HandoffOptions {
            return_mapping: crate::desktop::ReturnMapping {
                geometry: crate::desktop::Geometry {
                    monitors: vec![Rect {
                        x: -100,
                        y: 0,
                        width: 100,
                        height: 100,
                    }],
                },
                edge: Edge::Right,
                local_start: 0.0,
                local_end: 1.0,
                remote_start: 0.0,
                remote_end: 1.0,
            },
            entry_position: CursorPosition { x: -1.0, y: 50.0 },
            entry_region: Rect {
                x: -9,
                y: 0,
                width: 9,
                height: 100,
            },
            edge: Edge::Left,
            start: 0,
            end: crate::desktop::FRACTION_MAX,
            position: 500_000,
            expected_width: 2880,
            expected_height: 1620,
        };
        let prepared = DesktopResponse::Prepared {
            geometry: crate::desktop::Geometry {
                monitors: vec![Rect {
                    x: -2880,
                    y: -200,
                    width: 2880,
                    height: 1620,
                }],
            },
            position: Point { x: -2879, y: 610 },
        };
        assert!(validate_prepared(&handoff, prepared.clone()).is_ok());
        assert!(
            validate_prepared(
                &HandoffOptions {
                    expected_width: 3840,
                    ..handoff.clone()
                },
                prepared
            )
            .is_err()
        );
        assert!(validate_prepared(&handoff, DesktopResponse::Active).is_err());
        assert!(
            validate_prepared(&handoff, DesktopResponse::unavailable("missing extension")).is_err()
        );
    }

    #[test]
    fn handoff_return_requires_explicit_finish_acknowledgement() {
        assert!(validate_finished(Ok(DesktopResponse::Finished)).is_ok());
        assert!(validate_finished(Ok(DesktopResponse::Active)).is_err());
        assert!(
            validate_finished(Ok(DesktopResponse::unavailable("receiver still active"))).is_err()
        );
        assert!(validate_finished(Err(anyhow!("disconnected"))).is_err());
    }

    #[tokio::test]
    async fn cancellation_ignores_false_updates_and_accepts_sender_loss() {
        let (sender, mut receiver) = watch::channel(false);
        sender.send(false).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(5), stop_requested(&mut receiver))
                .await
                .is_err()
        );
        drop(sender);
        tokio::time::timeout(Duration::from_millis(50), stop_requested(&mut receiver))
            .await
            .unwrap();
    }

    fn touch_event(width: i32, height: i32, contacts: &[(i32, f32, f32)]) -> NativeEvent {
        let mut event = NativeEvent {
            kind: NativeEventKind::Touch as u32,
            contact_count: contacts.len() as u8,
            surface_width: width,
            surface_height: height,
            ..NativeEvent::default()
        };
        for (slot, &(id, x, y)) in event.contacts.iter_mut().zip(contacts) {
            *slot = NativeContact { id, x, y };
        }
        event
    }

    fn touch_state(event: NativeEvent) -> TouchState {
        native_frame(event)
            .unwrap()
            .unwrap()
            .frame
            .touch_snapshot
            .unwrap()
    }

    #[test]
    fn trackpad_contacts_are_sent_in_hundredths_of_a_millimetre() {
        // MultitouchSupport's origin is bottom-left; the wire's is top-left.
        let state = touch_state(touch_event(
            15_600,
            9_600,
            &[(1, 0.25, 0.75), (2, 1.0, 0.0)],
        ));
        let size = SourceDimensions {
            width: 15_600,
            height: 9_600,
        };
        let first = state.get(ContactId(1)).unwrap();
        assert_eq!((first.x, first.y), (3_900, 2_400));
        assert_eq!(first.source_dimensions, Some(size));
        let corner = state.get(ContactId(2)).unwrap();
        assert_eq!((corner.x, corner.y), (15_600, 9_600));
    }

    #[test]
    fn unknown_or_absurd_trackpad_size_falls_back_to_a_magic_trackpad() {
        for (width, height) in [(0, 0), (-1, 11_000), (16_000, 50_001), (i32::MAX, 1)] {
            let state = touch_state(touch_event(width, height, &[(1, 0.5, 0.5)]));
            let contact = state.get(ContactId(1)).unwrap();
            assert_eq!((contact.x, contact.y), (8_000, 5_750));
            assert_eq!(contact.source_dimensions, Some(FALLBACK_TRACKPAD_SIZE));
        }
        assert_eq!(
            trackpad_size(50_000, 1),
            SourceDimensions {
                width: 50_000,
                height: 1,
            }
        );
    }

    #[test]
    fn modifier_sides_follow_device_specific_flags() {
        let pairs = [
            (55, 54, 0x0010_0000, 0x0000_0008, 0x0000_0010),
            (56, 60, 0x0002_0000, 0x0000_0002, 0x0000_0004),
            (58, 61, 0x0008_0000, 0x0000_0020, 0x0000_0040),
            (59, 62, 0x0004_0000, 0x0000_0001, 0x0000_2000),
        ];

        for (left_keycode, right_keycode, aggregate, left, right) in pairs {
            let both_pressed = aggregate | left | right;
            assert!(modifier_pressed(left_keycode, both_pressed));
            assert!(modifier_pressed(right_keycode, both_pressed));

            let left_released = aggregate | right;
            assert!(!modifier_pressed(left_keycode, left_released));
            assert!(modifier_pressed(right_keycode, left_released));

            let right_released = aggregate | left;
            assert!(modifier_pressed(left_keycode, right_released));
            assert!(!modifier_pressed(right_keycode, right_released));
        }
    }

    #[test]
    fn raw_touch_suppresses_phased_scroll_even_after_lift() {
        for active in [false, true] {
            for phase in [1, 2, 4, 8, 16, 32, 128] {
                assert!(!forward_scroll(true, active, phase, 0));
                assert!(!forward_scroll(true, active, 0, phase));
                assert!(!forward_scroll(true, active, phase, phase));
            }
        }
    }

    #[test]
    fn raw_touch_preserves_unphased_wheel_after_lift() {
        assert!(forward_scroll(true, false, 0, 0));
        assert!(!forward_scroll(true, true, 0, 0));
    }

    #[test]
    fn pointer_only_capture_preserves_scroll_and_momentum() {
        for active in [false, true] {
            for (scroll, momentum) in [(0, 0), (1, 0), (2, 0), (0, 1), (0, 2), (0, 3)] {
                assert!(forward_scroll(false, active, scroll, momentum));
            }
        }
    }

    fn forward_scroll(raw: bool, active: bool, scroll: i64, momentum: i64) -> bool {
        // SAFETY: the helper accepts scalar values and does not access capture state.
        unsafe {
            zflow_mac_should_forward_scroll(u8::from(raw), u8::from(active), scroll, momentum) == 1
        }
    }

    fn modifier_pressed(keycode: u16, flags: u64) -> bool {
        // SAFETY: the helper accepts scalar values and does not access capture state.
        unsafe { zflow_mac_modifier_pressed(keycode, flags) == 1 }
    }
}
