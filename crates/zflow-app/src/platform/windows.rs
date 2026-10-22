//! Windows: the app keeps the input engine running beside it, lives in the
//! notification area, starts at sign-in through the Run key, restarts as
//! administrator on request, and updates itself through Velopack.
#![allow(
    unsafe_code,
    reason = "Win32 handles, events and the handshake window have no safe binding"
)]

use std::{
    io::Read as _,
    os::windows::process::CommandExt as _,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WAIT_OBJECT_0,
            WPARAM,
        },
        System::{
            LibraryLoader::GetModuleHandleW,
            Threading::{
                CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, OpenEventW, OpenProcess,
                PROCESS_SYNCHRONIZE, SetEvent, WaitForSingleObject,
            },
        },
        UI::{
            Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowW, GetMessageW,
                MB_ICONERROR, MSG, MessageBoxW, PostMessageW, RegisterClassW, SW_SHOWNORMAL,
                TranslateMessage, WINDOW_EX_STYLE, WNDCLASSW, WS_POPUP,
            },
        },
    },
    core::{HSTRING, PCWSTR, w},
};
use zflow::app::api::{Action, Health, Peer, Request};
use zz_gpui::{
    AnyElement, App, AppContext as _, Context, Entity, IntoElement, ParentElement as _,
    Styled as _, SystemNotificationResponse, Task, Window, div, px,
};
use zz_gpui_kit::{
    Disableable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::DialogButtonProps,
    settings::{SettingEntry, SettingsStack, settings_card_ground},
    switch::Switch,
};

use super::Updates;
use crate::{
    model::{Model, Snapshot},
    widgets,
    window::ZflowWindow,
};

/// Windows computers are not looked at through a lens yet.
pub const CAN_SHARE_SCREEN: bool = false;
/// The Windows engine lets every trusted computer take control.
pub const CAN_REFUSE_CONTROL: bool = false;

pub const OWN_SCREENS_NOTE: &str =
    "Between this PC’s own screens, the pointer follows Windows display settings.";

/// Closing the window hides it to the notification area.
pub const LIVES_IN_TRAY: bool = true;

/// Messages the handshake window takes from other copies of the app.
const WM_QUIT_APP: u32 = 0x8002;
const WM_SHOW_APP: u32 = 0x8003;
/// The handshake window's class, which `--quit` looks for.
const HANDSHAKE_CLASS: PCWSTR = w!("Zflow.App");
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const PREFERENCES_KEY: &str = r"Software\zflow";
const UPDATE_INTERVAL: Duration = Duration::from_hours(6);
/// How often the app makes sure the engine runs.
const SUPERVISE: Duration = Duration::from_secs(5);

fn message_box(text: &str) {
    // SAFETY: an owner-less message box with two NUL-terminated strings.
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), w!("zflow"), MB_ICONERROR);
    }
}

pub fn default_config() -> PathBuf {
    zflow::windows::config_path()
}

/// The stable launcher Velopack keeps beside each version, which survives
/// updates, else this copy.
fn startup_executable() -> PathBuf {
    let current = std::env::current_exe().unwrap_or_default();
    current
        .parent()
        .filter(|dir| dir.file_name().is_some_and(|name| name == "current"))
        .and_then(Path::parent)
        .map(|root| root.join("Zflow.App.exe"))
        .filter(|path| path.exists())
        .unwrap_or(current)
}

fn installed() -> bool {
    startup_executable() != std::env::current_exe().unwrap_or_default()
}

fn run_value() -> String {
    format!("\"{}\" --background", startup_executable().display())
}

fn starts_at_sign_in() -> bool {
    windows_registry::CURRENT_USER
        .open(RUN_KEY)
        .and_then(|key| key.get_string("zflow"))
        .is_ok()
}

fn set_starts_at_sign_in(on: bool) -> anyhow::Result<()> {
    let key = windows_registry::CURRENT_USER.create(RUN_KEY)?;
    if on {
        key.set_string("zflow", run_value())?;
    } else {
        let _ = key.remove_value("zflow");
    }
    Ok(())
}

