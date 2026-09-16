use std::{
    collections::BTreeSet,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::config::save_text;

pub const MAX_MONITORS: usize = 32;
pub const MAX_COORDINATE: i32 = 100_000;
pub const MAX_DIMENSION: u32 = 16_384;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Layout {
    pub monitors: Vec<Monitor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Monitor {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    pub source: usize,
    pub target: usize,
    pub edge: Edge,
    pub source_start: f64,
    pub source_end: f64,
    pub target_start: f64,
    pub target_end: f64,
}

impl Monitor {
    fn bounds(&self) -> (i64, i64, i64, i64) {
        let (x, y) = (i64::from(self.x), i64::from(self.y));
        (x, y, x + i64::from(self.width), y + i64::from(self.height))
    }

    fn overlaps(&self, other: &Self) -> bool {
        let (left, top, right, bottom) = self.bounds();
        let (other_left, other_top, other_right, other_bottom) = other.bounds();
        left < other_right && right > other_left && top < other_bottom && bottom > other_top
    }

    fn valid_geometry(&self) -> bool {
        (-MAX_COORDINATE..=MAX_COORDINATE).contains(&self.x)
            && (-MAX_COORDINATE..=MAX_COORDINATE).contains(&self.y)
            && (1..=MAX_DIMENSION).contains(&self.width)
            && (1..=MAX_DIMENSION).contains(&self.height)
    }
}

impl Layout {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.monitors.len() <= MAX_MONITORS,
            "Use at most {MAX_MONITORS} monitors."
        );
        let mut ids = BTreeSet::new();
        for (index, monitor) in self.monitors.iter().enumerate() {
            for value in [&monitor.id, &monitor.label]
                .into_iter()
                .chain(monitor.peer.iter())
            {
                ensure!(
                    !value.trim().is_empty()
                        && value.len() <= 128
                        && !value.chars().any(char::is_control),
                    "Monitor IDs, labels, and peer names must contain 1 to 128 bytes without control characters."
                );
            }
            ensure!(ids.insert(&monitor.id), "Monitor IDs must be unique.");
            ensure!(
                monitor.valid_geometry(),
                "Monitor coordinates must be within ±{MAX_COORDINATE}; dimensions must be between 1 and {MAX_DIMENSION}."
            );
            for other in &self.monitors[..index] {
                ensure!(
                    !monitor.overlaps(other),
                    "Monitors {} and {} overlap.",
                    monitor.label,
                    other.label
                );
            }
        }
        Ok(())
    }

    pub fn transitions(&self) -> Vec<Transition> {
        if self.validate().is_err() {
            return Vec::new();
        }
        let mut transitions = Vec::new();
        for (source, a) in self.monitors.iter().enumerate() {
            for (target, b) in self.monitors.iter().enumerate() {
                if source == target || a.peer == b.peer {
                    continue;
                }
                let (al, at, ar, ab) = a.bounds();
                let (bl, bt, br, bb) = b.bounds();
                let edge = if ar == bl {
                    Some(Edge::Right)
                } else if al == br {
                    Some(Edge::Left)
                } else if ab == bt {
                    Some(Edge::Bottom)
                } else if at == bb {
                    Some(Edge::Top)
                } else {
                    None
                };
                let Some(edge) = edge else { continue };
                let (a_start, a_end, b_start, b_end) = match edge {
                    Edge::Left | Edge::Right => (at, ab, bt, bb),
                    Edge::Top | Edge::Bottom => (al, ar, bl, br),
                };
                let start = a_start.max(b_start);
                let end = a_end.min(b_end);
                if start < end {
                    transitions.push(Transition {
                        source,
                        target,
                        edge,
                        source_start: (start - a_start) as f64 / (a_end - a_start) as f64,
                        source_end: (end - a_start) as f64 / (a_end - a_start) as f64,
                        target_start: (start - b_start) as f64 / (b_end - b_start) as f64,
                        target_end: (end - b_start) as f64 / (b_end - b_start) as f64,
                    });
                }
            }
        }
        transitions
    }

    /// Snap to a nearby shared edge, or keep a valid free position. Reject overlap.
    pub fn snap_move(&self, index: usize, x: i32, y: i32, tolerance: i32) -> Option<(i32, i32)> {
        let moving = self.monitors.get(index)?;
        if !(0..=MAX_COORDINATE).contains(&tolerance) {
            return None;
        }
        let fits = |x: i64, y: i64| {
            let mut candidate = moving.clone();
            candidate.x = i32::try_from(x).ok()?;
            candidate.y = i32::try_from(y).ok()?;
            (candidate.valid_geometry()
                && self
                    .monitors
                    .iter()
                    .enumerate()
                    .all(|(i, other)| i == index || !candidate.overlaps(other)))
            .then_some((candidate.x, candidate.y))
        };
        let (x, y, tolerance) = (i64::from(x), i64::from(y), i64::from(tolerance));
        let (width, height) = (i64::from(moving.width), i64::from(moving.height));
        let mut best: Option<(i64, (i32, i32))> = None;
        for (other_index, other) in self.monitors.iter().enumerate() {
            if other_index == index {
                continue;
            }
            let (left, top, right, bottom) = other.bounds();
            let mut consider = |cx: i64, cy: i64| {
                let (dx, dy) = (cx - x, cy - y);
                if dx.abs() > tolerance || dy.abs() > tolerance {
                    return;
                }
                let distance = dx * dx + dy * dy;
                if best.is_none_or(|(best_distance, _)| distance < best_distance)
                    && let Some(position) = fits(cx, cy)
                {
                    best = Some((distance, position));
                }
            };
            for cx in [left - width, right] {
                for cy in [y, top, bottom - height] {
                    if cy < bottom && cy + height > top {
                        consider(cx, cy);
                    }
                }
            }
            for cy in [top - height, bottom] {
                for cx in [x, left, right - width] {
                    if cx < right && cx + width > left {
                        consider(cx, cy);
                    }
                }
            }
        }
        best.map(|(_, position)| position).or_else(|| fits(x, y))
    }
}

