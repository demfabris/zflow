//! Under the arrangement: every computer, or the one selected with where the
//! pointer crosses from it, its screens and its settings.

use zflow::{
    app::{
        api::{Peer, Request},
        layout_model::Seam,
    },
    core::KeyboardMode,
};
use zz_gpui::{
    AnyElement, Context, IntoElement, ParentElement as _, SharedString, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use zz_gpui_kit::{
    Disableable as _, Icon, IconName, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::DialogButtonProps,
    h_flex,
    menu::{DropdownMenu as _, PopupMenuItem},
    settings::{SettingEntry, SettingsStack, settings_card_ground},
    switch::Switch,
    v_flex,
};

use crate::{
    board::{self, Computer, LOCAL},
    copy,
    model::Snapshot,
    platform, widgets,
    window::ZflowWindow,
};

pub fn render(
    this: &mut ZflowWindow,
    snapshot: &Snapshot,
    window: &mut Window,
    cx: &mut Context<ZflowWindow>,
) -> AnyElement {
    let computers = board::computers(snapshot);
    let selection = this.board.read(cx).selection.clone();
    match computers
        .iter()
        .find(|computer| Some(&computer.owner) == selection.as_ref())
    {
        Some(computer) => computer_page(this, snapshot, computer, window, cx),
        None => v_flex()
            .gap(px(20.0))
            .child(overview(this, &computers, cx))
            .children(found(this, snapshot, cx))
            .into_any_element(),
    }
}

fn summary(computer: &Computer) -> String {
    let screens = match computer.screens.len() {
        0 => "Not on the arrangement".to_owned(),
        1 => "1 screen".to_owned(),
        count => format!("{count} screens"),
    };
    match &computer.peer {
        Some(peer) => format!("{screens} · {}", copy::peer_state(peer.state)),
        None => screens,
    }
}

fn overview(
    this: &ZflowWindow,
    computers: &[Computer],
    cx: &mut Context<ZflowWindow>,
) -> AnyElement {
    let board = this.board.clone();
    let entries = computers.iter().map(|computer| {
        let owner = computer.owner.clone();
        let board = board.clone();
        SettingEntry::new(computer.name.clone(), summary(computer))
            .title_icon(Icon::new(if computer.owner == LOCAL {
                IconName::Laptop
            } else {
                IconName::Server
            }))
            .control(
                h_flex()
                    .gap(px(8.0))
                    .when_some(computer.peer.as_ref(), |this, peer| {
                        this.child(widgets::state_dot(Some(peer.state), cx))
                    })
                    .children(computer.mark.as_deref().map(widgets::key_mark))
                    .child(
                        Button::compact_icon(
                            SharedString::from(format!("open-{owner}")),
                            IconName::ChevronRight,
                        )
                        .on(settings_card_ground(cx))
                        .tooltip("Show its crossings, screens and settings")
                        .on_click(move |_, window, cx| {
                            board.update(cx, |board, cx| board.select(Some(owner.clone()), cx));
                            window.refresh();
                        }),
                    ),
            )
    });
    SettingsStack::titled("Your computers")
        .description(
            "Select a computer on the arrangement or here to see its crossings, screens and settings.",
        )
        .children(entries)
        .into_any_element()
}

/// The computers found on the network, each with a button that adds it
/// beside this one: the arrangement's drag, for the keyboard and screen
/// readers.
fn found(
    this: &ZflowWindow,
    snapshot: &Snapshot,
    cx: &mut Context<ZflowWindow>,
) -> Option<AnyElement> {
    if snapshot.unplaced.is_empty() {
        return None;
    }
    let entries = snapshot.unplaced.iter().map(|computer| {
        let board = this.board.clone();
        let id = computer.id.clone();
        SettingEntry::new(computer.name.clone(), copy::unplaced_detail(computer)).control(
            h_flex()
                .gap(px(8.0))
                .children(computer.mark.as_deref().map(widgets::key_mark))
                .child(
                    Button::new(SharedString::from(format!("add-{id}")))
                        .label("Add")
                        .on(settings_card_ground(cx))
                        .disabled(!copy::placeable(computer))
                        .tooltip(format!("Put it beside {}", copy::THIS_COMPUTER_LOWER))
                        .on_click(move |_, _, cx| board.read(cx).place_beside(&id, cx)),
                ),
        )
    });
    Some(
        SettingsStack::titled("Found on this network")
            .description("Drag one onto the arrangement, or add it beside this computer.")
            .children(entries)
            .into_any_element(),
    )
}

fn computer_page(
    this: &mut ZflowWindow,
    snapshot: &Snapshot,
    computer: &Computer,
    window: &mut Window,
    cx: &mut Context<ZflowWindow>,
) -> AnyElement {
    let board = this.board.clone();
    let adjusting = board.read(cx).adjusting.as_deref() == Some(computer.owner.as_str());
    let local = computer.owner == LOCAL;
    let header = {
        let close = board.clone();
        h_flex()
            .w_full()
            .justify_between()
            .child(super::heading(
                computer.name.clone(),
                computer
                    .peer
                    .as_ref()
                    .map_or_else(|| "This computer".to_owned(), |peer| peer.detail.clone()),
                cx,
            ))
            .child(
                h_flex()
                    .gap(px(8.0))
                    .children(computer.mark.as_deref().map(widgets::key_mark))
                    .child(
                        Button::compact_icon("close-computer", IconName::Xmark)
                            .tooltip("Show all computers")
                            .on_click(move |_, _, cx| {
                                close.update(cx, |board, cx| board.select(None, cx));
                            }),
                    ),
            )
    };

    let mut arrange = Vec::new();
    if computer.screens.len() > 1 {
        let owner = computer.owner.clone();
        let adjust = board.clone();
        arrange.push(
            SettingEntry::new(
                "Adjust screens",
                "Move its screens one by one instead of together. Double-clicking it on the arrangement does too.",
            )
            .control(Switch::new("adjust").checked(adjusting).on_click(move |on: &bool, _, cx| {
                adjust.update(cx, |board, cx| board.adjust(&owner, *on, cx));
            })),
        );
    }
    if local {
        arrange.extend(platform::display_settings_entry(cx));
    }

    let mine: Vec<&str> = computer
        .screens
        .iter()
        .map(|monitor| monitor.id.as_str())
        .collect();
    let crossings = board::crossings(snapshot, computer);
    let crossing_entries: Vec<SettingEntry> = if crossings.is_empty() {
        vec![SettingEntry::new(
            "None yet",
            "Move it until an edge touches another computer’s screen.",
        )]
    } else {
        crossings
            .iter()
            .map(|seam| crossing_entry(snapshot, seam, &mine))
            .collect()
    };
    let mut crossings_stack = SettingsStack::titled("Crossings").children(crossing_entries);
    if local && computer.screens.len() > 1 {
        crossings_stack = crossings_stack.description(platform::OWN_SCREENS_NOTE);
    }
    let screens =
        SettingsStack::titled("Screens").children(computer.screens.iter().map(|monitor| {
            let detail = monitor
                .display
                .as_ref()
                .map_or_else(String::new, |display| {
                    copy::screen_size(
                        display.width_mm,
                        display.height_mm,
                        display.bounds.width,
                        display.bounds.height,
                    )
                });
            SettingEntry::new(
                monitor
                    .display
                    .as_ref()
                    .map_or_else(|| monitor.label.clone(), |display| display.name.clone()),
                detail,
            )
        }));

    v_flex()
        .w_full()
        .gap(px(18.0))
        .child(header)
        .when(!arrange.is_empty(), |this| {
            this.child(SettingsStack::new().children(arrange))
        })
        .when(!computer.screens.is_empty(), |this| {
            this.child(crossings_stack).child(screens)
        })
        .when_some(computer.peer.as_ref(), |column, peer| {
            column.child(peer_settings(this, peer, window, cx))
        })
        .into_any_element()
}

fn crossing_entry(snapshot: &Snapshot, seam: &Seam, mine: &[&str]) -> SettingEntry {
    let here_is_a = mine.contains(&seam.a.as_str());
    let (here, there) = if here_is_a {
        (&seam.a, &seam.b)
    } else {
        (&seam.b, &seam.a)
    };
    let screen = |id: &str| {
        board::monitors(snapshot)
            .find(|monitor| monitor.id == id)
            .map_or_else(
                || "a screen".to_owned(),
                |monitor| {
                    monitor
                        .display
                        .as_ref()
                        .map_or_else(|| monitor.label.clone(), |display| display.name.clone())
                },
            )
    };
    let other = board::monitors(snapshot)
        .find(|monitor| &monitor.id == there)
        .map_or_else(
            || there.clone(),
            |monitor| {
                monitor
                    .peer
                    .clone()
                    .unwrap_or_else(|| copy::THIS_COMPUTER.to_owned())
            },
        );
    SettingEntry::new(
        other,
        format!(
            "{} edge of {} → {}",
            board::geometry::crossing_side(seam, here_is_a),
            screen(here),
            screen(there)
        ),
    )
    .title_icon(Icon::new(if seam.vertical {
        IconName::ArrowRight
    } else {
        IconName::ArrowDown
    }))
}

fn peer_settings(
    this: &mut ZflowWindow,
    peer: &Peer,
    window: &mut Window,
    cx: &mut Context<ZflowWindow>,
) -> AnyElement {
    let _ = window;
    let name = peer.name.clone();
    let set = |name: &str| {
        let name = name.to_owned();
        move |allow_control: Option<bool>,
              keyboard: Option<KeyboardMode>,
              reverse_scroll: Option<bool>,
              share_screen: Option<bool>| Request::SetPeer {
            name: name.clone(),
            allow_control,
            keyboard,
            reverse_scroll,
            share_screen,
        }
    };
    let model = this.model.clone();
    let control = {
        let (model, set) = (model.clone(), set(&name));
        SettingEntry::new(
            format!("{name} can control {}", copy::THIS_COMPUTER_LOWER),
            format!(
                "Push the pointer past {name}’s edge to move onto {}. Take it back with {}.",
                copy::THIS_COMPUTER_LOWER,
                copy::RETURN_KEYS
            ),
        )
        .control(
            Switch::new("allow-control")
                .checked(peer.allow_control)
                .on_click(move |on: &bool, _, cx| {
                    model.read(cx).send(set(Some(*on), None, None, None));
                }),
        )
    };
    let keyboard = {
        let (model, set) = (model.clone(), set(&name));
        let current = peer.keyboard;
        SettingEntry::new(
            format!("Keys from {name}"),
            format!(
                "{} Changes apply the next time {name} takes control.",
                copy::keyboard_detail(current)
            ),
        )
        .control(
            Button::new("keyboard-mode")
                .on(settings_card_ground(cx))
                .label(copy::keyboard_title(current))
                .dropdown_caret(true)
                .dropdown_menu(move |menu, _, _| {
                    copy::KEYBOARD_MODES.iter().fold(menu, |menu, &mode| {
                        let (model, set) = (model.clone(), set.clone());
                        menu.item(
                            PopupMenuItem::new(copy::keyboard_title(mode))
                                .checked(mode == current)
                                .on_click(move |_, _, cx| {
                                    model.read(cx).send(set(None, Some(mode), None, None));
                                }),
                        )
                    })
                }),
        )
    };
    let scroll = {
        let (model, set) = (model.clone(), set(&name));
        SettingEntry::new("Reverse scrolling", "Turn its scrolling around here.").control(
            Switch::new("reverse-scroll")
                .checked(peer.reverse_scroll)
                .on_click(move |on: &bool, _, cx| {
                    model.read(cx).send(set(None, None, Some(*on), None));
                }),
        )
    };
    let mut entries = Vec::new();
    if platform::CAN_REFUSE_CONTROL {
        entries.push(control);
    }
    entries.extend([keyboard, scroll]);
    if platform::CAN_SHARE_SCREEN {
        let (model, set) = (model.clone(), set(&name));
        entries.push(
            SettingEntry::new("Can see this screen", "Through a lens around its pointer.").control(
                Switch::new("share-screen")
                    .checked(peer.share_screen)
                    .on_click(move |on: &bool, _, cx| {
                        model.read(cx).send(set(None, None, None, Some(*on)));
                    }),
            ),
        );
    }
    entries.extend(platform::peer_entries(this, peer, cx));
    let forget = {
        let name = name.clone();
        let model = model.clone();
        SettingEntry::new(
            format!("Forget {name}"),
            "Stops input between the two computers and removes its trusted identity. It goes back to Found on this network.",
        )
        .control(
            super::card_danger("forget", "Forget…", cx).on_click(move |_, window, cx| {
                let (name, model) = (name.clone(), model.clone());
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let (name, model) = (name.clone(), model.clone());
                    alert
                        .title(format!("Forget {name}?"))
                        .description("This stops input between the two computers and removes its trusted identity. It goes back to Found on this network.")
                        .button_props(
                            DialogButtonProps::default()
                                .ok_text("Forget")
                                .ok_variant(zz_gpui_kit::button::ButtonVariant::Paint(zz_gpui_kit::Tone::Danger, zz_gpui_kit::Emphasis::Solid))
                                .show_cancel(true),
                        )
                        .on_ok(move |_, _, cx| {
                            model.read(cx).send(Request::Forget { name: name.clone() });
                            true
                        })
                });
            }),
        )
    };
    let _ = Button::new("unused").ghost();
    div()
        .w_full()
        .child(
            v_flex()
                .gap(px(18.0))
                .child(SettingsStack::titled("Settings").children(entries))
                .child(SettingsStack::new().child(forget)),
        )
        .into_any_element()
}
