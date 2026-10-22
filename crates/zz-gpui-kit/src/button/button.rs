//! The [`Button`] element and its variant/state color matrix.

use std::rc::Rc;

use zz_gpui::{
    AnyElement, App, ClickEvent, Corners, Div, Edges, ElementId, Hsla, InteractiveElement,
    Interactivity, IntoElement, MouseButton, ParentElement, Pixels, RenderOnce, Role, SharedString,
    Stateful, StatefulInteractiveElement as _, StyleRefinement, Styled, Window, div,
    prelude::FluentBuilder as _, px, relative,
};

use crate::{
    ActiveTheme as _, Colorize as _, Disableable, Emphasis, FOCUS_RING_WIDTH, Ground, Icon,
    IconName, Kind, Layer, Paint, PaintState, Selectable, Sizable, Size, StyledExt as _, Surface,
    SurfaceRole, Tone, h_flex, rems_from_px, tooltip::Tooltip,
};

use super::button_icon::ButtonIcon;

pub const COMPACT_ICON_BUTTON_SIZE: f32 = 24.0;
const COMPACT_ICON_DROP: Pixels = px(0.5);

/// Corner radius of a [`Button`]: the theme radius, or an explicit override.
#[derive(Default, Clone, Copy)]
pub enum ButtonRounded {
    #[default]
    Medium,
    Size(Pixels),
}

impl From<Pixels> for ButtonRounded {
    fn from(px: Pixels) -> Self {
        ButtonRounded::Size(px)
    }
}

/// The variant selectors, as a trait so wrappers around a Button can forward
/// them (see [`ButtonVariant`] for the variants themselves).
pub trait ButtonVariants: Sized {
    fn with_variant(self, variant: ButtonVariant) -> Self;

    /// The main action of a surface: primary, solid.
    fn primary(self) -> Self {
        self.with_variant(ButtonVariant::Paint(Tone::Primary, Emphasis::Solid))
    }

    /// An action beside the main one: neutral, soft.
    fn secondary(self) -> Self {
        self.with_variant(ButtonVariant::Paint(Tone::Neutral, Emphasis::Soft))
    }

    fn danger(self) -> Self {
        self.with_variant(ButtonVariant::Paint(Tone::Danger, Emphasis::Soft))
    }

    fn warning(self) -> Self {
        self.with_variant(ButtonVariant::Paint(Tone::Warning, Emphasis::Soft))
    }

    fn success(self) -> Self {
        self.with_variant(ButtonVariant::Paint(Tone::Success, Emphasis::Soft))
    }

    /// A quiet action: neutral, ghost.
    fn ghost(self) -> Self {
        self.with_variant(ButtonVariant::Paint(Tone::Neutral, Emphasis::Ghost))
    }

    fn link(self) -> Self {
        self.with_variant(ButtonVariant::Link)
    }

    /// Unpadded, styled as plain text.
    fn text(self) -> Self {
        self.with_variant(ButtonVariant::Text)
    }
}

/// How a button is drawn: a tone at an emphasis, or one of the two
/// unpadded forms that read as text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonVariant {
    /// A tone at an emphasis, drawn from the theme's [`Paint`].
    Paint(Tone, Emphasis),
    /// Underlined text.
    Link,
    /// Plain text with no padding.
    Text,
}

impl Default for ButtonVariant {
    fn default() -> Self {
        Self::Paint(Tone::Neutral, Emphasis::Soft)
    }
}

impl ButtonVariant {
    #[inline]
    pub fn is_link(&self) -> bool {
        matches!(self, Self::Link)
    }

    #[inline]
    pub fn is_text(&self) -> bool {
        matches!(self, Self::Text)
    }

    #[inline]
    fn no_padding(&self) -> bool {
        self.is_link() || self.is_text()
    }

    /// The surface a painted button draws: its own role, or the one its
    /// emphasis implies. A solid button is the prominent one; a ghost one is
    /// quiet, or an icon button when it shows only an icon.
    fn surface(
        &self,
        role: Option<SurfaceRole>,
        icon_only: bool,
        on: Option<Ground>,
    ) -> Option<Surface> {
        let Self::Paint(tone, emphasis) = *self else {
            return None;
        };
        let role = role.unwrap_or(match emphasis {
            Emphasis::Ghost if icon_only => SurfaceRole::ICON_BUTTON,
            Emphasis::Ghost | Emphasis::Outline => SurfaceRole::QUIET_BUTTON,
            Emphasis::Solid | Emphasis::Soft => Kind::Button.role(),
        });
        Some(
            Surface::new(role)
                .tone(tone)
                .prominent(emphasis == Emphasis::Solid)
                .on_maybe(on),
        )
    }
}

