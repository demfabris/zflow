use super::*;
use std::os::unix::fs::MetadataExt;

pub(super) fn start(shared: Arc<Shared>) -> Result<()> {
    let path = Path::new(crate::peer_view::SOCKET_PATH);
    let parent = path.parent().expect("fixed socket parent");
    let metadata = fs::symlink_metadata(parent)
        .context("Install the updated systemd unit to provide the desktop API directory")?;
    let daemon_uid = nix::unistd::geteuid().as_raw();
    if !metadata.is_dir() || metadata.uid() != daemon_uid || metadata.mode() & 0o022 != 0 {
        bail!("unsafe desktop API directory");
    }
    // Only this desktop endpoint is reachable by desktop users. The input
    // control socket and the private config/state directories keep their modes.
    fs::set_permissions(parent, fs::Permissions::from_mode(0o755))?;
    prepare_socket_path(path)?;
    let listener = UnixListener::bind(path)?;
    let guard = SocketGuard(path.to_owned());
    fs::set_permissions(path, fs::Permissions::from_mode(0o666))?;
    tokio::spawn(async move {
        let _guard = guard;
        let slots = Arc::new(Semaphore::new(8));
        let pairing_slot = Arc::new(Semaphore::new(1));
        while let Ok((mut stream, _)) = listener.accept().await {
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                continue;
            };
            let shared = shared.clone();
            let pairing_slot = pairing_slot.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let result = async {
                    authorize_peer(&stream, daemon_uid, shared.active_uid())?;
                    let request =
                        tokio::time::timeout(Duration::from_secs(3), read_message(&mut stream))
                            .await??;
                    match request {
                        crate::peer_view::Request::Desktop {} => {
                            super::desktop::serve(shared.clone(), stream, daemon_uid).await?;
                        }
                        crate::peer_view::Request::Pair { remote, code } => {
                            let result = async {
                                let _slot = pairing_slot
                                    .try_acquire_owned()
                                    .context("Another pairing is already open")?;
                                pair(&mut stream, &shared, daemon_uid, remote, code).await
                            }
                            .await;
                            if let Err(error) = result {
                                let response = crate::peer_view::PairingEvent::Error {
                                    message: format!("{error:#}"),
                                };
                                let _ = tokio::time::timeout(
                                    Duration::from_secs(1),
                                    write_message(&mut stream, &response),
                                )
                                .await;
                            }
                        }
                        crate::peer_view::Request::PairRespond { .. } => {
                            bail!("No pairing is waiting for an answer")
                        }
                        request => {
                            let reply = desktop_command(&mut stream, &shared, daemon_uid, request)
                                .await
                                .unwrap_or_else(|error| crate::peer_view::DesktopReply::Error {
                                    message: format!("{error:#}"),
                                });
                            tokio::time::timeout(
                                Duration::from_secs(3),
                                write_message(&mut stream, &reply),
                            )
                            .await??;
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if result.is_err() {
                    tracing::debug!("desktop request denied, invalid, or timed out");
                }
            });
        }
    });
    Ok(())
}

