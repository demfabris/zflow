use zz_gpui::{
    App, Corners, Entity, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    RenderOnce, StyleRefinement, Styled, Window, div, prelude::FluentBuilder as _, px,
};

use crate::{ActiveTheme as _, PaintState, StyledExt as _, SurfaceRole, input::field_chrome};

use super::{
    CodeEditorState,
    state::{CONTEXT, VIM_CONTEXT},
};

/// A full-height editor element bound to a [`CodeEditorState`].
#[derive(IntoElement)]
pub struct CodeEditor {
    state: Entity<CodeEditorState>,
    style: StyleRefinement,
    bordered: bool,
    focus_bordered: bool,
    disabled: bool,
    background_opacity: f32,
    tab_index: isize,
    surface_role: Option<SurfaceRole>,
}

impl CodeEditor {
    pub fn new(state: &Entity<CodeEditorState>) -> Self {
        Self {
            state: state.clone(),
            style: StyleRefinement::default(),
            bordered: false,
            focus_bordered: false,
            disabled: false,
            background_opacity: 1.0,
            tab_index: 0,
            surface_role: None,
        }
    }

    #[must_use]
    pub fn bordered(mut self, bordered: bool) -> Self {
        self.bordered = bordered;
        self
    }

    #[must_use]
    pub fn focus_bordered(mut self, bordered: bool) -> Self {
        self.focus_bordered = bordered;
        self
    }

    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    #[must_use]
    pub fn background_opacity(mut self, opacity: f32) -> Self {
        self.background_opacity = opacity.clamp(0.0, 1.0);
        self
    }

    #[must_use]
    pub fn tab_index(mut self, tab_index: isize) -> Self {
        self.tab_index = tab_index;
        self
    }

    /// Draw the frame as `role` draws, instead of as a field.
    #[must_use]
    pub fn surface_role(mut self, role: SurfaceRole) -> Self {
        self.surface_role = Some(role);
        self
    }
}

impl Styled for CodeEditor {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for CodeEditor {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let framed = self.bordered || self.focus_bordered;
        let role = self.surface_role.unwrap_or(SurfaceRole::TEXT_AREA);
        let frame = cx.theme().layer(role, PaintState::Rest);
        let edge = frame.border_width.max(px(1.0));
        let inner = Corners::all((frame.radius - edge).max(px(0.0)));
        self.state.update(cx, |state, cx| {
            state.disabled = self.disabled;
            if state.background_opacity != self.background_opacity {
                state.background_opacity = self.background_opacity;
                cx.notify();
            }
            if framed {
                state.set_corner_radii(inner, cx);
            }
        });
        let focus_handle = self.state.read(cx).focus_handle_ref().clone();
        let focused = focus_handle.is_focused(window) && !self.disabled;
        let key_context = if self.state.read(cx).vim_mode().is_some() {
            VIM_CONTEXT
        } else {
            CONTEXT
        };
        let paint_state = if self.disabled {
            PaintState::Disabled
        } else if focused {
            PaintState::Focused
        } else {
            PaintState::Rest
        };

