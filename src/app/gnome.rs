//! Session service shared by the GNOME panel and GTK settings window.
use super::{
    desktop::DesktopReceiver,
    nearby::{BrowserStatus, NearbyBrowser},
    pairing::Pairing,
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub const BUS: &str = "io.zflow.Desktop";
const PATH: &str = "/io/zflow/Desktop";

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
        remote: Option<std::net::SocketAddr>,
    },
    PairConfirm {
        name: String,
        code: String,
    },
    PairCancel,
    OpenSettings,
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
        use crate::peer_view::{DesktopReply, Request as DaemonRequest};
        match request {
            Request::Snapshot => {
                let daemon = crate::peer_view::request(&DaemonRequest::Status {}).await;
                let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
                let nearby = state.nearby.snapshot();
                let (daemon, error) = match daemon {
                    Ok(DesktopReply::Status(value)) => (Some(value), None),
                    Ok(_) => (
                        None,
                        Some("Unexpected response; update the zflow service".to_owned()),
                    ),
                    Err(error) => (None, Some(format!("{error:#}"))),
                };
                return Ok(serde_json::json!({
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
            Request::Pair { remote } => {
                let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
                ensure!(!state.pairing.active(), "Pairing is already open");
                state.pairing.start(PathBuf::new(), remote)?;
            }
            Request::PairConfirm { name, code } => self
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pairing
                .confirm(name, code)?,
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

fn xdg(variable: &str, fallback: &str) -> Result<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(fallback)))
        .context("HOME is not set")
}

fn autostart_path() -> Result<PathBuf> {
    Ok(xdg("XDG_CONFIG_HOME", ".config")?.join("autostart/io.zflow.desktop-agent.desktop"))
}

fn autostart_enabled() -> Result<bool> {
    match std::fs::read_to_string(autostart_path()?) {
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
    let path = autostart_path()?;
    if enabled {
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            path,
            format!(
                "[Desktop Entry]\nType=Application\nName=zflow\nExec=\"{}\" desktop-agent\nOnlyShowIn=GNOME;\nTerminal=false\n",
                executable()?
            ),
        )?;
    } else if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error.into());
    }
    Ok(())
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
            "app.js",
            include_str!("../../packaging/gnome-extension/app.js"),
        ),
    ] {
        std::fs::write(path.join(name), contents)?;
    }
    Ok(())
}

pub(super) fn install() -> Result<()> {
    let base = xdg("XDG_DATA_HOME", ".local/share")?;
    let apps = base.join("applications");
    let services = base.join("dbus-1/services");
    std::fs::create_dir_all(&apps)?;
    std::fs::create_dir_all(&services)?;
    std::fs::write(
        apps.join("io.zflow.zflow.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName=zflow\nComment=Share your keyboard, pointer, and trackpad\nExec=\"{}\" settings\nIcon=input-mouse\nTerminal=false\nCategories=Settings;GTK;GNOME;\nStartupNotify=true\n",
            executable()?
        ),
    )?;
    // D-Bus service Exec uses shell-style quoting, not Desktop Entry field codes.
    let path = std::env::current_exe()?;
    let path = path.to_str().context("Executable path must be UTF-8")?;
    let quoted = format!("'{}'", path.replace('\'', "'\\''"));
    std::fs::write(
        services.join("io.zflow.Desktop.service"),
        format!("[D-BUS Service]\nName={BUS}\nExec={quoted} desktop-agent\n"),
    )?;
    set_autostart(true)
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
    runtime.block_on(async {
        let connection = zbus::Connection::session().await?;
        let proxy = zbus::fdo::DBusProxy::new(&connection).await?;
        if !proxy.name_has_owner(BUS.try_into()?).await? {
            std::process::Command::new(std::env::current_exe()?)
                .arg("desktop-agent")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn()?;
        }
        Ok::<_, anyhow::Error>(())
    })?;
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
}
