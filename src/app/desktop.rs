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

    let report = async |terminal| {
        let request = crate::peer_view::Request::Focus { terminal };
        if let Err(error) = crate::peer_view::request(&request).await {
            tracing::debug!(error = %format_args!("{error:#}"), "desktop focus not sent");
        }
    };
    // The daemon forgets the focus when this stream closes, however this ends.
    tokio::select! {
        result = serve(&mut stream, &proxy, &bus, &owner) => result,
        result = forward_focus(connection, &proxy, owner.as_str(), report) => result,
    }
}

/// Answers the daemon's desktop requests through GNOME Shell.
#[cfg(target_os = "linux")]
async fn serve(
    stream: &mut tokio::net::UnixStream,
    proxy: &zbus::Proxy<'_>,
    bus: &zbus::fdo::DBusProxy<'_>,
    owner: &zbus::names::OwnedUniqueName,
) -> anyhow::Result<()> {
    use crate::desktop::{DesktopRequest, DesktopResponse};
    loop {
        let request: DesktopRequest = crate::control::read_message(stream).await?;
        request.validate()?;
        let response = match call(proxy, &request).await {
            Ok(response) => response,
            Err(error) => {
                // A restarted Shell has a new unique name; start over to find and check it.
                anyhow::ensure!(
                    bus.name_has_owner(owner.into()).await?,
                    "GNOME Shell restarted"
                );
                DesktopResponse::unavailable(format!("GNOME integration unavailable: {error}"))
            }
        };
        crate::control::write_message(stream, &response).await?;
    }
}

/// Reports whether a terminal has focus, first as it is now and then on every
/// change, so Mac shortcuts can use Ctrl+Shift there. Returns only on error.
#[cfg(target_os = "linux")]
async fn forward_focus(
    connection: &zbus::Connection,
    proxy: &zbus::Proxy<'_>,
    owner: &str,
    mut report: impl AsyncFnMut(bool),
) -> anyhow::Result<()> {
    use anyhow::Context;
    use zbus::{MatchRule, MessageStream, message::Type};
    // The extension sends this only to the agent. Anyone can address the
    // agent, so accept it only from Shell.
    let changes = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(owner)?
        .path(crate::desktop::OBJECT_PATH)?
        .interface(crate::desktop::BUS_NAME)?
        .member("FocusChanged")?
        .build();
    let owners = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .arg(0, crate::desktop::BUS_NAME)?
        .build();
    // Subscribe before asking, so no change falls in between.
    let mut changes = MessageStream::for_match_rule(changes, connection, None).await?;
    let mut owners = MessageStream::for_match_rule(owners, connection, None).await?;
    let mut terminal = focus(proxy).await;
    loop {
        report(terminal).await;
        terminal = tokio::select! {
            message = next_message(&mut changes) => {
                message.context("The session bus closed")??.body().deserialize::<bool>()?
            }
            message = next_message(&mut owners) => {
                let message = message.context("The session bus closed")??;
                let body = message.body();
                let (_, _, new): (&str, &str, &str) = body.deserialize()?;
                // Shell drops the name while the extension is off, such as
                // on the lock screen, and takes it again after.
                match new {
                    "" => false,
                    new if new == owner => focus(proxy).await,
                    _ => anyhow::bail!("GNOME Shell restarted"),
                }
            }
        };
    }
}

