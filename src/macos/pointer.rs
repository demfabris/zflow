//! Pointer motion and scroll for posting on the Mac: the acceleration curve,
//! keeping the cursor on a display, and wheel lines versus pixels.

use std::{
    str::FromStr,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};

use super::{CursorPosition, DesktopRect};

// libinput's defaults, so motion feels as it does on a Linux receiver.
// Speeds are in device units per millisecond for a 1000 dpi mouse.
const DECELERATE_BELOW: f64 = 0.07;
const POINTS_PER_UNIT: f64 = 1.0;
/// A pause this long forgets the last speed.
const MOTION_TIMEOUT: Duration = Duration::from_millis(300);
const MIN_INTERVAL_MS: f64 = 1.0;
const MAX_INTERVAL_MS: f64 = 50.0;
/// Scroll on the wire counts 120 units to a detent, as Linux does.
const WHEEL_DETENT: i64 = 120;
/// CoreGraphics' default scale from lines to pixels (CGEvent.h), so a detent
/// scrolls as far in pixels as it does as a line.
const PIXELS_PER_LINE: i64 = 10;

/// How motion from a peer is scaled. `speed` is libinput's setting, -1 to 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Profile {
    /// Faster motion moves further, as libinput's adaptive profile does.
    Adaptive { speed: f64 },
    /// Every motion is scaled by `1 + speed`.
    Flat { speed: f64 },
}

impl Default for Profile {
    fn default() -> Self {
        Self::Adaptive { speed: 0.0 }
    }
}

impl FromStr for Profile {
    type Err = anyhow::Error;

    /// Reads `adaptive:<speed>` or `flat:<speed>`.
    fn from_str(value: &str) -> Result<Self> {
        let (kind, speed) = value
            .split_once(':')
            .context("expected adaptive:<speed> or flat:<speed>")?;
        let speed: f64 = speed.parse().context("the speed is not a number")?;
        ensure!(
            (-1.0..=1.0).contains(&speed),
            "the speed must be from -1 to 1"
        );
        match kind {
            "adaptive" => Ok(Self::Adaptive { speed }),
            "flat" => Ok(Self::Flat { speed }),
            _ => bail!("expected adaptive or flat, not {kind}"),
        }
    }
}

/// libinput's linear profile: slower than 1:1 below 0.07 units/ms, 1:1 up
/// to a threshold, then a line up to the maximum factor.
fn adaptive_factor(speed: f64, velocity: f64) -> f64 {
    let threshold = (0.4 - 0.25 * speed).max(0.2);
    let max_factor = 2.0 + 1.5 * speed;
    let incline = 1.1 + 0.75 * speed;
    let factor = if velocity < DECELERATE_BELOW {
        10.0 * velocity + 0.3
    } else if velocity < threshold {
        1.0
    } else {
        incline * (velocity - threshold) + 1.0
    };
    factor.min(max_factor)
}

#[derive(Clone, Copy, Debug)]
struct LastMotion {
    at: Instant,
    velocity: f64,
}

/// Turns motion deltas in device units into whole points to move.
#[derive(Clone, Debug)]
pub struct Acceleration {
    profile: Profile,
    last: Option<LastMotion>,
    remainder: (f64, f64),
}

impl Acceleration {
    pub fn new(profile: Profile) -> Self {
        Self {
            profile,
            last: None,
            remainder: (0.0, 0.0),
        }
    }

    /// Forgets the last speed and the part under a point.
    pub fn reset(&mut self) {
        *self = Self::new(self.profile);
    }

    /// The part under a point carries into the next motion, so slow motion
    /// still adds up.
    pub fn apply(&mut self, dx: i64, dy: i64, now: Instant) -> (i64, i64) {
        if dx == 0 && dy == 0 {
            return (0, 0);
        }
        let factor = self.factor(dx, dy, now);
        let x = dx as f64 * factor * POINTS_PER_UNIT + self.remainder.0;
        let y = dy as f64 * factor * POINTS_PER_UNIT + self.remainder.1;
        self.remainder = (x - x.round(), y - y.round());
        (x.round() as i64, y.round() as i64)
    }

