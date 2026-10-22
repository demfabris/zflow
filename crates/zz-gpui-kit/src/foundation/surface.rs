//! Surfaces and their roles.
//!
//! Every painted piece of UI names a [`SurfaceRole`]: a [`Kind`] of widget and, for
//! the pieces a theme wants to tell apart, the place it sits in. A role's
//! [`Style`] says which treatments it takes (a fill, an outline, a shadow, a
//! shape, and what pointing at it or selecting it adds), and the theme's
//! materials say what each treatment looks like: its radius, outline width,
//! shadows and glass. A theme changes a kind or a role with a [`StylePatch`];
//! a role takes the kind's built-in style, then the theme's patch for the
//! kind, then its own built-in patch, then the theme's patch for the role.

use std::cell::RefCell;
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use zz_gpui::{
    App, BoxShadow, FULL_CORNER_RADIUS, GlassMaterial, Hsla, InteractiveElement, Pixels,
    StatefulInteractiveElement, Styled, point, px,
};

use super::{
    ActiveTheme as _, Colorize as _, Elevation, Emphasis, Ground, PaintState, Theme, Tone,
};

/// What a surface is made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fill {
    /// Nothing: what is behind shows through.
    None,
    /// A translucent wash over what is behind.
    Tint,
    /// An opaque fill a step off what it sits on.
    Solid,
    /// The theme's glass, tinted with the solid fill. On glass it paints a
    /// tint instead, and where the theme has no glass it paints solid.
    Glass,
}

/// Which of the theme's shadows a surface casts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shadow {
    None,
    /// The tight shadow of something resting on the window.
    Contact,
    /// The contact shadow under a deep, soft one, for what floats.
    Float,
}

/// The corners of a surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shape {
    Square,
    /// The theme's radius. Rows take it less the list's inset, so they sit
    /// concentric inside the menu or panel that holds them, and go square
    /// when the theme insets nothing.
    Round,
    /// Ends rounded into half circles.
    Pill,
}

/// What a surface adds while it is pointed at or pressed, or while it is
/// the selected or current one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Highlight {
    None,
    /// A wash a step deeper than the fill.
    Tint,
    /// The wash with the outline and a contact shadow, as if the surface
    /// rose off what it sits on.
    Lift,
    /// The accent, with the text that reads on it.
    Accent,
    /// A ring in the accent just outside the edge.
    Ring,
    /// A bar in the accent along the leading edge.
    Bar,
    Glass,
}

impl Highlight {
    pub const ALL: &'static [Self] = &[
        Self::None,
        Self::Tint,
        Self::Lift,
        Self::Accent,
        Self::Ring,
        Self::Bar,
        Self::Glass,
    ];
}

/// The treatments a surface takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Style {
    pub fill: Fill,
    /// Whether it draws the theme's outline.
    pub outline: bool,
    pub shadow: Shadow,
    pub shape: Shape,
    pub hover: Highlight,
    pub selected: Highlight,
}

/// Changes to a [`Style`]: what a theme file writes for a kind or a role.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StylePatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill: Option<Fill>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outline: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow: Option<Shadow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shape: Option<Shape>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hover: Option<Highlight>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<Highlight>,
}

impl StylePatch {
    pub const EMPTY: Self = Self {
        fill: None,
        outline: None,
        shadow: None,
        shape: None,
        hover: None,
        selected: None,
    };

    pub const fn fill(self, fill: Fill) -> Self {
        Self {
            fill: Some(fill),
            ..self
        }
    }

    pub const fn outline(self, outline: bool) -> Self {
        Self {
            outline: Some(outline),
            ..self
        }
    }

    pub const fn shadow(self, shadow: Shadow) -> Self {
        Self {
            shadow: Some(shadow),
            ..self
        }
    }

    pub const fn shape(self, shape: Shape) -> Self {
        Self {
            shape: Some(shape),
            ..self
        }
    }

    pub const fn hover(self, hover: Highlight) -> Self {
        Self {
            hover: Some(hover),
            ..self
        }
    }

    pub const fn selected(self, selected: Highlight) -> Self {
        Self {
            selected: Some(selected),
            ..self
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::EMPTY
    }

    pub fn apply(&self, style: &mut Style) {
        if let Some(fill) = self.fill {
            style.fill = fill;
        }
        if let Some(outline) = self.outline {
            style.outline = outline;
        }
        if let Some(shadow) = self.shadow {
            style.shadow = shadow;
        }
        if let Some(shape) = self.shape {
            style.shape = shape;
        }
        if let Some(hover) = self.hover {
            style.hover = hover;
        }
        if let Some(selected) = self.selected {
            style.selected = selected;
        }
    }

    /// The patch that turns `from` into `to`, naming only what differs.
    pub fn between(from: Style, to: Style) -> Self {
        let differs = |same: bool| !same;
        Self {
            fill: differs(from.fill == to.fill).then_some(to.fill),
            outline: differs(from.outline == to.outline).then_some(to.outline),
            shadow: differs(from.shadow == to.shadow).then_some(to.shadow),
            shape: differs(from.shape == to.shape).then_some(to.shape),
            hover: differs(from.hover == to.hover).then_some(to.hover),
            selected: differs(from.selected == to.selected).then_some(to.selected),
        }
    }
}

/// The kinds of widget every role is one of. Each carries a built-in style;
/// the Default theme changes none of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Bar,
    Panel,
    Pane,
    Card,
    Floating,
    Button,
    Item,
    Field,
    Label,
    Rule,
}

impl Kind {
    pub const ALL: [Self; 10] = [
        Self::Bar,
        Self::Panel,
        Self::Pane,
        Self::Card,
        Self::Floating,
        Self::Button,
        Self::Item,
        Self::Field,
        Self::Label,
        Self::Rule,
    ];

