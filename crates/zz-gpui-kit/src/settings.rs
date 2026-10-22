//! Settings pages: a scrolling column of titled stacks, each a run of
//! entries that share one card, with the copy on the left and the control on
//! the right. Also the sidebar rows that pick a page.

use crate::{
    ActiveTheme as _, Colorize as _, Disableable as _, Emphasis, Ground, IconName, Layer,
    PaintState, RULE_WIDTH, Selectable as _, Sizable as _, StyledExt as _,
    badge::Badge,
    button::{Button, ButtonVariants as _},
    icon::{Icon, cap_center_drop},
    roles,
    scroll::ScrollableElement as _,
    select::SelectItem,
    separator::Separator,
};
use zz_gpui::{
    AnyElement, App, Bounds, ElementId, Hsla, IntoElement, ParentElement, Pixels, RenderOnce,
    ScrollHandle, SharedString, Styled as _, div, prelude::*, px, relative,
};

/// The height of a sidebar row that picks a page.
pub const SETTINGS_NAVIGATION_ROW_HEIGHT: f32 = 32.0;

/// A sidebar row that picks a page. The caller attaches behavior. It draws as
/// [`roles::SIDEBAR_ROW`], so a theme styles every sidebar row at once.
pub fn settings_navigation_button(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    icon: impl Into<Icon>,
    selected: bool,
    cx: &App,
) -> Button {
    Button::new(id)
        .w_full()
        .px(px(8.0))
        .ghost()
        .surface_role(roles::SIDEBAR_ROW)
        .on(cx.theme().ground_of(roles::SIDEBAR))
        .icon(icon)
        .selected(selected)
        .label(title)
        .child(div().flex_1())
        .when(selected, crate::StyledExt::font_medium)
}

/// The row that leaves a settings route, above its page list.
pub fn settings_navigation_back_button(id: impl Into<ElementId>) -> Button {
    Button::new(id)
        .w_full()
        .px(px(8.0))
        .ghost()
        .surface_role(roles::SIDEBAR_ROW)
        .icon(IconName::ArrowLeft)
        .label("Back")
        .child(div().flex_1())
}

/// Holds a navigation row at the sidebar's row height.
pub fn settings_navigation_row(button: Button) -> zz_gpui::Div {
    div()
        .flex()
        .flex_none()
        .h(px(SETTINGS_NAVIGATION_ROW_HEIGHT))
        .items_center()
        .child(button)
}

/// Label above a group of navigation rows.
pub fn settings_navigation_group_label(title: impl Into<SharedString>, cx: &App) -> zz_gpui::Div {
    div()
        .px(px(8.0))
        .pt(px(12.0))
        .pb(px(2.0))
        .text_sm()
        .text_color(cx.theme().foreground.muted())
        .map(crate::StyledExt::font_medium)
        .child(title.into())
}

const SETTINGS_CONTENT_MAX_WIDTH: f32 = 960.0;
const SETTINGS_PAGE_PADDING: f32 = 14.0;

/// Centered, bounded content shared by every settings page.
pub fn settings_page_content() -> zz_gpui::Div {
    div()
        .flex()
        .flex_col()
        .w_full()
        .min_w_0()
        .max_w(px(SETTINGS_CONTENT_MAX_WIDTH))
        .mx_auto()
}

/// A settings page's heading: its title, then a line saying what it holds.
/// Under a coarse pointer it leaves the title to the sidebar or the bar above
/// the page.
pub fn settings_page_heading(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    cx: &App,
) -> zz_gpui::Div {
    let title = title.into();
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(4.0))
        .when(!crate::touch::CoarsePointer::get(cx), |this| {
            this.child(
                crate::StyledExt::font_medium(div().text_size(crate::rems_from_px(20.0)))
                    .child(title),
            )
        })
        .child(
            div()
                .text_size(crate::rems_from_px(11.0))
                .text_color(cx.theme().foreground.muted())
                .child(description.into()),
        )
}

/// Room a host keeps clear below the last row of every settings page, such as
/// an iOS home indicator the page draws under. Unset means none.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SettingsBottomInset(pub Pixels);

impl zz_gpui::Global for SettingsBottomInset {}

impl SettingsBottomInset {
    #[must_use]
    pub fn get(cx: &App) -> Pixels {
        cx.try_global::<Self>().map_or(px(0.0), |inset| inset.0)
    }
}

