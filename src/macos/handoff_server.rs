//! Answers a controlling peer's desktop handoff, as the GNOME extension does
//! on Linux. Prepare puts the cursor where the pointer enters, Poll says when
//! it leaves again through the same edge, and Finish or a lapsed lease ends
//! the handoff.

use std::{
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use anyhow::Result;
use tokio::sync::Notify;

use super::{
    CursorPosition,
    receive::{LeaseGuard, Ownership},
};
use crate::desktop::{
    DesktopRequest, DesktopResponse, Edge, FRACTION_MAX, Geometry, LEASE_MS, POLL_HOLD_MS, Point,
    Rect,
};

const LEASE: Duration = Duration::from_millis(LEASE_MS);
const POLL_HOLD: Duration = Duration::from_millis(POLL_HOLD_MS);
/// How long Prepare waits for the cursor to reach the entry point.
const ENTRY_WAIT: Duration = Duration::from_millis(100);
/// How far from the entry point the cursor may land.
const ENTRY_SLOP: f64 = 2.0;
/// How often a waiting request looks at the cursor.
const SAMPLE: Duration = Duration::from_millis(8);
/// How often a held lease is checked, as the GNOME extension does.
const LEASE_CHECK: Duration = Duration::from_millis(250);
/// The entry display must be at least this big both ways.
const MIN_DISPLAY: u32 = 8;
// The Linux daemon's words for the same answers.
const ENDED: &str = "Desktop handoff expired or ended";
const STALE: &str = "Desktop handoff token is stale";
const NO_DISPLAY: &str = "No Mac display touches the selected crossing range";

/// This Mac as the handoff server sees it. Tests use a fake.
pub(crate) trait Desk: Send + Sync + 'static {
    fn geometry(&self) -> Result<Geometry>;
    /// Changes whenever macOS reconfigures a display.
    fn generation(&self) -> u32;
    fn cursor(&self) -> Result<CursorPosition>;
    /// Moves the cursor with a posted move. A warp would hold off the Mac's
    /// own input for a quarter second.
    fn move_to(&self, point: Point);
    /// Lets go of whatever the peer holds on this Mac.
    fn release_all(&self);
    /// Wakes the display, as local input would.
    fn wake(&self);
    /// Ends a session whose handoff lapsed.
    fn close(&self, peer: &str, session_id: u64);
    /// Whether `session_id` is still `peer`'s open session.
    fn current(&self, peer: &str, session_id: u64) -> bool;
}

/// A stretch of an edge with a display behind it, in points along the edge.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Segment {
    start: i32,
    end: i32,
    display: Rect,
}

/// One outer edge of this desktop, cut to a handoff's range: where the
/// pointer enters, and where leaving through it hands the pointer back.
#[derive(Clone, Debug, PartialEq)]
struct ArmedEdge {
    edge: Edge,
    start: u32,
    end: u32,
    /// The edge's line: an x for Left and Right, a y for Top and Bottom.
    boundary: i32,
    /// Where the desktop starts along the edge, and how long the edge is.
    origin: i32,
    span: u32,
    segments: Vec<Segment>,
}

impl ArmedEdge {
    fn new(geometry: &Geometry, edge: Edge, start: u32, end: u32) -> Result<Self> {
        let bounds = geometry.bounds()?;
        let vertical = matches!(edge, Edge::Left | Edge::Right);
        let (origin, span) = if vertical {
            (bounds.y, bounds.height)
        } else {
            (bounds.x, bounds.width)
        };
        let boundary = side(&bounds, edge);
        // The range's first and last pixels round inward.
        let scaled = |fraction: u32| u64::from(fraction) * u64::from(span);
        let range_start = origin + scaled(start).div_ceil(u64::from(FRACTION_MAX)) as i32;
        let range_end = origin + (scaled(end) / u64::from(FRACTION_MAX)) as i32;
        let segments = geometry
            .monitors
            .iter()
            .filter(|display| side(display, edge) == boundary)
            .map(|display| {
                let (low, length) = if vertical {
                    (display.y, display.height)
                } else {
                    (display.x, display.width)
                };
                Segment {
                    start: low.max(range_start),
                    end: (low + length as i32).min(range_end),
                    display: *display,
                }
            })
            .filter(|segment| segment.end > segment.start)
            .collect();
        Ok(Self {
            edge,
            start,
            end,
            boundary,
            origin,
            span,
            segments,
        })
    }

