//! The mark that says whether an option is picked: a checkbox when several
//! can be, a radio when one can. A row or tile around it handles the click.

use zz_gpui::{
    App, IntoElement, ParentElement as _, RenderOnce, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};

use crate::{
    ActiveTheme as _, Disableable, Emphasis, Ground, Icon, IconName, PaintExt as _, PaintState,
    Sizable, Size, Tone, h_flex,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Checkbox,
    Radio,
}

#[derive(IntoElement)]
pub struct Check {
    kind: Kind,
    checked: bool,
    tone: Tone,
    on: Option<Ground>,
    disabled: bool,
    size: Size,
}

impl Check {
    #[must_use]
    pub fn checkbox(checked: bool) -> Self {
        Self::new(Kind::Checkbox, checked)
    }

    #[must_use]
    pub fn radio(checked: bool) -> Self {
        Self::new(Kind::Radio, checked)
    }

    fn new(kind: Kind, checked: bool) -> Self {
        Self {
            kind,
            checked,
            tone: Tone::Primary,
            on: None,
            disabled: false,
            size: Size::default(),
        }
    }

    /// The tone of the checked mark. Primary by default.
    #[must_use]
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }

    /// Draw the mark on `ground`, the paint it sits inside, instead of the
    /// window.
    #[must_use]
    pub fn on(mut self, ground: Ground) -> Self {
        self.on = Some(ground);
        self
    }
}

impl Disableable for Check {
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl Sizable for Check {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

impl RenderOnce for Check {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let side = match self.size {
            Size::Small => 14.0,
            Size::Medium => 16.0,
            Size::Size(size) => f32::from(size),
        };
        let theme = cx.theme();
        let ground = self.on.unwrap_or_else(|| theme.ground());
        let paint = if self.checked {
            theme.paint_on(ground, self.tone, Emphasis::Solid)
        } else {
            theme.paint_on(ground, Tone::Neutral, Emphasis::Outline)
        };
        let state = if self.disabled {
            PaintState::Disabled
        } else {
            PaintState::Rest
        };
        let ink = paint.text_at(state);
        h_flex()
            .flex_none()
            .justify_center()
            .size(px(side))
            .map(|mark| match self.kind {
                Kind::Checkbox => mark.rounded(theme.radius / 2.0),
                Kind::Radio => mark.rounded_full(),
            })
            .painted(paint, state)
            .when(self.checked, |mark| match self.kind {
                Kind::Checkbox => mark.child(
                    Icon::new(IconName::Check)
                        .size(px((side * 0.7).round()))
                        .text_color(ink),
                ),
                Kind::Radio => mark.child(
                    div()
                        .size(px((side * 0.375).round()))
                        .rounded_full()
                        .bg(ink),
                ),
            })
    }
}