/// A settings page: a scrolling column of [`SettingsStack`]s, with the shared
/// scrollbar overlaid. `id` keys the scroll handle, so each page keeps its own
/// position.
#[must_use]
pub fn settings_scroll_column(id: &'static str) -> SettingsScrollColumn {
    SettingsScrollColumn {
        id,
        children: Vec::new(),
    }
}

#[derive(IntoElement)]
pub struct SettingsScrollColumn {
    id: &'static str,
    children: Vec<AnyElement>,
}

impl zz_gpui::ParentElement for SettingsScrollColumn {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for SettingsScrollColumn {
    fn render(self, window: &mut zz_gpui::Window, cx: &mut App) -> impl IntoElement {
        let handle = window
            .use_keyed_state(zz_gpui::ElementId::Name(self.id.into()), cx, |_, _| {
                zz_gpui::ScrollHandle::default()
            })
            .read(cx)
            .clone();
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .relative()
            .child(
                div()
                    .on_children_prepainted({
                        let handle = handle.clone();
                        move |_, window, _| {
                            if let Some(target) = window.take_autoscroll()
                                && reveal_in_scroll(&handle, target)
                            {
                                window.request_animation_frame();
                            }
                        }
                    })
                    .id(self.id)
                    .flex()
                    .flex_col()
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&handle)
                    .p(px(SETTINGS_PAGE_PADDING))
                    .child(
                        settings_page_content()
                            .flex_none()
                            .gap(px(18.0))
                            .children(self.children),
                    )
                    .child(div().flex_none().h(SettingsBottomInset::get(cx))),
            )
            .vertical_scrollbar(&handle)
    }
}

fn reveal_shift(viewport: Bounds<Pixels>, target: Bounds<Pixels>) -> Option<Pixels> {
    if target.right() <= viewport.left() || target.left() >= viewport.right() {
        return None;
    }
    if target.bottom() > viewport.bottom() {
        Some((target.bottom() - viewport.bottom()).min(target.top() - viewport.top()))
    } else if target.top() < viewport.top() {
        Some(target.top() - viewport.top())
    } else {
        None
    }
}

fn reveal_in_scroll(handle: &ScrollHandle, target: Bounds<Pixels>) -> bool {
    let Some(shift) = reveal_shift(handle.bounds(), target) else {
        return false;
    };
    let mut offset = handle.offset();
    let floor = -handle.max_offset().y.max(px(0.0));
    let next = (offset.y - shift).clamp(floor, px(0.0));
    if next == offset.y {
        return false;
    }
    offset.y = next;
    handle.set_offset(offset);
    true
}

const SETTINGS_GROUP_TITLE_SIZE: f32 = 12.0;
const SETTINGS_GROUP_TITLE_LINE: f32 = 18.0;
const SETTINGS_DISCLOSURE_WIDTH: f32 = 14.0;

fn settings_group_header(
    title: SharedString,
    description: Option<SharedString>,
    cx: &App,
) -> zz_gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .px(px(2.0))
        .child(
            crate::StyledExt::font_medium(
                div()
                    .text_size(crate::rems_from_px(SETTINGS_GROUP_TITLE_SIZE))
                    .line_height(px(SETTINGS_GROUP_TITLE_LINE)),
            )
            .child(title),
        )
        .when_some(description, |this, description| {
            this.child(
                div()
                    .text_size(crate::rems_from_px(11.0))
                    .text_color(cx.theme().foreground.muted())
                    .child(description),
            )
        })
}

/// A group heading that shows or hides the rows after it, as a standalone
/// row in [`settings_virtual_column`].
pub fn settings_list_disclosure_header(
    id: &'static str,
    title: &'static str,
    description: Option<&'static str>,
    expanded: bool,
    cx: &App,
) -> zz_gpui::Stateful<zz_gpui::Div> {
    let muted = cx.theme().foreground.muted();
    div()
        .id(id)
        .pt(px(10.0))
        .flex()
        .items_start()
        .gap(px(2.0))
        .cursor_pointer()
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(px(SETTINGS_DISCLOSURE_WIDTH))
                .h(px(SETTINGS_GROUP_TITLE_LINE))
                .child(
                    Icon::new(if expanded {
                        crate::IconName::ChevronDown
                    } else {
                        crate::IconName::ChevronRight
                    })
                    .size(px(SETTINGS_GROUP_TITLE_SIZE))
                    .text_color(muted)
                    .relative()
                    .top(cap_center_drop(
                        px(SETTINGS_GROUP_TITLE_SIZE),
                        px(SETTINGS_GROUP_TITLE_SIZE),
                        cx,
                    )),
                ),
        )
        .child(settings_group_header(
            title.into(),
            description.map(Into::into),
            cx,
        ))
}

