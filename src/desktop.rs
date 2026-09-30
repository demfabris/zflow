//! Desktop handoff metadata carried inside an authenticated input session.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const FRACTION_MAX: u32 = 1_000_000;
pub const MAX_TOKEN: u64 = 9_007_199_254_740_991;
pub const LEASE_MS: u64 = 2_000;
// packaging/gnome-extension/extension.js mirrors this hold duration.
pub const POLL_HOLD_MS: u64 = 200;
pub const REQUEST_TIMEOUT_MS: u64 = 1_000;
pub const MAX_MESSAGE_BYTES: usize = 4_096;
pub const EXTENSION_ID: &str = "zflow@demfabris";
pub const BUS_NAME: &str = "org.gnome.Shell.Extensions.Zflow";
pub const OBJECT_PATH: &str = "/org/gnome/Shell/Extensions/Zflow";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn contains(&self, point: Point) -> bool {
        i64::from(point.x) >= i64::from(self.x)
            && i64::from(point.y) >= i64::from(self.y)
            && i64::from(point.x) < i64::from(self.x) + i64::from(self.width)
            && i64::from(point.y) < i64::from(self.y) + i64::from(self.height)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Geometry {
    pub monitors: Vec<Rect>,
}

impl Geometry {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.monitors.is_empty() && self.monitors.len() <= 16,
            "Desktop must contain 1 to 16 monitors"
        );
        for rect in &self.monitors {
            ensure!(
                (1..=16384).contains(&rect.width)
                    && (1..=16384).contains(&rect.height)
                    && rect.x.unsigned_abs() <= 65536
                    && rect.y.unsigned_abs() <= 65536,
                "Invalid desktop monitor geometry"
            );
        }
        Ok(())
    }
    pub fn bounds(&self) -> Result<Rect> {
        self.validate()?;
        let x = self.monitors.iter().map(|r| r.x).min().unwrap();
        let y = self.monitors.iter().map(|r| r.y).min().unwrap();
        let right = self
            .monitors
            .iter()
            .map(|r| i64::from(r.x) + i64::from(r.width))
            .max()
            .unwrap();
        let bottom = self
            .monitors
            .iter()
            .map(|r| i64::from(r.y) + i64::from(r.height))
            .max()
            .unwrap();
        Ok(Rect {
            x,
            y,
            width: (right - i64::from(x)) as u32,
            height: (bottom - i64::from(y)) as u32,
        })
    }
}

/// Maps positions between the saved local edge and the receiver crossing range.
#[derive(Clone, Debug)]
pub struct ReturnMapping {
    pub geometry: Geometry,
    pub edge: Edge,
    pub local_start: f64,
    pub local_end: f64,
    pub remote_start: f64,
    pub remote_end: f64,
}

impl ReturnMapping {
    fn validate(&self) -> Result<()> {
        ensure!(
            [
                self.local_start,
                self.local_end,
                self.remote_start,
                self.remote_end
            ]
            .iter()
            .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
                && self.local_start < self.local_end
                && self.remote_start < self.remote_end,
            "Invalid desktop return mapping"
        );
        Ok(())
    }

    pub fn fraction(&self, point: Point) -> Result<u32> {
        self.validate()?;
        let bounds = self.geometry.bounds()?;
        let along = match self.edge {
            Edge::Left | Edge::Right => {
                (f64::from(point.y) - f64::from(bounds.y)) / f64::from(bounds.height)
            }
            Edge::Top | Edge::Bottom => {
                (f64::from(point.x) - f64::from(bounds.x)) / f64::from(bounds.width)
            }
        };
        let progress =
            ((along - self.local_start) / (self.local_end - self.local_start)).clamp(0.0, 1.0);
        let remote = self.remote_start + progress * (self.remote_end - self.remote_start);
        Ok((remote * f64::from(FRACTION_MAX)).round() as u32)
    }