    fn factor(&mut self, dx: i64, dy: i64, now: Instant) -> f64 {
        let speed = match self.profile {
            Profile::Flat { speed } => return 1.0 + speed,
            Profile::Adaptive { speed } => speed,
        };
        // The first motion after a pause counts as spread over the longest
        // interval, so it is not taken for a flick.
        let (interval, previous) = match self.last {
            Some(last) => {
                let elapsed = now.saturating_duration_since(last.at);
                let previous = if elapsed > MOTION_TIMEOUT {
                    0.0
                } else {
                    last.velocity
                };
                let interval = elapsed.as_secs_f64() * 1000.0;
                (interval.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS), previous)
            }
            None => (MAX_INTERVAL_MS, 0.0),
        };
        let velocity = (dx as f64).hypot(dy as f64) / interval;
        self.last = Some(LastMotion { at: now, velocity });
        // Simpson's rule over the last and current speed, as libinput does.
        (adaptive_factor(speed, previous)
            + 4.0 * adaptive_factor(speed, (previous + velocity) / 2.0)
            + adaptive_factor(speed, velocity))
            / 6.0
    }
}

fn contains(display: &DesktopRect, point: CursorPosition) -> bool {
    point.x >= display.x
        && point.x < display.x + display.width
        && point.y >= display.y
        && point.y < display.y + display.height
}

/// Keeps a point on a display: unchanged when it is on one, otherwise moved
/// to the nearest point of the nearest display. macOS does not clamp posted
/// positions itself.
pub fn clamp_to_displays(point: CursorPosition, displays: &[DesktopRect]) -> CursorPosition {
    if displays.iter().any(|display| contains(display, point)) {
        return point;
    }
    displays
        .iter()
        .map(|display| CursorPosition {
            x: point.x.max(display.x).min(display.x + display.width - 1.0),
            y: point.y.max(display.y).min(display.y + display.height - 1.0),
        })
        .min_by(|a, b| {
            let distance = |p: &CursorPosition| (p.x - point.x).hypot(p.y - point.y);
            distance(a).total_cmp(&distance(b))
        })
        .unwrap_or(point)
}

/// A scroll to post, with the wire's signs. macOS accelerates lines as it
/// does a real wheel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    Lines { x: i32, y: i32 },
    Pixels { x: i32, y: i32 },
}

/// Posts whole detents as lines and everything else as pixels, a line's
/// worth per detent.
#[derive(Clone, Debug, Default)]
pub struct ScrollSplit {
    /// Units since the last whole detent on each axis. A high-resolution
    /// wheel stays in pixels until it lines up with a detent again.
    pending: (i64, i64),
    /// The part under a pixel on each axis, in 120ths of a pixel, carried so
    /// small steps still add up.
    remainder: (i64, i64),
}

impl ScrollSplit {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn split(&mut self, x: i64, y: i64) -> Option<Scroll> {
        if x == 0 && y == 0 {
            return None;
        }
        if self.pending == (0, 0) && x % WHEEL_DETENT == 0 && y % WHEEL_DETENT == 0 {
            return Some(Scroll::Lines {
                x: wheel(x / WHEEL_DETENT),
                y: wheel(y / WHEEL_DETENT),
            });
        }
        self.pending = (
            (self.pending.0 + x % WHEEL_DETENT) % WHEEL_DETENT,
            (self.pending.1 + y % WHEEL_DETENT) % WHEEL_DETENT,
        );
        let x = pixels(&mut self.remainder.0, x);
        let y = pixels(&mut self.remainder.1, y);
        (x != 0 || y != 0).then_some(Scroll::Pixels { x, y })
    }
}

/// Whole pixels for `units` of wheel, keeping the part under a pixel.
fn pixels(remainder: &mut i64, units: i64) -> i32 {
    let scaled = units
        .saturating_mul(PIXELS_PER_LINE)
        .saturating_add(*remainder);
    *remainder = scaled % WHEEL_DETENT;
    wheel(scaled / WHEEL_DETENT)
}

