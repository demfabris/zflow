//! An app window's frame: a sidebar column beside a content column, the
//! title-height strip over the content that carries the window controls,
//! and the drag that lets either strip move the window.

use crate::{ActiveTheme as _, InteractiveElementExt};
use zz_gpui::{
    AnyElement, App, ElementId, Hsla, InteractiveElement as _, IntoElement, MouseButton,
    MouseMoveEvent, ParentElement as _, Stateful, Styled as _, Window, WindowControlArea, div,
    prelude::FluentBuilder,
};

/// The chrome's fill: translucent while the window behind it blurs.
#[allow(
    clippy::disallowed_methods,
    reason = "blurred chrome lets the window material show through"
)]
pub fn chrome_background(background: Hsla, blur: bool) -> Hsla {
    if blur {
        background.opacity(0.93)
    } else {
        background
    }
}

/// The whole window: `sidebar` beside a column of `titlebar` over `content`,
/// with `overlays` such as the dialog and notification layers on top.
pub fn app_shell_surface(
    id: impl Into<ElementId>,
    background: Hsla,
    sidebar: impl IntoElement,
    titlebar: Option<AnyElement>,
    content: impl IntoElement,
    overlays: impl IntoIterator<Item = AnyElement>,
) -> Stateful<zz_gpui::Div> {
    div()
        .id(id)
        .relative()
        .flex()
        .flex_col()
        .size_full()
        .overflow_hidden()
        .bg(background)
        .child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .overflow_hidden()
                .child(sidebar)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .overflow_hidden()
                        .children(titlebar)
                        .child(
                            div()
                                .flex()
                                .flex_1()
                                .min_w_0()
                                .min_h_0()
                                .overflow_hidden()
                                .child(content),
                        ),
                ),
        )
        .children(overlays)
}

/// A title-height strip over the content column: `leading` and a drag
/// region, then `controls` at its trailing end, such as the client-side
/// window buttons.
pub fn app_titlebar_strip(
    id: impl Into<ElementId>,
    leading: impl IntoElement,
    controls: impl IntoElement,
    cx: &App,
) -> Stateful<zz_gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .w_full()
        .h(cx.theme().title_bar_height())
        .items_center()
        .child(
            div()
                .flex()
                .flex_1()
                .h_full()
                .min_w_0()
                .items_center()
                .window_control_area(WindowControlArea::Drag)
                .child(leading),
        )
        .child(controls)
}

struct DragCandidate {
    armed: bool,
    accepts_click: bool,
}

/// Drags `strip` to move the window, and a double click zooms it, the way a
/// title bar does. `key` must be unique among the strips drawn at once. The
/// caller declares the `WindowControlArea::Drag` hitbox, which is all Windows
/// needs.
pub fn window_drag_handle<E: InteractiveElementExt + FluentBuilder>(
    key: impl Into<ElementId>,
    strip: E,
    window: &mut Window,
    cx: &mut App,
) -> E {
    if cfg!(target_os = "windows") {
        return strip;
    }
    let state = window.use_keyed_state(key.into(), cx, |_, _| DragCandidate {
        armed: false,
        accepts_click: false,
    });
    strip
        .when(cfg!(target_os = "linux"), |strip| {
            strip.on_double_click(window.listener_for(&state, |state, _, window, _| {
                if state.accepts_click {
                    window.zoom_window();
                }
            }))
        })
        .when(cfg!(target_os = "macos"), |strip| {
            strip.on_double_click(window.listener_for(&state, |state, _, window, _| {
                if state.accepts_click {
                    window.titlebar_double_click();
                }
            }))
        })
        .on_mouse_down_out(window.listener_for(&state, |state, _, _, _| {
            state.armed = false;
            state.accepts_click = false;
        }))
        .on_mouse_down(
            MouseButton::Left,
            window.listener_for(&state, |state, _, window, _| {
                state.accepts_click = !window.default_prevented();
                state.armed = state.accepts_click;
            }),
        )
        .on_mouse_up(
            MouseButton::Left,
            window.listener_for(&state, |state, _, _, _| {
                state.armed = false;
            }),
        )
        .on_mouse_move(
            window.listener_for(&state, |state, event: &MouseMoveEvent, window, _| {
                if state.armed {
                    state.armed = false;
                    if event.pressed_button == Some(MouseButton::Left) {
                        window.start_window_move();
                    }
                }
            }),
        )
}
