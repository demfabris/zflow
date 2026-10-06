//! User-session connection between the daemon and GNOME's desktop integration.

use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Default)]
struct State {
    ready: bool,
    message: String,
    /// The last failure logged as a warning. The agent retries every two
    /// seconds, and a stale GNOME extension fails the same way until the next
    /// login, so only a new failure is worth a warning.
    failure: String,
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
        {
            let mut state = lock(&state);
            state.ready = false;
            state.message = "Connecting to the local GNOME desktop…".into();
        }
        self.task = Some(tokio::spawn(async move {
            let result = run(&connection, &state).await;
            let mut state = lock(&state);
            state.ready = false;
            state.message = match result {
                Ok(()) => "Receiving stopped".into(),
                Err(error) => {
                    let message = format!("{error:#}");
                    if message != state.failure {
                        tracing::warn!(error = %message, "desktop agent stopped");
                        state.failure.clone_from(&message);
                    }
                    message
                }
            };
        }));
    }
    pub fn stop(&mut self) {
        tracing::debug!("stopping desktop agent");
        if let Some(task) = self.task.take() {
            task.abort();
        }
        *lock(&self.state) = State {
            message: "Receiving stopped".into(),
            ..State::default()
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
    let snapshot = call(
        &proxy,
        &crate::peer_view::AgentRequest::Handoff(DesktopRequest::Snapshot),
    )
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
        failure: String::new(),
    };
    tracing::info!("desktop agent ready");

    let report = async |request: crate::peer_view::Request| {
        if let Err(error) = crate::peer_view::request(&request).await {
            tracing::debug!(error = %format_args!("{error:#}"), "desktop report not sent");
        }
    };
    let mut awake = IdleInhibitor::default();
    // The daemon forgets the focus when this stream closes, however this ends.
    let result = tokio::select! {
        result = serve(&mut stream, &proxy, &bus, &owner, &mut awake) => result,
        result = forward_signals(connection, &proxy, owner.as_str(), report) => result,
    };
    // Without the service nothing would show the pointer again or hear a push.
    awake.set(connection, false).await;
    for request in [
        crate::peer_view::LocalRequest::Sending { active: false },
        crate::peer_view::LocalRequest::Edges {
            edges: Vec::new(),
            pause_ms: 0,
        },
    ] {
        let _ = call(&proxy, &crate::peer_view::AgentRequest::Local(request)).await;
    }
    result
}

/// Keeps GNOME from treating the session as idle while this computer's
/// input goes to another one. Grabbed devices send GNOME nothing, so it
/// would otherwise blank and lock the screen. gnome-session drops the
/// inhibitor if the agent exits.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct IdleInhibitor(Option<u32>);

#[cfg(target_os = "linux")]
impl IdleInhibitor {
    const IDLE: u32 = 8;

    async fn set(&mut self, connection: &zbus::Connection, active: bool) {
        // The service waits for this request, so a slow gnome-session must not
        // hold it up.
        let result = tokio::time::timeout(std::time::Duration::from_millis(300), async {
            let proxy = zbus::Proxy::new(
                connection,
                "org.gnome.SessionManager",
                "/org/gnome/SessionManager",
                "org.gnome.SessionManager",
            )
            .await?;
            match (active, self.0) {
                (true, None) => {
                    let reason = "Controlling another computer";
                    self.0 = Some(
                        proxy
                            .call("Inhibit", &("zflow", 0_u32, reason, Self::IDLE))
                            .await?,
                    );
                }
                (false, Some(cookie)) => {
                    self.0 = None;
                    proxy.call::<_, _, ()>("Uninhibit", &(cookie,)).await?;
                }
                _ => {}
            }
            Ok::<_, zbus::Error>(())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, active, "GNOME idle inhibitor not changed"),
            Err(_) => tracing::warn!(active, "GNOME idle inhibitor timed out"),
        }
    }
}

