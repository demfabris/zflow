//! Session service shared by the GNOME panel and GTK settings window.
use super::{
    api::{self, Action, Health, Level, Peer, PeerState, Request, Shortcut, Status},
    desktop::DesktopReceiver,
    nearby::{BrowserStatus, NearbyBrowser},
    pairing::Pairing,
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub const BUS: &str = "io.zflow.Desktop";
const PATH: &str = "/io/zflow/Desktop";
/// packaging/gnome-extension/client.js mirrors this. Raise both when the agent
/// and the extension stop understanding each other.
pub(super) const API: u32 = 2;
const AUTOSTART: &str = "autostart/io.zflow.desktop-agent.desktop";
/// Where the package keeps the service's settings.
const CONFIG_PATH: &str = "/etc/zflow/zflow.toml";

#[derive(Default)]
pub(super) struct State {
    pub receiver: DesktopReceiver,
    pub nearby: NearbyBrowser,
    pub pairing: Pairing,
}

pub(super) struct Service(pub Arc<Mutex<State>>);

#[zbus::interface(name = "io.zflow.Desktop")]
impl Service {
    async fn call(&self, json: &str) -> zbus::fdo::Result<String> {
        self.dispatch(json)
            .await
            .map(|v| v.to_string())
            .map_err(|error| zbus::fdo::Error::Failed(format!("{error:#}")))
    }
}

impl Service {
    async fn dispatch(&self, json: &str) -> Result<serde_json::Value> {
        ensure!(json.len() <= 4096, "Desktop request exceeds limit");
        let request: Request = serde_json::from_str(json)?;
        use crate::peer_view::Request as DaemonRequest;
        match request {
            Request::Snapshot => {
                let daemon = crate::peer_view::status().await;
                let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
                let mut value = serde_json::to_value(snapshot(daemon, &state)?)?;
                // The panel reads the level before anything else.
                value["api"] = API.into();
                return Ok(value);
            }
            Request::SetSharing { enabled } => {
                crate::peer_view::request(&DaemonRequest::SetSharing { enabled }).await?;
            }
            Request::Forget { name } => {
                crate::peer_view::request(&DaemonRequest::Forget { name }).await?;
            }
            Request::SetPeer {
                name,
                allow_control,
                keyboard,
                reverse_scroll,
            } => {
                crate::peer_view::request(&DaemonRequest::SetPeer {
                    name,
                    allow_control,
                    keyboard,
                    reverse_scroll,
                })
                .await?;
            }
            Request::MoveTile {
                id,
                x,
                y,
                tolerance,
            } => {
                crate::peer_view::request(&DaemonRequest::MoveTile {
                    id,
                    x,
                    y,
                    tolerance,
                })
                .await?;
            }
            Request::SetAutostart { enabled } => set_autostart(enabled)?,
            Request::SetSwitching { pause_at_edges } => {
                crate::peer_view::request(&DaemonRequest::SetSwitching { pause_at_edges }).await?;
            }
            Request::SetClipboard { share } => {
                crate::peer_view::request(&DaemonRequest::SetClipboard { share }).await?;
            }
            Request::Pair { address, code } => {
                let remote = address
                    .as_deref()
                    .map(crate::pairing::parse_pairing_address)
                    .transpose()?;
                let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
                ensure!(!state.pairing.active(), "Pairing is already open");
                state.pairing.start(PathBuf::new(), remote, code)?;
            }
            Request::PairRespond { allow } => self
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pairing
                .respond(allow)?,
            Request::PairCancel => self
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pairing
                .cancel(),
            Request::OpenSettings => {
                let mut child = tokio::process::Command::new(std::env::current_exe()?)
                    .arg("settings")
                    .spawn()?;
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
            }
            Request::OpenLogs => {
                let mut child = log_viewers()
                    .iter()
                    .find_map(|(program, arguments)| {
                        tokio::process::Command::new(program)
                            .args(*arguments)
                            .spawn()
                            .ok()
                    })
                    .context("Install a terminal or GNOME Logs to read zflow's log")?;
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
            }
            Request::InstallExtension => {
                let connection = zbus::Connection::session().await?;
                let running = super::desktop::install_extension(Some(&connection)).await?;
                return Ok(serde_json::json!({"ok": true, "running": running}));
            }
            Request::Retry => {
                crate::peer_view::request(&DaemonRequest::Retry {}).await?;
            }
            Request::Reload
            | Request::SetAwdl { .. }
            | Request::HelperReady { .. }
            | Request::AllowAccessibility
            | Request::CheckAccessibility
            | Request::Discover
            | Request::Place { .. }
            | Request::AddAddress { .. } => bail!("Not available on this computer"),
        }
        Ok(serde_json::json!({"ok": true}))
    }
}

/// What the settings window and the panel show. Linux has no rows of its
/// own under the shared ones yet.
fn snapshot(
    daemon: Result<crate::peer_view::DesktopStatus>,
    state: &State,
) -> Result<api::Snapshot<()>> {
    let nearby = state.nearby.snapshot();
    let mut health = Vec::new();
    let (sharing, peers, shortcuts, layout) = match &daemon {
        Ok(daemon) => {
            let peers = peers(daemon);
            health.push(Health::new(
                "service",
                Level::Ok,
                "Background service",
                "Running",
            ));
            let ready = state.receiver.is_ready();
            health.push(Health::new(
                "desktop",
                if ready || !daemon.sharing {
                    Level::Ok
                } else {
                    Level::Error
                },
                "GNOME desktop",
                state.receiver.status(),
            ));
            if daemon.sharing {
                health.extend(link_health(&peers, &daemon.links));
            }
            (
                Some(daemon.sharing),
                peers,
                shortcuts(daemon),
                daemon.layout.clone(),
            )
        }
        Err(error) => {
            health.push(Health::new(
                "service",
                Level::Error,
                "Background service",
                format!("{error:#}"),
            ));
            (None, Vec::new(), Vec::new(), None)
        }
    };
    if let BrowserStatus::Failed(error) = nearby.status {
        health.push(Health::new(
            "discovery",
            Level::Warning,
            "Nearby computers",
            error,
        ));
    }
    let connected = peers.iter().any(|peer| {
        matches!(
            peer.state,
            PeerState::Connected | PeerState::ControllingThis | PeerState::ControlledFromHere
        )
    });
    let checking = !connected && peers.iter().any(|peer| peer.state == PeerState::Connecting);
    Ok(api::Snapshot {
        status: Status::new(sharing, &peers, &health, checking),
        sharing,
        health,
        layout,
        peers,
        pairing: state.pairing.snapshot(),
        nearby: nearby.records.into_values().collect(),
        pause_at_edges: daemon.as_ref().ok().map(|daemon| daemon.pause_at_edges),
        shortcuts,
        share_clipboard: daemon.as_ref().ok().map(|daemon| daemon.share_clipboard),
        autostart: Some(autostart_enabled()?),
        config_path: CONFIG_PATH.into(),
        // Filled in once the service says hello and keeps a shelf.
        pairing_window: Default::default(),
        own_mark: None,
        unplaced: Vec::new(),
        notices: Vec::new(),
        platform: (),
    })
}

/// Ways to show the service's log, best first: the default terminal
/// following it, Debian's terminal alternative, then GNOME Logs.
fn log_viewers() -> [(&'static str, &'static [&'static str]); 3] {
    const FOLLOW: &[&str] = &["journalctl", "--unit=zflowd.service", "--follow"];
    [
        ("xdg-terminal-exec", FOLLOW),
        (
            "x-terminal-emulator",
            &["-e", "journalctl", "--unit=zflowd.service", "--follow"],
        ),
        ("gnome-logs", &[]),
    ]
}

fn peers(daemon: &crate::peer_view::DesktopStatus) -> Vec<Peer> {
    use crate::peer_view::LinkStatus;
    let is = |peer: &Option<String>, name: &str| peer.as_deref() == Some(name);
    daemon
        .peers
        .iter()
        .map(|(name, record)| {
            let (state, reason) = if is(&daemon.receiving_from, name) {
                (PeerState::ControllingThis, None)
            } else if is(&daemon.sending_to, name) {
                (PeerState::ControlledFromHere, None)
            } else if daemon.connected.contains(name) {
                (PeerState::Connected, None)
            } else {
                match daemon.links.get(name) {
                    Some(LinkStatus::Connecting) => (PeerState::Connecting, None),
                    Some(LinkStatus::Unreachable { reason, .. }) => {
                        (PeerState::Unreachable, Some(reason))
                    }
                    // Not dialed: nothing listens where it was paired, as with a Mac.
                    None => (PeerState::Paired, None),
                }
            };
            let mut peer = Peer::new(name, record, state);
            if let Some(reason) = reason {
                peer.detail = reason.clone();
            }
            peer
        })
        .collect()
}

/// One row for the links to paired computers, with Retry while one cannot
/// be reached. Each computer's own row says why. A computer that is asleep
/// or away only warns; one a person has to update or pair again needs
/// attention.
fn link_health(
    peers: &[Peer],
    links: &std::collections::BTreeMap<String, crate::peer_view::LinkStatus>,
) -> Option<Health> {
    use crate::peer_view::LinkStatus;
    if peers.is_empty() {
        return None;
    }
    let down: Vec<_> = peers
        .iter()
        .filter(|peer| peer.state == PeerState::Unreachable)
        .map(|peer| peer.name.as_str())
        .collect();
    let needs_fix = down.iter().any(|name| {
        matches!(
            links.get(*name),
            Some(LinkStatus::Unreachable {
                needs_fix: true,
                ..
            })
        )
    });
    let title = "Paired computers";
    Some(if !down.is_empty() {
        Health {
            action: Some(Action {
                label: "Retry".into(),
                command: "retry".into(),
            }),
            ..Health::new(
                "computers",
                if needs_fix {
                    Level::Error
                } else {
                    Level::Warning
                },
                title,
                format!("Cannot connect to {}", down.join(", ")),
            )
        }
    } else if peers.iter().any(|peer| peer.state == PeerState::Connecting) {
        Health::new("computers", Level::Ok, title, "Checking…")
    } else {
        Health::new("computers", Level::Ok, title, "No problems found.")
    })
}

fn shortcuts(daemon: &crate::peer_view::DesktopStatus) -> Vec<Shortcut> {
    [
        ("Switch to the other computer", &daemon.activation_chord),
        ("Return input to this computer", &daemon.escape_chord),
    ]
    .into_iter()
    .map(|(title, keys)| Shortcut {
        title: title.into(),
        keys: chord_label(keys),
    })
    .collect()
}

/// Evdev key names as people read them: KEY_LEFTMETA is the Super key.
fn chord_label(keys: &[String]) -> String {
    keys.iter()
        .map(|key| {
            let key = key.trim_start_matches("KEY_");
            let key = key
                .strip_prefix("LEFT")
                .or_else(|| key.strip_prefix("RIGHT"))
                .unwrap_or(key);
            match key {
                "CTRL" => "Ctrl".into(),
                "META" => "Super".into(),
                "ALT" => "Alt".into(),
                "SHIFT" => "Shift".into(),
                key => {
                    let mut chars = key.chars();
                    chars.next().map_or_else(String::new, |first| {
                        first.to_string() + &chars.as_str().to_lowercase()
                    })
                }
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

pub(super) async fn connect(state: Arc<Mutex<State>>) -> Result<zbus::Connection> {
    let connection = zbus::connection::Builder::session()?
        .serve_at(PATH, Service(state))?
        .build()
        .await?;
    connection
        .request_name_with_flags(BUS, zbus::fdo::RequestNameFlags::DoNotQueue.into())
        .await
        .context("The zflow desktop agent is already running")?;
    Ok(connection)
}

pub(super) fn xdg(variable: &str, fallback: &str) -> Result<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(fallback)))
        .context("HOME is not set")
}

/// The directories in an XDG search path such as XDG_DATA_DIRS.
pub(super) fn xdg_dirs(variable: &str, fallback: &str) -> Vec<PathBuf> {
    let value = std::env::var_os(variable).filter(|value| !value.is_empty());
    std::env::split_paths(value.as_deref().unwrap_or(fallback.as_ref()))
        .filter(|path| path.is_absolute())
        .collect()
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

fn entry_enabled(path: &Path) -> Result<bool> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(!text.lines().any(|line| {
            matches!(
                line.trim(),
                "Hidden=true" | "X-GNOME-Autostart-enabled=false"
            )
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// The package starts the agent for every GNOME user from a system autostart
/// entry. A user entry with the same name replaces it, even a hidden one.
struct Autostart {
    user: PathBuf,
    system: Option<PathBuf>,
}

impl Autostart {
    fn find() -> Result<Self> {
        Ok(Self {
            user: xdg("XDG_CONFIG_HOME", ".config")?.join(AUTOSTART),
            system: xdg_dirs("XDG_CONFIG_DIRS", "/etc/xdg")
                .into_iter()
                .map(|dir| dir.join(AUTOSTART))
                .find(|path| path.is_file()),
        })
    }

    fn enabled(&self) -> Result<bool> {
        if self.user.symlink_metadata().is_ok() {
            return entry_enabled(&self.user);
        }
        self.system.as_deref().map_or(Ok(false), entry_enabled)
    }

    fn set(&self, enabled: bool) -> Result<()> {
        let system = match &self.system {
            Some(path) => entry_enabled(path)?,
            None => false,
        };
        if enabled == system {
            return remove_if_present(&self.user);
        }
        // Hidden=true is how the XDG autostart spec turns a system entry off.
        std::fs::create_dir_all(self.user.parent().unwrap())?;
        std::fs::write(
            &self.user,
            format!(
                "[Desktop Entry]\nType=Application\nName=zflow\nExec=\"{}\" desktop-agent\nOnlyShowIn=GNOME;\nTerminal=false\n{}",
                executable()?,
                if enabled { "" } else { "Hidden=true\n" }
            ),
        )?;
        Ok(())
    }
}

fn autostart_enabled() -> Result<bool> {
    Autostart::find()?.enabled()
}

fn executable() -> Result<String> {
    let path = std::env::current_exe()?;
    let path = path.to_str().context("Executable path must be UTF-8")?;
    ensure!(
        !path.chars().any(char::is_control),
        "Invalid executable path"
    );
    // Desktop Entry string escaping happens before Exec argument unquoting.
    Ok(path
        .replace('\\', "\\\\\\\\")
        .replace('"', "\\\\\"")
        .replace('`', "\\\\`")
        .replace('$', "\\\\$")
        .replace('%', "%%"))
}

pub(super) fn set_autostart(enabled: bool) -> Result<()> {
    Autostart::find()?.set(enabled)
}

pub(super) fn write_assets(path: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    for (name, contents) in [
        (
            "client.js",
            include_str!("../../packaging/gnome-extension/client.js"),
        ),
        (
            "settings.js",
            include_str!("../../packaging/gnome-extension/settings.js"),
        ),
        (
            "setup.js",
            include_str!("../../packaging/gnome-extension/setup.js"),
        ),
        (
            "app.js",
            include_str!("../../packaging/gnome-extension/app.js"),
        ),
    ] {
        std::fs::write(path.join(name), contents)?;
    }
    Ok(())
}

/// Moves this account to the launcher, D-Bus activation file and autostart
/// entry that the package installs for every user. Older versions wrote
/// copies into the account, and those would shadow the system files.
pub(super) fn install() -> Result<()> {
    let data = xdg("XDG_DATA_HOME", ".local/share")?;
    let system = xdg_dirs("XDG_DATA_DIRS", "/usr/local/share:/usr/share");
    for name in [
        "applications/io.zflow.zflow.desktop",
        "dbus-1/services/io.zflow.Desktop.service",
    ] {
        if system.iter().any(|dir| dir.join(name).is_file()) {
            remove_if_present(&data.join(name))?;
        }
    }
    // Older versions turned Start at Login on here; keep it off only if the
    // user turned it off since.
    let autostart = Autostart::find()?;
    if autostart.user.symlink_metadata().is_err() || autostart.enabled()? {
        autostart.set(true)?;
    }
    Ok(())
}

/// Starts the desktop agent in this login session. With `replace`, a running
/// agent stops first, so the binary an update just installed takes over.
pub(super) async fn start_agent(connection: &zbus::Connection, replace: bool) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    if let Ok(owner) = bus.get_name_owner(BUS.try_into()?).await {
        if !replace {
            return Ok(());
        }
        let pid = bus
            .get_connection_unix_process_id(owner.into_inner().into())
            .await?;
        // Stop only the agent itself, which exits cleanly on SIGTERM.
        let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if name.trim() != "zflow" {
            return Ok(());
        }
        std::process::Command::new("kill")
            .arg(pid.to_string())
            .status()?;
        for _ in 0..50 {
            if !bus.name_has_owner(BUS.try_into()?).await? {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    // D-Bus activation runs the agent outside the caller's terminal and logs
    // to the journal. The bus may not have read a service file installed
    // moments ago until it reloads.
    let _ = bus.reload_config().await;
    if let Err(error) = bus.start_service_by_name(BUS.try_into()?, 0).await {
        tracing::debug!(%error, "D-Bus could not start the desktop agent");
        std::process::Command::new(std::env::current_exe()?)
            .arg("desktop-agent")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .process_group(0)
            .spawn()?;
    }
    Ok(())
}

pub fn settings() -> Result<()> {
    use std::os::unix::process::CommandExt;
    ensure!(
        !nix::unistd::Uid::effective().is_root(),
        "Open settings as your desktop user, without sudo"
    );
    let path = xdg("XDG_CACHE_HOME", ".cache")?.join("zflow/desktop");
    write_assets(&path)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async { start_agent(&zbus::Connection::session().await?, false).await })?;
    drop(runtime);
    Err(std::process::Command::new("gjs")
        .arg("-m")
        .arg(path.join("app.js"))
        .exec())
    .context("Install GJS, GTK4 and libadwaita to open zflow settings")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires a private bus: dbus-run-session -- cargo test app::gnome::tests -- --ignored"]
    async fn session_service_has_one_owner_and_rejects_extra_authority() {
        let connection = connect(Arc::new(Mutex::new(State::default())))
            .await
            .unwrap();
        assert!(
            connect(Arc::new(Mutex::new(State::default())))
                .await
                .is_err()
        );
        let client = zbus::Connection::session().await.unwrap();
        let proxy = zbus::Proxy::new(&client, BUS, PATH, BUS).await.unwrap();
        let result: zbus::Result<String> = proxy.call("Call", &(r#"{"command":"set_sharing","enabled":true,"permissions":{"inject_prelogin":true}}"#,)).await;
        assert!(result.is_err());
        let reply: String = proxy
            .call("Call", &(r#"{"command":"pair_cancel"}"#,))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reply).unwrap()["ok"],
            true
        );
        drop(connection);
    }

    #[test]
    fn the_service_state_becomes_peer_and_health_rows() {
        use crate::peer_view::LinkStatus;
        let mut config = crate::config::Config::default();
        for name in ["desk", "laptop", "mac", "new", "old"] {
            config.peers.insert(
                name.into(),
                crate::config::PeerConfig {
                    spki_der_hex: "01".into(),
                    addresses: Vec::new(),
                    permissions: crate::config::PeerPermissions {
                        connect: true,
                        send_normal: name != "old",
                        receive_normal: true,
                        inject_prelogin: false,
                    },
                    keyboard: crate::core::KeyboardMode::Standard,
                    reverse_scroll: false,
                },
            );
        }
        let reinstalled = "Reset or reinstalled. Pair it again.";
        let status = crate::peer_view::DesktopStatus {
            receiving_from: Some("mac".into()),
            connected: vec!["desk".into(), "mac".into()],
            links: [
                (
                    "laptop".into(),
                    LinkStatus::Unreachable {
                        reason: reinstalled.into(),
                        needs_fix: true,
                    },
                ),
                ("new".into(), LinkStatus::Connecting),
                // A session wins over a stale link state.
                ("desk".into(), LinkStatus::Connecting),
            ]
            .into(),
            ..crate::peer_view::DesktopStatus::from_config(&config)
        };
        let rows: Vec<_> = peers(&status)
            .into_iter()
            .map(|peer| (peer.name, peer.state, peer.detail, peer.allow_control))
            .collect();
        let row = |name: &str, state, detail: &str, control| {
            (name.to_owned(), state, detail.to_owned(), control)
        };
        assert_eq!(
            rows,
            [
                row("desk", PeerState::Connected, "Connected", true),
                row("laptop", PeerState::Unreachable, reinstalled, true),
                row(
                    "mac",
                    PeerState::ControllingThis,
                    "Controlling this computer",
                    true
                ),
                row("new", PeerState::Connecting, "Connecting…", true),
                row("old", PeerState::Paired, "Paired", false),
            ]
        );
        let health = link_health(&peers(&status), &status.links).unwrap();
        assert_eq!(
            (health.level, health.detail.as_str()),
            (Level::Error, "Cannot connect to laptop")
        );
        assert_eq!(health.action.unwrap().command, "retry");
        // A computer that is only asleep or away warns and keeps retrying.
        let mut asleep = status.links.clone();
        asleep.insert(
            "laptop".into(),
            LinkStatus::Unreachable {
                reason: "input connection to 192.0.2.7:43119 timed out".into(),
                needs_fix: false,
            },
        );
        let health = link_health(&peers(&status), &asleep).unwrap();
        assert_eq!(health.level, Level::Warning);
        assert!(health.action.is_some());
        assert_eq!(
            shortcuts(&status)[1].keys,
            "Ctrl+Super+Backspace",
            "the default escape chord"
        );
        // The settings window arranges the layout the service keeps.
        let layout = crate::app::layout_model::Layout {
            monitors: vec![crate::app::layout_model::Monitor {
                id: "local".into(),
                label: "This computer".into(),
                peer: None,
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
            }],
        };
        let up = snapshot(
            Ok(crate::peer_view::DesktopStatus {
                layout: Some(layout.clone()),
                share_clipboard: true,
                ..status
            }),
            &State::default(),
        )
        .unwrap();
        assert_eq!(up.layout, Some(layout));
        assert_eq!(up.share_clipboard, Some(true));
        // While sharing, a computer that cannot be reached is a problem.
        assert!(
            up.health
                .iter()
                .any(|row| row.id == "computers" && row.level == Level::Error)
        );

        let down = snapshot(
            Err(anyhow::anyhow!("Start the zflow system service")),
            &State::default(),
        )
        .unwrap();
        assert_eq!(down.sharing, None);
        assert_eq!(down.status.state, api::State::Attention);
        assert_eq!(down.health[0].level, Level::Error);
        assert_eq!(down.health[0].detail, "Start the zflow system service");
        assert!(down.peers.is_empty() && down.shortcuts.is_empty() && down.layout.is_none());
        assert_eq!(down.share_clipboard, None);
    }

    #[test]
    fn chords_read_like_keys() {
        let keys = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            chord_label(&keys(&["KEY_LEFTCTRL", "KEY_LEFTMETA", "KEY_F12"])),
            "Ctrl+Super+F12"
        );
        assert_eq!(
            chord_label(&keys(&["KEY_RIGHTALT", "KEY_LEFTSHIFT", "KEY_BACKSPACE"])),
            "Alt+Shift+Backspace"
        );
    }

    #[test]
    fn start_at_login_overrides_the_system_entry_instead_of_deleting_it() {
        let temp = tempfile::tempdir().unwrap();
        let system = temp.path().join("etc/autostart.desktop");
        std::fs::create_dir_all(system.parent().unwrap()).unwrap();
        std::fs::write(&system, "[Desktop Entry]\nExec=zflow desktop-agent\n").unwrap();
        let autostart = Autostart {
            user: temp
                .path()
                .join("home/autostart/io.zflow.desktop-agent.desktop"),
            system: Some(system),
        };
        assert!(autostart.enabled().unwrap(), "the package turns it on");
        autostart.set(false).unwrap();
        assert!(!autostart.enabled().unwrap());
        let text = std::fs::read_to_string(&autostart.user).unwrap();
        assert!(text.contains("\nHidden=true\n"), "{text}");
        autostart.set(true).unwrap();
        assert!(autostart.enabled().unwrap());
        assert!(!autostart.user.exists(), "the system entry applies again");

        // Without a system entry, a user entry is the only way to start.
        let autostart = Autostart {
            system: None,
            ..autostart
        };
        assert!(!autostart.enabled().unwrap());
        autostart.set(true).unwrap();
        assert!(autostart.enabled().unwrap());
        assert!(
            !std::fs::read_to_string(&autostart.user)
                .unwrap()
                .contains("Hidden")
        );
        autostart.set(false).unwrap();
        assert!(!autostart.user.exists());
    }
}
