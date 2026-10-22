//! The arrangement: each computer's screens as one island under its name,
//! lit seams where the pointer crosses to another computer, and the
//! computers found on the network on a shelf underneath. Dragging a
//! computer asks the engine where it would land and shows that; dropping it
//! moves it. Double-clicking a computer moves its screens one by one.

pub mod geometry;

use std::{cell::Cell, collections::HashMap, rc::Rc};

use zflow::app::{
    api::{PeerState, Request},
    layout_model::{Monitor, Preview, Seam},
};
use zz_gpui::{
    App, BorderStyle, Bounds, Context, Corners, DispatchPhase, Edges, Entity, FocusHandle,
    Focusable, Hsla, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, PathBuilder, Pixels, Point,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window, canvas, div, point,
    prelude::FluentBuilder as _, px, quad, size,
};
use zz_gpui_kit::{
    ActiveTheme as _, Colorize as _, Icon, IconName, PaintState, Sizable as _, StyledExt as _,
    Surface,
    button::{Button, ButtonVariants as _},
    h_flex, rems_from_px, v_flex,
};

use self::geometry::{
    BOARD_HEIGHT, DRAG_THRESHOLD, DRAG_TOLERANCE, Fit, KEY_STEP, KEY_TOLERANCE, NEW_TILE,
    PLACE_TOLERANCE, Rect, chip_spot, chip_spots, hull, travel,
};
use crate::{
    copy,
    model::{Model, Snapshot},
    theme::{BOARD, CHIP, SCREEN, SHELF_TILE},
    widgets,
};

/// This computer's island: its screens have no peer.
pub const LOCAL: &str = "local";
const CHIP_HEIGHT: f32 = 24.0;
const CHIP_TEXT: f32 = 12.0;
/// The shelf of computers found on the network, under the board.
const FOUND_SIZE: (f32, f32) = (132.0, 64.0);
const SHELF_TOP: f32 = 48.0;
const SHELF_GAP: f32 = 8.0;
/// A seam this long or longer gets arrows across it.
const BADGE_ROOM: f32 = 34.0;

/// Whose screen a layout monitor is.
pub fn owner_of(monitor: &Monitor) -> String {
    monitor
        .peer
        .as_ref()
        .map_or_else(|| LOCAL.to_owned(), |peer| format!("peer:{peer}"))
}

/// A drag of a computer's screens, or of one of them.
#[derive(Clone, Debug)]
struct Drag {
    owner: String,
    anchor: Monitor,
    /// Whether the whole computer moves.
    computer: bool,
    /// The fit the drag began with, so nothing rescales under the pointer.
    fit: Fit,
    /// How far the pointer went, in pixels.
    pixels: (f32, f32),
    /// The move the engine last answered for, and its answer.
    asked: Option<(i32, i32)>,
    landing: Landing,
    dropped: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Landing {
    /// No answer yet: the screens follow the pointer.
    Unknown,
    At(Preview),
    /// The engine says they cannot go there.
    Refused,
}

#[derive(Clone, Debug)]
enum Hit {
    Done(String),
    Island {
        owner: String,
        anchor: Monitor,
        computer: bool,
    },
    Found(String),
}

#[derive(Clone, Debug)]
struct Pressed {
    hit: Option<Hit>,
    at: (f32, f32),
    dragging: bool,
}

/// A found computer carried off the shelf, by id, with its middle.
#[derive(Clone, Debug)]
struct Carried {
    id: String,
    middle: (f32, f32),
}

pub struct Board {
    model: Entity<Model>,
    focus: FocusHandle,
    /// The computer whose inspector shows, by owner.
    pub selection: Option<String>,
    /// The computer whose screens move one by one.
    pub adjusting: Option<String>,
    hovered: Option<String>,
    drag: Option<Drag>,
    pressed: Option<Pressed>,
    carried: Option<Carried>,
    /// Where the board was last painted, in window pixels.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Which of its spots each computer's name took, kept while dragging.
    spots: HashMap<String, usize>,
    asking: bool,
}

/// One computer on the board as this frame draws it.
struct Island {
    owner: String,
    name: SharedString,
    state: Option<PeerState>,
    /// The layout's monitors, with where they are and where they show.
    screens: Vec<(Monitor, Rect, Rect)>,
    hull: Option<Rect>,
    chip: Rect,
}

impl Board {
    pub fn new(model: Entity<Model>, cx: &mut Context<Self>) -> Self {
        cx.observe(&model, |board, model, cx| {
            board.forget_missing(&model, cx);
            cx.notify();
        })
        .detach();
        Self {
            model,
            focus: cx.focus_handle(),
            selection: None,
            adjusting: None,
            hovered: None,
            drag: None,
            pressed: None,
            carried: None,
            bounds: Rc::new(Cell::new(Bounds::default())),
            spots: HashMap::new(),
            asking: false,
        }
    }

    pub fn select(&mut self, owner: Option<String>, cx: &mut Context<Self>) {
        if owner != self.selection {
            if self.adjusting != owner {
                self.adjusting = None;
            }
            self.selection = owner;
            cx.notify();
        }
    }

    pub fn adjust(&mut self, owner: &str, on: bool, cx: &mut Context<Self>) {
        self.selection = Some(owner.to_owned());
        self.adjusting = on.then(|| owner.to_owned());
        cx.notify();
    }

