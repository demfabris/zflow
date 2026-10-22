//! zflow's app: one window with the computers and the settings, a menu bar
//! or tray icon on the Mac and Windows, and on the Mac the engine itself.

// No console window on Windows.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod board;
mod copy;
mod lens;
mod model;
mod pages;
mod platform;
mod theme;
#[cfg(not(target_os = "linux"))]
mod tray;
mod widgets;
mod window;

use std::path::PathBuf;

use zz_gpui::{
    App, AppContext as _, Bounds, Entity, QuitMode, SystemNotification, SystemNotificationAction,
    WindowBounds, WindowDecorations, WindowHandle, WindowOptions, px, size,
};
use zz_gpui_kit::{Root, TitleBar};

use crate::{
    model::{Model, Page, Shared},
    window::ZflowWindow,
};

pub const APP_ID: &str = "io.zflow.zflow";
const FORGET: &str = "forget";

/// What the app was started with.
#[derive(Clone, Debug, Default)]
pub struct Arguments {
    pub config: Option<PathBuf>,
    /// Opens on the Settings page.
    pub show_settings: bool,
    /// Starts in the menu bar or tray, without the window.
    pub background: bool,
    /// Asks a running copy to quit, then exits.
    pub quit: bool,
    /// Windows: this copy runs as administrator in place of process
    /// `replace_process`, for user `replace_user`, and sets the event
    /// `restart_ready` once it has taken over.
    pub replace_process: Option<u32>,
    pub replace_user: Option<String>,
    pub restart_ready: Option<String>,
}

impl Arguments {
    fn parse(mut arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut parsed = Self::default();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--config" => {
                    parsed.config = Some(arguments.next().ok_or("--config takes a path")?.into());
                }
                "--show-settings" => parsed.show_settings = true,
                "--background" => parsed.background = true,
                "--quit" => parsed.quit = true,
                "--replace-process" => {
                    let pid = arguments
                        .next()
                        .ok_or("--replace-process takes a process id")?;
                    parsed.replace_process = Some(
                        pid.parse()
                            .map_err(|_| "--replace-process takes a process id")?,
                    );
                }
                "--replace-user" => {
                    parsed.replace_user =
                        Some(arguments.next().ok_or("--replace-user takes an id")?);
                }
                "--restart-ready" => {
                    parsed.restart_ready =
                        Some(arguments.next().ok_or("--restart-ready takes a name")?);
                }
                // macOS adds this to apps it launches from Finder.
                argument if argument.starts_with("-psn_") => {}
                argument => return Err(format!("unknown argument: {argument}")),
            }
        }
        Ok(parsed)
    }
}

