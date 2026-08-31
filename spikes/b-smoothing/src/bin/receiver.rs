//! Receives cumulative motion frames and injects them through a uinput
//! virtual mouse in one of three playout modes. Usage:
//!
//!   receiver [--port 5555] [--mode raw|fixed|adaptive] [--delay-ms 8] [--log FILE]
//!
//! --log appends one "t_us,dx,dy" line per injection for offline analysis.
//!
//! Needs write access to /dev/uinput (root, or a udev rule).
//! Switch modes at runtime by typing on stdin (works over ssh):
//!
//!   raw          apply every frame the instant it arrives
//!   fixed        fixed playout delay, full catch-up jump after a burst
//!   adaptive     adaptive delay (p95 jitter + margin) with capped catch-up
//!   delay <ms>   set the fixed-mode delay
//!   stats        print counters now
//!
//! A mode switch resets playout state (queue and target), so expect the
//! remote cursor to sit still for an instant right after switching.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::net::UdpSocket;
use std::process::exit;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AttributeSet, EventType, InputEvent, Key, RelativeAxisType};
use spike_b_smoothing::{MotionFrame, FRAME_LEN};

const MIN_WINDOW_US: i64 = 3_000_000; // sliding window for clock offset + jitter
const ADAPT_MARGIN_US: i64 = 2_000;
const ADAPT_MIN_US: i64 = 2_000;
const ADAPT_MAX_US: i64 = 80_000;
const TICK: Duration = Duration::from_millis(1);

// catch-up slew: drain backlog at CATCHUP_SPEEDUP times the sender's real
// motion velocity (measured from frame capture timestamps, so capping the
// output can never inflate the estimate and feed back). Bounded per tick so
// a stale estimate can neither freeze nor teleport the cursor.
const CATCHUP_SPEEDUP: f64 = 3.0;
const STEP_FLOOR: f64 = 5.0; // px per tick
const STEP_CEIL: f64 = 60.0; // px per tick

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Raw,
    Fixed,
    Adaptive,
}

struct RecvFrame {
    t_capture_us: i64,
    total_dx: i64,
    total_dy: i64,
}

struct Shared {
    mode: Mode,
    fixed_delay_us: i64,
    session: Option<u64>,
    queue: VecDeque<RecvFrame>,
    highest_seq: Option<u64>,
    last_totals: Option<(i64, i64)>,
    // latest matured totals; playout slews last_totals toward this each tick
    target_totals: Option<(i64, i64)>,
    // previous matured frame (totals + capture time) for velocity estimation
    last_matured: Option<(i64, i64, i64)>,
    // sender motion velocity in px/ms, from capture-time deltas
    vel_ewma: f64,
    recv_count: u64,
    gap_count: u64,
    // (t_recv_us, owd_us): owd carries the unknown clock offset; the sliding
    // minimum estimates that offset plus base one-way delay.
    owd_window: VecDeque<(i64, i64)>,
    // monotonic deque over owd_window for O(1) sliding minimum
    owd_min: VecDeque<(i64, i64)>,
}

impl Shared {
    fn offset_min(&self) -> Option<i64> {
        self.owd_min.front().map(|&(_, v)| v)
    }

    fn push_owd(&mut self, t_recv: i64, owd: i64) {
        self.owd_window.push_back((t_recv, owd));
        while self
            .owd_window
            .front()
            .map_or(false, |&(t, _)| t_recv - t > MIN_WINDOW_US)
        {
            self.owd_window.pop_front();
        }
        while self.owd_min.back().map_or(false, |&(_, v)| v >= owd) {
            self.owd_min.pop_back();
        }
        self.owd_min.push_back((t_recv, owd));
        while self
            .owd_min
            .front()
            .map_or(false, |&(t, _)| t_recv - t > MIN_WINDOW_US)
        {
            self.owd_min.pop_front();
        }
    }