/// Points an existing Run value at this install, after an update moves it.
fn refresh_startup_path() {
    if installed() && starts_at_sign_in() {
        let _ = set_starts_at_sign_in(true);
    }
}

fn remove_startup_path() {
    let Ok(key) = windows_registry::CURRENT_USER.create(RUN_KEY) else {
        return;
    };
    let ours = format!("\"{}\"", startup_executable().display());
    if key
        .get_string("zflow")
        .is_ok_and(|command| command.to_lowercase().starts_with(&ours.to_lowercase()))
    {
        let _ = key.remove_value("zflow");
    }
}

fn checks_for_updates() -> bool {
    windows_registry::CURRENT_USER
        .open(PREFERENCES_KEY)
        .and_then(|key| key.get_u32("CheckForUpdates"))
        != Ok(0)
}

/// Velopack's install and uninstall hooks, which end the process when
/// Velopack started it for them.
pub fn velopack_hooks() {
    velopack::VelopackApp::build()
        .set_auto_apply_on_startup(false)
        .on_after_install_fast_callback(|_| refresh_startup_path())
        .on_before_uninstall_fast_callback(|_| remove_startup_path())
        .run();
}

/// Runs before the app: `--quit`, a restart as administrator taking over,
/// and the one-copy rule. False ends it.
pub fn before_start(arguments: &crate::Arguments) -> bool {
    if arguments.quit {
        post_to_running(WM_QUIT_APP);
        return false;
    }
    if !wait_for_previous(arguments) {
        return false;
    }
    let name = HSTRING::from(format!(
        r"Local\Zflow.WinUI.{}",
        std::env::var("USERNAME").unwrap_or_default()
    ));
    // SAFETY: a named mutex the process holds until it exits.
    let mutex = unsafe { CreateMutexW(None, true, &name) };
    let running = match mutex {
        // An elevated copy owns it, which this one cannot open.
        Err(_) => true,
        Ok(_) => (unsafe { GetLastError() }) == ERROR_ALREADY_EXISTS,
    };
    if running {
        post_to_running(WM_SHOW_APP);
        return false;
    }
    refresh_startup_path();
    true
}

/// Finds the running app by its handshake window, or the `WinUI` app that
/// came before it by its window title.
fn post_to_running(message: u32) {
    // SAFETY: looking up and posting to another process's top-level window.
    unsafe {
        let window = FindWindowW(HANDSHAKE_CLASS, PCWSTR::null())
            .or_else(|_| FindWindowW(PCWSTR::null(), w!("zflow")));
        if let Ok(window) = window {
            let _ = PostMessageW(Some(window), message, WPARAM(0), LPARAM(0));
        }
    }
}