    pub fn position(&self, position: u32) -> Result<Point> {
        self.validate()?;
        ensure!(
            position >= (self.remote_start * f64::from(FRACTION_MAX)).round() as u32
                && position <= (self.remote_end * f64::from(FRACTION_MAX)).round() as u32,
            "The other computer returned an invalid crossing position"
        );
        let remote = f64::from(position) / f64::from(FRACTION_MAX);
        let progress =
            ((remote - self.remote_start) / (self.remote_end - self.remote_start)).clamp(0.0, 1.0);
        let along = self.local_start + progress * (self.local_end - self.local_start);
        let point = edge_point(self.geometry.bounds()?, self.edge, along);
        // The receiver's barrier spans the whole shared range, which can include
        // parts of this edge with no display behind them. Return to the nearest
        // point on the edge that has one.
        self.geometry
            .monitors
            .iter()
            .filter_map(|r| {
                let near = match self.edge {
                    Edge::Left | Edge::Right => Point {
                        y: point.y.clamp(r.y, r.y + r.height as i32 - 1),
                        ..point
                    },
                    Edge::Top | Edge::Bottom => Point {
                        x: point.x.clamp(r.x, r.x + r.width as i32 - 1),
                        ..point
                    },
                };
                r.contains(near).then_some(near)
            })
            .min_by_key(|near| near.x.abs_diff(point.x) + near.y.abs_diff(point.y))
            .context("No active Mac display touches the return edge; check the layout")
    }
}

