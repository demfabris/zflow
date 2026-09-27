//! Session service shared by the GNOME panel and GTK settings window.
use super::{
    desktop::DesktopReceiver,
    nearby::{BrowserStatus, NearbyBrowser},
    pairing::Pairing,
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub const BUS: &str = "io.zflow.Desktop";
const PATH: &str = "/io/zflow/Desktop";
/// packaging/gnome-extension/client.js mirrors this. Raise both when the agent
/// and the extension stop understanding each other.
pub(super) const API: u32 = 1;
const AUTOSTART: &str = "autostart/io.zflow.desktop-agent.desktop";

#[derive(Default)]
pub(super) struct State {
    pub receiver: DesktopReceiver,
    pub nearby: NearbyBrowser,
    pub pairing: Pairing,
}

pub(super) struct Service(pub Arc<Mutex<State>>);

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Snapshot,
    SetSharing {
        enabled: bool,
    },
    SetAutostart {
        enabled: bool,
    },
    Forget {
        name: String,
    },
    Pair {
        /// Absent to listen; otherwise an IP address, with the port optional.
        remote: Option<String>,
        /// The code shown on the other computer, when connecting.
        code: Option<String>,
    },
    PairRespond {
        allow: bool,
    },
    PairCancel,
    OpenSettings,
    InstallExtension,
}

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
                let nearby = state.nearby.snapshot();
                let (daemon, error) = match daemon {
                    Ok(value) => (Some(value), None),
                    Err(error) => (None, Some(format!("{error:#}"))),
                };
                return Ok(serde_json::json!({
                    "api": API,
                    "daemon": daemon, "error": error,
                    "desktop": state.receiver.status(),
                    "desktop_ready": state.receiver.is_ready(),
                    "pairing": state.pairing.snapshot(),
                    "nearby": nearby.records.values().collect::<Vec<_>>(),
                    "discovery_error": match nearby.status { BrowserStatus::Failed(error) => Some(error), _ => None },
                    "autostart": autostart_enabled()?,
                }));
            }
            Request::SetSharing { enabled } => {
                crate::peer_view::request(&DaemonRequest::SetSharing { enabled }).await?;
            }
            Request::Forget { name } => {
                crate::peer_view::request(&DaemonRequest::Forget { name }).await?;
            }
            Request::SetAutostart { enabled } => set_autostart(enabled)?,
            Request::Pair { remote, code } => {
                let remote = remote
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
            Request::InstallExtension => {
                let connection = zbus::Connection::session().await?;
                let running = super::desktop::install_extension(Some(&connection)).await?;
                return Ok(serde_json::json!({"ok": true, "running": running}));
            }
        }
        Ok(serde_json::json!({"ok": true}))
    }
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
