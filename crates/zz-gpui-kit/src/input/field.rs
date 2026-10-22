//! [`Input`]: the chrome around a text field, rebuilt every frame.

use zz_gpui::{
    AnyElement, App, DefiniteLength, Entity, InteractiveElement, IntoElement, MouseButton,
    ParentElement as _, RenderOnce, Role, StatefulInteractiveElement as _, StyleRefinement, Styled,
    TextAlign, Window, div, prelude::FluentBuilder as _, px,
};

use crate::{
    ActiveTheme as _, Disableable, Emphasis, Ground, Kind, Layer, Paint, PaintState, Sizable, Size,
    StyledExt as _, Surface, SurfaceRole, Theme, Tone,
    button::Button,
    foundation::restyle,
    h_flex,
    icon::{Icon, IconName},
    spinner::Spinner,
};

use super::{actions, state::InputState};
use crate::Colorize as _;

const LINE_HEIGHT: DefiniteLength = DefiniteLength::Fraction(1.25);
/// A semantic hint about what a field holds, for the accessibility role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputContentType {
    Url,
    EmailAddress,
    /// A password: the rendered text is masked, single-line only. Copy and cut
    /// do nothing, and word motions treat the value as one word.
    Password,
}

impl InputContentType {
    const fn role(self) -> Role {
        match self {
            Self::Url => Role::UrlInput,
            Self::EmailAddress => Role::EmailInput,
            Self::Password => Role::PasswordInput,
        }
    }

    const fn masks(self) -> bool {
        matches!(self, Self::Password)
    }
}

/// A text field bound to an [`InputState`].
#[derive(IntoElement)]
pub struct Input {
    state: Entity<InputState>,
    style: StyleRefinement,
    size: Size,
    prefix: Option<AnyElement>,
    suffix: Option<AnyElement>,
    appearance: bool,
    bordered: bool,
    focus_bordered: bool,
    cleanable: bool,
    disabled: bool,
    tab_index: isize,
    content_type: Option<InputContentType>,
    align: TextAlign,
    tone: Tone,
    surface_role: Option<SurfaceRole>,
    on: Option<Ground>,
}

/// The paint of a field drawn as `surface`: its tone, on its ground or the
/// window, at the emphasis its role's fill and outline make. Its text,
/// placeholder and focus colors are the field's.
pub fn field_paint(surface: impl Into<Surface>, cx: &App) -> Paint {
    let surface = surface.into();
    let theme = cx.theme();
    theme.paint_on(
        surface.ground.unwrap_or_else(|| theme.ground()),
        surface.tone,
        theme.field_emphasis(surface.role),
    )
}

/// A field's fill, border, corners and text in `state`, drawn as `surface`
/// draws, with the hover highlight while it rests. A role with no fill and
/// no outline draws no box. The border is always at least a pixel wide,
/// clear when the role has none, so focusing a field never moves its text;
/// focus turns it to the field's focus color, unless the theme draws no
/// outlines or the field is glass, whose rim is its edge. A field that holds
/// more than its text, like the agent composer, draws its shell with this.
pub fn field_chrome<E: Styled + InteractiveElement>(
    element: E,
    surface: impl Into<Surface>,
    state: PaintState,
    bordered: bool,
    focus_bordered: bool,
    cx: &App,
) -> E {
    let surface = surface.into();
    let theme = cx.theme();
    let layer = field_layer(theme, surface, state, focus_bordered);
    let element = element.layer(&layer);
    let element = if bordered {
        element
    } else {
        element.border(px(0.0))
    };
    if state == PaintState::Rest {
        let hover = field_layer(theme, surface, PaintState::Hover, focus_bordered);
        element.hover(move |style| restyle(style, &hover))
    } else {
        element
    }
}

fn field_layer(theme: &Theme, surface: Surface, state: PaintState, focus_bordered: bool) -> Layer {
    let mut layer = theme.layer(surface, state);
    layer.border_width = layer.border_width.max(px(1.0));
    if state == PaintState::Focused && focus_bordered && theme.outlined() && layer.glass.is_none() {
        layer.border = theme
            .paint_on(
                surface.ground.unwrap_or_else(|| theme.ground()),
                surface.tone,
                Emphasis::Ghost,
            )
            .focus;
    }
    layer
}

