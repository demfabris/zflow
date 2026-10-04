//! Where a crossing enters the other computer and where it comes back.
//! The Mac finds the edge by watching its cursor and its own pointer's
//! pushes; Linux by a GNOME barrier.

use anyhow::{Result, bail, ensure};

use crate::desktop::{
    DesktopRequest, DesktopResponse, Edge, FRACTION_MAX, Geometry, MAX_TOKEN, Point, Rect,
    ReturnMapping,
};

use super::layout_model::{Layout, Transition};

#[derive(Clone, Debug)]
pub(crate) struct Handoff {
    pub peer: String,
    pub monitor: Option<String>,
    pub source_geometry: Geometry,
    pub edge: Edge,
    pub start: u32,
    pub end: u32,
    pub position: u32,
    pub expected_width: u32,
    pub expected_height: u32,
    /// Where the Mac's cursor may travel while the crossing is prepared.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub entry_region: Rect,
    pub return_mapping: ReturnMapping,
}

impl Handoff {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn matches_geometry(&self, geometry: &Geometry) -> bool {
        self.source_geometry == *geometry
    }

    /// Asks the other computer to put its cursor at the entry point.
    pub fn prepare(&self, token: u64) -> DesktopRequest {
        DesktopRequest::Prepare {
            monitor: self.monitor.clone(),
            token,
            edge: self.edge,
            start: self.start,
            end: self.end,
            position: self.position,
        }
    }

    /// Checks that the other computer prepared a desktop of the size the
    /// layout expects, so the crossing lands where the layout shows it.
    pub fn check_prepared(&self, response: DesktopResponse) -> Result<()> {
        response.validate()?;
        match response {
            DesktopResponse::Prepared { geometry, .. } => {
                let bounds = geometry.for_monitor(self.monitor.as_deref())?.bounds()?;
                ensure!(
                    bounds.width == self.expected_width && bounds.height == self.expected_height,
                    "the other computer's desktop changed size; refresh and save the computer layout before sharing"
                );
                Ok(())
            }
            DesktopResponse::Unavailable { reason } => {
                bail!("the other computer's desktop is unavailable: {reason}")
            }
            _ => bail!("the other computer did not prepare its desktop for input"),
        }
    }
}

/// A token for one crossing's desktop requests.
pub(crate) fn token() -> Result<u64> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("could not create a desktop handoff token: {error}"))?;
    Ok((u64::from_ne_bytes(random) & MAX_TOKEN).max(1))
}

