//! A desktop lease binds cursor placement and the return edge to one session.
use super::input;
use crate::desktop::{
    DesktopRequest, DesktopResponse, Edge, FRACTION_MAX, Geometry, LEASE_MS, Point, ReturnMapping,
};
use anyhow::{Result, ensure};
use std::time::{Duration, Instant};

pub struct Lease {
    pub peer: String,
    pub session: u64,
    token: u64,
    geometry: Geometry,
    edge: Edge,
    start: u32,
    end: u32,
    expires: Instant,
    next_geometry: Instant,
    returned: Option<u32>,
}
impl Lease {
    pub fn belongs(&self, peer: &str, session: u64) -> bool {
        self.peer == peer && self.session == session
    }
    pub fn expired(&self) -> bool {
        Instant::now() >= self.expires
    }
    pub fn returned(&self) -> bool {
        self.returned.is_some()
    }
    pub fn sample(&mut self) -> Result<()> {
        if Instant::now() >= self.next_geometry {
            ensure!(
                input::geometry()? == self.geometry,
                "The Windows desktop changed"
            );
            self.next_geometry = Instant::now() + Duration::from_millis(250);
        }
        if input::clean() {
            self.returned = self.returned.or_else(|| {
                at_edge(
                    &self.geometry,
                    self.edge,
                    self.start,
                    self.end,
                    input::cursor().ok()?,
                )
            });
        }
        Ok(())
    }
    pub fn request(&mut self, request: DesktopRequest) -> Result<DesktopResponse> {
        ensure!(!self.expired(), "Desktop handoff expired");
        ensure!(
            request.token() == Some(self.token),
            "Desktop handoff token is stale"
        );
        match request {
            DesktopRequest::Poll { .. } => {
                self.expires = Instant::now() + Duration::from_millis(LEASE_MS);
                self.sample()?;
                Ok(self.returned.map_or(DesktopResponse::Active, |position| {
                    DesktopResponse::Returned { position }
                }))
            }
            DesktopRequest::Finish { .. } => Ok(DesktopResponse::Finished),
            _ => anyhow::bail!("A desktop handoff is already active"),
        }
    }
}
pub fn prepare(
    peer: String,
    session: u64,
    request: DesktopRequest,
) -> Result<(Lease, DesktopResponse)> {
    request.validate()?;
    let DesktopRequest::Prepare {
        token,
        edge,
        start,
        end,
        position,
    } = request
    else {
        anyhow::bail!("Expected Prepare");
    };
    let geometry = input::geometry()?;
    let p = entry(&geometry, edge, start, end, position)?;
    input::move_to(p)?;
    let actual = input::cursor()?;
    ensure!(
        actual.x.abs_diff(p.x) <= 2 && actual.y.abs_diff(p.y) <= 2,
        "Windows did not place the pointer at the entry point"
    );
    let lease = Lease {
        peer,
        session,
        token,
        geometry: geometry.clone(),
        edge,
        start,
        end,
        expires: Instant::now() + Duration::from_millis(LEASE_MS),
        next_geometry: Instant::now(),
        returned: None,
    };
    Ok((
        lease,
        DesktopResponse::Prepared {
            geometry,
            position: actual,
        },
    ))
}
fn entry(geometry: &Geometry, edge: Edge, start: u32, end: u32, position: u32) -> Result<Point> {
    let fraction = |n: u32| f64::from(n) / f64::from(FRACTION_MAX);
    ReturnMapping {
        geometry: geometry.clone(),
        edge,
        local_start: fraction(start),
        local_end: fraction(end),
        remote_start: fraction(start),
        remote_end: fraction(end),
    }
    .position(position)
}
fn at_edge(geometry: &Geometry, edge: Edge, start: u32, end: u32, p: Point) -> Option<u32> {
    let b = geometry.bounds().ok()?;
    let (hit, along, origin, span) = match edge {
        Edge::Left => (p.x <= b.x, p.y, b.y, b.height),
        Edge::Right => (p.x >= b.x + b.width as i32 - 1, p.y, b.y, b.height),
        Edge::Top => (p.y <= b.y, p.x, b.x, b.width),
        Edge::Bottom => (p.y >= b.y + b.height as i32 - 1, p.x, b.x, b.width),
    };
    if !hit || !geometry.monitors.iter().any(|m| m.contains(p)) {
        return None;
    }
    let position =
        ((f64::from(along - origin) / f64::from(span)) * f64::from(FRACTION_MAX)).round() as u32;
    (start..=end).contains(&position).then_some(position)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::Rect;
    #[test]
    fn negative_origin_and_partial_edge_return() {
        let g = Geometry {
            monitors: vec![
                Rect {
                    x: -1920,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
                Rect {
                    x: 0,
                    y: 0,
                    width: 2560,
                    height: 1440,
                },
            ],
        };
        let p = entry(&g, Edge::Left, 0, 500_000, 250_000).unwrap();
        assert_eq!(p, Point { x: -1917, y: 360 });
        assert_eq!(
            at_edge(&g, Edge::Left, 0, 500_000, Point { x: -1920, y: 360 }),
            Some(250_000)
        );
        assert_eq!(
            at_edge(&g, Edge::Left, 0, 500_000, Point { x: -1920, y: 900 }),
            None
        );
        assert_eq!(
            at_edge(&g, Edge::Left, 0, FRACTION_MAX, Point { x: -1920, y: 1200 }),
            None
        );
        assert_eq!(at_edge(&g, Edge::Left, 0, FRACTION_MAX, p), None);
    }
}