/// An administrator copy started by "Restart as administrator" takes over
/// once the copy that asked has gone.
fn wait_for_previous(arguments: &crate::Arguments) -> bool {
    let Some(pid) = arguments.replace_process else {
        return true;
    };
    let result = (|| -> Result<(), String> {
        if arguments.replace_user.as_deref() != zflow::windows::user_sid().ok().as_deref() {
            return Err(
                "Restart using the same Windows account to keep your computers and settings."
                    .into(),
            );
        }
        if !zflow::windows::elevated() || pid == std::process::id() {
            return Err("Could not restart zflow as administrator.".into());
        }
        let ready = arguments
            .restart_ready
            .as_ref()
            .ok_or("The restart request is incomplete.")?;
        // SAFETY: opening and setting the event the asking copy waits on,
        // then waiting for it to exit.
        unsafe {
            let event = OpenEventW(EVENT_MODIFY_STATE, false, &HSTRING::from(ready.as_str()))
                .map_err(|error| error.to_string())?;
            let _ = SetEvent(event);
            let _ = CloseHandle(event);
            if let Ok(previous) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
                let waited = WaitForSingleObject(previous, 20_000);
                let _ = CloseHandle(previous);
                if waited != WAIT_OBJECT_0 {
                    return Err(
                        "The previous zflow app did not stop. Quit it and try again.".into(),
                    );
                }
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => true,
        Err(message) => {
            message_box(&message);
            false
        }
    }
}

/// Starts this app again as administrator, so input reaches administrator
/// windows. Keeps running if the person says no.
fn restart_as_administrator(cx: &mut App) {
    let ready = format!(r"Local\Zflow.Restart.{:032x}", rand_id());
    let arguments = format!(
        "--replace-process {} --replace-user {} --restart-ready {ready}",
        std::process::id(),
        zflow::windows::user_sid().unwrap_or_default()
    );
    let result = (|| -> Result<bool, String> {
        // SAFETY: a named event, and an elevated launch of this executable
        // that signals it once it has taken over.
        unsafe {
            let event = CreateEventW(None, true, false, &HSTRING::from(ready.as_str()))
                .map_err(|error| error.to_string())?;
            let executable = HSTRING::from(std::env::current_exe().unwrap_or_default().as_os_str());
            let parameters = HSTRING::from(arguments.as_str());
            let mut info = SHELLEXECUTEINFOW {
                cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
                fMask: SEE_MASK_NOCLOSEPROCESS,
                lpVerb: w!("runas"),
                lpFile: PCWSTR(executable.as_ptr()),
                lpParameters: PCWSTR(parameters.as_ptr()),
                nShow: SW_SHOWNORMAL.0,
                ..Default::default()
            };
            if ShellExecuteExW(&raw mut info).is_err() {
                let _ = CloseHandle(event);
                // Cancelled at the prompt: keep the current app.
                return Ok(false);
            }
            if !info.hProcess.is_invalid() {
                let _ = CloseHandle(info.hProcess);
            }
            let waited = WaitForSingleObject(event, 8_000);
            let _ = CloseHandle(event);
            if waited != WAIT_OBJECT_0 {
                return Err(
                    "The administrator app could not start. Your current zflow is still running."
                        .into(),
                );
            }
            Ok(true)
        }
    })();
    match result {
        Ok(true) => quit(cx),
        Ok(false) => {}
        Err(message) => crate::model::model(cx).update(cx, |model, cx| model.report(message, cx)),
    }
}

/// A name no other restart uses at the same time.
fn rand_id() -> u128 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    nanos ^ (u128::from(std::process::id()) << 96)
}

/// The engine process this app keeps running.
struct Engine {
    config: PathBuf,
    child: Option<Child>,
    /// The last lines it wrote to stderr, for when it fails to start.
    errors: Arc<Mutex<Vec<String>>>,
    quitting: bool,
}

impl Engine {
    fn pipe(&self) -> anyhow::Result<zflow::app::windows_view::Pipe> {
        zflow::app::windows_view::Pipe::new(&self.config)
    }

    /// Starts the engine unless one answers. An administrator app does not
    /// reuse a normal engine, which could not reach administrator windows.
    fn ensure(&mut self) -> Result<(), String> {
        let pipe = self.pipe().map_err(|error| format!("{error:#}"))?;
        if let Some(status) = pipe.probe(Duration::from_millis(400)) {
            let elevated = status["elevated"].as_bool() == Some(true);
            if !zflow::windows::elevated() || elevated {
                return Ok(());
            }
            self.stop();
        }
        let executable = std::env::current_exe()
            .map_err(|error| error.to_string())?
            .with_file_name("zflow.exe");
        if !executable.exists() {
            return Err("The Windows input engine is missing. Reinstall zflow.".into());
        }
        if let Some(dir) = self.config.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut child = Command::new(executable)
            .arg("--config")
            .arg(&self.config)
            .arg("run")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|error| format!("The input engine could not start: {error}"))?;
        if let Some(mut stderr) = child.stderr.take() {
            let errors = self.errors.clone();
            std::thread::spawn(move || {
                let mut text = String::new();
                let mut buffer = [0u8; 1024];
                while let Ok(read) = stderr.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    text.push_str(&String::from_utf8_lossy(&buffer[..read]));
                    let mut kept = errors
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *kept = text
                        .lines()
                        .rev()
                        .take(12)
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    kept.reverse();
                }
            });
        }
        for _ in 0..40 {
            if let Ok(Some(_)) = child.try_wait() {
                let errors = self
                    .errors
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .join("\n");
                return Err(format!("Input engine could not start. {errors}"));
            }
            if pipe.probe(Duration::from_millis(400)).is_some() {
                self.child = Some(child);
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.child = Some(child);
        Err("The input engine did not start. Check that UDP port 43119 is available.".into())
    }

    /// Stops the engine, which lets go of input a peer holds.
    fn stop(&mut self) {
        let Ok(pipe) = self.pipe() else { return };
        let pid = pipe
            .probe(Duration::from_millis(1500))
            .and_then(|status| status["pid"].as_u64())
            .and_then(|pid| u32::try_from(pid).ok());
        let _ = pipe.quit();
        if let Some(pid) = pid {
            // SAFETY: waiting on a process handle opened for that alone.
            unsafe {
                if let Ok(process) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
                    WaitForSingleObject(process, 8_000);
                    let _ = CloseHandle(process);
                }
            }
        }
        if let Some(mut child) = self.child.take() {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

struct EngineHandle(Arc<Mutex<Engine>>);

impl zz_gpui::Global for EngineHandle {}

/// What the Updates rows know.
enum Release {
    None,
    Available(Box<velopack::UpdateInfo>),
    Downloaded(velopack::VelopackAsset),
}

pub struct Services {
    pub updates: Updates,
    release: Release,
    pub login: bool,
    pub elevated: bool,
    pub engine_error: Option<String>,
    pub progress: Option<i16>,
    _tasks: Vec<Task<()>>,
}

fn update_manager() -> Option<velopack::UpdateManager> {
    let source =
        velopack::sources::GithubSource::new("https://github.com/demfabris/zflow", None, false);
    let options = velopack::UpdateOptions {
        ExplicitChannel: Some("win-x64".into()),
        ..Default::default()
    };
    velopack::UpdateManager::new(source, Some(options), None)
        .ok()
        .filter(|manager| !manager.get_is_portable())
}

impl Services {
    fn new(cx: &mut Context<Self>) -> Self {
        let manager = update_manager();
        let release = manager
            .as_ref()
            .and_then(velopack::UpdateManager::get_update_pending_restart)
            .map_or(Release::None, Release::Downloaded);
        let updates_task = cx.spawn(async move |this, cx| {
            loop {
                let Ok(automatic) =
                    this.update(cx, |services, _| services.updates.automatic == Some(true))
                else {
                    return;
                };
                if automatic {
                    let _ = this.update(cx, Services::check);
                }
                cx.background_executor().timer(UPDATE_INTERVAL).await;
            }
        });
        let mut services = Self {
            updates: Updates {
                unavailable: manager.is_none().then(|| {
                    "Updates are available in the installed zflow. Get the installer from the releases page.".into()
                }),
                automatic: Some(checks_for_updates()),
                ..Updates::default()
            },
            release,
            login: starts_at_sign_in(),
            elevated: zflow::windows::elevated(),
            engine_error: None,
            progress: None,
            _tasks: vec![updates_task],
        };
        services.describe();
        services
    }

    fn describe(&mut self) {
        match &self.release {
            Release::None => self.updates.available = None,
            Release::Available(info) => {
                self.updates.available = Some(info.TargetFullRelease.Version.clone());
                self.updates.note = Some(format!(
                    "zflow {} is available.",
                    info.TargetFullRelease.Version
                ));
            }
            Release::Downloaded(asset) => {
                self.updates.available = Some(asset.Version.clone());
                self.updates.note = Some(format!(
                    "zflow {} is ready. Restart when you are ready to pause sharing.",
                    asset.Version
                ));
            }
        }
    }

    pub fn check(&mut self, cx: &mut Context<Self>) {
        if self.updates.checking
            || self.updates.unavailable.is_some()
            || matches!(self.release, Release::Downloaded(_))
        {
            return;
        }
        self.updates.checking = true;
        self.updates.note = Some("Checking for updates…".into());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async {
                    let manager = update_manager().ok_or("Updates are not available here")?;
                    manager
                        .check_for_updates()
                        .map_err(|error| error.to_string())
                })
                .await;
            let _ = this.update(cx, |services, cx| {
                services.updates.checking = false;
                match result {
                    Ok(velopack::UpdateCheck::UpdateAvailable(info)) => {
                        services.release = Release::Available(info);
                    }
                    Ok(_) => {
                        services.release = Release::None;
                        services.updates.note = Some("You have the latest version.".into());
                    }
                    Err(error) => services.updates.note = Some(error.clone()),
                }
                services.describe();
                cx.notify();
            });
        })
        .detach();
    }

    fn download(&mut self, cx: &mut Context<Self>) {
        let Release::Available(info) = &self.release else {
            return;
        };
        let info = (**info).clone();
        self.updates.checking = true;
        self.progress = Some(0);
        cx.notify();
        let (progress, ticks) = std::sync::mpsc::channel::<i16>();
        let (sender, receiver) = async_channel::unbounded();
        std::thread::spawn(move || {
            while let Ok(tick) = ticks.recv() {
                let _ = sender.send_blocking(tick);
            }
        });
        let ticker = cx.spawn(async move |this, cx| {
            while let Ok(tick) = receiver.recv().await {
                let _ = this.update(cx, |services, cx| {
                    services.progress = Some(tick);
                    cx.notify();
                });
            }
        });
        cx.spawn(async move |this, cx| {
            let target = info.TargetFullRelease.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let manager =
                        update_manager().ok_or("Updates are not available here".to_owned())?;
                    manager
                        .download_updates(&info, Some(progress))
                        .map_err(|error| error.to_string())
                })
                .await;
            drop(ticker);
            let _ = this.update(cx, |services, cx| {
                services.updates.checking = false;
                services.progress = None;
                match result {
                    Ok(()) => services.release = Release::Downloaded(target),
                    Err(error) => services.updates.note = Some(error),
                }
                services.describe();
                cx.notify();
            });
        })
        .detach();
    }

    /// Stops the engine first, so the updater can replace it, then hands
    /// over to Velopack, which restarts zflow.
    fn restart_and_update(&mut self, cx: &mut Context<Self>) {
        let Release::Downloaded(asset) = &self.release else {
            return;
        };
        let asset = asset.clone();
        stop_engine(cx);
        match update_manager().map(|manager| {
            manager.wait_exit_then_apply_updates(&asset, false, true, Vec::<String>::new())
        }) {
            Some(Ok(())) => cx.quit(),
            Some(Err(error)) => {
                self.updates.note = Some(error.to_string());
                start_engine(false, cx);
            }
            None => start_engine(false, cx),
        }
        cx.notify();
    }
}

