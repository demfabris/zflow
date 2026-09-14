//! Foreground macOS source capture for the first Mac-to-Linux path.

mod awdl;

use std::{
    ffi::CStr,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::raw::c_char,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use quinn::Endpoint;
use tokio::sync::{mpsc, watch};

use crate::{
    capture::{
        CaptureFrame, CaptureTransition, CapturedDeviceFrame, KeyState, MAX_TOUCHPAD_CONTACTS,
    },
    config::Config,
    core::{
        ActivationId, ContactId, HidUsage, MotionDelta, PointerButton, SessionCloseReason,
        SessionContext, SessionEpoch, SourceDimensions, TouchContact, TouchState, TouchTool,
        TransportGeneration,
    },
    desktop::{DesktopRequest, DesktopResponse, Edge},
    identity::Identity,
    session::{SessionEventKind, SessionOptions, start_session},
    transport::{InputClientConfig, connect_input, input_client_config},
    wire::CURRENT_PROTOCOL_VERSION,
};

const CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(1);
const TOUCH_STALE_TIMEOUT: Duration = Duration::from_millis(150);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct SourceOptions {
    pub config_path: PathBuf,
    pub peer: String,
    pub address: Option<SocketAddr>,
    pub raw_touch: bool,
    pub reduce_wifi_latency: bool,
    pub handoff: Option<HandoffOptions>,
}

#[derive(Clone, Debug)]
pub struct HandoffOptions {
    pub entry_position: CursorPosition,
    pub entry_region: DesktopRect,
    pub return_mapping: crate::desktop::ReturnMapping,
    pub edge: Edge,
    pub start: u32,
    pub end: u32,
    pub position: u32,
    pub expected_width: u32,
    pub expected_height: u32,
}

pub async fn run(options: SourceOptions) -> Result<()> {
    let (stop, stopped) = watch::channel(false);
    let (status, _events) = mpsc::unbounded_channel();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let signals = tokio::spawn(async move {
        tokio::select! {
            _ = terminate.recv() => {},
            _ = interrupt.recv() => {},
            _ = hangup.recv() => {},
        }
        let _ = stop.send(true);
    });
    let result = run_controlled(options, stopped, status).await;
    signals.abort();
    result
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceStatus {
    Connecting,
    Sharing,
    LocalInputRestored,
    Returned { position: u32 },
    Stopped,
    Failed(String),
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

impl DesktopRect {
    pub fn contains(self, position: CursorPosition) -> bool {
        [
            self.x,
            self.y,
            self.width,
            self.height,
            self.x + self.width,
            self.y + self.height,
            position.x,
            position.y,
        ]
        .iter()
        .all(|v| v.is_finite())
            && self.width > 0.0
            && self.height > 0.0
            && position.x >= self.x
            && position.y >= self.y
            && position.x < self.x + self.width
            && position.y < self.y + self.height
    }
}

/// `prompt` asks macOS to show its normal Accessibility permission UI.
pub fn accessibility_authorized(prompt: bool) -> bool {
    // SAFETY: the bridge creates and releases its own permission options.
    unsafe { zflow_mac_accessibility_authorized(i32::from(prompt)) == 1 }
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

/// Move the local cursor after source cleanup, using global desktop coordinates.
pub fn warp_cursor(position: CursorPosition) -> Result<()> {
    let _guard = SourceGuard::acquire()?;
    if !position.x.is_finite()
        || !position.y.is_finite()
        || !active_desktop_rectangles()?
            .iter()
            .any(|rect| rect.contains(position))
    {
        bail!("cursor return position is outside the active Mac displays");
    }
    // SAFETY: finite, visible coordinates were checked; no source can start
    // while this operation holds the source guard.
    if unsafe { zflow_mac_warp_cursor(position) } != 0 {
        bail!("could not return the Mac cursor to its display");
    }
    Ok(())
}

pub fn input_is_neutral() -> bool {
    // SAFETY: this reads physical key, button and modifier state without capture.
    unsafe { zflow_mac_input_is_neutral() == 1 }
}

/// Stop by setting the watch value to true or dropping its sender, then await
/// completion so cursor, session and optional radio cleanup can finish.
pub async fn run_controlled(
    options: SourceOptions,
    mut stop: watch::Receiver<bool>,
    status: mpsc::UnboundedSender<SourceStatus>,
) -> Result<()> {
    let started = Instant::now();
    tracing::info!(peer = %options.peer, raw_touch = options.raw_touch,
        reduce_wifi_latency = options.reduce_wifi_latency, "source worker started");
    let _ = status.send(SourceStatus::Connecting);
    let result = run_source(options, &mut stop, &status).await;
    let final_status = match &result {
        Ok(returned) => {
            if let Some(position) = returned {
                let _ = status.send(SourceStatus::Returned {
                    position: *position,
                });
            }
            SourceStatus::Stopped
        }
        Err(error) => {
            tracing::error!(error = %format!("{error:#}"), "source worker failed");
            eprintln!("Remote input failed: {error:#}");
            SourceStatus::Failed(format!("{error:#}"))
        }
    };
    let _ = status.send(final_status);
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        success = result.is_ok(),
        "source worker completed"
    );
    result.map(|_| ())
}

async fn stop_requested(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() || stop.changed().await.is_err() {
            return;
        }
    }
}

static SOURCE_ACTIVE: AtomicBool = AtomicBool::new(false);

struct SourceGuard;

impl SourceGuard {
    fn acquire() -> Result<Self> {
        SOURCE_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow!("a macOS input source is already running"))?;
        Ok(Self)
    }
}

impl Drop for SourceGuard {
    fn drop(&mut self) {
        SOURCE_ACTIVE.store(false, Ordering::Release);
    }
}

struct SourceEndpoint(Endpoint);

impl SourceEndpoint {
    async fn shutdown(&self) -> Result<()> {
        let started = Instant::now();
        tracing::debug!("QUIC endpoint shutdown started");
        self.0.close(0_u32.into(), b"foreground source stopped");
        // Keep the worker runtime alive until Quinn has sent the disconnect.
        // Otherwise a new crossing can reach Ubuntu before the old session ends.
        let result = tokio::time::timeout(Duration::from_secs(2), self.0.wait_idle())
            .await
            .context("timed out closing the input connection; sharing remains off");
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            success = result.is_ok(),
            "QUIC endpoint shutdown completed"
        );
        result
    }
}

