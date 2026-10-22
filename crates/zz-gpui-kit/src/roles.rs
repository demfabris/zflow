//! The roles of an app's window chrome: the frame, title bar, sidebar,
//! settings cards and the rules between them. They sit after the kit's own
//! kinds and groups, and an app adds its own with [`register`], so a theme
//! file can name any of them.

use std::sync::OnceLock;

use crate::{Fill, Highlight, Kind, Knobs, Shadow, Shape, StylePatch, SurfaceRole};

const P: StylePatch = StylePatch::EMPTY;

pub const WINDOW: SurfaceRole = SurfaceRole::new(
    "window",
    Kind::Panel,
    "Window",
    "The edge and drop shadow of the window frame the app draws on Linux",
)
.with(P.outline(true).shadow(Shadow::Float))
.with_knobs(Knobs::FIXED);
pub const TITLEBAR: SurfaceRole = SurfaceRole::new(
    "titlebar",
    Kind::Bar,
    "Title bar",
    "The strip across the top of the window",
)
.with_knobs(Knobs::FIXED);
pub const CHROME_BUTTON: SurfaceRole = SurfaceRole::under(
    &SurfaceRole::ICON_BUTTON,
    "chrome.button",
    "Chrome buttons",
    "Icon buttons on the window chrome",
);
pub const WINDOW_CONTROLS: SurfaceRole = SurfaceRole::under(
    &SurfaceRole::ICON_BUTTON,
    "titlebar.window-controls",
    "Window controls",
    "Minimize, maximize and close on Windows and Linux",
)
.with(
    P.shape(Shape::Square)
        .hover(Highlight::Tint)
        .selected(Highlight::None),
);
pub const SIDEBAR: SurfaceRole = SurfaceRole::new(
    "sidebar",
    Kind::Panel,
    "Sidebar",
    "The navigation column at the window's leading edge",
)
.with_knobs(Knobs::FIXED);
pub const SIDEBAR_ROW: SurfaceRole = SurfaceRole::under(
    &SurfaceRole::ROW,
    "sidebar.row",
    "Sidebar rows",
    "The pages and items in the sidebar",
);
pub const SETTINGS_CARD: SurfaceRole = SurfaceRole::new(
    "settings.card",
    Kind::Card,
    "Setting groups",
    "The cards that group settings",
);
pub const SETTINGS_PREVIEW: SurfaceRole = SurfaceRole::new(
    "settings.preview",
    Kind::Card,
    "Previews",
    "Wells that show what a setting draws",
)
.with(P.shadow(Shadow::None));
pub const SETTINGS_TILE: SurfaceRole = SurfaceRole::under(
    &SurfaceRole::TILE,
    "settings.tile",
    "Picker tiles",
    "Pickable tiles in Settings",
)
.with(P.fill(Fill::None).outline(false).shadow(Shadow::None));
pub const BAR_RULE: SurfaceRole = SurfaceRole::new(
    "rule.bar",
    Kind::Rule,
    "Bar dividers",
    "Lines under the title bar and over footers",
);
pub const SIDEBAR_RULE: SurfaceRole = SurfaceRole::new(
    "rule.sidebar",
    Kind::Rule,
    "Sidebar edge",
    "The line between the sidebar and the page",
);
pub const LIST_RULE: SurfaceRole = SurfaceRole::new(
    "rule.list",
    Kind::Rule,
    "List dividers",
    "Lines between menu groups and setting rows",
);

/// The chrome roles, in the order a theme creator lists them.
pub const CHROME: [SurfaceRole; 12] = [
    WINDOW,
    SIDEBAR,
    TITLEBAR,
    SETTINGS_CARD,
    SETTINGS_PREVIEW,
    CHROME_BUTTON,
    WINDOW_CONTROLS,
    SIDEBAR_ROW,
    SETTINGS_TILE,
    BAR_RULE,
    SIDEBAR_RULE,
    LIST_RULE,
];

static APP: OnceLock<&'static [SurfaceRole]> = OnceLock::new();

/// Adds the app's own roles after the chrome's. Call it once, before a theme
/// file is read; a second call is ignored.
pub fn register(roles: &'static [SurfaceRole]) {
    let _ = APP.set(roles);
}

/// Every role a theme file may name: the kit's kinds and groups, the
/// chrome's, then the app's.
pub fn all() -> impl Iterator<Item = SurfaceRole> {
    SurfaceRole::KIT
        .into_iter()
        .chain(CHROME)
        .chain(APP.get().copied().unwrap_or_default().iter().copied())
}

/// The role a theme file calls `id`.
pub fn find(id: &str) -> Option<SurfaceRole> {
    all().find(|role| role.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_spelled_like_a_theme_file() {
        let ids = all().map(|role| role.id).collect::<Vec<_>>();
        for (index, id) in ids.iter().enumerate() {
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-' || c == '.'),
                "{id}"
            );
            assert!(!ids[..index].contains(id), "{id} is named twice");
        }
    }

    #[test]
    fn every_chrome_role_hangs_from_its_kind_through_known_groups() {
        for role in CHROME {
            assert!(!role.is_kind(), "{} is a kind", role.id);
            for group in role.lineage().skip(1) {
                assert_eq!(find(group.id), Some(group), "{}", role.id);
            }
            assert_eq!(role.lineage().last(), Some(role.kind.role()));
        }
    }
}