        div()
            .id(("code-editor", self.state.entity_id()))
            .refine_style(&self.style)
            .relative()
            .flex()
            .size_full()
            .min_w_0()
            .overflow_hidden()
            .key_context(key_context)
            .track_focus(&focus_handle)
            .tab_index(self.tab_index)
            .when(framed, |this| {
                field_chrome(this, role, paint_state, true, self.focus_bordered, cx)
            })
            .when(!self.disabled, |this| {
                this.on_action(window.listener_for(&self.state, CodeEditorState::backspace))
                    .on_action(window.listener_for(&self.state, CodeEditorState::delete))
                    .on_action(
                        window.listener_for(
                            &self.state,
                            CodeEditorState::delete_to_beginning_of_line,
                        ),
                    )
                    .on_action(
                        window.listener_for(&self.state, CodeEditorState::delete_to_end_of_line),
                    )
                    .on_action(
                        window.listener_for(&self.state, CodeEditorState::delete_previous_word),
                    )
                    .on_action(window.listener_for(&self.state, CodeEditorState::delete_next_word))
                    .on_action(window.listener_for(&self.state, CodeEditorState::enter))
                    .on_action(window.listener_for(&self.state, CodeEditorState::indent_inline))
                    .on_action(window.listener_for(&self.state, CodeEditorState::outdent_inline))
                    .on_action(window.listener_for(&self.state, CodeEditorState::indent_block))
                    .on_action(window.listener_for(&self.state, CodeEditorState::outdent_block))
                    .on_action(window.listener_for(&self.state, CodeEditorState::cut))
                    .on_action(window.listener_for(&self.state, CodeEditorState::paste))
                    .on_action(window.listener_for(&self.state, CodeEditorState::undo))
                    .on_action(window.listener_for(&self.state, CodeEditorState::redo))
            })
            .on_action(window.listener_for(&self.state, CodeEditorState::left))
            .on_action(window.listener_for(&self.state, CodeEditorState::right))
            .on_action(window.listener_for(&self.state, CodeEditorState::up))
            .on_action(window.listener_for(&self.state, CodeEditorState::down))
            .on_action(window.listener_for(&self.state, CodeEditorState::page_up))
            .on_action(window.listener_for(&self.state, CodeEditorState::page_down))
            .on_action(window.listener_for(&self.state, CodeEditorState::home))
            .on_action(window.listener_for(&self.state, CodeEditorState::end))
            .on_action(window.listener_for(&self.state, CodeEditorState::move_to_start))
            .on_action(window.listener_for(&self.state, CodeEditorState::move_to_end))
            .on_action(window.listener_for(&self.state, CodeEditorState::move_to_previous_word))
            .on_action(window.listener_for(&self.state, CodeEditorState::move_to_next_word))
            .on_action(window.listener_for(&self.state, CodeEditorState::select_left))
            .on_action(window.listener_for(&self.state, CodeEditorState::select_right))
            .on_action(window.listener_for(&self.state, CodeEditorState::select_up))
            .on_action(window.listener_for(&self.state, CodeEditorState::select_down))
            .on_action(window.listener_for(&self.state, CodeEditorState::select_to_start))
            .on_action(window.listener_for(&self.state, CodeEditorState::select_to_end))
            .on_action(window.listener_for(&self.state, CodeEditorState::select_to_start_of_line))
            .on_action(window.listener_for(&self.state, CodeEditorState::select_to_end_of_line))
            .on_action(window.listener_for(&self.state, CodeEditorState::select_to_previous_word))
            .on_action(window.listener_for(&self.state, CodeEditorState::select_to_next_word))
            .on_action(window.listener_for(&self.state, CodeEditorState::select_all))
            .on_action(window.listener_for(&self.state, CodeEditorState::copy))
            .on_action(window.listener_for(&self.state, CodeEditorState::escape))
            .on_action(window.listener_for(&self.state, CodeEditorState::show_character_palette))
            .on_action(window.listener_for(&self.state, CodeEditorState::vim_half_page_down))
            .on_action(window.listener_for(&self.state, CodeEditorState::vim_half_page_up))
            .on_action(window.listener_for(&self.state, CodeEditorState::vim_page_down))
            .on_action(window.listener_for(&self.state, CodeEditorState::vim_page_up))
            .on_action(window.listener_for(&self.state, CodeEditorState::vim_redo))
            .on_key_down(window.listener_for(&self.state, CodeEditorState::on_key_down))
            .on_mouse_down(
                MouseButton::Left,
                window.listener_for(&self.state, CodeEditorState::on_mouse_down),
            )
            .on_mouse_down(
                MouseButton::Right,
                window.listener_for(&self.state, CodeEditorState::on_mouse_down),
            )
            .on_scroll_wheel(window.listener_for(&self.state, CodeEditorState::on_scroll_wheel))
            .child(self.state)
    }
}