/// Checks the other computer's answer to Finish.
pub(crate) fn check_finished(response: Result<DesktopResponse>) -> Result<()> {
    match response? {
        DesktopResponse::Finished => Ok(()),
        DesktopResponse::Unavailable { reason } => {
            bail!("the other computer could not finish the desktop handoff: {reason}")
        }
        _ => bail!("the other computer did not confirm the desktop handoff cleanup"),
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn validate(layout: &Layout, geometry: &Geometry) -> Result<()> {
    layout.validate()?;
    geometry.validate()?;
    let locals: Vec<_> = layout
        .monitors
        .iter()
        .filter(|m| m.peer.is_none() && m.active())
        .collect();
    ensure!(!locals.is_empty(), "The layout needs a local monitor");
    for local in locals {
        let actual = geometry
            .for_monitor(local.display.as_ref().map(|d| d.id.as_str()))?
            .bounds()?;
        let expected = local.display.as_ref().map(|d| d.bounds);
        ensure!(
            expected.map_or(
                actual.width == local.width && actual.height == local.height,
                |r| r == actual
            ),
            "This computer's monitors changed. Waiting for its updated layout"
        );
    }
    ensure!(
        layout
            .transitions()
            .iter()
            .any(|t| layout.monitors[t.source].peer.is_none()),
        "Drag a paired computer until its edge touches this computer"
    );
    Ok(())
}

/// The crossing for a push against `edge` at `position`, a fraction of this
/// desktop along that edge, as a GNOME barrier reports it.
#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
pub(crate) fn from_edge(
    layout: &Layout,
    geometry: &Geometry,
    monitor: Option<&str>,
    edge: Edge,
    position: u32,
) -> Option<Handoff> {
    let bounds = geometry.for_monitor(monitor).ok()?.bounds().ok()?;
    let along = f64::from(position.min(FRACTION_MAX)) / f64::from(FRACTION_MAX);
    let offset = |span: u32| (along * f64::from(span)).floor().min(f64::from(span - 1)) as i32;
    let current = match edge {
        Edge::Left => Point {
            x: bounds.x,
            y: bounds.y + offset(bounds.height),
        },
        Edge::Right => Point {
            x: bounds.x + bounds.width as i32 - 1,
            y: bounds.y + offset(bounds.height),
        },
        Edge::Top => Point {
            x: bounds.x + offset(bounds.width),
            y: bounds.y,
        },
        Edge::Bottom => Point {
            x: bounds.x + offset(bounds.width),
            y: bounds.y + bounds.height as i32 - 1,
        },
    };
    layout
        .transitions()
        .iter()
        .filter(|t| layout.monitors[t.source].peer.is_none() && t.edge == edge)
        .filter(|t| {
            layout.monitors[t.source]
                .display
                .as_ref()
                .map(|d| d.id.as_str())
                == monitor
        })
        .find(|t| (t.source_start..=t.source_end).contains(&along))
        .and_then(|transition| handoff_at(layout, geometry, transition, current))
}

/// How far from the desktop's corners a crossing stops, so a push into a
/// corner, such as a hot corner, stays here. GNOME's barriers stop as far.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const DEAD_CORNER: i32 = 8;

/// The crossing for a pointer that moves from `previous` onto an edge at
/// `current`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn crossing(
    layout: &Layout,
    geometry: &Geometry,
    previous: Point,
    current: Point,
) -> Option<Handoff> {
    if !contains(geometry, previous) {
        return None;
    }
    reaching(layout, geometry, current, |bounds, edge| {
        bounds.contains(previous) && !touches(bounds, edge, previous)
    })
}

/// The crossing for a pointer held on an edge at `current` while it pushes
/// on by `dx` and `dy`. macOS keeps reporting motion when the cursor cannot
/// move, as GNOME reports a push against a barrier.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn pushed(
    layout: &Layout,
    geometry: &Geometry,
    current: Point,
    dx: f64,
    dy: f64,
) -> Option<Handoff> {
    reaching(layout, geometry, current, |_, edge| match edge {
        Edge::Left => dx < 0.0,
        Edge::Right => dx > 0.0,
        Edge::Top => dy < 0.0,
        Edge::Bottom => dy > 0.0,
    })
}

/// The crossing for a pointer at `current` on this desktop's `edge`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn on_edge(
    layout: &Layout,
    geometry: &Geometry,
    edge: Edge,
    current: Point,
) -> Option<Handoff> {
    reaching(layout, geometry, current, |_, wanted| wanted == edge)
}

/// Whether `point` is on any of this desktop's outer edges.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn on_any_edge(geometry: &Geometry, point: Point) -> bool {
    geometry.monitors.iter().any(|bounds| {
        [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom]
            .into_iter()
            .any(|edge| bounds.contains(point) && touches(bounds, edge, point))
    })
}

/// The crossing for a pointer at `current` on one of the `wanted` edges,
/// clear of the corners, where the layout puts another computer.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn reaching(
    layout: &Layout,
    geometry: &Geometry,
    current: Point,
    wanted: impl Fn(&Rect, Edge) -> bool,
) -> Option<Handoff> {
    if !contains(geometry, current) {
        return None;
    }
    for transition in layout.transitions() {
        let edge = transition.edge;
        let source = &layout.monitors[transition.source];
        if source.peer.is_some() {
            continue;
        }
        let Ok(selected) = geometry.for_monitor(source.display.as_ref().map(|d| d.id.as_str()))
        else {
            continue;
        };
        let bounds = selected.bounds().ok()?;
        if !bounds.contains(current) || !wanted(&bounds, edge) || !touches(&bounds, edge, current) {
            continue;
        }
        let (offset, span) = match edge {
            Edge::Left | Edge::Right => (current.y - bounds.y, bounds.height as i32),
            Edge::Top | Edge::Bottom => (current.x - bounds.x, bounds.width as i32),
        };
        let along = f64::from(offset) / f64::from(span);
        if offset < DEAD_CORNER
            || offset >= span - DEAD_CORNER
            || along < transition.source_start
            || along >= transition.source_end
        {
            continue;
        }
        return handoff_at(layout, geometry, &transition, current);
    }
    None
}