    /// A forgotten computer lets go of the selection; a drop ends once the
    /// snapshot has its screen somewhere else.
    fn forget_missing(&mut self, model: &Entity<Model>, cx: &App) {
        let Some(snapshot) = &model.read(cx).snapshot else {
            return;
        };
        let owners: Vec<String> = computers(snapshot).into_iter().map(|c| c.owner).collect();
        if self
            .selection
            .as_ref()
            .is_some_and(|owner| !owners.contains(owner))
        {
            self.selection = None;
        }
        if self
            .adjusting
            .as_ref()
            .is_some_and(|owner| !owners.contains(owner))
        {
            self.adjusting = None;
        }
        if let Some(drag) = &self.drag
            && drag.dropped
        {
            let moved = monitors(snapshot)
                .find(|monitor| monitor.id == drag.anchor.id)
                .is_none_or(|monitor| monitor.x != drag.anchor.x || monitor.y != drag.anchor.y);
            if moved {
                self.drag = None;
            }
        }
    }

    fn size(&self) -> (f32, f32) {
        let width = f32::from(self.bounds.get().size.width);
        (if width > 0.0 { width } else { 640.0 }, BOARD_HEIGHT)
    }

    fn moves(drag: &Drag, owner: &str, monitor: &Monitor) -> bool {
        if drag.computer {
            owner == drag.owner
        } else {
            monitor.id == drag.anchor.id
        }
    }

    /// How far the moving screens show from their spot: where the engine
    /// says they land, else under the pointer.
    fn shift(drag: &Drag) -> (f32, f32) {
        match &drag.landing {
            Landing::At(preview) => (
                (preview.x - drag.anchor.x) as f32 * drag.fit.scale,
                (preview.y - drag.anchor.y) as f32 * drag.fit.scale,
            ),
            _ => drag.pixels,
        }
    }

    fn request(drag: &Drag, preview: bool) -> Request {
        let (id, x, y) = (
            drag.anchor.id.clone(),
            drag.anchor.x + drag.fit.units(drag.pixels.0),
            drag.anchor.y + drag.fit.units(drag.pixels.1),
        );
        let tolerance = drag.fit.units(DRAG_TOLERANCE).unsigned_abs();
        let computer = drag.computer;
        if preview {
            Request::Preview {
                id,
                x,
                y,
                tolerance,
                computer,
            }
        } else {
            Request::MoveTile {
                id,
                x,
                y,
                tolerance,
                computer,
            }
        }
    }

    /// The islands as this frame draws them.
    fn islands(
        &mut self,
        snapshot: &Snapshot,
        window: &mut Window,
        cx: &App,
    ) -> (Option<Fit>, Vec<Island>) {
        let (width, height) = self.size();
        let shown: Vec<&Monitor> = monitors(snapshot).collect();
        let fit = match &self.drag {
            Some(drag) => Some(drag.fit),
            None => Fit::new(shown.iter().copied(), width, height),
        };
        let Some(fit) = fit else {
            return (None, Vec::new());
        };
        let shift = self.drag.as_ref().map(Self::shift).unwrap_or_default();
        let mut islands: Vec<Island> = Vec::new();
        for monitor in shown {
            let owner = owner_of(monitor);
            let home = fit.rect(monitor, (0.0, 0.0));
            let moving = self
                .drag
                .as_ref()
                .is_some_and(|drag| Self::moves(drag, &owner, monitor));
            let at = if moving {
                fit.rect(monitor, shift)
            } else {
                home
            };
            if let Some(island) = islands.iter_mut().find(|island| island.owner == owner) {
                island.screens.push((monitor.clone(), home, at));
            } else {
                let peer = monitor
                    .peer
                    .as_ref()
                    .and_then(|name| snapshot.peers.iter().find(|peer| &peer.name == name));
                islands.push(Island {
                    owner,
                    name: monitor
                        .peer
                        .clone()
                        .unwrap_or_else(|| copy::THIS_COMPUTER.to_owned())
                        .into(),
                    state: peer.map(|peer| peer.state),
                    screens: vec![(monitor.clone(), home, at)],
                    hull: None,
                    chip: Rect::default(),
                });
            }
        }
        for island in &mut islands {
            island.hull = hull(island.screens.iter().map(|(_, _, at)| *at));
        }
        // Names go where they cover the least of the others.
        let mut taken: Vec<Rect> = Vec::new();
        let hulls: Vec<Option<Rect>> = islands.iter().map(|island| island.hull).collect();
        for (index, island) in islands.iter_mut().enumerate() {
            let Some(hull) = island.hull else { continue };
            let chip_width =
                chip_width(&island.name, island.state.is_some(), window, cx).min(width - 8.0);
            let done = if self.adjusting.as_deref() == Some(island.owner.as_str()) {
                geometry::CHIP_GAP + 58.0
            } else {
                0.0
            };
            let span = chip_width + done;
            let others: Vec<Rect> = hulls
                .iter()
                .enumerate()
                .filter(|(other, _)| *other != index)
                .filter_map(|(_, hull)| *hull)
                .chain(taken.iter().copied())
                .collect();
            let kept = self
                .spots
                .get(&island.owner)
                .filter(|_| self.drag.is_some());
            let spot = if let Some(spot) = kept {
                *spot
            } else {
                let spot = chip_spot(hull, span, CHIP_HEIGHT, (width, height), &others);
                self.spots.insert(island.owner.clone(), spot);
                spot
            };
            let (x, wanted) = chip_spots(hull, span, CHIP_HEIGHT, width)[spot];
            let y = wanted.min(height - CHIP_HEIGHT - 4.0).max(4.0).round();
            island.chip = Rect::new(x, y, chip_width, CHIP_HEIGHT);
            taken.push(Rect::new(x, y, span, CHIP_HEIGHT));
        }
        (Some(fit), islands)
    }