/// Group heading used as a standalone row in [`settings_virtual_column`].
pub fn settings_list_group_header(
    title: &'static str,
    description: Option<&'static str>,
    cx: &App,
) -> zz_gpui::Div {
    div().pt(px(10.0)).child(settings_group_header(
        title.into(),
        description.map(Into::into),
        cx,
    ))
}

const SETTINGS_STACK_PADDING: f32 = 12.0;

/// A run of settings sharing one surface, divided by hairlines. Fill it with
/// [`SettingEntry`].
#[derive(IntoElement)]
pub struct SettingsStack {
    title: Option<SharedString>,
    description: Option<SharedString>,
    entries: Vec<SettingEntry>,
}

impl SettingsStack {
    /// An untitled stack, for a run that needs no heading of its own.
    #[must_use]
    pub fn new() -> Self {
        Self {
            title: None,
            description: None,
            entries: Vec::new(),
        }
    }

    #[must_use]
    pub fn titled(title: impl Into<SharedString>) -> Self {
        Self {
            title: Some(title.into()),
            ..Self::new()
        }
    }

    /// A line of context under the title. No effect on an untitled stack.
    #[must_use]
    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Append an entry. The stack assigns its [`StackPosition`] on render.
    #[must_use]
    pub fn child(mut self, entry: SettingEntry) -> Self {
        self.entries.push(entry);
        self
    }

    #[must_use]
    pub fn children(mut self, entries: impl IntoIterator<Item = SettingEntry>) -> Self {
        self.entries.extend(entries);
        self
    }
}

impl Default for SettingsStack {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderOnce for SettingsStack {
    fn render(self, _: &mut zz_gpui::Window, cx: &mut App) -> impl IntoElement {
        let last = self.entries.len().saturating_sub(1);
        let rows = self
            .entries
            .into_iter()
            .enumerate()
            .map(|(index, entry)| entry.position(StackPosition::at(index, last)));

        div()
            .flex()
            .flex_col()
            .w_full()
            .gap(px(8.0))
            .when_some(self.title, |this, title| {
                this.child(settings_group_header(title, self.description, cx))
            })
            .child(div().flex().flex_col().w_full().children(rows))
    }
}

/// Where a [`SettingEntry`] sits in its run: which corners it rounds, and
/// whether a rule separates it from the entry above. [`SettingsStack`] assigns
/// this; a virtualized page has to assign it by hand.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StackPosition {
    /// The only entry in its run: rounds all four corners.
    Only,
    /// Rounds the top corners, and draws no rule above.
    First,
    #[default]
    Middle,
    /// Rounds the bottom corners.
    Last,
}

impl StackPosition {
    /// The position implied by which ends of the run an entry is on. This is
    /// the form a virtualized page needs, where a run is bounded by whatever is
    /// not an entry rather than by a comparable index.
    #[must_use]
    pub const fn new(is_first: bool, is_last: bool) -> Self {
        match (is_first, is_last) {
            (true, true) => Self::Only,
            (true, false) => Self::First,
            (false, true) => Self::Last,
            (false, false) => Self::Middle,
        }
    }

    /// The position of item `index` in a run whose last index is `last`.
    #[must_use]
    pub const fn at(index: usize, last: usize) -> Self {
        Self::new(index == 0, index == last)
    }

    /// Whether the run stops here. The entry rounds its bottom corners, and a
    /// virtualized page owes it the gap before whatever comes next.
    #[must_use]
    pub const fn ends_run(self) -> bool {
        matches!(self, Self::Only | Self::Last)
    }

    const fn rounds_top(self) -> bool {
        matches!(self, Self::Only | Self::First)
    }

    const fn rounds_bottom(self) -> bool {
        self.ends_run()
    }

    const fn rules_above(self) -> bool {
        matches!(self, Self::Middle | Self::Last)
    }
}

/// One setting inside a [`SettingsStack`]: copy on the left, its control
/// centered at the right edge, and room beneath for a full-width control. Each
/// entry draws its own share of the run's surface. See [`StackPosition`].
#[derive(IntoElement)]
pub struct SettingEntry {
    title: SharedString,
    description: SharedString,
    title_icon: Option<Icon>,
    title_actions: Option<AnyElement>,
    control: Option<AnyElement>,
    disabled: bool,
    position: StackPosition,
    children: Vec<AnyElement>,
}