pub struct LayoutDocument {
    pub path: PathBuf,
    pub draft: Layout,
    saved: Layout,
    disk_contents: Option<Vec<u8>>,
}

impl LayoutDocument {
    pub fn open(config_path: &Path) -> Result<Self> {
        let mut file_name = config_path
            .file_name()
            .context("Configuration path needs a filename")?
            .to_os_string();
        file_name.push(".layout.toml");
        Self::open_path(config_path.with_file_name(file_name))
    }

    fn open_path(path: PathBuf) -> Result<Self> {
        let path = std::path::absolute(path).context("Could not resolve layout path")?;
        let disk_contents = read_contents(&path)?;
        let draft: Layout = match &disk_contents {
            Some(bytes) => {
                toml::from_str(std::str::from_utf8(bytes).context("Layout is not UTF-8")?)
                    .with_context(|| format!("Could not parse {}", path.display()))?
            }
            None => Layout::default(),
        };
        draft.validate()?;
        Ok(Self {
            path,
            saved: draft.clone(),
            draft,
            disk_contents,
        })
    }

    pub fn is_dirty(&self) -> bool {
        self.draft != self.saved
    }

    pub fn is_new(&self) -> bool {
        self.disk_contents.is_none()
    }

    pub fn save(&mut self) -> Result<()> {
        self.draft.validate()?;
        let text = toml::to_string_pretty(&self.draft)?;
        if read_contents(&self.path)? != self.disk_contents {
            bail!(
                "{} changed on disk. Reload it before saving; keep a copy of your edits first.",
                self.path.display()
            );
        }
        save_text(&self.path, &text)?;
        self.disk_contents = Some(text.into_bytes());
        self.saved = self.draft.clone();
        Ok(())
    }

    pub fn reload(&mut self) -> Result<()> {
        *self = Self::open_path(self.path.clone())?;
        Ok(())
    }
}

