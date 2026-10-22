//! Tone and emphasis: the two choices every filled, tinted or outlined widget
//! makes, so a badge, a button and a tile that mean the same thing are drawn
//! the same way.

use serde::{Deserialize, Serialize};
use zz_gpui::{Hsla, Pixels, StatefulInteractiveElement, Styled, px};

use super::{
    Colorize as _, Theme,
    color::{faint_weight, muted_weight, readable_on},
};

/// What a widget means, which picks its color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Tone {
    /// Nothing beyond being there: the foreground's grays.
    #[default]
    Neutral,
    /// The main thing to do here. The look picks its color, see
    /// [`PrimaryStyle`].
    Primary,
    /// Something failed or is destructive.
    Danger,
    /// Something needs attention but still works.
    Warning,
    /// Something completed or is healthy.
    Success,
}

impl Tone {
    pub const ALL: [Self; 5] = [
        Self::Neutral,
        Self::Primary,
        Self::Danger,
        Self::Warning,
        Self::Success,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::Primary => "primary",
            Self::Danger => "danger",
            Self::Warning => "warning",
            Self::Success => "success",
        }
    }
}

/// How loudly a widget draws its tone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Emphasis {
    /// A full fill in the tone, with text that reads on it.
    Solid,
    /// A tinted fill, with text in the tone.
    #[default]
    Soft,
    /// A border in the tone and no fill.
    Outline,
    /// Nothing but the text until hovered.
    Ghost,
}

impl Emphasis {
    pub const ALL: [Self; 4] = [Self::Solid, Self::Soft, Self::Outline, Self::Ghost];

    pub fn name(self) -> &'static str {
        match self {
            Self::Solid => "solid",
            Self::Soft => "soft",
            Self::Outline => "outline",
            Self::Ghost => "ghost",
        }
    }
}

/// What the primary tone is drawn in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrimaryStyle {
    /// The foreground: white on a dark theme, black on a light one.
    #[default]
    Foreground,
    /// The accent color.
    Accent,
}

/// The state a painted widget is drawn in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PaintState {
    #[default]
    Rest,
    Hover,
    Active,
    /// The row the cursor is on in a list you pick from: a menu, the palette,
    /// a picker. Drawn in the look's selection color.
    Selected,
    /// Where you are, or what is switched on: the open page, a toggle that is
    /// on, the active session. Always the neutral wash, so chrome that stays
    /// in this state does not carry the selection color.
    Current,
    /// Keyboard focus: the border takes the focus color.
    Focused,
    Disabled,
}

impl PaintState {
    pub const ALL: [Self; 7] = [
        Self::Rest,
        Self::Hover,
        Self::Active,
        Self::Selected,
        Self::Current,
        Self::Focused,
        Self::Disabled,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Rest => "rest",
            Self::Hover => "hover",
            Self::Active => "pressed",
            Self::Selected => "selected",
            Self::Current => "current",
            Self::Focused => "focused",
            Self::Disabled => "disabled",
        }
    }
}

/// The colors a tone and an emphasis resolve to on one ground.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Paint {
    pub fill: Hsla,
    pub hover: Hsla,
    pub active: Hsla,
    pub selected: Hsla,
    pub current: Hsla,
    pub border: Hsla,
    /// Zero when the emphasis draws no border.
    pub border_width: Pixels,
    /// The border of a focused field or control: the primary tone, or this
    /// paint's tone when it has a meaning of its own.
    pub focus: Hsla,
    pub text: Hsla,
    pub selected_text: Hsla,
    /// What this paint sits on.
    pub ground: Ground,
}

/// How far a surface stands off the window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Elevation {
    /// The window itself.
    #[default]
    Window,
    /// A panel set into the window: a bar, a card, an inset box.
    Panel,
    /// A surface floating over the window: a menu, a popover, a sheet. Take
    /// its fill `.opaque()` so what it covers does not read through.
    Popover,
}

/// What a painted widget sits on: a fill, and the ink that reads on it. A
/// widget nested inside another paints on the outer one's
/// [`Paint::ground_at`], so a keycap in a red button or on a selected row
/// takes that fill's ink instead of the window's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ground {
    pub fill: Hsla,
    pub ink: Hsla,
    /// The surface this ground belongs to. Controls cast a shadow only on the
    /// window: a panel or a popover already stands off it.
    pub elevation: Elevation,
    /// Whether it is glass, so what sits on it does not become glass too.
    pub glass: bool,
}

impl Ground {
    /// This ground with its fill made opaque, for a surface that floats over
    /// content.
    pub fn opaque(self) -> Self {
        Self {
            fill: self.fill.opaque(),
            ..self
        }
    }
}

impl Paint {
    pub fn fill_at(&self, state: PaintState) -> Hsla {
        match state {
            PaintState::Rest | PaintState::Focused => self.fill,
            PaintState::Hover => self.hover,
            PaintState::Active => self.active,
            PaintState::Selected => self.selected,
            PaintState::Current => self.current,
            PaintState::Disabled => self.fill.opacity(DISABLED),
        }
    }