    /// What a theme file calls it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bar => "bar",
            Self::Panel => "panel",
            Self::Pane => "pane",
            Self::Card => "card",
            Self::Floating => "floating",
            Self::Button => "button",
            Self::Item => "item",
            Self::Field => "field",
            Self::Label => "label",
            Self::Rule => "rule",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Bar => "Bars",
            Self::Panel => "Panels",
            Self::Pane => "Panes",
            Self::Card => "Cards",
            Self::Floating => "Floating",
            Self::Button => "Buttons",
            Self::Item => "Items",
            Self::Field => "Fields",
            Self::Label => "Labels",
            Self::Rule => "Dividers",
        }
    }

    const fn about(self) -> &'static str {
        match self {
            Self::Bar => {
                "Strips across a window, pane or sheet: the title bar, pane headers, the phone bar"
            }
            Self::Panel => "Columns set into the window, like the sidebar",
            Self::Pane => "Terminal, browser, agent and editor panes",
            Self::Card => {
                "Boxes in the flow: message bubbles, setting groups, code blocks, previews"
            }
            Self::Floating => {
                "Everything over the content: menus, palettes, dialogs, sheets, toasts, tooltips, floating panes and pane chips"
            }
            Self::Button => {
                "Everything you press to do something: buttons, icon buttons, and the rows of menus, palettes and pickers"
            }
            Self::Item => {
                "What you pick that stays picked or open: sidebar rows, tabs, agent steps, picker tiles"
            }
            Self::Field => {
                "Text fields, selects and anything you type into. Selected is the one you are typing in"
            }
            Self::Label => "Badges, key hints and other small marks",
            Self::Rule => "Divider lines",
        }
    }

    /// The kind as a role, for a widget nothing more specific names.
    pub const fn role(self) -> SurfaceRole {
        SurfaceRole::new(self.name(), self, self.label(), self.about())
    }

    /// The built-in style, which draws the Default theme.
    pub const fn base(self) -> Style {
        const fn style(
            fill: Fill,
            outline: bool,
            shadow: Shadow,
            hover: Highlight,
            selected: Highlight,
        ) -> Style {
            Style {
                fill,
                outline,
                shadow,
                shape: Shape::Round,
                hover,
                selected,
            }
        }
        use Highlight as H;
        match self {
            Self::Bar | Self::Panel => Style {
                shape: Shape::Square,
                ..style(Fill::None, false, Shadow::None, H::None, H::None)
            },
            Self::Rule => Style {
                shape: Shape::Square,
                ..style(Fill::None, true, Shadow::None, H::None, H::None)
            },
            Self::Pane => style(Fill::Glass, true, Shadow::Contact, H::None, H::Ring),
            Self::Card => style(Fill::Solid, true, Shadow::Contact, H::None, H::Ring),
            Self::Floating => style(Fill::Glass, true, Shadow::Float, H::None, H::Accent),
            Self::Button => style(Fill::Solid, true, Shadow::Contact, H::Tint, H::Tint),
            Self::Item => style(Fill::None, false, Shadow::None, H::Lift, H::Lift),
            Self::Field => style(Fill::Tint, true, Shadow::None, H::Tint, H::None),
            Self::Label => style(Fill::Tint, true, Shadow::None, H::None, H::None),
        }
    }

    /// A control: it casts its shadow only on the window, since a panel or a
    /// popover already stands off it.
    pub const fn is_control(self) -> bool {
        matches!(self, Self::Button | Self::Item | Self::Field | Self::Label)
    }

    /// Floats over the window, so its fill steps off the window's color
    /// whatever it is placed on.
    pub const fn floats(self) -> bool {
        matches!(self, Self::Floating)
    }

    /// How far its fill stands off the window.
    pub const fn elevation(self) -> Elevation {
        if self.floats() {
            Elevation::Popover
        } else if self.is_control() || matches!(self, Self::Bar | Self::Rule) {
            Elevation::Window
        } else {
            Elevation::Panel
        }
    }

    pub const fn knobs(self) -> Knobs {
        use Fill as F;
        use Shadow as S;
        use Shape as Sh;
        const FILLS: &[Fill] = &[F::None, F::Tint, F::Solid, F::Glass];
        const BACKED: &[Fill] = &[F::Tint, F::Solid, F::Glass];
        const OUTLINE: &[bool] = &[true, false];
        const SHADOWS: &[Shadow] = &[S::None, S::Contact, S::Float];
        const RESTING: &[Shadow] = &[S::None, S::Contact];
        const SHAPES: &[Shape] = &[Sh::Square, Sh::Round, Sh::Pill];
        const ALL: &[Highlight] = Highlight::ALL;
        const LIT: &[Highlight] = &[Highlight::None, Highlight::Ring];
        const FOCUS: &[Highlight] = &[
            Highlight::None,
            Highlight::Tint,
            Highlight::Lift,
            Highlight::Ring,
            Highlight::Bar,
            Highlight::Glass,
        ];
        const FIXED: Knobs = Knobs::FIXED;
        const SURFACE: Knobs = Knobs {
            fill: FILLS,
            outline: OUTLINE,
            shadow: SHADOWS,
            ..FIXED
        };
        const CONTROL: Knobs = Knobs {
            shape: SHAPES,
            hover: ALL,
            selected: ALL,
            ..SURFACE
        };
        match self {
            Self::Bar => Knobs {
                shadow: RESTING,
                ..SURFACE
            },
            Self::Panel => FIXED,
            Self::Pane => Knobs {
                selected: LIT,
                ..SURFACE
            },
            Self::Card => Knobs {
                shape: Knobs::BOXES,
                ..SURFACE
            },
            Self::Floating => Knobs {
                fill: BACKED,
                shape: SHAPES,
                ..SURFACE
            },
            Self::Button => CONTROL,
            Self::Item => Knobs {
                shadow: RESTING,
                ..CONTROL
            },
            Self::Field => Knobs {
                selected: FOCUS,
                ..CONTROL
            },
            Self::Label => Knobs {
                shadow: RESTING,
                shape: SHAPES,
                ..SURFACE
            },
            Self::Rule => Knobs {
                outline: OUTLINE,
                ..FIXED
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Knobs {
    pub fill: &'static [Fill],
    pub outline: &'static [bool],
    pub shadow: &'static [Shadow],
    pub shape: &'static [Shape],
    pub hover: &'static [Highlight],
    pub selected: &'static [Highlight],
}

impl Knobs {
    pub const FIXED: Self = Self {
        fill: &[],
        outline: &[],
        shadow: &[],
        shape: &[],
        hover: &[],
        selected: &[],
    };

    /// The shapes of a box too big to be a pill.
    pub const BOXES: &'static [Shape] = &[Shape::Square, Shape::Round];

    /// These knobs, with every hover and selected highlight open.
    pub const fn pressable(self) -> Self {
        Self {
            hover: Highlight::ALL,
            selected: Highlight::ALL,
            ..self
        }
    }

    fn open(&self) -> [bool; 6] {
        [
            !self.fill.is_empty(),
            !self.outline.is_empty(),
            !self.shadow.is_empty(),
            !self.shape.is_empty(),
            !self.hover.is_empty(),
            !self.selected.is_empty(),
        ]
    }

    pub fn admit(&self, patch: StylePatch) -> StylePatch {
        fn keep<T: PartialEq>(value: Option<T>, allowed: &[T]) -> Option<T> {
            value.filter(|value| allowed.contains(value))
        }
        StylePatch {
            fill: keep(patch.fill, self.fill),
            outline: keep(patch.outline, self.outline),
            shadow: keep(patch.shadow, self.shadow),
            shape: keep(patch.shape, self.shape),
            hover: keep(patch.hover, self.hover),
            selected: keep(patch.selected, self.selected),
        }
    }

    pub fn refusal(&self, patch: &StylePatch) -> Option<String> {
        fn check<T: PartialEq + Serialize>(
            prop: &str,
            value: Option<T>,
            allowed: &[T],
        ) -> Option<String> {
            let value = value.filter(|value| !allowed.contains(value))?;
            let spell = |value: &T| serde_json::to_string(value).unwrap_or_default();
            Some(if allowed.is_empty() {
                format!("`{prop}` is fixed")
            } else {
                format!(
                    "`{prop}` can't be {}, only {}",
                    spell(&value),
                    allowed.iter().map(spell).collect::<Vec<_>>().join(", ")
                )
            })
        }
        check("fill", patch.fill, self.fill)
            .or_else(|| check("outline", patch.outline, self.outline))
            .or_else(|| check("shadow", patch.shadow, self.shadow))
            .or_else(|| check("shape", patch.shape, self.shape))
            .or_else(|| check("hover", patch.hover, self.hover))
            .or_else(|| check("selected", patch.selected, self.selected))
    }
}

/// One distinct piece of UI a theme can style: a kind, the group within it
/// the piece belongs to, and the built-in changes this piece makes to what
/// it takes from them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SurfaceRole {
    /// What a theme file calls it: the kind's name for a kind, or a dotted
    /// name for a place, like `sidebar.row`.
    pub id: &'static str,
    pub kind: Kind,
    /// The group it belongs to within its kind; the kind itself when unset.
    pub parent: Option<&'static SurfaceRole>,
    /// What the theme creator calls it.
    pub label: &'static str,
    /// Where it shows up, in plain words.
    pub about: &'static str,
    /// How it differs from its parent before any theme speaks.
    pub patch: StylePatch,
    pub knobs: Option<Knobs>,
    /// A row in a list: its round corners sit concentric inside the list,
    /// and a bar highlight runs down its leading edge.
    pub row: bool,
    /// Whether its round shape turns into a pill at the largest radius; its
    /// group's, or its kind's, when unset.
    pub capsule: Option<bool>,
}

impl SurfaceRole {
    pub const fn new(
        id: &'static str,
        kind: Kind,
        label: &'static str,
        about: &'static str,
    ) -> Self {
        Self {
            id,
            kind,
            parent: None,
            label,
            about,
            patch: StylePatch::EMPTY,
            knobs: None,
            row: false,
            capsule: None,
        }
    }

    /// A role within the group `parent`, taking its kind and its look.
    pub const fn under(
        parent: &'static SurfaceRole,
        id: &'static str,
        label: &'static str,
        about: &'static str,
    ) -> Self {
        Self {
            parent: Some(parent),
            row: parent.row,
            ..Self::new(id, parent.kind, label, about)
        }
    }

    pub const fn with(self, patch: StylePatch) -> Self {
        Self { patch, ..self }
    }

    pub const fn with_knobs(self, knobs: Knobs) -> Self {
        Self {
            knobs: Some(knobs),
            ..self
        }
    }

    pub const fn rows(self) -> Self {
        Self { row: true, ..self }
    }

    pub const fn capsule(self, capsule: bool) -> Self {
        Self {
            capsule: Some(capsule),
            ..self
        }
    }

    pub const fn capsules(&self) -> bool {
        match (self.capsule, self.parent) {
            (Some(capsule), _) => capsule,
            (None, Some(parent)) => parent.capsules(),
            (None, None) => matches!(self.kind, Kind::Button | Kind::Field),
        }
    }

    pub const fn knobs(&self) -> Knobs {
        match (self.knobs, self.parent) {
            (Some(knobs), _) => knobs,
            (None, Some(parent)) => parent.knobs(),
            (None, None) => self.kind.knobs(),
        }
    }

    /// Whether this role is its kind, not a place within it.
    pub fn is_kind(&self) -> bool {
        self.id == self.kind.name()
    }

    /// The group this role belongs to: its parent, or its kind.
    pub fn group(&self) -> Option<Self> {
        match self.parent {
            Some(parent) => Some(*parent),
            None if self.is_kind() => None,
            None => Some(self.kind.role()),
        }
    }

    /// This role, then each group it belongs to out to its kind.
    pub fn lineage(self) -> impl Iterator<Item = Self> {
        std::iter::successors(Some(self), Self::group)
    }

    pub const POPOVER: Self = Self::new(
        "popover",
        Kind::Floating,
        "Menus and popovers",
        "Menus, dropdowns and popovers",
    )
    .with_knobs(Knobs {
        shape: Knobs::BOXES,
        ..Kind::Floating.knobs()
    });

    pub const DIALOG: Self = Self::new(
        "dialog",
        Kind::Floating,
        "Dialogs",
        "Dialogs and modal pickers",
    )
    .with_knobs(Knobs {
        shape: Knobs::BOXES,
        ..Kind::Floating.knobs()
    });

    pub const SHEET: Self = Self::new(
        "sheet",
        Kind::Floating,
        "Sheets",
        "Sheets that rise from the bottom",
    )
    .with_knobs(Knobs {
        shape: Knobs::BOXES,
        ..Kind::Floating.knobs()
    });

    pub const TOAST: Self = Self::new(
        "toast",
        Kind::Floating,
        "Toasts",
        "Notifications and banners",
    );

    /// A tooltip rests just off what it explains, so it casts the contact
    /// shadow.
    pub const TOOLTIP: Self = Self::new(
        "tooltip",
        Kind::Floating,
        "Tooltips",
        "Tooltips and key hints",
    )
    .with(
        StylePatch::EMPTY
            .shadow(Shadow::Contact)
            .selected(Highlight::None),
    );

    /// A button that is only an icon: no fill until pointed at.
    pub const ICON_BUTTON: Self = Self::new(
        "icon-button",
        Kind::Button,
        "Icon buttons",
        "Buttons that are only an icon",
    )
    .with(
        StylePatch::EMPTY
            .fill(Fill::None)
            .outline(false)
            .shadow(Shadow::None)
            .hover(Highlight::Lift)
            .selected(Highlight::Lift),
    );

    /// A button that shows no fill until pointed at.
    pub const QUIET_BUTTON: Self = Self::new(
        "button.quiet",
        Kind::Button,
        "Quiet buttons",
        "Buttons with no fill until pointed at: Cancel, Skip, text actions",
    )
    .with(
        StylePatch::EMPTY
            .fill(Fill::None)
            .outline(false)
            .shadow(Shadow::None)
            .hover(Highlight::Lift)
            .selected(Highlight::Lift),
    );

    /// An icon button set into a bar, a field or a row: it washes while
    /// pointed at but never rises.
    pub const FLAT_ICON_BUTTON: Self = Self::under(
        &Self::ICON_BUTTON,
        "icon-button.flat",
        "Flat icon buttons",
        "Icon buttons set into a bar, a field or a row, that wash but never rise",
    )
    .with(
        StylePatch::EMPTY
            .hover(Highlight::Tint)
            .selected(Highlight::Tint),
    );

    /// A row you press to run or pick something.
    pub const MENU_ITEM: Self = Self::new(
        "menu-item",
        Kind::Button,
        "Menu items",
        "Rows you press to run or pick something: menus, palettes, pickers, dropdowns",
    )
    .rows()
    .capsule(false)
    .with(
        StylePatch::EMPTY
            .fill(Fill::None)
            .outline(false)
            .shadow(Shadow::None)
            .hover(Highlight::Tint)
            .selected(Highlight::Accent),
    )
    .with_knobs(Knobs {
        shadow: &[],
        ..Kind::Button.knobs()
    });

    /// A row that shows which one is open or current.
    pub const ROW: Self = Self::new(
        "row",
        Kind::Item,
        "Rows",
        "Rows that show which one is open or current",
    )
    .rows()
    .capsule(true);

    pub const TAB: Self =
        Self::new("tab", Kind::Item, "Tabs", "Tabs and window pills").capsule(true);

    /// A pickable thumbnail, raised like a card.
    pub const TILE: Self = Self::new("tile", Kind::Item, "Tiles", "Pickable thumbnails").with(
        StylePatch::EMPTY
            .fill(Fill::Solid)
            .outline(true)
            .shadow(Shadow::Contact)
            .hover(Highlight::None)
            .selected(Highlight::Ring),
    );

    /// A field that holds more than one line. It keeps the theme's radius
    /// where a single-line field may be a pill.
    pub const TEXT_AREA: Self = Self::new(
        "field.multiline",
        Kind::Field,
        "Text areas",
        "Fields that hold more than one line, and framed code editors",
    )
    .with(StylePatch::EMPTY.shape(Shape::Round))
    .capsule(false);

    pub const BADGE: Self =
        Self::new("badge", Kind::Label, "Badges", "Small labels and counts").capsule(true);

    pub const KEYCAP: Self = Self::new("keycap", Kind::Label, "Keycaps", "Key hints");

    /// The handle a sheet or a bar is dragged by, filled with ink.
    pub const GRABBER: Self = Self::under(
        &Self::BADGE,
        "grabber",
        "Grabbers",
        "The handles on the phone bar and on sheets",
    )
    .with(StylePatch::EMPTY.shape(Shape::Pill).outline(false));

    /// The roles the kit itself names: every kind and the groups within
    /// them.
    pub const KIT: [Self; 26] = [
        Kind::Bar.role(),
        Kind::Panel.role(),
        Kind::Pane.role(),
        Kind::Card.role(),
        Kind::Floating.role(),
        Self::POPOVER,
        Self::DIALOG,
        Self::SHEET,
        Self::TOAST,
        Self::TOOLTIP,
        Kind::Button.role(),
        Self::QUIET_BUTTON,
        Self::ICON_BUTTON,
        Self::FLAT_ICON_BUTTON,
        Self::MENU_ITEM,
        Kind::Item.role(),
        Self::ROW,
        Self::TAB,
        Self::TILE,
        Kind::Field.role(),
        Self::TEXT_AREA,
        Kind::Label.role(),
        Self::BADGE,
        Self::KEYCAP,
        Self::GRABBER,
        Kind::Rule.role(),
    ];
}

impl From<Kind> for SurfaceRole {
    fn from(kind: Kind) -> Self {
        kind.role()
    }
}

/// One of the theme's shadows, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShadowMaterial {
    pub y: f32,
    pub blur: f32,
    pub spread: f32,
    /// How dark it is: 1 is the theme's shadow color as the mode sets it,
    /// 0 casts nothing.
    pub strength: f32,
}

impl ShadowMaterial {
    pub const CONTACT: Self = Self {
        y: 1.0,
        blur: 2.0,
        spread: 0.0,
        strength: 1.0,
    };
    pub const FLOAT: Self = Self {
        y: 12.0,
        blur: 32.0,
        spread: -4.0,
        strength: 1.0,
    };
}

impl Default for ShadowMaterial {
    fn default() -> Self {
        Self::CONTACT
    }
}

/// How far a shadow's ink goes on the contact shadow: the theme's shadow
/// color at this alpha, times the material's strength.
const CONTACT_ALPHA: f32 = 0.2;
/// The width of an accent ring around a chosen tile.
const RING_WIDTH: Pixels = px(2.0);
/// The width of an accent bar along a selected row.
const BAR_WIDTH: Pixels = px(3.0);
/// How strongly a control's outline shows its ink.
const CONTROL_EDGE_ALPHA: f32 = 0.1;
const DISABLED: f32 = 0.5;
/// The marker ring of the role a theme editor inspects: a magenta no theme
/// uses, so it reads on every surface. It runs just inside the edge, so a
/// translucent fill never shows it through.
const INSPECT_COLOR: Hsla = Hsla {
    h: 0.833,
    s: 1.0,
    l: 0.55,
    a: 1.0,
};

thread_local! {
    static DRAWN: RefCell<Option<BTreeSet<&'static str>>> = const { RefCell::new(None) };
}

pub fn note_drawn_roles() {
    DRAWN.with_borrow_mut(|drawn| *drawn = Some(BTreeSet::new()));
}

pub fn drawn_roles() -> BTreeSet<&'static str> {
    DRAWN.with_borrow_mut(|drawn| drawn.take().unwrap_or_default())
}