/// A link or text button in `state`: no fill or edge, just its ink.
fn text_layer(variant: ButtonVariant, state: PaintState, ground: Ground, cx: &App) -> Layer {
    let theme = cx.theme();
    let foreground = theme.foreground;
    Layer {
        fill: theme.transparent,
        glass: None,
        border: theme.transparent,
        border_width: px(0.0),
        shadow: Vec::new(),
        text: match (variant, state) {
            (_, PaintState::Disabled) => foreground.faint(),
            (ButtonVariant::Text, PaintState::Active) => foreground.muted(),
            _ => foreground,
        },
        radius: px(0.0),
        ground,
    }
}

#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    base: Stateful<Div>,
    style: StyleRefinement,
    icon: Option<ButtonIcon>,
    label: Option<SharedString>,
    children: Vec<AnyElement>,
    disabled: bool,
    selected: bool,
    variant: ButtonVariant,
    surface_role: Option<SurfaceRole>,
    on: Option<Ground>,
    hover_bg: Option<Hsla>,
    rounded: ButtonRounded,
    flat: bool,
    dropdown_caret: bool,
    size: Size,
    compact: bool,
    tooltip: Option<SharedString>,
    on_click: Option<Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>>,
    on_hover: Option<Rc<dyn Fn(&bool, &mut Window, &mut App)>>,
    loading: bool,

    tab_index: isize,
    tab_stop: bool,
    hit_slop: Option<(f32, f32)>,
}

impl From<Button> for AnyElement {
    fn from(button: Button) -> Self {
        button.into_any_element()
    }
}

impl Button {
    pub fn new(id: impl Into<ElementId>) -> Self {
        let id = id.into();

        Self {
            id: id.clone(),
            base: div().flex_shrink_0().id(id),
            style: StyleRefinement::default(),
            icon: None,
            label: None,
            disabled: false,
            selected: false,
            variant: ButtonVariant::default(),
            surface_role: None,
            on: None,
            hover_bg: None,
            rounded: ButtonRounded::Medium,
            size: Size::Medium,
            tooltip: None,
            on_click: None,
            on_hover: None,
            loading: false,
            compact: false,
            flat: false,
            children: Vec::new(),
            dropdown_caret: false,
            tab_index: 0,
            tab_stop: true,
            hit_slop: None,
        }
    }

    pub fn compact_icon(id: impl Into<ElementId>, icon: impl Into<Icon>) -> Self {
        Self::new(id)
            .ghost()
            .compact()
            .size(rems_from_px(COMPACT_ICON_BUTTON_SIZE))
            .icon(icon.into().relative().top(COMPACT_ICON_DROP))
    }

    pub fn flat(mut self) -> Self {
        self.flat = true;
        self
    }

    /// Replace the variant's hovered fill. Setting `hover` from outside panics,
    /// because the variant matrix already claimed it.
    #[must_use]
    pub fn hover_bg(mut self, bg: Hsla) -> Self {
        self.hover_bg = Some(bg);
        self
    }

    pub fn rounded(mut self, rounded: impl Into<ButtonRounded>) -> Self {
        self.rounded = rounded.into();
        self
    }

    /// Set the label. A button with no label renders as an icon button.
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Set the leading icon.
    pub fn icon(mut self, icon: impl Into<Icon>) -> Self {
        self.icon = Some(ButtonIcon::new(icon));
        self
    }

    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }

    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }

    /// Called with `true` when the pointer enters, `false` when it leaves.
    pub fn on_hover(mut self, handler: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_hover = Some(Rc::new(handler));
        self
    }

    /// Defaults to 0.
    pub fn tab_index(mut self, tab_index: isize) -> Self {
        self.tab_index = tab_index;
        self
    }

    /// Defaults to true.
    pub fn tab_stop(mut self, tab_stop: bool) -> Self {
        self.tab_stop = tab_stop;
        self
    }

    #[must_use]
    pub fn hit_slop(mut self, horizontal: f32, vertical: f32) -> Self {
        self.hit_slop = Some((horizontal, vertical));
        self
    }

    pub fn dropdown_caret(mut self, dropdown_caret: bool) -> Self {
        self.dropdown_caret = dropdown_caret;
        self
    }

    #[inline]
    fn clickable(&self) -> bool {
        !(self.disabled || self.loading) && self.on_click.is_some()
    }

    #[inline]
    fn hoverable(&self) -> bool {
        !(self.disabled || self.loading) && self.on_hover.is_some()
    }
}