    /// What a press at board point (`x`, `y`) picked up: the Done button, a
    /// computer's name, one of its screens, or a found computer.
    fn hit(
        &self,
        islands: &[Island],
        found: &[(String, Rect, bool)],
        x: f32,
        y: f32,
    ) -> Option<Hit> {
        for island in islands {
            if self.adjusting.as_deref() == Some(island.owner.as_str()) {
                let done = Rect::new(
                    island.chip.right() + geometry::CHIP_GAP,
                    island.chip.y,
                    58.0,
                    CHIP_HEIGHT,
                );
                if done.contains(x, y) {
                    return Some(Hit::Done(island.owner.clone()));
                }
            }
        }
        for island in islands {
            if island.hull.is_some() && island.chip.contains(x, y) {
                return Some(Hit::Island {
                    owner: island.owner.clone(),
                    anchor: island.screens[0].0.clone(),
                    computer: true,
                });
            }
        }
        // Later screens draw on top.
        for island in islands.iter().rev() {
            for (monitor, _, at) in island.screens.iter().rev() {
                if at.contains(x, y) {
                    return Some(Hit::Island {
                        owner: island.owner.clone(),
                        anchor: monitor.clone(),
                        computer: self.adjusting.as_deref() != Some(island.owner.as_str()),
                    });
                }
            }
        }
        found
            .iter()
            .find(|(_, rect, placeable)| *placeable && rect.contains(x, y))
            .map(|(id, _, _)| Hit::Found(id.clone()))
    }

    fn press(
        &mut self,
        x: f32,
        y: f32,
        clicks: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = self.model.read(cx).snapshot.clone() else {
            return;
        };
        window.focus(&self.focus, cx);
        let (_, islands) = self.islands(&snapshot, window, cx);
        let found = self.found_tiles(&snapshot);
        let hit = self.hit(&islands, &found, x, y);
        if let Some(Hit::Done(owner)) = &hit {
            let owner = owner.clone();
            self.adjust(&owner, false, cx);
            self.pressed = None;
            return;
        }
        // A double-click on a found computer puts it beside this one, the
        // way a keyboard does elsewhere; a single click never places one.
        if clicks == 2
            && let Some(Hit::Found(id)) = &hit
        {
            self.place_beside(id, cx);
            self.pressed = None;
            return;
        }
        let mut hit = hit;
        if clicks == 2
            && let Some(Hit::Island {
                owner, computer, ..
            }) = &mut hit
        {
            // A second press soon after the first adjusts its screens, or
            // stops adjusting them.
            let on = self.adjusting.as_deref() != Some(owner.as_str());
            let owner = owner.clone();
            self.adjust(&owner, on, cx);
            *computer = !on;
        }
        self.pressed = Some(Pressed {
            hit,
            at: (x, y),
            dragging: false,
        });
    }

    fn drag_to(&mut self, x: f32, y: f32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pressed) = self.pressed.clone() else {
            return;
        };
        let Some(snapshot) = self.model.read(cx).snapshot.clone() else {
            return;
        };
        let (dx, dy) = (x - pressed.at.0, y - pressed.at.1);
        if !pressed.dragging {
            if dx.hypot(dy) < DRAG_THRESHOLD {
                return;
            }
            if let Some(pressed) = &mut self.pressed {
                pressed.dragging = true;
            }
            match &pressed.hit {
                Some(Hit::Island {
                    owner,
                    anchor,
                    computer,
                }) => {
                    let _ = window;
                    let (width, height) = self.size();
                    let Some(fit) = Fit::new(monitors(&snapshot), width, height) else {
                        return;
                    };
                    self.drag = Some(Drag {
                        owner: owner.clone(),
                        anchor: anchor.clone(),
                        computer: *computer,
                        fit,
                        pixels: (0.0, 0.0),
                        asked: None,
                        landing: Landing::Unknown,
                        dropped: false,
                    });
                    self.selection = Some(owner.clone());
                }
                Some(Hit::Found(id)) => {
                    self.carried = Some(Carried {
                        id: id.clone(),
                        middle: (x, y),
                    });
                }
                _ => return,
            }
        }
        if let Some(carried) = &mut self.carried {
            carried.middle = (x, y);
            cx.notify();
            return;
        }
        let board = self.size();
        if let Some(drag) = self.drag.clone() {
            let moving: Vec<Rect> = monitors(&snapshot)
                .filter(|monitor| Self::moves(&drag, &owner_of(monitor), monitor))
                .map(|monitor| drag.fit.rect(monitor, (0.0, 0.0)))
                .collect();
            if let Some(drag) = &mut self.drag {
                drag.pixels = travel(&moving, dx, dy, board);
            }
            self.ask(cx);
            cx.notify();
        }
    }

