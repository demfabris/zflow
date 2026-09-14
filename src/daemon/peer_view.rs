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
                    let seat = tokio::task::spawn_blocking(query_primary_seat).await?;
                    authorize_peer(&stream, daemon_uid, seat.active_authenticated_uid())?;
                    let request =
                        tokio::time::timeout(Duration::from_secs(3), read_message(&mut stream))
                            .await??;
                    match request {
                        crate::peer_view::Request::Desktop {} => {
                            super::desktop::serve(shared.clone(), stream, daemon_uid).await?;
                        }
                        crate::peer_view::Request::Snapshot {} => {
                            let snapshot = crate::peer_view::Snapshot::from_config(
                                &*shared.config.read().await,
                            );
                            let seat = tokio::task::spawn_blocking(query_primary_seat).await?;
                            authorize_peer(&stream, daemon_uid, seat.active_authenticated_uid())?;
                            tokio::time::timeout(
                                Duration::from_secs(3),
                                write_message(&mut stream, &snapshot),
                            )
                            .await??;
                        }
                        crate::peer_view::Request::Pair { remote } => {
                            let result = async {
                                let _slot = pairing_slot
                                    .try_acquire_owned()
                                    .context("Another pairing is already open")?;
                                pair(&mut stream, &shared, daemon_uid, remote).await
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
                        crate::peer_view::Request::PairConfirm { .. } => {
                            bail!("Start pairing before confirming a code")
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

async fn pair(
    stream: &mut tokio::net::UnixStream,
    shared: &Arc<Shared>,
    daemon_uid: u32,
    remote: Option<std::net::SocketAddr>,
) -> Result<()> {
    use crate::peer_view::{PairingEvent, Request};
    let (session, name, authentication_code) = tokio::time::timeout(Duration::from_secs(120), async {
    write_message(stream, &PairingEvent::Ready).await?;
    let input_port = shared.config.read().await.transport.listen.port();
    let session = tokio::select! {
        session = crate::pairing::begin(&shared.identity, remote, input_port) => session?,
        _ = read_message::<_, Request>(stream) => bail!("Pairing cancelled or confirmed before a code was available"),
    };
    write_message(stream, &PairingEvent::Confirm {
        peer_label: session.peer_label.clone(),
        authentication_code: session.authentication_code.clone(),
    }).await?;
    let Request::PairConfirm { name, authentication_code } = read_message(stream).await? else {
        bail!("Expected a pairing confirmation");
    };
    Ok::<_, anyhow::Error>((session, name, authentication_code))
    }).await.context("Pairing expired; try again")??;
    let _mutation = shared.config_mutation.lock().await;
    let seat = tokio::task::spawn_blocking(query_primary_seat).await?;
    authorize_peer(stream, daemon_uid, seat.active_authenticated_uid())?;
    let mut config = shared.config.read().await.clone();
    crate::pairing::add_confirmed_peer(
        &mut config,
        session.observation(),
        &name,
        &authentication_code,
        true,
    )?;
    shared.apply_config_locked(config, true).await?;
    tokio::time::timeout(
        Duration::from_secs(3),
        write_message(stream, &PairingEvent::Paired),
    )
    .await??;
    Ok(())
}
