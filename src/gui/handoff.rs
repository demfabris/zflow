use anyhow::{Result, ensure};

use crate::desktop::{Edge, FRACTION_MAX, Geometry, Point, Rect};

use super::layout_model::{self, Layout};

#[derive(Clone, Debug)]
pub(super) struct Handoff {
    pub peer: String,
    pub edge: Edge,
    pub start: u32,
    pub end: u32,
    pub position: u32,
    pub expected_width: u32,
    pub expected_height: u32,
    local_edge: Edge,
    local_start: f64,
    local_end: f64,
    remote_start: f64,
    remote_end: f64,
    geometry: Geometry,
}

impl Handoff {
    pub fn matches_geometry(&self, geometry: &Geometry) -> bool {
        self.geometry == *geometry
    }

    pub fn return_position(&self, position: u32) -> Result<Point> {
        ensure!(
            position >= self.start && position <= self.end,
            "The receiver returned an invalid crossing position"
        );
        let remote = f64::from(position) / f64::from(FRACTION_MAX);
        let progress =
            ((remote - self.remote_start) / (self.remote_end - self.remote_start)).clamp(0.0, 1.0);
        let along = self.local_start + progress * (self.local_end - self.local_start);
        let bounds = self.geometry.bounds()?;
        let point = edge_point(bounds, self.local_edge, along);
        ensure!(
            contains(&self.geometry, point),
            "The return point is outside the active Mac displays; check the layout"
        );
        Ok(point)
    }
}

pub(super) fn validate(layout: &Layout, geometry: &Geometry) -> Result<()> {
    layout.validate()?;
    geometry.validate()?;
    let locals: Vec<_> = layout
        .monitors
        .iter()
        .filter(|m| m.peer.is_none())
        .collect();
    ensure!(
        locals.len() == 1,
        "Save a layout with one local computer before enabling sharing"
    );
    let bounds = geometry.bounds()?;
    ensure!(
        locals[0].width == bounds.width && locals[0].height == bounds.height,
        "The Mac desktop changed. Refresh and save its layout before enabling sharing"
    );
    ensure!(
        layout
            .transitions()
            .iter()
            .any(|t| layout.monitors[t.source].peer.is_none()),
        "Place a paired computer against the Mac's edge and save the layout"
    );
    Ok(())
}

pub(super) fn crossing(
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
        let peer = layout.monitors[transition.target].peer.as_ref()?;
        let local_edge = match transition.edge {
            layout_model::Edge::Left => Edge::Left,
            layout_model::Edge::Right => Edge::Right,
            layout_model::Edge::Top => Edge::Top,
            layout_model::Edge::Bottom => Edge::Bottom,
        };
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
        let progress =
            (along - transition.source_start) / (transition.source_end - transition.source_start);
        let target =
            transition.target_start + progress * (transition.target_end - transition.target_start);
        return Some(Handoff {
            peer: peer.clone(),
            edge: opposite(local_edge),
            start: fraction(transition.target_start),
            end: fraction(transition.target_end),
            position: fraction(target),
            expected_width: layout.monitors[transition.target].width,
            expected_height: layout.monitors[transition.target].height,
            local_edge,
            local_start: transition.source_start,
            local_end: transition.source_end,
            remote_start: transition.target_start,
            remote_end: transition.target_end,
            geometry: geometry.clone(),
        });
    }
    None
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

fn contains(geometry: &Geometry, point: Point) -> bool {
    geometry.monitors.iter().any(|r| {
        point.x >= r.x
            && point.y >= r.y
            && i64::from(point.x) < i64::from(r.x) + i64::from(r.width)
            && i64::from(point.y) < i64::from(r.y) + i64::from(r.height)
    })
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
            handoff.return_position(250_000).unwrap(),
            Point { x: 996, y: 550 }
        );
        assert!(handoff.return_position(500_001).is_err());
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
            assert_eq!(handoff.return_position(handoff.position).unwrap(), returned);
            assert!(crossing(&layout, &geometry, current, previous).is_none());
        }
    }

    #[test]
    fn returning_into_a_missing_part_of_the_mac_desktop_is_rejected() {
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
        assert!(handoff.return_position(750_000).is_err());
    }
}