    pub fn border_at(&self, state: PaintState) -> Hsla {
        match state {
            PaintState::Focused => self.focus,
            PaintState::Disabled => self.border.opacity(DISABLED),
            _ => self.border,
        }
    }

    pub fn text_at(&self, state: PaintState) -> Hsla {
        match state {
            PaintState::Selected => self.selected_text,
            PaintState::Disabled => self.text.opacity(DISABLED),
            _ => self.text,
        }
    }

    /// Secondary text, such as a description under a label, in `state`:
    /// the text pulled toward the fill it sits on, so it stays legible on a
    /// solid or an accent fill as well as on the window.
    pub fn muted_at(&self, state: PaintState) -> Hsla {
        self.text_at(state)
            .mix_oklab(self.ground_at(state).fill, 1.0 - muted_weight())
    }

    /// Text quieter than [`Self::muted_at`]: a placeholder or a hint.
    pub fn faint_at(&self, state: PaintState) -> Hsla {
        self.text_at(state)
            .mix_oklab(self.ground_at(state).fill, 1.0 - faint_weight())
    }

    /// This paint with its fills composited over its ground, for something
    /// that floats over content (a badge over an editor, a hint over a web
    /// page) and must not let it show through.
    pub fn opaque(self) -> Self {
        let ground = self.ground.fill;
        Self {
            fill: ground.blend(self.fill),
            hover: ground.blend(self.hover),
            active: ground.blend(self.active),
            selected: ground.blend(self.selected),
            current: ground.blend(self.current),
            ..self
        }
    }

    /// The ground this paint makes for what sits inside it in `state`.
    pub fn ground_at(&self, state: PaintState) -> Ground {
        Ground {
            fill: self.ground.fill.blend(self.fill_at(state)),
            ink: self.text_at(state),
            ..self.ground
        }
    }
}

const DISABLED: f32 = 0.5;
const OPAQUE_ENOUGH: f32 = 0.5;
const FOCUS_ALPHA: f32 = 0.8;
const SOFT_ALPHA: [f32; 3] = [0.12, 0.18, 0.24];
const NEUTRAL_OUTLINE_ALPHA: f32 = 0.3;
const TONE_OUTLINE_ALPHA: f32 = 0.55;
/// How much of the outline emphasis's border a soft paint keeps as its
/// hairline.
const SOFT_OUTLINE_SHARE: f32 = 1.0 / 3.0;
const TONE_TEXT_WEIGHT: f32 = 0.7;

impl Theme {
    /// The color `tone` is drawn in.
    pub fn tone(&self, tone: Tone) -> Hsla {
        match tone {
            Tone::Neutral => self.foreground,
            Tone::Primary => match self.primary {
                PrimaryStyle::Foreground => self.foreground,
                PrimaryStyle::Accent => self.accent,
            },
            Tone::Danger => self.danger,
            Tone::Warning => self.warning,
            Tone::Success => self.success,
        }
    }

    /// What a field drawn as `role` is at rest, as the theme's style for it
    /// decides.
    pub fn field_emphasis(&self, role: super::SurfaceRole) -> Emphasis {
        let style = self.style(role);
        match style.fill {
            super::Fill::None if style.outline => Emphasis::Outline,
            super::Fill::None => Emphasis::Ghost,
            _ => Emphasis::Soft,
        }
    }

    /// The window: the ground everything sits on unless it is nested in
    /// another paint.
    pub fn ground(&self) -> Ground {
        Ground {
            fill: self.background,
            ink: self.foreground,
            elevation: Elevation::Window,
            glass: false,
        }
    }

    /// The ground of a surface at `elevation`: the window, or the window's
    /// fill raised one step for a panel and two for a popover.
    pub fn surface(&self, elevation: Elevation) -> Ground {
        let fill = match elevation {
            Elevation::Window => return self.ground(),
            Elevation::Panel => self.background.raised(1),
            Elevation::Popover => self.background.raised(2),
        };
        Ground {
            fill,
            ink: self.foreground,
            elevation,
            glass: false,
        }
    }

    /// `tone` at `emphasis` on the window.
    pub fn paint(&self, tone: Tone, emphasis: Emphasis) -> Paint {
        self.paint_on(self.ground(), tone, emphasis)
    }

