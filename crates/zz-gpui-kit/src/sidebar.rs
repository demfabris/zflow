//! The navigation column at a window's leading edge: its title strip, its
//! footer, the panel that holds them, and the chrome buttons that sit on it.

use crate::{
    ActiveTheme as _, Edge, Ground, IconName, Sizable as _, StyledExt as _, Surface,
    button::Button, roles,
};
use zz_gpui::{
    App, CursorStyle, ElementId, Hsla, IntoElement, ParentElement as _, Pixels, Stateful,
    Styled as _, div, prelude::*, px,
};

pub const SIDEBAR_DEFAULT_WIDTH: f32 = 256.0;
pub const SIDEBAR_CONTENT_INSET: f32 = 8.0;
/// Icon buttons on the chrome, a little larger than list rows.
pub const CHROME_BUTTON_SIZE: Pixels = px(32.0);
const CHROME_ICON_SIZE: Pixels = px(17.0);
const SIDEBAR_EDGE_INSET: f32 = 6.0;
const SIDEBAR_FOOTER_TOP: f32 = 4.0;
const SIDEBAR_RESIZE_HANDLE_WIDTH: f32 = 8.0;
const STATUS_DOT_SIZE: f32 = 6.0;

/// A small round state light.
#[must_use]
pub fn status_dot(color: Hsla) -> zz_gpui::Div {
    div()
        .flex_none()
        .size(crate::rems_from_px(STATUS_DOT_SIZE))
        .rounded_full()
        .bg(color)
}

/// How far the chrome's leading controls sit from the window edge: clear of
/// the traffic lights on macOS.
#[must_use]
pub fn controls_leading_inset(cx: &App) -> Pixels {
    if cfg!(target_os = "macos") {
        crate::macos_traffic_light_clearance(cx)
    } else {
        px(SIDEBAR_CONTENT_INSET)
    }
}

/// An icon button on the window chrome.
#[must_use]
pub fn chrome_button(id: impl Into<ElementId>, icon: IconName, tooltip: &'static str) -> Button {
    Button::compact_icon(id, icon)
        .surface_role(roles::CHROME_BUTTON)
        .with_size(CHROME_ICON_SIZE / 0.75)
        .size(CHROME_BUTTON_SIZE)
        .tooltip(tooltip)
}

fn edge_inset(cx: &App) -> Pixels {
    cx.theme().row_inset + px(SIDEBAR_EDGE_INSET)
}

/// The sidebar's top strip: its controls sit at the trailing edge, clear of
/// the traffic lights.
#[must_use]
pub fn sidebar_titlebar(
    id: impl Into<ElementId>,
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
        .justify_end()
        .pl(controls_leading_inset(cx))
        .pr(edge_inset(cx))
        .child(controls)
}

/// The strip under the sidebar's navigation.
pub fn sidebar_footer(
    id: impl Into<ElementId>,
    leading: impl IntoElement,
    trailing: Option<zz_gpui::AnyElement>,
    cx: &App,
) -> Stateful<zz_gpui::Div> {
    let inset = edge_inset(cx);
    div()
        .id(id)
        .flex()
        .flex_none()
        .w_full()
        .items_center()
        .justify_between()
        .pt(px(SIDEBAR_FOOTER_TOP))
        .pl(inset)
        .pr(inset)
        .pb(inset)
        .child(leading)
        .children(trailing)
}

/// The full-height sidebar on `ground`, what it sits on, with its edge rule
/// while `divider` is set.
#[must_use]
pub fn sidebar_panel(
    id: impl Into<ElementId>,
    width: f32,
    titlebar: impl IntoElement,
    navigation: impl IntoElement,
    ground: Ground,
    divider: bool,
    cx: &App,
) -> Stateful<zz_gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .w(px(width))
        .h_full()
        .min_h_0()
        .flex_none()
        .overflow_hidden()
        .relative()
        .surface(Surface::new(roles::SIDEBAR).on(ground), cx)
        .when(divider, |this| {
            this.rule_on(roles::SIDEBAR_RULE, Edge::Right, cx)
        })
        .child(titlebar)
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .pb(px(8.0))
                .child(navigation),
        )
}

/// The strip along the sidebar's trailing edge that resizes it. The caller
/// attaches the drag.
#[must_use]
pub fn sidebar_resize_handle(
    id: impl Into<ElementId>,
    divider: bool,
    cx: &App,
) -> Stateful<zz_gpui::Div> {
    div()
        .id(id)
        .absolute()
        .top(px(0.0))
        .right(px(0.0))
        .bottom(px(0.0))
        .w(px(SIDEBAR_RESIZE_HANDLE_WIDTH))
        .cursor(CursorStyle::ResizeLeftRight)
        .occlude()
        .when(!divider, |this| {
            this.hover(|this| this.rule_on(roles::SIDEBAR_RULE, Edge::Right, cx))
        })
}