/// Scroll event fields are 32-bit; an absurd value is clamped.
fn wheel(value: i64) -> i32 {
    value.clamp(i32::MIN.into(), i32::MAX.into()) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    fn point(x: f64, y: f64) -> CursorPosition {
        CursorPosition { x, y }
    }

    fn display(x: f64, y: f64, width: f64, height: f64) -> DesktopRect {
        DesktopRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn adaptive_factor_follows_libinput() {
        let expected = [
            (0.0, 0.3),
            (0.05, 0.8),
            (0.07, 1.0),
            (0.3, 1.0),
            (0.4, 1.0),
            (1.0, 1.66),
            (5.0, 2.0),
        ];
        let mut last = 0.0;
        for (velocity, factor) in expected {
            let actual = adaptive_factor(0.0, velocity);
            assert!((actual - factor).abs() < 1e-9, "{velocity}: {actual}");
            assert!(actual >= last);
            last = actual;
        }
        assert!((adaptive_factor(1.0, 5.0) - 3.5).abs() < 1e-9);
        assert!((adaptive_factor(-1.0, 5.0) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn steady_fast_motion_reaches_the_maximum() {
        let start = Instant::now();
        let mut acceleration = Acceleration::new(Profile::default());
        acceleration.apply(40, 0, start);
        acceleration.apply(40, 0, at(start, 8));
        assert_eq!(acceleration.apply(40, 0, at(start, 16)), (80, 0));
        assert_eq!(acceleration.apply(0, -40, at(start, 24)), (0, -80));
    }

    #[test]
    fn flat_profile_ignores_speed() {
        let start = Instant::now();
        let mut acceleration = Acceleration::new(Profile::Flat { speed: 0.5 });
        assert_eq!(acceleration.apply(10, -4, start), (15, -6));
        assert_eq!(acceleration.apply(10, -4, at(start, 1)), (15, -6));
        assert_eq!(acceleration.apply(10, -4, at(start, 1000)), (15, -6));
    }

    #[test]
    fn the_part_under_a_point_carries() {
        let start = Instant::now();
        let mut acceleration = Acceleration::new(Profile::Flat { speed: -0.5 });
        let (mut x, mut y) = (0, 0);
        for step in 0..100 {
            let (dx, dy) = acceleration.apply(1, -1, at(start, step * 8));
            x += dx;
            y += dy;
        }
        assert_eq!((x, y), (50, -50));
    }

    #[test]
    fn speed_resets_after_a_pause() {
        let start = Instant::now();
        let fast = |pause: u64| {
            let mut acceleration = Acceleration::new(Profile::default());
            for step in 0..3 {
                acceleration.apply(40, 0, at(start, step * 8));
            }
            acceleration.apply(40, 0, at(start, 16 + pause)).0
        };
        // 40 units over the longest interval is 0.8 units/ms. Kept, the last
        // speed of 5 units/ms nearly doubles the move.
        assert_eq!(fast(290), 76);
        assert_eq!(fast(301), 38);

        let mut acceleration = Acceleration::new(Profile::default());
        assert_eq!(acceleration.apply(40, 0, start), (38, 0));
    }

    #[test]
    fn no_motion_changes_nothing() {
        let start = Instant::now();
        let mut acceleration = Acceleration::new(Profile::default());
        acceleration.apply(40, 0, start);
        acceleration.apply(40, 0, at(start, 8));
        assert_eq!(acceleration.apply(0, 0, at(start, 12)), (0, 0));
        assert_eq!(acceleration.apply(40, 0, at(start, 16)), (80, 0));

        acceleration.reset();
        assert_eq!(acceleration.apply(40, 0, at(start, 24)), (38, 0));
    }

    #[test]
    fn reads_the_profile_setting() {
        assert_eq!(
            "adaptive:0.5".parse::<Profile>().unwrap(),
            Profile::Adaptive { speed: 0.5 }
        );
        assert_eq!(
            "flat:-1".parse::<Profile>().unwrap(),
            Profile::Flat { speed: -1.0 }
        );
        for bad in [
            "adaptive",
            "flat:2",
            "flat:NaN",
            "linear:0",
            "adaptive:fast",
        ] {
            assert!(bad.parse::<Profile>().is_err(), "{bad}");
        }
    }

    #[test]
    fn keeps_the_cursor_on_one_display() {
        let displays = [display(0.0, 0.0, 3008.0, 1692.0)];
        let inside = point(1500.5, 800.0);
        assert_eq!(clamp_to_displays(inside, &displays), inside);
        assert_eq!(
            clamp_to_displays(point(4008.0, 800.0), &displays),
            point(3007.0, 800.0)
        );
        assert_eq!(
            clamp_to_displays(point(3008.0, 1692.0), &displays),
            point(3007.0, 1691.0)
        );
        assert_eq!(
            clamp_to_displays(point(-10.0, -10.0), &displays),
            point(0.0, 0.0)
        );
        assert_eq!(clamp_to_displays(inside, &[]), inside);
    }

    #[test]
    fn snaps_out_of_a_gap_to_the_nearest_display() {
        // A shorter display to the left of a taller one leaves a gap below it.
        let displays = [
            display(0.0, 0.0, 1920.0, 1080.0),
            display(1920.0, 0.0, 2560.0, 1440.0),
        ];
        assert_eq!(
            clamp_to_displays(point(1000.0, 1200.0), &displays),
            point(1000.0, 1079.0)
        );
        assert_eq!(
            clamp_to_displays(point(1900.0, 1300.0), &displays),
            point(1920.0, 1300.0)
        );
        let edge = point(1920.0, 1200.0);
        assert_eq!(clamp_to_displays(edge, &displays), edge);
    }

    #[test]
    fn snaps_out_of_the_empty_corner_of_an_l() {
        let displays = [
            display(0.0, 0.0, 1920.0, 1080.0),
            display(0.0, 1080.0, 1920.0, 1080.0),
            display(1920.0, 1080.0, 1920.0, 1080.0),
        ];
        assert_eq!(
            clamp_to_displays(point(3000.0, 500.0), &displays),
            point(3000.0, 1080.0)
        );
        assert_eq!(
            clamp_to_displays(point(2000.0, 100.0), &displays),
            point(1919.0, 100.0)
        );
    }

    #[test]
    fn whole_detents_scroll_lines() {
        let mut split = ScrollSplit::default();
        assert_eq!(split.split(0, 120), Some(Scroll::Lines { x: 0, y: 1 }));
        assert_eq!(split.split(0, 240), Some(Scroll::Lines { x: 0, y: 2 }));
        assert_eq!(split.split(0, -120), Some(Scroll::Lines { x: 0, y: -1 }));
        assert_eq!(split.split(120, 0), Some(Scroll::Lines { x: 1, y: 0 }));
        assert_eq!(split.split(-240, 0), Some(Scroll::Lines { x: -2, y: 0 }));
        assert_eq!(split.split(0, 0), None);
    }

    #[test]
    fn partial_detents_scroll_a_line_of_pixels_per_detent_until_they_line_up() {
        // A high-resolution Linux wheel sends 15 units a step.
        let mut split = ScrollSplit::default();
        let steps: Vec<_> = (0..8).map(|_| split.split(0, 15)).collect();
        let step = |y| Some(Scroll::Pixels { x: 0, y });
        assert_eq!(steps, [1, 1, 1, 2, 1, 1, 1, 2].map(step));
        assert_eq!(split.split(0, 120), Some(Scroll::Lines { x: 0, y: 1 }));

        assert_eq!(split.split(-60, 0), Some(Scroll::Pixels { x: -5, y: 0 }));
        assert_eq!(split.split(-120, 0), Some(Scroll::Pixels { x: -10, y: 0 }));
        assert_eq!(split.split(-60, 0), Some(Scroll::Pixels { x: -5, y: 0 }));
        assert_eq!(split.split(-120, 0), Some(Scroll::Lines { x: -1, y: 0 }));

        // Steps under a pixel post nothing until they add up, either way.
        assert_eq!(split.split(0, 9), None);
        assert_eq!(split.split(0, 9), step(1));
        assert_eq!(split.split(0, -6), None);
        for _ in 0..9 {
            assert_eq!(split.split(0, 12), step(1));
        }
        assert_eq!(split.split(0, 120), Some(Scroll::Lines { x: 0, y: 1 }));

        split.split(0, 7);
        split.reset();
        assert_eq!(split.split(0, 120), Some(Scroll::Lines { x: 0, y: 1 }));
    }

    #[test]
    fn absurd_scroll_is_clamped() {
        let mut split = ScrollSplit::default();
        assert_eq!(
            split.split(i64::MAX, i64::MIN),
            Some(Scroll::Pixels {
                x: i32::MAX,
                y: i32::MIN
            })
        );
    }
}
