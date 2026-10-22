//! The home page: how sharing is going, the problems in its way, and the
//! arrangement with each computer's inspector under it. Until a computer is
//! paired, it shows how to add the first one instead.

use zflow::app::api::Level;
use zz_gpui::{
    AnyElement, Context, IntoElement, ParentElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use zz_gpui_kit::{
    ActiveTheme as _, Colorize as _, Sizable as _, StyledExt as _, button::Button, h_flex,
    rems_from_px, settings::settings_scroll_column, sidebar::status_dot, v_flex,
};

use super::{fix_button, inspector};
use crate::{copy, model::Snapshot, platform, widgets, window::ZflowWindow};

pub fn render(
    this: &mut ZflowWindow,
    window: &mut Window,
    cx: &mut Context<ZflowWindow>,
) -> AnyElement {
    let Some(snapshot) = this.model.read(cx).snapshot.clone() else {
        return div().into_any_element();
    };
    if snapshot.peers.is_empty() {
        return first_run(this, &snapshot, window, cx);
    }
    settings_scroll_column("computers")
        .child(status(&snapshot, cx))
        .children(problems(&snapshot, None, cx))
        .child(this.board.clone())
        .child(widgets::muted(hint(), 11.0, cx))
        .child(inspector::render(this, &snapshot, window, cx))
        .into_any_element()
}

fn hint() -> &'static str {
    "Drag a computer by any of its screens or its name; double-click it to move its screens one by one. The pointer crosses to another computer at the lit seams."
}

fn status(snapshot: &Snapshot, cx: &mut Context<ZflowWindow>) -> impl IntoElement {
    let status = &snapshot.status;
    v_flex()
        .gap(px(4.0))
        .child(
            h_flex()
                .gap(px(10.0))
                .child(status_dot(widgets::status_color(status.state, cx)))
                .child(
                    div()
                        .text_size(rems_from_px(20.0))
                        .font_medium()
                        .child(status.title.clone()),
                ),
        )
        .child(widgets::muted(
            copy::status_hint(status.state, status.peer.as_deref()),
            12.0,
            cx,
        ))
}

/// A banner for each check that is not fine, with its fix. `skip` leaves
/// out a row another card stands in for.
fn problems(
    snapshot: &Snapshot,
    skip: Option<&str>,
    cx: &mut Context<ZflowWindow>,
) -> Vec<AnyElement> {
    snapshot
        .health
        .iter()
        .filter(|row| row.level != Level::Ok && Some(row.id.as_str()) != skip)
        .map(|row| {
            let action = platform::health_action(row, cx).map(|action| {
                fix_button(&row.id, &action, widgets::banner_ground(row.level, cx), cx)
            });
            widgets::banner(row.level, row.title.clone(), row.detail.clone(), action, cx)
                .into_any_element()
        })
        .collect()
}

/// The first computer, while nothing is paired.
fn first_run(
    this: &mut ZflowWindow,
    snapshot: &Snapshot,
    window: &mut Window,
    cx: &mut Context<ZflowWindow>,
) -> AnyElement {
    let other = snapshot
        .unplaced
        .iter()
        .find(|computer| copy::placeable(computer))
        .map(|computer| computer.name.clone());
    let note = copy::finish_note(&snapshot.pairing_window, other.as_deref(), &host_name());
    let copied = this.copied;
    settings_scroll_column("first-run")
        .children(platform::first_run_cards(this, snapshot, window, cx))
        // The cards above stand in for the sharing row.
        .children(problems(snapshot, Some("sharing"), cx))
        .child(super::heading(
            "Add your other computers",
            "Computers running zflow on this network show up here by themselves.",
            cx,
        ))
        .child(this.board.clone())
        .when_some(note, |column, (title, detail)| {
            column.child(widgets::note(title, detail, cx))
        })
        .child(
            h_flex()
                .w_full()
                .gap(px(10.0))
                .pl(px(12.0))
                .pr(px(6.0))
                .py(px(6.0))
                .surface(
                    zz_gpui_kit::Surface::new(zz_gpui_kit::roles::SETTINGS_PREVIEW),
                    cx,
                )
                .child(widgets::muted("Not listed? Run on it:", 12.0, cx).flex_none())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(rems_from_px(12.0))
                        .child(copy::INSTALL_COMMAND),
                )
                .child(
                    Button::new("copy-install")
                        .small()
                        .on(cx.theme().ground_of(zz_gpui_kit::roles::SETTINGS_PREVIEW))
                        .label(if copied { "Copied" } else { "Copy" })
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.write_to_clipboard(zz_gpui::ClipboardItem::new_string(
                                copy::INSTALL_COMMAND.to_owned(),
                            ));
                            this.copied = true;
                            cx.notify();
                        })),
                ),
        )
        .into_any_element()
}

/// The name other computers know this one by, as its hellos give it.
pub fn host_name() -> String {
    let name = zflow::hello::local_name();
    if name.is_empty() {
        copy::THIS_COMPUTER_LOWER.to_owned()
    } else {
        name
    }
}

#[allow(dead_code)]
fn muted_ink(cx: &zz_gpui::App) -> zz_gpui::Hsla {
    cx.theme().foreground.muted()
}