    /// Where the pointer enters at `position`: three points in from the
    /// edge, at the pixel nearest the wanted one that has a display behind
    /// it. The peer's tile for this Mac is its bounding box, so the wanted
    /// pixel can fall where no display touches the edge.
    fn entry(&self, position: u32) -> Result<Point, &'static str> {
        let wanted = self.origin
            + (u64::from(position) * u64::from(self.span) / u64::from(FRACTION_MAX)) as i32;
        let along = |segment: &Segment| wanted.clamp(segment.start, segment.end - 1);
        let segment = self
            .segments
            .iter()
            .min_by_key(|segment| along(segment).abs_diff(wanted))
            .ok_or(NO_DISPLAY)?;
        if segment.display.width < MIN_DISPLAY || segment.display.height < MIN_DISPLAY {
            return Err("The entry display is too small");
        }
        let along = along(segment);
        Ok(match self.edge {
            Edge::Left => Point {
                x: self.boundary + 3,
                y: along,
            },
            Edge::Right => Point {
                x: self.boundary - 4,
                y: along,
            },
            Edge::Top => Point {
                x: along,
                y: self.boundary + 3,
            },
            Edge::Bottom => Point {
                x: along,
                y: self.boundary - 4,
            },
        })
    }

    /// Where the cursor sits on the edge's outermost pixels, if it does.
    /// That is where the clamp leaves a pointer the peer pushes out, and
    /// where the Mac's own trackpad hands the pointer back.
    fn at_edge(&self, cursor: CursorPosition) -> Option<u32> {
        let line = f64::from(self.boundary);
        let (across, along) = self.axes(cursor);
        let on_edge = match self.edge {
            Edge::Left | Edge::Top => across < line + 1.0,
            Edge::Right | Edge::Bottom => across >= line - 1.0,
        };
        on_edge.then(|| self.position(along)).flatten()
    }

    /// Where a move from `from` to `to`, before the clamp to the displays,
    /// leaves through the edge, if it does.
    fn crossing(&self, from: CursorPosition, to: CursorPosition) -> Option<u32> {
        let line = f64::from(self.boundary);
        let ((from_across, from_along), (to_across, to_along)) = (self.axes(from), self.axes(to));
        let leaves = match self.edge {
            Edge::Left | Edge::Top => from_across >= line && to_across < line,
            Edge::Right | Edge::Bottom => from_across < line && to_across >= line,
        };
        if !leaves {
            return None;
        }
        let travelled = (line - from_across) / (to_across - from_across);
        self.position(from_along + travelled * (to_along - from_along))
    }

    /// A point's distance across the edge and along it.
    fn axes(&self, point: CursorPosition) -> (f64, f64) {
        match self.edge {
            Edge::Left | Edge::Right => (point.x, point.y),
            Edge::Top | Edge::Bottom => (point.y, point.x),
        }
    }

    /// The return position for a pointer leaving at `along`, or None where
    /// no display in the range touches the edge.
    fn position(&self, along: f64) -> Option<u32> {
        let pixel = along.floor();
        self.segments
            .iter()
            .any(|segment| f64::from(segment.start) <= pixel && pixel < f64::from(segment.end))
            .then(|| {
                let fraction = (along - f64::from(self.origin)) * f64::from(FRACTION_MAX)
                    / f64::from(self.span);
                fraction
                    .round()
                    .clamp(f64::from(self.start), f64::from(self.end)) as u32
            })
    }
}

/// Where `rect`'s side along `edge` is.
fn side(rect: &Rect, edge: Edge) -> i32 {
    match edge {
        Edge::Left => rect.x,
        Edge::Right => rect.x + rect.width as i32,
        Edge::Top => rect.y,
        Edge::Bottom => rect.y + rect.height as i32,
    }
}

/// The cursor as a pixel on a display, so the answer passes validation even
/// when macOS reports it a fraction past an edge.
fn on_display(geometry: &Geometry, cursor: CursorPosition) -> Point {
    let point = Point {
        x: cursor.x.floor() as i32,
        y: cursor.y.floor() as i32,
    };
    geometry
        .monitors
        .iter()
        .map(|display| Point {
            x: point
                .x
                .clamp(display.x, display.x + display.width as i32 - 1),
            y: point
                .y
                .clamp(display.y, display.y + display.height as i32 - 1),
        })
        .min_by_key(|near| near.x.abs_diff(point.x) + near.y.abs_diff(point.y))
        .unwrap_or(point)
}

struct Lease {
    peer: String,
    session_id: u64,
    token: u64,
    renewed: Instant,
    /// The display generation the entry was placed in.
    generation: u32,
    edge: ArmedEdge,
    /// Where the pointer left, once it has.
    returned: Option<u32>,
    _guard: LeaseGuard,
}

impl Lease {
    fn permits(&self, peer: &str, session_id: u64, token: u64) -> bool {
        self.peer == peer && self.session_id == session_id && self.token == token
    }
}