/// A role drawn in a tone, on a ground.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Surface {
    pub role: SurfaceRole,
    pub tone: Tone,
    /// Filled in the tone: the one action to take here.
    pub prominent: bool,
    /// What it sits on; the window when unset.
    pub ground: Option<Ground>,
}

impl Surface {
    pub const fn new(role: SurfaceRole) -> Self {
        Self {
            role,
            tone: Tone::Neutral,
            prominent: false,
            ground: None,
        }
    }

    pub const fn tone(self, tone: Tone) -> Self {
        Self { tone, ..self }
    }

    pub const fn prominent(self, prominent: bool) -> Self {
        Self { prominent, ..self }
    }

    pub const fn on(self, ground: Ground) -> Self {
        Self {
            ground: Some(ground),
            ..self
        }
    }

    pub fn on_maybe(self, ground: Option<Ground>) -> Self {
        Self {
            ground: ground.or(self.ground),
            ..self
        }
    }
}

impl From<SurfaceRole> for Surface {
    fn from(role: SurfaceRole) -> Self {
        Self::new(role)
    }
}

impl From<Kind> for Surface {
    fn from(kind: Kind) -> Self {
        Self::new(kind.role())
    }
}

/// Everything a surface paints in one state.
#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    pub fill: Hsla,
    pub glass: Option<GlassMaterial>,
    pub border: Hsla,
    /// The same in every state of a role, so a highlight never moves what
    /// is inside it.
    pub border_width: Pixels,
    pub shadow: Vec<BoxShadow>,
    pub text: Hsla,
    pub radius: Pixels,
    /// What content inside it paints on.
    pub ground: Ground,
}

