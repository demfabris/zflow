//! macOS: the engine runs in this process, zflow lives in the menu bar, and
//! the app keeps what only it can: the login item, the Wi-Fi helper,
//! Sparkle's updates and the permissions macOS asks about.
#![allow(
    unsafe_code,
    reason = "ServiceManagement and AppKit calls the objc2 bindings mark unsafe"
)]

mod sparkle;

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use objc2::{MainThreadMarker, rc::Retained, runtime::ProtocolObject};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSPasteboard, NSPasteboardAccessBehavior,
    NSPasteboardTypeString,
};
use objc2_foundation::{NSActivityOptions, NSBundle, NSObjectProtocol, NSProcessInfo, NSString};
use objc2_service_management::{SMAppService, SMAppServiceStatus};
use zflow::app::api::{Action, Health, Peer, Request};
use zz_gpui::{
    AnyElement, App, AppContext as _, Context, Entity, IntoElement, KeyBinding, Menu, MenuItem,
    ParentElement as _, Styled as _, SystemNotification, SystemNotificationResponse, Task, Window,
    actions, div, px,
};
use zz_gpui_kit::{
    Disableable as _, Icon, IconName,
    button::{Button, ButtonVariants as _},
    settings::{SettingEntry, SettingsStack, settings_card_ground},
    switch::Switch,
};

use super::{LocalNetwork, Updates};
use crate::{
    copy,
    model::{Model, Page, Snapshot},
    widgets,
    window::ZflowWindow,
};

/// A Mac can look at another computer's screen, not be looked at.
pub const CAN_SHARE_SCREEN: bool = false;
/// Each computer's control of this Mac can be turned off.
pub const CAN_REFUSE_CONTROL: bool = true;

pub const OWN_SCREENS_NOTE: &str =
    "Between this Mac’s own screens, the pointer follows System Settings › Displays.";

/// zflow lives in the menu bar; closing the window keeps it running.
pub const LIVES_IN_TRAY: bool = true;

const UPDATE_TAG: &str = "update";
/// How often the login item and the helper are checked.
const SERVICES_TICK: Duration = Duration::from_secs(10);
/// How long Local Network may read blocked before the window says so: it
/// reads blocked until the person answers macOS.
const NETWORK_GRACE: Duration = Duration::from_secs(5);
/// How long quitting waits for the engine to stop.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

actions!(
    zflow,
    [
        OpenSettings,
        OpenConfiguration,
        CheckForUpdates,
        Quit,
        HideZflow,
        About,
        CloseWindow
    ]
);

/// Where zflow runs from. Gatekeeper runs a quarantined app from a temporary
/// read-only copy until Finder moves it, and a disk image goes away when it
/// is ejected, so both need a move to Applications first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppLocation {
    pub path: String,
    pub quarantined: bool,
    pub read_only: bool,
    pub home: String,
}

impl AppLocation {
    /// Where this process runs from, which can't change while it runs.
    pub fn main() -> Self {
        let path = NSBundle::mainBundle().bundlePath().to_string();
        let c_path = std::ffi::CString::new(path.clone()).unwrap_or_default();
        // SAFETY: a NUL-terminated path and a size-only query.
        let quarantined = unsafe {
            libc::getxattr(
                c_path.as_ptr(),
                c"com.apple.quarantine".as_ptr(),
                std::ptr::null_mut(),
                0,
                0,
                0,
            ) >= 0
        };
        // SAFETY: `statfs` fills the zeroed struct for a NUL-terminated path.
        let read_only = unsafe {
            let mut stats: libc::statfs = std::mem::zeroed();
            libc::statfs(c_path.as_ptr(), &raw mut stats) == 0
                && stats.f_flags & libc::MNT_RDONLY as u32 != 0
        };
        Self {
            path,
            quarantined,
            read_only,
            home: std::env::var("HOME").unwrap_or_default(),
        }
    }

    /// Local builds under target/ carry no quarantine flag, so they stay put.
    pub fn needs_move(&self) -> bool {
        if self.path.contains("/AppTranslocation/") {
            return true;
        }
        if self.path.starts_with("/Volumes/") && self.read_only {
            return true;
        }
        let folders = [
            "/Applications/".to_owned(),
            format!("{}/Applications/", self.home),
        ];
        self.quarantined
            && !folders
                .iter()
                .any(|folder| self.path.starts_with(folder.as_str()))
    }
}

/// How macOS treats zflow reading what other apps copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteAccess {
    /// macOS has not asked yet.
    Default,
    Ask,
    AlwaysAllow,
    AlwaysDeny,
}

