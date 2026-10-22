//! Where things go on the arrangement, in board pixels from its top left.
//! Kept apart from drawing so the math is tested on its own; the GNOME and
//! Mac windows drew the same way.

use zflow::app::layout_model::{Monitor, Seam};

/// How tall the arrangement is, before the shelf under it.
pub const BOARD_HEIGHT: f32 = 240.0;
/// Room around the arrangement: at the sides, above it for the computers'
/// names, and under it.
pub const ROOM_SIDES: f32 = 28.0;
pub const ROOM_ABOVE: f32 = 48.0;
pub const ROOM_BELOW: f32 = 24.0;
/// The most a layout unit takes on the board.
pub const MAX_SCALE: f32 = 0.12;
/// How far an island's outline stands off its screens, and its name off the
/// outline.
pub const HULL: f32 = 8.0;
pub const CHIP_GAP: f32 = 6.0;
/// A drag snaps to an edge within this many pixels.
pub const DRAG_TOLERANCE: f32 = 14.0;
/// An arrow key moves a computer 100 layout units, about an inch, and snaps
/// within a little less, so a step away from a seam is not pulled back.
pub const KEY_STEP: i32 = 100;
pub const KEY_TOLERANCE: u32 = 99;
/// Enter puts a found computer against this one's right edge.
pub const PLACE_TOLERANCE: u32 = 150;
/// The size the engine gives a placed computer's screen until it says its own.
pub const NEW_TILE: (i32, i32) = (1920, 1080);
/// A press that moves less than this is a click.
pub const DRAG_THRESHOLD: f32 = 3.0;

/// A rectangle on the board: x, y, width, height.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    fn covered(&self, other: &Self) -> f32 {
        let width = (self.right().min(other.right()) - self.x.max(other.x)).max(0.0);
        let height = (self.bottom().min(other.bottom()) - self.y.max(other.y)).max(0.0);
        width * height
    }

    /// The smallest rectangle around all of `rects`.
    pub fn around(rects: impl IntoIterator<Item = Self>) -> Option<Self> {
        rects.into_iter().reduce(|a, b| {
            let x = a.x.min(b.x);
            let y = a.y.min(b.y);
            Self::new(
                x,
                y,
                a.right().max(b.right()) - x,
                a.bottom().max(b.bottom()) - y,
            )
        })
    }
}

/// The scale and offset that center a layout on the board.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    pub scale: f32,
    pub left: f32,
    pub top: f32,
    pub offset_x: f32,
    pub offset_y: f32,
}

