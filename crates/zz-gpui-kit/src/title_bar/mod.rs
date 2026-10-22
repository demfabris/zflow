//! The window title bar: a draggable strip that owns the top of a window.

use std::rc::Rc;

use smallvec::SmallVec;
use zz_gpui::{
    AnyElement, App, ClickEvent, Decorations, Div, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Pixels, Point, RenderOnce, Stateful, StatefulInteractiveElement as _,
    StyleRefinement, Styled, TitlebarOptions, Window, WindowControlArea, div, point,
    prelude::FluentBuilder as _, px,
};

use crate::UiZoom;
use crate::{
    ActiveTheme as _, Edge, Icon, IconName, InteractiveElementExt as _, Kind, Layer, PaintState,
    StyledExt as _, Surface, SurfaceRole, Tone, foundation::restyle, h_flex,
};

/// Height of the title bar while panes sit flush: one pane header. With a
/// pane margin it grows to [`Theme::title_bar_height`](crate::Theme::title_bar_height),
/// which every strip that lines up with it (the sidebar headers in zz-ui's
/// navigation and settings) reads.
pub const TITLE_BAR_HEIGHT: Pixels = crate::PANE_HEADER_HEIGHT;

/// Side of a native macOS window-button frame, and the width of all three
/// plus their gaps. Measured on macOS 27: each frame is 14x14, origins sit 23
/// apart, so the cluster spans 60 from the first left edge to the last right
/// edge. The platform layer centres the buttons in a container of
/// `glyph + 2 * traffic_light_position.y`, which is what ties them to the
/// title bar's height: keep the two in step or the lights stop sharing a
/// centre line with the strip's own controls.
pub const MACOS_TRAFFIC_LIGHT_GLYPH: f32 = 14.;
pub const MACOS_TRAFFIC_LIGHT_SPAN: f32 = 60.;

/// Space a strip reserves for the macOS traffic lights: the lights keep as
/// much room from the window's leading edge as from its top, and the strip's
/// own controls the same from the cluster's far edge, so the gap on either
/// side of the lights reads as one measurement. Both margins ride the zoom
/// with the lights' placement, the native glyphs do not.
#[must_use]
pub fn macos_traffic_light_clearance(cx: &App) -> Pixels {
    let margin =
        (cx.theme().title_bar_height() - UiZoom::unzoomed(px(MACOS_TRAFFIC_LIGHT_GLYPH), cx)) / 2.;
    margin * 2. + UiZoom::unzoomed(px(MACOS_TRAFFIC_LIGHT_SPAN), cx)
}

/// Whether the app draws the window's minimize / maximize / close buttons:
/// Windows outside fullscreen, and Linux under [`Decorations::Client`]. macOS
/// keeps its native traffic lights, and WASM has no window to control.
#[must_use]
pub fn draws_window_controls(window: &Window) -> bool {
    (cfg!(target_os = "windows") && !window.is_fullscreen())
        || (cfg!(target_os = "linux")
            && matches!(window.window_decorations(), Decorations::Client { .. }))
}

#[must_use]
pub fn window_controls_width(window: &Window) -> Pixels {
    if draws_window_controls(window) {
        px(f32::from(CONTROL_ICON_WIDTH) * 3.0)
    } else {
        px(0.0)
    }
}

/// Left inset that clears the native macOS traffic lights, in window points.
#[cfg(target_os = "macos")]
fn title_bar_left_padding(cx: &App) -> Pixels {
    macos_traffic_light_clearance(cx)
}
#[cfg(not(target_os = "macos"))]
fn title_bar_left_padding(_cx: &App) -> Pixels {
    px(12.)
}

type CloseHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// The window title bar. Children are laid out in the draggable region, to the
/// left of the window controls.
#[derive(IntoElement)]
pub struct TitleBar {
    style: StyleRefinement,
    children: SmallVec<[AnyElement; 1]>,
    on_close_window: Option<CloseHandler>,
}

impl TitleBar {
    #[must_use]
    pub fn new() -> Self {
        Self {
            style: StyleRefinement::default(),
            children: SmallVec::new(),
            on_close_window: None,
        }
    }