fn paste_access() -> PasteAccess {
    let behavior = NSPasteboard::generalPasteboard().accessBehavior();
    if behavior == NSPasteboardAccessBehavior::AlwaysAllow {
        PasteAccess::AlwaysAllow
    } else if behavior == NSPasteboardAccessBehavior::AlwaysDeny {
        PasteAccess::AlwaysDeny
    } else if behavior == NSPasteboardAccessBehavior::Ask {
        PasteAccess::Ask
    } else {
        PasteAccess::Default
    }
}

/// What the Reduce Wi-Fi lag row says about its helper.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Helper {
    pub ready: bool,
    pub note: String,
    /// The row's fix, when there is one.
    pub action: Option<&'static str>,
    pub busy: bool,
}

pub struct Services {
    pub updates: Updates,
    sparkle: Option<sparkle::Sparkle>,
    pub login: bool,
    pub helper: Helper,
    /// Only a build signed by a team can install the helper.
    pub has_team: bool,
    pub paste: PasteAccess,
    pub location: AppLocation,
    /// Since when Local Network has read blocked.
    network_blocked: Option<Instant>,
    /// The first-run page asked to look for computers.
    discovered: bool,
    /// The Mac stays out of App Nap while it shares input.
    activity: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    watching_accessibility: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
}

fn login_status() -> SMAppServiceStatus {
    // SAFETY: a class method with no arguments.
    unsafe { SMAppService::mainAppService().status() }
}

fn helper_service() -> Retained<SMAppService> {
    // SAFETY: a class method taking the plist's name.
    unsafe { SMAppService::daemonServiceWithPlistName(&NSString::from_str("io.zflow.awdl.plist")) }
}

fn open_login_items() {
    // SAFETY: a class method with no arguments.
    unsafe { SMAppService::openSystemSettingsLoginItems() };
}

impl Services {
    fn new(cx: &mut Context<Self>) -> Self {
        let location = AppLocation::main();
        let (events, inbox) = async_channel::unbounded();
        let (sparkle, unavailable) = match sparkle::configuration_issue(&location) {
            Some(reason) => (None, Some(reason)),
            None => match MainThreadMarker::new()
                .ok_or_else(|| "Sparkle needs the main thread".to_owned())
                .and_then(|mtm| sparkle::Sparkle::start(events, mtm))
            {
                Ok(sparkle) => (Some(sparkle), None),
                Err(reason) => (None, Some(reason)),
            },
        };
        let sparkle_events = cx.spawn(async move |this, cx| {
            while let Ok(event) = inbox.recv().await {
                let Ok(()) = this.update(cx, |services, cx| {
                    match event {
                        sparkle::Event::Found { version, shown } => {
                            // A menu bar icon is easy to miss, so a
                            // notification goes with a background find.
                            if !shown {
                                cx.show_system_notification(SystemNotification {
                                    tag: UPDATE_TAG.into(),
                                    title: format!("zflow {version} is available").into(),
                                    body: "Click to install it.".into(),
                                    actions: Vec::new(),
                                });
                            }
                            services.updates.available = Some(version);
                        }
                        sparkle::Event::Finished => {
                            services.updates.available = None;
                            cx.dismiss_system_notification(UPDATE_TAG);
                        }
                    }
                    cx.notify();
                }) else {
                    return;
                };
            }
        });
        let has_team = zflow::macos::signed_by_team();
        let mut services = Self {
            updates: Updates {
                unavailable,
                ..Updates::default()
            },
            sparkle,
            login: login_status() == SMAppServiceStatus::Enabled,
            helper: Helper {
                note: "Checking the Wi-Fi helper…".into(),
                ..Helper::default()
            },
            has_team,
            paste: paste_access(),
            location,
            network_blocked: None,
            discovered: false,
            activity: None,
            watching_accessibility: None,
            _tasks: vec![sparkle_events],
        };
        services.refresh_updates();
        services
    }

    fn refresh_updates(&mut self) {
        if let Some(sparkle) = &self.sparkle {
            self.updates.checking = !sparkle.can_check();
            self.updates.automatic = Some(sparkle.checks_automatically());
            self.updates.install_automatically = Some(sparkle.downloads_automatically());
        }
    }

    pub fn check_updates(&mut self) {
        if let Some(sparkle) = &self.sparkle
            && sparkle.can_check()
        {
            sparkle.check();
        }
    }

