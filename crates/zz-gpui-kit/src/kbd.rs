//! Keyboard-shortcut pill: one [`Keystroke`] as a platform-appropriate glyph
//! string, ⌘⇧A on Apple platforms and Ctrl+Shift+A elsewhere.

use zz_gpui::{
    Action, App, AsKeystroke as _, FocusHandle, FontWeight, IntoElement, KeyContext, Keystroke,
    ParentElement as _, RenderOnce, SharedString, StyleRefinement, Styled, Window, px,
};

use crate::{
    ActiveTheme as _, Ground, Layer, PaintState, Sizable, Size, StyledExt as _, Surface,
    SurfaceRole, Theme, h_flex, rems_from_px,
};

#[cfg(any(target_os = "macos", target_os = "ios"))]
const SEPARATOR: &str = "";
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
const SEPARATOR: &str = "+";

#[cfg(any(target_os = "macos", target_os = "ios"))]
const MODIFIERS: [&str; 4] = ["⌃", "⌥", "⇧", "⌘"];
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
const MODIFIERS: [&str; 4] = ["Ctrl", "Alt", "Shift", "Win"];

/// A keystroke drawn as a [`Keycap`].
#[derive(IntoElement, Clone, Debug)]
pub struct Kbd {
    style: StyleRefinement,
    stroke: Keystroke,
    lowercase: bool,
    surface_role: Option<SurfaceRole>,
    on: Option<Ground>,
    size: Size,
}

impl Kbd {
    #[must_use]
    pub fn new(stroke: Keystroke) -> Self {
        Self {
            style: StyleRefinement::default(),
            stroke,
            lowercase: false,
            surface_role: None,
            on: None,
            size: Size::default(),
        }
    }

    /// Render the key glyph in lower case, leaving modifier glyphs alone.
    #[must_use]
    pub fn lowercase(mut self) -> Self {
        self.lowercase = true;
        self
    }

    /// Draw the cap as `role` draws, instead of as a key cap.
    #[must_use]
    pub fn surface_role(mut self, role: SurfaceRole) -> Self {
        self.surface_role = Some(role);
        self
    }

    /// Draw the cap on `ground`, the paint it sits inside, instead of the
    /// window.
    #[must_use]
    pub fn on(mut self, ground: Ground) -> Self {
        self.on = Some(ground);
        self
    }

    fn cap(&self) -> Keycap {
        Keycap {
            label: self.text().into(),
            surface_role: self.surface_role,
            on: self.on,
            size: self.size,
            style: self.style.clone(),
        }
    }

    /// The keystroke as text, for a binding shown without a cap.
    #[must_use]
    pub fn text(&self) -> String {
        Self::format_key(&self.stroke, self.lowercase)
    }

    /// The highest-precedence binding for `action` as a pill, optionally scoped
    /// to a key context. `None` when the action is unbound.
    #[must_use]
    pub fn binding_for_action(
        action: &dyn Action,
        context: Option<&str>,
        window: &Window,
    ) -> Option<Self> {
        let binding = match context.and_then(|context| KeyContext::parse(context).ok()) {
            Some(context) => {
                window.highest_precedence_binding_for_action_in_context(action, context)
            }
            None => window.highest_precedence_binding_for_action(action),
        }?;
        binding
            .keystrokes()
            .first()
            .map(|key| Self::new(key.as_keystroke().clone()))
    }

    /// The highest-precedence binding for `action` in `focus_handle`'s context,
    /// as a pill. `None` when the action is unbound.
    #[must_use]
    pub fn binding_for_action_in(
        action: &dyn Action,
        focus_handle: &FocusHandle,
        window: &Window,
    ) -> Option<Self> {
        let binding = window.highest_precedence_binding_for_action_in(action, focus_handle)?;
        binding
            .keystrokes()
            .first()
            .map(|key| Self::new(key.as_keystroke().clone()))
    }

    /// Render a keystroke as its platform-specific display string.
    #[must_use]
    pub fn format(key: &Keystroke) -> String {
        Self::format_key(key, false)
    }

    pub fn format_key(key: &Keystroke, lowercase: bool) -> String {
        let m = &key.modifiers;
        let mut parts: Vec<&str> = [
            m.control.then_some(MODIFIERS[0]),
            m.alt.then_some(MODIFIERS[1]),
            m.shift.then_some(MODIFIERS[2]),
            m.platform.then_some(MODIFIERS[3]),
        ]
        .into_iter()
        .flatten()
        .collect();

        let key = key_symbol(key.key.as_str());
        let key = if lowercase { key.to_lowercase() } else { key };
        parts.push(&key);
        parts.join(SEPARATOR)
    }
}

fn key_symbol(key: &str) -> String {
    macro_rules! platform {
        ($mac:literal, $other:literal) => {{
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            {
                $mac.to_string()
            }
            #[cfg(not(any(target_os = "macos", target_os = "ios")))]
            {
                $other.to_string()
            }
        }};
    }

    match key {
        "ctrl" => platform!("⌃", "Ctrl"),
        "alt" => platform!("⌥", "Alt"),
        "shift" => platform!("⇧", "Shift"),
        "cmd" => platform!("⌘", "Win"),
        "backspace" => platform!("⌫", "Backspace"),
        "delete" => platform!("⌫", "Delete"),
        "escape" => platform!("⎋", "Esc"),
        "enter" => platform!("⏎", "Enter"),
        "left" => platform!("←", "Left"),
        "right" => platform!("→", "Right"),
        "up" => platform!("↑", "Up"),
        "down" => platform!("↓", "Down"),
        "space" => "Space".to_string(),
        "pagedown" => "Page Down".to_string(),
        "pageup" => "Page Up".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        }
    }
}