    /// The [`TitlebarOptions`] a window must open with for [`TitleBar`] to sit
    /// correctly in it: transparent, untitled, traffic lights placed to match
    /// a bar `height` tall and its left padding.
    #[must_use]
    pub fn title_bar_options(height: Pixels) -> TitlebarOptions {
        TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: Some(Self::traffic_light_position(height, 1.)),
            ..TitlebarOptions::default()
        }
    }

    /// Where the macOS traffic lights sit at `zoom` in a bar `height` tall, in
    /// window points: on the bar's zoomed centre line, as far in from the
    /// leading edge as from the top, at native size.
    #[must_use]
    pub fn traffic_light_position(height: Pixels, zoom: f32) -> Point<Pixels> {
        let margin = (height * zoom - px(MACOS_TRAFFIC_LIGHT_GLYPH)) / 2.;
        point(margin, margin)
    }

    /// Run `f` instead of `Window::remove_window` when the close button is
    /// clicked. Linux only: elsewhere the button is absent or hit-tested by the
    /// OS, so there is no click to intercept.
    #[must_use]
    pub fn on_close_window(
        mut self,
        f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        if cfg!(target_os = "linux") {
            self.on_close_window = Some(Rc::new(f));
        }
        self
    }
}

impl Default for TitleBar {
    fn default() -> Self {
        Self::new()
    }
}

impl Styled for TitleBar {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl ParentElement for TitleBar {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

#[cfg(not(target_os = "windows"))]
const CONTROL_ICON_WIDTH: Pixels = TITLE_BAR_HEIGHT;
/// The Windows 11 shell's own caption button width.
#[cfg(target_os = "windows")]
const CONTROL_ICON_WIDTH: Pixels = px(46.);

#[derive(IntoElement)]
struct WindowControl {
    icon: ControlIcon,
    role: SurfaceRole,
}

#[derive(Clone)]
enum ControlIcon {
    Minimize,
    Restore,
    Maximize,
    Close {
        on_close_window: Option<CloseHandler>,
    },
}

impl ControlIcon {
    fn minimize() -> Self {
        Self::Minimize
    }

    fn restore() -> Self {
        Self::Restore
    }

    fn maximize() -> Self {
        Self::Maximize
    }

    fn close(on_close_window: Option<CloseHandler>) -> Self {
        Self::Close { on_close_window }
    }

    fn id(&self) -> &'static str {
        match self {
            Self::Minimize => "minimize",
            Self::Restore => "restore",
            Self::Maximize => "maximize",
            Self::Close { .. } => "close",
        }
    }

    fn icon(&self) -> IconName {
        match self {
            Self::Minimize => IconName::WindowMinimize,
            Self::Restore => IconName::WindowRestore,
            Self::Maximize => IconName::WindowMaximize,
            Self::Close { .. } => IconName::WindowClose,
        }
    }

    fn window_control_area(&self) -> WindowControlArea {
        match self {
            Self::Minimize => WindowControlArea::Min,
            Self::Restore | Self::Maximize => WindowControlArea::Max,
            Self::Close { .. } => WindowControlArea::Close,
        }
    }

    fn is_close(&self) -> bool {
        matches!(self, Self::Close { .. })
    }
}

impl WindowControl {
    fn layer(&self, state: PaintState, cx: &App) -> Layer {
        let theme = cx.theme();
        match state {
            PaintState::Hover | PaintState::Active if self.icon.is_close() => theme.layer(
                Surface::new(self.role).tone(Tone::Danger).prominent(true),
                if state == PaintState::Hover {
                    PaintState::Rest
                } else {
                    PaintState::Active
                },
            ),
            state => theme.layer(self.role, state),
        }
    }
}

impl RenderOnce for WindowControl {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let is_linux = cfg!(target_os = "linux");
        let is_windows = cfg!(target_os = "windows");
        let hover = self.layer(PaintState::Hover, cx);
        let active = self.layer(PaintState::Active, cx);
        let rest = self.layer(PaintState::Rest, cx);
        let icon = self.icon.clone();
        let on_close_window = match &self.icon {
            ControlIcon::Close { on_close_window } => on_close_window.clone(),
            _ => None,
        };
        let control = self.icon;

        div()
            .id(control.id())
            .flex()
            .w(CONTROL_ICON_WIDTH)
            .h_full()
            .flex_shrink_0()
            .justify_center()
            .content_center()
            .items_center()
            .layer(&rest)
            .hover(move |style| restyle(style, &hover))
            .active(move |style| restyle(style, &active))
            .when(is_windows, |this| {
                this.window_control_area(control.window_control_area())
            })
            .when(is_linux, |this| {
                this.on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    match icon {
                        ControlIcon::Minimize => window.minimize_window(),
                        ControlIcon::Restore | ControlIcon::Maximize => window.zoom_window(),
                        ControlIcon::Close { .. } => {
                            if let Some(f) = on_close_window.clone() {
                                f(&ClickEvent::default(), window, cx);
                            } else {
                                window.remove_window();
                            }
                        }
                    }
                })
            })
            .child(Icon::new(control.icon()).medium())
    }
}