    pub fn set_login(&mut self, on: bool, cx: &mut Context<Self>) {
        let service = unsafe { SMAppService::mainAppService() };
        // SAFETY: registering or unregistering this app's own login item.
        let result = unsafe {
            if on && service.status() != SMAppServiceStatus::RequiresApproval {
                service.registerAndReturnError()
            } else if !on {
                service.unregisterAndReturnError()
            } else {
                Ok(())
            }
        };
        let status = unsafe { service.status() };
        if on && status == SMAppServiceStatus::RequiresApproval {
            open_login_items();
        } else if let Err(error) = result {
            let message = format!(
                "zflow could not change its login item: {}",
                error.localizedDescription()
            );
            crate::model::model(cx).update(cx, |model, cx| model.report(message, cx));
        }
        self.login = status == SMAppServiceStatus::Enabled;
        cx.notify();
    }

    /// Reads the helper's state; `ready` comes from a check off the UI
    /// thread.
    fn read_helper(&mut self, answered: Option<bool>) {
        if !self.has_team {
            self.helper = Helper {
                note: "Needs a signed build of zflow. Sharing works without it.".into(),
                ..Helper::default()
            };
            return;
        }
        let status = unsafe { helper_service().status() };
        self.helper = match status {
            SMAppServiceStatus::NotRegistered | SMAppServiceStatus::NotFound => Helper {
                note: "Needs a helper. macOS asks you to allow it.".into(),
                action: Some("Install…"),
                ..Helper::default()
            },
            SMAppServiceStatus::RequiresApproval => Helper {
                note: "Allow zflow in System Settings › General › Login Items & Extensions.".into(),
                action: Some("Allow…"),
                ..Helper::default()
            },
            SMAppServiceStatus::Enabled => {
                let ready = answered.unwrap_or(self.helper.ready);
                Helper {
                    ready,
                    note: if ready {
                        String::new()
                    } else {
                        "The helper is installed but did not answer.".into()
                    },
                    action: (!ready).then_some("Repair…"),
                    busy: false,
                }
            }
            _ => Helper {
                note: "macOS returned an unknown status for the helper.".into(),
                ..Helper::default()
            },
        };
    }

    /// Installs the helper, or sends the person to approve it.
    pub fn fix_helper(&mut self, model: Entity<Model>, cx: &mut Context<Self>) {
        if self.helper.busy || !self.has_team {
            return;
        }
        self.helper.busy = true;
        cx.notify();
        let service = helper_service();
        let status = unsafe { service.status() };
        if status == SMAppServiceStatus::RequiresApproval {
            open_login_items();
        } else if status == SMAppServiceStatus::Enabled && self.helper.ready {
            // Unregistering disables the item, and macOS refuses an
            // immediate register, so a working helper is left alone.
        } else {
            // SAFETY: (re)registering this app's own launch daemon.
            let result = unsafe {
                if status == SMAppServiceStatus::Enabled {
                    let _ = service.unregisterAndReturnError();
                }
                service.registerAndReturnError()
            };
            if unsafe { service.status() } == SMAppServiceStatus::RequiresApproval {
                open_login_items();
            } else if let Err(error) = result {
                let message = error.localizedDescription().to_string();
                model.update(cx, |model, cx| model.report(message, cx));
            }
        }
        self.check_helper(model, cx);
    }

    fn check_helper(&mut self, model: Entity<Model>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let ready = cx
                .background_executor()
                .spawn(async { zflow::macos::helper_ready() })
                .await;
            let _ = this.update(cx, |services, cx| {
                services.read_helper(Some(ready));
                cx.notify();
            });
            model.read_with(cx, |model, _| model.send(Request::HelperReady { ready }));
        })
        .detach();
    }

    /// The services' slow checks: the login item always, the helper only
    /// while Reduce Wi-Fi lag is on, since checking it means a call to it.
    fn tick(&mut self, model: &Entity<Model>, cx: &mut Context<Self>) {
        self.login = login_status() == SMAppServiceStatus::Enabled;
        self.refresh_updates();
        let block_awdl = model.read(cx).platform().block_awdl == Some(true);
        if block_awdl {
            self.check_helper(model.clone(), cx);
        } else {
            self.read_helper(None);
        }
        cx.notify();
    }

    /// Follows the snapshot: App Nap while sharing, paste access, and how
    /// long Local Network has read blocked.
    fn observe(&mut self, snapshot: &Snapshot, cx: &mut Context<Self>) {
        // The engine polls input on 1 to 12 ms timers. App Nap and timer
        // coalescing would stutter the pointer and delay crossings.
        let sharing = snapshot.sharing == Some(true);
        let info = NSProcessInfo::processInfo();
        match (sharing, self.activity.take()) {
            (true, None) => {
                self.activity = Some(info.beginActivityWithOptions_reason(
                    NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
                        | NSActivityOptions::LatencyCritical,
                    &NSString::from_str("Sharing input with another computer"),
                ));
            }
            (false, Some(activity)) => unsafe { info.endActivity(&activity) },
            (_, kept) => self.activity = kept,
        }
        let paste = paste_access();
        let blocked = snapshot
            .platform
            .as_ref()
            .and_then(|platform| platform.local_network)
            == Some(LocalNetwork::Blocked);
        let network_blocked = match (blocked, self.network_blocked) {
            (true, None) => Some(Instant::now()),
            (true, since) => since,
            (false, _) => None,
        };
        if paste != self.paste || network_blocked != self.network_blocked {
            self.paste = paste;
            self.network_blocked = network_blocked;
            cx.notify();
        }
    }

    fn network_blocked(&self) -> bool {
        self.network_blocked
            .is_some_and(|since| since.elapsed() >= NETWORK_GRACE)
    }
}