fn edge_point(bounds: Rect, edge: Edge, along: f64) -> Point {
    let inset_x = 3.min((bounds.width.saturating_sub(1) / 2) as i32);
    let inset_y = 3.min((bounds.height.saturating_sub(1) / 2) as i32);
    let x = bounds.x
        + (along * f64::from(bounds.width))
            .floor()
            .clamp(0.0, f64::from(bounds.width - 1)) as i32;
    let y = bounds.y
        + (along * f64::from(bounds.height))
            .floor()
            .clamp(0.0, f64::from(bounds.height - 1)) as i32;
    match edge {
        Edge::Left => Point {
            x: bounds.x + inset_x,
            y,
        },
        Edge::Right => Point {
            x: bounds.x + bounds.width as i32 - 1 - inset_x,
            y,
        },
        Edge::Top => Point {
            x,
            y: bounds.y + inset_y,
        },
        Edge::Bottom => Point {
            x,
            y: bounds.y + bounds.height as i32 - 1 - inset_y,
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum DesktopRequest {
    Snapshot,
    Prepare {
        token: u64,
        edge: Edge,
        start: u32,
        end: u32,
        position: u32,
    },
    Poll {
        token: u64,
    },
    Finish {
        token: u64,
    },
}

impl DesktopRequest {
    pub fn token(&self) -> Option<u64> {
        match self {
            Self::Snapshot => None,
            Self::Prepare { token, .. } | Self::Poll { token } | Self::Finish { token } => {
                Some(*token)
            }
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.token()
                .is_none_or(|token| token > 0 && token <= MAX_TOKEN),
            "Desktop handoff token must be a positive safe JSON integer"
        );
        if let Self::Prepare {
            start,
            end,
            position,
            ..
        } = self
        {
            ensure!(
                start < end && *end <= FRACTION_MAX && position >= start && position <= end,
                "Invalid desktop edge range or position"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum DesktopResponse {
    Snapshot { geometry: Geometry, position: Point },
    Prepared { geometry: Geometry, position: Point },
    Active,
    Returned { position: u32 },
    Finished,
    Unavailable { reason: String },
}

impl DesktopResponse {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        let reason: String = reason.into();
        Self::Unavailable {
            reason: reason.chars().take(256).collect(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Snapshot { geometry, position } | Self::Prepared { geometry, position } => {
                geometry.validate()?;
                ensure!(
                    geometry.monitors.iter().any(|r| r.contains(*position)),
                    "Cursor is outside active monitors"
                );
            }
            Self::Returned { position } => {
                ensure!(*position <= FRACTION_MAX, "Invalid return position")
            }
            Self::Unavailable { reason } => {
                ensure!(reason.len() <= 1024, "Desktop diagnostic is too long")
            }
            _ => {}
        }
        Ok(())
    }
}

/// At most this many computers share a layout, which keeps it inside one
/// desktop message.
pub const MAX_SHARED_TILES: usize = 16;

/// The arrangement every paired computer keeps. Tiles are keyed by key
/// fingerprint, because each computer names the others differently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedLayout {
    /// Raised by every edit. With `editor` it orders two versions, so both
    /// computers keep the same one.
    pub version: u64,
    /// The fingerprint of the computer that made this version.
    pub editor: String,
    pub tiles: Vec<Tile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tile {
    /// The computer's key fingerprint.
    pub key: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl SharedLayout {
    pub fn validate(&self) -> Result<()> {
        use crate::app::layout_model::{MAX_COORDINATE, MAX_DIMENSION};
        let fingerprint = |key: &str| {
            key.len() == 64
                && key
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        ensure!(fingerprint(&self.editor), "Invalid layout editor");
        // Kept to what JSON carries exactly, which also stops two computers
        // from pinning it at the maximum and trading edits forever.
        ensure!(self.version <= MAX_TOKEN, "Invalid layout version");
        ensure!(
            self.tiles.len() <= MAX_SHARED_TILES,
            "A shared layout holds at most {MAX_SHARED_TILES} computers"
        );
        let mut keys = std::collections::BTreeSet::new();
        for tile in &self.tiles {
            ensure!(
                fingerprint(&tile.key) && keys.insert(&tile.key),
                "Invalid or repeated layout tile"
            );
            ensure!(
                (-MAX_COORDINATE..=MAX_COORDINATE).contains(&tile.x)
                    && (-MAX_COORDINATE..=MAX_COORDINATE).contains(&tile.y)
                    && (1..=MAX_DIMENSION).contains(&tile.width)
                    && (1..=MAX_DIMENSION).contains(&tile.height),
                "Invalid layout tile geometry"
            );
        }
        Ok(())
    }

    /// Whether this version replaces `other`. Both computers answer the same.
    pub fn is_newer_than(&self, other: &Self) -> bool {
        (self.version, &self.editor) > (other.version, &other.editor)
    }

    /// A new version in which the computer `key` gives its own tile the size
    /// of its desktop. Each computer writes only its own size, so two never
    /// edit the same tile. The sides that touch a neighbour stay where they
    /// are, so the crossings stay; a tile that would still overlap another
    /// moves to the right of the rest. A layout from a computer that never
    /// had this one gets its tile beside the first tile, so an edge leads to
    /// it. None when the size is already right or the layout is full.
    pub fn with_own_size(&self, key: &str, width: u32, height: u32) -> Option<Self> {
        let Some(index) = self.tiles.iter().position(|tile| tile.key == key) else {
            if self.tiles.len() >= MAX_SHARED_TILES {
                return None;
            }
            let mut next = self.clone();
            next.tiles.push(free_spot(
                &self.tiles,
                self.tiles.first(),
                key,
                (width, height),
            )?);
            next.version = self.version.checked_add(1)?;
            next.editor = key.to_owned();
            return next.validate().ok().map(|()| next);
        };
        let old = &self.tiles[index];
        if (old.width, old.height) == (width, height) {
            return None;
        }
        let others = || {
            self.tiles
                .iter()
                .enumerate()
                .filter(move |(i, _)| *i != index)
                .map(|(_, other)| other)
        };
        let ((left, right), (top, bottom)) = (span(old.x, old.width), span(old.y, old.height));
        let beside = |other: &Tile| {
            let (other_top, other_bottom) = span(other.y, other.height);
            other_top < bottom && other_bottom > top
        };
        let above_or_below = |other: &Tile| {
            let (other_left, other_right) = span(other.x, other.width);
            other_left < right && other_right > left
        };
        let touches_left = others().any(|o| beside(o) && span(o.x, o.width).1 == left);
        let touches_right = others().any(|o| beside(o) && i64::from(o.x) == right);
        let touches_top = others().any(|o| above_or_below(o) && span(o.y, o.height).1 == top);
        let touches_bottom = others().any(|o| above_or_below(o) && i64::from(o.y) == bottom);
        let mut next = self.clone();
        next.version = self.version.checked_add(1)?;
        next.editor = key.to_owned();
        let tile = &mut next.tiles[index];
        if touches_right && !touches_left {
            tile.x = i32::try_from(right - i64::from(width)).ok()?;
        }
        if touches_bottom && !touches_top {
            tile.y = i32::try_from(bottom - i64::from(height)).ok()?;
        }
        tile.width = width;
        tile.height = height;
        let resized = next.tiles[index].clone();
        if others().any(|other| overlaps(&resized, other)) {
            let right = others()
                .map(|other| other.x.saturating_add(other.width as i32))
                .max()
                .unwrap_or(0);
            next.tiles[index].x = right;
            next.tiles[index].y = 0;
        }
        next.validate().ok().map(|()| next)
    }

    /// A new version, edited by `own`, with a tile of `size` for each of
    /// `keys` the layout lacks, such as a computer paired after the layout
    /// was arranged. Each goes beside `own`'s tile where there is room: to
    /// the right, left, below or above, else to the right of all the rest.
    /// None when no tile is missing or the layout is full.
    pub fn with_tiles_for<'a>(
        &self,
        own: &str,
        keys: impl IntoIterator<Item = &'a str>,
        (width, height): (u32, u32),
    ) -> Option<Self> {
        let mut next = self.clone();
        for key in keys {
            if key == own || next.tiles.iter().any(|tile| tile.key == key) {
                continue;
            }
            if next.tiles.len() >= MAX_SHARED_TILES {
                break;
            }
            let anchor = next.tiles.iter().find(|tile| tile.key == own);
            let placed = free_spot(&next.tiles, anchor, key, (width, height))?;
            next.tiles.push(placed);
        }
        if next.tiles.len() == self.tiles.len() {
            return None;
        }
        next.version = self.version.checked_add(1)?;
        next.editor = own.to_owned();
        next.validate().ok().map(|()| next)
    }
}

/// Where a new tile of `size` for `key` goes among `tiles`: beside `anchor`
/// where there is room (right, left, below, above), else to the right of all
/// of them. None when that is off the canvas.
fn free_spot(
    tiles: &[Tile],
    anchor: Option<&Tile>,
    key: &str,
    (width, height): (u32, u32),
) -> Option<Tile> {
    use crate::app::layout_model::MAX_COORDINATE;
    let tile = |x: i64, y: i64| {
        let bounds = -i64::from(MAX_COORDINATE)..=i64::from(MAX_COORDINATE);
        (bounds.contains(&x) && bounds.contains(&y)).then(|| Tile {
            key: key.to_owned(),
            x: x as i32,
            y: y as i32,
            width,
            height,
        })
    };
    let (w, h) = (i64::from(width), i64::from(height));
    let beside = anchor.map_or_else(Vec::new, |a| {
        let ((left, right), (top, bottom)) = (span(a.x, a.width), span(a.y, a.height));
        vec![
            (right, top),
            (left - w, top),
            (left, bottom),
            (left, top - h),
        ]
    });
    let past_all = tiles
        .iter()
        .map(|tile| span(tile.x, tile.width).1)
        .max()
        .unwrap_or(0);
    let row = anchor.map_or(0, |a| i64::from(a.y));
    beside
        .into_iter()
        .chain([(past_all, row)])
        .filter_map(|(x, y)| tile(x, y))
        .find(|tile| !tiles.iter().any(|other| overlaps(tile, other)))
}

fn span(start: i32, size: u32) -> (i64, i64) {
    (i64::from(start), i64::from(start) + i64::from(size))
}

fn overlaps(a: &Tile, b: &Tile) -> bool {
    let ((al, ar), (at, ab)) = (span(a.x, a.width), span(a.y, a.height));
    let ((bl, br), (bt, bb)) = (span(b.x, b.width), span(b.y, b.height));
    al < br && ar > bl && at < bb && ab > bt
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "direction", rename_all = "snake_case", deny_unknown_fields)]
pub enum DesktopMessage {
    Request {
        id: u64,
        request: DesktopRequest,
    },
    Response {
        id: u64,
        response: DesktopResponse,
    },
    /// The sender's layout, sent when a session starts and after each change.
    /// It needs no answer.
    Layout {
        layout: SharedLayout,
    },
}

impl DesktopMessage {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Layout { layout } => layout.validate(),
            Self::Request { id, request } => {
                ensure!(*id != 0, "Invalid desktop request ID");
                request.validate()
            }
            Self::Response { id, response } => {
                ensure!(*id != 0, "Invalid desktop response ID");
                response.validate()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_computer_paired_later_gets_a_tile_beside_this_one() {
        let key = |n: usize| format!("{n:064x}");
        let tile = |n, x, y, width, height| Tile {
            key: key(n),
            x,
            y,
            width,
            height,
        };
        let size = (1920, 1080);
        let alone = SharedLayout {
            version: 3,
            editor: key(9),
            tiles: vec![tile(1, 0, 0, 1000, 800)],
        };
        let keys = [key(1), key(2), key(2)];
        let placed = alone
            .with_tiles_for(&key(1), keys.iter().map(String::as_str), size)
            .unwrap();
        assert_eq!((placed.version, &placed.editor), (4, &key(1)));
        assert_eq!(placed.tiles[1..], [tile(2, 1000, 0, 1920, 1080)]);
        assert!(
            placed
                .with_tiles_for(&key(1), keys.iter().map(String::as_str), size)
                .is_none(),
            "nothing missing"
        );
        // The right is taken, so the next one goes left. Below would overlap
        // the tall tile on the right, so the one after goes above.
        let keys = [key(3), key(4)];
        let more = placed
            .with_tiles_for(&key(1), keys.iter().map(String::as_str), size)
            .unwrap();
        assert_eq!(
            more.tiles[2..],
            [tile(3, -1920, 0, 1920, 1080), tile(4, 0, -1080, 1920, 1080)]
        );
        // Without this computer's own tile, it goes right of the rest.
        let foreign = SharedLayout {
            tiles: vec![tile(2, 0, 0, 1000, 800)],
            ..alone.clone()
        };
        let keys = [key(3)];
        let past = foreign
            .with_tiles_for(&key(1), keys.iter().map(String::as_str), size)
            .unwrap();
        assert_eq!(past.tiles[1..], [tile(3, 1000, 0, 1920, 1080)]);
        // A layout from a computer that never had this one: this computer
        // writes its own tile beside the first one.
        let joined = foreign.with_own_size(&key(1), 1280, 720).unwrap();
        assert_eq!((joined.version, &joined.editor), (4, &key(1)));
        assert_eq!(joined.tiles[1..], [tile(1, 1000, 0, 1280, 720)]);
        let full = SharedLayout {
            tiles: (0..MAX_SHARED_TILES)
                .map(|n| tile(n + 10, n as i32 * 2000, 0, 1000, 800))
                .collect(),
            ..alone
        };
        let keys = [key(3)];
        assert!(
            full.with_tiles_for(&key(10), keys.iter().map(String::as_str), size)
                .is_none(),
            "no room for another computer"
        );
        assert!(full.with_own_size(&key(1), 1280, 720).is_none());
    }

    #[test]
    fn a_full_shared_layout_fits_one_desktop_message_and_orders_versions() {
        let key = |n: usize| format!("{n:064x}");
        let tile = |n| Tile {
            key: key(n),
            x: -crate::app::layout_model::MAX_COORDINATE,
            y: -crate::app::layout_model::MAX_COORDINATE,
            width: crate::app::layout_model::MAX_DIMENSION,
            height: crate::app::layout_model::MAX_DIMENSION,
        };
        let layout = SharedLayout {
            version: MAX_TOKEN,
            editor: key(99),
            tiles: (0..MAX_SHARED_TILES).map(tile).collect(),
        };
        let message = DesktopMessage::Layout {
            layout: layout.clone(),
        };
        message.validate().unwrap();
        assert!(serde_json::to_vec(&message).unwrap().len() <= MAX_MESSAGE_BYTES);

        let mut invalid = layout.clone();
        invalid.tiles.push(tile(MAX_SHARED_TILES));
        assert!(invalid.validate().is_err(), "too many tiles");
        let mut invalid = layout.clone();
        invalid.tiles[1].key = key(0);
        assert!(invalid.validate().is_err(), "repeated key");
        let mut invalid = layout.clone();
        invalid.tiles[0].width = 0;
        assert!(invalid.validate().is_err(), "a tile without area");
        let mut invalid = layout.clone();
        invalid.editor = "ABC".into();
        assert!(invalid.validate().is_err(), "not a fingerprint");

        // A computer writes its own size, as a new version it edited.
        let small = SharedLayout {
            version: 5,
            editor: key(9),
            tiles: vec![
                Tile {
                    key: key(1),
                    x: 0,
                    y: 0,
                    width: 1000,
                    height: 800,
                },
                Tile {
                    key: key(2),
                    x: 1000,
                    y: 0,
                    width: 500,
                    height: 500,
                },
            ],
        };
        assert!(
            small.with_own_size(&key(1), 1000, 800).is_none(),
            "already right"
        );
        let added = small.with_own_size(&key(3), 1000, 800).unwrap();
        assert_eq!(
            (added.tiles[2].x, added.tiles[2].y),
            (-1000, 0),
            "a missing tile goes beside the first, where there is room"
        );
        let taller = small.with_own_size(&key(2), 500, 900).unwrap();
        assert_eq!((taller.version, &taller.editor), (6, &key(2)));
        assert_eq!((taller.tiles[1].x, taller.tiles[1].height), (1000, 900));
        // The side touching a neighbour stays, whether the tile grows or shrinks.
        let wider = small.with_own_size(&key(1), 1200, 800).unwrap();
        assert_eq!((wider.tiles[0].x, wider.tiles[0].width), (-200, 1200));
        let narrower = small.with_own_size(&key(1), 600, 800).unwrap();
        assert_eq!((narrower.tiles[0].x, narrower.tiles[0].width), (400, 600));
        // Squeezed between two neighbours, a grown tile moves past the others.
        let mut middle = small.clone();
        middle.tiles.push(Tile {
            key: key(3),
            x: -300,
            y: 0,
            width: 300,
            height: 800,
        });
        let crowded = middle.with_own_size(&key(1), 1200, 800).unwrap();
        assert_eq!((crowded.tiles[0].x, crowded.tiles[0].y), (1500, 0));
        let mut maxed = small.clone();
        maxed.version = MAX_TOKEN;
        assert!(
            maxed.with_own_size(&key(1), 1200, 800).is_none(),
            "the version cannot grow"
        );
        maxed.version = MAX_TOKEN + 1;
        assert!(maxed.validate().is_err());

        let older = SharedLayout {
            version: 3,
            editor: key(9),
            tiles: Vec::new(),
        };
        let tie = SharedLayout {
            version: 3,
            editor: key(1),
            tiles: Vec::new(),
        };
        let newer = SharedLayout {
            version: 4,
            editor: key(0),
            tiles: Vec::new(),
        };
        assert!(newer.is_newer_than(&older) && !older.is_newer_than(&newer));
        assert!(
            older.is_newer_than(&tie) && !tie.is_newer_than(&older),
            "the editor breaks a tie"
        );
        assert!(!older.is_newer_than(&older));
    }

    #[test]
    fn rejects_invalid_range_and_token() {
        assert!(DesktopRequest::Poll { token: 0 }.validate().is_err());
        assert!(
            DesktopRequest::Prepare {
                token: 1,
                edge: Edge::Left,
                start: 100,
                end: 200,
                position: 0
            }
            .validate()
            .is_err()
        );
        assert!(
            DesktopRequest::Prepare {
                token: 1,
                edge: Edge::Left,
                start: 0,
                end: FRACTION_MAX,
                position: 0
            }
            .validate()
            .is_ok()
        );
    }
    #[test]
    fn fraction_round_trips_all_edges_and_clamps_partial_ranges() {
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            for (local_start, local_end, remote_start, remote_end) in
                [(0.0, 1.0, 0.0, 1.0), (0.25, 0.75, 0.125, 0.875)]
            {
                let mapping = ReturnMapping {
                    geometry: Geometry {
                        monitors: vec![Rect {
                            x: -1000,
                            y: -200,
                            width: 1600,
                            height: 1000,
                        }],
                    },
                    edge,
                    local_start,
                    local_end,
                    remote_start,
                    remote_end,
                };
                let start = (remote_start * f64::from(FRACTION_MAX)) as u32;
                let end = (remote_end * f64::from(FRACTION_MAX)) as u32;
                for position in [start, start + (end - start) / 3, (start + end) / 2, end] {
                    let point = mapping.position(position).unwrap();
                    // Flooring a local pixel costs at most 1500 fraction units in these ranges.
                    assert!(mapping.fraction(point).unwrap().abs_diff(position) <= 1501);
                }
                for (point, expected) in [
                    (
                        Point {
                            x: i32::MIN,
                            y: i32::MIN,
                        },
                        start,
                    ),
                    (
                        Point {
                            x: i32::MAX,
                            y: i32::MAX,
                        },
                        end,
                    ),
                ] {
                    assert_eq!(mapping.fraction(point).unwrap(), expected);
                }
            }
        }
        let mapping = ReturnMapping {
            geometry: Geometry {
                monitors: vec![Rect {
                    x: -1000,
                    y: -200,
                    width: 2000,
                    height: 1000,
                }],
            },
            edge: Edge::Right,
            local_start: 0.5,
            local_end: 1.0,
            remote_start: 0.0,
            remote_end: 0.5,
        };
        assert_eq!(mapping.fraction(Point { x: 999, y: 550 }).unwrap(), 250_000);
        assert_eq!(mapping.fraction(Point { x: 999, y: 300 }).unwrap(), 0);
        assert_eq!(mapping.fraction(Point { x: 999, y: 800 }).unwrap(), 500_000);
    }

    #[test]
    fn fraction_and_position_reject_invalid_mapping() {
        let mut mapping = ReturnMapping {
            geometry: Geometry {
                monitors: vec![Rect {
                    x: 0,
                    y: 0,
                    width: 1000,
                    height: 1000,
                }],
            },
            edge: Edge::Left,
            local_start: 0.0,
            local_end: 1.0,
            remote_start: 0.0,
            remote_end: 1.0,
        };
        for (start, end) in [(0.5, 0.5), (0.8, 0.2), (-0.1, 1.0), (0.0, f64::NAN)] {
            mapping.local_start = start;
            mapping.local_end = end;
            assert!(mapping.fraction(Point { x: 0, y: 500 }).is_err());
            assert!(mapping.position(500_000).is_err());
        }
    }

    #[test]
    fn desktop_position_must_be_on_a_monitor() {
        let geometry = Geometry {
            monitors: vec![
                Rect {
                    x: -100,
                    y: 0,
                    width: 100,
                    height: 100,
                },
                Rect {
                    x: 100,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ],
        };
        assert_eq!(geometry.bounds().unwrap().width, 300);
        assert!(
            DesktopResponse::Snapshot {
                geometry,
                position: Point { x: 20, y: 20 }
            }
            .validate()
            .is_err()
        );
    }
}