fn main() {
    // Velopack calls the app with its own arguments around installs and
    // updates, and ends the process once its hooks ran.
    #[cfg(target_os = "windows")]
    platform::velopack_hooks();
    let arguments = match Arguments::parse(std::env::args().skip(1)) {
        Ok(arguments) => arguments,
        Err(error) => {
            eprintln!("zflow-app: {error}");
            std::process::exit(2);
        }
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,zflow=info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
    if !platform::before_start(&arguments) {
        return;
    }
    let config = arguments
        .config
        .clone()
        .unwrap_or_else(platform::default_config);
    let application = zz_gpui_platform::application();
    // Opening zflow again while it runs, as from Applications, shows the window.
    #[cfg(target_os = "macos")]
    application.on_reopen(open);
    application
        .with_assets(zz_gpui_kit::Assets)
        .with_quit_mode(if platform::LIVES_IN_TRAY {
            QuitMode::Explicit
        } else {
            QuitMode::LastWindowClosed
        })
        .run(move |cx: &mut App| {
            cx.set_app_identity(APP_ID, "zflow");
            theme::init(cx);
            let model = cx.new(|cx| Model::new(config, cx));
            cx.set_global(Shared(model.clone()));
            if arguments.show_settings {
                model.update(cx, |model, cx| model.show(Page::Settings, cx));
            }
            let services = platform::init(cx);
            notify_notices(&model, cx);
            // Each system decides whether the window opens at start.
            platform::start(&model, &services, &arguments, cx);
        });
}

/// Shows the window, opening it if it is closed.
pub fn open(cx: &mut App) {
    if let Some(window) = cx.try_global::<MainWindow>().and_then(|window| window.0)
        && window
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        platform::window_shown(cx);
        cx.activate(true);
        return;
    }
    let model = model::model(cx);
    let Some(services) = cx
        .try_global::<platform::SharedServices>()
        .map(|services| services.0.clone())
    else {
        return;
    };
    let bounds = Bounds::centered(None, size(px(960.0), px(680.0)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitleBar::title_bar_options(
                zz_gpui_kit::Theme::global(cx).title_bar_height(),
            )),
            window_min_size: Some(size(px(700.0), px(480.0))),
            // X11 window managers frame their windows themselves.
            window_decorations: Some(if cx.compositor_name() == "X11" {
                WindowDecorations::Server
            } else {
                WindowDecorations::Client
            }),
            app_id: Some(APP_ID.into()),
            ..Default::default()
        },
        |window, cx| {
            theme::apply(Some(window), cx);
            window.set_window_title("zflow");
            window.on_window_should_close(cx, |_, cx| platform::may_close(cx));
            let view = cx.new(|cx| ZflowWindow::new(model, services, window, cx));
            cx.new(|cx| Root::new(view, window, cx).bordered(cfg!(target_os = "linux")))
        },
    );
    match opened {
        Ok(handle) => {
            cx.set_global(MainWindow(Some(handle)));
            platform::window_shown(cx);
            cx.activate(true);
        }
        Err(error) => log::error!("zflow could not open its window: {error:#}"),
    }
}

/// The open window, if any.
pub struct MainWindow(pub Option<WindowHandle<Root>>);

impl zz_gpui::Global for MainWindow {}

/// Tells people once when a computer joins, with a Forget button for one
/// that is not theirs.
fn notify_notices(model: &Entity<Model>, cx: &mut App) {
    cx.observe(model, |model, cx| {
        let notices = model.update(cx, |model, _| model.take_notices());
        for notice in notices {
            cx.show_system_notification(SystemNotification {
                tag: format!("joined:{}", notice.name).into(),
                title: format!("{} joined", notice.name).into(),
                body: format!(
                    "Its mark is {}. It can share {}’s keyboard and mouse. Not yours?",
                    notice.mark,
                    copy::THIS_COMPUTER_LOWER
                )
                .into(),
                actions: vec![SystemNotificationAction {
                    id: FORGET.into(),
                    label: "Forget".into(),
                }],
            });
        }
    })
    .detach();
    cx.on_system_notification_response(|response, cx| {
        if let Some(name) = response.tag.strip_prefix("joined:") {
            let model = model::model(cx);
            if response.action_id.as_deref() == Some(FORGET) {
                model.read(cx).send(zflow::app::api::Request::Forget {
                    name: name.to_owned(),
                });
            } else {
                model.update(cx, |model, cx| {
                    model.show(Page::Computer(name.to_owned()), cx);
                });
                open(cx);
            }
        } else {
            platform::notification_response(&response, cx);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_read_as_the_native_apps_took_them() {
        let parse = |words: &[&str]| Arguments::parse(words.iter().map(|word| (*word).to_owned()));
        let parsed = parse(&["--config", "/tmp/z.toml", "--show-settings"]).unwrap();
        assert_eq!(parsed.config, Some(PathBuf::from("/tmp/z.toml")));
        assert!(parsed.show_settings && !parsed.background && !parsed.quit);
        assert!(parse(&["--background", "-psn_0_1234"]).unwrap().background);
        assert!(parse(&["--config"]).is_err());
        assert!(parse(&["--sparkle"]).is_err());
    }
}