fn read_contents(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Could not read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(id: &str, x: i32, y: i32, width: u32, height: u32) -> Monitor {
        Monitor {
            id: id.into(),
            label: id.into(),
            peer: (id != "local").then(|| id.into()),
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn touching_edges_have_reciprocal_normalized_ranges() {
        let layout = Layout {
            monitors: vec![
                monitor("local", 0, 0, 100, 100),
                monitor("peer", 100, 50, 200, 100),
            ],
        };
        let links = layout.transitions();
        assert_eq!(
            links,
            vec![
                Transition {
                    source: 0,
                    target: 1,
                    edge: Edge::Right,
                    source_start: 0.5,
                    source_end: 1.0,
                    target_start: 0.0,
                    target_end: 0.5
                },
                Transition {
                    source: 1,
                    target: 0,
                    edge: Edge::Left,
                    source_start: 0.0,
                    source_end: 0.5,
                    target_start: 0.5,
                    target_end: 1.0
                },
            ]
        );
        let source_position = 0.75;
        let link = &links[0];
        let mapped = link.target_start
            + (source_position - link.source_start) / (link.source_end - link.source_start)
                * (link.target_end - link.target_start);
        assert_eq!(mapped, 0.25);
    }

    #[test]
    fn stacked_monitors_split_an_edge_and_skip_same_owner() {
        let mut layout = Layout {
            monitors: vec![
                monitor("local", 0, 0, 100, 100),
                monitor("a", 100, 0, 100, 50),
                monitor("b", 100, 50, 100, 50),
            ],
        };
        layout.monitors[2].peer = Some("a".into());
        let links = layout.transitions();
        assert_eq!(links.len(), 4);
        assert_eq!((links[0].source_start, links[0].source_end), (0.0, 0.5));
        assert_eq!((links[1].source_start, links[1].source_end), (0.5, 1.0));
        layout.monitors[1].peer = None;
        layout.monitors[2].peer = None;
        assert!(layout.transitions().is_empty());
    }

    #[test]
    fn vertical_links_and_gaps_and_corners() {
        let mut layout = Layout {
            monitors: vec![
                monitor("local", -50, -100, 100, 100),
                monitor("a", 0, 0, 100, 100),
            ],
        };
        let links = layout.transitions();
        assert_eq!(links[0].edge, Edge::Bottom);
        assert_eq!(links[1].edge, Edge::Top);
        assert_eq!(links[0].source_start, 0.5);
        layout.monitors[1].y = 1;
        assert!(layout.transitions().is_empty());
        layout.monitors[1].y = 0;
        layout.monitors[1].x = 50;
        assert!(layout.transitions().is_empty());
    }

    #[test]
    fn invalid_geometry_names_and_overlaps_fail_validation() {
        let good = Layout {
            monitors: vec![
                monitor("local", 0, 0, 100, 100),
                monitor("peer", 100, 0, 100, 100),
            ],
        };
        for bad in [
            Monitor {
                x: 99,
                ..good.monitors[1].clone()
            },
            Monitor {
                x: i32::MIN,
                ..good.monitors[1].clone()
            },
            Monitor {
                width: u32::MAX,
                ..good.monitors[1].clone()
            },
            Monitor {
                height: 0,
                ..good.monitors[1].clone()
            },
            Monitor {
                id: "local".into(),
                ..good.monitors[1].clone()
            },
            Monitor {
                label: " ".into(),
                ..good.monitors[1].clone()
            },
            Monitor {
                peer: Some("".into()),
                ..good.monitors[1].clone()
            },
        ] {
            let layout = Layout {
                monitors: vec![good.monitors[0].clone(), bad],
            };
            assert!(layout.validate().is_err());
            assert!(layout.transitions().is_empty());
        }
        let mut same_labels = good;
        same_labels.monitors[1].label = "local".into();
        same_labels.validate().unwrap();
    }

    #[test]
    fn snapping_uses_nearest_edge_and_rejects_overlap() {
        let layout = Layout {
            monitors: vec![
                monitor("local", 0, 0, 100, 100),
                monitor("peer", 200, 0, 100, 100),
            ],
        };
        assert_eq!(layout.snap_move(1, 106, 20, 10), Some((100, 20)));
        assert_eq!(layout.snap_move(1, -108, 20, 10), Some((-100, 20)));
        assert_eq!(layout.snap_move(1, 20, 108, 10), Some((20, 100)));
        assert_eq!(layout.snap_move(1, 20, -108, 10), Some((20, -100)));
        assert_eq!(layout.snap_move(1, 96, 20, 10), Some((100, 20)));
        assert_eq!(layout.snap_move(1, 50, 0, 10), None);
        assert_eq!(layout.snap_move(1, 200, 200, 10), Some((200, 200)));
        assert_eq!(layout.snap_move(1, i32::MAX, 0, 10), None);
        assert_eq!(layout.snap_move(1, i32::MIN, i32::MIN, 10), None);
        assert_eq!(layout.snap_move(2, 0, 0, 10), None);
        assert_eq!(layout.snap_move(1, 100, 0, -1), None);
    }

    #[test]
    fn sidecar_is_explicit_and_round_trips_without_touching_config() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("new/zflow.toml");
        let mut document = LayoutDocument::open(&config).unwrap();
        assert!(document.is_new());
        assert!(!document.is_dirty());
        assert!(!config.parent().unwrap().exists());
        document
            .draft
            .monitors
            .push(monitor("revoked-peer", 0, 0, 100, 100));
        assert!(document.is_dirty());
        document.save().unwrap();
        assert_eq!(document.path.file_name().unwrap(), "zflow.toml.layout.toml");
        assert!(!config.exists());
        assert!(!document.is_new());
        assert!(!document.is_dirty());
        assert_eq!(LayoutDocument::open(&config).unwrap().draft, document.draft);
        document.draft.monitors.clear();
        document.reload().unwrap();
        assert_eq!(document.draft.monitors.len(), 1);
    }

    #[test]
    fn external_create_edit_delete_and_malformed_reload_preserve_draft() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("zflow.toml");
        let mut document = LayoutDocument::open(&config).unwrap();
        fs::write(&document.path, "monitors = []\n").unwrap();
        assert!(document.save().is_err());
        document.reload().unwrap();
        document
            .draft
            .monitors
            .push(monitor("local", 0, 0, 100, 100));
        fs::write(&document.path, "monitors = [unfinished").unwrap();
        assert!(LayoutDocument::open(&config).is_err());
        assert!(document.reload().is_err());
        assert!(document.save().is_err());
        assert_eq!(document.draft.monitors.len(), 1);
        fs::remove_file(&document.path).unwrap();
        assert!(document.save().is_err());
        assert!(!document.path.exists());
    }

    #[test]
    fn invalid_save_preserves_existing_contents() {
        let directory = tempfile::tempdir().unwrap();
        let mut document = LayoutDocument::open(&directory.path().join("zflow.toml")).unwrap();
        document.save().unwrap();
        let before = fs::read(&document.path).unwrap();
        document.draft.monitors.push(monitor("local", 0, 0, 0, 100));
        assert!(document.save().is_err());
        assert_eq!(fs::read(&document.path).unwrap(), before);
    }
}