impl Fit {
    /// Centers `monitors` on a board `width` by `height`, with room above
    /// for the computers' names.
    pub fn new<'a>(
        monitors: impl IntoIterator<Item = &'a Monitor>,
        width: f32,
        height: f32,
    ) -> Option<Self> {
        let mut monitors = monitors.into_iter().peekable();
        monitors.peek()?;
        let (mut left, mut top, mut right, mut bottom) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
        for monitor in monitors {
            left = left.min(i64::from(monitor.x));
            top = top.min(i64::from(monitor.y));
            right = right.max(i64::from(monitor.x) + i64::from(monitor.width));
            bottom = bottom.max(i64::from(monitor.y) + i64::from(monitor.height));
        }
        let (span_x, span_y) = ((right - left).max(1) as f32, (bottom - top).max(1) as f32);
        let scale = ((width - 2.0 * ROOM_SIDES) / span_x)
            .min((height - ROOM_ABOVE - ROOM_BELOW) / span_y)
            .clamp(0.001, MAX_SCALE);
        Some(Self {
            scale,
            left: left as f32,
            top: top as f32,
            offset_x: (width - span_x * scale) / 2.0,
            offset_y: ROOM_ABOVE + (height - ROOM_ABOVE - ROOM_BELOW - span_y * scale) / 2.0,
        })
    }

    /// A monitor on the board, `shift` pixels from its spot. Every screen
    /// rounds its edges alike, so touching edges stay touching.
    pub fn rect(&self, monitor: &Monitor, shift: (f32, f32)) -> Rect {
        let x = |value: f32| (self.offset_x + (value - self.left) * self.scale + shift.0).round();
        let y = |value: f32| (self.offset_y + (value - self.top) * self.scale + shift.1).round();
        let (left, top) = (x(monitor.x as f32), y(monitor.y as f32));
        Rect::new(
            left,
            top,
            (x(monitor.x as f32 + monitor.width as f32) - left).max(1.0),
            (y(monitor.y as f32 + monitor.height as f32) - top).max(1.0),
        )
    }

    /// The layout point under board pixel (`x`, `y`).
    pub fn layout_point(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.left + (x - self.offset_x) / self.scale,
            self.top + (y - self.offset_y) / self.scale,
        )
    }

    /// A seam on the board, short of its ends by `inset`: start and end
    /// points.
    pub fn seam_line(&self, seam: &Seam, inset: f32) -> ((f32, f32), (f32, f32)) {
        let across = if seam.vertical {
            self.offset_x + (seam.at as f32 - self.left) * self.scale
        } else {
            self.offset_y + (seam.at as f32 - self.top) * self.scale
        };
        let along = if seam.vertical {
            self.offset_y - self.top * self.scale
        } else {
            self.offset_x - self.left * self.scale
        };
        let start = along + seam.start as f32 * self.scale + inset;
        let end = start.max(along + seam.end as f32 * self.scale - inset);
        let line = across.round();
        if seam.vertical {
            ((line, start), (line, end))
        } else {
            ((start, line), (end, line))
        }
    }

    /// Board pixels as layout units, rounded.
    pub fn units(&self, pixels: f32) -> i32 {
        (pixels / self.scale).round() as i32
    }
}

/// An island's screens as shown, with room around them.
pub fn hull(shown: impl IntoIterator<Item = Rect>) -> Option<Rect> {
    Rect::around(shown).map(|rect| {
        Rect::new(
            rect.x - HULL,
            rect.y - HULL,
            rect.width + 2.0 * HULL,
            rect.height + 2.0 * HULL,
        )
    })
}

/// Where a computer's name goes: over its outline's top left corner, or
/// another corner, or just inside, wherever it covers the least of `others`.
/// Off the board counts as covering everything. Returns the spot's index,
/// so a drag can keep it.
pub fn chip_spot(hull: Rect, span: f32, height: f32, board: (f32, f32), others: &[Rect]) -> usize {
    chip_spots(hull, span, height, board.0)
        .iter()
        .enumerate()
        .map(|(index, &(x, y))| {
            let cost = if y < 4.0 || y + height > board.1 - 4.0 {
                f32::INFINITY
            } else {
                let chip = Rect::new(x, y, span, height);
                others.iter().map(|other| chip.covered(other)).sum()
            };
            (index, cost)
        })
        .fold((0, f32::INFINITY), |best, next| {
            if next.1 < best.1 { next } else { best }
        })
        .0
}

/// The five places a name may go, as `chip_spot` numbers them.
pub fn chip_spots(hull: Rect, span: f32, height: f32, board_width: f32) -> [(f32, f32); 5] {
    let fit = |x: f32| x.min(board_width - span - 4.0).max(4.0).round();
    let (start, end) = (fit(hull.x), fit(hull.right() - span));
    let above = hull.y - height - CHIP_GAP;
    let below = hull.bottom() + CHIP_GAP;
    let inside = hull.y + HULL + 4.0;
    [
        (start, above),
        (end, above),
        (start, below),
        (end, below),
        (start, inside),
    ]
}

/// How far the pointer went, but with the moving screens kept on the board.
pub fn travel(moving: &[Rect], dx: f32, dy: f32, board: (f32, f32)) -> (f32, f32) {
    let Some(bounds) = Rect::around(moving.iter().copied()) else {
        return (dx, dy);
    };
    let clamp = |value: f32, min: f32, max: f32| value.max(min).min(max.max(min));
    (
        clamp(dx, -bounds.x, board.0 - bounds.right()),
        clamp(dy, -bounds.y, board.1 - bounds.bottom()),
    )
}

