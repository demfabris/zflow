//! The one window: a sidebar with the pages, and the page beside it.

use zflow::app::api::Request;
use zz_gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Window, WindowControlArea, div, prelude::FluentBuilder as _, px,
};
use zz_gpui_kit::{
    ActiveTheme as _, Disableable as _, IconName, Root, WindowControls, WindowExt as _, h_flex,
    input::InputState,
    settings::{settings_navigation_button, settings_navigation_row},
    shell::{app_shell_surface, app_titlebar_strip, window_drag_handle},
    sidebar::{chrome_button, sidebar_footer, sidebar_panel, sidebar_titlebar},
    switch::Switch,
    v_flex,
};

use crate::{
    board::Board,
    model::{Model, Page},
    pages, platform, widgets,
};

const SIDEBAR_WIDTH: f32 = 220.0;

pub struct ZflowWindow {
    pub model: Entity<Model>,
    pub services: Entity<platform::Services>,
    pub board: Entity<Board>,
    /// What the Add by Address dialog holds.
    pub address: Entity<InputState>,
    pub address_error: Option<String>,
    pub looking_up: bool,
    /// The install command was copied.
    pub copied: bool,
    focus: FocusHandle,
    _observers: Vec<Subscription>,
}

impl ZflowWindow {
    pub fn new(
        model: Entity<Model>,
        services: Entity<platform::Services>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let board = cx.new(|cx| Board::new(model.clone(), cx));
        let address = cx.new(|cx| InputState::new(window, cx).placeholder("100.64.0.7"));
        let observers = vec![
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.observe(&services, |_, _, cx| cx.notify()),
            cx.observe(&board, |_, _, cx| cx.notify()),
            cx.observe_window_appearance(window, |_, window, cx| {
                crate::theme::apply(Some(window), cx);
            }),
            // A request that failed shows as a toast.
            cx.observe_in(&model, window, |_, model, window, cx| {
                if let Some(failure) = model.update(cx, |model, _| model.take_failure()) {
                    window.push_notification(
                        zz_gpui_kit::notification::Notification::warning(failure)
                            .title("That didn’t work"),
                        cx,
                    );
                }
            }),
        ];
        Self {
            model,
            services,
            board,
            address,
            address_error: None,
            looking_up: false,
            copied: false,
            focus: cx.focus_handle(),
            _observers: observers,
        }
    }

    pub fn send(&self, request: Request, cx: &App) {
        self.model.read(cx).send(request);
    }

    /// Opens the dialog that says hello to a computer mDNS cannot see.
    pub fn open_add_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.address_error = None;
        self.address
            .update(cx, |input, cx| input.set_value("", window, cx));
        let this = cx.entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            pages::add_address(&this, dialog, cx)
        });
    }

    fn sidebar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let page = self.model.read(cx).page.clone();
        let row = |id: &'static str,
                   title: &'static str,
                   icon: IconName,
                   target: Page,
                   cx: &mut Context<Self>| {
            let selected = page == target;
            settings_navigation_row(
                settings_navigation_button(id, title, icon, selected, cx).on_click(cx.listener(
                    move |this, _, _, cx| {
                        let target = target.clone();
                        this.model.update(cx, |model, cx| model.show(target, cx));
                    },
                )),
            )
        };
        let navigation = v_flex()
            .px(cx.theme().row_inset)
            .child(row(
                "nav-computers",
                "Computers",
                IconName::LayoutDashboard,
                Page::Computers,
                cx,
            ))
            .child(row(
                "nav-settings",
                "Settings",
                IconName::Settings,
                Page::Settings,
                cx,
            ))
            .child(div().flex_1())
            .child(sidebar_footer(
                "sidebar-footer",
                widgets::muted(format!("zflow {}", env!("CARGO_PKG_VERSION")), 11.0, cx),
                None,
                cx,
            ));
        let titlebar = window_drag_handle(
            "sidebar-drag",
            sidebar_titlebar("sidebar-title", div(), cx)
                .window_control_area(WindowControlArea::Drag),
            window,
            cx,
        );
        sidebar_panel(
            "sidebar",
            SIDEBAR_WIDTH,
            titlebar,
            navigation.size_full(),
            cx.theme().ground(),
            true,
            cx,
        )
        .into_any_element()
    }

    fn titlebar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let model = self.model.read(cx);
        let tools = (model.page == Page::Computers)
            .then_some(model.snapshot.as_ref())
            .flatten()
            .filter(|snapshot| !snapshot.peers.is_empty())
            .map(|snapshot| snapshot.sharing);
        let leading = h_flex()
            .flex_1()
            .justify_end()
            .gap(px(8.0))
            .pr(px(8.0))
            .when_some(tools, |this, sharing| {
                this.child(widgets::muted("Sharing", 12.0, cx)).child(
                    Switch::new("titlebar-sharing")
                        .checked(sharing == Some(true))
                        .disabled(sharing.is_none())
                        .on_click(cx.listener(|this, enabled: &bool, _, cx| {
                            this.send(Request::SetSharing { enabled: *enabled }, cx);
                        })),
                )
            })
            .when(model.page == Page::Computers, |this| {
                this.child(
                    chrome_button("add-address", IconName::Plus, "Add a computer by address")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_add_address(window, cx)),
                        ),
                )
            });
        // Empty where the system draws them. The drawn close button asks the
        // platform too, as the system's does.
        let controls = WindowControls::new().on_close_window(|_, window, cx| {
            if platform::may_close(cx) {
                window.remove_window();
            }
        });
        window_drag_handle(
            "titlebar-drag",
            app_titlebar_strip("titlebar", leading, controls, cx),
            window,
            cx,
        )
        .into_any_element()
    }
}

impl Focusable for ZflowWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ZflowWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar = self.sidebar(window, cx);
        let titlebar = self.titlebar(window, cx);
        let content = pages::page(self, window, cx);
        let dialogs = Root::render_dialog_layer(window, cx).map(IntoElement::into_any_element);
        let notifications =
            Root::render_notification_layer(window, cx).map(IntoElement::into_any_element);
        app_shell_surface(
            SharedString::from("zflow"),
            cx.theme().background,
            sidebar,
            Some(titlebar),
            content,
            dialogs.into_iter().chain(notifications),
        )
        .track_focus(&self.focus)
    }
}