impl Layer {
    pub(crate) fn muted_text(&self) -> Hsla {
        if super::color::is_colored(self.ground.fill) {
            return self.text;
        }
        self.text
            .mix_oklab(self.ground.fill, 1.0 - super::color::muted_weight())
    }
}

impl Theme {
    /// Whether the theme's patch for `group`, one of the groups `role`
    /// belongs to, still changes `role`: some prop its knobs open is left by
    /// the theme's patches for everything between them.
    pub fn group_reaches(&self, group: SurfaceRole, role: SurfaceRole) -> bool {
        if !role.lineage().any(|node| node.id == group.id) {
            return false;
        }
        let named = |patch: StylePatch| {
            [
                patch.fill.is_some(),
                patch.outline.is_some(),
                patch.shadow.is_some(),
                patch.shape.is_some(),
                patch.hover.is_some(),
                patch.selected.is_some(),
            ]
        };
        let mut held = [false; 6];
        for node in role.lineage().take_while(|node| node.id != group.id) {
            let themed = self.styles.get(node.id).copied().unwrap_or_default();
            for (held, themed) in held.iter_mut().zip(named(themed)) {
                *held |= themed;
            }
        }
        held.into_iter()
            .zip(role.knobs().open())
            .any(|(held, open)| open && !held)
    }