pub fn init(cx: &mut App) -> Entity<Services> {
    let services = cx.new(Services::new);
    cx.set_global(super::SharedServices(services.clone()));
    services
}

pub fn start(
    model: &Entity<Model>,
    services: &Entity<Services>,
    arguments: &crate::Arguments,
    cx: &mut App,
) {
    let engine = Arc::new(Mutex::new(Engine {
        config: model.read(cx).config.clone(),
        child: None,
        errors: Arc::new(Mutex::new(Vec::new())),
        quitting: false,
    }));
    cx.set_global(EngineHandle(engine.clone()));
    // Keeps the engine running, and starts it again if it stops.
    let (errors, inbox) = async_channel::unbounded::<Option<String>>();
    std::thread::Builder::new()
        .name("zflow-engine".into())
        .spawn(move || {
            loop {
                // Stopped for quitting or an update: waits, since a failed
                // update starts the engine again.
                let outcome = {
                    let mut engine = engine
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    (!engine.quitting).then(|| engine.ensure())
                };
                if let Some(outcome) = outcome
                    && errors.send_blocking(outcome.err()).is_err()
                {
                    return;
                }
                std::thread::sleep(SUPERVISE);
            }
        })
        .ok();
    {
        let services = services.downgrade();
        let model = model.clone();
        cx.spawn(async move |cx| {
            while let Ok(error) = inbox.recv().await {
                let started = error.is_none();
                let changed = services
                    .update(cx, |services, cx| {
                        let changed = services.engine_error != error;
                        services.engine_error = error;
                        cx.notify();
                        changed
                    })
                    .unwrap_or(false);
                if started && changed {
                    model.update(cx, super::super::model::Model::reconnect);
                }
            }
        })
        .detach();
    }
    handshake_window(cx);
    crate::tray::init(model, cx);
    cx.on_app_quit(|cx| {
        stop_engine(cx);
        async {}
    })
    .detach();
    if !arguments.background {
        crate::open(cx);
    }
}

