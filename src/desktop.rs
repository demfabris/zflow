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
    /// edit the same tile. A tile that would overlap another moves to the
    /// right of the rest. None when the size is already right or the tile is
    /// missing.
    pub fn with_own_size(&self, key: &str, width: u32, height: u32) -> Option<Self> {
        let index = self.tiles.iter().position(|tile| tile.key == key)?;
        let tile = &self.tiles[index];
        if (tile.width, tile.height) == (width, height) {
            return None;
        }
        let mut next = self.clone();
        next.version = self.version.saturating_add(1);
        next.editor = key.to_owned();
        let overlaps = |a: &Tile, b: &Tile| {
            let span =
                |start: i32, size: u32| (i64::from(start), i64::from(start) + i64::from(size));
            let ((al, ar), (at, ab)) = (span(a.x, a.width), span(a.y, a.height));
            let ((bl, br), (bt, bb)) = (span(b.x, b.width), span(b.y, b.height));
            al < br && ar > bl && at < bb && ab > bt
        };
        next.tiles[index].width = width;
        next.tiles[index].height = height;
        let resized = next.tiles[index].clone();
        if next
            .tiles
            .iter()
            .enumerate()
            .any(|(i, other)| i != index && overlaps(&resized, other))
        {
            let right = next
                .tiles
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != index)
                .map(|(_, other)| other.x.saturating_add(other.width as i32))
                .max()
                .unwrap_or(0);
            next.tiles[index].x = right;
            next.tiles[index].y = 0;
        }
        next.validate().ok().map(|()| next)
    }
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
            version: u64::MAX,
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
        assert!(
            small.with_own_size(&key(3), 1000, 800).is_none(),
            "not in the layout"
        );
        let taller = small.with_own_size(&key(2), 500, 900).unwrap();
        assert_eq!((taller.version, &taller.editor), (6, &key(2)));
        assert_eq!((taller.tiles[1].x, taller.tiles[1].height), (1000, 900));
        // Growing into a neighbour moves the tile past the others.
        let wider = small.with_own_size(&key(1), 1200, 800).unwrap();
        assert_eq!((wider.tiles[0].x, wider.tiles[0].width), (1500, 1200));

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