pub fn init(cx: &mut App) -> Entity<Services> {
    let services = cx.new(Services::new);
    cx.set_global(super::SharedServices(services.clone()));
    services
}

pub fn before_start(arguments: &crate::Arguments) -> bool {
    !arguments.quit
}

pub fn default_config() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("Library/Application Support/zflow/zflow.toml")
}

fn set_policy(regular: bool) {
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm).setActivationPolicy(if regular {
            NSApplicationActivationPolicy::Regular
        } else {
            NSApplicationActivationPolicy::Accessory
        });
    }
}

/// The app gets its Dock icon while the window is open.
pub fn window_shown(_: &mut App) {
    set_policy(true);
}

/// The app stays in the menu bar, so the window always closes.
pub fn may_close(_: &App) -> bool {
    true
}

pub fn notification_response(response: &SystemNotificationResponse, cx: &mut App) {
    if response.tag.as_ref() == UPDATE_TAG
        && let Some(services) = cx
            .try_global::<super::SharedServices>()
            .map(|s| s.0.clone())
    {
        services.update(cx, |services, _| services.check_updates());
    }
}

pub fn start(
    model: &Entity<Model>,
    services: &Entity<Services>,
    arguments: &crate::Arguments,
    cx: &mut App,
) {
    // Without a window, zflow lives in the menu bar only.
    set_policy(false);
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            set_policy(false);
        }
    })
    .detach();
    {
        let services = services.clone();
        cx.observe(model, move |model, cx| {
            if let Some(snapshot) = model.read(cx).snapshot.clone() {
                services.update(cx, |services, cx| services.observe(&snapshot, cx));
            }
        })
        .detach();
    }
    {
        let (services, model) = (services.downgrade(), model.clone());
        cx.spawn(async move |cx| {
            loop {
                if services
                    .update(cx, |services, cx| services.tick(&model, cx))
                    .is_err()
                {
                    return;
                }
                cx.background_executor().timer(SERVICES_TICK).await;
            }
        })
        .detach();
    }
    // A receiver can drop the session while the Mac sleeps, and the link
    // would still look ready until its next keep-alive.
    {
        let model = model.clone();
        cx.on_system_wake(move |cx| model.read(cx).send(Request::Retry))
            .detach();
    }
    install_menus(cx);
    crate::tray::init(model, cx);
    crate::lens::init(model, cx);
    {
        let model = model.clone();
        // GPUI gives quit handlers 200 ms, too little for the engine to let
        // go of keys it holds for a peer, so this waits here instead.
        cx.on_app_quit(move |cx| {
            let done = model.read(cx).shutdown();
            let deadline = Instant::now() + SHUTDOWN_WAIT;
            while matches!(done.try_recv(), Err(async_channel::TryRecvError::Empty))
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            async {}
        })
        .detach();
    }
    // The engine's first answer says whether any computer is paired; the
    // window opens on its own only when there is something to do.
    let show_settings = arguments.show_settings;
    let needs_move = services.read(cx).location.needs_move();
    let model = model.clone();
    cx.spawn(async move |cx| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let known = model.read_with(cx, |model, _| {
                model.snapshot.is_some() || model.error.is_some()
            });
            if known {
                break;
            }
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
        }
        let () = cx.update(|cx| {
            let unpaired = model
                .read(cx)
                .snapshot
                .as_ref()
                .is_none_or(|snapshot| snapshot.peers.is_empty());
            if show_settings || unpaired || needs_move {
                crate::open(cx);
            }
        });
    })
    .detach();
}