    fn built_in(node: SurfaceRole, style: &mut Style) {
        if let Some(group) = node.group() {
            Self::built_in(group, style);
        }
        node.patch.apply(style);
    }

    fn themed(&self, node: SurfaceRole, knobs: Knobs, style: &mut Style) {
        if let Some(group) = node.group() {
            self.themed(group, knobs, style);
        }
        if let Some(patch) = self.styles.get(node.id) {
            knobs.admit(*patch).apply(style);
        }
    }

    /// The style `role` resolves to in this theme: its kind's built-in
    /// style and each group's built-in patch from the kind inward, then the
    /// theme's patches the same way, so a theme's patch for a kind beats
    /// the built-in look of everything in it.
    pub fn style(&self, role: SurfaceRole) -> Style {
        DRAWN.with_borrow_mut(|drawn| {
            if let Some(drawn) = drawn {
                drawn.insert(role.id);
                for group in role.lineage().skip(1) {
                    if self.group_reaches(group, role) {
                        drawn.insert(group.id);
                    }
                }
            }
        });
        let mut style = role.kind.base();
        Self::built_in(role, &mut style);
        self.themed(role, role.knobs(), &mut style);
        style
    }

    /// The corner radius `shape` gives `role`. At the theme's largest radius
    /// every button is a pill.
    pub fn shape_radius(&self, role: SurfaceRole, shape: Shape) -> Pixels {
        match shape {
            Shape::Square => px(0.0),
            Shape::Pill => FULL_CORNER_RADIUS,
            Shape::Round if role.row && !self.inset_rows() => px(0.0),
            Shape::Round if role.capsules() && self.control_radius() == FULL_CORNER_RADIUS => {
                FULL_CORNER_RADIUS
            }
            Shape::Round if role.row => self.menu_radius(),
            Shape::Round => self.radius,
        }
    }

    /// The theme's contact shadow.
    pub fn contact_shadows(&self) -> Vec<BoxShadow> {
        let material = self.contact_shadow;
        if material.strength <= 0.0 {
            return Vec::new();
        }
        vec![shadow_of(
            material,
            self.shadow_color
                .alpha((CONTACT_ALPHA * material.strength).min(1.0)),
        )]
    }

    /// The theme's floating shadow: the contact shadow under a deep one.
    pub fn float_shadows(&self) -> Vec<BoxShadow> {
        let material = self.float_shadow;
        let mut shadows = self.contact_shadows();
        if material.strength > 0.0 {
            let ink = self.shadow_color;
            shadows.push(shadow_of(
                material,
                ink.alpha((ink.a * material.strength).min(1.0)),
            ));
        }
        shadows
    }

    /// The shadows `shadow` names.
    pub fn shadows_of(&self, shadow: Shadow) -> Vec<BoxShadow> {
        match shadow {
            Shadow::None => Vec::new(),
            Shadow::Contact => self.contact_shadows(),
            Shadow::Float => self.float_shadows(),
        }
    }

    /// The theme's glass when it draws more than a plain fill would.
    fn drawn_glass(&self) -> Option<GlassMaterial> {
        self.glass.filter(|material| !material.is_opaque_fill())
    }