/// The text size of a field at `size`.
pub(crate) fn field_text<E: Styled>(element: E, size: Size) -> E {
    element.text_size(size.control_text())
}

impl Input {
    pub fn new(state: &Entity<InputState>) -> Self {
        Self {
            state: state.clone(),
            style: StyleRefinement::default(),
            size: Size::default(),
            prefix: None,
            suffix: None,
            appearance: true,
            bordered: true,
            focus_bordered: true,
            cleanable: false,
            disabled: false,
            tab_index: 0,
            content_type: None,
            align: TextAlign::Left,
            tone: Tone::Neutral,
            surface_role: None,
            on: None,
        }
    }

    /// Draw the field in `tone`: danger for an invalid value.
    #[must_use]
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }

    /// Draw the field as `role` draws, instead of as a field.
    #[must_use]
    pub fn surface_role(mut self, role: SurfaceRole) -> Self {
        self.surface_role = Some(role);
        self
    }

    /// Draw the field on `ground`, the paint it sits inside, instead of the
    /// window.
    #[must_use]
    pub fn on(mut self, ground: Ground) -> Self {
        self.on = Some(ground);
        self
    }

    /// Place the text horizontally within the field. Hit-testing, the caret and
    /// selection quads all follow the alignment.
    #[must_use]
    pub fn text_align(mut self, align: TextAlign) -> Self {
        self.align = align;
        self
    }

    #[must_use]
    pub fn prefix(mut self, prefix: impl IntoElement) -> Self {
        self.prefix = Some(prefix.into_any_element());
        self
    }

    #[must_use]
    pub fn suffix(mut self, suffix: impl IntoElement) -> Self {
        self.suffix = Some(suffix.into_any_element());
        self
    }

    /// Draw the field's background, radius and border. With `false` the field
    /// is bare text, for a field embedded in chrome that already has an edge.
    #[must_use]
    pub fn appearance(mut self, appearance: bool) -> Self {
        self.appearance = appearance;
        self
    }

    /// Draw the resting border. Only meaningful with [`Self::appearance`].
    #[must_use]
    pub fn bordered(mut self, bordered: bool) -> Self {
        self.bordered = bordered;
        self
    }

    /// Draw the focus ring. Only meaningful with [`Self::bordered`].
    #[must_use]
    pub fn focus_bordered(mut self, bordered: bool) -> Self {
        self.focus_bordered = bordered;
        self
    }

    /// Show a clear button while the field is non-empty. Single-line only.
    #[must_use]
    pub fn cleanable(mut self, cleanable: bool) -> Self {
        self.cleanable = cleanable;
        self
    }

    #[must_use]
    pub fn tab_index(mut self, index: isize) -> Self {
        self.tab_index = index;
        self
    }

    /// What the field holds, for assistive technology.
    #[must_use]
    pub fn content_type(mut self, content_type: InputContentType) -> Self {
        self.content_type = Some(content_type);
        self
    }

    fn field_surface(&self, multi_line: bool) -> Surface {
        let field = if multi_line {
            SurfaceRole::TEXT_AREA
        } else {
            Kind::Field.role()
        };
        Surface::new(self.surface_role.unwrap_or(field))
            .tone(self.tone)
            .on_maybe(self.on)
    }

    fn sized<E: Styled>(element: E, size: Size) -> E {
        field_text(element.px(size.input_px()).py(size.input_py()), size)
    }

    fn height<E: Styled>(element: E, size: Size) -> E {
        element.h(size.control_h())
    }

    fn clear_button(state: &Entity<InputState>, cx: &App) -> Button {
        let state = state.clone();
        Button::new("zz-input-clear")
            .icon(Icon::new(IconName::CircleX))
            .emphasis(Emphasis::Ghost)
            .small()
            .tab_stop(false)
            .text_color(cx.theme().foreground.muted())
            .on_click(move |_, window, cx| {
                state.update(cx, |this, cx| {
                    this.clear(cx);
                    this.focus(window, cx);
                });
            })
    }
}