impl SettingEntry {
    pub fn new(title: impl Into<SharedString>, description: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            description: description.into(),
            title_icon: None,
            title_actions: None,
            control: None,
            disabled: false,
            position: StackPosition::Middle,
            children: Vec::new(),
        }
    }

    /// A state glyph drawn before the title; see [`SettingCopy::title_icon`].
    #[must_use]
    pub fn title_icon(mut self, icon: impl Into<Icon>) -> Self {
        self.title_icon = Some(icon.into());
        self
    }

    /// Which end of its run the entry sits on. [`SettingsStack`] sets this;
    /// call it directly only on a virtualized page.
    #[must_use]
    pub fn position(mut self, position: StackPosition) -> Self {
        self.position = position;
        self
    }

    /// Reset and provenance widgets, which sit beside the title rather than
    /// with the control.
    #[must_use]
    pub fn title_actions(mut self, actions: impl IntoElement) -> Self {
        self.title_actions = Some(actions.into_any_element());
        self
    }

    /// **Must be bounded.** The control keeps its natural width while the copy
    /// column shrinks, so one that grows with its content squeezes the copy to
    /// a character per line. Give it a width, or a menu trigger.
    #[must_use]
    pub fn control(mut self, control: impl IntoElement) -> Self {
        self.control = Some(control.into_any_element());
        self
    }

    /// Dim the row and make it inert, for a setting the current configuration
    /// has no effect on.
    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl ParentElement for SettingEntry {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for SettingEntry {
    fn render(self, _: &mut zz_gpui::Window, cx: &mut App) -> impl IntoElement {
        let disabled = self.disabled;
        let position = self.position;
        let coarse = crate::touch::CoarsePointer::get(cx);
        let heading = if coarse {
            let description = (!self.description.is_empty()).then(|| {
                div()
                    .text_size(crate::rems_from_px(11.0))
                    .text_color(cx.theme().foreground.muted())
                    .child(self.description.clone())
            });
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(16.0))
                        .child(
                            div()
                                .flex()
                                .flex_1()
                                .min_w_0()
                                .items_center()
                                .gap(px(4.0))
                                .when_some(self.title_icon, |this, icon| {
                                    this.child(icon.with_size(crate::Size::Medium))
                                })
                                .child(div().text_size(crate::rems_from_px(13.0)).child(self.title))
                                .when_some(self.title_actions, zz_gpui::ParentElement::child),
                        )
                        .when_some(self.control, zz_gpui::ParentElement::child),
                )
                .children(description)
        } else {
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(16.0))
                .child(
                    SettingCopy::new(self.title, self.description)
                        .when_some(self.title_icon, SettingCopy::title_icon)
                        .when_some(self.title_actions, SettingCopy::title_actions),
                )
                .when_some(self.control, zz_gpui::ParentElement::child)
        };

        let theme = cx.theme();
        let card = theme.layer(roles::SETTINGS_CARD, PaintState::Rest);
        let (inset, cast): (Vec<_>, Vec<_>) =
            card.shadow.iter().cloned().partition(|shadow| shadow.inset);
        let radius = card.radius;
        let ends = move |this: zz_gpui::Div| {
            this.when(position.rounds_top(), |this| this.rounded_t(radius))
                .when(position.rounds_bottom(), |this| this.rounded_b(radius))
        };

        let body = div()
            .relative()
            .flex()
            .flex_col()
            .w_full()
            .gap(px(10.0))
            .px(px(SETTINGS_STACK_PADDING))
            .py(px(11.0))
            .when(disabled, |this| {
                this.opacity(0.5)
                    .child(div().absolute().inset_0().occlude())
            })
            .child(heading)
            .children(self.children);

        let surface = div()
            .flex()
            .flex_col()
            .w_full()
            .layer(&Layer {
                shadow: inset,
                radius: px(0.0),
                ..card
            })
            .map(ends)
            .when(!position.rounds_top(), zz_gpui::Styled::border_t_0)
            .when(!position.rounds_bottom(), zz_gpui::Styled::border_b_0)
            .when(position.rules_above(), |this| {
                this.child(
                    div().flex_none().px(px(SETTINGS_STACK_PADDING)).child(
                        Separator::horizontal()
                            .surface_role(roles::LIST_RULE)
                            .h(RULE_WIDTH),
                    ),
                )
            })
            .child(body);

        div()
            .relative()
            .flex()
            .flex_col()
            .w_full()
            .when(!cast.is_empty(), |this| {
                let extent = px(8.0);
                this.child(
                    div()
                        .absolute()
                        .left(-extent)
                        .right(-extent)
                        .top(if position.rounds_top() {
                            -extent
                        } else {
                            px(0.0)
                        })
                        .bottom(if position.rounds_bottom() {
                            -extent
                        } else {
                            px(0.0)
                        })
                        .overflow_hidden()
                        .child(
                            div()
                                .absolute()
                                .left(extent)
                                .right(extent)
                                .top(if position.rounds_top() {
                                    extent
                                } else {
                                    -extent
                                })
                                .bottom(if position.rounds_bottom() {
                                    extent
                                } else {
                                    -extent
                                })
                                .map(ends)
                                .shadow(cast),
                        ),
                )
            })
            .child(surface)
    }
}