fn install_menus(cx: &mut App) {
    cx.on_action(|_: &OpenSettings, cx| {
        crate::model::model(cx).update(cx, |model, cx| model.show(Page::Settings, cx));
        crate::open(cx);
    });
    cx.on_action(|_: &OpenConfiguration, cx| open_config(cx));
    cx.on_action(|_: &CheckForUpdates, cx| {
        if let Some(services) = cx
            .try_global::<super::SharedServices>()
            .map(|s| s.0.clone())
        {
            services.update(cx, |services, _| services.check_updates());
        }
    });
    cx.on_action(|_: &HideZflow, cx| cx.hide());
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &About, cx| {
        if let Some(mtm) = MainThreadMarker::new() {
            cx.activate(true);
            NSApplication::sharedApplication(mtm).orderFrontStandardAboutPanel(None);
        }
    });
    cx.on_action(|_: &CloseWindow, cx| {
        if let Some(window) = cx.active_window() {
            let _ = window.update(cx, |_, window, _| window.remove_window());
        }
    });
    cx.bind_keys([
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-h", HideZflow, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
    ]);
    cx.set_menus([
        Menu {
            name: "zflow".into(),
            items: vec![
                MenuItem::action("About zflow", About),
                MenuItem::action("Check for Updates…", CheckForUpdates),
                MenuItem::separator(),
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::action("Open Configuration…", OpenConfiguration),
                MenuItem::separator(),
                MenuItem::action("Hide zflow", HideZflow),
                MenuItem::action("Quit zflow", Quit),
            ],
            disabled: false,
        },
        Menu {
            name: "Window".into(),
            items: vec![MenuItem::action("Close Window", CloseWindow)],
            disabled: false,
        },
    ]);
}

/// Opens Sparkle's check, from the menu.
pub fn check_updates(cx: &mut App) {
    if let Some(services) = cx
        .try_global::<super::SharedServices>()
        .map(|s| s.0.clone())
    {
        services.update(cx, |services, _| services.check_updates());
    }
}

/// Quits: the engine lets go of any input a peer holds first.
pub fn quit(cx: &mut App) {
    cx.quit();
}

fn open_config(cx: &mut App) {
    let path = crate::model::model(cx).read(cx).config.clone();
    cx.open_with_system(&path);
}

fn open_privacy(anchor: &str, cx: &App) {
    cx.open_url(&format!(
        "x-apple.systempreferences:com.apple.preference.security?{anchor}"
    ));
}

fn open_accessibility(this: &mut ZflowWindow, cx: &mut Context<ZflowWindow>) {
    this.send(Request::AllowAccessibility, cx);
    open_privacy("Privacy_Accessibility", cx);
    watch_accessibility(this, cx);
}

/// While Accessibility is missing, checks it faster than the engine's own
/// ticks, and comes back from System Settings once it is on.
fn watch_accessibility(this: &mut ZflowWindow, cx: &mut Context<ZflowWindow>) {
    let model = this.model.clone();
    let task = cx.spawn(async move |_, cx| {
        loop {
            let missing =
                model.read_with(cx, |model, _| model.platform().accessibility == Some(false));
            if !missing {
                break;
            }
            let () = model.read_with(cx, |model, _| model.send(Request::CheckAccessibility));
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
        }
        let () = cx.update(|cx| cx.activate(true));
    });
    this.services.update(cx, |services, _| {
        services.watching_accessibility = Some(task);
    });
}

/// Looking for computers is what makes macOS ask; once it has, the switch is
/// in System Settings.
fn allow_local_network(this: &ZflowWindow, cx: &App) {
    if this
        .model
        .read(cx)
        .platform()
        .local_network
        .unwrap_or_default()
        == LocalNetwork::Unknown
    {
        this.send(Request::Discover, cx);
    } else {
        open_privacy("Privacy_LocalNetwork", cx);
    }
}

/// The first read makes macOS ask, which also lists zflow in System
/// Settings; after that, the switch is there.
fn allow_paste(this: &mut ZflowWindow, cx: &mut Context<ZflowWindow>) {
    if paste_access() == PasteAccess::Default {
        // SAFETY: reading the general pasteboard's string, as any copy does.
        let _ = unsafe { NSPasteboard::generalPasteboard().stringForType(NSPasteboardTypeString) };
        this.services.update(cx, |services, cx| {
            services.paste = paste_access();
            cx.notify();
        });
    } else {
        open_privacy("Privacy_Pasteboard", cx);
    }
}

