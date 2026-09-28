//! Where a crossing enters the other computer and where it comes back.
//! The Mac finds the edge by watching its cursor; Linux by a GNOME barrier.

use anyhow::{Result, bail, ensure};

use crate::desktop::{
    DesktopRequest, DesktopResponse, Edge, FRACTION_MAX, Geometry, MAX_TOKEN, Point, Rect,
    ReturnMapping,
};

use super::layout_model::{Layout, Transition};

#[derive(Clone, Debug)]
pub(crate) struct Handoff {
    pub peer: String,
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
        self.return_mapping.geometry == *geometry
    }

    /// Asks the other computer to put its cursor at the entry point.
    // The Mac still uses its own copies of this and the checks below until it
    // switches over (ROADMAP change 10).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn prepare(&self, token: u64) -> DesktopRequest {
        DesktopRequest::Prepare {
            token,
            edge: self.edge,
            start: self.start,
            end: self.end,
            position: self.position,
        }
    }

    /// Checks that the other computer prepared a desktop of the size the
    /// layout expects, so the crossing lands where the layout shows it.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn check_prepared(&self, response: DesktopResponse) -> Result<()> {
        response.validate()?;
        match response {
            DesktopResponse::Prepared { geometry, .. } => {
                let bounds = geometry.bounds()?;
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
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn token() -> Result<u64> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("could not create a desktop handoff token: {error}"))?;
    Ok((u64::from_ne_bytes(random) & MAX_TOKEN).max(1))
}

/// Checks the other computer's answer to Finish.
#[cfg_attr(target_os = "macos", allow(dead_code))]
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
        .filter(|m| m.peer.is_none())
        .collect();
    ensure!(locals.len() == 1, "The layout needs one local computer");
    let bounds = geometry.bounds()?;
    ensure!(
        locals[0].width == bounds.width && locals[0].height == bounds.height,
        "This computer's desktop changed. Waiting for its updated layout"
    );
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
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn from_edge(
    layout: &Layout,
    geometry: &Geometry,
    edge: Edge,
    position: u32,
) -> Option<Handoff> {
    let bounds = geometry.bounds().ok()?;
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
        .find(|t| (t.source_start..=t.source_end).contains(&along))
        .and_then(|transition| handoff_at(layout, geometry, transition, current))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn crossing(
    layout: &Layout,
    geometry: &Geometry,
    previous: Point,
    current: Point,
) -> Option<Handoff> {
    let bounds = geometry.bounds().ok()?;
    if !contains(geometry, previous) || !contains(geometry, current) {
        return None;
    }
    for transition in layout.transitions() {
        if layout.monitors[transition.source].peer.is_some() {
            continue;
        }
        let local_edge = transition.edge;
        let (entered, along) = match local_edge {
            Edge::Left => (
                current.x <= bounds.x + 1 && previous.x > bounds.x + 1,
                f64::from(current.y - bounds.y) / f64::from(bounds.height),
            ),
            Edge::Right => {
                let edge = bounds.x + bounds.width as i32 - 2;
                (
                    current.x >= edge && previous.x < edge,
                    f64::from(current.y - bounds.y) / f64::from(bounds.height),
                )
            }
            Edge::Top => (
                current.y <= bounds.y + 1 && previous.y > bounds.y + 1,
                f64::from(current.x - bounds.x) / f64::from(bounds.width),
            ),
            Edge::Bottom => {
                let edge = bounds.y + bounds.height as i32 - 2;
                (
                    current.y >= edge && previous.y < edge,
                    f64::from(current.x - bounds.x) / f64::from(bounds.width),
                )
            }
        };
        if !entered || along < transition.source_start || along >= transition.source_end {
            continue;
        }
        return handoff_at(layout, geometry, &transition, current);
    }
    None
}

/// The crossing through `transition` for a cursor at `current` on its edge.
fn handoff_at(
    layout: &Layout,
    geometry: &Geometry,
    transition: &Transition,
    current: Point,
) -> Option<Handoff> {
    let peer = layout.monitors[transition.target].peer.as_ref()?;
    let return_mapping = ReturnMapping {
        edge: transition.edge,
        local_start: transition.source_start,
        local_end: transition.source_end,
        remote_start: transition.target_start,
        remote_end: transition.target_end,
        geometry: geometry.clone(),
    };
    Some(Handoff {
        peer: peer.clone(),
        edge: opposite(transition.edge),
        start: fraction(transition.target_start),
        end: fraction(transition.target_end),
        position: return_mapping.fraction(current).ok()?,
        expected_width: layout.monitors[transition.target].width,
        expected_height: layout.monitors[transition.target].height,
        entry_region: entry_region(
            geometry,
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

    fn setup() -> (Layout, Geometry) {
        (
            Layout {
                monitors: vec![
                    Monitor {
                        id: "mac".into(),
                        label: "Mac".into(),
                        peer: None,
                        x: 0,
                        y: 0,
                        width: 2000,
                        height: 1000,
                    },
                    Monitor {
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
        let pushed = from_edge(&layout, &geometry, Edge::Right, 750_000).unwrap();
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
            from_edge(&layout, &geometry, Edge::Right, 100_000).is_none(),
            "no computer there"
        );
        assert!(from_edge(&layout, &geometry, Edge::Left, 750_000).is_none());

        let prepared = |width, height| DesktopResponse::Prepared {
            geometry: Geometry {
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
