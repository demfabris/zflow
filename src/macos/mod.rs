//! Foreground macOS source capture for the first Mac-to-Linux path.

mod awdl;

use std::{
    ffi::CStr,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::raw::c_char,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use quinn::Endpoint;
use tokio::sync::mpsc;

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
    identity::Identity,
    session::{SessionEventKind, SessionOptions, start_session},
    transport::{connect_input, input_client_config},
    wire::CURRENT_PROTOCOL_VERSION,
};

const CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(1);
const TOUCH_STALE_TIMEOUT: Duration = Duration::from_millis(150);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct SourceOptions {
    pub config_path: PathBuf,
    pub peer: String,
    pub address: Option<SocketAddr>,
    pub raw_touch: bool,
    pub reduce_wifi_latency: bool,
}

pub async fn run(options: SourceOptions) -> Result<()> {
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
    let endpoint = Endpoint::client(bind_address)?;
    println!("connecting to {} at {address}", options.peer);
    let connection = tokio::time::timeout(
        CONNECT_TIMEOUT,
        connect_input(&endpoint, address, &client_config),
    )
    .await
    .context("input connection timed out")??;

    let (session_events, mut events) = mpsc::channel(128);
    let session = start_session(
        connection,
        options.peer.clone(),
        TransportGeneration(1),
        SessionOptions::from_config(&config)?,
        session_events,
    )
    .await?;

    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut awdl_lease = if options.reduce_wifi_latency {
        Some(awdl::AwDlLease::acquire().await?)
    } else {
        None
    };

    let mut epoch = [0_u8; 16];
    getrandom::fill(&mut epoch)
        .map_err(|error| anyhow!("could not create the source session epoch: {error}"))?;
    session.begin_outbound(SessionContext {
        protocol_version: CURRENT_PROTOCOL_VERSION,
        session_epoch: SessionEpoch(epoch),
        transport_generation: session.generation(),
        activation_id: ActivationId(1),
    })?;

    let mut capture = MacCapture::start(raw_available)?;
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

    loop {
        tokio::select! {
            _ = awdl_renewal.tick(), if awdl_lease.is_some() => {
                if let Err(error) = awdl_lease.as_mut().expect("active AWDL lease").renew().await {
                    terminal_error = Some(error);
                    break;
                }
            }
            _ = interval.tick() => {
                while let Some(event) = capture.poll() {
                    if event.kind == NativeEventKind::Escape as u32 {
                        break;
                    }
                    if event.kind == NativeEventKind::Touch as u32 {
                        last_touch = Instant::now();
                        touch_active = event.contact_count > 0;
                    }
                    if let Some(frame) = native_frame(event)?
                        && let Err(error) = session.capture(frame)
                    {
                        terminal_error = Some(error);
                        break;
                    }
                }
                if capture.stop_requested() || terminal_error.is_some() {
                    break;
                }
                if touch_active && last_touch.elapsed() >= TOUCH_STALE_TIMEOUT {
                    session.capture(touch_frame(TouchState::default()))?;
                    touch_active = false;
                }
            }
            _ = interrupt.recv() => break,
            _ = terminate.recv() => break,
            _ = hangup.recv() => break,
            event = events.recv() => {
                match event.map(|event| event.kind) {
                    Some(SessionEventKind::ReceiverEffects { applied, .. }) => {
                        let message = "foreground macOS source does not inject received input".to_owned();
                        let _ = applied.send(Err(message.clone()));
                        terminal_error = Some(anyhow!(message));
                        break;
                    }
                    Some(SessionEventKind::Closed { reason }) => {
                        terminal_error = Some(anyhow!("input session closed: {reason}"));
                        break;
                    }
                    Some(SessionEventKind::OutboundEnded) => {
                        terminal_error = Some(anyhow!("remote input ownership ended"));
                        break;
                    }
                    None => {
                        terminal_error = Some(anyhow!("input session event channel closed"));
                        break;
                    }
                }
            }
        }
    }

    if let Err(error) = capture.stop() {
        eprintln!("{error:#}");
        terminal_error.get_or_insert(error);
    }
    if touch_active {
        let _ = session.capture(touch_frame(TouchState::default()));
    }
    if tokio::time::timeout(
        Duration::from_millis(200),
        session.end_outbound(SessionCloseReason::LocalRelease),
    )
    .await
    .is_err()
    {
        session.close(SessionCloseReason::LocalRelease);
    }
    endpoint.close(0_u32.into(), b"foreground source stopped");
    println!("remote input capture stopped");
    if let Some(lease) = awdl_lease.take()
        && let Err(error) = lease.release().await
    {
        eprintln!("Could not confirm AWDL restoration: {error:#}");
        terminal_error.get_or_insert(error);
    }

    if let Some(error) = terminal_error {
        return Err(error);
    }
    Ok(())
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
    fn zflow_mac_raw_touch_available() -> i32;
    fn zflow_mac_capture_start(raw_touch: i32) -> i32;
    fn zflow_mac_capture_poll(event: *mut NativeEvent) -> i32;
    fn zflow_mac_capture_stop_requested() -> i32;
    fn zflow_mac_capture_stop() -> i32;
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

    fn start(raw_touch: bool) -> Result<Self> {
        // SAFETY: start initializes the native thread before returning.
        let status = unsafe { zflow_mac_capture_start(i32::from(raw_touch)) };
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
        if self.running {
            // SAFETY: stop is idempotent after a successful start and joins the native thread.
            let status = unsafe { zflow_mac_capture_stop() };
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