    /// Asks the engine where the drag lands, one question at a time, the
    /// newest travel next.
    fn ask(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = &self.drag else { return };
        if drag.dropped || self.asking {
            return;
        }
        let request = Self::request(drag, true);
        let Request::Preview { x, y, .. } = &request else {
            return;
        };
        let asked = (*x, *y);
        if drag.asked == Some(asked) {
            return;
        }
        self.asking = true;
        let answer = self.model.read(cx).ask(request);
        cx.spawn(async move |board, cx| {
            let landing = answer.recv().await.ok().and_then(Result::ok).map(|value| {
                serde_json::from_value::<Option<Preview>>(value)
                    .ok()
                    .flatten()
                    .map_or(Landing::Refused, Landing::At)
            });
            let _ = board.update(cx, |board, cx| {
                board.asking = false;
                let answered = landing.is_some();
                if let Some(drag) = &mut board.drag
                    && !drag.dropped
                {
                    drag.asked = answered.then_some(asked);
                    drag.landing = landing.unwrap_or(Landing::Unknown);
                    cx.notify();
                }
                if answered {
                    board.ask(cx);
                }
            });
        })
        .detach();
    }

    fn release(&mut self, x: f32, y: f32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pressed) = self.pressed.take() else {
            return;
        };
        if !pressed.dragging {
            match pressed.hit {
                Some(Hit::Island { owner, .. }) => self.select(Some(owner), cx),
                // A click beside the computers lets go of the selected one.
                None if y < BOARD_HEIGHT => self.select(None, cx),
                _ => {}
            }
            return;
        }
        if let Some(carried) = self.carried.take() {
            self.drop_found(&carried.id, x, y, window, cx);
            cx.notify();
            return;
        }
        let Some(drag) = &mut self.drag else { return };
        drag.dropped = true;
        let request = Self::request(drag, false);
        let done = self.model.read(cx).attempt(request);
        cx.spawn(async move |board, cx| {
            let failed = done.recv().await.map_or(true, |result| result.is_err());
            let _ = board.update(cx, |board, cx| {
                // A refused move goes back; a done one ends with the next
                // snapshot that has the screens somewhere else.
                if failed {
                    board.drag = None;
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// A found computer let go with its middle at (`x`, `y`) joins the
    /// layout there, with a new screen's size. Let go on the shelf, it goes
    /// back.
    fn drop_found(
        &mut self,
        id: &str,
        x: f32,
        y: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = window;
        if y >= BOARD_HEIGHT {
            return;
        }
        let Some(snapshot) = &self.model.read(cx).snapshot else {
            return;
        };
        let (width, height) = self.size();
        let Some(fit) = Fit::new(monitors(snapshot), width, height) else {
            return;
        };
        let (mx, my) = fit.layout_point(x, y);
        let request = Request::Place {
            id: id.to_owned(),
            x: (mx - NEW_TILE.0 as f32 / 2.0).round() as i32,
            y: (my - NEW_TILE.1 as f32 / 2.0).round() as i32,
            tolerance: fit.units(DRAG_TOLERANCE).unsigned_abs(),
        };
        self.model.read(cx).send(request);
    }

    /// Keyboard people put a found computer beside this one.
    pub fn place_beside(&self, id: &str, cx: &App) {
        let Some(snapshot) = &self.model.read(cx).snapshot else {
            return;
        };
        let Some(local) = monitors(snapshot).find(|monitor| monitor.peer.is_none()) else {
            return;
        };
        self.model.read(cx).send(Request::Place {
            id: id.to_owned(),
            x: local.x + local.width as i32,
            y: local.y,
            tolerance: PLACE_TOLERANCE,
        });
    }

    fn hover(&mut self, x: f32, y: f32, window: &mut Window, cx: &mut Context<Self>) {
        if self.pressed.is_some() {
            return;
        }
        let Some(snapshot) = self.model.read(cx).snapshot.clone() else {
            return;
        };
        let (_, islands) = self.islands(&snapshot, window, cx);
        let hovered = islands
            .iter()
            .find(|island| {
                island.hull.is_some_and(|hull| hull.contains(x, y)) || island.chip.contains(x, y)
            })
            .map(|island| island.owner.clone());
        if hovered != self.hovered {
            self.hovered = hovered;
            cx.notify();
        }
    }

    /// Arrow keys move the selected computer, or its screen while it is
    /// being adjusted. Escape lets go of it.
    fn key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let step = match event.keystroke.key.as_str() {
            "escape" => {
                if self.selection.is_some() {
                    self.select(None, cx);
                    cx.stop_propagation();
                }
                return;
            }
            "left" => (-1, 0),
            "right" => (1, 0),
            "up" => (0, -1),
            "down" => (0, 1),
            _ => return,
        };
        let Some(owner) = self.selection.clone() else {
            return;
        };
        let Some(snapshot) = &self.model.read(cx).snapshot else {
            return;
        };
        let Some(anchor) = monitors(snapshot)
            .find(|monitor| owner_of(monitor) == owner)
            .cloned()
        else {
            return;
        };
        cx.stop_propagation();
        self.model.read(cx).send(Request::MoveTile {
            id: anchor.id.clone(),
            x: anchor.x + step.0 * KEY_STEP,
            y: anchor.y + step.1 * KEY_STEP,
            tolerance: KEY_TOLERANCE,
            computer: self.adjusting.as_deref() != Some(owner.as_str()),
        });
    }

    /// The found computers' tiles on the shelf: id, where, and whether one
    /// can be placed.
    fn found_tiles(&self, snapshot: &Snapshot) -> Vec<(String, Rect, bool)> {
        let (width, height) = self.size();
        let per_row = ((width - 2.0 * SHELF_GAP - 16.0) / (FOUND_SIZE.0 + SHELF_GAP))
            .floor()
            .max(1.0) as usize;
        snapshot
            .unplaced
            .iter()
            .enumerate()
            .map(|(index, computer)| {
                let rect = Rect::new(
                    2.0 * SHELF_GAP + (index % per_row) as f32 * (FOUND_SIZE.0 + SHELF_GAP),
                    height + SHELF_TOP + (index / per_row) as f32 * (FOUND_SIZE.1 + SHELF_GAP),
                    FOUND_SIZE.0,
                    FOUND_SIZE.1,
                );
                (computer.id.clone(), rect, copy::placeable(computer))
            })
            .collect()
    }

    fn shelf_height(&self, snapshot: &Snapshot) -> f32 {
        let found = self.found_tiles(snapshot);
        Rect::around(found.iter().map(|(_, rect, _)| *rect))
            .map_or(0.0, |rect| rect.bottom() - BOARD_HEIGHT + SHELF_GAP)
    }

    /// The seams to draw: while a drag waits for the engine, only those it
    /// cannot change.
    fn shown_seams(&self, snapshot: &Snapshot) -> Vec<Seam> {
        let Some(drag) = &self.drag else {
            return snapshot.seams.clone();
        };
        if let Landing::At(preview) = &drag.landing {
            return preview.seams.clone();
        }
        let moving: Vec<&str> = monitors(snapshot)
            .filter(|monitor| Self::moves(drag, &owner_of(monitor), monitor))
            .map(|monitor| monitor.id.as_str())
            .collect();
        snapshot
            .seams
            .iter()
            .filter(|seam| !moving.contains(&seam.a.as_str()) && !moving.contains(&seam.b.as_str()))
            .cloned()
            .collect()
    }
}

impl Focusable for Board {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The layout's screens that are on, in its order.
pub fn monitors(snapshot: &Snapshot) -> impl Iterator<Item = &Monitor> {
    snapshot
        .layout
        .iter()
        .flat_map(|layout| layout.monitors.iter())
        .filter(|monitor| monitor.active())
}

/// A computer the inspector can show.
#[derive(Clone, Debug)]
pub struct Computer {
    pub owner: String,
    pub name: String,
    pub peer: Option<zflow::app::api::Peer>,
    pub screens: Vec<Monitor>,
    pub mark: Option<String>,
}

/// The computers on the arrangement, in its order, then any paired one with
/// no screen there.
pub fn computers(snapshot: &Snapshot) -> Vec<Computer> {
    let mut list: Vec<Computer> = Vec::new();
    for monitor in monitors(snapshot) {
        let owner = owner_of(monitor);
        if let Some(computer) = list.iter_mut().find(|computer| computer.owner == owner) {
            computer.screens.push(monitor.clone());
            continue;
        }
        let peer = monitor
            .peer
            .as_ref()
            .and_then(|name| snapshot.peers.iter().find(|peer| &peer.name == name))
            .cloned();
        list.push(Computer {
            owner,
            name: monitor
                .peer
                .clone()
                .unwrap_or_else(|| copy::THIS_COMPUTER.to_owned()),
            mark: match &peer {
                Some(peer) => Some(peer.mark.clone()),
                None if monitor.peer.is_none() => snapshot.own_mark.clone(),
                None => None,
            },
            peer,
            screens: vec![monitor.clone()],
        });
    }
    for peer in &snapshot.peers {
        let owner = format!("peer:{}", peer.name);
        if !list.iter().any(|computer| computer.owner == owner) {
            list.push(Computer {
                owner,
                name: peer.name.clone(),
                mark: Some(peer.mark.clone()),
                peer: Some(peer.clone()),
                screens: Vec::new(),
            });
        }
    }
    list
}

/// Where the pointer goes from `computer`'s screens to another's.
pub fn crossings<'a>(snapshot: &'a Snapshot, computer: &Computer) -> Vec<&'a Seam> {
    let mine: Vec<&str> = computer
        .screens
        .iter()
        .map(|monitor| monitor.id.as_str())
        .collect();
    snapshot
        .seams
        .iter()
        .filter(|seam| {
            !seam.same_computer
                && mine.contains(&seam.a.as_str()) != mine.contains(&seam.b.as_str())
        })
        .collect()
}

fn chip_width(name: &str, dot: bool, window: &mut Window, cx: &App) -> f32 {
    let mut font = zz_gpui::font(cx.theme().font_family.clone());
    font.weight = zz_gpui::FontWeight::MEDIUM;
    let line = window.text_system().shape_line(
        SharedString::from(name.to_owned()),
        px(CHIP_TEXT),
        &[zz_gpui::TextRun {
            len: name.len(),
            font,
            color: cx.theme().foreground,
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    f32::from(line.width) + 18.0 + if dot { 12.0 } else { 0.0 }
}

impl Render for Board {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(snapshot) = self.model.read(cx).snapshot.clone() else {
            return div().into_any_element();
        };
        let (fit, islands) = self.islands(&snapshot, window, cx);
        let theme = cx.theme().clone();
        let shelf_height = self.shelf_height(&snapshot);
        let total = BOARD_HEIGHT + shelf_height;
        let refused = self
            .drag
            .as_ref()
            .is_some_and(|drag| drag.landing == Landing::Refused);
        let crossing = snapshot.sharing == Some(true);
        let seams = self.shown_seams(&snapshot);
        let selection = self.selection.clone();
        let hovered = self.hovered.clone();
        let adjusting = self.adjusting.clone();
        let outlines: Vec<(Rect, bool, bool)> = islands
            .iter()
            .filter_map(|island| {
                let selected = selection.as_deref() == Some(island.owner.as_str());
                let lit = selected || hovered.as_deref() == Some(island.owner.as_str());
                Some((
                    island.hull.filter(|_| lit)?,
                    selected,
                    adjusting.as_deref() == Some(island.owner.as_str()),
                ))
            })
            .collect();
        let bounds = self.bounds.clone();
        let entity = cx.entity();
        let carried = self.carried.clone().and_then(|carried| {
            fit.map(|fit| {
                let (width, height) =
                    (NEW_TILE.0 as f32 * fit.scale, NEW_TILE.1 as f32 * fit.scale);
                (
                    carried.id,
                    Rect::new(
                        carried.middle.0 - width / 2.0,
                        carried.middle.1 - height / 2.0,
                        width,
                        height,
                    ),
                    carried.middle,
                )
            })
        });
        let moving_ids: Vec<String> = self
            .drag
            .as_ref()
            .map(|drag| {
                monitors(&snapshot)
                    .filter(|monitor| Self::moves(drag, &owner_of(monitor), monitor))
                    .map(|monitor| monitor.id.clone())
                    .collect()
            })
            .unwrap_or_default();

        // The mat, the computers' outlines, and the pointer's handlers.
        let mat_theme = theme.clone();
        // The outline follows the theme's radius, as the well it sits in does.
        let ring_radius = theme.shape_radius(BOARD, zz_gpui_kit::Shape::Round);
        let backdrop = canvas(
            move |area, _, _| {
                bounds.set(area);
                area
            },
            move |area, _, window, _| {
                let ink = mat_theme.foreground;
                #[allow(
                    clippy::disallowed_methods,
                    reason = "the mat's dots and outlines are drawn ink, not a surface"
                )]
                let dot = ink.opacity(0.07);
                let mut x = 9.0;
                while x < f32::from(area.size.width) {
                    let mut y = 9.0;
                    while y < BOARD_HEIGHT {
                        window.paint_quad(quad(
                            Bounds::new(
                                area.origin + point(px(x - 0.8), px(y - 0.8)),
                                size(px(1.6), px(1.6)),
                            ),
                            px(0.8),
                            dot,
                            Edges::default(),
                            dot,
                            BorderStyle::Solid,
                        ));
                        y += 18.0;
                    }
                    x += 18.0;
                }
                for (hull, selected, adjusting) in &outlines {
                    #[allow(
                        clippy::disallowed_methods,
                        reason = "a hovered computer's outline is a stroke over the mat"
                    )]
                    let color = if *selected {
                        mat_theme.accent
                    } else {
                        ink.opacity(0.35)
                    };
                    window.paint_quad(quad(
                        to_bounds(area, *hull),
                        Corners::all(ring_radius),
                        zz_gpui::transparent_black(),
                        // The kit's chosen ring when selected, its focus ring
                        // width when hovered.
                        Edges::all(if *selected {
                            px(2.0)
                        } else {
                            zz_gpui_kit::FOCUS_RING_WIDTH
                        }),
                        color,
                        if *adjusting {
                            BorderStyle::Dashed
                        } else {
                            BorderStyle::Solid
                        },
                    ));
                }
                let origin = area.origin;
                let at = move |position: Point<Pixels>| {
                    (
                        f32::from(position.x - origin.x),
                        f32::from(position.y - origin.y),
                    )
                };
                let down = entity.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble
                        || event.button != MouseButton::Left
                        || !area.contains(&event.position)
                    {
                        return;
                    }
                    let (x, y) = at(event.position);
                    down.update(cx, |board, cx| {
                        board.press(x, y, event.click_count, window, cx);
                    });
                });
                let moved = entity.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let (x, y) = at(event.position);
                    moved.update(cx, |board, cx| {
                        if event.pressed_button == Some(MouseButton::Left)
                            && board.pressed.is_some()
                        {
                            board.drag_to(x, y, window, cx);
                        } else if area.contains(&event.position) {
                            board.hover(x, y, window, cx);
                        } else if board.hovered.take().is_some() {
                            cx.notify();
                        }
                    });
                });
                let up = entity.clone();
                window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                        return;
                    }
                    let (x, y) = at(event.position);
                    up.update(cx, |board, cx| board.release(x, y, window, cx));
                });
            },
        )
        .absolute()
        .inset_0();

        let seam_theme = theme.clone();
        let carried_rect = carried.as_ref().map(|(_, rect, _)| *rect);
        let seam_layer = canvas(
            |_, _, _| {},
            move |area, (), window, _| {
                let Some(fit) = fit else { return };
                let strength = if crossing { 1.0 } else { 0.3 };
                #[allow(
                    clippy::disallowed_methods,
                    reason = "seams are lit lines drawn in the accent, not surfaces"
                )]
                let (joint, glow, core) = (
                    seam_theme.foreground.opacity(0.5),
                    seam_theme.accent.opacity(0.28 * strength),
                    seam_theme.accent.opacity(strength),
                );
                for seam in &seams {
                    let inset = if seam.same_computer { 4.0 } else { 3.0 };
                    let (start, end) = fit.seam_line(seam, inset);
                    let (from, to) = (offset(area, start), offset(area, end));
                    if seam.same_computer {
                        stroke(window, from, to, 2.0, joint, Some(&[px(0.1), px(5.0)]));
                        continue;
                    }
                    stroke(window, from, to, 11.0, glow, None);
                    stroke(window, from, to, 4.0, core, None);
                    if (seam.end - seam.start) as f32 * fit.scale > BADGE_ROOM {
                        badge(
                            window,
                            (from + to) / 2.0,
                            seam.vertical,
                            core,
                            seam_theme.background,
                        );
                    }
                }
                if let Some(rect) =
                    carried_rect.filter(|rect| rect.y + rect.height / 2.0 < BOARD_HEIGHT)
                {
                    window.paint_quad(quad(
                        to_bounds(area, rect),
                        Corners::all(px(6.0)),
                        zz_gpui::transparent_black(),
                        Edges::all(px(1.5)),
                        seam_theme.accent,
                        BorderStyle::Dashed,
                    ));
                }
            },
        )
        .absolute()
        .inset_0();

        let mut screens = Vec::new();
        let mut chips = Vec::new();
        for island in &islands {
            let named = selection.as_deref() == Some(island.owner.as_str())
                || hovered.as_deref() == Some(island.owner.as_str());
            let away = island.owner != LOCAL
                && matches!(
                    island.state,
                    None | Some(PeerState::Paired | PeerState::Unreachable)
                );
            for (monitor, _, at) in &island.screens {
                let moving = moving_ids.contains(&monitor.id);
                // A computer that is away draws its screens disabled.
                let state = if moving && refused {
                    PaintState::Focused
                } else if moving {
                    PaintState::Hover
                } else if away {
                    PaintState::Disabled
                } else {
                    PaintState::Rest
                };
                screens.push(
                    div()
                        .id(SharedString::from(format!("screen-{}", monitor.id)))
                        .absolute()
                        .left(px(at.x))
                        .top(px(at.y))
                        .w(px(at.width))
                        .h(px(at.height))
                        .flex()
                        .items_end()
                        .overflow_hidden()
                        .surface_at(
                            Surface::new(SCREEN).tone(if moving && refused {
                                zz_gpui_kit::Tone::Danger
                            } else {
                                zz_gpui_kit::Tone::Neutral
                            }),
                            state,
                            cx,
                        )
                        .when(
                            monitor.display.is_none() && at.width.min(at.height) >= 24.0,
                            |this| {
                                this.items_center().justify_center().child(
                                    Icon::new(if island.owner == LOCAL {
                                        IconName::Laptop
                                    } else {
                                        IconName::Server
                                    })
                                    .with_size(px(16.0))
                                    .text_color(theme.foreground.muted()),
                                )
                            },
                        )
                        .when(
                            named
                                && monitor.display.is_some()
                                && at.width >= 64.0
                                && at.height >= 30.0,
                            |this| {
                                this.child(
                                    div().m(px(5.0)).min_w_0().child(
                                        zz_gpui_kit::badge::Badge::new(
                                            monitor
                                                .display
                                                .as_ref()
                                                .map_or(monitor.label.clone(), |display| {
                                                    display.name.clone()
                                                }),
                                        )
                                        .opaque(),
                                    ),
                                )
                            },
                        )
                        .tooltip({
                            let label = SharedString::from(monitor.label.clone());
                            move |window, cx| {
                                zz_gpui_kit::tooltip::Tooltip::new(label.clone()).build(window, cx)
                            }
                        }),
                );
            }
            if island.hull.is_none() {
                continue;
            }
            let selected = selection.as_deref() == Some(island.owner.as_str());
            chips.push(
                h_flex()
                    .id(SharedString::from(format!("chip-{}", island.owner)))
                    .absolute()
                    .left(px(island.chip.x))
                    .top(px(island.chip.y))
                    .max_w(px(self.size().0 - 8.0))
                    .h(px(CHIP_HEIGHT))
                    .px(px(9.0))
                    .gap(px(6.0))
                    .justify_center()
                    .rounded_full()
                    .surface_at(
                        Surface::new(CHIP),
                        if selected {
                            PaintState::Selected
                        } else {
                            PaintState::Rest
                        },
                        cx,
                    )
                    .child(
                        div()
                            .text_size(rems_from_px(CHIP_TEXT))
                            .font_medium()
                            .whitespace_nowrap()
                            .child(island.name.clone()),
                    )
                    .when(island.owner != LOCAL, |this| {
                        this.child(widgets::state_dot(island.state, cx))
                    })
                    .tooltip({
                        let text: SharedString = match island.state {
                            Some(state) => {
                                format!("{} · {}", island.name, copy::peer_state(state)).into()
                            }
                            None => island.name.clone(),
                        };
                        move |window, cx| {
                            zz_gpui_kit::tooltip::Tooltip::new(text.clone()).build(window, cx)
                        }
                    })
                    .into_any_element(),
            );
            if adjusting.as_deref() == Some(island.owner.as_str()) {
                let owner = island.owner.clone();
                chips.push(
                    div()
                        .absolute()
                        .left(px(island.chip.right() + geometry::CHIP_GAP))
                        .top(px(island.chip.y))
                        .child(
                            Button::new(SharedString::from(format!("done-{owner}")))
                                .small()
                                .primary()
                                .label("Done")
                                .tooltip("Move its screens together again")
                                .on_click(cx.listener(move |board, _, _, cx| {
                                    board.adjust(&owner, false, cx);
                                })),
                        )
                        .into_any_element(),
                );
            }
        }

        let found = self.found_tiles(&snapshot);
        let shelf_layer = (!snapshot.unplaced.is_empty()).then(|| {
            let carried_id = carried.as_ref().map(|(id, _, _)| id.clone());
            let carried_middle = carried.as_ref().map(|(_, _, middle)| *middle);
            div()
                .absolute()
                .left(px(SHELF_GAP))
                .right(px(SHELF_GAP))
                .top(px(BOARD_HEIGHT))
                .h(px(shelf_height - SHELF_GAP))
                .surface(Surface::new(zz_gpui_kit::roles::SETTINGS_PREVIEW), cx)
                .child(
                    v_flex()
                        .px(px(12.0))
                        .pt(px(8.0))
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_size(rems_from_px(13.0))
                                .font_medium()
                                .child("Found on this network"),
                        )
                        .child(widgets::muted(
                            "Drag one next to a screen to add it.",
                            12.0,
                            cx,
                        )),
                )
                .children(snapshot.unplaced.iter().zip(&found).map(
                    |(computer, (id, rect, placeable))| {
                        let carried = carried_id.as_deref() == Some(id.as_str());
                        let (x, y) = match carried_middle.filter(|_| carried) {
                            Some((mx, my)) => (mx - FOUND_SIZE.0 / 2.0, my - FOUND_SIZE.1 / 2.0),
                            None => (rect.x, rect.y),
                        };
                        found_tile(computer, *placeable, carried, cx)
                            .left(px(x - SHELF_GAP))
                            .top(px(y - BOARD_HEIGHT))
                            .into_any_element()
                    },
                ))
        });

        div()
            .id("board")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|board, event: &KeyDownEvent, _, cx| board.key(event, cx)))
            .relative()
            .w_full()
            .h(px(total))
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top_0()
                    .h(px(BOARD_HEIGHT))
                    .surface(Surface::new(crate::theme::BOARD), cx),
            )
            .child(backdrop)
            .when(islands.is_empty(), |this| {
                this.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .top_0()
                        .h(px(BOARD_HEIGHT))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(widgets::muted(
                            format!("Detecting {}’s displays…", copy::THIS_COMPUTER_LOWER),
                            13.0,
                            cx,
                        )),
                )
            })
            .children(screens)
            .child(seam_layer)
            .children(chips)
            .children(shelf_layer)
            .into_any_element()
    }
}

