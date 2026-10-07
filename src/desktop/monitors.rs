//! Monitor identity and desk placement are separate from OS cursor coordinates.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Display {
    /// Platform identity, scoped to the computer. Never an enumeration index.
    pub id: String,
    pub name: String,
    pub bounds: Rect,
    /// Zero when the platform cannot supply a reliable physical size.
    #[serde(default)]
    pub width_mm: u32,
    #[serde(default)]
    pub height_mm: u32,
    #[serde(default = "active")]
    pub active: bool,
}

fn active() -> bool {
    true
}
pub(super) fn valid_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= 128 && !id.chars().any(char::is_control)
}

impl Display {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            valid_id(&self.id) && valid_id(&self.name),
            "Invalid monitor identity"
        );
        ensure!(
            (self.width_mm == 0 && self.height_mm == 0)
                || ((10..=4000).contains(&self.width_mm) && (10..=4000).contains(&self.height_mm)),
            "Invalid physical monitor size"
        );
        Geometry {
            monitors: vec![self.bounds],
            displays: Vec::new(),
        }
        .validate()
    }

    /// About 100 desk units per inch. Unknown sizes retain logical proportions.
    pub fn desk_size(&self) -> (u32, u32) {
        if self.width_mm > 0 && self.height_mm > 0 {
            (self.width_mm * 4, self.height_mm * 4)
        } else {
            (self.bounds.width, self.bounds.height)
        }
    }
}

impl Geometry {
    pub(super) fn validate_displays(&self) -> Result<()> {
        if self.displays.is_empty() {
            return Ok(());
        }
        ensure!(
            self.displays.len() == self.monitors.len(),
            "Incomplete monitor identities"
        );
        let mut ids = std::collections::BTreeSet::new();
        let mut rectangles = Vec::new();
        for display in &self.displays {
            display.validate()?;
            ensure!(
                display.active
                    && ids.insert(&display.id)
                    && self.monitors.contains(&display.bounds)
                    && !rectangles.contains(&display.bounds),
                "Invalid or repeated active monitor"
            );
            rectangles.push(display.bounds);
        }
        Ok(())
    }

    /// Legacy requests address the whole desktop; new ones name one active display.
    pub fn for_monitor(&self, id: Option<&str>) -> Result<Self> {
        self.validate()?;
        let Some(id) = id else {
            return Ok(self.clone());
        };
        let display = self
            .displays
            .iter()
            .find(|d| d.id == id && d.active)
            .context("The selected monitor is disconnected; refresh the screen arrangement")?;
        Ok(Self {
            monitors: vec![display.bounds],
            displays: vec![display.clone()],
        })
    }
}