/// Answers the daemon's desktop requests through GNOME Shell.
#[cfg(target_os = "linux")]
async fn serve(
    stream: &mut tokio::net::UnixStream,
    proxy: &zbus::Proxy<'_>,
    bus: &zbus::fdo::DBusProxy<'_>,
    owner: &zbus::names::OwnedUniqueName,
    awake: &mut IdleInhibitor,
) -> anyhow::Result<()> {
    use crate::control::{read_message_within, write_message_within};
    use crate::desktop::DesktopResponse;
    use crate::peer_view::{AgentReply, AgentRequest, LocalRequest, MAX_AGENT_MESSAGE};
    loop {
        let request: AgentRequest = read_message_within(stream, MAX_AGENT_MESSAGE).await?;
        request.validate()?;
        if let AgentRequest::Local(LocalRequest::Sending { active }) = request {
            awake.set(proxy.connection(), active).await;
        }
        let reply = match request {
            AgentRequest::Local(LocalRequest::ReadClipboard) => {
                read_clipboard(proxy).await.map(AgentReply::Clipboard)
            }
            AgentRequest::Local(LocalRequest::WriteClipboard { kind, data }) => {
                write_clipboard(proxy, kind, data)
                    .await
                    .map(|()| AgentReply::Desktop(DesktopResponse::Finished))
            }
            request => call(proxy, &request).await.map(AgentReply::Desktop),
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                // A restarted Shell has a new unique name; start over to find and check it.
                anyhow::ensure!(
                    bus.name_has_owner(owner.into()).await?,
                    "GNOME Shell restarted"
                );
                AgentReply::Desktop(DesktopResponse::unavailable(format!(
                    "GNOME integration unavailable: {error}"
                )))
            }
        };
        write_message_within(stream, &reply, MAX_AGENT_MESSAGE).await?;
    }
}

/// GNOME waits up to a second for the app that copied to hand the clipboard
/// over. The service waits longer than this for the agent.
#[cfg(target_os = "linux")]
const CLIPBOARD_CALL: std::time::Duration = std::time::Duration::from_millis(1500);

/// Reads the clipboard through the extension, as one clip: text if there is
/// any, else a PNG. The extension names a clip over the limit
/// "too_large:<bytes>" and keeps its bytes.
#[cfg(target_os = "linux")]
async fn read_clipboard(
    proxy: &zbus::Proxy<'_>,
) -> anyhow::Result<crate::peer_view::ClipboardContents> {
    use crate::clipboard::{ClipKind, MAX_CLIP_BYTES};
    use crate::peer_view::{ClipData, ClipboardContents};
    use anyhow::Context;
    let started = std::time::Instant::now();
    let (kind, data): (String, Vec<u8>) =
        tokio::time::timeout(CLIPBOARD_CALL, proxy.call("ReadClipboard", &())).await??;
    let bytes = data.len();
    let clip = |kind| {
        if bytes > MAX_CLIP_BYTES {
            ClipboardContents::TooLarge { bytes }
        } else {
            ClipboardContents::Clip {
                kind,
                data: ClipData(data),
            }
        }
    };
    let contents = match kind.as_str() {
        "text" => clip(ClipKind::Text),
        "png" => clip(ClipKind::Png),
        "empty" => ClipboardContents::Empty,
        other => ClipboardContents::TooLarge {
            bytes: other
                .strip_prefix("too_large:")
                .and_then(|bytes| bytes.parse().ok())
                .context("GNOME named an unknown clipboard kind")?,
        },
    };
    tracing::debug!(
        kind = kind.as_str(),
        bytes,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "GNOME clipboard read"
    );
    Ok(contents)
}

/// Puts a clip on the clipboard through the extension, after checking its
/// bytes are what `kind` says.
#[cfg(target_os = "linux")]
async fn write_clipboard(
    proxy: &zbus::Proxy<'_>,
    kind: crate::clipboard::ClipKind,
    data: crate::peer_view::ClipData,
) -> anyhow::Result<()> {
    use crate::clipboard::{Clip, ClipKind};
    let started = std::time::Instant::now();
    let clip = Clip::new(kind, data.0)?;
    let name = match clip.kind() {
        ClipKind::Text => "text",
        ClipKind::Png => "png",
    };
    tokio::time::timeout(
        CLIPBOARD_CALL,
        proxy.call::<_, _, ()>("WriteClipboard", &(name, clip.data())),
    )
    .await??;
    tracing::debug!(
        kind = name,
        bytes = clip.data().len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "GNOME clipboard written"
    );
    Ok(())
}