/// "Move zflow to Applications", above every page while it runs from a
/// disk image or a temporary copy.
#[allow(
    clippy::unnecessary_wraps,
    reason = "every platform answers with the same signature"
)]
pub fn banner(
    this: &mut ZflowWindow,
    _: &mut Window,
    cx: &mut Context<ZflowWindow>,
) -> Option<AnyElement> {
    if !this.services.read(cx).location.needs_move() {
        return None;
    }
    Some(
        div()
            .p(px(14.0))
            .pb_0()
            .child(widgets::banner(
                zflow::app::api::Level::Warning,
                "Move zflow to Applications",
                "zflow is running from a disk image or a temporary folder. Drag it into Applications, then open it from there.",
                Some(
                    Button::new("show-applications")
                        .on(widgets::banner_ground(zflow::app::api::Level::Warning, cx))
                        .label("Show Applications")
                        .on_click(|_, _, cx| cx.open_with_system(std::path::Path::new("/Applications")))
                        .into_any_element(),
                ),
                cx,
            ))
            .into_any_element(),
    )
}

/// The app handles the fixes that open something here.
pub fn fix(
    this: &mut ZflowWindow,
    command: &str,
    _: &mut Window,
    cx: &mut Context<ZflowWindow>,
) -> bool {
    match command {
        "allow_accessibility" => open_accessibility(this, cx),
        "allow_paste" => allow_paste(this, cx),
        "open_config" => open_config(cx),
        _ => return false,
    }
    true
}

/// A health row's fix: the engine's own, or, for the clipboard row while
/// macOS keeps zflow from reading the pasteboard, one the app adds.
pub fn health_action(row: &Health, cx: &App) -> Option<Action> {
    if let Some(action) = &row.action {
        return Some(action.clone());
    }
    let paste = cx
        .try_global::<super::SharedServices>()
        .map_or(PasteAccess::AlwaysAllow, |services| {
            services.0.read(cx).paste
        });
    (row.id == "clipboard" && paste != PasteAccess::AlwaysAllow).then(|| Action {
        label: "Allow…".into(),
        command: "allow_paste".into(),
    })
}

/// A step that still needs the person, with the button that does it.
fn card(
    id: &'static str,
    icon: IconName,
    title: &'static str,
    detail: &'static str,
    action: impl Fn(&mut ZflowWindow, &mut Context<ZflowWindow>) + 'static,
    cx: &mut Context<ZflowWindow>,
) -> AnyElement {
    SettingsStack::new()
        .child(
            SettingEntry::new(title, detail)
                .title_icon(Icon::new(icon))
                .control(
                    Button::new(id)
                        .primary()
                        .label("Allow…")
                        .on_click(cx.listener(move |this, _, _, cx| action(this, cx))),
                ),
        )
        .into_any_element()
}

pub fn first_run_cards(
    this: &mut ZflowWindow,
    snapshot: &Snapshot,
    _: &mut Window,
    cx: &mut Context<ZflowWindow>,
) -> Vec<AnyElement> {
    // Looking for computers is what makes macOS ask for Local Network access.
    if !this.services.read(cx).discovered {
        this.services
            .update(cx, |services, _| services.discovered = true);
        this.send(Request::Discover, cx);
    }
    let platform = snapshot.platform.clone().unwrap_or_default();
    let mut cards = Vec::new();
    if platform.accessibility == Some(false) {
        cards.push(card(
            "allow-accessibility",
            IconName::CircleUser,
            "Let zflow move the pointer here",
            "macOS asks once, in Privacy & Security › Accessibility.",
            open_accessibility,
            cx,
        ));
    }
    if this.services.read(cx).network_blocked() {
        cards.push(card(
            "allow-network",
            IconName::Globe,
            "Let zflow look for your computers",
            "Turn on zflow in Privacy & Security › Local Network.",
            |this, cx| allow_local_network(this, cx),
            cx,
        ));
    }
    cards
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "every platform answers with the same signature"
)]
pub fn display_settings_entry(cx: &App) -> Option<SettingEntry> {
    Some(
        SettingEntry::new("Displays", "Arrange this Mac’s own screens.").control(
            Button::new("display-settings")
                .label("Displays…")
                .on(settings_card_ground(cx))
                .on_click(|_, _, cx| {
                    cx.open_url("x-apple.systempreferences:com.apple.Displays-Settings.extension");
                }),
        ),
    )
}