fn engine(cx: &App) -> Option<Arc<Mutex<Engine>>> {
    cx.try_global::<EngineHandle>()
        .map(|handle| handle.0.clone())
}

fn stop_engine(cx: &App) {
    if let Some(engine) = engine(cx) {
        let mut engine = engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        engine.quitting = true;
        engine.stop();
    }
}

/// Starts the engine, or stops it and starts it again, off the main thread:
/// waiting for it to answer takes seconds. The supervisor reports failures.
fn start_engine(restart: bool, cx: &App) {
    if let Some(engine) = engine(cx) {
        cx.background_executor()
            .spawn(async move {
                let mut engine = engine
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if restart {
                    engine.stop();
                }
                engine.quitting = false;
                let _ = engine.ensure();
            })
            .detach();
    }
}

/// A hidden window other copies find: `--quit` asks this one to quit, and a
/// second launch asks it to show the window. Listens from the start, so a
/// copy started in the background answers too.
fn handshake_window(cx: &mut App) {
    let (events, inbox) = async_channel::unbounded::<u32>();
    std::thread::Builder::new()
        .name("zflow-handshake".into())
        .spawn(move || {
            let _ = HANDSHAKE.set(events);
            // SAFETY: a plain window class and window on this thread, and its
            // message loop.
            unsafe {
                let Ok(instance) = GetModuleHandleW(None) else {
                    return;
                };
                let class = WNDCLASSW {
                    lpfnWndProc: Some(handshake_procedure),
                    hInstance: instance.into(),
                    lpszClassName: HANDSHAKE_CLASS,
                    ..Default::default()
                };
                RegisterClassW(&raw const class);
                let Ok(_) = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    HANDSHAKE_CLASS,
                    w!("zflow"),
                    WS_POPUP,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    Some(instance.into()),
                    None,
                ) else {
                    return;
                };
                let mut message = MSG::default();
                while GetMessageW(&raw mut message, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&raw const message);
                    DispatchMessageW(&raw const message);
                }
            }
        })
        .ok();
    cx.spawn(async move |cx| {
        while let Ok(message) = inbox.recv().await {
            let () = cx.update(|cx| match message {
                WM_QUIT_APP => quit(cx),
                _ => crate::open(cx),
            });
        }
    })
    .detach();
}