impl Drop for SourceEndpoint {
    fn drop(&mut self) {
        self.0.close(0_u32.into(), b"foreground source stopped");
    }
}

async fn run_source(
    options: SourceOptions,
    stop: &mut watch::Receiver<bool>,
    status: &mpsc::UnboundedSender<SourceStatus>,
) -> Result<Option<u32>> {
    if *stop.borrow() || stop.has_changed().is_err() {
        return Ok(None);
    }
    let _guard = SourceGuard::acquire()?;
    let mut config = Config::load(&options.config_path)?;
    let peer = config
        .peers
        .get(&options.peer)
        .cloned()
        .with_context(|| format!("unknown paired peer {}", options.peer))?;
    if !peer.permissions.connect || !peer.permissions.receive_normal {
        bail!(
            "peer {} is not permitted to receive normal input",
            options.peer
        );
    }

    let raw_requested = options.raw_touch && config.input.experimental_touchpad;
    let raw_available = raw_requested && MacCapture::raw_touch_available();
    if raw_requested && !raw_available {
        eprintln!(
            "raw Magic Trackpad capture unavailable: {}; falling back to pointer and scroll",
            MacCapture::last_error()
        );
    }
    config.input.experimental_touchpad = raw_available;

    let address = options
        .address
        .or_else(|| peer.addresses.first().copied())
        .with_context(|| format!("peer {} has no input address", options.peer))?;
    let identity = Identity::load_or_create(&config.daemon.state_dir)?;
    let client_config = input_client_config(&identity, &peer.spki_der()?)?;
    let bind_address = match address.ip() {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };
    let endpoint = SourceEndpoint(Endpoint::client(bind_address)?);
    let result = run_endpoint(
        &endpoint.0,
        &client_config,
        address,
        config,
        options,
        stop,
        status,
    )
    .await;
    if let Err(error) = endpoint.shutdown().await {
        if result.is_ok() {
            return Err(error);
        }
        eprintln!("Could not finish input connection shutdown: {error:#}");
    }
    result
}

