//! Reads the physical mouse via evdev and streams cumulative motion totals
//! over UDP. Usage:
//!
//!   sender <dest-ip:port> [--device /dev/input/eventX] [--rate 250] [--grab]
//!
//! Without --device, picks the first device exposing REL_X and REL_Y.
//! --grab takes the mouse away from the local session until exit (ctrl+c
//! from the keyboard still works; the keyboard is never touched).

use std::net::UdpSocket;
use std::path::PathBuf;
use std::process::exit;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use evdev::{Device, InputEventKind, RelativeAxisType, Synchronization};
use spike_b_smoothing::MotionFrame;

/// Totals committed per SYN_REPORT so a frame never carries a torn X/Y pair,
/// with the capture time of the commit.
#[derive(Clone, Copy, Default)]
struct Snapshot {
    total_dx: i64,
    total_dy: i64,
    t_capture_us: u64,
}

fn usage() -> ! {
    eprintln!("usage: sender <dest-ip:port> [--device /dev/input/eventX] [--rate HZ] [--grab]");
    exit(2)
}

fn find_mouse() -> Option<(PathBuf, Device)> {
    for (path, dev) in evdev::enumerate() {
        if dev.name().map_or(false, |n| n.contains("zflow")) {
            continue; // never capture our own virtual devices
        }
        if let Some(rel) = dev.supported_relative_axes() {
            if rel.contains(RelativeAxisType::REL_X) && rel.contains(RelativeAxisType::REL_Y) {
                return Some((path, dev));
            }
        }
    }
    None
}

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let dest = args.next().unwrap_or_else(|| usage());
    let mut device_path: Option<String> = None;
    let mut rate_hz: u64 = 250;
    let mut grab = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--device" => device_path = Some(args.next().unwrap_or_else(|| usage())),
            "--rate" => rate_hz = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--grab" => grab = true,
            _ => usage(),
        }
    }
    let rate_hz = rate_hz.clamp(20, 1000);

    let (path, mut dev) = match device_path {
        Some(p) => (PathBuf::from(&p), Device::open(&p)?),
        None => find_mouse().unwrap_or_else(|| {
            eprintln!("no relative pointer device found (are you in the input group?)");
            exit(1)
        }),
    };
    eprintln!("capturing {} ({})", path.display(), dev.name().unwrap_or("?"));
    if grab {
        dev.grab()?;
        eprintln!("grabbed: local cursor is frozen until this process exits");
    }

    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.connect(&dest)?;

    let session: u64 = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        (d.as_nanos() as u64) ^ ((std::process::id() as u64) << 48)
    };
    eprintln!("streaming to {dest} at {rate_hz} Hz, session {session:016x}");

    let start = Instant::now();
    let snap = Arc::new(Mutex::new(Snapshot::default()));

    {
        let snap = snap.clone();
        std::thread::spawn(move || {
            let mut acc = (0i64, 0i64);
            loop {
                let events = match dev.fetch_events() {
                    Ok(ev) => ev,
                    Err(e) => {
                        eprintln!("device read failed: {e}");
                        exit(1);
                    }
                };
                for ev in events {
                    match ev.kind() {
                        InputEventKind::RelAxis(RelativeAxisType::REL_X) => {
                            acc.0 += ev.value() as i64
                        }
                        InputEventKind::RelAxis(RelativeAxisType::REL_Y) => {
                            acc.1 += ev.value() as i64
                        }
                        InputEventKind::Synchronization(Synchronization::SYN_REPORT) => {
                            if acc != (0, 0) {
                                let mut s = snap.lock().unwrap();
                                s.total_dx += acc.0;
                                s.total_dy += acc.1;
                                s.t_capture_us = start.elapsed().as_micros() as u64;
                                acc = (0, 0);
                            }
                        }
                        _ => {}
                    }
                }
                // batch without a trailing SYN_REPORT: commit anyway
                if acc != (0, 0) {
                    let mut s = snap.lock().unwrap();
                    s.total_dx += acc.0;
                    s.total_dy += acc.1;
                    s.t_capture_us = start.elapsed().as_micros() as u64;
                    acc = (0, 0);
                }
            }
        });
    }

    let tick = Duration::from_nanos(1_000_000_000 / rate_hz);
    let heartbeat = Duration::from_millis(50);
    let mut seq: u64 = 0;
    let mut last_sent = (i64::MIN, i64::MIN);
    let mut last_send_time = Instant::now();
    let mut sent_frames: u64 = 0;
    let mut last_report = Instant::now();

    loop {
        std::thread::sleep(tick);
        let s = *snap.lock().unwrap();
        let totals = (s.total_dx, s.total_dy);
        let moved = totals != last_sent;
        if !moved && last_send_time.elapsed() < heartbeat {
            continue;
        }
        let frame = MotionFrame {
            session,
            seq,
            // motion frames carry the commit time of their last input batch so
            // sender pacing shows up as delay; idle heartbeats carry send time
            t_capture_us: if moved {
                s.t_capture_us
            } else {
                start.elapsed().as_micros() as u64
            },
            total_dx: totals.0,
            total_dy: totals.1,
        };
        if let Err(e) = sock.send(&frame.encode()) {
            eprintln!("send failed: {e}");
        }
        seq += 1;
        sent_frames += 1;
        last_sent = totals;
        last_send_time = Instant::now();

        if last_report.elapsed() >= Duration::from_secs(5) {
            eprintln!(
                "sent {} frames, totals ({}, {})",
                sent_frames, totals.0, totals.1
            );
            last_report = Instant::now();
        }
    }
}