/// The trailing minimize / maximize / close cluster. Empty wherever
/// [`draws_window_controls`] is false. Mountable on its own, without a
/// [`TitleBar`].
#[derive(IntoElement)]
pub struct WindowControls {
    on_close_window: Option<CloseHandler>,
    surface_role: SurfaceRole,
}

impl Default for WindowControls {
    fn default() -> Self {
        Self {
            on_close_window: None,
            // A role under the icon button, so the controls keep its bare,
            // unfilled look; `ICON_BUTTON.with` would replace it.
            surface_role: crate::roles::WINDOW_CONTROLS,
        }
    }
}

impl WindowControls {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Draw the controls' hover and press as `role` draws, instead of as
    /// square icon buttons.
    #[must_use]
    pub fn surface_role(mut self, role: SurfaceRole) -> Self {
        self.surface_role = role;
        self
    }

    /// Run `f` instead of `Window::remove_window` when close is clicked. Linux
    /// only, as with [`TitleBar::on_close_window`]; on Windows the veto belongs
    /// on `Window::on_window_should_close`.
    #[must_use]
    pub fn on_close_window(
        mut self,
        f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        if cfg!(target_os = "linux") {
            self.on_close_window = Some(Rc::new(f));
        }
        self
    }
}

impl RenderOnce for WindowControls {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        if !draws_window_controls(window) {
            return div().id("window-controls");
        }

        let role = self.surface_role;
        let control = move |icon| WindowControl { icon, role };
        h_flex()
            .id("window-controls")
            .items_center()
            .flex_shrink_0()
            .h_full()
            .child(control(ControlIcon::minimize()))
            .child(control(if window.is_maximized() {
                ControlIcon::restore()
            } else {
                ControlIcon::maximize()
            }))
            .child(control(ControlIcon::close(self.on_close_window)))
    }
}

struct TitleBarState {
    should_move: bool,
}

fn arm_window_move(bar: Stateful<Div>, window: &mut Window, cx: &mut App) -> Stateful<Div> {
    if cfg!(target_os = "windows") {
        return bar;
    }
    let state = window.use_state(cx, |_, _| TitleBarState { should_move: false });
    bar.on_mouse_down_out(window.listener_for(&state, |state, _, _, _| {
        state.should_move = false;
    }))
    .on_mouse_down(
        MouseButton::Left,
        window.listener_for(&state, |state, _, _, _| {
            state.should_move = true;
        }),
    )
    .on_mouse_up(
        MouseButton::Left,
        window.listener_for(&state, |state, _, _, _| {
            state.should_move = false;
        }),
    )
    .on_mouse_move(window.listener_for(&state, |state, _, window, _| {
        if state.should_move {
            state.should_move = false;
            window.start_window_move();
        }
    }))
}

impl RenderOnce for TitleBar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let is_client_decorated = matches!(window.window_decorations(), Decorations::Client { .. });
        let is_web = cfg!(target_family = "wasm");
        let is_linux = cfg!(target_os = "linux");
        let is_macos = cfg!(target_os = "macos");
        let is_fullscreen = window.is_fullscreen();

        div().flex_shrink_0().child(
            div()
                .id("title-bar")
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .h(cx.theme().title_bar_height())
                .pl(title_bar_left_padding(cx))
                .rule_on(Kind::Rule.role(), Edge::Bottom, cx)
                .bg(cx.theme().background)
                .refine_style(&self.style)
                .when(is_linux, |this| {
                    this.on_double_click(|_, window, _| window.zoom_window())
                })
                .when(is_macos, |this| {
                    this.on_double_click(|_, window, _| window.titlebar_double_click())
                })
                .map(|bar| arm_window_move(bar, window, cx))
                .child(
                    h_flex()
                        .id("bar")
                        .h_full()
                        .justify_between()
                        .flex_shrink_0()
                        .flex_1()
                        .when(!is_web, |this| {
                            this.window_control_area(WindowControlArea::Drag)
                                .when(is_fullscreen, zz_gpui::Styled::pl_3)
                                .when(is_linux && is_client_decorated, |this| {
                                    this.child(
                                        div()
                                            .top_0()
                                            .left_0()
                                            .absolute()
                                            .size_full()
                                            .h_full()
                                            .on_mouse_down(
                                                MouseButton::Right,
                                                move |ev, window, _| {
                                                    window.show_window_menu(ev.position);
                                                },
                                            ),
                                    )
                                })
                        })
                        .children(self.children),
                )
                .child(WindowControls {
                    on_close_window: self.on_close_window,
                    ..WindowControls::default()
                }),
        )
    }
}
