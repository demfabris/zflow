//! A desktop lease binds cursor placement and the exit edges to one session.
use super::input;
use crate::core::ReceiverEffect;
use crate::desktop::{
    DesktopRequest, DesktopResponse, Edge, Exit, FRACTION_MAX, Geometry, LEASE_MS, Point,
    ReturnMapping,
};
use anyhow::{Result, ensure};
use std::time::{Duration, Instant};

pub struct Lease {
    pub peer: String,
    pub session: u64,
    token: u64,
    geometry: Geometry,
    /// Each exit the peer gave that has a monitor behind it, by its index,
    /// with that monitor.
    exits: Vec<(u32, Geometry, Exit)>,
    expires: Instant,
    next_geometry: Instant,
    /// The exit the pointer left through, and where, once it has.
    exited: Option<(u32, u32)>,
    /// A Poll has answered `exited`.
    delivered: bool,
    /// The peer kept the pointer here after an exit, so resting on an exit's
    /// edge does not report it again until the pointer moves off.
    resting: bool,
}
impl Lease {
    pub fn belongs(&self, peer: &str, session: u64) -> bool {
        self.peer == peer && self.session == session
    }
    pub fn expired(&self) -> bool {
        Instant::now() >= self.expires
    }
    /// An exit waits for the peer, so its input is dropped meanwhile.
    pub fn exited(&self) -> bool {
        self.exited.is_some()
    }
    pub fn boundaries(&self) -> Vec<input::Boundary> {
        if self.expired() || self.exited() {
            return Vec::new();
        }
        self.exits
            .iter()
            .filter_map(|(_, selected, exit)| {
                input::Boundary::new(
                    selected.bounds().ok()?,
                    exit.edge,
                    exit.start,
                    exit.end,
                    true,
                )
            })
            .collect()
    }
    pub fn sample(&mut self) -> Result<()> {
        if Instant::now() >= self.next_geometry {
            ensure!(
                input::geometry()? == self.geometry,
                "The Windows desktop changed"
            );
            self.next_geometry = Instant::now() + Duration::from_millis(250);
        }
        if input::clean()
            && self.exited.is_none()
            && let Ok(cursor) = input::cursor()
        {
            let resting = self.resting_on(cursor);
            self.resting &= resting.is_some();
            if !self.resting && resting.is_some() {
                self.exited = resting;
                self.delivered = false;
            }
        }
        Ok(())
    }
    /// The exit whose outermost pixels `cursor` rests on, and where.
    fn resting_on(&self, cursor: Point) -> Option<(u32, u32)> {
        self.exits.iter().find_map(|(index, selected, exit)| {
            Some((
                *index,
                at_edge(selected, exit.edge, exit.start, exit.end, cursor)?,
            ))
        })
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
                if self.delivered {
                    // Polling again after an exit means the pointer stays.
                    self.exited = None;
                    self.delivered = false;
                    self.resting = true;
                }
                self.sample()?;
                Ok(match self.exited {
                    Some((exit, position)) => {
                        self.delivered = true;
                        DesktopResponse::Exited { exit, position }
                    }
                    None => DesktopResponse::Active,
                })
            }
            DesktopRequest::Finish { .. } => Ok(DesktopResponse::Finished),
            _ => anyhow::bail!("A desktop handoff is already active"),
        }
    }
}
/// Keeps the pointer where it is while an exit waits for the peer, so it
/// cannot leave again or slip onto another monitor. Keys, clicks and scroll
/// still apply, since the peer may keep the pointer here.
pub fn hold_pointer(effects: &mut [ReceiverEffect]) {
    for effect in effects {
        if let ReceiverEffect::Motion { delta, .. } = effect {
            delta.dx = 0;
            delta.dy = 0;
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
        monitor,
        token,
        edge,
        start,
        end,
        position,
        exits,
    } = request
    else {
        anyhow::bail!("Expected Prepare");
    };
    let geometry = input::geometry()?;
    let selected = geometry.for_monitor(monitor.as_deref())?;
    let p = entry(&selected, edge, start, end, position)?;
    // An exit on a monitor that is gone leads nowhere.
    let exits = (0..)
        .zip(exits)
        .filter_map(|(index, exit)| {
            let selected = geometry.for_monitor(exit.monitor.as_deref()).ok()?;
            Some((index, selected, exit))
        })
        .collect();
    input::move_to(p)?;
    let actual = input::cursor()?;
    ensure!(
        actual.x.abs_diff(p.x) <= 2 && actual.y.abs_diff(p.y) <= 2,
        "Windows did not place the pointer at the entry point"
    );
    let mut lease = Lease {
        peer,
        session,
        token,
        geometry: geometry.clone(),
        exits,
        expires: Instant::now() + Duration::from_millis(LEASE_MS),
        next_geometry: Instant::now(),
        exited: None,
        delivered: false,
        resting: false,
    };
    // An entry on another exit's outermost pixels, as near a corner, is
    // not a push out through it.
    lease.resting = lease.resting_on(actual).is_some();
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
    use crate::core::{HidUsage, MotionDelta, MotionSequence, PointerButton};
    use crate::desktop::Rect;
    #[test]
    fn a_pending_exit_holds_the_pointer_but_keeps_keys_clicks_and_scroll() {
        let key = ReceiverEffect::Key {
            key: HidUsage::keyboard(4),
            pressed: true,
            synthetic: false,
        };
        let click = ReceiverEffect::Button {
            button: PointerButton(1),
            pressed: true,
            synthetic: false,
        };
        let motion = |dx, dy| ReceiverEffect::Motion {
            delta: MotionDelta {
                dx,
                dy,
                scroll_x: 0,
                scroll_y: 120,
            },
            through_sequence: MotionSequence(3),
        };
        let mut effects = vec![key.clone(), motion(-40, 7), click.clone()];
        hold_pointer(&mut effects);
        assert_eq!(effects, [key, motion(0, 0), click]);
    }
    #[test]
    fn negative_origin_and_partial_edge_return() {
        let g = Geometry {
            displays: Vec::new(),
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