#[derive(IntoElement)]
pub struct SettingCopy {
    title: SharedString,
    description: SharedString,
    title_icon: Option<Icon>,
    title_actions: Option<AnyElement>,
}

impl SettingCopy {
    pub fn new(title: impl Into<SharedString>, description: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            description: description.into(),
            title_icon: None,
            title_actions: None,
        }
    }

    /// A state glyph drawn before the title, sized to the title's line. Color
    /// it before passing.
    #[must_use]
    pub fn title_icon(mut self, icon: impl Into<Icon>) -> Self {
        self.title_icon = Some(icon.into());
        self
    }

    #[must_use]
    pub fn title_actions(mut self, actions: impl IntoElement) -> Self {
        self.title_actions = Some(actions.into_any_element());
        self
    }
}

impl RenderOnce for SettingCopy {
    fn render(self, _: &mut zz_gpui::Window, cx: &mut App) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .w_full()
            .max_w(relative(0.7))
            .min_w_0()
            .gap(px(3.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .when_some(self.title_icon, |this, icon| {
                        this.child(icon.with_size(crate::Size::Medium))
                    })
                    .child(div().text_size(crate::rems_from_px(13.0)).child(self.title))
                    .when_some(self.title_actions, zz_gpui::ParentElement::child),
            )
            .when(!self.description.is_empty(), |this| {
                this.child(
                    div()
                        .text_size(crate::rems_from_px(11.0))
                        .text_color(cx.theme().foreground.muted())
                        .child(self.description),
                )
            })
    }
}

/// The ground of a settings card, for the controls mounted on it, so their
/// fills step up from the card instead of dissolving into it.
#[must_use]
pub fn settings_card_ground(cx: &App) -> Ground {
    cx.theme().ground_of(roles::SETTINGS_CARD)
}

/// A preview well in Settings, around `fill`, the color of what it shows:
/// [`roles::SETTINGS_PREVIEW`]'s edge, corners and shadow, clipping what it
/// holds.
#[must_use]
pub fn settings_preview(fill: Hsla, cx: &App) -> zz_gpui::Div {
    let layer = cx.theme().layer(roles::SETTINGS_PREVIEW, PaintState::Rest);
    div().overflow_hidden().layer(&Layer {
        fill,
        glass: None,
        ..layer
    })
}

pub fn settings_provenance_badge(label: impl Into<SharedString>) -> Badge {
    Badge::new(label).emphasis(Emphasis::Outline)
}

pub fn settings_reset_button(
    id: impl Into<zz_gpui::ElementId>,
    tooltip: impl Into<SharedString>,
    enabled: bool,
) -> Button {
    let slop = crate::touch::control_slop(crate::Size::Small);
    Button::new(id)
        .small()
        .compact()
        .ghost()
        .surface_role(crate::SurfaceRole::FLAT_ICON_BUTTON)
        .hit_slop(slop, slop)
        .icon(crate::IconName::Undo2)
        .tooltip(tooltip)
        .disabled(!enabled)
}

#[derive(Clone)]
pub struct SettingsSelectItem {
    title: SharedString,
    value: String,
    font: bool,
}

impl SettingsSelectItem {
    pub fn new(title: impl Into<SharedString>, value: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            value: value.into(),
            font: false,
        }
    }

    /// Draw the row in the font family its value names.
    #[must_use]
    pub fn preview_font(mut self) -> Self {
        self.font = true;
        self
    }
}

impl SelectItem for SettingsSelectItem {
    type Value = String;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }

    fn font_family(&self) -> Option<SharedString> {
        self.font.then(|| self.value.clone().into())
    }
}
