//! User-session connection between the daemon and GNOME's desktop integration.

use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Default)]
struct State {
    ready: bool,
    message: String,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// A task on the agent's runtime, which outlives anything zbus spawns for it.
/// The agent runs on one thread, so an aborted task never writes state again.
#[derive(Default)]
pub struct DesktopReceiver {
    state: Arc<Mutex<State>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl DesktopReceiver {
    pub fn is_active(&self) -> bool {
        self.task.as_ref().is_some_and(|task| !task.is_finished())
    }
    pub fn is_ready(&self) -> bool {
        lock(&self.state).ready
    }
    pub fn status(&self) -> String {
        lock(&self.state).message.clone()
    }
    /// `connection` must own the agent's bus name; the extension answers no
    /// other caller.
    pub fn start(&mut self, connection: &zbus::Connection) {
        if self.is_active() {
            return;
        }
        tracing::debug!("starting desktop agent");
        let connection = connection.clone();
        let state = self.state.clone();
        *lock(&state) = State {
            ready: false,
            message: "Connecting to the local GNOME desktop…".into(),
        };
        self.task = Some(tokio::spawn(async move {
            let result = run(&connection, &state).await;
            if let Err(error) = &result {
                tracing::warn!(error = %format_args!("{error:#}"), "desktop agent stopped");
            }
            *lock(&state) = State {
                ready: false,
                message: match result {
                    Ok(()) => "Receiving stopped".into(),
                    Err(error) => format!("{error:#}"),
                },
            };
        }));
    }
    pub fn stop(&mut self) {
        tracing::debug!("stopping desktop agent");
        if let Some(task) = self.task.take() {
            task.abort();
        }
        *lock(&self.state) = State {
            ready: false,
            message: "Receiving stopped".into(),
        };
    }
}

impl Drop for DesktopReceiver {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(target_os = "linux")]
async fn run(connection: &zbus::Connection, state: &Mutex<State>) -> anyhow::Result<()> {
    use crate::desktop::{DesktopRequest, DesktopResponse};
    use anyhow::{Context, ensure};
    const ENABLE: &str = "Enable the zflow GNOME integration; a newly installed extension may require logging out and back in";
    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    // Login can start this agent before Shell enables the extension. Wait for
    // its name quietly instead of failing or asking D-Bus to start it.
    let mut waiting = false;
    let owner = loop {
        match bus
            .get_name_owner(crate::desktop::BUS_NAME.try_into()?)
            .await
        {
            Ok(owner) => break owner,
            Err(zbus::fdo::Error::NameHasNoOwner(_)) if !waiting => {
                tracing::debug!("waiting for the zflow GNOME extension");
                lock(state).message = ENABLE.into();
                waiting = true;
            }
            Err(zbus::fdo::Error::NameHasNoOwner(_)) => {}
            Err(error) => return Err(error.into()),
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    };
    // Trust replies only from GNOME Shell, and keep talking to that connection
    // even if another program takes the name later.
    ensure!(
        owner == bus.get_name_owner("org.gnome.Shell".try_into()?).await?,
        "Another program owns the zflow GNOME integration name"
    );
    let proxy = zbus::Proxy::new(
        connection,
        owner.clone(),
        crate::desktop::OBJECT_PATH,
        crate::desktop::BUS_NAME,
    )
    .await?;
    let snapshot = call(&proxy, &DesktopRequest::Snapshot)
        .await
        .context(ENABLE)?;
    if let DesktopResponse::Unavailable { reason } = snapshot {
        anyhow::bail!("{reason}");
    }
    ensure!(
        matches!(snapshot, DesktopResponse::Snapshot { .. }),
        "Unexpected GNOME response"
    );
    let daemon_uid = nix::unistd::User::from_name("zflow")?
        .context("Install the zflow service first")?
        .uid
        .as_raw();
    let mut stream = tokio::net::UnixStream::connect(crate::peer_view::SOCKET_PATH)
        .await
        .context("Start the installed zflow service first")?;
    let uid = crate::control::peer_uid(&stream)?;
    ensure!(
        uid == daemon_uid || uid == 0,
        "The desktop API is not owned by the zflow service"
    );
    crate::control::write_message(&mut stream, &crate::peer_view::Request::Desktop {}).await?;
    let ready: DesktopResponse = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        crate::control::read_message(&mut stream),
    )
    .await??;
    ensure!(
        matches!(ready, DesktopResponse::Finished),
        "The service denied the desktop connection"
    );
    *lock(state) = State {
        ready: true,
        message: "Ready to receive through the GNOME desktop".into(),
    };
    tracing::info!("desktop agent ready");

    loop {
        let request: DesktopRequest = crate::control::read_message(&mut stream).await?;
        request.validate()?;
        let response = match call(&proxy, &request).await {
            Ok(response) => response,
            Err(error) => {
                // A restarted Shell has a new unique name; start over to find and check it.
                ensure!(
                    bus.name_has_owner((&owner).into()).await?,
                    "GNOME Shell restarted"
                );
                DesktopResponse::unavailable(format!("GNOME integration unavailable: {error}"))
            }
        };
        crate::control::write_message(&mut stream, &response).await?;
    }
}

#[cfg(target_os = "linux")]
async fn call(
    proxy: &zbus::Proxy<'_>,
    request: &crate::desktop::DesktopRequest,
) -> anyhow::Result<crate::desktop::DesktopResponse> {
    let started = std::time::Instant::now();
    let operation = crate::session::desktop_operation(request);
    tracing::trace!(operation, "GNOME desktop RPC started");
    let result = async {
        // The extension refuses an agent from another API level with "Update zflow".
        let mut json = serde_json::to_value(request)?;
        json["api"] = super::gnome::API.into();
        let json = json.to_string();
        let response: String = tokio::time::timeout(
            std::time::Duration::from_millis(450),
            proxy.call("Call", &(json,)),
        )
        .await??;
        anyhow::ensure!(
            response.len() <= crate::desktop::MAX_MESSAGE_BYTES,
            "GNOME response exceeded limit"
        );
        let response: crate::desktop::DesktopResponse = serde_json::from_str(&response)?;
        response.validate()?;
        Ok::<_, anyhow::Error>(response)
    }
    .await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match &result {
        Ok(response) => {
            let outcome = crate::session::desktop_response_kind(response);
            if operation != "poll"
                || elapsed_ms >= crate::desktop::POLL_HOLD_MS + 150
                || outcome != "active"
            {
                tracing::debug!(
                    operation,
                    outcome,
                    elapsed_ms,
                    "GNOME desktop RPC completed"
                );
            } else {
                tracing::trace!(
                    operation,
                    outcome,
                    elapsed_ms,
                    "GNOME desktop RPC completed"
                );
            }
            if let crate::desktop::DesktopResponse::Unavailable { reason } = response {
                tracing::warn!(operation, elapsed_ms, %reason, "GNOME desktop RPC unavailable");
            }
        }
        Err(error) => {
            tracing::warn!(operation, elapsed_ms, error = %format_args!("{error:#}"), "GNOME desktop RPC failed")
        }
    }
    result
}

/// The extension as desktop-agent --install writes it. debian/rules installs
/// and scripts/pack-extension.sh uploads the same files.
const EXTENSION_FILES: [(&str, &str); 6] = [
    (
        "metadata.json",
        include_str!("../../packaging/gnome-extension/metadata.json"),
    ),
    (
        "extension.js",
        include_str!("../../packaging/gnome-extension/extension.js"),
    ),
    (
        "indicator.js",
        include_str!("../../packaging/gnome-extension/indicator.js"),
    ),
    (
        "client.js",
        include_str!("../../packaging/gnome-extension/client.js"),
    ),
    (
        "settings.js",
        include_str!("../../packaging/gnome-extension/settings.js"),
    ),
    (
        "prefs.js",
        include_str!("../../packaging/gnome-extension/prefs.js"),
    ),
];
// GNOME Shell's ExtensionState values.
const ACTIVE: f64 = 1.0;
const OUT_OF_DATE: f64 = 4.0;

/// Installs and enables the GNOME extension for this user. Returns whether it
/// runs now; otherwise GNOME loads it at the next login.
pub async fn install_extension(shell: Option<&zbus::Connection>) -> anyhow::Result<bool> {
    // None when GNOME Shell could not be asked, Some(None) when this login
    // session has not loaded the extension.
    let state = match shell {
        Some(connection) => extension_state(connection).await.ok(),
        None => None,
    };
    // Shell reads extension folders only at login, except for an extension it
    // downloads from extensions.gnome.org itself: that one it loads at once.
    if let (Some(connection), Some(None)) = (shell, state)
        && let Ok(reply) = shell_call(connection, "InstallRemoteExtension").await
        && reply
            .body()
            .deserialize::<String>()
            .is_ok_and(|result| result == "successful")
    {
        return Ok(true);
    }
    write_extension()?;
    enable_extension()?;
    let (Some(connection), Some(Some(_))) = (shell, state) else {
        return Ok(false);
    };
    // Shell turns on an extension it has loaded when the setting changes.
    for _ in 0..30 {
        match extension_state(connection).await? {
            Some(ACTIVE) => return Ok(true),
            Some(OUT_OF_DATE) => {
                anyhow::bail!(
                    "The zflow extension does not support this GNOME version. Update zflow"
                )
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
        }
    }
    Ok(false)
}

async fn shell_call(connection: &zbus::Connection, method: &str) -> zbus::Result<zbus::Message> {
    connection
        .call_method(
            Some("org.gnome.Shell"),
            "/org/gnome/Shell",
            Some("org.gnome.Shell.Extensions"),
            method,
            &(crate::desktop::EXTENSION_ID,),
        )
        .await
}

async fn extension_state(connection: &zbus::Connection) -> zbus::Result<Option<f64>> {
    let info: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
        shell_call(connection, "GetExtensionInfo")
            .await?
            .body()
            .deserialize()?;
    // Shell answers with no fields for an extension it has not loaded.
    Ok(info
        .get("state")
        .and_then(|state| f64::try_from(state).ok()))
}

/// Makes this version's extension the one GNOME finds at login, unless the
/// copy came from extensions.gnome.org, which GNOME keeps updated itself.
fn write_extension() -> anyhow::Result<()> {
    let path = std::path::Path::new("gnome-shell/extensions").join(crate::desktop::EXTENSION_ID);
    let user = super::gnome::xdg("XDG_DATA_HOME", ".local/share")?.join(&path);
    // extensions.gnome.org marks the metadata of every copy it serves.
    if std::fs::read_to_string(user.join("metadata.json"))
        .is_ok_and(|text| text.contains("\"_generated\""))
    {
        return Ok(());
    }
    let packaged = super::gnome::xdg_dirs("XDG_DATA_DIRS", "/usr/local/share:/usr/share")
        .iter()
        .any(|dir| dir.join(&path).join("metadata.json").is_file());
    if packaged {
        // Older versions copied the extension here too, which would shadow
        // the package's copy.
        if std::fs::symlink_metadata(&user).is_ok_and(|metadata| metadata.is_dir()) {
            std::fs::remove_dir_all(&user)?;
        }
        return Ok(());
    }
    std::fs::create_dir_all(&user)?;
    for (name, contents) in EXTENSION_FILES {
        std::fs::write(user.join(name), contents)?;
    }
    Ok(())
}

/// Adds the extension to GNOME's enabled list. `gnome-extensions enable`
/// refuses an extension that Shell has not loaded yet, so this changes the
/// setting directly, as that command does when Shell is not running.
fn enable_extension() -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    let id = crate::desktop::EXTENSION_ID;
    for (key, listed) in [("enabled-extensions", true), ("disabled-extensions", false)] {
        let output = std::process::Command::new("gsettings")
            .args(["get", "org.gnome.shell", key])
            .output()
            .context("GNOME's gsettings command is missing")?;
        ensure!(output.status.success(), "gsettings could not read {key}");
        let mut uuids = parse_strv(&String::from_utf8_lossy(&output.stdout))
            .with_context(|| format!("Unexpected {key} setting"))?;
        if uuids.iter().any(|uuid| uuid == id) == listed {
            continue;
        }
        uuids.retain(|uuid| uuid != id);
        if listed {
            uuids.push(id.into());
        }
        let status = std::process::Command::new("gsettings")
            .args(["set", "org.gnome.shell", key, &format_strv(&uuids)])
            .status()?;
        ensure!(status.success(), "gsettings could not change {key}");
    }
    Ok(())
}

/// Reads a string list as `gsettings get` prints it, like ['a', 'b'] or @as [].
/// Escapes other than quotes and backslashes are refused, not guessed.
fn parse_strv(text: &str) -> Option<Vec<String>> {
    let text = text.trim();
    let list = text.strip_prefix("@as").unwrap_or(text).trim();
    let body = list.strip_prefix('[')?.strip_suffix(']')?.trim();
    let mut chars = body.chars().peekable();
    let mut items = Vec::new();
    while let Some(quote) = chars.next() {
        if quote != '\'' && quote != '"' {
            return None;
        }
        let mut item = String::new();
        loop {
            match chars.next()? {
                '\\' => item.push(chars.next().filter(|c| matches!(c, '\\' | '\'' | '"'))?),
                c if c == quote => break,
                c => item.push(c),
            }
        }
        items.push(item);
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        match chars.next() {
            None => break,
            Some(',') => while chars.next_if(|c| c.is_whitespace()).is_some() {},
            Some(_) => return None,
        }
        chars.peek()?;
    }
    Some(items)
}

fn format_strv(items: &[String]) -> String {
    let items: Vec<String> = items
        .iter()
        .map(|item| format!("'{}'", item.replace('\\', "\\\\").replace('\'', "\\'")))
        .collect();
    format!("[{}]", items.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Shell(Arc<Mutex<Vec<(String, serde_json::Value)>>>);

    #[zbus::interface(name = "org.gnome.Shell.Extensions.Zflow")]
    impl Shell {
        fn call(&self, #[zbus(header)] header: zbus::message::Header<'_>, json: &str) -> String {
            let sender = header.sender().unwrap().to_string();
            let request: serde_json::Value = serde_json::from_str(json).unwrap();
            self.0
                .lock()
                .unwrap()
                .push((sender, request["api"].clone()));
            r#"{"status":"unavailable","reason":"fake shell"}"#.into()
        }
    }

    #[test]
    fn gsettings_lists_round_trip() {
        assert_eq!(parse_strv("@as []\n"), Some(vec![]));
        let list = parse_strv("['ding@rastersoft.com', \"it's@x\", 'a\\\\b']\n").unwrap();
        assert_eq!(list, ["ding@rastersoft.com", "it's@x", "a\\b"]);
        assert_eq!(parse_strv(&format_strv(&list)), Some(list));
        assert_eq!(format_strv(&[]), "[]");
        for text in ["['a', ]", "['a' 'b']", "['a\\nb']", "['open", "a, b"] {
            assert_eq!(parse_strv(text), None, "{text}");
        }
    }

    #[tokio::test]
    #[ignore = "requires a private bus: dbus-run-session -- cargo test app::desktop::tests -- --ignored"]
    async fn receiver_runs_as_a_task_that_stop_ends() {
        let agent = zbus::Connection::session().await.unwrap();
        let mut receiver = DesktopReceiver::default();
        receiver.start(&agent);
        assert!(receiver.is_active());
        assert!(!receiver.is_ready());
        receiver.stop();
        assert!(!receiver.is_active());
        tokio::task::yield_now().await;
        assert_eq!(receiver.status(), "Receiving stopped");
        receiver.start(&agent);
        assert!(receiver.is_active());
    }

    #[tokio::test]
    #[ignore = "requires a private bus: dbus-run-session -- cargo test app::desktop::tests -- --ignored"]
    async fn receiver_waits_for_gnome_shell_and_calls_from_its_own_connection() {
        use crate::desktop::{BUS_NAME, OBJECT_PATH};
        let state = Mutex::new(State::default());
        let agent = zbus::Connection::session().await.unwrap();
        // Login can start the agent before Shell enables the extension.
        let early =
            tokio::time::timeout(std::time::Duration::from_millis(700), run(&agent, &state)).await;
        assert!(
            early.is_err(),
            "the receiver keeps waiting for the extension"
        );
        assert!(lock(&state).message.starts_with("Enable the zflow"));
        let shell = zbus::connection::Builder::session()
            .unwrap()
            .name("org.gnome.Shell")
            .unwrap()
            .build()
            .await
            .unwrap();
        let impostor = zbus::Connection::session().await.unwrap();
        impostor.request_name(BUS_NAME).await.unwrap();
        let error = run(&agent, &state).await.unwrap_err();
        assert!(format!("{error:#}").contains("Another program owns"));
        impostor.release_name(BUS_NAME).await.unwrap();

        let callers = Arc::new(Mutex::new(Vec::new()));
        shell
            .object_server()
            .at(OBJECT_PATH, Shell(callers.clone()))
            .await
            .unwrap();
        shell.request_name(BUS_NAME).await.unwrap();
        let error = run(&agent, &state).await.unwrap_err();
        assert_eq!(format!("{error:#}"), "fake shell");
        assert_eq!(
            *callers.lock().unwrap(),
            [(
                agent.unique_name().unwrap().to_string(),
                serde_json::json!(crate::app::gnome::API)
            )]
        );
    }
}
