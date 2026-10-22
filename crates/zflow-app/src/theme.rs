//! zz's Default interface theme over the kit, in the system's light or dark
//! mode, plus the roles only zflow draws.

use zz_gpui::{App, Window};
use zz_gpui_kit::{
    ADAPTIVE_CORNER_FRACTION, CORNER_SMOOTHING, Highlight, Kind, Shadow, StylePatch, SurfaceRole,
    Theme, interface_theme,
};

const P: StylePatch = StylePatch::EMPTY;

/// The arrangement's well, behind the islands.
pub const BOARD: SurfaceRole = SurfaceRole::new(
    "board",
    Kind::Card,
    "Arrangement",
    "The well the computers' screens are arranged on",
)
.with(P.shadow(Shadow::None));
/// One screen on the arrangement.
pub const SCREEN: SurfaceRole = SurfaceRole::new(
    "board.screen",
    Kind::Card,
    "Screens",
    "A screen on the arrangement",
)
.with(P.shadow(Shadow::Contact));
/// The name above a computer's screens.
pub const CHIP: SurfaceRole = SurfaceRole::under(
    &SurfaceRole::BADGE,
    "board.chip",
    "Computer names",
    "The name and state above each computer on the arrangement",
)
.with(P.selected(Highlight::Tint));
/// A computer found on the network, waiting on the shelf.
pub const SHELF_TILE: SurfaceRole = SurfaceRole::new(
    "board.found",
    Kind::Card,
    "Found computers",
    "Computers found on the network, under the arrangement",
);
/// A row that needs attention, with its fix.
pub const BANNER: SurfaceRole = SurfaceRole::new(
    "banner",
    Kind::Card,
    "Banners",
    "Problems and notes above the page",
)
.with(P.shadow(Shadow::None));

const ROLES: &[SurfaceRole] = &[BOARD, SCREEN, CHIP, SHELF_TILE, BANNER];

pub fn init(cx: &mut App) {
    zz_gpui_kit::roles::register(ROLES);
    zz_gpui_kit::init(cx);
    apply(None, cx);
}

/// Follows the system's mode, then draws it in the Default theme.
pub fn apply(mut window: Option<&mut Window>, cx: &mut App) {
    if let Some(window) = window.as_deref_mut() {
        window.set_default_corner_smoothing(CORNER_SMOOTHING);
        window.set_adaptive_corner_fraction(Some(ADAPTIVE_CORNER_FRACTION));
    }
    Theme::sync_system_appearance(window, cx);
    interface_theme::default_theme().apply(Theme::global_mut(cx));
    cx.refresh_windows();
}