pub fn peer_entries(
    _: &mut ZflowWindow,
    peer: &Peer,
    cx: &mut Context<ZflowWindow>,
) -> Vec<SettingEntry> {
    if !copy::linked(peer.state) {
        return Vec::new();
    }
    let name = peer.name.clone();
    vec![
        SettingEntry::new(
            format!("Look at {name}"),
            format!(
                "See its screen around the pointer, and work there from this Mac. {} opens and closes it.",
                crate::lens::HOTKEY_LABEL
            ),
        )
        .control(
            Button::new("look")
                .label("Look")
                .on(settings_card_ground(cx))
                .on_click(move |_, _, cx| crate::lens::look(&name, cx)),
        ),
    ]
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "every platform answers with the same signature"
)]
pub fn login_entry(
    this: &mut ZflowWindow,
    _: &Snapshot,
    cx: &mut Context<ZflowWindow>,
) -> Option<SettingEntry> {
    let services = this.services.read(cx);
    let (on, needs_move) = (services.login, services.location.needs_move());
    Some(
        SettingEntry::new(
            "Start at login",
            "Open zflow in the menu bar when you log in.",
        )
        // A login item would point at the disk image or a temporary copy.
        .disabled(needs_move)
        .control(Switch::new("login").checked(on).on_click(cx.listener(
            |this, on: &bool, _, cx| {
                this.services
                    .update(cx, |services, cx| services.set_login(*on, cx));
            },
        ))),
    )
}

pub fn settings_stacks(
    this: &mut ZflowWindow,
    snapshot: &Snapshot,
    _: &mut Window,
    cx: &mut Context<ZflowWindow>,
) -> Vec<SettingsStack> {
    let platform = snapshot.platform.clone().unwrap_or_default();
    let services = this.services.read(cx);
    let mut stacks = Vec::new();
    // Only a signed build can install the helper this needs.
    if services.has_team {
        let enabled = platform.block_awdl == Some(true);
        let helper = services.helper.clone();
        let waiting = enabled && !helper.ready;
        let mut entry = SettingEntry::new(
            "Reduce Wi-Fi lag",
            if waiting {
                helper.note.clone()
            } else {
                "Pauses AirDrop’s radio while input is shared with another computer.".into()
            },
        );
        let fix = waiting.then_some(helper.action).flatten().map(|label| {
            Button::new("fix-helper")
                .label(label)
                .disabled(helper.busy)
                .on(settings_card_ground(cx))
                .on_click(cx.listener(|this, _, _, cx| {
                    let model = this.model.clone();
                    this.services
                        .update(cx, |services, cx| services.fix_helper(model, cx));
                }))
        });
        entry = entry.control(
            zz_gpui_kit::h_flex().gap(px(8.0)).children(fix).child(
                Switch::new("block-awdl")
                    .checked(enabled)
                    .on_click(cx.listener(|this, on: &bool, _, cx| {
                        this.send(Request::SetAwdl { enabled: *on }, cx);
                    })),
            ),
        );
        stacks.push(SettingsStack::titled("Wi-Fi").child(entry));
    }

    let allowed = |title: &'static str,
                   allowed: bool,
                   detail: &'static str,
                   id: &'static str,
                   cx: &mut Context<ZflowWindow>,
                   allow: fn(&mut ZflowWindow, &mut Context<ZflowWindow>)| {
        SettingEntry::new(title, if allowed { "" } else { detail }).control(if allowed {
            zz_gpui_kit::h_flex()
                .gap(px(6.0))
                .child(
                    Icon::new(IconName::CircleCheck)
                        .text_color(zz_gpui_kit::ActiveTheme::theme(&**cx).success),
                )
                .child(widgets::muted("Allowed", 12.0, cx))
                .into_any_element()
        } else {
            Button::new(id)
                .label("Allow…")
                .on(settings_card_ground(cx))
                .on_click(cx.listener(move |this, _, _, cx| allow(this, cx)))
                .into_any_element()
        })
    };
    let paste = services.paste;
    stacks.push(
        SettingsStack::titled("Permissions")
            .description(format!(
                "Take back control from any computer with {}.",
                copy::RETURN_KEYS
            ))
            .child(allowed(
                "Accessibility",
                platform.accessibility == Some(true),
                "Needed to move the pointer and type here.",
                "permission-accessibility",
                cx,
                open_accessibility,
            ))
            .child(allowed(
                "Local Network",
                platform.local_network == Some(LocalNetwork::Allowed),
                if platform.local_network == Some(LocalNetwork::Blocked) {
                    "Needed to find your computers and reach them."
                } else {
                    "macOS asks when zflow first looks for computers."
                },
                "permission-network",
                cx,
                |this, cx| allow_local_network(this, cx),
            ))
            .child(allowed(
                "Paste from other apps",
                paste == PasteAccess::AlwaysAllow,
                "Needed to share what you copy on this Mac.",
                "permission-paste",
                cx,
                allow_paste,
            )),
    );

    let services = this.services.read(cx);
    let updates = services.updates.clone();
    let mut stack = SettingsStack::titled("Updates");
    if let Some(reason) = updates.unavailable {
        stack = stack.child(SettingEntry::new("Updates", reason));
    } else {
        let checks = updates.automatic.unwrap_or(false);
        stack = stack
            .child(
                SettingEntry::new(
                    format!("zflow {}", env!("CARGO_PKG_VERSION")),
                    updates.available.as_ref().map_or_else(
                        || "Check for a new release.".into(),
                        |version| format!("zflow {version} is available."),
                    ),
                )
                .control(
                    Button::new("check-updates")
                        .label("Check for Updates…")
                        .disabled(updates.checking)
                        .on(settings_card_ground(cx))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.services
                                .update(cx, |services, _| services.check_updates());
                        })),
                ),
            )
            .child(
                SettingEntry::new("Check for updates automatically", "").control(
                    Switch::new("updates-check")
                        .checked(checks)
                        .on_click(cx.listener(|this, on: &bool, _, cx| {
                            this.services.update(cx, |services, cx| {
                                if let Some(sparkle) = &services.sparkle {
                                    sparkle.set_checks_automatically(*on);
                                }
                                services.refresh_updates();
                                cx.notify();
                            });
                        })),
                ),
            )
            .child(
                SettingEntry::new("Download and install updates automatically", "")
                    .disabled(!checks)
                    .control(
                        Switch::new("updates-install")
                            .checked(updates.install_automatically.unwrap_or(false))
                            .on_click(cx.listener(|this, on: &bool, _, cx| {
                                this.services.update(cx, |services, cx| {
                                    if let Some(sparkle) = &services.sparkle {
                                        sparkle.set_downloads_automatically(*on);
                                    }
                                    services.refresh_updates();
                                    cx.notify();
                                });
                            })),
                    ),
            );
    }
    stacks.push(stack);
    stacks
}