async fn run_endpoint(
    endpoint: &Endpoint,
    client_config: &InputClientConfig,
    address: SocketAddr,
    config: Config,
    options: SourceOptions,
    stop: &mut watch::Receiver<bool>,
    status: &mpsc::UnboundedSender<SourceStatus>,
) -> Result<Option<u32>> {
    let raw_available = config.input.experimental_touchpad;
    let connecting = Instant::now();
    tracing::info!(peer = %options.peer, %address, "input connection starting");
    println!("connecting to {} at {address}", options.peer);
    let connection = tokio::select! {
        biased;
        _ = stop_requested(stop) => {
            tracing::info!(elapsed_ms = connecting.elapsed().as_millis() as u64, "cancelled during connection");
            return Ok(None);
        },
        result = tokio::time::timeout(
        CONNECT_TIMEOUT,
        connect_input(endpoint, address, client_config),
    )
        => result.context("input connection timed out")??,
    };
    tracing::info!(
        elapsed_ms = connecting.elapsed().as_millis() as u64,
        "QUIC connection established"
    );

    let (session_events, mut events) = mpsc::channel(128);
    let negotiating = Instant::now();
    let session = tokio::select! {
        biased;
        _ = stop_requested(stop) => {
            tracing::info!("cancelled during session negotiation");
            return Ok(None);
        },
        result = start_session(
        connection,
        options.peer.clone(),
        TransportGeneration(1),
        SessionOptions::from_config(&config)?,
        session_events,
        ) => result?,
    };
    tracing::info!(
        session_id = session.id(),
        elapsed_ms = negotiating.elapsed().as_millis() as u64,
        "input session negotiated"
    );

    let mut awdl_lease = None;
    let mut prepared = None;
    let mut activation_started = false;
    let mut returned = None;
    let local_desktop = options
        .handoff
        .as_ref()
        .map(|_| active_desktop_rectangles())
        .transpose()?;

    let result = async {
        if *stop.borrow() || stop.has_changed().is_err() {
            return Ok(());
        }
        if options.reduce_wifi_latency {
            awdl_lease = tokio::select! {
                biased;
                _ = stop_requested(stop) => return Ok(()),
                result = awdl::AwDlLease::acquire() => Some(result?),
            };
        }
        if let Some(handoff) = &options.handoff {
            let preparing = Instant::now();
            tracing::info!("desktop preparation starting");
            let mut token_bytes = [0_u8; 8];
            getrandom::fill(&mut token_bytes)
                .map_err(|error| anyhow!("could not create a desktop handoff token: {error}"))?;
            let token = handoff_token(token_bytes);
            let request = DesktopRequest::Prepare {
                token, edge: handoff.edge, start: handoff.start,
                end: handoff.end, position: handoff.position,
            };
            request.validate()?;
            // Even a cancelled request may already have reached the receiver.
            prepared = Some(token);
            let response = tokio::select! {
                biased;
                _ = stop_requested(stop) => {
                    tracing::info!(elapsed_ms = preparing.elapsed().as_millis() as u64, "cancelled during desktop preparation");
                    return Ok(());
                },
                response = session.desktop_request(request) => response.context(
                    "Could not prepare desktop handoff. On Ubuntu, run just install-linux to update and restart the installed zflowd service; just run updates only the GUI. If the GNOME integration was newly installed, log out and back in, then enable desktop handoff in the Ubuntu app"
                )?,
            };
            validate_prepared(handoff, response)?;
            tracing::info!(elapsed_ms = preparing.elapsed().as_millis() as u64, "desktop preparation completed");
        }
        if *stop.borrow() || stop.has_changed().is_err() {
            return Ok(());
        }
        if options.handoff.is_some() && !input_is_neutral() {
            bail!("release held keys and buttons before crossing to the other computer");
        }
        if let Some(handoff) = &options.handoff {
            let current = cursor_position()?;
            tracing::debug!(expected_x = handoff.entry_position.x, expected_y = handoff.entry_position.y,
                current_x = current.x, current_y = current.y, "checking cursor before capture");
            validate_entry_position(handoff.entry_region, current)?;
            if local_desktop.as_ref() != Some(&active_desktop_rectangles()?) {
                bail!("the Mac desktop changed while connecting; refresh and save its layout");
            }
        }
        let mut epoch = [0_u8; 16];
        getrandom::fill(&mut epoch)
            .map_err(|error| anyhow!("could not create the source session epoch: {error}"))?;
        session.begin_outbound(SessionContext {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            session_epoch: SessionEpoch(epoch),
            transport_generation: session.generation(),
            activation_id: ActivationId(1),
        })?;
        activation_started = true;

        let capture_start = Instant::now();
        tracing::debug!("native capture starting");
        let mut capture = MacCapture::start(raw_available, options.handoff.as_ref().map(|h| h.entry_region))
            .inspect_err(|error| {
                tracing::warn!(elapsed_ms = capture_start.elapsed().as_millis() as u64,
                    cursor = ?cursor_position().ok().map(|p| (p.x, p.y)),
                    error = %error, "native capture start rejected");
            })?;
        tracing::info!(elapsed_ms = capture_start.elapsed().as_millis() as u64,
            connection_to_capture_ms = connecting.elapsed().as_millis() as u64, "native capture started");
        let _ = status.send(SourceStatus::Sharing);
        println!(
            "remote input active: raw_touch={}; escape with Ctrl+Cmd+Backspace or Ctrl+C",
            capture.raw_touch
        );

        let mut interval = tokio::time::interval(CAPTURE_POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut awdl_renewal = tokio::time::interval(awdl::RENEW_INTERVAL);
        awdl_renewal.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut touch_active = false;
        let mut last_touch = Instant::now();
        let mut terminal_error = None;
        let mut stop_reason = "stop requested";
        let mut captured_events = 0_u64;
        let mut last_capture_poll = Instant::now();
        let mut max_poll_gap = Duration::ZERO;
        let desktop_poll = poll_desktop(&session, prepared);
        tokio::pin!(desktop_poll);

        loop {
            tokio::select! {
                biased;
                _ = stop_requested(stop) => break,
                response = &mut desktop_poll, if prepared.is_some() => {
                    match response {
                        Ok(DesktopResponse::Active) => desktop_poll.set(poll_desktop(&session, prepared)),
                        Ok(DesktopResponse::Returned { position }) => {
                            stop_reason = "GNOME return edge";
                            tracing::info!(position, "receiver reported return edge");
                            if let Some(handoff) = &options.handoff
                                && (position < handoff.start || position > handoff.end)
                            {
                                terminal_error = Some(anyhow!("receiver returned outside the configured edge range"));
                            } else {
                                returned = Some(position);
                            }
                            break;
                        },
                        Ok(DesktopResponse::Unavailable { reason }) => {
                            stop_reason = "desktop unavailable";
                            terminal_error = Some(anyhow!("receiver desktop unavailable: {reason}"));
                            break;
                        },
                        Ok(_) => {
                            stop_reason = "unexpected desktop response";
                            terminal_error = Some(anyhow!("unexpected receiver desktop response"));
                            break;
                        },
                        Err(error) => {
                            stop_reason = "desktop request failed";
                            terminal_error = Some(error);
                            break;
                        }
                    }
                },
                _ = awdl_renewal.tick(), if awdl_lease.is_some() => {
                    if let Err(error) = awdl_lease.as_mut().expect("active AWDL lease").renew().await {
                        stop_reason = "AWDL renewal failed";
                        terminal_error = Some(error);
                        break;
                    }
                }
                _ = interval.tick() => {
                    max_poll_gap = max_poll_gap.max(last_capture_poll.elapsed());
                    last_capture_poll = Instant::now();
                    while let Some(event) = capture.poll() {
                        captured_events += 1;
                        if event.kind == NativeEventKind::Escape as u32 {
                            stop_reason = "native escape event";
                            break;
                        }
                        if event.kind == NativeEventKind::Touch as u32 {
                            last_touch = Instant::now();
                            touch_active = event.contact_count > 0;
                        }
                        match native_frame(event).and_then(|frame| {
                            frame.map_or(Ok(()), |frame| session.capture(frame))
                        }) {
                            Ok(()) => {},
                            Err(error) => {
                                stop_reason = "capture forwarding failed";
                                terminal_error = Some(error);
                                break;
                            }
                        }
                    }
                    if capture.stop_requested() || terminal_error.is_some() {
                        if terminal_error.is_none() && stop_reason != "native escape event" {
                            stop_reason = "native capture requested stop";
                        }
                        break;
                    }
                    if touch_active && last_touch.elapsed() >= TOUCH_STALE_TIMEOUT {
                        if let Err(error) = session.capture(touch_frame(TouchState::default())) {
                            terminal_error = Some(error);
                            break;
                        }
                        touch_active = false;
                    }
                }
                event = events.recv() => {
                    match event.map(|event| event.kind) {
                        Some(SessionEventKind::Desktop { reply, .. }) => {
                            let _ = reply.send(DesktopResponse::unavailable("Mac source cannot receive desktop handoffs"));
                        }
                        Some(SessionEventKind::ReceiverEffects { applied, .. }) => {
                            let message = "foreground macOS source does not inject received input".to_owned();
                            let _ = applied.send(Err(message.clone()));
                            terminal_error = Some(anyhow!(message));
                            break;
                        }
                        Some(SessionEventKind::Closed { reason }) => {
                            stop_reason = "input session closed";
                            terminal_error = Some(anyhow!("input session closed: {reason}"));
                            break;
                        }
                        Some(SessionEventKind::OutboundEnded) => {
                            stop_reason = "remote ownership ended";
                            terminal_error = Some(anyhow!("remote input ownership ended"));
                            break;
                        }
                        None => {
                            stop_reason = "session event channel closed";
                            terminal_error = Some(anyhow!("input session event channel closed"));
                            break;
                        }
                    }
                }
            }
        }

        tracing::info!(reason = stop_reason, captured_events,
            active_ms = capture_start.elapsed().as_millis() as u64,
            max_capture_poll_gap_ms = max_poll_gap.as_millis() as u64,
            error = ?terminal_error.as_ref().map(|error| format!("{error:#}")), "stopping native capture");
        let return_point = returned.and_then(|position| {
            let result = options.handoff.as_ref().context("missing desktop return mapping")
                .and_then(|handoff| handoff.return_mapping.position(position))
                .and_then(|point| {
                    anyhow::ensure!(local_desktop.as_ref() == Some(&active_desktop_rectangles()?),
                        "the Mac desktop changed before returning input");
                    Ok(CursorPosition { x: f64::from(point.x), y: f64::from(point.y) })
                });
            match result {
                Ok(point) => Some(point),
                Err(error) => { terminal_error.get_or_insert(error); None }
            }
        });
        if let Err(error) = capture.stop_at(return_point) {
            eprintln!("{error:#}");
            terminal_error.get_or_insert(error);
        } else if return_point.is_some() {
            tracing::info!(point = ?return_point, "Mac cursor positioned before local input resumed");
            let _ = status.send(SourceStatus::LocalInputRestored);
        }
        if touch_active {
            let _ = session.capture(touch_frame(TouchState::default()));
        }
        println!("remote input capture stopped");
        terminal_error.map_or(Ok(()), Err)
    }.await;
    let mut result = result;
    if activation_started {
        let releasing = Instant::now();
        let released = tokio::time::timeout(
            Duration::from_millis(200),
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
        if let Err(error) = released {
            eprintln!("Could not confirm remote input release: {error:#}");
            if result.is_ok() && returned.is_some() {
                result = Err(error);
            }
            session.close(SessionCloseReason::LocalRelease);
        }
    }
    if let Some(token) = prepared {
        let finishing = Instant::now();
        let finished = session
            .desktop_request(DesktopRequest::Finish { token })
            .await;
        let finished = validate_finished(finished);
        tracing::info!(
            elapsed_ms = finishing.elapsed().as_millis() as u64,
            success = finished.is_ok(),
            "desktop handoff cleanup completed"
        );
        if let Err(error) = finished {
            eprintln!("Could not confirm desktop handoff cleanup: {error:#}");
            if result.is_ok() && returned.is_some() {
                result = Err(error);
            }
        }
    }
    session.close(SessionCloseReason::LocalRelease);
    if let Some(lease) = awdl_lease.take()
        && let Err(error) = lease.release().await
    {
        eprintln!("Could not confirm AWDL restoration: {error:#}");
        if result.is_ok() {
            result = Err(error);
        }
    }

    result.map(|_| returned)
}

fn validate_prepared(handoff: &HandoffOptions, response: DesktopResponse) -> Result<()> {
    response.validate()?;
    match response {
        DesktopResponse::Prepared { geometry, .. } => {
            let bounds = geometry.bounds()?;
            if bounds.width != handoff.expected_width || bounds.height != handoff.expected_height {
                bail!(
                    "receiver desktop changed size; refresh and save the computer layout before sharing"
                );
            }
            Ok(())
        }
        DesktopResponse::Unavailable { reason } => bail!("receiver desktop unavailable: {reason}"),
        _ => bail!("receiver did not prepare its desktop for input"),
    }
}

fn handoff_token(random: [u8; 8]) -> u64 {
    (u64::from_ne_bytes(random) & crate::desktop::MAX_TOKEN).max(1)
}

fn validate_entry_position(region: DesktopRect, current: CursorPosition) -> Result<()> {
    if !region.contains(current) {
        bail!(
            "crossing cancelled because the Mac cursor left the configured edge while connecting"
        );
    }
    Ok(())
}

fn validate_finished(response: Result<DesktopResponse>) -> Result<()> {
    match response? {
        DesktopResponse::Finished => Ok(()),
        DesktopResponse::Unavailable { reason } => {
            bail!("receiver could not finish desktop handoff: {reason}")
        }
        _ => bail!("receiver did not confirm desktop handoff cleanup"),
    }
}

async fn poll_desktop(
    session: &crate::session::SessionHandle,
    token: Option<u64>,
) -> Result<DesktopResponse> {
    tokio::time::sleep(Duration::from_millis(16)).await;
    session
        .desktop_request(DesktopRequest::Poll {
            token: token.context("desktop handoff has no token")?,
        })
        .await
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
            let Some(usage) = mac_keycode_to_hid(event.code) else {
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
            let count = usize::from(event.contact_count).min(MAX_TOUCHPAD_CONTACTS);
            let contacts = event.contacts[..count].iter().filter_map(|contact| {
                let id = u32::try_from(contact.id).ok()?;
                let x = normalized_axis(contact.x);
                // MultitouchSupport uses a bottom-left origin; Linux touchpads use top-left.
                let y = normalized_axis(1.0 - contact.y);
                Some(TouchContact {
                    id: ContactId(id),
                    x,
                    y,
                    pressure: None,
                    major: None,
                    minor: None,
                    orientation_millidegrees: None,
                    tool: TouchTool::Finger,
                    source_dimensions: Some(SourceDimensions {
                        width: u16::MAX.into(),
                        height: u16::MAX.into(),
                    }),
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

fn normalized_axis(value: f32) -> i32 {
    (value.clamp(0.0, 1.0) * f32::from(u16::MAX)).round() as i32
}

fn pressed_state(pressed: u8) -> KeyState {
    if pressed == 0 {
        KeyState::Released
    } else {
        KeyState::Pressed
    }
}

fn mac_keycode_to_hid(code: u16) -> Option<HidUsage> {
    let usage = match code {
        0 => 0x04,
        1 => 0x16,
        2 => 0x07,
        3 => 0x09,
        4 => 0x0b,
        5 => 0x0a,
        6 => 0x1d,
        7 => 0x1b,
        8 => 0x06,
        9 => 0x19,
        11 => 0x05,
        12 => 0x14,
        13 => 0x1a,
        14 => 0x08,
        15 => 0x15,
        16 => 0x1c,
        17 => 0x17,
        18 => 0x1e,
        19 => 0x1f,
        20 => 0x20,
        21 => 0x21,
        22 => 0x23,
        23 => 0x22,
        24 => 0x2e,
        25 => 0x26,
        26 => 0x24,
        27 => 0x2d,
        28 => 0x25,
        29 => 0x27,
        30 => 0x30,
        31 => 0x12,
        32 => 0x18,
        33 => 0x2f,
        34 => 0x0c,
        35 => 0x13,
        36 => 0x28,
        37 => 0x0f,
        38 => 0x0d,
        39 => 0x34,
        40 => 0x0e,
        41 => 0x33,
        42 => 0x31,
        43 => 0x36,
        44 => 0x38,
        45 => 0x11,
        46 => 0x10,
        47 => 0x37,
        48 => 0x2b,
        49 => 0x2c,
        50 => 0x35,
        51 => 0x2a,
        53 => 0x29,
        54 => 0xe7,
        55 => 0xe3,
        56 => 0xe1,
        57 => 0x39,
        58 => 0xe2,
        59 => 0xe0,
        60 => 0xe5,
        61 => 0xe6,
        62 => 0xe4,
        65 => 0x63,
        67 => 0x55,
        69 => 0x57,
        71 => 0x53,
        75 => 0x54,
        76 => 0x58,
        78 => 0x56,
        81 => 0x67,
        82 => 0x62,
        83 => 0x59,
        84 => 0x5a,
        85 => 0x5b,
        86 => 0x5c,
        87 => 0x5d,
        88 => 0x5e,
        89 => 0x5f,
        91 => 0x60,
        92 => 0x61,
        96 => 0x3e,
        97 => 0x3f,
        98 => 0x40,
        99 => 0x3c,
        100 => 0x41,
        101 => 0x42,
        103 => 0x44,
        105 => 0x68,
        106 => 0x6b,
        107 => 0x69,
        109 => 0x43,
        111 => 0x45,
        113 => 0x6a,
        114 => 0x49,
        115 => 0x4a,
        116 => 0x4b,
        117 => 0x4c,
        118 => 0x3d,
        119 => 0x4d,
        120 => 0x3b,
        121 => 0x4e,
        122 => 0x3a,
        123 => 0x50,
        124 => 0x4f,
        125 => 0x51,
        126 => 0x52,
        _ => return None,
    };
    Some(HidUsage::keyboard(usage))
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
    contacts: [NativeContact; MAX_TOUCHPAD_CONTACTS],
}

unsafe extern "C" {
    fn zflow_mac_accessibility_authorized(prompt: i32) -> i32;
    fn zflow_mac_cursor_position(position: *mut CursorPosition) -> i32;
    fn zflow_mac_desktop_rectangles(rectangles: *mut DesktopRect, capacity: u32) -> i32;
    fn zflow_mac_warp_cursor(position: CursorPosition) -> i32;
    fn zflow_mac_input_is_neutral() -> i32;
    fn zflow_mac_raw_touch_available() -> i32;
    fn zflow_mac_capture_start(raw_touch: i32, entry: *const DesktopRect) -> i32;
    fn zflow_mac_capture_stop_at(position: *const CursorPosition) -> i32;
    fn zflow_mac_capture_poll(event: *mut NativeEvent) -> i32;
    fn zflow_mac_capture_stop_requested() -> i32;
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
    fn raw_touch_available() -> bool {
        // SAFETY: the C preflight owns and releases all temporary framework objects.
        unsafe { zflow_mac_raw_touch_available() == 1 }
    }

    fn start(raw_touch: bool, entry: Option<DesktopRect>) -> Result<Self> {
        // SAFETY: start initializes the native thread before returning.
        let status = unsafe {
            zflow_mac_capture_start(
                i32::from(raw_touch),
                entry.as_ref().map_or(std::ptr::null(), |position| position),
            )
        };
        if status < 0 {
            bail!("could not start macOS capture: {}", Self::last_error());
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

    fn stop(&mut self) -> Result<()> {
        self.stop_at(None)
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

impl Drop for MacCapture {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("{error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALPHA_SHIFT: u64 = 0x0001_0000;

    #[tokio::test]
    async fn source_worker_shutdown_notifies_peer_before_its_runtime_exits() {
        use crate::transport::{accept_input, input_server_config};

        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let left = Identity::load_or_create(left_dir.path()).unwrap();
        let right = Identity::load_or_create(right_dir.path()).unwrap();
        let client_config = input_client_config(&left, right.spki()).unwrap();
        let server_config = input_server_config(&right, left.spki()).unwrap();
        let server =
            Endpoint::server(server_config.quinn_config(), "127.0.0.1:0".parse().unwrap()).unwrap();
        let address = server.local_addr().unwrap();
        // A crossing uses a fresh thread and runtime. Observe each disconnect
        // from a separate runtime, including when the client runtime is gone.
        for _ in 0..3 {
            let client_config = client_config.clone();
            let (accepted, ready) = tokio::sync::oneshot::channel();
            let worker = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let endpoint = SourceEndpoint(
                            Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap(),
                        );
                        let _connection = connect_input(&endpoint.0, address, &client_config)
                            .await
                            .unwrap();
                        ready.await.unwrap();
                        endpoint.shutdown().await.unwrap();
                    });
            });
            let connection = tokio::time::timeout(Duration::from_secs(3), async {
                accept_input(server.accept().await.unwrap(), &server_config)
                    .await
                    .unwrap()
            })
            .await
            .unwrap();
            accepted.send(()).unwrap();
            tokio::task::spawn_blocking(move || worker.join().unwrap())
                .await
                .unwrap();
            let reason = tokio::time::timeout(Duration::from_millis(250), connection.closed())
                .await
                .expect("the peer must not wait for the old connection's idle timeout");
            assert!(matches!(
                reason,
                quinn::ConnectionError::ApplicationClosed(_)
            ));
        }
        server.close(0_u32.into(), b"test finished");
    }

    #[test]
    fn handoff_tokens_fit_the_receiver_javascript_integer_range() {
        assert_eq!(handoff_token([0; 8]), 1);
        assert_eq!(handoff_token([u8::MAX; 8]), crate::desktop::MAX_TOKEN);
        assert!(handoff_token([0x80; 8]) <= crate::desktop::MAX_TOKEN);
    }

    #[test]
    fn entry_admission_allows_recorded_along_edge_motion_but_rejects_departure() {
        let region = DesktopRect {
            x: 0.0,
            y: 62.0,
            width: 9.0,
            height: 1620.0,
        };
        for y in [928.8125, 917.8125, 62.0, 1681.99] {
            assert!(validate_entry_position(region, CursorPosition { x: 0.0, y }).is_ok());
        }
        for point in [
            CursorPosition {
                x: 9.0,
                y: 917.8125,
            },
            CursorPosition {
                x: -1.0,
                y: 917.8125,
            },
            CursorPosition { x: 0.0, y: 61.99 },
            CursorPosition { x: 0.0, y: 1682.0 },
            CursorPosition {
                x: f64::NAN,
                y: 917.8125,
            },
        ] {
            assert!(validate_entry_position(region, point).is_err());
        }
        assert!(
            !DesktopRect {
                width: f64::INFINITY,
                ..region
            }
            .contains(CursorPosition { x: 0.0, y: 100.0 })
        );
    }

    #[test]
    fn handoff_requires_prepared_geometry_to_match_saved_target() {
        let handoff = HandoffOptions {
            return_mapping: crate::desktop::ReturnMapping {
                geometry: crate::desktop::Geometry {
                    monitors: vec![crate::desktop::Rect {
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
            entry_region: DesktopRect {
                x: -9.0,
                y: 0.0,
                width: 9.0,
                height: 100.0,
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
                monitors: vec![crate::desktop::Rect {
                    x: -2880,
                    y: -200,
                    width: 2880,
                    height: 1620,
                }],
            },
            position: crate::desktop::Point { x: -2879, y: 610 },
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

    #[test]
    fn source_guard_excludes_concurrent_sources_and_releases_on_drop() {
        let guard = SourceGuard::acquire().unwrap();
        assert!(SourceGuard::acquire().is_err());
        drop(guard);
        assert!(SourceGuard::acquire().is_ok());
    }

    #[test]
    fn desktop_positions_preserve_negative_origins_and_exclude_outer_boundary() {
        let rect = DesktopRect {
            x: -1920.0,
            y: -200.0,
            width: 1920.0,
            height: 1080.0,
        };
        assert!(rect.contains(CursorPosition {
            x: -1919.0,
            y: -199.0
        }));
        assert!(!rect.contains(CursorPosition { x: 0.0, y: 0.0 }));
        assert!(!rect.contains(CursorPosition { x: -1.0, y: 880.0 }));
        assert!(!rect.contains(CursorPosition {
            x: f64::NAN,
            y: 0.0
        }));
    }

    #[tokio::test]
    async fn controlled_source_cancels_before_config_or_capture() {
        for close_sender in [false, true] {
            let (stop, receiver) = watch::channel(!close_sender);
            if close_sender {
                drop(stop);
            }
            let (status, mut events) = mpsc::unbounded_channel();
            run_controlled(
                SourceOptions {
                    config_path: PathBuf::from("/nonexistent-zflow-test-config"),
                    peer: "unused".into(),
                    address: None,
                    raw_touch: false,
                    reduce_wifi_latency: false,
                    handoff: None,
                },
                receiver,
                status,
            )
            .await
            .unwrap();
            assert_eq!(events.recv().await, Some(SourceStatus::Connecting));
            assert_eq!(events.recv().await, Some(SourceStatus::Stopped));
            assert_eq!(events.recv().await, None);
        }
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

    #[test]
    fn maps_main_return_to_hid_return() {
        assert_eq!(mac_keycode_to_hid(36), Some(HidUsage::keyboard(0x28)));
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
    fn caps_lock_uses_the_aggregate_flag() {
        assert!(modifier_pressed(57, ALPHA_SHIFT));
        assert!(!modifier_pressed(57, 0));
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
