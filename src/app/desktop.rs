//! User-session connection between the daemon and GNOME's desktop integration.

use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    active: bool,
    ready: bool,
    message: String,
}

#[derive(Default)]
pub struct DesktopReceiver {
    state: Arc<Mutex<State>>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    generation: Arc<std::sync::atomic::AtomicU64>,
}

impl DesktopReceiver {
    pub fn is_active(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).active
    }
    pub fn is_ready(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).ready
    }
    pub fn status(&self) -> String {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .message
            .clone()
    }
    /// `connection` must own the agent's bus name; the extension answers no
    /// other caller.
    pub fn start(&mut self, connection: &zbus::Connection) {
        if self.is_active() {
            return;
        }
        self.stop();
        let connection = connection.clone();
        let generation = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let generation_ref = self.generation.clone();
        tracing::debug!(generation, "starting desktop agent");
        let state = self.state.clone();

        let (cancel, receipt) = tokio::sync::oneshot::channel();
        self.cancel = Some(cancel);
        *state.lock().unwrap_or_else(|e| e.into_inner()) = State {
            active: true,
            ready: false,
            message: "Connecting to the local GNOME desktop…".into(),
        };
        std::thread::spawn(move || {
            let result = (|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(async {
                        tokio::select! {
                            result=run(&connection,&state,&generation_ref,generation) => result,
                            _=receipt => Ok(()),
                        }
                    })
            })();
            if generation_ref.load(std::sync::atomic::Ordering::SeqCst) == generation {
                if let Err(error) = &result {
                    tracing::warn!(generation, error = %format_args!("{error:#}"), "desktop agent stopped");
                }
                *state.lock().unwrap_or_else(|e| e.into_inner()) = State {
                    active: false,
                    ready: false,
                    message: match result {
                        Ok(()) => "Receiving stopped".into(),
                        Err(error) => format!("{error:#}"),
                    },
                };
            }
        });
    }
    pub fn stop(&mut self) {
        tracing::debug!("stopping desktop agent");
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = State {
            active: false,
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
async fn run(
    connection: &zbus::Connection,
    state: &Arc<Mutex<State>>,
    generation: &std::sync::atomic::AtomicU64,
    expected: u64,
) -> anyhow::Result<()> {
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
                state.lock().unwrap_or_else(|e| e.into_inner()).message = ENABLE.into();
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
    if generation.load(std::sync::atomic::Ordering::SeqCst) != expected {
        return Ok(());
    }
    state.lock().unwrap_or_else(|e| e.into_inner()).message =
        "Ready to receive through the GNOME desktop".into();
    state.lock().unwrap_or_else(|e| e.into_inner()).ready = true;
    tracing::info!(generation = expected, "desktop agent ready");

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
    async fn receiver_waits_for_gnome_shell_and_calls_from_its_own_connection() {
        use crate::desktop::{BUS_NAME, OBJECT_PATH};
        let state = Arc::new(Mutex::new(State::default()));
        let generation = std::sync::atomic::AtomicU64::new(1);
        let agent = zbus::Connection::session().await.unwrap();
        // Login can start the agent before Shell enables the extension.
        let early = tokio::time::timeout(
            std::time::Duration::from_millis(700),
            run(&agent, &state, &generation, 1),
        )
        .await;
        assert!(
            early.is_err(),
            "the receiver keeps waiting for the extension"
        );
        assert!(
            state
                .lock()
                .unwrap()
                .message
                .starts_with("Enable the zflow")
        );
        let shell = zbus::connection::Builder::session()
            .unwrap()
            .name("org.gnome.Shell")
            .unwrap()
            .build()
            .await
            .unwrap();
        let impostor = zbus::Connection::session().await.unwrap();
        impostor.request_name(BUS_NAME).await.unwrap();
        let error = run(&agent, &state, &generation, 1).await.unwrap_err();
        assert!(format!("{error:#}").contains("Another program owns"));
        impostor.release_name(BUS_NAME).await.unwrap();

        let callers = Arc::new(Mutex::new(Vec::new()));
        shell
            .object_server()
            .at(OBJECT_PATH, Shell(callers.clone()))
            .await
            .unwrap();
        shell.request_name(BUS_NAME).await.unwrap();
        let error = run(&agent, &state, &generation, 1).await.unwrap_err();
        assert_eq!(format!("{error:#}"), "fake shell");
        assert_eq!(
            *callers.lock().unwrap(),
            [agent.unique_name().unwrap().to_string()]
        );
    }
}