    /// How `surface` paints in `state`.
    pub fn layer(&self, surface: impl Into<Surface>, state: PaintState) -> Layer {
        let surface = surface.into();
        let kind = surface.role.kind;
        let style = self.style(surface.role);
        let ground = surface.ground.unwrap_or_else(|| self.ground());
        let mut glass = match style.fill {
            Fill::Glass if !ground.glass => self.drawn_glass(),
            _ => None,
        };
        let fill = match style.fill {
            Fill::Glass if ground.glass => Fill::Tint,
            Fill::Glass if glass.is_none() => Fill::Solid,
            fill => fill,
        };
        let tone = if surface.prominent && surface.tone == Tone::Neutral {
            Tone::Primary
        } else {
            surface.tone
        };

        let soft = self.paint_on(ground, tone, Emphasis::Soft);
        let solid = self.paint_on(ground, tone, Emphasis::Solid);
        let base = if kind.floats() {
            self.background.opaque()
        } else {
            ground.fill.opaque()
        };
        let step = match kind.elevation() {
            Elevation::Popover => 2,
            _ => 1,
        };
        let paint = if surface.prominent && fill != Fill::Glass {
            solid
        } else if kind.is_control() {
            match fill {
                Fill::None => self.paint_on(ground, tone, Emphasis::Ghost),
                Fill::Tint => soft,
                Fill::Solid | Fill::Glass => soft.opaque(),
            }
        } else {
            let rest = match fill {
                Fill::None => self.transparent,
                Fill::Tint => base.washed(step),
                Fill::Solid | Fill::Glass => base.raised(step),
            };
            let over = |level: u8| match fill {
                Fill::None => base.washed(level),
                Fill::Tint => base.washed(step + level),
                Fill::Solid | Fill::Glass => rest.blend(base.washed(level)),
            };
            super::Paint {
                fill: rest,
                hover: over(1),
                active: over(2),
                current: over(2),
                ..self.paint_on(ground, tone, Emphasis::Ghost)
            }
        };
        let paint = if surface.prominent {
            super::Paint {
                text: solid.text,
                selected_text: solid.selected_text,
                ..paint
            }
        } else {
            paint
        };

        let highlight = match state {
            PaintState::Hover | PaintState::Active => style.hover,
            PaintState::Selected | PaintState::Current | PaintState::Focused => style.selected,
            _ => Highlight::None,
        };
        let highlight = match highlight {
            Highlight::Glass if glass.is_some() || ground.glass => Highlight::Tint,
            Highlight::Glass if self.drawn_glass().is_none() => Highlight::Lift,
            highlight => highlight,
        };
        let flattened = self.flat_highlights && highlight == Highlight::Lift;
        let highlight = if flattened {
            Highlight::Tint
        } else {
            highlight
        };
        if highlight == Highlight::Glass {
            glass = self.drawn_glass();
        }
        let washed = |paint: &super::Paint| {
            let step = (paint.current.a - paint.hover.a).max(0.0);
            let deeper = |wash: Hsla| wash.alpha((wash.a + step).min(1.0));
            match (state, flattened) {
                (PaintState::Hover, false) => paint.hover,
                (PaintState::Active, false) | (PaintState::Hover, true) => paint.active,
                (PaintState::Active, true) => deeper(paint.active),
                (_, false) => paint.current,
                (_, true) => deeper(paint.current),
            }
        };
        let (mut fill_color, mut text) = match highlight {
            Highlight::Tint | Highlight::Lift => (washed(&paint), paint.text),
            Highlight::Accent => (paint.selected, paint.selected_text),
            Highlight::Glass if kind.is_control() => (soft.opaque().fill, paint.text),
            Highlight::Glass => (base.raised(step), paint.text),
            Highlight::None | Highlight::Ring | Highlight::Bar => (paint.fill, paint.text),
        };
        if state == PaintState::Disabled {
            fill_color = paint.fill.opacity(DISABLED);
            text = paint.text_at(PaintState::Disabled);
        }

        let edge = if kind.is_control() {
            if soft.border_width > px(0.0) && tone != Tone::Neutral {
                soft.border
            } else {
                ground.ink.opacity(CONTROL_EDGE_ALPHA)
            }
        } else {
            self.border()
        };
        let lifts = !self.flat_highlights
            && [style.hover, style.selected].into_iter().any(|highlight| {
                highlight == Highlight::Lift
                    || (highlight == Highlight::Glass && self.drawn_glass().is_none())
            });
        let border_width = if (style.outline || lifts) && self.outlined() {
            self.outline
        } else {
            px(0.0)
        };
        let lifted = highlight == Highlight::Lift && state != PaintState::Disabled;
        let border = if glass.is_some() || border_width == px(0.0) {
            self.transparent
        } else if style.outline || lifted {
            match state {
                PaintState::Disabled => edge.opacity(DISABLED),
                _ => edge,
            }
        } else {
            self.transparent
        };

        let casts = !kind.is_control() || ground.elevation == Elevation::Window;
        let mut shadow = if casts && state != PaintState::Disabled {
            let mut shadow = self.shadows_of(style.shadow);
            if lifted && shadow.is_empty() {
                shadow = self.contact_shadows();
            }
            shadow
        } else {
            Vec::new()
        };
        match highlight {
            Highlight::Ring => shadow.push(BoxShadow {
                color: self.accent.opaque(),
                offset: point(px(0.0), px(0.0)),
                blur_radius: px(0.0),
                spread_radius: RING_WIDTH,
                inset: false,
            }),
            Highlight::Bar => shadow.push(BoxShadow {
                color: self.accent.opaque(),
                offset: if surface.role.row {
                    point(BAR_WIDTH, px(0.0))
                } else {
                    point(px(0.0), -BAR_WIDTH)
                },
                blur_radius: px(0.0),
                spread_radius: px(0.0),
                inset: true,
            }),
            _ => {}
        }
        if shadow.iter().any(|shadow| !shadow.inset) && glass.is_none() && !ground.glass {
            fill_color = ground.fill.opaque().blend(fill_color);
        }

        if self.inspect_strength > 0.0
            && self.inspect.as_deref().is_some_and(|inspect| {
                inspect == surface.role.id
                    || surface
                        .role
                        .lineage()
                        .skip(1)
                        .any(|group| inspect == group.id && self.group_reaches(group, surface.role))
            })
        {
            shadow.push(BoxShadow {
                color: INSPECT_COLOR.alpha(self.inspect_strength.min(1.0)),
                offset: point(px(0.0), px(0.0)),
                blur_radius: px(0.0),
                spread_radius: RING_WIDTH,
                inset: true,
            });
        }
        let glass = glass.map(|material| {
            let tint = if surface.prominent {
                self.tone(tone).opaque()
            } else {
                fill_color.opaque()
            };
            material.tint(tint.alpha(material.tint.a)).fallback(tint)
        });
        let under = ground.fill.opaque().blend(fill_color);
        Layer {
            fill: fill_color,
            border,
            border_width,
            shadow,
            text,
            radius: self.shape_radius(surface.role, style.shape),
            ground: Ground {
                fill: under,
                ink: text,
                elevation: if fill == Fill::None {
                    ground.elevation
                } else {
                    ground.elevation.max(kind.elevation())
                },
                glass: ground.glass || glass.is_some(),
            },
            glass,
        }
    }

    /// What content inside `surface` paints on while it rests.
    pub fn ground_of(&self, surface: impl Into<Surface>) -> Ground {
        self.layer(surface, PaintState::Rest).ground
    }
}

fn shadow_of(material: ShadowMaterial, color: Hsla) -> BoxShadow {
    BoxShadow {
        color,
        offset: point(px(0.0), px(material.y)),
        blur_radius: px(material.blur.max(0.0)),
        spread_radius: px(material.spread),
        inset: false,
    }
}

/// Which edge of a box a rule runs along.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

/// Paints a surface that follows the pointer.
pub trait InteractiveSurfaceExt: InteractiveElement + Styled + Sized {
    /// `surface` in `state`, taking its hover highlight while pointed at
    /// when it rests.
    fn surface_hover(self, surface: impl Into<Surface>, state: PaintState, cx: &App) -> Self {
        let surface = surface.into();
        let theme = cx.theme();
        let this = super::StyledExt::surface_at(self, surface, state, cx);
        if !matches!(state, PaintState::Rest | PaintState::Focused) {
            return this;
        }
        let hover = theme.layer(surface, PaintState::Hover);
        this.hover(move |style| restyle(style, &hover))
    }
}

impl<E: InteractiveElement + Styled> InteractiveSurfaceExt for E {}

/// Paints a surface that follows the pointer and presses, on an element with
/// an id.
pub trait StatefulSurfaceExt: StatefulInteractiveElement + Styled + Sized {
    /// `surface` in `state`, with its hover and pressed highlights when it
    /// rests.
    fn surface_interactive(self, surface: impl Into<Surface>, state: PaintState, cx: &App) -> Self {
        let surface = surface.into();
        let theme = cx.theme();
        let this = self.surface_hover(surface, state, cx);
        if !matches!(state, PaintState::Rest | PaintState::Focused) {
            return this;
        }
        let active = theme.layer(surface, PaintState::Active);
        this.active(move |style| restyle(style, &active))
    }
}

impl<E: StatefulInteractiveElement + Styled> StatefulSurfaceExt for E {}