/// Answers desktop requests from the peers that may control this Mac. One
/// handoff at a time holds the Mac, through its `Ownership`.
pub(crate) struct HandoffServer<D> {
    desk: D,
    ownership: Ownership,
    lease: Mutex<Option<Lease>>,
    /// Wakes a held Poll when the pointer returns or the handoff ends, and
    /// `watch` when a handoff starts.
    changed: Notify,
}

impl<D: Desk> HandoffServer<D> {
    pub fn new(desk: D, ownership: Ownership) -> Self {
        Self {
            desk,
            ownership,
            lease: Mutex::new(None),
            changed: Notify::new(),
        }
    }

    fn lease(&self) -> MutexGuard<'_, Option<Lease>> {
        self.lease.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Answers one request from `peer`'s session. Whether that peer may
    /// control this Mac at all is the caller's to check first.
    pub async fn request(
        &self,
        peer: &str,
        session_id: u64,
        request: DesktopRequest,
    ) -> DesktopResponse {
        if let Err(error) = request.validate() {
            return DesktopResponse::unavailable(error.to_string());
        }
        self.expire_at(Instant::now());
        if let Err(reason) = self.ownership.desktop_allowed(peer, session_id) {
            return DesktopResponse::unavailable(reason);
        }
        match request {
            DesktopRequest::Snapshot => self.snapshot(),
            request @ DesktopRequest::Prepare { .. } => {
                self.prepare(peer, session_id, request).await
            }
            DesktopRequest::Poll { token } => self.poll(peer, session_id, token).await,
            DesktopRequest::Finish { token } => self.finish(peer, session_id, token),
        }
    }

    fn snapshot(&self) -> DesktopResponse {
        let desktop = self
            .desk
            .geometry()
            .and_then(|geometry| Ok((self.desk.cursor()?, geometry)));
        match desktop {
            Ok((cursor, geometry)) => DesktopResponse::Snapshot {
                position: on_display(&geometry, cursor),
                geometry,
            },
            Err(error) => DesktopResponse::unavailable(format!("{error:#}")),
        }
    }

    /// Holds this Mac for the handoff and puts the cursor where the pointer
    /// enters. The hold lasts until Finish, or until Poll stops renewing it.
    async fn prepare(
        &self,
        peer: &str,
        session_id: u64,
        request: DesktopRequest,
    ) -> DesktopResponse {
        let DesktopRequest::Prepare {
            token,
            edge,
            start,
            end,
            position,
        } = request
        else {
            return DesktopResponse::unavailable("Not a desktop handoff");
        };
        // Read first, so a display change while this runs ends the handoff.
        let generation = self.desk.generation();
        let planned = self.desk.geometry().and_then(|geometry| {
            let armed = ArmedEdge::new(&geometry, edge, start, end)?;
            Ok((geometry, armed))
        });
        let (geometry, armed) = match planned {
            Ok(planned) => planned,
            Err(error) => return DesktopResponse::unavailable(format!("{error:#}")),
        };
        let entry = match armed.entry(position) {
            Ok(entry) => entry,
            Err(reason) => return DesktopResponse::unavailable(reason),
        };
        {
            let mut lease = self.lease();
            // A request can outlive its session, whose close ended any
            // handoff before this one could start.
            if !self.desk.current(peer, session_id) {
                return DesktopResponse::unavailable(ENDED);
            }
            let guard = match self.ownership.begin_lease(peer, session_id) {
                Ok(guard) => guard,
                Err(reason) => return DesktopResponse::unavailable(reason),
            };
            *lease = Some(Lease {
                peer: peer.to_owned(),
                session_id,
                token,
                renewed: Instant::now(),
                generation,
                edge: armed,
                returned: None,
                _guard: guard,
            });
        }
        self.changed.notify_waiters();
        self.desk.wake();
        self.desk.move_to(entry);
        let deadline = Instant::now() + ENTRY_WAIT;
        loop {
            if !self.holds(session_id, token) {
                return DesktopResponse::unavailable("The Mac desktop changed during entry");
            }
            if let Ok(cursor) = self.desk.cursor()
                && (cursor.x - f64::from(entry.x)).abs() <= ENTRY_SLOP
                && (cursor.y - f64::from(entry.y)).abs() <= ENTRY_SLOP
            {
                tracing::info!(%peer, session_id, x = entry.x, y = entry.y, "desktop handoff prepared");
                return DesktopResponse::Prepared {
                    position: on_display(&geometry, cursor),
                    geometry,
                };
            }
            if Instant::now() >= deadline {
                self.end(session_id, token);
                return DesktopResponse::unavailable(
                    "The Mac did not place the cursor at the requested entry",
                );
            }
            tokio::time::sleep(SAMPLE).await;
        }
    }