impl SharedLayout {
    /// Reconcile only this computer's detected displays. Unplugged displays keep
    /// their saved positions but cannot participate in crossings.
    pub fn with_geometry(&self, key: &str, geometry: &Geometry) -> Option<Self> {
        geometry.validate().ok()?;
        if geometry.displays.is_empty() {
            let bounds = geometry.bounds().ok()?;
            return self.with_own_size(key, bounds.width, bounds.height);
        }
        let mut next = self.clone();
        for tile in next.tiles.iter_mut().filter(|t| t.key == key) {
            if let Some(old) = &mut tile.display {
                old.active = geometry.displays.iter().any(|d| d.id == old.id);
            }
        }
        // Dormant placements must not push active screens away or block a replug.
        let dormant: Vec<_> = next
            .tiles
            .iter()
            .filter(|t| t.display.as_ref().is_some_and(|d| !d.active))
            .cloned()
            .collect();
        next.tiles
            .retain(|t| t.display.as_ref().is_none_or(|d| d.active));

        // Split the old bounding rectangle in-place before resizing. This retains
        // the outside contacts and the OS's ordering on the first upgrade.
        if let Some(index) = next
            .tiles
            .iter()
            .position(|t| t.key == key && t.display.is_none())
        {
            let old = next.tiles.remove(index);
            let bounds = geometry.bounds().ok()?;
            let sx = f64::from(old.width) / f64::from(bounds.width);
            let sy = f64::from(old.height) / f64::from(bounds.height);
            for display in &geometry.displays {
                let rect = display.bounds;
                let x = ((rect.x - bounds.x) as f64 * sx).round() as i32;
                let y = ((rect.y - bounds.y) as f64 * sy).round() as i32;
                let right =
                    ((i64::from(rect.x) + i64::from(rect.width) - i64::from(bounds.x)) as f64 * sx)
                        .round() as i32;
                let bottom = ((i64::from(rect.y) + i64::from(rect.height) - i64::from(bounds.y))
                    as f64
                    * sy)
                    .round() as i32;
                next.tiles.push(Tile {
                    key: key.to_owned(),
                    display: Some(display.clone()),
                    x: old.x.checked_add(x)?,
                    y: old.y.checked_add(y)?,
                    width: (right - x).max(1) as u32,
                    height: (bottom - y).max(1) as u32,
                });
            }
        }

        for display in &geometry.displays {
            if let Some(index) = next.tiles.iter().position(|t| {
                t.key == key && t.display.as_ref().is_some_and(|d| d.id == display.id)
            }) {
                next.tiles[index].display = Some(display.clone());
                // A returning monitor keeps its spot if still free. If another
                // screen now occupies it, find a free edge before resizing.
                if next
                    .tiles
                    .iter()
                    .enumerate()
                    .any(|(i, t)| i != index && overlaps(&next.tiles[index], t))
                {
                    let old = next.tiles.remove(index);
                    let mut placed = free_spot(
                        &next.tiles,
                        next.tiles.first(),
                        key,
                        (old.width, old.height),
                    )?;
                    placed.display = Some(display.clone());
                    next.tiles.push(placed);
                }
                let (width, height) = display.desk_size();
                if let Some(resized) = next.with_display_size(key, Some(&display.id), width, height)
                {
                    next = resized;
                }
            } else {
                let anchor = next
                    .tiles
                    .iter()
                    .find(|t| t.key == key)
                    .or_else(|| next.tiles.first());
                let mut placed = free_spot(&next.tiles, anchor, key, display.desk_size())?;
                placed.display = Some(display.clone());
                next.tiles.push(placed);
            }
        }
        next.tiles.extend(dormant);
        // Stable ordering also avoids publishing a new version at every poll.
        next.tiles.sort_by(|a, b| {
            (&a.key, a.display.as_ref().map(|d| &d.id))
                .cmp(&(&b.key, b.display.as_ref().map(|d| &d.id)))
        });
        let mut before = self.tiles.clone();
        before.sort_by(|a, b| {
            (&a.key, a.display.as_ref().map(|d| &d.id))
                .cmp(&(&b.key, b.display.as_ref().map(|d| &d.id)))
        });
        if next.tiles == before {
            return None;
        }
        next.version = self.version.checked_add(1)?;
        next.editor = key.to_owned();
        next.validate().ok().map(|()| next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::layout_model::Layout;
    use std::collections::BTreeMap;

    fn display(id: &str, x: i32, y: i32, width: u32, height: u32) -> Display {
        Display {
            id: id.into(),
            name: id.into(),
            bounds: Rect {
                x,
                y,
                width,
                height,
            },
            width_mm: 0,
            height_mm: 0,
            active: true,
        }
    }
    fn geometry(displays: Vec<Display>) -> Geometry {
        Geometry {
            monitors: displays.iter().map(|d| d.bounds).collect(),
            displays,
        }
    }
    fn key(n: u8) -> String {
        format!("{n:064x}")
    }
    fn legacy() -> SharedLayout {
        serde_json::from_value(serde_json::json!({"version":9,"editor":key(1),"tiles":[
            {"key":key(1),"x":0,"y":0,"width":3840,"height":1080},
            {"key":key(2),"x":3840,"y":0,"width":1920,"height":1080}
        ]}))
        .unwrap()
    }

    #[test]
    fn upgrade_splits_the_old_tile_without_moving_its_outside_neighbor() {
        let g = geometry(vec![
            display("left", -1920, 0, 1920, 1080),
            display("right", 0, 0, 1920, 1080),
        ]);
        let next = legacy().with_geometry(&key(1), &g).unwrap();
        assert_eq!(next.version, 10);
        assert_eq!(next.tiles.len(), 3);
        let view = Layout::from_shared(
            &next,
            &key(1),
            "This PC",
            &BTreeMap::from([("mac".into(), key(2))]),
        );
        view.validate().unwrap();
        assert_eq!(view.monitors.iter().filter(|m| m.peer.is_none()).count(), 2);
        let right = view
            .monitors
            .iter()
            .position(|m| m.display.as_ref().is_some_and(|d| d.id == "right"))
            .unwrap();
        assert!(
            view.transitions()
                .iter()
                .any(|t| t.source == right && t.edge == Edge::Right)
        );
        assert!(
            next.with_geometry(&key(1), &g).is_none(),
            "polling must not rewrite the arrangement"
        );
        let mut reordered = g.clone();
        reordered.displays.reverse();
        reordered.monitors.reverse();
        assert!(
            next.with_geometry(&key(1), &reordered).is_none(),
            "enumeration order is not identity"
        );
    }

    #[test]
    fn fitting_this_computer_keeps_the_tiles_of_computers_it_has_not_paired() {
        // This computer, key 1, has paired the Mac, key 2, but not key 3,
        // which the Mac placed below itself, with a monitor now unplugged.
        let mut layout = legacy();
        let theirs = |x, y, display| Tile {
            key: key(3),
            display,
            x,
            y,
            width: 1920,
            height: 1080,
        };
        let mut unplugged = display("old", 0, 0, 1920, 1080);
        unplugged.active = false;
        layout.tiles.push(theirs(3840, 1080, None));
        layout.tiles.push(theirs(9000, 0, Some(unplugged)));
        let split = geometry(vec![
            display("left", -1920, 0, 1920, 1080),
            display("right", 0, 0, 1920, 1080),
        ]);
        let resized = Geometry {
            monitors: vec![Rect {
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
            }],
            displays: Vec::new(),
        };
        for geometry in [split, resized] {
            let next = layout.with_geometry(&key(1), &geometry).unwrap();
            next.validate().unwrap();
            assert_eq!(
                next.tiles.iter().filter(|tile| tile.key == key(3)).count(),
                2,
                "an edit that only fits this computer keeps the others' tiles"
            );
        }
    }

    #[test]
    fn physical_size_is_independent_of_resolution_and_disabled_positions_survive_edits() {
        let mut left = display("dell", 0, 0, 3840, 2160);
        left.width_mm = 600;
        left.height_mm = 340;
        let mut right = display("samsung", 3840, 0, 3840, 2160);
        right.width_mm = 700;
        right.height_mm = 390;
        let g = geometry(vec![left.clone(), right.clone()]);
        let next = legacy().with_geometry(&key(1), &g).unwrap();
        let dell = next
            .tiles
            .iter()
            .find(|t| t.display.as_ref().is_some_and(|d| d.id == "dell"))
            .unwrap()
            .clone();
        assert_eq!((dell.width, dell.height), (2400, 1360));
        let unplugged = next
            .with_geometry(&key(1), &geometry(vec![right.clone()]))
            .unwrap();
        let view = Layout::from_shared(
            &unplugged,
            &key(1),
            "This PC",
            &BTreeMap::from([("mac".into(), key(2))]),
        );
        assert_eq!(view.monitors.iter().filter(|m| m.active()).count(), 2);
        let saved = view.to_shared(
            unplugged.version + 1,
            &key(1),
            &BTreeMap::from([("mac".into(), key(2))]),
        );
        let replugged = saved.with_geometry(&key(1), &g).unwrap();
        assert_eq!(
            replugged
                .tiles
                .iter()
                .find(|t| t.display.as_ref().is_some_and(|d| d.id == "dell")),
            Some(&dell)
        );
        left.bounds.width = 1920;
        left.bounds.height = 1080;
        let scaled = replugged
            .with_geometry(&key(1), &geometry(vec![left, right]))
            .unwrap();
        let changed = scaled
            .tiles
            .iter()
            .find(|t| t.display.as_ref().is_some_and(|d| d.id == "dell"))
            .unwrap();
        assert_eq!(
            (changed.x, changed.y, changed.width, changed.height),
            (dell.x, dell.y, dell.width, dell.height)
        );
    }

    #[test]
    fn identities_are_scoped_to_a_computer_and_unknown_or_duplicate_displays_fail() {
        let g = geometry(vec![display("panel", 0, 0, 1920, 1080)]);
        assert!(g.for_monitor(Some("unplugged")).is_err());
        let next = legacy()
            .with_geometry(&key(1), &g)
            .unwrap()
            .with_geometry(&key(2), &g)
            .unwrap();
        next.validate().unwrap();
        let mut duplicate = g.clone();
        duplicate.displays.push(g.displays[0].clone());
        duplicate.monitors.push(g.monitors[0]);
        assert!(duplicate.validate().is_err());
        let mut bad = g;
        bad.displays[0].bounds.x = 99;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn maximum_monitor_layout_fits_the_wire_limit() {
        let display = display(&"x".repeat(128), -65536, -65536, 16384, 16384);
        let layout = SharedLayout {
            version: MAX_TOKEN,
            editor: key(1),
            tiles: (0..MAX_SHARED_TILES)
                .map(|n| Tile {
                    key: format!("{n:064x}"),
                    display: Some(Display {
                        name: "\\\"".repeat(64),
                        ..display.clone()
                    }),
                    x: -100000,
                    y: 100000,
                    width: 16384,
                    height: 16384,
                })
                .collect(),
        };
        let message = DesktopMessage::Layout { layout };
        message.validate().unwrap();
        assert!(serde_json::to_vec(&message).unwrap().len() < MAX_MESSAGE_BYTES);
    }
}