impl Button {
    /// Draw the button in `tone`, keeping its emphasis if it has one and
    /// soft otherwise.
    #[must_use]
    pub fn tone(mut self, tone: Tone) -> Self {
        let emphasis = match self.variant {
            ButtonVariant::Paint(_, emphasis) => emphasis,
            _ => Emphasis::default(),
        };
        self.variant = ButtonVariant::Paint(tone, emphasis);
        self
    }

    /// Draw the button at `emphasis`, keeping its tone if it has one and
    /// neutral otherwise.
    #[must_use]
    pub fn emphasis(mut self, emphasis: Emphasis) -> Self {
        let tone = match self.variant {
            ButtonVariant::Paint(tone, _) => tone,
            _ => Tone::default(),
        };
        self.variant = ButtonVariant::Paint(tone, emphasis);
        self
    }

    /// Draw the button as `role` draws, instead of the role its emphasis
    /// implies.
    #[must_use]
    pub fn surface_role(mut self, role: SurfaceRole) -> Self {
        self.surface_role = Some(role);
        self
    }

    /// Paint the button on `ground`, the surface it sits on, instead of the
    /// window.
    #[must_use]
    pub fn on(mut self, ground: Ground) -> Self {
        self.on = Some(ground);
        self
    }
}

impl Disableable for Button {
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl Selectable for Button {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl Sizable for Button {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

impl ButtonVariants for Button {
    fn with_variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }
}

impl Styled for Button {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl ParentElement for Button {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl InteractiveElement for Button {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

fn with_focus_ring<T: ParentElement + Styled + Sized>(
    mut this: T,
    is_focused: bool,
    margins: Pixels,
    paint: &Paint,
    window: &Window,
) -> T {
    if !is_focused {
        return this;
    }

    let rem_size = window.rem_size();
    let style = this.style();

    let border_widths = Edges::<Pixels> {
        top: style
            .border_widths
            .top
            .map(|v| v.to_pixels(rem_size))
            .unwrap_or_default(),
        bottom: style
            .border_widths
            .bottom
            .map(|v| v.to_pixels(rem_size))
            .unwrap_or_default(),
        left: style
            .border_widths
            .left
            .map(|v| v.to_pixels(rem_size))
            .unwrap_or_default(),
        right: style
            .border_widths
            .right
            .map(|v| v.to_pixels(rem_size))
            .unwrap_or_default(),
    };

    let radius = Corners::<Pixels> {
        top_left: style
            .corner_radii
            .top_left
            .map(|v| v.to_pixels(rem_size))
            .unwrap_or_default(),
        top_right: style
            .corner_radii
            .top_right
            .map(|v| v.to_pixels(rem_size))
            .unwrap_or_default(),
        bottom_left: style
            .corner_radii
            .bottom_left
            .map(|v| v.to_pixels(rem_size))
            .unwrap_or_default(),
        bottom_right: style
            .corner_radii
            .bottom_right
            .map(|v| v.to_pixels(rem_size))
            .unwrap_or_default(),
    }
    .map(|v| *v + FOCUS_RING_WIDTH);

    let mut inner_style = StyleRefinement::default();
    inner_style.corner_radii.top_left = Some(radius.top_left.into());
    inner_style.corner_radii.top_right = Some(radius.top_right.into());
    inner_style.corner_radii.bottom_left = Some(radius.bottom_left.into());
    inner_style.corner_radii.bottom_right = Some(radius.bottom_right.into());

    let inset = FOCUS_RING_WIDTH + margins;

    this.child(
        div()
            .flex_none()
            .absolute()
            .top(-(inset + border_widths.top))
            .left(-(inset + border_widths.left))
            .right(-(inset + border_widths.right))
            .bottom(-(inset + border_widths.bottom))
            .focus_ring(paint)
            .refine_style(&inner_style),
    )
}

impl RenderOnce for Button {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let variant = self.variant;
        let ground = self.on.unwrap_or_else(|| cx.theme().ground());
        let icon_only = self.label.is_none() && self.children.is_empty();
        let surface = variant.surface(self.surface_role, icon_only, self.on);
        let flat = self.flat;
        let layer_at = move |state: PaintState, cx: &App| -> Layer {
            let Some(surface) = surface else {
                return text_layer(variant, state, ground, cx);
            };
            let mut layer = cx.theme().layer(surface, state);
            if flat {
                layer.border = cx.theme().transparent;
                layer.shadow.clear();
            }
            layer
        };
        let clickable = self.clickable();
        let is_disabled = self.disabled;
        let hoverable = self.hoverable();
        let rest = layer_at(
            if self.disabled {
                PaintState::Disabled
            } else if self.selected {
                PaintState::Current
            } else {
                PaintState::Rest
            },
            cx,
        );
        let icon_size = match self.size {
            Size::Size(v) => Size::Size(v * 0.75),
            _ => self.size,
        };

        let focus_handle = window
            .use_keyed_state(self.id.clone(), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let is_focused = focus_handle.is_focused(window);

        let rounding = match self.rounded {
            ButtonRounded::Medium => rest.radius,
            ButtonRounded::Size(px) => px,
        };
        let coarse = crate::touch::CoarsePointer::get(cx);
        let slop = self.hit_slop.filter(|_| coarse);
        let press = (coarse && clickable)
            .then(|| crate::touch::press_feedback(self.id.clone(), window, cx));
        let pressed = press.as_ref().map_or(0.0, |press| press.amount);
        let press_listener = press.map(crate::touch::PressFeedback::listener);
        let press_fill = layer_at(PaintState::Active, cx).fill;
        let hover = layer_at(PaintState::Hover, cx);
        let active = layer_at(PaintState::Active, cx);
        let hover_bg = self.hover_bg;
        let on_mouse_down =
            move |_: &zz_gpui::MouseDownEvent, window: &mut Window, cx: &mut App| {
                if is_disabled {
                    cx.stop_propagation();
                    return;
                }

                window.prevent_default();

                crate::text::suppress_text_selection(cx);
            };
        let on_click = self.on_click.map(|on_click| {
            move |event: &ClickEvent, window: &mut Window, cx: &mut App| {
                if !clickable {
                    cx.stop_propagation();
                    return;
                }

                on_click(event, window, cx);
            }
        });

        let element = self
            .base
            .role(if self.variant.is_link() {
                Role::Link
            } else {
                Role::Button
            })
            .when_some(
                self.label.as_ref().or(self.tooltip.as_ref()),
                |this, label| this.aria_label(label.clone()),
            )
            .aria_selected(self.selected)
            .aria_disabled(is_disabled)
            .when(!self.disabled, |this| {
                this.track_focus(
                    &focus_handle
                        .tab_index(self.tab_index)
                        .tab_stop(self.tab_stop),
                )
            })
            .cursor_default()
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .when(self.variant.is_link(), |this| this.cursor_pointer())
            .when(!variant.no_padding(), |this| {
                let height = self.size.control_h();
                if icon_only {
                    this.size(height)
                } else {
                    let this = this.h(height);
                    match self.size {
                        Size::Size(size) => this.px(size * 0.2),
                        Size::Small => this
                            .px_2()
                            .when(self.compact, |this| this.min_w(height).px_1()),
                        Size::Medium => this
                            .px_3()
                            .when(self.compact, |this| this.min_w(height).px_1p5()),
                    }
                }
            })
            .rounded(rounding)
            .border(rest.border_width)
            .border_color(rest.border)
            .shadow(rest.shadow.clone())
            .text_color(rest.text)
            .map(|this| match rest.glass {
                Some(material) => this.glass(material),
                None => this.bg(rest.fill),
            })
            .when(variant.is_link() && !self.disabled, |this| {
                this.text_decoration_1()
            })
            .when(!self.disabled && !self.selected && !coarse, |this| {
                this.hover(move |style| match hover_bg {
                    Some(bg) => style
                        .border_color(hover.border)
                        .shadow(hover.shadow.clone())
                        .text_color(hover.text)
                        .bg(bg),
                    None => crate::foundation::restyle(style, &hover),
                })
                .active(move |style| crate::foundation::restyle(style, &active))
            })
            .refine_style(&self.style)
            .when(pressed > 0.0, |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .rounded(rounding)
                        .bg(press_fill)
                        .opacity(pressed),
                )
            })
            .map(|this| match slop {
                None => this
                    .on_mouse_down(MouseButton::Left, on_mouse_down)
                    .when_some(on_click, |this, on_click| this.on_click(on_click))
                    .children(press_listener),
                Some((x, y)) => this.child(
                    crate::touch::hit_area("touch", x, y)
                        .children(press_listener)
                        .on_mouse_down(MouseButton::Left, on_mouse_down)
                        .when_some(on_click, |this, on_click| {
                            this.on_click(move |event, window, cx| {
                                on_click(event, window, cx);
                                cx.stop_propagation();
                            })
                        }),
                ),
            })
            .when_some(self.on_hover.filter(|_| hoverable), |this, on_hover| {
                this.on_hover(move |hovered, window, cx| {
                    on_hover(hovered, window, cx);
                })
            })
            .child({
                let label = h_flex()
                    .id("label")
                    .size_full()
                    .items_center()
                    .justify_center();

                label
                    .text_size(self.size.control_text())
                    .when(pressed > 0.0, |this| this.opacity(1.0 - 0.35 * pressed))
                    .map(|this| match self.size {
                        Size::Small => this.gap_1(),
                        _ => this.gap_1p5(),
                    })
                    .when_some(self.icon, |this, icon| {
                        this.child(icon.loading(self.loading).with_size(icon_size))
                    })
                    .when_some(self.label, |this, label| {
                        this.child(div().flex_none().line_height(relative(1.)).child(label))
                    })
                    .children(self.children)
                    .when(self.dropdown_caret, |this| {
                        this.justify_between().child(
                            Icon::new(IconName::ChevronDown).small().text_color(
                                match self.disabled {
                                    true => rest.text.opacity(0.3),
                                    false => rest.text.opacity(0.5),
                                },
                            ),
                        )
                    })
            })
            .when(self.loading && !self.disabled, |this| {
                this.bg(rest.fill.opacity(0.8))
                    .border_color(rest.border.opacity(0.8))
                    .text_color(rest.text.opacity(0.8))
            })
            .when_some(self.tooltip, |this, tooltip| {
                this.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            });

        let tone = match variant {
            ButtonVariant::Paint(tone, _) => tone,
            _ => Tone::Neutral,
        };
        let focus_paint = cx.theme().paint_on(ground, tone, Emphasis::Ghost);
        with_focus_ring(element, is_focused, px(0.), &focus_paint, window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[zz_gpui::test]
    fn test_button_clickable_logic(_cx: &mut zz_gpui::TestAppContext) {
        let clickable = Button::new("test").on_click(|_, _, _| {});
        assert!(clickable.clickable());

        let disabled = Button::new("test").disabled(true).on_click(|_, _, _| {});
        assert!(!disabled.clickable());

        let loading = Button::new("test").loading(true).on_click(|_, _, _| {});
        assert!(!loading.clickable());
    }

    #[zz_gpui::test]
    fn test_button_hoverable_logic(_cx: &mut zz_gpui::TestAppContext) {
        assert!(!Button::new("test").hoverable());

        assert!(Button::new("test").on_hover(|_, _, _| {}).hoverable());

        assert!(
            !Button::new("test")
                .disabled(true)
                .on_hover(|_, _, _| {})
                .hoverable()
        );

        assert!(
            !Button::new("test")
                .loading(true)
                .on_hover(|_, _, _| {})
                .hoverable()
        );
    }

    #[zz_gpui::test]
    fn test_button_variant_methods(_cx: &mut zz_gpui::TestAppContext) {
        assert!(ButtonVariant::Link.is_link());
        assert!(ButtonVariant::Text.is_text());

        assert!(ButtonVariant::Link.no_padding());
        assert!(ButtonVariant::Text.no_padding());
        assert!(!ButtonVariant::default().no_padding());
        assert_eq!(
            ButtonVariant::default()
                .surface(None, false, None)
                .map(|surface| surface.role),
            Some(Kind::Button.role())
        );
        assert_eq!(
            Button::new("b")
                .ghost()
                .variant
                .surface(None, true, None)
                .map(|surface| surface.role),
            Some(SurfaceRole::ICON_BUTTON)
        );
    }

    #[zz_gpui::test]
    fn the_shorthands_name_a_tone_and_an_emphasis(_cx: &mut zz_gpui::TestAppContext) {
        let variant = |button: Button| button.variant;
        assert_eq!(
            variant(Button::new("b").primary()),
            ButtonVariant::Paint(Tone::Primary, Emphasis::Solid)
        );
        assert_eq!(
            variant(Button::new("b").ghost()),
            ButtonVariant::Paint(Tone::Neutral, Emphasis::Ghost)
        );
        assert_eq!(
            variant(Button::new("b").danger().emphasis(Emphasis::Outline)),
            ButtonVariant::Paint(Tone::Danger, Emphasis::Outline)
        );
    }
}