    /// Renews the handoff, then answers where the pointer left, or Active
    /// once the hold runs out. Meanwhile it watches the cursor, so the Mac's
    /// own trackpad can hand the pointer back too.
    async fn poll(&self, peer: &str, session_id: u64, token: u64) -> DesktopResponse {
        {
            let mut lease = self.lease();
            let Some(active) = lease.as_mut() else {
                return DesktopResponse::unavailable(ENDED);
            };
            if !active.permits(peer, session_id, token) {
                return DesktopResponse::unavailable(STALE);
            }
            active.renewed = Instant::now();
        }
        let hold = tokio::time::Instant::now() + POLL_HOLD;
        loop {
            let changed = self.changed.notified();
            if let Ok(cursor) = self.desk.cursor() {
                self.sample(cursor);
            }
            match self.returned(session_id, token) {
                Err(reason) => return DesktopResponse::unavailable(reason),
                Ok(Some(position)) => return DesktopResponse::Returned { position },
                Ok(None) => {}
            }
            let now = tokio::time::Instant::now();
            if now >= hold {
                return DesktopResponse::Active;
            }
            tokio::select! {
                () = changed => {}
                () = tokio::time::sleep_until((now + SAMPLE).min(hold)) => {}
            }
        }
    }

    /// Ends the handoff and lets go of whatever the peer still holds.
    fn finish(&self, peer: &str, session_id: u64, token: u64) -> DesktopResponse {
        let ended = {
            let mut lease = self.lease();
            match lease.as_ref() {
                None => return DesktopResponse::unavailable(ENDED),
                Some(active) if !active.permits(peer, session_id, token) => {
                    return DesktopResponse::unavailable(STALE);
                }
                Some(_) => lease.take(),
            }
        };
        drop(ended);
        self.changed.notify_waiters();
        self.desk.release_all();
        DesktopResponse::Finished
    }

    /// Checks one of the peer's pointer moves before it is posted. A move
    /// out through the handoff's edge hands the pointer back and wakes the
    /// Poll. True once the pointer is back, so this and later moves are
    /// dropped.
    pub fn moved(&self, from: CursorPosition, to: CursorPosition) -> bool {
        let mut lease = self.lease();
        let Some(active) = lease.as_mut() else {
            return false;
        };
        if active.returned.is_some() {
            return true;
        }
        let Some(position) = active.edge.crossing(from, to) else {
            return false;
        };
        active.returned = Some(position);
        drop(lease);
        tracing::info!(position, "pointer left through the desktop handoff edge");
        self.changed.notify_waiters();
        true
    }

    /// Latches the return when the cursor sits on the handoff's edge.
    fn sample(&self, cursor: CursorPosition) {
        let mut lease = self.lease();
        if let Some(active) = lease.as_mut()
            && active.returned.is_none()
            && let Some(position) = active.edge.at_edge(cursor)
        {
            active.returned = Some(position);
            drop(lease);
            tracing::info!(position, "cursor reached the desktop handoff edge");
            self.changed.notify_waiters();
        }
    }

