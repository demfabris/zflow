//! Desktop handoff metadata carried inside an authenticated input session.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const FRACTION_MAX: u32 = 1_000_000;
pub const MAX_TOKEN: u64 = 9_007_199_254_740_991;
pub const LEASE_MS: u64 = 2_000;
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