pub fn this_computer_entries(
    _: &mut ZflowWindow,
    snapshot: &Snapshot,
    cx: &mut Context<ZflowWindow>,
) -> Vec<SettingEntry> {
    vec![
        SettingEntry::new("Configuration", snapshot.config_path.display().to_string()).control(
            Button::new("open-config")
                .label("Open…")
                .on(settings_card_ground(cx))
                .on_click(|_, _, cx| open_config(cx)),
        ),
    ]
}

/// The menu bar icon: the zflow mark as a template image the bar tints,
/// dimmed while zflow needs the person, or the update arrow.
pub fn tray_look(healthy: bool, update: bool) -> zz_gpui_tray::TrayIcon {
    let svg = if update {
        UPDATE_SVG.to_owned()
    } else {
        MARK_SVG.replace("OPACITY", if healthy { "1" } else { "0.45" })
    };
    zz_gpui_tray::TrayIcon {
        png: crate::tray::rasterize(&svg, 36),
        template: true,
    }
}

/// The zflow mark from assets/logo.svg.
const MARK_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="166 149 640 640" fill-opacity="OPACITY"><path d="M766 212C780 208 792 220 788 234L680 640C676 655 655 657 648 643L590 530L414 706C402 718 382 718 370 706L318 654C306 642 306 622 318 610L494 434L381 376C367 369 369 348 384 344Z"/><path d="M188 474L274 388C283 379 297 379 306 388L324 406C333 415 333 429 324 438L238 524C229 533 215 533 206 524L188 506C179 497 179 483 188 474ZM204 670L234 640C243 631 257 631 266 640L284 658C293 667 293 681 284 690L254 720C245 729 231 729 222 720L204 702C195 693 195 679 204 670Z"/></svg>"#;
/// A circled arrow pointing down, while an update waits.
const UPDATE_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M12 7v9M8 12.5l4 4 4-4"/></svg>"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn location(path: &str, quarantined: bool, read_only: bool) -> AppLocation {
        AppLocation {
            path: path.into(),
            quarantined,
            read_only,
            home: "/Users/me".into(),
        }
    }

    #[test]
    fn only_temporary_copies_need_a_move() {
        assert!(
            location(
                "/private/var/folders/x/AppTranslocation/y/zflow.app",
                false,
                true
            )
            .needs_move()
        );
        assert!(location("/Volumes/zflow/zflow.app", false, true).needs_move());
        assert!(location("/Users/me/Downloads/zflow.app", true, false).needs_move());
        assert!(!location("/Applications/zflow.app", true, false).needs_move());
        assert!(!location("/Users/me/Applications/zflow.app", true, false).needs_move());
        assert!(!location("/Users/me/dev/zflow/target/debug/zflow.app", false, false).needs_move());
    }
}