    fn returned(&self, session_id: u64, token: u64) -> Result<Option<u32>, &'static str> {
        match self.lease().as_ref() {
            Some(lease) if lease.session_id == session_id && lease.token == token => {
                Ok(lease.returned)
            }
            _ => Err(ENDED),
        }
    }

    fn holds(&self, session_id: u64, token: u64) -> bool {
        self.returned(session_id, token).is_ok()
    }

    /// Drops the handoff, if it is still this one.
    fn end(&self, session_id: u64, token: u64) {
        let ended = self
            .lease()
            .take_if(|lease| lease.session_id == session_id && lease.token == token);
        if ended.is_some() {
            drop(ended);
            self.changed.notify_waiters();
        }
    }

    /// Ends the handoff of a session that closed, and lets go of whatever
    /// its peer held.
    pub fn closed(&self, peer: &str, session_id: u64) {
        let ended = self
            .lease()
            .take_if(|lease| lease.peer == peer && lease.session_id == session_id);
        if ended.is_some() {
            drop(ended);
            tracing::info!(%peer, session_id, "desktop handoff ended with its session");
            self.changed.notify_waiters();
            self.desk.release_all();
        }
    }

    /// Ends a handoff that went two seconds without a Poll, or whose
    /// displays changed. As on Linux, whatever the peer holds is let go and
    /// its session closes. True if one ended.
    pub fn expire_at(&self, now: Instant) -> bool {
        let generation = self.desk.generation();
        let expired = self.lease().take_if(|lease| {
            now.saturating_duration_since(lease.renewed) >= LEASE || lease.generation != generation
        });
        let Some(expired) = expired else {
            return false;
        };
        let (peer, session_id) = (expired.peer.clone(), expired.session_id);
        drop(expired);
        tracing::info!(%peer, session_id, "desktop handoff expired");
        self.changed.notify_waiters();
        self.desk.release_all();
        self.desk.close(&peer, session_id);
        true
    }

    /// Checks the lease every quarter second while there is one, so a peer
    /// that stops polling, or a display change, ends the handoff. Runs until
    /// dropped.
    pub async fn watch(&self) {
        loop {
            let changed = self.changed.notified();
            self.expire_at(Instant::now());
            if self.lease().is_some() {
                tokio::time::sleep(LEASE_CHECK).await;
            } else {
                changed.await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::macos::receive::{HANDOFF_ACTIVE, OWNED, SENDING};

    const MAX: u32 = FRACTION_MAX;

    fn desktop(monitors: &[(i32, i32, u32, u32)]) -> Geometry {
        Geometry {
            monitors: monitors
                .iter()
                .map(|&(x, y, width, height)| Rect {
                    x,
                    y,
                    width,
                    height,
                })
                .collect(),
        }
    }

    fn single() -> Geometry {
        desktop(&[(0, 0, 1920, 1080)])
    }

    fn at(x: f64, y: f64) -> CursorPosition {
        CursorPosition { x, y }
    }

    fn entry(geometry: &Geometry, edge: Edge, start: u32, end: u32, position: u32) -> Point {
        ArmedEdge::new(geometry, edge, start, end)
            .unwrap()
            .entry(position)
            .unwrap()
    }

    // The GNOME extension's vectors, from tests/gnome_desktop_test.mjs.
    #[test]
    fn entries_land_where_the_gnome_extension_puts_them() {
        let one = single();
        for (edge, x, y) in [
            (Edge::Left, 3, 540),
            (Edge::Right, 1916, 540),
            (Edge::Top, 960, 3),
            (Edge::Bottom, 960, 1076),
        ] {
            assert_eq!(entry(&one, edge, 0, MAX, 500_000), Point { x, y });
        }
        // The first and last pixel of a partial range round inside it.
        assert_eq!(entry(&one, Edge::Left, 185_185, MAX, 185_185).y, 200);
        assert_eq!(entry(&one, Edge::Left, 0, 500_000, 500_000).y, 539);

        let gap = desktop(&[(-200, -100, 200, 100), (-200, 100, 200, 100)]);
        let inside_gap = ArmedEdge::new(&gap, Edge::Left, 366_667, 600_000).unwrap();
        assert_eq!(inside_gap.entry(500_000), Err(NO_DISPLAY));
        assert_eq!(
            entry(&gap, Edge::Left, 0, MAX, 100_000),
            Point { x: -197, y: -70 }
        );
        assert_eq!(
            entry(&gap, Edge::Left, 0, MAX, 500_000),
            Point { x: -197, y: 100 },
            "an entry into the gap moves to the nearest display"
        );

        // 1920x1080 left of 2560x1440: the bottom of the left edge has no display.
        let wide = desktop(&[(0, 0, 1920, 1080), (1920, 0, 2560, 1440)]);
        assert_eq!(
            entry(&wide, Edge::Left, 0, MAX, 900_000),
            Point { x: 3, y: 1079 }
        );

        let thin = desktop(&[(0, 0, 4, 1080)]);
        let edge = ArmedEdge::new(&thin, Edge::Left, 0, MAX).unwrap();
        assert_eq!(edge.entry(500_000), Err("The entry display is too small"));
    }

    #[test]
    fn the_pointer_returns_where_it_leaves_the_edge() {
        let one = single();
        let left = ArmedEdge::new(&one, Edge::Left, 0, MAX).unwrap();
        assert_eq!(left.at_edge(at(0.0, 270.0)), Some(250_000));
        assert_eq!(left.at_edge(at(0.9, 270.5)), Some(250_463));
        assert_eq!(left.at_edge(at(1.0, 270.0)), None);
        assert_eq!(
            left.crossing(at(4.0, 260.0), at(-4.0, 280.0)),
            Some(250_000),
            "the move crosses x 0 halfway"
        );
        assert_eq!(left.crossing(at(4.0, 260.0), at(0.5, 280.0)), None);
        assert_eq!(left.crossing(at(-1.0, 260.0), at(-4.0, 280.0)), None);

        let right = ArmedEdge::new(&one, Edge::Right, 0, MAX).unwrap();
        assert_eq!(right.at_edge(at(1919.0, 540.0)), Some(500_000));
        assert_eq!(right.at_edge(at(1918.9, 540.0)), None);
        assert_eq!(
            right.crossing(at(1919.0, 540.0), at(1921.0, 540.0)),
            Some(500_000)
        );
        let top = ArmedEdge::new(&one, Edge::Top, 0, MAX).unwrap();
        assert_eq!(top.crossing(at(480.0, 2.0), at(480.0, -1.0)), Some(250_000));
        let bottom = ArmedEdge::new(&one, Edge::Bottom, 0, MAX).unwrap();
        assert_eq!(bottom.at_edge(at(480.0, 1079.0)), Some(250_000));

        // Nothing returns where no display in the range touches the edge.
        let gap = desktop(&[(-200, -100, 200, 100), (-200, 100, 200, 100)]);
        let gap = ArmedEdge::new(&gap, Edge::Left, 0, MAX).unwrap();
        assert_eq!(gap.at_edge(at(-200.0, 50.0)), None);
        assert_eq!(gap.at_edge(at(-200.0, 150.0)), Some(833_333));
        let partial = ArmedEdge::new(&one, Edge::Left, 0, 500_000).unwrap();
        assert_eq!(partial.at_edge(at(0.0, 600.0)), None);
        assert_eq!(partial.at_edge(at(0.0, 539.9)), Some(499_907));
    }

    #[derive(Default)]
    struct Fake {
        geometry: Option<Geometry>,
        generation: u32,
        cursor: Option<CursorPosition>,
        /// The cursor lands where it is moved, as it does when macOS shows
        /// the move at once.
        follows: bool,
        moves: Vec<Point>,
        releases: usize,
        wakes: usize,
        closed: Vec<(String, u64)>,
        /// Sessions that ended before their request was answered.
        ended: Vec<(String, u64)>,
    }

    #[derive(Clone)]
    struct FakeDesk(Arc<Mutex<Fake>>);

    impl FakeDesk {
        fn state(&self) -> MutexGuard<'_, Fake> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    impl Desk for FakeDesk {
        fn geometry(&self) -> Result<Geometry> {
            self.state()
                .geometry
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no displays"))
        }

        fn generation(&self) -> u32 {
            self.state().generation
        }

        fn cursor(&self) -> Result<CursorPosition> {
            self.state()
                .cursor
                .ok_or_else(|| anyhow::anyhow!("no cursor"))
        }

        fn move_to(&self, point: Point) {
            let mut fake = self.state();
            fake.moves.push(point);
            if fake.follows {
                fake.cursor = Some(at(f64::from(point.x), f64::from(point.y)));
            }
        }

        fn release_all(&self) {
            self.state().releases += 1;
        }

        fn wake(&self) {
            self.state().wakes += 1;
        }

        fn close(&self, peer: &str, session_id: u64) {
            self.state().closed.push((peer.to_owned(), session_id));
        }

        fn current(&self, peer: &str, session_id: u64) -> bool {
            !self.state().ended.contains(&(peer.to_owned(), session_id))
        }
    }

    fn setup(geometry: Geometry) -> (Arc<HandoffServer<FakeDesk>>, FakeDesk, Ownership) {
        let desk = FakeDesk(Arc::new(Mutex::new(Fake {
            geometry: Some(geometry),
            cursor: Some(at(960.5, 540.5)),
            follows: true,
            ..Fake::default()
        })));
        let ownership = Ownership::default();
        let server = HandoffServer::new(desk.clone(), ownership.clone());
        (Arc::new(server), desk, ownership)
    }

    fn prepare(token: u64) -> DesktopRequest {
        DesktopRequest::Prepare {
            token,
            edge: Edge::Left,
            start: 0,
            end: MAX,
            position: 500_000,
        }
    }

    fn unavailable(reason: &str) -> DesktopResponse {
        DesktopResponse::unavailable(reason)
    }

    #[tokio::test]
    async fn a_handoff_enters_polls_returns_and_finishes() {
        let (server, desk, ownership) = setup(single());
        let ask = |request| server.request("linux", 1, request);
        assert_eq!(
            ask(DesktopRequest::Snapshot).await,
            DesktopResponse::Snapshot {
                geometry: single(),
                position: Point { x: 960, y: 540 },
            }
        );
        assert_eq!(
            ask(prepare(7)).await,
            DesktopResponse::Prepared {
                geometry: single(),
                position: Point { x: 3, y: 540 },
            }
        );
        assert_eq!(desk.state().moves, [Point { x: 3, y: 540 }]);
        assert_eq!(desk.state().wakes, 1, "Prepare wakes the display");
        assert_eq!(ownership.controller().as_deref(), Some("linux"));

        // With the pointer inside, a Poll holds, then answers Active.
        let started = Instant::now();
        let poll = DesktopRequest::Poll { token: 7 };
        assert_eq!(ask(poll.clone()).await, DesktopResponse::Active);
        assert!(started.elapsed() >= POLL_HOLD);

        // The Mac's own trackpad at the edge hands the pointer back too.
        desk.state().cursor = Some(at(0.0, 270.0));
        let returned = DesktopResponse::Returned { position: 250_000 };
        assert_eq!(ask(poll.clone()).await, returned);
        assert_eq!(
            ask(DesktopRequest::Finish { token: 8 }).await,
            unavailable(STALE)
        );
        assert_eq!(
            ask(poll.clone()).await,
            returned,
            "a stale token cannot end the handoff"
        );
        assert_eq!(
            server.request("desk", 2, poll.clone()).await,
            unavailable(OWNED)
        );
        assert_eq!(desk.state().releases, 0);
        assert_eq!(
            ask(DesktopRequest::Finish { token: 7 }).await,
            DesktopResponse::Finished
        );
        assert_eq!(desk.state().releases, 1);
        assert_eq!(ownership.controller(), None);
        assert_eq!(ask(poll).await, unavailable(ENDED));
        assert_eq!(
            ask(DesktopRequest::Finish { token: 7 }).await,
            unavailable(ENDED)
        );
        assert!(matches!(
            ask(DesktopRequest::Poll { token: 0 }).await,
            DesktopResponse::Unavailable { .. }
        ));
    }

    #[tokio::test]
    async fn one_handoff_at_a_time_and_none_while_the_mac_sends() {
        let (server, _desk, ownership) = setup(single());
        assert!(matches!(
            server.request("linux", 1, prepare(7)).await,
            DesktopResponse::Prepared { .. }
        ));
        assert_eq!(
            server.request("linux", 1, prepare(8)).await,
            unavailable(HANDOFF_ACTIVE)
        );
        assert_eq!(
            server.request("desk", 2, prepare(8)).await,
            unavailable(OWNED)
        );
        server
            .request("linux", 1, DesktopRequest::Finish { token: 7 })
            .await;

        let outbound = ownership.begin_outbound().unwrap();
        for request in [DesktopRequest::Snapshot, prepare(9)] {
            assert_eq!(
                server.request("linux", 1, request).await,
                unavailable(SENDING)
            );
        }
        drop(outbound);

        let claim = ownership.claim_inbound("desk", 2).unwrap();
        assert_eq!(
            server.request("linux", 1, prepare(9)).await,
            unavailable(OWNED)
        );
        drop(claim);

        // A range with no display behind it takes nothing.
        let (server, _desk, ownership) =
            setup(desktop(&[(-200, -100, 200, 100), (-200, 100, 200, 100)]));
        let gap = DesktopRequest::Prepare {
            token: 7,
            edge: Edge::Left,
            start: 366_667,
            end: 600_000,
            position: 500_000,
        };
        assert_eq!(
            server.request("linux", 1, gap).await,
            unavailable(NO_DISPLAY)
        );
        assert_eq!(ownership.controller(), None);
    }

    #[tokio::test]
    async fn a_waiting_poll_wakes_when_the_peer_moves_out() {
        let (server, _desk, _) = setup(single());
        server.request("linux", 1, prepare(7)).await;
        let polling = tokio::spawn({
            let server = server.clone();
            async move {
                let started = Instant::now();
                let response = server
                    .request("linux", 1, DesktopRequest::Poll { token: 7 })
                    .await;
                (response, started.elapsed())
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !server.moved(at(3.0, 270.0), at(1.0, 270.0)),
            "still inside"
        );
        assert!(server.moved(at(1.0, 270.0), at(-2.0, 270.0)));
        let (response, elapsed) = polling.await.unwrap();
        assert_eq!(response, DesktopResponse::Returned { position: 250_000 });
        assert!(elapsed < POLL_HOLD, "woken, not timed out");
        assert!(
            server.moved(at(0.0, 270.0), at(10.0, 270.0)),
            "later motion is dropped"
        );
    }

    #[tokio::test]
    async fn a_lease_lapses_without_polls_or_when_the_displays_change() {
        let (server, desk, ownership) = setup(single());
        let before = Instant::now();
        server.request("linux", 1, prepare(7)).await;
        assert!(!server.expire_at(before + LEASE - Duration::from_millis(1)));
        assert!(server.expire_at(Instant::now() + LEASE + Duration::from_millis(1)));
        assert_eq!(desk.state().closed, [("linux".to_owned(), 1)]);
        assert_eq!(desk.state().releases, 1);
        assert_eq!(ownership.controller(), None);
        let poll = |token| server.request("linux", 1, DesktopRequest::Poll { token });
        assert_eq!(poll(7).await, unavailable(ENDED));

        // A display change ends the next request's handoff.
        server.request("linux", 1, prepare(8)).await;
        desk.state().generation += 1;
        assert_eq!(poll(8).await, unavailable(ENDED));
        assert_eq!(desk.state().closed.len(), 2);

        // A held Poll ends with its lease.
        server.request("linux", 1, prepare(9)).await;
        let polling = tokio::spawn({
            let server = server.clone();
            async move {
                server
                    .request("linux", 1, DesktopRequest::Poll { token: 9 })
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(server.expire_at(Instant::now() + LEASE));
        assert_eq!(polling.await.unwrap(), unavailable(ENDED));

        // The watch notices a display change without any request.
        let watching = tokio::spawn({
            let server = server.clone();
            async move { server.watch().await }
        });
        server.request("linux", 1, prepare(10)).await;
        desk.state().generation += 1;
        tokio::time::sleep(LEASE_CHECK + Duration::from_millis(50)).await;
        assert_eq!(ownership.controller(), None);
        assert_eq!(desk.state().closed.len(), 4);
        watching.abort();
    }

    #[tokio::test]
    async fn a_closed_session_ends_its_handoff() {
        let (server, desk, ownership) = setup(single());
        server.request("linux", 1, prepare(7)).await;
        server.closed("linux", 2);
        server.closed("desk", 1);
        assert_eq!(ownership.controller().as_deref(), Some("linux"));
        assert_eq!(desk.state().releases, 0);
        server.closed("linux", 1);
        assert_eq!(ownership.controller(), None);
        assert_eq!(desk.state().releases, 1);
        assert!(desk.state().closed.is_empty(), "it is closed already");
    }

    #[tokio::test]
    async fn a_prepare_that_outlives_its_session_takes_nothing() {
        let (server, desk, ownership) = setup(single());
        desk.state().ended.push(("linux".to_owned(), 1));
        assert_eq!(
            server.request("linux", 1, prepare(7)).await,
            unavailable(ENDED)
        );
        assert_eq!(ownership.controller(), None);
        assert!(desk.state().moves.is_empty());
        assert!(matches!(
            server.request("linux", 2, prepare(8)).await,
            DesktopResponse::Prepared { .. }
        ));
    }

    #[tokio::test]
    async fn the_cursor_has_to_reach_the_entry() {
        let (server, desk, ownership) = setup(single());
        desk.state().follows = false;
        let started = Instant::now();
        assert_eq!(
            server.request("linux", 1, prepare(7)).await,
            unavailable("The Mac did not place the cursor at the requested entry")
        );
        assert!(started.elapsed() >= ENTRY_WAIT);
        assert_eq!(ownership.controller(), None);

        // Within two points of the entry counts.
        desk.state().cursor = Some(at(4.5, 541.9));
        assert_eq!(
            server.request("linux", 1, prepare(8)).await,
            DesktopResponse::Prepared {
                geometry: single(),
                position: Point { x: 4, y: 541 },
            }
        );
        server
            .request("linux", 1, DesktopRequest::Finish { token: 8 })
            .await;

        // A display change while the cursor travels ends the handoff.
        desk.state().cursor = Some(at(500.0, 500.0));
        let preparing = tokio::spawn({
            let server = server.clone();
            async move { server.request("linux", 1, prepare(9)).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        desk.state().generation += 1;
        assert!(server.expire_at(Instant::now()));
        assert_eq!(
            preparing.await.unwrap(),
            unavailable("The Mac desktop changed during entry")
        );
    }

    #[tokio::test]
    async fn snapshots_put_the_cursor_on_a_display() {
        let (server, desk, _) = setup(desktop(&[(-200, -100, 200, 100), (-200, 100, 200, 100)]));
        let snapshot = || server.request("linux", 1, DesktopRequest::Snapshot);
        for (cursor, x, y) in [
            (at(0.5, -0.25), -1, -1),
            (at(-100.0, 50.0), -100, 100),
            (at(-250.0, 150.7), -200, 150),
        ] {
            desk.state().cursor = Some(cursor);
            let DesktopResponse::Snapshot { position, .. } = snapshot().await else {
                panic!("no snapshot");
            };
            assert_eq!(position, Point { x, y });
        }
        desk.state().cursor = None;
        assert_eq!(snapshot().await, unavailable("no cursor"));
    }
}