/// Reports whether a terminal has focus, first as it is now and then on every
/// change, so Mac shortcuts can use Ctrl+Shift there. Also reports the pointer
/// pushing against an edge that leads to another computer. Returns only on
/// error.
#[cfg(target_os = "linux")]
async fn forward_signals(
    connection: &zbus::Connection,
    proxy: &zbus::Proxy<'_>,
    owner: &str,
    mut report: impl AsyncFnMut(crate::peer_view::Request),
) -> anyhow::Result<()> {
    use crate::peer_view::Request;
    use anyhow::Context;
    use zbus::{MatchRule, MessageStream, message::Type};
    // The extension sends these only to the agent. Anyone can address the
    // agent, so accept them only from Shell.
    let from_shell = |member| {
        Ok::<_, zbus::Error>(
            MatchRule::builder()
                .msg_type(Type::Signal)
                .sender(owner)?
                .path(crate::desktop::OBJECT_PATH)?
                .interface(crate::desktop::BUS_NAME)?
                .member(member)?
                .build(),
        )
    };
    let changes = from_shell("FocusChanged")?;
    let hits = from_shell("EdgeHit")?;
    let owners = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .arg(0, crate::desktop::BUS_NAME)?
        .build();
    // Subscribe before asking, so no change falls in between.
    let mut changes = MessageStream::for_match_rule(changes, connection, None).await?;
    let mut hits = MessageStream::for_match_rule(hits, connection, None).await?;
    let mut owners = MessageStream::for_match_rule(owners, connection, None).await?;
    // None after an edge hit, which leaves the focus as it was.
    let mut terminal = Some(focus(proxy).await);
    loop {
        if let Some(terminal) = terminal {
            report(Request::Focus { terminal }).await;
        }
        terminal = tokio::select! {
            message = next_message(&mut changes) => {
                Some(message.context("The session bus closed")??.body().deserialize::<bool>()?)
            }
            message = next_message(&mut hits) => {
                let message = message.context("The session bus closed")??;
                let (monitor, edge, position): (String, String, u32) = message.body().deserialize()?;
                let monitor = (!monitor.is_empty()).then_some(monitor);
                match serde_json::from_value(edge.into()) {
                    Ok(edge) => report(Request::EdgeHit { monitor, edge, position }).await,
                    Err(error) => tracing::debug!(%error, "unknown edge from GNOME"),
                }
                None
            }
            message = next_message(&mut owners) => {
                let message = message.context("The session bus closed")??;
                let body = message.body();
                let (_, _, new): (&str, &str, &str) = body.deserialize()?;
                // Shell drops the name while the extension is off, such as
                // on the lock screen, and takes it again after.
                Some(match new {
                    "" => false,
                    // Shell turns the extension back on after unlocking, and it
                    // starts without barriers. A new connection makes the
                    // service send them again.
                    new if new == owner => anyhow::bail!("The zflow GNOME extension restarted"),
                    _ => anyhow::bail!("GNOME Shell restarted"),
                })
            }
        };
    }
}

#[cfg(target_os = "linux")]
pub(super) async fn next_message(
    stream: &mut zbus::MessageStream,
) -> Option<zbus::Result<zbus::Message>> {
    use zbus::export::futures_core::Stream;
    std::future::poll_fn(|context| std::pin::Pin::new(&mut *stream).poll_next(context)).await
}

/// Whether a terminal has focus, or false when the extension cannot say.
#[cfg(target_os = "linux")]
async fn focus(proxy: &zbus::Proxy<'_>) -> bool {
    #[derive(serde::Deserialize)]
    #[serde(tag = "status", rename_all = "snake_case")]
    enum Reply {
        Focus { terminal: bool },
    }
    let reply = async {
        let request = serde_json::json!({"command": "focus", "api": super::gnome::API}).to_string();
        let json: String = tokio::time::timeout(
            std::time::Duration::from_millis(450),
            proxy.call("Call", &(request,)),
        )
        .await??;
        let Reply::Focus { terminal } = serde_json::from_str(&json)?;
        anyhow::Ok(terminal)
    }
    .await;
    reply.unwrap_or_else(|error| {
        tracing::debug!(error = %format_args!("{error:#}"), "GNOME focus unavailable");
        false
    })
}

/// The reason GNOME gave for the last refusal logged as a warning.
#[cfg(target_os = "linux")]
static REFUSED: Mutex<String> = Mutex::new(String::new());

