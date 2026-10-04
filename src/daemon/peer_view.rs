use super::*;
use crate::peer_view::DesktopStatus;
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
        while let Ok((mut stream, _)) = listener.accept().await {
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                continue;
            };
            let shared = shared.clone();
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
    use crate::peer_view::{DesktopReply, Request};
    // Sent on every focus change, so it skips the config lock.
    if let Request::Focus { terminal } = request {
        authorize_peer(stream, daemon_uid, shared.active_uid())?;
        shared.desktop.focus(&shared.runtime, terminal).await;
        return Ok(DesktopReply::Ack);
    }
    if let Request::EdgeHit {
        monitor,
        edge,
        position,
    } = request
    {
        authorize_peer(stream, daemon_uid, shared.active_uid())?;
        shared.edge_hit(monitor, edge, position);
        return Ok(DesktopReply::Ack);
    }
    if let Request::Retry {} = request {
        authorize_peer(stream, daemon_uid, shared.active_uid())?;
        shared.retry_links().await;
        return Ok(DesktopReply::Ack);
    }
    if let Request::AddAddress { address } = request {
        authorize_peer(stream, daemon_uid, shared.active_uid())?;
        shared.add_address(address)?;
        return Ok(DesktopReply::Ack);
    }
    // Asked every second and only reads, so it skips the config lock too.
    if let Request::Status {} = request {
        authorize_peer(stream, daemon_uid, shared.active_uid())?;
        return Ok(DesktopReply::Status(Box::new(status(shared).await)));
    }
    let _mutation = shared.config_mutation.lock().await;
    let uid = authorize_peer(stream, daemon_uid, shared.active_uid())?;
    let mut config = shared.config.read().await.clone();
    match request {
        Request::SetSharing { enabled } => {
            let was = config.daemon.sharing;
            config.daemon.sharing = enabled;
            shared.apply_config_locked(config, true).await?;
            // A pause nobody remembers asking for is hard to trace otherwise.
            let pid = stream.peer_cred().ok().and_then(|caller| caller.pid());
            tracing::info!(
                sharing = enabled,
                was,
                source = "desktop app",
                uid,
                pid,
                "input sharing set"
            );
            Ok(DesktopReply::Ack)
        }
        Request::SetSwitching { pause_at_edges } => {
            config.switching.pause_at_edges = pause_at_edges;
            shared.apply_config_locked(config, true).await?;
            Ok(DesktopReply::Ack)
        }
        Request::SetClipboard { share } => {
            config.clipboard.share = share;
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
            reverse_scroll,
        } => {
            let Some(peer) = config.peers.get_mut(&name) else {
                bail!("Unknown computer {name}");
            };
            crate::peer_view::set_peer(peer, allow_control, keyboard, reverse_scroll);
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
        Request::Place {
            id,
            x,
            y,
            tolerance,
        } => {
            let spot = super::arrange::Spot { x, y, tolerance };
            let name = shared.trust(config, &id, Some(spot)).await?;
            tracing::info!(peer = %name, uid, "computer placed and trusted");
            Ok(DesktopReply::Ack)
        }
        _ => bail!("Unsupported desktop operation"),
    }
}

/// What the settings window shows. Each lock goes before the next wait: on
/// a fresh install the layout starts here, and keeping it takes the
/// sessions lock again.
async fn status(shared: &Shared) -> DesktopStatus {
    let config = shared.config.read().await.clone();
    let receiving_from = shared
        .inbound_owner
        .lock()
        .await
        .as_ref()
        .map(|(peer, _)| peer.clone());
    let sending_to = shared
        .active_outbound
        .lock()
        .await
        .as_ref()
        .map(|active| active.peer.clone());
    let connected = shared.sessions.lock().await.keys().cloned().collect();
    let links = shared.link_status().await;
    let layout = shared.layout_status().await;
    DesktopStatus {
        receiving_from,
        sending_to,
        connected,
        links,
        layout,
        pairing_window: shared.window().view(tokio::time::Instant::now()),
        own_mark: Some(crate::neighbors::mark(shared.identity.spki())),
        unplaced: shared.unplaced(&config),
        notices: shared.notices(),
        ..DesktopStatus::from_config(&config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        control::{Request, Response},
        pairing_window::{Reason, State},
        peer_view::{DesktopReply, Request as DesktopRequest},
    };

    /// A computer called `name` answered the hello this one sent to its
    /// record. It takes input where nothing listens, so its link dials
    /// nothing real.
    fn answered(shared: &Shared, spki: &[u8], name: &str) {
        let address: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let hello = crate::hello::make_hello(name, 9, Vec::new(), false);
        let record = format!("zf-{name}");
        let now = tokio::time::Instant::now();
        shared.neighbors.send_modify(|neighbors| {
            neighbors.instance_seen(&record, vec![address], true, Some(name.into()));
            neighbors.take_due_hellos(1, now);
            neighbors.hello(spki, address, &hello, Some(&record), now);
        });
    }

    fn identity() -> (tempfile::TempDir, Identity) {
        let directory = tempfile::tempdir().unwrap();
        let identity = Identity::load_or_create(directory.path()).unwrap();
        (directory, identity)
    }

    /// A fresh install as a person first sees it: the desktop agent is
    /// connected, nobody arranged the computers yet, the settings window
    /// asks for the status every second and the pairing window is open.
    /// The window takes the lone computer around, and a person adds the
    /// next one with `zflow trust`.
    #[tokio::test(start_paused = true)]
    async fn a_fresh_install_answers_while_computers_join_and_are_trusted() {
        let (shared, _kept) = test_daemon();
        let state_dir = shared.config.read().await.daemon.state_dir.clone();
        PairingWindow::mark_eligible(&state_dir).unwrap();
        *shared.window() = PairingWindow::load(&state_dir);
        shared.open_pairing_window().await;
        let _agent = shared.desktop.answer_everything().await;
        let uid = nix::unistd::geteuid().as_raw();
        let settings = tokio::spawn({
            let shared = shared.clone();
            async move {
                let (mut stream, _other) = tokio::net::UnixStream::pair().unwrap();
                loop {
                    let status = DesktopRequest::Status {};
                    let reply = desktop_command(&mut stream, &shared, uid, status).await;
                    assert!(matches!(reply, Ok(DesktopReply::Status(_))));
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        });
        let looking_around = super::super::arrange::start(shared.clone());

        let (_desk_directory, desk) = identity();
        answered(&shared, desk.spki(), "desk");
        let joined = tokio::time::timeout(Duration::from_secs(30), async {
            while !shared.config.read().await.peers.contains_key("desk") {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        assert!(joined.is_ok(), "desk joined while the window was open");
        let window = shared.window().view(tokio::time::Instant::now());
        assert_eq!(
            (window.state, window.reason),
            (State::Closed, Some(Reason::Accepted))
        );

        let (_laptop_directory, laptop) = identity();
        answered(&shared, laptop.spki(), "laptop");
        let trust = Request::Trust {
            computer: "laptop".into(),
        };
        let trusted =
            tokio::time::timeout(Duration::from_secs(10), dispatch_result(trust, &shared))
                .await
                .expect("zflow trust answered")
                .unwrap();
        assert!(matches!(trusted, Response::Trusted { name, .. } if name == "laptop"));
        assert!(
            !settings.is_finished(),
            "the settings window still gets its status"
        );
        settings.abort();
        looking_around.abort();
    }
}