    /// `tone` at `emphasis` on `ground`. Neutral takes the ground's ink, and
    /// its washes tint the ground's fill.
    pub fn paint_on(&self, ground: Ground, tone: Tone, emphasis: Emphasis) -> Paint {
        let neutral = tone == Tone::Neutral
            || (tone == Tone::Primary && self.primary == PrimaryStyle::Foreground);
        let ink = ground.ink;
        let color = if neutral { ink } else { self.tone(tone) };
        let window = ground == self.ground();
        let clear = self.transparent;
        let focus = match tone {
            Tone::Danger | Tone::Warning | Tone::Success => color,
            Tone::Neutral | Tone::Primary => match self.primary {
                PrimaryStyle::Foreground => ink,
                PrimaryStyle::Accent => self.accent,
            },
        }
        .opacity(FOCUS_ALPHA);
        let soft = |step: u8| {
            if neutral {
                ground.fill.washed(step + 1)
            } else {
                color.opacity(SOFT_ALPHA[usize::from(step)])
            }
        };
        let selected = if neutral && window {
            self.selection_background()
        } else {
            soft(2)
        };
        let on_accent = self
            .on_accent
            .unwrap_or_else(|| readable_on(self.accent.opaque()));
        let accent_selection = neutral && window;
        let readable = |fill: Hsla, text: Hsla| {
            if accent_selection {
                on_accent
            } else if fill.a > OPAQUE_ENOUGH {
                readable_on(fill)
            } else {
                text
            }
        };
        let text = if neutral {
            ink
        } else {
            color.mix_oklab(ink, TONE_TEXT_WEIGHT)
        };
        let outline = if neutral {
            ink.opacity(NEUTRAL_OUTLINE_ALPHA)
        } else {
            color.opacity(TONE_OUTLINE_ALPHA)
        };
        let solid_text = if tone == Tone::Primary && self.primary == PrimaryStyle::Accent {
            on_accent
        } else {
            readable_on(color)
        };
        match emphasis {
            Emphasis::Solid => Paint {
                fill: color,
                hover: color.hover(),
                active: color.active(),
                selected: color.active(),
                current: color.active(),
                border: clear,
                border_width: px(0.0),
                focus,
                text: solid_text,
                selected_text: solid_text,
                ground,
            },
            Emphasis::Soft => {
                let hairline = self.outlined();
                Paint {
                    fill: soft(0),
                    hover: soft(1),
                    active: soft(2),
                    selected,
                    current: soft(2),
                    border: if hairline {
                        outline.opacity(SOFT_OUTLINE_SHARE)
                    } else {
                        clear
                    },
                    border_width: if hairline { self.outline } else { px(0.0) },
                    focus,
                    text,
                    selected_text: readable(selected, text),
                    ground,
                }
            }
            Emphasis::Outline => Paint {
                fill: clear,
                hover: soft(0),
                active: soft(1),
                selected,
                current: soft(2),
                border: outline,
                border_width: px(1.0),
                focus,
                text,
                selected_text: readable(selected, text),
                ground,
            },
            Emphasis::Ghost => Paint {
                fill: clear,
                hover: soft(1),
                active: soft(2),
                selected,
                current: soft(2),
                border: clear,
                border_width: px(0.0),
                focus,
                text,
                selected_text: readable(selected, text),
                ground,
            },
        }
    }
}

/// Draws a [`Paint`] on any element.
pub trait PaintExt: Styled + Sized {
    /// `paint` as it looks in `state`. Hover and press are not tracked; see
    /// [`InteractivePaintExt::painted_interactive`].
    fn painted(self, paint: Paint, state: PaintState) -> Self {
        self.bg(paint.fill_at(state))
            .border(paint.border_width)
            .border_color(paint.border_at(state))
            .text_color(paint.text_at(state))
    }
}

impl<E: Styled> PaintExt for E {}

/// Draws a [`Paint`] that follows the pointer, on an element with an id.
pub trait InteractivePaintExt: StatefulInteractiveElement + Styled + Sized {
    fn painted_interactive(self, paint: Paint, selected: bool, enabled: bool) -> Self {
        let state = match (enabled, selected) {
            (false, _) => PaintState::Disabled,
            (true, true) => PaintState::Selected,
            (true, false) => PaintState::Rest,
        };
        let this = self.painted(paint, state);
        if !enabled || selected {
            return this;
        }
        this.hover(move |style| style.bg(paint.hover))
            .active(move |style| style.bg(paint.active))
    }
}

impl<E: StatefulInteractiveElement + Styled> InteractivePaintExt for E {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ThemeColor;
    use zz_gpui::hsla;

    fn accent_theme(on_accent: Option<Hsla>) -> Theme {
        let mut theme = Theme::from(&*ThemeColor::dark());
        theme.primary = PrimaryStyle::Accent;
        theme.colors.on_accent = on_accent;
        theme
    }

    #[test]
    fn accent_fills_take_the_themes_accent_text() {
        let ink = hsla(0.5, 0.5, 0.5, 1.0);
        let theme = accent_theme(Some(ink));
        assert_eq!(theme.paint(Tone::Primary, Emphasis::Solid).text, ink);
        assert_eq!(
            theme.paint(Tone::Neutral, Emphasis::Ghost).selected_text,
            ink
        );
        assert_ne!(theme.paint(Tone::Danger, Emphasis::Solid).text, ink);
    }

    #[test]
    fn accent_text_reads_on_the_accent_when_the_theme_names_none() {
        let theme = accent_theme(None);
        let expected = readable_on(theme.accent.opaque());
        assert_eq!(theme.paint(Tone::Primary, Emphasis::Solid).text, expected);
        assert_eq!(
            theme.paint(Tone::Neutral, Emphasis::Ghost).selected_text,
            expected
        );
    }
}