    fn jitter_p95(&self) -> Option<i64> {
        let min = self.offset_min()?;
        if self.owd_window.len() < 20 {
            return None;
        }
        let mut v: Vec<i64> = self.owd_window.iter().map(|&(_, o)| o - min).collect();
        v.sort_unstable();
        // nearest-rank p95, not the max at small counts
        let idx = ((v.len() * 95).div_ceil(100)).saturating_sub(1);
        Some(v[idx])
    }

    /// New sender run (or first frame ever): drop everything derived from the
    /// old session and baseline totals so history is never replayed.
    fn reset_session(&mut self, f: &MotionFrame) {
        self.session = Some(f.session);
        self.queue.clear();
        self.highest_seq = Some(f.seq);
        self.last_totals = Some((f.total_dx, f.total_dy));
        self.target_totals = None;
        self.last_matured = None;
        self.vel_ewma = 0.0;
        self.owd_window.clear();
        self.owd_min.clear();
    }

    fn reset_playout(&mut self) {
        self.queue.clear();
        self.target_totals = None;
        self.last_matured = None;
        self.vel_ewma = 0.0;
    }
}

fn now_us(start: Instant) -> i64 {
    start.elapsed().as_micros() as i64
}

struct InjectLog(Option<Mutex<std::fs::File>>);

impl InjectLog {
    fn write(&self, t_us: i64, dx: i64, dy: i64) {
        if let Some(f) = &self.0 {
            let _ = writeln!(f.lock().unwrap(), "{t_us},{dx},{dy}");
        }
    }
}

#[must_use]
fn inject(vdev: &mut VirtualDevice, dx: i64, dy: i64) -> bool {
    if dx == 0 && dy == 0 {
        return true;
    }
    let events = [
        InputEvent::new(
            EventType::RELATIVE,
            RelativeAxisType::REL_X.0,
            dx.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        ),
        InputEvent::new(
            EventType::RELATIVE,
            RelativeAxisType::REL_Y.0,
            dy.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        ),
    ];
    match vdev.emit(&events) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("inject failed: {e}");
            false
        }
    }
}

/// Injection failed: walk the logical position back so a later cumulative
/// frame repairs the lost displacement.
fn roll_back(shared: &Mutex<Shared>, dx: i64, dy: i64) {
    let mut s = shared.lock().unwrap();
    if let Some((ax, ay)) = s.last_totals {
        s.last_totals = Some((ax - dx, ay - dy));
    }
}