static HANDSHAKE: std::sync::OnceLock<async_channel::Sender<u32>> = std::sync::OnceLock::new();

unsafe extern "system" fn handshake_procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if (message == WM_QUIT_APP || message == WM_SHOW_APP)
        && let Some(events) = HANDSHAKE.get()
    {
        let _ = events.try_send(message);
        return LRESULT(0);
    }
    // SAFETY: the default procedure for everything else.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

pub fn window_shown(_: &mut App) {}

/// The app stays in the notification area, so the window always closes.
pub fn may_close(_: &App) -> bool {
    true
}

pub fn notification_response(_: &SystemNotificationResponse, _: &mut App) {}

pub fn check_updates(cx: &mut App) {
    crate::open(cx);
    if let Some(services) = cx
        .try_global::<super::SharedServices>()
        .map(|s| s.0.clone())
    {
        services.update(cx, Services::check);
    }
}

/// Stops the engine and quits.
pub fn quit(cx: &mut App) {
    stop_engine(cx);
    cx.quit();
}

pub fn tray_look(_: bool, _: bool) -> zz_gpui_tray::TrayIcon {
    zz_gpui_tray::TrayIcon {
        png: include_bytes!("../../../../assets/windows/zflow.png").to_vec(),
        template: false,
    }
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "every platform answers with the same signature"
)]
pub fn banner(
    this: &mut ZflowWindow,
    _: &mut Window,
    cx: &mut Context<ZflowWindow>,
) -> Option<AnyElement> {
    let error = this.services.read(cx).engine_error.clone()?;
    Some(
        div()
            .p(px(14.0))
            .pb_0()
            .child(widgets::banner(
                zflow::app::api::Level::Error,
                "zflow needs attention",
                error,
                Some(
                    Button::new("restart-engine")
                        .on(widgets::banner_ground(zflow::app::api::Level::Error, cx))
                        .label("Restart")
                        .on_click(|_, _, cx| start_engine(false, cx))
                        .into_any_element(),
                ),
                cx,
            ))
            .into_any_element(),
    )
}

