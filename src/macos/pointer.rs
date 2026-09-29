//! Pointer motion and scroll for posting on the Mac: the acceleration curve,
//! keeping the cursor on a display, and wheel units to pixels.

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
/// How far a detent scrolls. Deskflow posts 3 lines a detent, and
/// CoreGraphics counts a line as 10 pixels (CGEvent.h).
const PIXELS_PER_DETENT: i64 = 30;

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

/// Turns wheel units into pixels to post. Each scroll posts how far the
/// rounded running total moved, so the part under a pixel carries, and the
/// distance depends only on the total, not on how playout batched it.
#[derive(Clone, Debug, Default)]
pub struct ScrollScale {
    /// Wheel units since the activation began, on each axis.
    total: (i64, i64),
}

impl ScrollScale {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Pixels to post, with the wire's signs, or `None` while neither axis
    /// has moved a whole pixel.
    pub fn pixels(&mut self, x: i64, y: i64) -> Option<(i32, i32)> {
        let x = advance(&mut self.total.0, x);
        let y = advance(&mut self.total.1, y);
        (x != 0 || y != 0).then_some((x, y))
    }
}

/// Adds `units` to an axis' total and returns the whole pixels it moved.
fn advance(total: &mut i64, units: i64) -> i32 {
    let before = total_pixels(*total);
    *total = total.saturating_add(units);
    wheel(total_pixels(*total) - before)
}

/// Pixels for a total of wheel units, rounded half away from zero so both
/// directions scroll alike.
fn total_pixels(units: i64) -> i64 {
    let scaled = units.saturating_mul(PIXELS_PER_DETENT);
    scaled.saturating_add(scaled.signum() * (WHEEL_DETENT / 2)) / WHEEL_DETENT
}

/// Scroll event fields are 32-bit; an absurd value is clamped.
fn wheel(value: i64) -> i32 {
    value.clamp(i32::MIN.into(), i32::MAX.into()) as i32
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

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

    /// Every pixel a stream of scrolls posts, summed on each axis.
    fn scrolled(steps: impl IntoIterator<Item = (i64, i64)>) -> (i64, i64) {
        let mut scale = ScrollScale::default();
        steps
            .into_iter()
            .filter_map(|(x, y)| scale.pixels(x, y))
            .fold((0, 0), |(sum_x, sum_y), (x, y)| {
                (sum_x + i64::from(x), sum_y + i64::from(y))
            })
    }

    #[test]
    fn a_detent_scrolls_30_pixels() {
        let mut scale = ScrollScale::default();
        assert_eq!(scale.pixels(0, 120), Some((0, 30)));
        assert_eq!(scale.pixels(0, -240), Some((0, -60)));
        assert_eq!(scale.pixels(120, 0), Some((30, 0)));
        assert_eq!(scale.pixels(-120, 360), Some((-30, 90)));
        assert_eq!(scale.pixels(0, 0), None);
    }

    #[test]
    fn distance_does_not_depend_on_how_scroll_was_batched() {
        // Ten detents down and three right: one detent at a time, bunched as
        // playout sends them when they come fast, cut at odd places by
        // catch-up, and with the axes interleaved and partly undone.
        let slow: Vec<_> = [(0, 120); 10].into_iter().chain([(120, 0); 3]).collect();
        let fast = vec![(0, 600), (360, 600)];
        let cut = vec![(0, 67), (0, 53), (7, 473), (0, 7), (353, 600)];
        let mixed = vec![(15, 1), (0, 119), (-15, 240), (361, -7), (-1, 847)];
        for stream in [slow, fast, cut, mixed] {
            assert_eq!(scrolled(stream.clone()), (90, 300), "{stream:?}");
            let reversed = stream.iter().map(|&(x, y)| (-x, -y));
            assert_eq!(scrolled(reversed), (-90, -300), "{stream:?}");
        }
    }

    #[test]
    fn high_resolution_steps_add_up_like_detents() {
        // A high-resolution Linux wheel sends 15 units a step, 3.75 pixels.
        assert_eq!(scrolled([(0, 15); 80]), scrolled([(0, 120); 10]));
        assert_eq!(scrolled([(-15, 0); 80]), (-300, 0));
        let mut scale = ScrollScale::default();
        let steps: Vec<_> = (0..8).map(|_| scale.pixels(0, 15)).collect();
        let step = |y| Some((0, y));
        assert_eq!(steps, [4, 4, 3, 4, 4, 4, 3, 4].map(step));

        // Steps under a pixel post nothing until they add up, either way.
        let mut scale = ScrollScale::default();
        assert_eq!(scale.pixels(0, 1), None);
        assert_eq!(scale.pixels(0, 1), step(1));
        assert_eq!(scale.pixels(0, -2), step(-1));
        assert_eq!(scale.pixels(0, -1), None);

        scale.pixels(0, 7);
        scale.reset();
        assert_eq!(scale.pixels(0, 1), None);
    }

    proptest! {
        /// However a stream is cut up, it scrolls as far as its total at
        /// once, and turned around it scrolls exactly as far back.
        #[test]
        fn any_stream_scrolls_as_far_as_its_total(
            steps in prop::collection::vec((-600_i64..=600, -600_i64..=600), 0..64),
        ) {
            let total = steps
                .iter()
                .fold((0, 0), |(x, y), &(dx, dy)| (x + dx, y + dy));
            let distance = scrolled(steps.iter().copied());
            prop_assert_eq!(distance, scrolled([total]));
            let reversed = steps.iter().map(|&(x, y)| (-x, -y));
            prop_assert_eq!(scrolled(reversed), (-distance.0, -distance.1));
        }
    }

    #[test]
    fn absurd_scroll_is_clamped() {
        let mut scale = ScrollScale::default();
        assert_eq!(scale.pixels(i64::MAX, i64::MIN), Some((i32::MAX, i32::MIN)));
    }
}