impl Styled for Kbd {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl Sizable for Kbd {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

impl RenderOnce for Kbd {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.cap()
    }
}

/// One key drawn as a cap: a shortcut digit on a row, the Enter mark inside a
/// button. Its label reads muted on the cap's fill, and at full ink on an
/// accent or other colored fill, where muting would wash it into the color.
#[derive(IntoElement)]
pub struct Keycap {
    label: SharedString,
    surface_role: Option<SurfaceRole>,
    on: Option<Ground>,
    size: Size,
    style: StyleRefinement,
}

impl Keycap {
    #[must_use]
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            surface_role: None,
            on: None,
            size: Size::default(),
            style: StyleRefinement::default(),
        }
    }

    /// Draw the cap as `role` draws, instead of as a key cap.
    #[must_use]
    pub fn surface_role(mut self, role: SurfaceRole) -> Self {
        self.surface_role = Some(role);
        self
    }

    fn surface_layer(&self, theme: &Theme) -> Layer {
        theme.layer(
            Surface::new(self.surface_role.unwrap_or(SurfaceRole::KEYCAP)).on_maybe(self.on),
            PaintState::Rest,
        )
    }

    /// Draw the cap on `ground`, the paint it sits inside (see
    /// [`crate::Paint::ground_at`]), instead of the window.
    #[must_use]
    pub fn on(mut self, ground: Ground) -> Self {
        self.on = Some(ground);
        self
    }
}

impl Sizable for Keycap {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

impl Styled for Keycap {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Keycap {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let (height, text) = match self.size {
            Size::Small => (16.0, 10.0),
            Size::Medium | Size::Size(_) => (18.0, 11.0),
        };
        let layer = self.surface_layer(cx.theme());
        h_flex()
            .flex_none()
            .justify_center()
            .h(px(height))
            .min_w(px(height))
            .px(px(4.0))
            .layer(&layer)
            .text_color(layer.muted_text())
            .text_size(rems_from_px(text))
            .line_height(px(height - 2.0))
            .font_weight(FontWeight::NORMAL)
            .refine_style(&self.style)
            .child(self.label)
    }
}

#[cfg(test)]
mod tests {
    use super::{Kbd, Keycap};
    use crate::{Elevation, Theme, ThemeColor};
    use zz_gpui::Keystroke;

    #[test]
    fn kbd_and_keycap_draw_one_cap_on_the_same_ground() {
        let theme = Theme::from(&*ThemeColor::dark());
        let stroke = Keystroke::parse("cmd-k").unwrap();
        assert_eq!(
            Kbd::new(stroke.clone()).cap().surface_layer(&theme),
            Keycap::new("⌘K").surface_layer(&theme)
        );
        let panel = theme.surface(Elevation::Panel);
        let on_panel = Kbd::new(stroke).on(panel).cap().surface_layer(&theme);
        assert_eq!(on_panel, Keycap::new("⌘K").on(panel).surface_layer(&theme));
        assert_ne!(on_panel, Keycap::new("⌘K").surface_layer(&theme));
    }

    #[test]
    fn lowercase_touches_only_the_key_glyph() {
        let key = |s: &str| Keystroke::parse(s).unwrap();
        assert_eq!(Kbd::format(&key("t")), "T");
        assert_eq!(Kbd::format_key(&key("t"), true), "t");
        let modified = Kbd::format_key(&key("ctrl-a"), true);
        assert!(modified.ends_with('a'), "{modified}");
        assert_eq!(
            modified.trim_end_matches('a'),
            Kbd::format(&key("ctrl-a")).trim_end_matches('A')
        );
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn formats_apple_glyphs() {
        let f = |s: &str| Kbd::format(&Keystroke::parse(s).unwrap());
        assert_eq!(f("cmd-a"), "⌘A");
        assert_eq!(f("cmd--"), "⌘-");
        assert_eq!(f("cmd-+"), "⌘+");
        assert_eq!(f("cmd-enter"), "⌘⏎");
        assert_eq!(f("secondary-f12"), "⌘F12");
        assert_eq!(f("shift-pagedown"), "⇧Page Down");
        assert_eq!(f("shift-pageup"), "⇧Page Up");
        assert_eq!(f("shift-space"), "⇧Space");
        assert_eq!(f("cmd-ctrl-a"), "⌃⌘A");
        assert_eq!(f("cmd-alt-backspace"), "⌥⌘⌫");
        assert_eq!(f("shift-delete"), "⇧⌫");
        assert_eq!(f("cmd-ctrl-shift-a"), "⌃⇧⌘A");
        assert_eq!(f("cmd-ctrl-shift-alt-a"), "⌃⌥⇧⌘A");
    }

    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    #[test]
    fn formats_named_keys() {
        let f = |s: &str| Kbd::format(&Keystroke::parse(s).unwrap());
        assert_eq!(f("a"), "A");
        assert_eq!(f("ctrl-a"), "Ctrl+A");
        assert_eq!(f("shift-space"), "Shift+Space");
        assert_eq!(f("ctrl-alt-a"), "Ctrl+Alt+A");
        assert_eq!(f("ctrl-alt-shift-a"), "Ctrl+Alt+Shift+A");
        assert_eq!(f("ctrl-alt-shift-win-a"), "Ctrl+Alt+Shift+Win+A");
        assert_eq!(f("ctrl-shift-backspace"), "Ctrl+Shift+Backspace");
        assert_eq!(f("alt-delete"), "Alt+Delete");
        assert_eq!(f("alt-tab"), "Alt+Tab");
    }
}