pub fn fix(_: &mut ZflowWindow, _: &str, _: &mut Window, _: &mut Context<ZflowWindow>) -> bool {
    false
}

pub fn health_action(row: &Health, _: &App) -> Option<Action> {
    row.action.clone()
}

pub fn first_run_cards(
    _: &mut ZflowWindow,
    _: &Snapshot,
    _: &mut Window,
    _: &mut Context<ZflowWindow>,
) -> Vec<AnyElement> {
    Vec::new()
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "every platform answers with the same signature"
)]
pub fn display_settings_entry(cx: &App) -> Option<SettingEntry> {
    Some(
        SettingEntry::new(
            "Display settings",
            "Arrange this PC’s own screens in Windows Settings.",
        )
        .control(
            Button::new("display-settings")
                .label("Open")
                .on(settings_card_ground(cx))
                .on_click(|_, _, cx| cx.open_url("ms-settings:display")),
        ),
    )
}

pub fn peer_entries(
    _: &mut ZflowWindow,
    peer: &Peer,
    cx: &mut Context<ZflowWindow>,
) -> Vec<SettingEntry> {
    if !crate::copy::linked(peer.state) {
        return Vec::new();
    }
    let name = peer.name.clone();
    vec![
        SettingEntry::new(
            "Take control",
            format!("Send this PC’s keyboard and mouse to {name} now."),
        )
        .control(
            Button::new("take-control")
                .label("Control")
                .on(settings_card_ground(cx))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.send(Request::TakeControl { name: name.clone() }, cx);
                })),
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
    let on = this.services.read(cx).login;
    Some(
        SettingEntry::new("Run at startup", "Start zflow when you sign in to Windows.").control(
            Switch::new("login")
                .checked(on)
                .on_click(cx.listener(|this, on: &bool, _, cx| {
                    let result = set_starts_at_sign_in(*on);
                    this.services.update(cx, |services, cx| {
                        services.login = starts_at_sign_in();
                        cx.notify();
                    });
                    if let Err(error) = result {
                        this.model
                            .update(cx, |model, cx| model.report(format!("{error:#}"), cx));
                    }
                })),
        ),
    )
}

