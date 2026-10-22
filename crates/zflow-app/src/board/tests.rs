use zflow::app::{
    api::{self, PeerState, Request, State, Status},
    layout_model::{Layout, Monitor, Seam},
};
use zz_gpui::{
    AppContext as _, Modifiers, MouseButton, Point, TestAppContext, VisualTestContext, point, px,
};

use super::{Board, geometry::Fit, monitors};
use crate::model::{Model, Requests, Snapshot};

fn screen(id: &str, peer: Option<&str>, x: i32) -> Monitor {
    Monitor {
        id: id.into(),
        label: id.into(),
        peer: peer.map(str::to_owned),
        display: None,
        x,
        y: 0,
        width: 1920,
        height: 1080,
    }
}

/// One paired Mac to the right of this computer.
pub(crate) fn snapshot() -> Snapshot {
    let peer = api::Peer {
        name: "mac".into(),
        state: PeerState::Connected,
        detail: "Connected".into(),
        allow_control: true,
        keyboard: zflow::core::KeyboardMode::Standard,
        reverse_scroll: false,
        share_screen: false,
        mark: "996615".into(),
    };
    api::Snapshot {
        status: Status::new(Some(true), std::slice::from_ref(&peer), &[], false),
        sharing: Some(true),
        health: Vec::new(),
        pairing_window: zflow::pairing_window::View::default(),
        layout: Some(Layout {
            monitors: vec![
                screen("local", None, 0),
                screen("peer:mac", Some("mac"), 1920),
            ],
        }),
        seams: vec![Seam {
            a: "local".into(),
            b: "peer:mac".into(),
            vertical: true,
            at: 1920,
            start: 0,
            end: 1080,
            same_computer: false,
        }],
        own_mark: Some("f04ee5".into()),
        unplaced: Vec::new(),
        peers: vec![peer],
        pause_at_edges: Some(false),
        shortcuts: Vec::new(),
        share_clipboard: Some(true),
        autostart: Some(true),
        config_path: "/etc/zflow/zflow.toml".into(),
        notices: Vec::new(),
        platform: None,
    }
}

/// A board on a fake model, drawn once so it knows its size.
fn board(cx: &mut TestAppContext) -> (Requests, &mut VisualTestContext, Fit) {
    cx.update(crate::theme::init);
    let (model, requests) = {
        let mut requests = None;
        let model = cx.new(|cx| {
            let (model, queue) = Model::fake(snapshot(), cx);
            requests = Some(queue);
            model
        });
        (
            model,
            requests.expect("the fake model hands its requests over"),
        )
    };
    let (board, cx) = cx.add_window_view(|_, cx| Board::new(model, cx));
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    let fit = cx.update(|_, cx| {
        let board = board.read(cx);
        let width = f32::from(board.bounds.get().size.width);
        let snapshot = snapshot();
        Fit::new(monitors(&snapshot), width, super::geometry::BOARD_HEIGHT)
            .expect("two screens fit")
    });
    (requests, cx, fit)
}

fn middle(fit: &Fit, x: i32) -> Point<zz_gpui::Pixels> {
    let rect = fit.rect(&screen("probe", None, x), (0.0, 0.0));
    point(
        px(rect.x + rect.width / 2.0),
        px(rect.y + rect.height / 2.0),
    )
}

#[zz_gpui::test]
fn dragging_a_computer_asks_where_it_lands_then_moves_it(cx: &mut TestAppContext) {
    let (requests, cx, fit) = board(cx);
    let start = middle(&fit, 1920);
    let end = start + point(px(60.0), px(0.0));
    cx.simulate_mouse_move(start, None, Modifiers::default());
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    let landing = serde_json::json!({"x": 1920 + 500, "y": 0, "seams": []});
    let (asked, question) = requests
        .next(landing)
        .expect("the drag asks where it lands");
    assert!(question);
    let Request::Preview {
        id, x, computer, ..
    } = asked
    else {
        panic!("expected a preview, got {asked:?}");
    };
    assert_eq!(id, "peer:mac");
    assert!(computer, "a screen out of adjusting carries its computer");
    assert_eq!(x, 1920 + fit.units(60.0));
    cx.run_until_parked();
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    let moves: Vec<_> = requests
        .drain()
        .into_iter()
        .filter_map(|(request, _)| match request {
            Request::MoveTile {
                id,
                x,
                tolerance,
                computer,
                ..
            } => Some((id, x, tolerance, computer)),
            _ => None,
        })
        .collect();
    assert_eq!(moves.len(), 1, "one move on drop");
    let (id, x, tolerance, computer) = &moves[0];
    assert_eq!(id, "peer:mac");
    assert_eq!(*x, 1920 + fit.units(60.0));
    assert!(*computer);
    assert_eq!(
        *tolerance,
        fit.units(super::geometry::DRAG_TOLERANCE).unsigned_abs()
    );
}

#[zz_gpui::test]
fn a_click_selects_and_arrow_keys_move_the_selected_computer(cx: &mut TestAppContext) {
    let (requests, cx, fit) = board(cx);
    let local = middle(&fit, 0);
    cx.simulate_mouse_down(local, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_up(local, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("right");
    cx.run_until_parked();
    let moved = requests.drain();
    assert!(
        moved.iter().any(|(request, _)| matches!(
            request,
            Request::MoveTile { id, x: 100, tolerance: 99, computer: true, .. } if id == "local"
        )),
        "{moved:?}"
    );
}

#[test]
fn a_status_is_ready_when_a_peer_is_linked() {
    assert_eq!(snapshot().status.state, State::Ready);
}
