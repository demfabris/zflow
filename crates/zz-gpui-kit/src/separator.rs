//! Thin divider rule.

use zz_gpui::{
    App, Axis, Hsla, IntoElement, ParentElement as _, RenderOnce, StyleRefinement, Styled, Window,
    div, prelude::FluentBuilder as _,
};

use crate::{Edge, Kind, StyledExt as _, SurfaceRole};

/// A one-pixel divider rule. The rule is an absolutely positioned child, so the
/// separator takes no space along its own axis; only its margins do.
#[derive(IntoElement)]
pub struct Separator {
    style: StyleRefinement,
    color: Option<Hsla>,
    surface_role: Option<SurfaceRole>,
    axis: Axis,
}

impl Separator {
    /// A vertical rule, filling the height of its container.
    #[must_use]
    pub fn vertical() -> Self {
        Self {
            style: StyleRefinement::default(),
            color: None,
            surface_role: None,
            axis: Axis::Vertical,
        }
    }

    /// A horizontal rule, filling the width of its container.
    #[must_use]
    pub fn horizontal() -> Self {
        Self {
            axis: Axis::Horizontal,
            ..Self::vertical()
        }
    }

    /// Override the rule color. Defaults to the theme border.
    #[must_use]
    pub fn color(mut self, color: impl Into<Hsla>) -> Self {
        self.color = Some(color.into());
        self
    }

    /// Draw the rule as `role` draws, instead of as a divider: nothing when
    /// the role has no outline.
    #[must_use]
    pub fn surface_role(mut self, role: SurfaceRole) -> Self {
        self.surface_role = Some(role);
        self
    }
}

impl Styled for Separator {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Separator {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let role = self.surface_role.unwrap_or(Kind::Rule.role());
        let vertical = self.axis == Axis::Vertical;

        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .map(|this| {
                if vertical {
                    this.h_full()
                } else {
                    this.w_full()
                }
            })
            .refine_style(&self.style)
            .child(
                div()
                    .absolute()
                    .map(|this| {
                        if vertical {
                            this.h_full().rule_on(role, Edge::Left, cx)
                        } else {
                            this.w_full().rule_on(role, Edge::Top, cx)
                        }
                    })
                    .when_some(self.color, Styled::border_color),
            )
    }
}