pub fn settings_stacks(
    this: &mut ZflowWindow,
    _: &Snapshot,
    _: &mut Window,
    cx: &mut Context<ZflowWindow>,
) -> Vec<SettingsStack> {
    let services = this.services.read(cx);
    let elevated = services.elevated;
    let administrator = SettingEntry::new(
        if elevated {
            "Running as administrator"
        } else {
            "Running as user"
        },
        "Administrator windows need zflow to run as administrator too.",
    )
    .control(
        Button::new("elevate")
            .label("Restart as administrator")
            .disabled(elevated)
            .on(settings_card_ground(cx))
            .on_click(|_, _, cx| restart_as_administrator(cx)),
    );
    let updates = services.updates.clone();
    let mut stack = SettingsStack::titled("Updates");
    if let Some(reason) = updates.unavailable {
        stack = stack.child(SettingEntry::new(format!("zflow {}", env!("CARGO_PKG_VERSION")), reason).control(
            Button::new("get-installer")
                .label("Get installer")
                .on(settings_card_ground(cx))
                .on_click(|_, _, cx| {
                    cx.open_url("https://github.com/demfabris/zflow/releases/latest/download/zflow-windows-x86_64-Setup.exe");
                }),
        ));
    } else {
        let (label, action): (&'static str, fn(&mut Services, &mut Context<Services>)) =
            match services.release {
                Release::None => ("Check for updates", Services::check),
                Release::Available(_) => ("Download update", Services::download),
                Release::Downloaded(_) => ("Restart and update", |_, _| {}),
            };
        let downloaded = matches!(services.release, Release::Downloaded(_));
        let note = match services.progress {
            Some(progress) => format!("Downloading… {progress}%"),
            None => updates.note.clone().unwrap_or_else(|| {
                "Check for new releases automatically. You choose when to download and restart."
                    .into()
            }),
        };
        stack = stack
            .child(
                SettingEntry::new(format!("zflow {}", env!("CARGO_PKG_VERSION")), note).control(
                    Button::new("update-action")
                        .label(label)
                        .disabled(updates.checking)
                        .on(settings_card_ground(cx))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if downloaded {
                                confirm_restart(this, window, cx);
                            } else {
                                this.services.update(cx, action);
                            }
                        })),
                ),
            )
            .child(
                SettingEntry::new("Check for updates automatically", "").control(
                    Switch::new("updates-automatic")
                        .checked(updates.automatic.unwrap_or(true))
                        .on_click(cx.listener(|this, on: &bool, _, cx| {
                            if let Ok(key) = windows_registry::CURRENT_USER.create(PREFERENCES_KEY)
                            {
                                let _ = key.set_u32("CheckForUpdates", u32::from(*on));
                            }
                            this.services.update(cx, |services, cx| {
                                services.updates.automatic = Some(*on);
                                if *on {
                                    services.check(cx);
                                }
                                cx.notify();
                            });
                        })),
                ),
            );
    }
    vec![
        SettingsStack::titled("Administrator").child(administrator),
        stack,
    ]
}

fn confirm_restart(this: &mut ZflowWindow, window: &mut Window, cx: &mut Context<ZflowWindow>) {
    let services = this.services.clone();
    window.open_alert_dialog(cx, move |alert, _, _| {
        let services = services.clone();
        alert
            .title("Restart and update zflow?")
            .description("Input sharing will stop while zflow updates and restarts. Your settings and paired computers will stay.")
            .button_props(DialogButtonProps::default().ok_text("Restart and update").cancel_text("Later").show_cancel(true))
            .on_ok(move |_, _, cx| {
                services.update(cx, Services::restart_and_update);
                true
            })
    });
}

pub fn this_computer_entries(
    _: &mut ZflowWindow,
    snapshot: &Snapshot,
    cx: &mut Context<ZflowWindow>,
) -> Vec<SettingEntry> {
    let platform = snapshot.platform.clone().unwrap_or_default();
    let addresses = platform.addresses.join(", ");
    let config = snapshot.config_path.clone();
    vec![
        SettingEntry::new(
            "Return here",
            format!(
                "Bring input back to this PC. {} also pauses sharing.",
                "Ctrl+Win+Backspace"
            ),
        )
        .control(
            Button::new("return-here")
                .label("Return here")
                .on(settings_card_ground(cx))
                .on_click(cx.listener(|this, _, _, cx| this.send(Request::ReturnHere, cx))),
        ),
        SettingEntry::new(
            "Network addresses",
            format!(
                "Listening on {}{}",
                platform.listen.unwrap_or_default(),
                if addresses.is_empty() {
                    String::new()
                } else {
                    format!(" · {addresses}")
                }
            ),
        ),
        SettingEntry::new("Configuration", "Open your local settings file.").control(
            Button::new("open-config")
                .label("Open file")
                .on(settings_card_ground(cx))
                .on_click(move |_, _, _| {
                    let _ = Command::new("notepad.exe").arg(&config).spawn();
                }),
        ),
        SettingEntry::new("Input engine", "Restart after editing the configuration.").control(
            Button::new("restart-engine-settings")
                .label("Restart")
                .on(settings_card_ground(cx))
                .on_click(|_, _, cx| start_engine(true, cx)),
        ),
        SettingEntry::new("Quit zflow", "Stop sharing and close the app.").control(
            Button::new("quit")
                .danger()
                .label("Quit")
                .on(settings_card_ground(cx))
                .on_click(|_, _, cx| quit(cx)),
        ),
    ]
}