#[cfg(target_os = "linux")]
async fn next_message(stream: &mut zbus::MessageStream) -> Option<zbus::Result<zbus::Message>> {
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
        let json: String = tokio::time::timeout(
            std::time::Duration::from_millis(450),
            proxy.call("Call", &(r#"{"command":"focus"}"#,)),
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

#[cfg(target_os = "linux")]
async fn call(
    proxy: &zbus::Proxy<'_>,
    request: &crate::desktop::DesktopRequest,
) -> anyhow::Result<crate::desktop::DesktopResponse> {
    let started = std::time::Instant::now();
    let operation = crate::session::desktop_operation(request);
    tracing::trace!(operation, "GNOME desktop RPC started");
    let result = async {
        let json = serde_json::to_string(request)?;
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

/// Called only by the explicit Install GNOME integration action.
pub fn install_extension() -> anyhow::Result<()> {
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("The receiver integration requires Linux with GNOME");
    #[cfg(target_os = "linux")]
    {
        use anyhow::Context;
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME").map(|p| std::path::PathBuf::from(p).join(".local/share"))
            })
            .context("HOME is not set")?;
        let path = base
            .join("gnome-shell/extensions")
            .join(crate::desktop::EXTENSION_ID);
        std::fs::create_dir_all(&path)?;
        std::fs::write(
            path.join("metadata.json"),
            include_str!("../../packaging/gnome-extension/metadata.json"),
        )?;
        std::fs::write(
            path.join("extension.js"),
            include_str!("../../packaging/gnome-extension/extension.js"),
        )?;
        super::gnome::write_assets(&path)?;
        std::fs::write(
            path.join("indicator.js"),
            include_str!("../../packaging/gnome-extension/indicator.js"),
        )?;
        std::fs::write(
            path.join("prefs.js"),
            include_str!("../../packaging/gnome-extension/prefs.js"),
        )?;
        let mut child = std::process::Command::new("gnome-extensions")
            .args(["enable", crate::desktop::EXTENSION_ID])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context(
                "Integration installed. Log out and back in, then enable zflow in GNOME Extensions",
            )?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if let Some(status) = child.try_wait()? {
                anyhow::ensure!(
                    status.success(),
                    "Integration installed. Log out and back in, then enable zflow in GNOME Extensions"
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "Integration installed. GNOME did not respond; enable zflow in GNOME Extensions after logging out and back in"
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Shell(Arc<Mutex<Vec<String>>>);

    #[zbus::interface(name = "org.gnome.Shell.Extensions.Zflow")]
    impl Shell {
        fn call(&self, #[zbus(header)] header: zbus::message::Header<'_>, _json: &str) -> String {
            let sender = header.sender().map(|sender| sender.to_string());
            self.0.lock().unwrap().extend(sender);
            r#"{"status":"unavailable","reason":"fake shell"}"#.into()
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
            [agent.unique_name().unwrap().to_string()]
        );
    }

    struct FocusShell;

    #[zbus::interface(name = "org.gnome.Shell.Extensions.Zflow")]
    impl FocusShell {
        fn call(&self, json: &str) -> String {
            assert_eq!(json, r#"{"command":"focus"}"#);
            r#"{"status":"focus","terminal":true}"#.into()
        }
    }

    #[tokio::test]
    #[ignore = "requires a private bus: dbus-run-session -- cargo test app::desktop::tests -- --ignored"]
    async fn focus_follows_shell_signals_and_owner() {
        use crate::desktop::{BUS_NAME, OBJECT_PATH};
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
        let forward = tokio::spawn({
            let agent = agent.clone();
            let owner = owner.clone();
            async move {
                let proxy = zbus::Proxy::new(&agent, owner.clone(), OBJECT_PATH, BUS_NAME)
                    .await
                    .unwrap();
                // Not an async closure: its future would borrow `sent`, and
                // tokio::spawn cannot prove that Send.
                let report = move |terminal| {
                    sent.send(terminal).unwrap();
                    std::future::ready(())
                };
                forward_focus(&agent, &proxy, owner.as_str(), report).await
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
        emit(&shell, true).await;
        assert_eq!(reports.recv().await, Some(true));
        shell.release_name(BUS_NAME).await.unwrap();
        assert_eq!(
            reports.recv().await,
            Some(false),
            "no extension, no terminal"
        );
        shell.request_name(BUS_NAME).await.unwrap();
        assert_eq!(
            reports.recv().await,
            Some(true),
            "asks again when it returns"
        );
        shell.release_name(BUS_NAME).await.unwrap();
        impostor.request_name(BUS_NAME).await.unwrap();
        assert_eq!(reports.recv().await, Some(false));
        let error = forward.await.unwrap().unwrap_err();
        assert_eq!(format!("{error:#}"), "GNOME Shell restarted");
        assert_eq!(reports.recv().await, None);
    }
}