/// Whether `point` is on `edge`'s outermost two points.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn touches(bounds: &Rect, edge: Edge, point: Point) -> bool {
    match edge {
        Edge::Left => point.x <= bounds.x + 1,
        Edge::Right => point.x >= bounds.x + bounds.width as i32 - 2,
        Edge::Top => point.y <= bounds.y + 1,
        Edge::Bottom => point.y >= bounds.y + bounds.height as i32 - 2,
    }
}

/// The crossing through `transition` for a cursor at `current` on its edge.
fn handoff_at(
    layout: &Layout,
    geometry: &Geometry,
    transition: &Transition,
    current: Point,
) -> Option<Handoff> {
    let peer = layout.monitors[transition.target].peer.as_ref()?;
    let source = &layout.monitors[transition.source];
    let target = &layout.monitors[transition.target];
    let selected = geometry
        .for_monitor(source.display.as_ref().map(|d| d.id.as_str()))
        .ok()?;
    let return_mapping = ReturnMapping {
        edge: transition.edge,
        local_start: transition.source_start,
        local_end: transition.source_end,
        remote_start: transition.target_start,
        remote_end: transition.target_end,
        geometry: selected.clone(),
    };
    Some(Handoff {
        peer: peer.clone(),
        monitor: target.display.as_ref().map(|d| d.id.clone()),
        source_geometry: geometry.clone(),
        edge: opposite(transition.edge),
        start: fraction(transition.target_start),
        end: fraction(transition.target_end),
        position: return_mapping.fraction(current).ok()?,
        expected_width: target
            .display
            .as_ref()
            .map_or(target.width, |d| d.bounds.width),
        expected_height: target
            .display
            .as_ref()
            .map_or(target.height, |d| d.bounds.height),
        entry_region: entry_region(
            &selected,
            transition.edge,
            transition.source_start,
            transition.source_end,
            current,
        )?,
        return_mapping,
    })
}

fn entry_region(
    geometry: &Geometry,
    edge: Edge,
    start: f64,
    end: f64,
    entry: Point,
) -> Option<Rect> {
    let bounds = geometry.bounds().ok()?;
    let monitor = geometry
        .monitors
        .iter()
        .find(|monitor| monitor.contains(entry))?;
    // Keep the original eight-pixel inward allowance, but let the pointer
    // travel along the connected part of this monitor during preparation.
    let vertical = matches!(edge, Edge::Left | Edge::Right);
    let span = if vertical {
        bounds.height
    } else {
        bounds.width
    };
    let along_start = (start * f64::from(span)).round() as i32;
    let along_end = (end * f64::from(span)).round() as i32;
    let depth = 9.min(if vertical {
        bounds.width
    } else {
        bounds.height
    }) as i32;
    let (x, y, right, bottom) = match edge {
        Edge::Left => (
            bounds.x,
            bounds.y + along_start,
            bounds.x + depth,
            bounds.y + along_end,
        ),
        Edge::Right => (
            bounds.x + bounds.width as i32 - depth,
            bounds.y + along_start,
            bounds.x + bounds.width as i32,
            bounds.y + along_end,
        ),
        Edge::Top => (
            bounds.x + along_start,
            bounds.y,
            bounds.x + along_end,
            bounds.y + depth,
        ),
        Edge::Bottom => (
            bounds.x + along_start,
            bounds.y + bounds.height as i32 - depth,
            bounds.x + along_end,
            bounds.y + bounds.height as i32,
        ),
    };
    let x = x.max(monitor.x);
    let y = y.max(monitor.y);
    let right = right.min(monitor.x + monitor.width as i32);
    let bottom = bottom.min(monitor.y + monitor.height as i32);
    (right > x && bottom > y).then_some(Rect {
        x,
        y,
        width: (right - x) as u32,
        height: (bottom - y) as u32,
    })
}

fn fraction(value: f64) -> u32 {
    (value.clamp(0.0, 1.0) * f64::from(FRACTION_MAX)).round() as u32
}

fn opposite(edge: Edge) -> Edge {
    match edge {
        Edge::Left => Edge::Right,
        Edge::Right => Edge::Left,
        Edge::Top => Edge::Bottom,
        Edge::Bottom => Edge::Top,
    }
}

fn contains(geometry: &Geometry, point: Point) -> bool {
    geometry
        .monitors
        .iter()
        .any(|monitor| monitor.contains(point))
}