pub(crate) fn restyle(style: zz_gpui::StyleRefinement, layer: &Layer) -> zz_gpui::StyleRefinement {
    let style = style
        .border_color(layer.border)
        .shadow(layer.shadow.clone())
        .text_color(layer.text);
    match layer.glass {
        Some(material) => style.glass(material).bg(zz_gpui::transparent_black()),
        None => style.bg(layer.fill),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ThemeColor;

    fn theme() -> Theme {
        Theme::from(&*ThemeColor::dark())
    }

    #[test]
    fn flat_highlights_wash_a_step_deeper_instead_of_lifting() {
        let mut theme = theme();
        let lifted = theme.layer(SurfaceRole::TAB, PaintState::Selected);
        assert!(!lifted.shadow.is_empty());
        assert!(lifted.border_width > px(0.0));
        let tinted = SurfaceRole::under(&SurfaceRole::TAB, "test.tab", "Test", "")
            .with(StylePatch::EMPTY.selected(Highlight::Tint));
        let tint = theme.layer(tinted, PaintState::Selected);

        theme.flat_highlights = true;
        let flat = theme.layer(SurfaceRole::TAB, PaintState::Selected);
        assert!(flat.shadow.is_empty());
        assert_eq!(flat.border_width, px(0.0));
        assert!(flat.fill.a > tint.fill.a);
        assert_eq!(
            theme.layer(SurfaceRole::TILE, PaintState::Hover).fill,
            theme.layer(SurfaceRole::TILE, PaintState::Rest).fill,
            "a surface that does not highlight stays bare"
        );
    }

    #[test]
    fn a_theme_patch_beats_every_built_in_and_the_closest_patch_wins() {
        let mut theme = theme();
        let role = SurfaceRole::under(&SurfaceRole::MENU_ITEM, "test.row", "Test", "")
            .with(StylePatch::EMPTY.selected(Highlight::Tint));
        assert_eq!(theme.style(role).selected, Highlight::Tint);
        assert_eq!(
            theme.style(role).hover,
            SurfaceRole::MENU_ITEM.patch.hover.unwrap()
        );
        assert!(role.row);

        theme
            .styles
            .insert("button".into(), StylePatch::EMPTY.hover(Highlight::Glass));
        assert_eq!(theme.style(role).hover, Highlight::Glass);
        assert_eq!(theme.style(role).selected, Highlight::Tint);
        theme.styles.insert(
            "menu-item".into(),
            StylePatch::EMPTY
                .hover(Highlight::Accent)
                .selected(Highlight::Bar),
        );
        assert_eq!(theme.style(role).hover, Highlight::Accent);
        assert_eq!(theme.style(role).selected, Highlight::Bar);

        theme.styles.insert(
            "test.row".into(),
            StylePatch::EMPTY.selected(Highlight::Ring),
        );
        assert_eq!(theme.style(role).selected, Highlight::Ring);
        assert_eq!(theme.style(SurfaceRole::MENU_ITEM).selected, Highlight::Bar);
        assert_eq!(theme.style(Kind::Button.role()).hover, Highlight::Glass);
    }

    #[test]
    fn a_group_patch_reaches_its_members_and_rings_them() {
        let mut theme = theme();
        theme.styles.insert(
            "icon-button".into(),
            StylePatch::EMPTY.fill(Fill::Glass).hover(Highlight::Ring),
        );
        let flat = theme.style(SurfaceRole::FLAT_ICON_BUTTON);
        assert_eq!(flat.fill, Fill::Glass);
        assert_eq!(flat.hover, Highlight::Ring);
        theme.styles.insert(
            "icon-button.flat".into(),
            StylePatch::EMPTY.hover(Highlight::Tint),
        );
        assert_eq!(
            theme.style(SurfaceRole::FLAT_ICON_BUTTON).hover,
            Highlight::Tint
        );
        assert!(theme.group_reaches(SurfaceRole::ICON_BUTTON, SurfaceRole::FLAT_ICON_BUTTON));
        assert!(!theme.group_reaches(SurfaceRole::ICON_BUTTON, SurfaceRole::QUIET_BUTTON));
        theme.inspect = Some("icon-button".into());
        let ringed = |role| {
            theme
                .layer(role, PaintState::Rest)
                .shadow
                .iter()
                .any(|shadow| shadow.color == INSPECT_COLOR)
        };
        assert!(ringed(SurfaceRole::FLAT_ICON_BUTTON));
        assert!(!ringed(SurfaceRole::QUIET_BUTTON));
    }

    #[test]
    fn glass_paints_a_tint_on_glass_and_solid_without_the_themes_glass() {
        let mut theme = theme();
        let popover = SurfaceRole::POPOVER;
        theme.glass = None;
        let plain = theme.layer(popover, PaintState::Rest);
        assert!(plain.glass.is_none());
        assert_eq!(plain.fill.a, 1.0);

        theme.glass = Some(GlassMaterial::frosted());
        let frosted = theme.layer(popover, PaintState::Rest);
        assert!(frosted.glass.is_some());
        assert!(frosted.ground.glass);

        let nested = theme.layer(Surface::new(popover).on(frosted.ground), PaintState::Rest);
        assert!(nested.glass.is_none());
        assert!(nested.fill.a < 1.0);
    }

    #[test]
    fn a_glass_highlight_turns_a_solid_surface_to_glass_while_it_lasts() {
        let mut theme = theme();
        theme.glass = Some(GlassMaterial::frosted());
        let tab = SurfaceRole::new("test.tab", Kind::Item, "Test", "").with(
            StylePatch::EMPTY
                .fill(Fill::Solid)
                .selected(Highlight::Glass),
        );
        let rest = theme.layer(tab, PaintState::Rest);
        let selected = theme.layer(tab, PaintState::Selected);
        assert!(rest.glass.is_none());
        assert!(!rest.ground.glass);
        assert!(selected.glass.is_some());
        assert!(selected.ground.glass);
        assert_eq!(selected.border_width, rest.border_width);

        let nested = theme.layer(Surface::new(tab).on(selected.ground), PaintState::Selected);
        assert!(nested.glass.is_none());
        assert_eq!(
            nested.fill,
            theme
                .layer(
                    Surface::new(tab.with(tab.patch.selected(Highlight::Tint))).on(selected.ground),
                    PaintState::Selected
                )
                .fill
        );

        theme.glass = None;
        let lifted = theme.layer(tab, PaintState::Selected);
        assert!(lifted.glass.is_none());
        assert!(!lifted.shadow.is_empty());
        assert_ne!(lifted.fill, theme.layer(tab, PaintState::Rest).fill);
    }

    #[test]
    fn a_prominent_glass_surface_takes_the_ink_of_its_tone() {
        let mut theme = theme();
        theme.glass = Some(GlassMaterial::frosted());
        let role = SurfaceRole::new("test.send", Kind::Button, "Test", "")
            .with(StylePatch::EMPTY.fill(Fill::Glass));
        let layer = theme.layer(Surface::new(role).prominent(true), PaintState::Rest);
        assert!(layer.glass.is_some());
        assert_eq!(layer.text, theme.paint(Tone::Primary, Emphasis::Solid).text);
    }

    #[test]
    fn restyling_a_glass_layer_carries_its_glass() {
        let mut theme = theme();
        theme.glass = Some(GlassMaterial::frosted());
        let hover = theme.layer(SurfaceRole::POPOVER, PaintState::Hover);
        assert!(hover.glass.is_some());
        let restyled = restyle(zz_gpui::StyleRefinement::default().bg(hover.fill), &hover);
        assert_eq!(restyled.glass, hover.glass);
        assert!(
            restyled
                .background
                .and_then(|fill| fill.color())
                .is_none_or(|color| color.is_transparent())
        );
    }

    #[test]
    fn a_tinted_surface_deepens_on_hover_and_selection() {
        let theme = theme();
        let role = SurfaceRole::new("test.tile", Kind::Card, "Test", "").with(
            StylePatch::EMPTY
                .fill(Fill::Tint)
                .shadow(Shadow::None)
                .hover(Highlight::Tint)
                .selected(Highlight::Tint),
        );
        let rest = theme.layer(role, PaintState::Rest).fill;
        let hover = theme.layer(role, PaintState::Hover).fill;
        let current = theme.layer(role, PaintState::Current).fill;
        assert!(hover.a > rest.a, "{rest:?} {hover:?}");
        assert!(current.a > hover.a, "{hover:?} {current:?}");
        let clear = SurfaceRole::new("test.clear", Kind::Card, "Test", "").with(
            StylePatch::EMPTY
                .fill(Fill::None)
                .shadow(Shadow::None)
                .hover(Highlight::Tint),
        );
        assert!(theme.layer(clear, PaintState::Hover).fill.a > 0.0);
    }

    #[test]
    fn a_ring_never_shows_through_its_fill() {
        let theme = theme();
        let role = SurfaceRole::new("test.ring", Kind::Card, "Test", "").with(
            StylePatch::EMPTY
                .fill(Fill::Tint)
                .shadow(Shadow::None)
                .selected(Highlight::Ring),
        );
        assert_eq!(theme.layer(role, PaintState::Current).fill.a, 1.0);
        assert!(theme.layer(role, PaintState::Rest).fill.a < 1.0);
    }

    #[test]
    fn controls_cast_shadows_only_on_the_window() {
        let theme = theme();
        let button = Kind::Button.role();
        assert!(!theme.layer(button, PaintState::Rest).shadow.is_empty());
        let panel = theme.surface(Elevation::Panel);
        assert!(
            theme
                .layer(Surface::new(button).on(panel), PaintState::Rest)
                .shadow
                .is_empty()
        );
        assert!(
            !theme
                .layer(Surface::new(Kind::Card.role()).on(panel), PaintState::Rest)
                .shadow
                .is_empty()
        );
    }

    #[test]
    fn highlights_never_change_the_border_width() {
        let theme = theme();
        for role in SurfaceRole::KIT {
            let widths = PaintState::ALL.map(|state| theme.layer(role, state).border_width);
            assert!(
                widths.iter().all(|width| *width == widths[0]),
                "{} moves its border: {widths:?}",
                role.id
            );
        }
    }

    #[test]
    fn the_inspected_role_rings_every_surface_it_names() {
        let mut theme = theme();
        let role = Kind::Card.role();
        let rings = |theme: &Theme| {
            theme
                .layer(role, PaintState::Rest)
                .shadow
                .iter()
                .filter(|shadow| shadow.color == INSPECT_COLOR)
                .count()
        };
        assert_eq!(rings(&theme), 0);
        theme.inspect = Some("card".into());
        assert_eq!(rings(&theme), 1);
        theme.inspect = Some("row".into());
        assert_eq!(rings(&theme), 0);
        theme.inspect = Some("card".into());
        theme.inspect_strength = 0.0;
        assert_eq!(rings(&theme), 0);
    }

    #[test]
    fn inspecting_a_kind_rings_every_role_of_it() {
        let mut theme = theme();
        theme.inspect = Some("button".into());
        let ringed = |theme: &Theme, role: SurfaceRole| {
            theme
                .layer(role, PaintState::Rest)
                .shadow
                .iter()
                .any(|shadow| shadow.color == INSPECT_COLOR)
        };
        assert!(ringed(&theme, Kind::Button.role()));
        assert!(ringed(&theme, SurfaceRole::QUIET_BUTTON));
        assert!(!ringed(&theme, Kind::Card.role()));
        let sealed =
            SurfaceRole::new("button.sealed", Kind::Button, "Sealed", "").with_knobs(Knobs::FIXED);
        assert!(!ringed(&theme, sealed));
        theme.styles.insert(
            "button.quiet".into(),
            StylePatch::EMPTY
                .fill(Fill::None)
                .outline(false)
                .shadow(Shadow::None)
                .shape(Shape::Pill)
                .hover(Highlight::Lift)
                .selected(Highlight::Lift),
        );
        assert!(!ringed(&theme, SurfaceRole::QUIET_BUTTON));
    }

    #[test]
    fn a_kind_patch_reaches_only_what_a_role_takes() {
        let mut theme = theme();
        let fixed =
            SurfaceRole::new("card.fixed", Kind::Card, "Fixed", "").with_knobs(Knobs::FIXED);
        theme.styles.insert(
            "card".into(),
            StylePatch::EMPTY.fill(Fill::Glass).shape(Shape::Pill),
        );
        assert_eq!(theme.style(fixed), Kind::Card.base());
        assert_eq!(theme.style(Kind::Card.role()).fill, Fill::Glass);
        assert_eq!(theme.style(Kind::Card.role()).shape, Shape::Round);
        assert!(!theme.group_reaches(Kind::Card.role(), fixed));
        theme
            .styles
            .insert("panel".into(), StylePatch::EMPTY.fill(Fill::Glass));
        assert_eq!(theme.style(Kind::Panel.role()), Kind::Panel.base());
    }

    #[test]
    fn noting_drawn_roles_names_each_role_and_its_kind() {
        let theme = theme();
        theme.style(SurfaceRole::QUIET_BUTTON);
        assert!(drawn_roles().is_empty());
        note_drawn_roles();
        theme.style(SurfaceRole::QUIET_BUTTON);
        let drawn = drawn_roles();
        assert_eq!(
            drawn,
            BTreeSet::from([SurfaceRole::QUIET_BUTTON.id, "button"])
        );
        theme.style(Kind::Card.role());
        assert!(drawn_roles().is_empty());
    }

    #[test]
    fn a_patch_between_two_styles_names_only_what_differs() {
        let from = Kind::Button.base();
        let to = Style {
            fill: Fill::Glass,
            shape: Shape::Pill,
            ..from
        };
        let patch = StylePatch::between(from, to);
        assert_eq!(
            patch,
            StylePatch::EMPTY.fill(Fill::Glass).shape(Shape::Pill)
        );
        let mut style = from;
        patch.apply(&mut style);
        assert_eq!(style, to);
    }

    #[test]
    fn theme_files_spell_styles_in_lowercase() {
        let patch: StylePatch = serde_json::from_str(
            r#"{"fill":"glass","outline":false,"shadow":"float","shape":"pill","hover":"lift","selected":"accent"}"#,
        )
        .unwrap();
        assert_eq!(
            patch,
            StylePatch::EMPTY
                .fill(Fill::Glass)
                .outline(false)
                .shadow(Shadow::Float)
                .shape(Shape::Pill)
                .hover(Highlight::Lift)
                .selected(Highlight::Accent)
        );
        assert!(serde_json::from_str::<StylePatch>(r#"{"fil":"glass"}"#).is_err());
    }
}