/// A found computer on the shelf. One that cannot be placed yet says why.
fn found_tile(
    computer: &zflow::neighbors::Unplaced,
    placeable: bool,
    carried: bool,
    cx: &App,
) -> zz_gpui::Stateful<zz_gpui::Div> {
    v_flex()
        .id(SharedString::from(format!("found-{}", computer.id)))
        .absolute()
        .w(px(FOUND_SIZE.0))
        .h(px(FOUND_SIZE.1))
        .px(px(10.0))
        .py(px(8.0))
        .gap(px(1.0))
        .cursor_pointer()
        .surface_at(
            Surface::new(SHELF_TILE),
            if carried {
                PaintState::Hover
            } else if placeable {
                PaintState::Rest
            } else {
                PaintState::Disabled
            },
            cx,
        )
        .child(
            h_flex()
                .justify_between()
                .child(
                    Icon::new(if computer.os == Some(zflow::wire::Os::Macos) {
                        IconName::Laptop
                    } else {
                        IconName::Server
                    })
                    .with_size(px(14.0)),
                )
                .children(computer.mark.as_deref().map(widgets::key_mark)),
        )
        .child(div().flex_1())
        .child(
            div()
                .text_size(rems_from_px(13.0))
                .font_medium()
                .truncate()
                .child(computer.name.clone()),
        )
        .child(
            div()
                .text_size(rems_from_px(11.0))
                .truncate()
                .child(copy::unplaced_detail(computer)),
        )
}