#[cfg(target_os = "linux")]
async fn call(
    proxy: &zbus::Proxy<'_>,
    request: &crate::peer_view::AgentRequest,
) -> anyhow::Result<crate::desktop::DesktopResponse> {
    let started = std::time::Instant::now();
    let operation = request.operation();
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
            // Warn once for a run of identical refusals, such as each retry
            // against a GNOME extension from another release.
            let mut refused = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
            match response {
                crate::desktop::DesktopResponse::Unavailable { reason } if *reason != *refused => {
                    tracing::warn!(operation, elapsed_ms, %reason, "GNOME desktop RPC unavailable");
                    refused.clone_from(reason);
                }
                crate::desktop::DesktopResponse::Unavailable { .. } => {}
                _ => refused.clear(),
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

fn extension_paths() -> anyhow::Result<(std::path::PathBuf, bool)> {
    let path = std::path::Path::new("gnome-shell/extensions").join(crate::desktop::EXTENSION_ID);
    let user = super::gnome::xdg("XDG_DATA_HOME", ".local/share")?.join(&path);
    let packaged = super::gnome::xdg_dirs("XDG_DATA_DIRS", "/usr/local/share:/usr/share")
        .iter()
        .any(|dir| dir.join(&path).join("metadata.json").is_file());
    Ok((user, packaged))
}

/// Makes this version's extension the one GNOME finds at login, unless the
/// copy came from extensions.gnome.org, which GNOME keeps updated itself.
fn write_extension() -> anyhow::Result<()> {
    let (user, packaged) = extension_paths()?;
    write_extension_at(&user, packaged, true)
}

/// Refresh an existing bundled copy without installing, enabling, or fetching
/// an extension. GNOME keeps copies from extensions.gnome.org up to date.
pub(super) fn refresh_extension() -> anyhow::Result<()> {
    let (user, packaged) = extension_paths()?;
    write_extension_at(&user, packaged, false)
}

fn write_extension_at(user: &std::path::Path, packaged: bool, create: bool) -> anyhow::Result<()> {
    let metadata = std::fs::read_to_string(user.join("metadata.json")).ok();
    // extensions.gnome.org marks the metadata of every copy it serves.
    if metadata
        .as_ref()
        .is_some_and(|text| text.contains("\"_generated\""))
    {
        return Ok(());
    }
    if packaged {
        // Older versions copied the extension here too, which would shadow
        // the package's copy.
        if std::fs::symlink_metadata(user).is_ok_and(|metadata| metadata.is_dir()) {
            std::fs::remove_dir_all(user)?;
        }
        return Ok(());
    }
    if !create && metadata.is_none() {
        return Ok(());
    }
    std::fs::create_dir_all(user)?;
    for (name, contents) in EXTENSION_FILES {
        let contents = if name == "metadata.json" {
            let mut metadata: serde_json::Value = serde_json::from_str(contents)?;
            metadata["version-name"] = env!("CARGO_PKG_VERSION").into();
            serde_json::to_string_pretty(&metadata)? + "\n"
        } else {
            contents.into()
        };
        if std::fs::read_to_string(user.join(name)).ok().as_deref() != Some(&contents) {
            std::fs::write(user.join(name), contents)?;
        }
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

    #[test]
    fn extension_refresh_only_updates_existing_bundled_files() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("extension");
        write_extension_at(&user, false, false).unwrap();
        assert!(
            !user.exists(),
            "ordinary startup must not install an extension"
        );

        write_extension_at(&user, false, true).unwrap();
        std::fs::write(user.join("metadata.json"), r#"{"version-name":"0.0.1"}"#).unwrap();
        std::fs::write(user.join("extension.js"), "older release").unwrap();
        write_extension_at(&user, false, false).unwrap();
        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(user.join("metadata.json")).unwrap()).unwrap();
        assert_eq!(metadata["version-name"], env!("CARGO_PKG_VERSION"));
        for (name, contents) in EXTENSION_FILES {
            if name != "metadata.json" {
                assert_eq!(std::fs::read_to_string(user.join(name)).unwrap(), contents);
            }
        }
    }

    #[test]
    fn extension_refresh_preserves_ego_owned_copies() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path();
        let metadata = r#"{"_generated":"EGO","version":7}"#;
        std::fs::write(user.join("metadata.json"), metadata).unwrap();
        std::fs::write(user.join("extension.js"), "EGO owns this").unwrap();
        for packaged in [false, true] {
            for create in [false, true] {
                write_extension_at(user, packaged, create).unwrap();
                assert_eq!(
                    std::fs::read_to_string(user.join("metadata.json")).unwrap(),
                    metadata
                );
                assert_eq!(
                    std::fs::read_to_string(user.join("extension.js")).unwrap(),
                    "EGO owns this"
                );
            }
        }
    }

    #[test]
    fn extension_refresh_uses_the_system_package_without_shadowing_it() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("extension");
        write_extension_at(&user, true, false).unwrap();
        assert!(!user.exists());
        write_extension_at(&user, false, true).unwrap();
        assert!(user.exists());
        write_extension_at(&user, true, false).unwrap();
        assert!(
            !user.exists(),
            "the system package must take precedence over an old bundled copy"
        );
    }

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

    /// Tests that own org.gnome.Shell on the shared private bus take turns.
    static SHELL_NAME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// GNOME Shell's extension API as seen over D-Bus, with the state it reports.
    struct Extensions(Arc<Mutex<Option<f64>>>);

    #[zbus::interface(name = "org.gnome.Shell.Extensions")]
    impl Extensions {
        fn get_extension_info(
            &self,
            uuid: &str,
        ) -> std::collections::HashMap<String, zbus::zvariant::OwnedValue> {
            assert_eq!(uuid, crate::desktop::EXTENSION_ID);
            let state = *self.0.lock().unwrap();
            state
                .map(|state| {
                    (
                        "state".into(),
                        zbus::zvariant::Value::from(state).try_into().unwrap(),
                    )
                })
                .into_iter()
                .collect()
        }
        fn install_remote_extension(&self, _uuid: &str) -> String {
            *self.0.lock().unwrap() = Some(ACTIVE);
            "successful".into()
        }
    }

    #[tokio::test]
    #[ignore = "requires a private bus: dbus-run-session -- cargo test app::desktop::tests -- --ignored"]
    async fn an_extension_shell_has_not_seen_comes_from_extensions_gnome_org() {
        let _turn = SHELL_NAME.lock().await;
        let state = Arc::new(Mutex::new(None));
        let _shell = zbus::connection::Builder::session()
            .unwrap()
            .name("org.gnome.Shell")
            .unwrap()
            .serve_at("/org/gnome/Shell", Extensions(state.clone()))
            .unwrap()
            .build()
            .await
            .unwrap();
        let agent = zbus::Connection::session().await.unwrap();
        assert_eq!(extension_state(&agent).await.unwrap(), None);
        // Loaded live, so neither files nor settings change.
        assert!(install_extension(Some(&agent)).await.unwrap());
        assert_eq!(extension_state(&agent).await.unwrap(), Some(ACTIVE));
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
        let _turn = SHELL_NAME.lock().await;
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

    /// The extension's clipboard as the agent sees it over D-Bus: one kind
    /// and its bytes.
    struct ClipboardShell {
        clipboard: Arc<Mutex<(String, Vec<u8>)>>,
        calls: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    #[zbus::interface(name = "org.gnome.Shell.Extensions.Zflow")]
    impl ClipboardShell {
        fn call(&self, json: &str) -> String {
            let request = serde_json::from_str(json).unwrap();
            self.calls.lock().unwrap().push(request);
            r#"{"status":"finished"}"#.into()
        }
        fn read_clipboard(&self) -> (String, Vec<u8>) {
            self.clipboard.lock().unwrap().clone()
        }
        fn write_clipboard(&self, kind: String, data: Vec<u8>) {
            *self.clipboard.lock().unwrap() = (kind, data);
        }
    }

    #[tokio::test]
    #[ignore = "requires a private bus: dbus-run-session -- cargo test app::desktop::tests -- --ignored"]
    async fn whole_clips_cross_the_service_stream_and_handoff_replies_stay_the_same() {
        use crate::clipboard::{ClipKind, MAX_CLIP_BYTES};
        use crate::control::{read_message_within, write_message_within};
        use crate::desktop::{BUS_NAME, DesktopRequest, OBJECT_PATH};
        use crate::peer_view::{
            AgentReply, AgentRequest, ClipData, ClipboardContents, LocalRequest, MAX_AGENT_MESSAGE,
        };
        let _turn = SHELL_NAME.lock().await;
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.resize(MAX_CLIP_BYTES, 7);
        let clipboard = Arc::new(Mutex::new(("png".to_owned(), png.clone())));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let shell = zbus::connection::Builder::session()
            .unwrap()
            .serve_at(
                OBJECT_PATH,
                ClipboardShell {
                    clipboard: clipboard.clone(),
                    calls: calls.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        shell.request_name(BUS_NAME).await.unwrap();
        let owner = shell.unique_name().unwrap().to_owned();
        let agent = zbus::Connection::session().await.unwrap();
        let proxy = zbus::Proxy::new(&agent, owner.clone(), OBJECT_PATH, BUS_NAME)
            .await
            .unwrap();
        let bus = zbus::fdo::DBusProxy::new(&agent).await.unwrap();
        let (mut service, mut stream) = tokio::net::UnixStream::pair().unwrap();
        let mut awake = IdleInhibitor::default();
        let asking = async {
            let mut ask = async |request: AgentRequest| -> serde_json::Value {
                write_message_within(&mut service, &request, MAX_AGENT_MESSAGE)
                    .await
                    .unwrap();
                read_message_within(&mut service, MAX_AGENT_MESSAGE)
                    .await
                    .unwrap()
            };
            let reply =
                |value: serde_json::Value| serde_json::from_value::<AgentReply>(value).unwrap();
            let read = AgentRequest::Local(LocalRequest::ReadClipboard);
            let set = |kind: &str, data: Vec<u8>| *clipboard.lock().unwrap() = (kind.into(), data);

            // A whole clip is larger than any other local message.
            let value = ask(read.clone()).await;
            assert!(value.to_string().len() > crate::control::MAX_CONTROL_MESSAGE);
            assert_eq!(
                reply(value),
                AgentReply::Clipboard(ClipboardContents::Clip {
                    kind: ClipKind::Png,
                    data: ClipData(png),
                })
            );
            let text = vec![b'a'; MAX_CLIP_BYTES];
            let write = |kind, data: &[u8]| {
                AgentRequest::Local(LocalRequest::WriteClipboard {
                    kind,
                    data: ClipData(data.to_vec()),
                })
            };
            assert_eq!(
                ask(write(ClipKind::Text, &text)).await,
                serde_json::json!({"status": "finished"})
            );
            assert_eq!(
                *clipboard.lock().unwrap(),
                ("text".to_owned(), text.clone())
            );
            // Bytes that are not what they claim never reach GNOME.
            let refused = ask(write(ClipKind::Png, b"GIF89a")).await;
            assert_eq!(refused["status"], "unavailable");
            assert!(refused["reason"].as_str().unwrap().contains("not a PNG"));
            assert_eq!(clipboard.lock().unwrap().0, "text");

            set("too_large:5242880", Vec::new());
            assert_eq!(
                reply(ask(read.clone()).await),
                AgentReply::Clipboard(ClipboardContents::TooLarge { bytes: 5 << 20 })
            );
            set("text", vec![b'a'; MAX_CLIP_BYTES + 1]);
            assert_eq!(
                reply(ask(read.clone()).await),
                AgentReply::Clipboard(ClipboardContents::TooLarge {
                    bytes: MAX_CLIP_BYTES + 1
                }),
                "the agent holds the limit too"
            );
            set("empty", Vec::new());
            assert_eq!(
                reply(ask(read.clone()).await),
                AgentReply::Clipboard(ClipboardContents::Empty)
            );
            set("gif", vec![1]);
            let unknown = ask(read.clone()).await;
            assert!(
                unknown["reason"]
                    .as_str()
                    .unwrap()
                    .contains("unknown clipboard kind")
            );

            // A notice goes through the JSON call; handoff replies are as before.
            let notice = crate::clipboard::too_large(5 << 20);
            let finished = serde_json::json!({"status": "finished"});
            let local = LocalRequest::Notify {
                message: notice.clone(),
            };
            assert_eq!(ask(AgentRequest::Local(local)).await, finished);
            assert_eq!(
                ask(AgentRequest::Handoff(DesktopRequest::Snapshot)).await,
                finished
            );
            assert_eq!(
                *calls.lock().unwrap(),
                [
                    serde_json::json!({"command": "notify", "message": notice, "api": crate::app::gnome::API}),
                    serde_json::json!({"command": "snapshot", "api": crate::app::gnome::API}),
                ]
            );
        };
        tokio::select! {
            result = serve(&mut stream, &proxy, &bus, &owner, &mut awake) => {
                panic!("the agent stopped serving: {result:?}")
            }
            () = asking => {}
        }
    }

    struct FocusShell;

    #[zbus::interface(name = "org.gnome.Shell.Extensions.Zflow")]
    impl FocusShell {
        fn call(&self, json: &str) -> String {
            let request: serde_json::Value = serde_json::from_str(json).unwrap();
            assert_eq!(
                request,
                serde_json::json!({"command": "focus", "api": super::super::gnome::API})
            );
            r#"{"status":"focus","terminal":true}"#.into()
        }
    }

    #[tokio::test]
    #[ignore = "requires a private bus: dbus-run-session -- cargo test app::desktop::tests -- --ignored"]
    async fn focus_follows_shell_signals_and_owner() {
        use crate::desktop::{BUS_NAME, OBJECT_PATH};
        let _turn = SHELL_NAME.lock().await;
        let agent = zbus::Connection::session().await.unwrap();
        let shell = zbus::connection::Builder::session()
            .unwrap()
            .serve_at(OBJECT_PATH, FocusShell)
            .unwrap()
            .build()
            .await
            .unwrap();
        shell.request_name(BUS_NAME).await.unwrap();
        let owner = shell.unique_name().unwrap().to_owned();
        let impostor = zbus::Connection::session().await.unwrap();
        let (sent, mut reports) = tokio::sync::mpsc::unbounded_channel();
        let (hit, mut hits) = tokio::sync::mpsc::unbounded_channel();
        let forward = tokio::spawn({
            let agent = agent.clone();
            let owner = owner.clone();
            async move {
                let proxy = zbus::Proxy::new(&agent, owner.clone(), OBJECT_PATH, BUS_NAME)
                    .await
                    .unwrap();
                // Not an async closure: its future would borrow `sent`, and
                // tokio::spawn cannot prove that Send.
                let report = move |request| {
                    match request {
                        crate::peer_view::Request::Focus { terminal } => {
                            sent.send(terminal).unwrap()
                        }
                        crate::peer_view::Request::EdgeHit { edge, position, .. } => {
                            hit.send((edge, position)).unwrap()
                        }
                        other => panic!("unexpected report {other:?}"),
                    }
                    std::future::ready(())
                };
                forward_signals(&agent, &proxy, owner.as_str(), report).await
            }
        });
        let emit = async |from: &zbus::Connection, terminal: bool| {
            from.emit_signal(
                agent.unique_name(),
                OBJECT_PATH,
                BUS_NAME,
                "FocusChanged",
                &(terminal,),
            )
            .await
            .unwrap();
        };
        assert_eq!(reports.recv().await, Some(true), "asks once at the start");
        emit(&shell, false).await;
        assert_eq!(reports.recv().await, Some(false));
        emit(&impostor, true).await;
        // The bus has handled the impostor's signal once this returns.
        zbus::fdo::DBusProxy::new(&impostor)
            .await
            .unwrap()
            .get_id()
            .await
            .unwrap();
        emit(&shell, false).await;
        assert_eq!(reports.recv().await, Some(false), "only Shell is heard");
        let push = async |from: &zbus::Connection, edge: &str, position: u32| {
            from.emit_signal(
                agent.unique_name(),
                OBJECT_PATH,
                BUS_NAME,
                "EdgeHit",
                &("", edge, position),
            )
            .await
            .unwrap();
        };
        push(&impostor, "left", 1).await;
        push(&shell, "right", 500_000).await;
        assert_eq!(
            hits.recv().await,
            Some((crate::desktop::Edge::Right, 500_000)),
            "only Shell's edge hits reach the service"
        );
        assert!(
            reports.try_recv().is_err(),
            "an edge hit is not a focus change"
        );
        emit(&shell, true).await;
        assert_eq!(reports.recv().await, Some(true));
        shell.release_name(BUS_NAME).await.unwrap();
        assert_eq!(
            reports.recv().await,
            Some(false),
            "no extension, no terminal"
        );
        // A returning extension has lost its barriers; reconnecting resends them.
        shell.request_name(BUS_NAME).await.unwrap();
        let error = forward.await.unwrap().unwrap_err();
        assert_eq!(format!("{error:#}"), "The zflow GNOME extension restarted");
        assert_eq!(reports.recv().await, None);
    }
}