impl Sizable for Input {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

impl Disableable for Input {
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl Styled for Input {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Input {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let multi_line = self.state.read(cx).mode().is_multi_line();
        let surface = self.field_surface(multi_line);
        let paint = field_paint(surface, cx);
        let resting = if self.disabled {
            PaintState::Disabled
        } else {
            PaintState::Rest
        };
        self.state.update(cx, |state, _| {
            state.placeholder_color = Some(paint.faint_at(resting));
            state.disabled = self.disabled;
            state.size = self.size;
            state.align = self.align;
            state.masked = self.content_type.is_some_and(InputContentType::masks);
        });

        let (focused, empty, loading, focus_handle) = {
            let state = self.state.read(cx);
            (
                state.focus_handle_ref().is_focused(window) && !self.disabled,
                state.text().is_empty(),
                state.loading,
                state.focus_handle_ref().clone(),
            )
        };

        let role = if multi_line {
            Role::MultilineTextInput
        } else {
            self.content_type
                .map_or(Role::TextInput, InputContentType::role)
        };

        let gap = match self.size {
            Size::Small => px(4.),
            Size::Medium | Size::Size(_) => px(6.),
        };
        let paint_state = if focused {
            PaintState::Focused
        } else {
            resting
        };
        let show_clear = self.cleanable && !self.disabled && !loading && !empty && !multi_line;
        let clear_button = show_clear.then(|| Self::clear_button(&self.state, cx));
        let editable = !self.disabled;
        let disabled = self.disabled;
        let spinner_color = cx.theme().foreground.muted();
        let prefix = self.prefix;
        let suffix = self.suffix;
        let has_suffix = suffix.is_some() || loading || show_clear;

        let mut element = div()
            .id(("zz-input", self.state.entity_id()))
            .role(role)
            .aria_disabled(disabled)
            .key_context(actions::CONTEXT)
            .track_focus(&focus_handle)
            .tab_index(self.tab_index)
            .flex()
            .items_center()
            .gap(gap)
            .w_full()
            .line_height(LINE_HEIGHT)
            .text_color(paint.text_at(paint_state))
            .when(editable, zz_gpui::Styled::cursor_text);

        element = Self::sized(element, self.size);
        if !multi_line {
            element = Self::height(element, self.size);
        }

        element
            .when(self.appearance, |this| {
                field_chrome(
                    this,
                    surface,
                    paint_state,
                    self.bordered,
                    self.focus_bordered,
                    cx,
                )
            })
            .refine_style(&self.style)
            .when(editable, |this| {
                this.on_action(window.listener_for(&self.state, InputState::backspace))
                    .on_action(window.listener_for(&self.state, InputState::delete))
                    .on_action(
                        window.listener_for(&self.state, InputState::delete_to_previous_word_start),
                    )
                    .on_action(
                        window.listener_for(&self.state, InputState::delete_to_next_word_end),
                    )
                    .on_action(
                        window.listener_for(&self.state, InputState::delete_to_beginning_of_line),
                    )
                    .on_action(window.listener_for(&self.state, InputState::delete_to_end_of_line))
                    .on_action(window.listener_for(&self.state, InputState::enter))
                    .on_action(window.listener_for(&self.state, InputState::cut))
                    .on_action(window.listener_for(&self.state, InputState::paste))
                    .on_action(window.listener_for(&self.state, InputState::undo))
                    .on_action(window.listener_for(&self.state, InputState::redo))
                    .on_action(window.listener_for(&self.state, InputState::show_character_palette))
            })
            .on_action(window.listener_for(&self.state, InputState::move_left))
            .on_action(window.listener_for(&self.state, InputState::move_right))
            .on_action(window.listener_for(&self.state, InputState::select_left))
            .on_action(window.listener_for(&self.state, InputState::select_right))
            .on_action(window.listener_for(&self.state, InputState::move_home))
            .on_action(window.listener_for(&self.state, InputState::move_end))
            .on_action(window.listener_for(&self.state, InputState::select_to_start_of_line))
            .on_action(window.listener_for(&self.state, InputState::select_to_end_of_line))
            .on_action(window.listener_for(&self.state, InputState::move_to_start))
            .on_action(window.listener_for(&self.state, InputState::move_to_end))
            .on_action(window.listener_for(&self.state, InputState::select_to_start))
            .on_action(window.listener_for(&self.state, InputState::select_to_end))
            .on_action(window.listener_for(&self.state, InputState::move_to_previous_word))
            .on_action(window.listener_for(&self.state, InputState::move_to_next_word))
            .on_action(window.listener_for(&self.state, InputState::select_to_previous_word_start))
            .on_action(window.listener_for(&self.state, InputState::select_to_next_word_end))
            .on_action(window.listener_for(&self.state, InputState::select_all))
            .on_action(window.listener_for(&self.state, InputState::copy))
            .on_action(window.listener_for(&self.state, InputState::escape))
            .when(multi_line, |this| {
                this.on_action(window.listener_for(&self.state, InputState::move_up))
                    .on_action(window.listener_for(&self.state, InputState::move_down))
                    .on_action(window.listener_for(&self.state, InputState::select_up))
                    .on_action(window.listener_for(&self.state, InputState::select_down))
            })
            .on_key_down(window.listener_for(&self.state, InputState::on_key_down))
            .on_mouse_down(
                MouseButton::Left,
                window.listener_for(&self.state, InputState::on_mouse_down),
            )
            .on_mouse_down(
                MouseButton::Right,
                window.listener_for(&self.state, InputState::on_mouse_down),
            )
            .on_scroll_wheel(window.listener_for(&self.state, InputState::on_scroll_wheel))
            .children(prefix.map(|prefix| {
                div()
                    .flex_none()
                    .when(disabled, |this| this.opacity(0.5))
                    .child(prefix)
            }))
            .child(self.state.clone())
            .when(has_suffix, |this| {
                this.child(
                    h_flex()
                        .flex_none()
                        .gap(gap)
                        .items_center()
                        .when(disabled, |this| this.opacity(0.5))
                        .when(loading, |this| {
                            this.child(Spinner::new().small().color(spinner_color))
                        })
                        .children(clear_button)
                        .children(suffix),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Fill, StylePatch, ThemeColor};

    #[test]
    fn a_field_role_with_no_fill_and_no_outline_draws_no_box() {
        let theme = Theme::from(&*ThemeColor::dark());
        let bare = Surface::new(
            SurfaceRole::new("test.field", Kind::Field, "Bare fields", "")
                .with(StylePatch::EMPTY.fill(Fill::None).outline(false)),
        );
        let rest = field_layer(&theme, bare, PaintState::Rest, true);
        assert_eq!(rest.fill.a, 0.0);
        assert!(rest.glass.is_none() && rest.shadow.is_empty());
        assert_eq!(rest.border.a, 0.0);
        assert!(rest.border_width >= px(1.0));

        let focused = field_layer(&theme, bare, PaintState::Focused, true);
        assert_eq!(focused.border_width, rest.border_width);
        assert!(focused.border.a > 0.0);
    }

    #[test]
    fn focus_draws_no_border_without_outlines_or_on_glass() {
        let mut theme = Theme::from(&*ThemeColor::dark());
        theme.outline = px(0.0);
        let field = Surface::new(Kind::Field.role());
        let focused = field_layer(&theme, field, PaintState::Focused, true);
        assert_eq!(focused.border.a, 0.0);

        let mut theme = Theme::from(&*ThemeColor::dark());
        theme.glass = zz_gpui::GlassMaterial::preset("regular");
        let glass = Surface::new(
            SurfaceRole::new("test.glass-field", Kind::Field, "Glass fields", "")
                .with(StylePatch::EMPTY.fill(Fill::Glass)),
        );
        let focused = field_layer(&theme, glass, PaintState::Focused, true);
        assert!(focused.glass.is_some());
        assert_eq!(focused.border.a, 0.0);
    }
}