fn main() -> std::io::Result<()> {
    let mut port: u16 = 5555;
    let mut mode = Mode::Adaptive;
    let mut delay_ms: i64 = 8;
    let mut log_path: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.next().and_then(|v| v.parse().ok()).unwrap_or(5555),
            "--log" => log_path = args.next(),
            "--mode" => {
                mode = match args.next().as_deref() {
                    Some("raw") => Mode::Raw,
                    Some("fixed") => Mode::Fixed,
                    Some("adaptive") => Mode::Adaptive,
                    _ => {
                        eprintln!("mode must be raw|fixed|adaptive");
                        exit(2)
                    }
                }
            }
            "--delay-ms" => delay_ms = args.next().and_then(|v| v.parse().ok()).unwrap_or(8),
            _ => {
                eprintln!("usage: receiver [--port N] [--mode raw|fixed|adaptive] [--delay-ms N]");
                exit(2)
            }
        }
    }

    let mut axes = AttributeSet::<RelativeAxisType>::new();
    axes.insert(RelativeAxisType::REL_X);
    axes.insert(RelativeAxisType::REL_Y);
    let mut keys = AttributeSet::<Key>::new();
    keys.insert(Key::BTN_LEFT);
    keys.insert(Key::BTN_RIGHT);
    let vdev = VirtualDeviceBuilder::new()?
        .name("zflow-spike-mouse")
        .with_relative_axes(&axes)?
        .with_keys(&keys)?
        .build()
        .map_err(|e| {
            eprintln!("cannot create uinput device (need write access to /dev/uinput): {e}");
            e
        })?;
    let vdev = Arc::new(Mutex::new(vdev));

    let ilog = Arc::new(InjectLog(match &log_path {
        Some(p) => Some(Mutex::new(
            std::fs::OpenOptions::new().create(true).append(true).open(p)?,
        )),
        None => None,
    }));

    let sock = UdpSocket::bind(("0.0.0.0", port))?;
    eprintln!("listening on {port}, mode {mode:?}, fixed delay {delay_ms} ms");
    eprintln!("stdin commands: raw | fixed | adaptive | delay <ms> | stats");

    let start = Instant::now();
    let shared = Arc::new(Mutex::new(Shared {
        mode,
        fixed_delay_us: delay_ms * 1000,
        session: None,
        queue: VecDeque::new(),
        highest_seq: None,
        last_totals: None,
        target_totals: None,
        last_matured: None,
        vel_ewma: 0.0,
        recv_count: 0,
        gap_count: 0,
        owd_window: VecDeque::new(),
        owd_min: VecDeque::new(),
    }));

    // stdin control
    {
        let shared = shared.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines().map_while(Result::ok) {
                let mut s = shared.lock().unwrap();
                let words: Vec<&str> = line.split_whitespace().collect();
                let new_mode = match words.as_slice() {
                    ["raw"] => Some(Mode::Raw),
                    ["fixed"] => Some(Mode::Fixed),
                    ["adaptive"] => Some(Mode::Adaptive),
                    ["delay", ms] => {
                        if let Ok(v) = ms.parse::<i64>() {
                            s.fixed_delay_us = v.clamp(0, 500) * 1000;
                        }
                        None
                    }
                    ["stats"] => None,
                    _ => {
                        eprintln!("? commands: raw | fixed | adaptive | delay <ms> | stats");
                        continue;
                    }
                };
                if let Some(m) = new_mode {
                    if m != s.mode {
                        s.mode = m;
                        // stale queued totals from the old mode would replay
                        // as backwards motion; drop them
                        s.reset_playout();
                    }
                }
                eprintln!(
                    "mode {:?}, fixed delay {} ms, recv {}, gaps {}, queued {}, offset_min {:?} us, jitter_p95 {:?} us",
                    s.mode,
                    s.fixed_delay_us / 1000,
                    s.recv_count,
                    s.gap_count,
                    s.queue.len(),
                    s.offset_min(),
                    s.jitter_p95()
                );
            }
        });
    }

    // receive thread
    {
        let shared = shared.clone();
        let vdev = vdev.clone();
        let ilog = ilog.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            loop {
                let n = match sock.recv(&mut buf) {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("recv failed: {e}");
                        continue;
                    }
                };
                if n < FRAME_LEN {
                    continue;
                }
                let Some(f) = MotionFrame::decode(&buf[..n]) else {
                    continue;
                };
                if f.t_capture_us > (1 << 62) {
                    continue; // garbage timestamp would overflow owd math
                }
                let t_recv = now_us(start);
                let mut s = shared.lock().unwrap();
                s.recv_count += 1;

                match s.session {
                    Some(id) if id == f.session => match s.highest_seq {
                        Some(h) if f.seq <= h => continue, // stale or duplicate
                        Some(h) => {
                            if f.seq > h + 1 {
                                s.gap_count += f.seq - h - 1;
                            }
                            s.highest_seq = Some(f.seq);
                        }
                        None => s.highest_seq = Some(f.seq),
                    },
                    Some(_) => {
                        eprintln!("sender restart detected, resetting state");
                        s.reset_session(&f);
                        continue; // baseline frame carries no motion to apply
                    }
                    None => {
                        s.reset_session(&f);
                        continue;
                    }
                }
                s.push_owd(t_recv, t_recv - f.t_capture_us as i64);

                if s.mode == Mode::Raw {
                    let (lx, ly) = s.last_totals.unwrap();
                    let (dx, dy) = (f.total_dx - lx, f.total_dy - ly);
                    s.last_totals = Some((f.total_dx, f.total_dy));
                    drop(s);
                    if inject(&mut vdev.lock().unwrap(), dx, dy) {
                        ilog.write(now_us(start), dx, dy);
                    } else {
                        roll_back(&shared, dx, dy);
                    }
                } else {
                    s.queue.push_back(RecvFrame {
                        t_capture_us: f.t_capture_us as i64,
                        total_dx: f.total_dx,
                        total_dy: f.total_dy,
                    });
                    if s.queue.len() > 10_000 {
                        s.queue.drain(..5_000);
                    }
                }
            }
        });
    }

    // playout loop
    let mut cached_adaptive_us: i64 = ADAPT_MIN_US;
    let mut last_adapt = Instant::now();
    let mut last_report = Instant::now();
    loop {
        std::thread::sleep(TICK);
        let mut s = shared.lock().unwrap();

        if last_report.elapsed() >= Duration::from_secs(5) {
            eprintln!(
                "mode {:?}, recv {}, gaps {}, queued {}, delay {} ms",
                s.mode,
                s.recv_count,
                s.gap_count,
                s.queue.len(),
                match s.mode {
                    Mode::Raw => 0,
                    Mode::Fixed => s.fixed_delay_us / 1000,
                    Mode::Adaptive => cached_adaptive_us / 1000,
                }
            );
            last_report = Instant::now();
        }

        if s.mode == Mode::Raw {
            continue;
        }
        let Some(offset) = s.offset_min() else { continue };

        if last_adapt.elapsed() >= Duration::from_millis(100) {
            cached_adaptive_us = match s.jitter_p95() {
                Some(p95) => (p95 + ADAPT_MARGIN_US).clamp(ADAPT_MIN_US, ADAPT_MAX_US),
                // sparse or idle: decay toward the floor instead of holding
                // a stale spike-era delay forever
                None => (cached_adaptive_us * 3 / 4).max(ADAPT_MIN_US),
            };
            last_adapt = Instant::now();
        }
        let delay = match s.mode {
            Mode::Fixed => s.fixed_delay_us,
            _ => cached_adaptive_us,
        };

        // a frame is mature when capture time + offset + delay has passed
        let deadline = now_us(start) - offset - delay;
        while s.queue.front().map_or(false, |f| f.t_capture_us <= deadline) {
            let f = s.queue.pop_front().unwrap();
            if let Some((px, py, pt)) = s.last_matured {
                let dt_ms = (f.t_capture_us - pt) as f64 / 1000.0;
                if dt_ms > 0.0 {
                    let v = ((f.total_dx - px) as f64).hypot((f.total_dy - py) as f64) / dt_ms;
                    s.vel_ewma = s.vel_ewma * 0.9 + v * 0.1;
                }
            }
            s.last_matured = Some((f.total_dx, f.total_dy, f.t_capture_us));
            s.target_totals = Some((f.total_dx, f.total_dy));
        }
        let Some((tx, ty)) = s.target_totals else { continue };
        let Some((lx, ly)) = s.last_totals else {
            s.last_totals = Some((tx, ty));
            continue;
        };
        let (mut dx, mut dy) = (tx - lx, ty - ly);
        if dx == 0 && dy == 0 {
            continue;
        }

        if s.mode == Mode::Adaptive {
            let mag = (dx as f64).hypot(dy as f64);
            let step = (CATCHUP_SPEEDUP * s.vel_ewma).clamp(STEP_FLOOR, STEP_CEIL);
            if mag > step {
                let scale = step / mag;
                dx = (dx as f64 * scale).round() as i64;
                dy = (dy as f64 * scale).round() as i64;
            }
        }

        s.last_totals = Some((lx + dx, ly + dy));
        drop(s);
        if inject(&mut vdev.lock().unwrap(), dx, dy) {
            ilog.write(now_us(start), dx, dy);
        } else {
            roll_back(&shared, dx, dy);
        }
    }
}