fn to_bounds(area: Bounds<Pixels>, rect: Rect) -> Bounds<Pixels> {
    Bounds::new(
        area.origin + point(px(rect.x), px(rect.y)),
        size(px(rect.width), px(rect.height)),
    )
}

fn offset(area: Bounds<Pixels>, (x, y): (f32, f32)) -> Point<Pixels> {
    area.origin + point(px(x), px(y))
}

fn stroke(
    window: &mut Window,
    from: Point<Pixels>,
    to: Point<Pixels>,
    width: f32,
    color: Hsla,
    dash: Option<&[Pixels]>,
) {
    let mut path = PathBuilder::stroke(px(width));
    if let Some(dash) = dash {
        path = path.dash_array(dash);
    }
    path.move_to(from);
    path.line_to(to);
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

/// A seam's badge: arrows each way across it on an accent dot, ⇄ across an
/// upright seam and ⇅ across one lying flat.
fn badge(window: &mut Window, at: Point<Pixels>, vertical: bool, fill: Hsla, ink: Hsla) {
    window.paint_quad(quad(
        Bounds::new(at - point(px(9.0), px(9.0)), size(px(18.0), px(18.0))),
        px(9.0),
        fill,
        Edges::default(),
        fill,
        BorderStyle::Solid,
    ));
    let turn = |x: f32, y: f32| {
        if vertical {
            at + point(px(x), px(y))
        } else {
            at + point(px(-y), px(x))
        }
    };
    for points in [
        [(-4.5, -2.0), (4.5, -2.0)],
        [(2.5, -4.0), (4.5, -2.0)],
        [(4.5, -2.0), (2.5, 0.0)],
        [(4.5, 2.0), (-4.5, 2.0)],
        [(-2.5, 0.0), (-4.5, 2.0)],
        [(-4.5, 2.0), (-2.5, 4.0)],
    ] {
        stroke(
            window,
            turn(points[0].0, points[0].1),
            turn(points[1].0, points[1].1),
            1.4,
            ink,
            None,
        );
    }
}

#[cfg(test)]
pub(crate) mod tests;