#[cfg(test)]
mod tests {
    use super::super::layout_model::Monitor;
    use super::*;

    fn named(id: &str, bounds: Rect) -> crate::desktop::Display {
        crate::desktop::Display {
            id: id.into(),
            name: id.into(),
            bounds,
            width_mm: 0,
            height_mm: 0,
            active: true,
        }
    }

    #[test]
    fn each_monitor_maps_its_own_partial_edge_across_different_scaling() {
        let left = named(
            "left",
            Rect {
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );
        let right = named(
            "right",
            Rect {
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
            },
        );
        let mac = named(
            "mac-panel",
            Rect {
                x: 4000,
                y: -800,
                width: 3008,
                height: 1692,
            },
        );
        let tile =
            |display: crate::desktop::Display, peer: Option<&str>, x, y, width, height| Monitor {
                id: display.id.clone(),
                label: display.name.clone(),
                peer: peer.map(str::to_owned),
                display: Some(display),
                x,
                y,
                width,
                height,
            };
        let layout = Layout {
            monitors: vec![
                tile(left.clone(), None, 0, 0, 2400, 1360),
                tile(right.clone(), None, 2400, 0, 2800, 1560),
                tile(mac.clone(), Some("mac"), -600, 1360, 3000, 1600),
            ],
        };
        let geometry = Geometry {
            monitors: vec![left.bounds, right.bounds],
            displays: vec![left, right],
        };
        validate(&layout, &geometry).unwrap();
        let h = crossing(
            &layout,
            &geometry,
            Point { x: -960, y: 1070 },
            Point { x: -960, y: 1079 },
        )
        .unwrap();
        assert_eq!(h.monitor.as_deref(), Some("mac-panel"));
        assert_eq!((h.start, h.end, h.position), (200000, 1000000, 600000));
        assert_eq!((h.expected_width, h.expected_height), (3008, 1692));
        assert_eq!(
            h.return_mapping.position(600000).unwrap(),
            Point { x: -960, y: 1076 }
        );
        assert!(h.matches_geometry(&geometry));
        assert_eq!(
            from_edge(&layout, &geometry, Some("left"), Edge::Bottom, 500000)
                .unwrap()
                .position,
            h.position
        );
        assert!(from_edge(&layout, &geometry, Some("right"), Edge::Bottom, 500000).is_none());
        let prepared = Geometry {
            monitors: vec![
                mac.bounds,
                Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            ],
            displays: vec![
                mac.clone(),
                named(
                    "other",
                    Rect {
                        x: 0,
                        y: 0,
                        width: 1920,
                        height: 1080,
                    },
                ),
            ],
        };
        h.check_prepared(DesktopResponse::Prepared {
            geometry: prepared.clone(),
            position: Point { x: 5804, y: -797 },
        })
        .unwrap();
        let mut removed = prepared;
        removed.monitors.remove(0);
        removed.displays.remove(0);
        assert!(
            h.check_prepared(DesktopResponse::Prepared {
                geometry: removed,
                position: Point { x: 3, y: 500 }
            })
            .is_err()
        );
        h.prepare(1).validate().unwrap();
    }

    #[test]
    fn arranged_remote_monitor_takes_priority_over_a_native_local_neighbor() {
        let left = named(
            "left",
            Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );
        let mut right = named(
            "right",
            Rect {
                x: 1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );
        let mut layout = Layout {
            monitors: vec![
                Monitor {
                    id: "left".into(),
                    label: "left".into(),
                    peer: None,
                    display: Some(left.clone()),
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
                Monitor {
                    id: "right".into(),
                    label: "right".into(),
                    peer: None,
                    display: Some(right.clone()),
                    x: 8000,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
                Monitor {
                    id: "mac".into(),
                    label: "mac".into(),
                    peer: Some("mac".into()),
                    display: None,
                    x: 1920,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            ],
        };
        assert_eq!(
            layout.transitions().len(),
            2,
            "both directions cross the native seam"
        );
        right.bounds.height = 540;
        layout.monitors[1].display = Some(right.clone());
        let geometry = Geometry {
            monitors: vec![left.bounds, right.bounds],
            displays: vec![left, right],
        };
        let internal = crossing(
            &layout,
            &geometry,
            Point { x: 1910, y: 200 },
            Point { x: 1919, y: 200 },
        )
        .unwrap();
        assert_eq!(internal.peer, "mac");
        assert_eq!(
            internal.return_mapping.position(internal.position).unwrap(),
            Point { x: 1916, y: 200 }
        );
        let h = crossing(
            &layout,
            &geometry,
            Point { x: 1910, y: 800 },
            Point { x: 1919, y: 800 },
        )
        .unwrap();
        assert_eq!(h.start, 0);
        assert_eq!(h.end, 1000000);
        // Gaps and same-computer neighbors continue to use native navigation.
        layout.monitors[2].x += 1;
        assert!(layout.transitions().is_empty());
        layout.monitors[2].x -= 1;
        layout.monitors[2].peer = None;
        assert!(layout.transitions().is_empty());
    }

    fn setup() -> (Layout, Geometry) {
        (
            Layout {
                monitors: vec![
                    Monitor {
                        display: None,
                        id: "mac".into(),
                        label: "Mac".into(),
                        peer: None,
                        x: 0,
                        y: 0,
                        width: 2000,
                        height: 1000,
                    },
                    Monitor {
                        display: None,
                        id: "ubuntu".into(),
                        label: "Ubuntu".into(),
                        peer: Some("ubuntu".into()),
                        x: 2000,
                        y: 500,
                        width: 1000,
                        height: 1000,
                    },
                ],
            },
            Geometry {
                displays: Vec::new(),
                monitors: vec![Rect {
                    x: -1000,
                    y: -200,
                    width: 2000,
                    height: 1000,
                }],
            },
        )
    }

    #[test]
    fn crossing_and_return_map_partial_edges_with_negative_desktop_origin() {
        let (layout, geometry) = setup();
        validate(&layout, &geometry).unwrap();
        let handoff = crossing(
            &layout,
            &geometry,
            Point { x: 990, y: 550 },
            Point { x: 999, y: 550 },
        )
        .unwrap();
        assert_eq!(handoff.peer, "ubuntu");
        assert_eq!(
            (handoff.expected_width, handoff.expected_height),
            (1000, 1000)
        );
        assert!(handoff.matches_geometry(&geometry));
        assert_eq!(handoff.edge, Edge::Left);
        assert_eq!(
            (handoff.start, handoff.end, handoff.position),
            (0, 500_000, 250_000)
        );
        assert_eq!(
            handoff.return_mapping.position(250_000).unwrap(),
            Point { x: 996, y: 550 }
        );
        assert!(handoff.return_mapping.position(500_001).is_err());
        assert_eq!(
            handoff.entry_region,
            Rect {
                x: 991,
                y: 300,
                width: 9,
                height: 500
            }
        );
        assert!(handoff.entry_region.contains(Point { x: 999, y: 539 }));
        assert!(!handoff.entry_region.contains(Point { x: 999, y: 299 }));
        assert!(!handoff.entry_region.contains(Point { x: 990, y: 550 }));
    }

    #[test]
    fn only_a_prepared_desktop_of_the_expected_size_and_a_finish_pass() {
        let (layout, geometry) = setup();
        let handoff = from_edge(&layout, &geometry, None, Edge::Right, 750_000).unwrap();
        let prepared = |width, height| DesktopResponse::Prepared {
            geometry: Geometry {
                displays: Vec::new(),
                monitors: vec![Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                }],
            },
            position: Point { x: 0, y: 0 },
        };
        handoff.check_prepared(prepared(1000, 1000)).unwrap();
        assert!(
            handoff.check_prepared(prepared(1200, 1000)).is_err(),
            "the desktop changed size"
        );
        assert!(handoff.check_prepared(DesktopResponse::Active).is_err());
        assert!(
            handoff
                .check_prepared(DesktopResponse::unavailable("locked"))
                .is_err()
        );
        check_finished(Ok(DesktopResponse::Finished)).unwrap();
        for response in [
            DesktopResponse::Active,
            DesktopResponse::unavailable("gone"),
        ] {
            assert!(check_finished(Ok(response)).is_err());
        }
        assert!(check_finished(Err(anyhow::anyhow!("timed out"))).is_err());
    }

    #[test]
    fn an_edge_push_finds_the_same_crossing_as_the_cursor_does() {
        let (layout, geometry) = setup();
        let watched = crossing(
            &layout,
            &geometry,
            Point { x: 990, y: 550 },
            Point { x: 999, y: 550 },
        )
        .unwrap();
        // Ubuntu touches the lower half of the right edge; y 550 is 75% down.
        let pushed = from_edge(&layout, &geometry, None, Edge::Right, 750_000).unwrap();
        assert_eq!(pushed.peer, watched.peer);
        assert_eq!(
            (pushed.edge, pushed.start, pushed.end, pushed.position),
            (watched.edge, watched.start, watched.end, watched.position)
        );
        assert_eq!(
            pushed.return_mapping.position(pushed.position).unwrap(),
            Point { x: 996, y: 550 }
        );
        assert!(
            from_edge(&layout, &geometry, None, Edge::Right, 100_000).is_none(),
            "no computer there"
        );
        assert!(from_edge(&layout, &geometry, None, Edge::Left, 750_000).is_none());

        let prepared = |width, height| DesktopResponse::Prepared {
            geometry: Geometry {
                displays: Vec::new(),
                monitors: vec![Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                }],
            },
            position: Point { x: 3, y: 500 },
        };
        pushed.check_prepared(prepared(1000, 1000)).unwrap();
        assert!(
            pushed.check_prepared(prepared(1000, 900)).is_err(),
            "resized desktop"
        );
        assert!(
            pushed
                .check_prepared(DesktopResponse::unavailable("locked"))
                .unwrap_err()
                .to_string()
                .contains("locked")
        );
        assert!((1..=MAX_TOKEN).contains(&token().unwrap()));
        assert!(pushed.prepare(token().unwrap()).validate().is_ok());
    }

    #[test]
    fn a_push_out_through_a_held_edge_crosses_like_the_approach() {
        let (layout, geometry) = setup();
        let held = Point { x: 999, y: 550 };
        let watched = crossing(&layout, &geometry, Point { x: 990, y: 550 }, held).unwrap();
        let pushed = pushed(&layout, &geometry, held, 0.5, 3.0).unwrap();
        assert_eq!(
            (pushed.peer, pushed.edge, pushed.position),
            (watched.peer, watched.edge, watched.position)
        );
        assert_eq!(pushed.entry_region, watched.entry_region);
        assert_eq!(pushed.return_mapping.edge, Edge::Right);
        let push = |at, dx, dy| super::pushed(&layout, &geometry, at, dx, dy).is_some();
        assert!(!push(held, -1.0, 0.0), "back inside");
        assert!(!push(held, 0.0, 5.0), "along the edge");
        assert!(!push(Point { x: 990, y: 550 }, 4.0, 0.0), "not on the edge");
        assert!(
            !push(Point { x: 999, y: 100 }, 4.0, 0.0),
            "no computer there"
        );
        assert!(on_edge(&layout, &geometry, Edge::Right, held).is_some());
        assert!(on_edge(&layout, &geometry, Edge::Left, held).is_none());
    }

    #[test]
    fn the_desktop_corners_stay_here() {
        let (layout, geometry) = setup();
        // The Mac's lower right corner is at y 799; eight points up still
        // counts as the corner.
        let from = |y| Point { x: 990, y };
        let to = |y| Point { x: 999, y };
        assert!(crossing(&layout, &geometry, from(792), to(792)).is_none());
        assert!(pushed(&layout, &geometry, to(799), 1.0, 1.0).is_none());
        assert!(crossing(&layout, &geometry, from(791), to(791)).is_some());
        assert!(pushed(&layout, &geometry, to(791), 1.0, 0.0).is_some());

        // Every corner of a desktop with a computer all along each edge.
        let mut layout = layout;
        layout.monitors[0].width = 1000;
        let geometry = Geometry {
            displays: Vec::new(),
            monitors: vec![Rect {
                x: -200,
                y: -300,
                width: 1000,
                height: 1000,
            }],
        };
        // Where the other computer is, a corner, and the first point clear
        // of it.
        let cases = [
            ((-1000, 0), (-200, -300), (-200, -292)),
            ((1000, 0), (799, 699), (799, 691)),
            ((0, -1000), (799, -300), (791, -300)),
            ((0, 1000), (-200, 699), (-192, 699)),
        ];
        for ((x, y), corner, clear) in cases {
            layout.monitors[1].x = x;
            layout.monitors[1].y = y;
            let push = |(x, y)| {
                [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom]
                    .into_iter()
                    .any(|edge| on_edge(&layout, &geometry, edge, Point { x, y }).is_some())
            };
            assert!(!push(corner), "{corner:?}");
            assert!(push(clear), "{clear:?}");
        }
    }

    #[test]
    fn stationary_cursor_gaps_and_unshared_edge_do_not_activate() {
        let (layout, mut geometry) = setup();
        assert!(
            crossing(
                &layout,
                &geometry,
                Point { x: 999, y: 550 },
                Point { x: 999, y: 550 }
            )
            .is_none()
        );
        assert!(
            crossing(
                &layout,
                &geometry,
                Point { x: 990, y: 100 },
                Point { x: 999, y: 100 }
            )
            .is_none()
        );
        geometry.monitors = vec![
            Rect {
                x: -1000,
                y: -200,
                width: 1000,
                height: 1000,
            },
            Rect {
                x: 0,
                y: -200,
                width: 1000,
                height: 300,
            },
        ];
        assert!(
            crossing(
                &layout,
                &geometry,
                Point { x: 990, y: 550 },
                Point { x: 999, y: 550 }
            )
            .is_none()
        );
        geometry.monitors[0].width = 1999;
        geometry.monitors.pop();
        assert!(validate(&layout, &geometry).is_err());
    }

    #[test]
    fn each_orientation_enters_once_and_returns_inside_the_local_edge() {
        let cases = [
            (
                (-1000, 0),
                Point { x: -190, y: 200 },
                Point { x: -200, y: 200 },
                Edge::Right,
                Point { x: -197, y: 200 },
            ),
            (
                (1000, 0),
                Point { x: 790, y: 200 },
                Point { x: 799, y: 200 },
                Edge::Left,
                Point { x: 796, y: 200 },
            ),
            (
                (0, -1000),
                Point { x: 300, y: -290 },
                Point { x: 300, y: -300 },
                Edge::Bottom,
                Point { x: 300, y: -297 },
            ),
            (
                (0, 1000),
                Point { x: 300, y: 690 },
                Point { x: 300, y: 699 },
                Edge::Top,
                Point { x: 300, y: 696 },
            ),
        ];
        for ((x, y), previous, current, edge, returned) in cases {
            let (mut layout, _) = setup();
            layout.monitors[0].width = 1000;
            layout.monitors[1].x = x;
            layout.monitors[1].y = y;
            let geometry = Geometry {
                displays: Vec::new(),
                monitors: vec![Rect {
                    x: -200,
                    y: -300,
                    width: 1000,
                    height: 1000,
                }],
            };
            validate(&layout, &geometry).unwrap();
            let handoff = crossing(&layout, &geometry, previous, current).unwrap();
            assert_eq!(handoff.edge, edge);
            assert!(handoff.entry_region.contains(current));
            let along = match edge {
                Edge::Left | Edge::Right => Point {
                    y: current.y + 100,
                    ..current
                },
                Edge::Top | Edge::Bottom => Point {
                    x: current.x + 100,
                    ..current
                },
            };
            assert!(handoff.entry_region.contains(along));
            assert!(!handoff.entry_region.contains(previous));
            assert_eq!(
                handoff.return_mapping.position(handoff.position).unwrap(),
                returned
            );
            assert!(crossing(&layout, &geometry, current, previous).is_none());
        }
    }

    #[test]
    fn returning_into_a_missing_part_of_the_mac_desktop_moves_to_the_nearest_display() {
        let (mut layout, mut geometry) = setup();
        layout.monitors[1].y = 0;
        geometry.monitors = vec![
            Rect {
                x: -1000,
                y: -200,
                width: 1000,
                height: 1000,
            },
            Rect {
                x: 0,
                y: -200,
                width: 1000,
                height: 300,
            },
        ];
        let handoff = crossing(
            &layout,
            &geometry,
            Point { x: 990, y: 0 },
            Point { x: 999, y: 0 },
        )
        .unwrap();
        // Only the upper display touches the right edge, but the receiver's
        // barrier covers the whole shared range.
        assert_eq!(
            handoff.return_mapping.position(750_000).unwrap(),
            Point { x: 996, y: 99 }
        );
        assert_eq!(
            handoff.entry_region,
            Rect {
                x: 991,
                y: -200,
                width: 9,
                height: 300
            }
        );
        assert!(!handoff.entry_region.contains(Point { x: 999, y: 100 }));
    }
}
