//! The palette: every color the widget layer is allowed to name.

use zz_gpui::Hsla;

use super::{
    Colorize as _,
    color::{edge_weight, tint},
};

/// The six palette roots, the scrim and the shadow ink the widget layer names. Read them off `cx.theme()`; derive
/// everything else with [`Colorize`].
///
/// [`Colorize`]: super::Colorize
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ThemeColor {
    /// The window's base plane. Every other surface is this, raised.
    pub background: Hsla,
    /// Default text, and the source of muted text, focus rings, links and
    /// selection and every edge.
    pub foreground: Hsla,
    /// The one chromatic emphasis: a checked switch, the selected tile ring,
    /// the active pane border and glow, the send button.
    pub accent: Hsla,
    /// Something completed or is healthy.
    pub success: Hsla,
    /// Something needs attention but still works.
    pub warning: Hsla,
    /// Something failed or is destructive.
    pub danger: Hsla,
    /// The dimming behind modals, sheets and slide-overs. Black in both modes,
    /// with a per-mode alpha.
    pub scrim: Hsla,
    /// The ink of every drop shadow. Its alpha is the shadow under a floating
    /// surface at full strength; small shadows keep its hue at their own
    /// alpha. A light theme on a tinted background tints it to match.
    pub shadow_color: Hsla,
    /// Text on an accent fill: a solid primary button in the accent, an
    /// accent selection. `None` picks black or white by the accent's
    /// lightness.
    pub on_accent: Option<Hsla>,
}

impl ThemeColor {
    pub fn border(&self) -> Hsla {
        self.background
            .mix_oklab(self.foreground, 1.0 - edge_weight())
            .opaque()
    }

    /// These colors with the background leaning toward the Oklch `hue`, in
    /// degrees, by `intensity` from 0 to 1. Every surface, edge and glass
    /// tint derives from the background, so they all lean with it.
    #[must_use]
    pub fn tinted(&self, hue: f32, intensity: f32) -> Self {
        Self {
            background: tint(self.background, hue, intensity),
            ..*self
        }
    }
}
