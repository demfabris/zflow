//! A short label that says what something is or how it is doing: a count, a
//! status, a kind.

use zz_gpui::{
    AnyElement, App, FontWeight, IntoElement, ParentElement, RenderOnce, SharedString,
    StyleRefinement, Styled, Window, prelude::FluentBuilder as _, px,
};

use crate::{
    ActiveTheme as _, Emphasis, Fill, Icon, Layer, PaintState, Sizable, Size, StyledExt as _,
    Surface, SurfaceRole, Theme, Tone, h_flex, rems_from_px,
};

#[derive(IntoElement)]
pub struct Badge {
    label: SharedString,
    icon: Option<Icon>,
    tone: Tone,
    emphasis: Emphasis,
    surface_role: Option<SurfaceRole>,
    opaque: bool,
    size: Size,
    style: StyleRefinement,
    children: Vec<AnyElement>,
}

impl Badge {
    /// A neutral, soft badge.
    #[must_use]
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            icon: None,
            tone: Tone::default(),
            emphasis: Emphasis::default(),
            surface_role: None,
            opaque: false,
            size: Size::default(),
            style: StyleRefinement::default(),
            children: Vec::new(),
        }
    }

    /// A badge whose content is its children alone.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(SharedString::default())
    }

    /// Fill the badge over the window instead of tinting what is under it,
    /// for a badge that floats over content.
    #[must_use]
    pub fn opaque(mut self) -> Self {
        self.opaque = true;
        self
    }

    #[must_use]
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }

    /// Solid fills the badge in its tone; outline and ghost leave it
    /// unfilled, ghost without its outline too.
    #[must_use]
    pub fn emphasis(mut self, emphasis: Emphasis) -> Self {
        self.emphasis = emphasis;
        self
    }

    /// Draw the badge as `role` draws, instead of as a badge.
    #[must_use]
    pub fn surface_role(mut self, role: SurfaceRole) -> Self {
        self.surface_role = Some(role);
        self
    }

    fn surface_layer(&self, theme: &Theme) -> Layer {
        let role = self.surface_role.unwrap_or(SurfaceRole::BADGE);
        let role = match self.emphasis {
            Emphasis::Solid | Emphasis::Soft => role,
            Emphasis::Outline => role.with(role.patch.fill(Fill::None)),
            Emphasis::Ghost => role.with(role.patch.fill(Fill::None).outline(false)),
        };
        let surface = Surface::new(role)
            .tone(self.tone)
            .prominent(self.emphasis == Emphasis::Solid);
        let mut layer = theme.layer(surface, PaintState::Rest);
        if self.opaque {
            layer.fill = theme.ground().fill.blend(layer.fill);
        }
        layer
    }

    /// An icon before the label, in the label's color.
    #[must_use]
    pub fn icon(mut self, icon: impl Into<Icon>) -> Self {
        self.icon = Some(icon.into());
        self
    }
}

impl Sizable for Badge {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

impl ParentElement for Badge {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl Styled for Badge {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Badge {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let (height, text, padding) = match self.size {
            Size::Small => (16.0, 10.0, 5.0),
            Size::Medium | Size::Size(_) => (18.0, 11.0, 6.0),
        };
        let layer = self.surface_layer(cx.theme());
        h_flex()
            .flex_none()
            .h(px(height))
            .gap(px(4.0))
            .px(px(padding))
            .layer(&layer)
            .text_size(rems_from_px(text))
            .line_height(px(height - 2.0))
            .font_weight(FontWeight::MEDIUM)
            .whitespace_nowrap()
            .refine_style(&self.style)
            .children(self.icon.map(|icon| icon.size(px(text + 1.0))))
            .when(!self.label.is_empty(), |badge| badge.child(self.label))
            .children(self.children)
    }
}
