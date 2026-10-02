use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::model::{Contents, Document};
use crate::desktop::{Edge, MAX_SHARED_TILES, SharedLayout, Tile};

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

    /// The shared form of this layout. `keys` maps each paired computer's
    /// name here to its key fingerprint. A tile for a computer that is not
    /// paired is left out, so a forgotten computer does not come back.
    pub fn to_shared(
        &self,
        version: u64,
        own: &str,
        keys: &BTreeMap<String, String>,
    ) -> SharedLayout {
        let mut seen = BTreeSet::new();
        let tiles = self
            .monitors
            .iter()
            .filter_map(|monitor| {
                let key = match &monitor.peer {
                    None => own.to_owned(),
                    Some(name) => keys.get(name)?.clone(),
                };
                // One tile per computer.
                seen.insert(key.clone()).then_some(Tile {
                    key,
                    x: monitor.x,
                    y: monitor.y,
                    width: monitor.width,
                    height: monitor.height,
                })
            })
            .take(MAX_SHARED_TILES)
            .collect();
        SharedLayout {
            version,
            editor: own.to_owned(),
            tiles,
        }
    }

    /// This computer's view of a shared layout: its own tile is the local
    /// one, and paired computers get their names here. Tiles of computers
    /// that are not paired here are left out.
    pub fn from_shared(
        shared: &SharedLayout,
        own: &str,
        own_label: &str,
        keys: &BTreeMap<String, String>,
    ) -> Self {
        let names: BTreeMap<&str, &str> = keys
            .iter()
            .map(|(name, key)| (key.as_str(), name.as_str()))
            .collect();
        let monitors = shared
            .tiles
            .iter()
            .filter_map(|tile| {
                let (id, label, peer) = if tile.key == own {
                    ("local".to_owned(), own_label.to_owned(), None)
                } else {
                    let name = *names.get(tile.key.as_str())?;
                    (
                        format!("peer:{name}"),
                        name.to_owned(),
                        Some(name.to_owned()),
                    )
                };
                Some(Monitor {
                    id,
                    label,
                    peer,
                    x: tile.x,
                    y: tile.y,
                    width: tile.width,
                    height: tile.height,
                })
            })
            .collect();
        Self { monitors }
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

pub type LayoutDocument = Document<Layout>;

impl Contents for Layout {
    const NAME: &'static str = "layout";

    fn missing(_: &Path) -> Self {
        Self::default()
    }

    fn check_read(&self) -> Result<()> {
        self.validate()
    }
}

impl LayoutDocument {
    /// Opens the layout saved beside the configuration as `NAME.layout.toml`.
    pub fn beside(config_path: &Path) -> Result<Self> {
        let mut file_name = config_path
            .file_name()
            .context("Configuration path needs a filename")?
            .to_os_string();
        file_name.push(".layout.toml");
        Self::open(config_path.with_file_name(file_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
    fn a_shared_layout_reads_the_same_arrangement_on_both_computers() {
        let (mac, ubuntu) = (format!("{:064x}", 1), format!("{:064x}", 2));
        // The Mac's layout, with a computer it paired and one it forgot.
        let on_mac = Layout {
            monitors: vec![
                monitor("local", 0, 0, 3008, 1692),
                monitor("ubuntu", 3008, 0, 2560, 1440),
                monitor("gone", -1000, 0, 1000, 800),
            ],
        };
        let mac_keys = BTreeMap::from([("ubuntu".to_owned(), ubuntu.clone())]);
        let shared = on_mac.to_shared(7, &mac, &mac_keys);
        shared.validate().unwrap();
        assert_eq!((shared.version, &shared.editor), (7, &mac));
        assert_eq!(shared.tiles.len(), 2, "the forgotten computer is left out");

        // Ubuntu calls the Mac "MacBook"; its own tile becomes the local one.
        let ubuntu_keys = BTreeMap::from([("MacBook".to_owned(), mac.clone())]);
        let on_ubuntu = Layout::from_shared(&shared, &ubuntu, "This computer", &ubuntu_keys);
        let local = on_ubuntu
            .monitors
            .iter()
            .find(|m| m.peer.is_none())
            .unwrap();
        assert_eq!(
            (local.id.as_str(), local.x, local.width),
            ("local", 3008, 2560)
        );
        let peer = on_ubuntu
            .monitors
            .iter()
            .find(|m| m.peer.is_some())
            .unwrap();
        assert_eq!(
            (peer.id.as_str(), peer.label.as_str(), peer.x),
            ("peer:MacBook", "MacBook", 0)
        );
        assert_eq!(
            on_ubuntu.transitions().len(),
            2,
            "one edge in each direction"
        );

        // A computer Ubuntu has not paired is not placed there.
        let stranger = Layout::from_shared(&shared, &ubuntu, "This computer", &BTreeMap::new());
        assert_eq!(stranger.monitors.len(), 1);
    }

    #[test]
    fn a_peers_layout_adds_no_trust() {
        // A peer placed a computer this one has only found on its shelf.
        let [own, desk, found] = [1, 2, 3].map(|n| format!("{n:064x}"));
        let tile = |key: &String, x| Tile {
            key: key.clone(),
            x,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let from_desk = SharedLayout {
            version: 4,
            editor: desk.clone(),
            tiles: vec![tile(&own, 0), tile(&desk, 1920), tile(&found, -1920)],
        };
        let keys = BTreeMap::from([("desk".to_owned(), desk)]);
        // Its tile is not shown here, so no crossing leads to it, and the
        // next layout this computer writes leaves it out.
        let here = Layout::from_shared(&from_desk, &own, "This computer", &keys);
        let ids: Vec<&str> = here.monitors.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["local", "peer:desk"]);
        let written = here.to_shared(5, &own, &keys);
        assert!(written.tiles.iter().all(|tile| tile.key != found));
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
        let mut document = LayoutDocument::beside(&config).unwrap();
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
        assert_eq!(
            LayoutDocument::beside(&config).unwrap().draft,
            document.draft
        );
        document.draft.monitors.clear();
        document.reload().unwrap();
        assert_eq!(document.draft.monitors.len(), 1);
    }

    #[test]
    fn external_create_edit_delete_and_malformed_reload_preserve_draft() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("zflow.toml");
        let mut document = LayoutDocument::beside(&config).unwrap();
        fs::write(&document.path, "monitors = []\n").unwrap();
        assert!(document.save().is_err());
        document.reload().unwrap();
        document
            .draft
            .monitors
            .push(monitor("local", 0, 0, 100, 100));
        fs::write(&document.path, "monitors = [unfinished").unwrap();
        assert!(LayoutDocument::beside(&config).is_err());
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
        let mut document = LayoutDocument::beside(&directory.path().join("zflow.toml")).unwrap();
        document.save().unwrap();
        let before = fs::read(&document.path).unwrap();
        document.draft.monitors.push(monitor("local", 0, 0, 0, 100));
        assert!(document.save().is_err());
        assert_eq!(fs::read(&document.path).unwrap(), before);
    }
}
