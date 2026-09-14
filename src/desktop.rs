//! Desktop handoff metadata carried inside an authenticated input session.

use anyhow::{Result, ensure};
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
            "The receiver returned an invalid crossing position"
        );
        let remote = f64::from(position) / f64::from(FRACTION_MAX);
        let progress =
            ((remote - self.remote_start) / (self.remote_end - self.remote_start)).clamp(0.0, 1.0);
        let along = self.local_start + progress * (self.local_end - self.local_start);
        let point = edge_point(self.geometry.bounds()?, self.edge, along);
        ensure!(
            self.geometry.monitors.iter().any(|r| r.contains(point)),
            "The return point is outside the active Mac displays; check the layout"
        );
        Ok(point)
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "direction", rename_all = "snake_case", deny_unknown_fields)]
pub enum DesktopMessage {
    Request { id: u64, request: DesktopRequest },
    Response { id: u64, response: DesktopResponse },
}

impl DesktopMessage {
    pub fn validate(&self) -> Result<()> {
        match self {
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
