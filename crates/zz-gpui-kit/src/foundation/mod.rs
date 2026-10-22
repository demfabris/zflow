//! The foundation every widget stands on: the theme global and the shared
//! traits.

#![allow(clippy::pedantic, clippy::style, clippy::complexity)]

mod animation;
mod color;
mod element_ext;
mod geometry;
mod index_path;
mod palette;
mod styled;
mod surface;
mod theme;
mod theme_color;
mod units;
mod variant;
mod window_border;

pub use animation::cubic_ease;
pub use animation::{SURFACE_ENTER_DURATION, surface_enter};
pub use color::{Colorize, oklab_lightness, oklch, parse_hex, to_hex};
pub use element_ext::{ElementExt, InteractiveElementExt};
pub use geometry::Side;
pub use index_path::IndexPath;
pub(crate) use styled::ROW_CORNER_SMOOTHING;
pub use styled::{
    Disableable, FOCUS_RING_WIDTH, RULE_WIDTH, Selectable, Sizable, Size, StyledExt, h_flex,
    lifted_shadow, list_row, muted_on, row_corners, row_radius, row_state, row_surface, v_flex,
};
pub(crate) use surface::restyle;
pub use surface::{
    Edge, Fill, Highlight, InteractiveSurfaceExt, Kind, Knobs, Layer, Shadow, ShadowMaterial,
    Shape, StatefulSurfaceExt, Style, StylePatch, Surface, SurfaceRole, drawn_roles,
    note_drawn_roles,
};
pub use theme::{
    ADAPTIVE_CORNER_FRACTION, ActiveTheme, CHROME_GAP, CORNER_SMOOTHING,
    PANE_HEADER_CONTROL_HEIGHT, PANE_HEADER_HEIGHT, ScrollbarShow, Theme, ThemeMode,
};
pub use theme_color::ThemeColor;
pub use units::{BASE_UI_FONT_SIZE, UiZoom, rems_from_px};
pub use variant::{
    Elevation, Emphasis, Ground, InteractivePaintExt, Paint, PaintExt, PaintState, PrimaryStyle,
    Tone,
};
pub use window_border::{window_border, window_paddings};

/// Install the [`Theme`] global. Must run once, before any window opens;
/// `zz_gpui_kit::init` calls it first, ahead of every widget's own `init`.
pub fn init(cx: &mut zz_gpui::App) {
    theme::init(cx);
}