async fn desktop_command(
    stream: &mut tokio::net::UnixStream,
    shared: &Arc<Shared>,
    daemon_uid: u32,
    request: crate::peer_view::Request,
) -> Result<crate::peer_view::DesktopReply> {
    use crate::peer_view::{DesktopReply, DesktopStatus, Request};
    // Sent on every focus change, so it skips the config lock.
    if let Request::Focus { terminal } = request {
        authorize_peer(stream, daemon_uid, shared.active_uid())?;
        shared.desktop.focus(&shared.runtime, terminal).await;
        return Ok(DesktopReply::Ack);
    }
    if let Request::EdgeHit { edge, position } = request {
        authorize_peer(stream, daemon_uid, shared.active_uid())?;
        shared.edge_hit(edge, position);
        return Ok(DesktopReply::Ack);
    }
    let _mutation = shared.config_mutation.lock().await;
    authorize_peer(stream, daemon_uid, shared.active_uid())?;
    let mut config = shared.config.read().await.clone();
    match request {
        Request::Status {} => Ok(DesktopReply::Status(DesktopStatus {
            receiving_from: shared
                .inbound_owner
                .lock()
                .await
                .as_ref()
                .map(|(peer, _)| peer.clone()),
            sending_to: shared
                .active_outbound
                .lock()
                .await
                .as_ref()
                .map(|active| active.peer.clone()),
            connected: shared.sessions.lock().await.keys().cloned().collect(),
            layout: shared.layout_status().await,
            ..DesktopStatus::from_config(&config)
        })),
        Request::SetSharing { enabled } => {
            config.daemon.sharing = enabled;
            shared.apply_config_locked(config, true).await?;
            Ok(DesktopReply::Ack)
        }
        Request::Forget { name } => {
            if config.peers.remove(&name).is_none() {
                bail!("Unknown computer {name}");
            }
            shared.apply_config_locked(config, true).await?;
            Ok(DesktopReply::Ack)
        }
        Request::SetPeer {
            name,
            allow_control,
            keyboard,
        } => {
            let Some(peer) = config.peers.get_mut(&name) else {
                bail!("Unknown computer {name}");
            };
            crate::peer_view::set_peer(peer, allow_control, keyboard);
            shared.apply_config_locked(config, true).await?;
            Ok(DesktopReply::Ack)
        }
        Request::MoveTile {
            id,
            x,
            y,
            tolerance,
        } => {
            shared.move_tile(&id, x, y, tolerance).await?;
            Ok(DesktopReply::Ack)
        }
        _ => bail!("Unsupported desktop operation"),
    }
}

/// A listener here waits up to this long for the other computer to be set up.
const LISTEN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

async fn pair(
    stream: &mut tokio::net::UnixStream,
    shared: &Arc<Shared>,
    daemon_uid: u32,
    remote: Option<std::net::SocketAddr>,
    code: Option<String>,
) -> Result<()> {
    use crate::{
        pairing::SetupCode,
        peer_view::{PairingEvent, Request},
    };
    let input_port = shared.config.read().await.transport.listen.port();
    let (code, limit) = match remote {
        Some(_) => (
            SetupCode::parse(code.as_deref().unwrap_or_default())?,
            CONNECT_TIMEOUT,
        ),
        None => {
            let code = SetupCode::generate()?;
            write_message(
                stream,
                &PairingEvent::Listening {
                    code: code.to_string(),
                },
            )
            .await?;
            (code, LISTEN_TIMEOUT)
        }
    };
    let mut session = tokio::select! {
        session = tokio::time::timeout(limit, crate::pairing::begin(&shared.identity, remote, input_port, &code)) => {
            session.context("Pairing expired; try again")??
        }
        _ = read_message::<_, Request>(stream) => bail!("Pairing cancelled"),
    };
    if remote.is_some() {
        write_message(stream, &PairingEvent::Approving).await?;
        tokio::select! {
            approved = session.approved() => approved?,
            _ = read_message::<_, Request>(stream) => bail!("Pairing cancelled"),
        }
    } else {
        // Knowing the code is not enough; the person at this computer allows it.
        write_message(
            stream,
            &PairingEvent::Confirm {
                name: session.display_name(),
                address: session.peer_ip().to_string(),
            },
        )
        .await?;
        let answer = tokio::time::timeout(
            crate::pairing::APPROVAL_TIMEOUT,
            read_message::<_, Request>(stream),
        )
        .await;
        if !matches!(answer, Ok(Ok(Request::PairRespond { allow: true }))) {
            session.finish(false).await;
            bail!("Pairing declined");
        }
    }
    let saved = async {
        let _mutation = shared.config_mutation.lock().await;
        authorize_peer(stream, daemon_uid, shared.active_uid())?;
        let mut config = shared.config.read().await.clone();
        let name = crate::pairing::add_paired_peer(&mut config, session.observation())?;
        shared.apply_config_locked(config, true).await?;
        Ok::<_, anyhow::Error>(name)
    }
    .await;
    // The other computer saves this one only after hearing that we kept it.
    session.finish(saved.is_ok()).await;
    let name = saved?;
    tokio::time::timeout(
        Duration::from_secs(3),
        write_message(stream, &PairingEvent::Paired { name }),
    )
    .await??;
    Ok(())
}
