//! A selectable row for list-shaped surfaces.

use zz_gpui::{
    AnyElement, App, ClickEvent, Div, ElementId, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, ParentElement, RenderOnce, Stateful,
    StatefulInteractiveElement as _, StyleRefinement, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};

use crate::Colorize as _;
use crate::{
    ActiveTheme as _, Disableable, PaintState, Selectable, Size, StyledExt as _, SurfaceRole,
    foundation::{restyle, row_corners},
    h_flex,
};

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
type MouseDownHandler = Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App) + 'static>;
type MouseMoveHandler = Box<dyn Fn(&MouseMoveEvent, &mut Window, &mut App) + 'static>;

/// One row of a list: a stateful horizontal flex box that hovers, selects and
/// clicks. [`Styled`], so a call site overrides height, padding, radius, cursor
/// and background as it would on a `div()`.
#[derive(IntoElement)]
pub struct ListItem {
    base: Stateful<Div>,
    style: StyleRefinement,
    disabled: bool,
    selected: bool,
    surface_role: Option<SurfaceRole>,
    on_click: Option<ClickHandler>,
    on_mouse_down: Vec<(MouseButton, MouseDownHandler)>,
    on_mouse_enter: Option<MouseMoveHandler>,
    children: Vec<AnyElement>,
}

impl ListItem {
    /// A row identified by `id`, which must be unique among its siblings.
    #[must_use]
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            base: h_flex().id(id),
            style: StyleRefinement::default(),
            disabled: false,
            selected: false,
            surface_role: None,
            on_click: None,
            on_mouse_down: Vec::new(),
            on_mouse_enter: None,
            children: Vec::new(),
        }
    }

    /// Render the row as selected. Inherent mirror of [`Selectable::selected`],
    /// so a call site does not have to import the trait.
    #[must_use]
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Draw the row as `role` draws, instead of as a list row.
    #[must_use]
    pub fn surface_role(mut self, role: SurfaceRole) -> Self {
        self.surface_role = Some(role);
        self
    }

    /// Handle a click on the row. Ignored while the row is disabled.
    #[must_use]
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    /// Handle a press of `button`, carrying the click count that
    /// [`Self::on_click`] drops. Ignored while disabled; handlers accumulate.
    #[must_use]
    pub fn on_mouse_down(
        mut self,
        button: MouseButton,
        handler: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_mouse_down.push((button, Box::new(handler)));
        self
    }

    /// Handle the pointer moving over the row, for lists that follow the mouse
    /// with their selection. Ignored while the row is disabled.
    #[must_use]
    pub fn on_mouse_enter(
        mut self,
        handler: impl Fn(&MouseMoveEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_mouse_enter = Some(Box::new(handler));
        self
    }
}

impl Disableable for ListItem {
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl Selectable for ListItem {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl Styled for ListItem {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl ParentElement for ListItem {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for ListItem {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            base,
            style,
            disabled,
            selected,
            surface_role,
            on_click,
            on_mouse_down,
            on_mouse_enter,
            children,
        } = self;

        let interactive = !disabled;
        let highlighted = interactive && selected;

        let theme = cx.theme();
        let role = surface_role.unwrap_or(SurfaceRole::MENU_ITEM);
        let layer = theme.layer(
            role,
            if highlighted {
                PaintState::Selected
            } else {
                PaintState::Rest
            },
        );
        let muted_foreground = theme.foreground.muted();

        base.relative()
            .justify_between()
            .gap_x_1()
            .py_1()
            .px_3()
            .text_base()
            .layer(&layer)
            .map(|this| row_corners(this, &layer, px(Size::Medium.control_px())))
            .refine_style(&style)
            .when(interactive, |this| {
                this.when_some(on_click, |this, on_click| this.on_click(on_click))
                    .when_some(on_mouse_enter, |this, on_mouse_enter| {
                        this.on_mouse_move(on_mouse_enter)
                    })
                    .map(|this| {
                        on_mouse_down
                            .into_iter()
                            .fold(this, |this, (button, handler)| {
                                this.on_mouse_down(button, handler)
                            })
                    })
                    .when(!selected, |this| {
                        let hover = theme.layer(role, PaintState::Hover);
                        this.hover(move |style| restyle(style, &hover))
                    })
            })
            .when(!interactive, |this| this.text_color(muted_foreground))
            .child(div().w_full().children(children))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Highlight, StylePatch};
    use zz_gpui::{Context, IntoElement, Render, TestAppContext, solid_background};

    const TINTED: SurfaceRole =
        SurfaceRole::under(&SurfaceRole::MENU_ITEM, "test.row", "Tinted rows", "")
            .with(StylePatch::EMPTY.selected(Highlight::Tint));

    struct Rows;

    impl Render for Rows {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(ListItem::new("plain").selected(true).child("Plain"))
                .child(
                    ListItem::new("tinted")
                        .surface_role(TINTED)
                        .selected(true)
                        .child("Tinted"),
                )
        }
    }

    #[zz_gpui::test]
    fn a_row_roles_selected_highlight_reaches_the_selected_row(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (_, cx) = cx.add_window_view(|_, _| Rows);
        cx.update(|window, cx| {
            _ = window.draw(cx);
            let theme = cx.theme();
            let tinted = theme.layer(TINTED, PaintState::Selected).fill;
            let accent = theme
                .layer(SurfaceRole::MENU_ITEM, PaintState::Selected)
                .fill;
            assert_eq!(accent, theme.selection_background());
            assert_ne!(tinted, accent);
            let quads = window.painted_quads();
            for fill in [accent, tinted] {
                assert!(
                    quads
                        .iter()
                        .any(|quad| quad.background == solid_background(fill)),
                    "no row painted {fill:?}"
                );
            }
        });
    }
}