/// Where the other computer is from here across `seam`: which edge of this
/// screen, as a crossing row says it.
pub fn crossing_side(seam: &Seam, here_is_a: bool) -> &'static str {
    match (seam.vertical, here_is_a) {
        (true, true) => "Right",
        (true, false) => "Left",
        (false, true) => "Bottom",
        (false, false) => "Top",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(id: &str, x: i32, y: i32, width: u32, height: u32) -> Monitor {
        Monitor {
            id: id.into(),
            label: id.into(),
            peer: None,
            display: None,
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn a_layout_is_centered_below_the_names() {
        let monitors = [
            monitor("a", 0, 0, 1000, 500),
            monitor("b", 1000, 0, 1000, 500),
        ];
        let fit = Fit::new(&monitors, 600.0, BOARD_HEIGHT).unwrap();
        assert!((fit.scale - 0.12).abs() < f32::EPSILON);
        let a = fit.rect(&monitors[0], (0.0, 0.0));
        let b = fit.rect(&monitors[1], (0.0, 0.0));
        assert_eq!(a.right(), b.x, "touching screens stay touching");
        assert_eq!(f32::midpoint(a.x, b.right()), 300.0);
        assert!(a.y >= ROOM_ABOVE);
        let small = Fit::new(&monitors, 200.0, BOARD_HEIGHT).unwrap();
        assert!(small.scale < 0.12);
        assert_eq!(Fit::new(&[], 300.0, 200.0), None);
    }

    #[test]
    fn layout_points_round_trip() {
        let monitors = [monitor("a", -500, 200, 1000, 500)];
        let fit = Fit::new(&monitors, 500.0, BOARD_HEIGHT).unwrap();
        let rect = fit.rect(&monitors[0], (0.0, 0.0));
        let (x, y) = fit.layout_point(rect.x, rect.y);
        assert!((x + 500.0).abs() < 10.0 && (y - 200.0).abs() < 10.0);
    }

    #[test]
    fn seams_sit_on_the_shared_edge() {
        let monitors = [
            monitor("a", 0, 0, 1000, 500),
            monitor("b", 1000, 0, 1000, 500),
        ];
        let fit = Fit::new(&monitors, 600.0, BOARD_HEIGHT).unwrap();
        let seam = Seam {
            a: "a".into(),
            b: "b".into(),
            vertical: true,
            at: 1000,
            start: 0,
            end: 500,
            same_computer: false,
        };
        let ((x0, y0), (x1, y1)) = fit.seam_line(&seam, 3.0);
        let a = fit.rect(&monitors[0], (0.0, 0.0));
        assert_eq!(x0, a.right());
        assert_eq!(x0, x1);
        assert!((y0 - (a.y + 3.0)).abs() < 1.0 && (y1 - (a.bottom() - 3.0)).abs() < 1.0);
    }

    #[test]
    fn names_avoid_other_computers_and_the_edges() {
        let hull = Rect::new(100.0, 10.0, 100.0, 80.0);
        // Above would leave the board, so it goes below.
        assert_eq!(chip_spot(hull, 60.0, 24.0, (400.0, 240.0), &[]), 2);
        let roomy = Rect::new(100.0, 60.0, 100.0, 80.0);
        assert_eq!(chip_spot(roomy, 60.0, 24.0, (400.0, 240.0), &[]), 0);
        let neighbor = Rect::new(80.0, 20.0, 80.0, 30.0);
        assert_eq!(chip_spot(roomy, 60.0, 24.0, (400.0, 240.0), &[neighbor]), 2);
    }

    #[test]
    fn a_drag_stays_on_the_board() {
        let moving = [Rect::new(10.0, 20.0, 50.0, 30.0)];
        assert_eq!(travel(&moving, -50.0, 0.0, (200.0, 100.0)), (-10.0, 0.0));
        assert_eq!(travel(&moving, 500.0, 500.0, (200.0, 100.0)), (140.0, 50.0));
        assert_eq!(travel(&[], 5.0, 5.0, (200.0, 100.0)), (5.0, 5.0));
    }
}
