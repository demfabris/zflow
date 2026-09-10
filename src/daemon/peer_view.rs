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
    // Only this metadata endpoint is reachable by desktop users. The input
    // control socket and the private config/state directories keep their modes.
    fs::set_permissions(parent, fs::Permissions::from_mode(0o755))?;
    prepare_socket_path(path)?;
    let listener = UnixListener::bind(path)?;
    let guard = SocketGuard(path.to_owned());
    fs::set_permissions(path, fs::Permissions::from_mode(0o666))?;
    tokio::spawn(async move {
        let _guard = guard;
        let slots = Arc::new(Semaphore::new(8));
        while let Ok((mut stream, _)) = listener.accept().await {
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                continue;
            };
            let shared = shared.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let result = tokio::time::timeout(Duration::from_secs(3), async {
                    let seat = tokio::task::spawn_blocking(query_primary_seat).await?;
                    authorize_peer(&stream, daemon_uid, seat.active_authenticated_uid())?;
                    let _: crate::peer_view::Request = read_message(&mut stream).await?;
                    let snapshot =
                        crate::peer_view::Snapshot::from_config(&*shared.config.read().await);
                    let seat = tokio::task::spawn_blocking(query_primary_seat).await?;
                    authorize_peer(&stream, daemon_uid, seat.active_authenticated_uid())?;
                    write_message(&mut stream, &snapshot).await?;
                    Ok::<_, anyhow::Error>(())
                })
                .await;
                if !matches!(result, Ok(Ok(()))) {
                    tracing::debug!("desktop metadata request denied, invalid, or timed out");
                }
            });
        }
    });
    Ok(())
}
