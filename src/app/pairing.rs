#[cfg(target_os = "macos")]
use super::model::ConfigDocument;
use crate::pairing::SetupCode;
use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;

/// A listener's code stays on screen while the other computer is set up.
const LISTEN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Default, Serialize)]
pub(super) struct PairingSnapshot {
    /// idle, listening, connecting, paired or failed.
    pub state: &'static str,
    /// The setup code this computer shows while listening.
    pub code: Option<String>,
    /// The name the other computer was saved under.
    pub name: Option<String>,
    pub error: Option<String>,
}

pub(super) struct Pairing {
    stage: Arc<Mutex<PairingSnapshot>>,
    cancel: Option<oneshot::Sender<()>>,
}

impl Default for Pairing {
    fn default() -> Self {
        Self {
            stage: Arc::new(Mutex::new(PairingSnapshot {
                state: "idle",
                ..Default::default()
            })),
            cancel: None,
        }
    }
}

impl Pairing {
    pub fn snapshot(&self) -> PairingSnapshot {
        self.stage.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn active(&self) -> bool {
        matches!(self.snapshot().state, "listening" | "connecting")
    }
    pub fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        self.stage = Arc::new(Mutex::new(PairingSnapshot {
            state: "idle",
            ..Default::default()
        }));
    }
    /// Connects to `remote` with the code the user typed, or listens and shows
    /// a fresh code when there is no remote.
    pub fn start(
        &mut self,
        path: PathBuf,
        remote: Option<SocketAddr>,
        code: Option<String>,
    ) -> Result<()> {
        let code = remote
            .map(|_| SetupCode::parse(code.as_deref().unwrap_or_default()))
            .transpose()?;
        self.cancel();
        self.stage.lock().unwrap().state = if remote.is_some() {
            "connecting"
        } else {
            "listening"
        };
        let (cancel, cancelled) = oneshot::channel();
        self.cancel = Some(cancel);
        let stage = self.stage.clone();
        let limit = if remote.is_some() {
            CONNECT_TIMEOUT
        } else {
            LISTEN_TIMEOUT
        };
        std::thread::Builder::new().name("zflow-pairing".into()).spawn(move || {
            let result=(|| {
                tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
                    tokio::select! {
                        _=cancelled => Ok(()),
                        result=tokio::time::timeout(limit,run(path,remote,code,&stage)) => result.context("Pairing expired; try again")?,
                    }
                })
            })();
            if let Err(error)=result {
                *stage.lock().unwrap_or_else(|e|e.into_inner())=PairingSnapshot { state:"failed",error:Some(format!("{error:#}")),..Default::default() };
            }
        })?;
        Ok(())
    }
}
impl Drop for Pairing {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(target_os = "macos")]
async fn run(
    path: PathBuf,
    remote: Option<SocketAddr>,
    code: Option<SetupCode>,
    stage: &Mutex<PairingSnapshot>,
) -> Result<()> {
    let mut document = ConfigDocument::open(path)?;
    let identity = crate::identity::Identity::load_or_create(&document.draft.daemon.state_dir)?;
    let code = match code {
        Some(code) => code,
        None => {
            let code = SetupCode::generate()?;
            stage.lock().unwrap_or_else(|e| e.into_inner()).code = Some(code.to_string());
            code
        }
    };
    let session = crate::pairing::begin(
        &identity,
        remote,
        document.draft.transport.listen.port(),
        &code,
    )
    .await?;
    let saved = crate::pairing::add_paired_peer(&mut document.draft, session.observation(), false)
        .and_then(|name| document.save().map(|()| name));
    session.finish(saved.is_ok()).await;
    *stage.lock().unwrap_or_else(|e| e.into_inner()) = PairingSnapshot {
        state: "paired",
        name: Some(saved?),
        ..Default::default()
    };
    Ok(())
}

#[cfg(target_os = "linux")]
async fn run(
    _path: PathBuf,
    remote: Option<SocketAddr>,
    code: Option<SetupCode>,
    stage: &Mutex<PairingSnapshot>,
) -> Result<()> {
    run_stream(
        crate::peer_view::connect_service().await?,
        remote,
        code,
        stage,
    )
    .await
}

/// The daemon holds this computer's identity, so it runs the pairing and
/// reports the code it shows and the name it saved.
#[cfg(target_os = "linux")]
async fn run_stream(
    mut stream: tokio::net::UnixStream,
    remote: Option<SocketAddr>,
    code: Option<SetupCode>,
    stage: &Mutex<PairingSnapshot>,
) -> Result<()> {
    use crate::{
        control::{read_message, write_message},
        peer_view::{PairingEvent, Request},
    };
    let code = code.map(|code| code.digits().to_owned());
    write_message(&mut stream, &Request::Pair { remote, code }).await?;
    loop {
        match read_message(&mut stream).await? {
            PairingEvent::Listening { code } => {
                stage.lock().unwrap_or_else(|e| e.into_inner()).code = Some(code);
            }
            PairingEvent::Paired { name } => {
                *stage.lock().unwrap_or_else(|e| e.into_inner()) = PairingSnapshot {
                    state: "paired",
                    name: Some(name),
                    ..Default::default()
                };
                return Ok(());
            }
            PairingEvent::Error { message } => anyhow::bail!("{message}"),
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::config::Config;

    #[tokio::test]
    async fn the_mac_saves_a_receiver_only_after_its_code_proves_out() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("local-identity");
        config.save(&path).unwrap();
        let remote_identity =
            crate::identity::Identity::load_or_create(&directory.path().join("remote-identity"))
                .unwrap();
        let code = SetupCode::generate().unwrap();
        let offer = crate::pairing::make_offer(Some("Ubuntu".into()), 43119, vec![]).unwrap();
        let listener = crate::pairing::PairingListener::bind(
            &remote_identity,
            "127.0.0.1:0".parse().unwrap(),
            offer,
            code.clone(),
        )
        .unwrap();
        let address = listener.local_addr().unwrap();

        let wrong = SetupCode::parse(if code.digits() == "000000" {
            "000001"
        } else {
            "000000"
        })
        .unwrap();
        let stage = Mutex::new(PairingSnapshot::default());
        let (result, ()) = tokio::join!(
            async {
                let refused = run(path.clone(), Some(address), Some(wrong), &stage).await;
                assert!(refused.unwrap_err().to_string().contains("does not match"));
                assert!(Config::load(&path).unwrap().peers.is_empty());
                run(path.clone(), Some(address), Some(code), &stage).await
            },
            async {
                // The listener drops the wrong attempt and keeps waiting.
                let session = listener.accept().await.unwrap();
                assert!(Config::load(&path).unwrap().peers.is_empty());
                session.finish(true).await;
            }
        );
        result.unwrap();
        assert_eq!(
            Config::load(&path).unwrap().peers["Ubuntu"]
                .spki_der()
                .unwrap(),
            remote_identity.spki()
        );
        let stage = stage.lock().unwrap();
        assert_eq!(
            (stage.state, stage.name.as_deref()),
            ("paired", Some("Ubuntu"))
        );
    }

    #[test]
    fn connecting_needs_a_six_digit_code_before_anything_starts() {
        let mut pairing = Pairing::default();
        let remote = Some("192.0.2.1:43120".parse().unwrap());
        assert!(pairing.start(PathBuf::new(), remote, None).is_err());
        assert!(
            pairing
                .start(PathBuf::new(), remote, Some("12345".into()))
                .is_err()
        );
        assert_eq!(pairing.snapshot().state, "idle");
    }

    #[test]
    fn cancellation_closes_channels_and_isolates_old_workers() {
        let mut pairing = Pairing::default();
        let old_stage = pairing.stage.clone();
        let (sender, mut cancelled) = oneshot::channel();
        pairing.cancel = Some(sender);
        pairing.cancel();
        assert_eq!(cancelled.try_recv(), Ok(()));
        old_stage.lock().unwrap().state = "paired";
        assert_eq!(pairing.snapshot().state, "idle");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;
    use crate::{
        control::{read_message, write_message},
        peer_view::{PairingEvent, Request},
    };

    #[tokio::test]
    async fn pairing_shows_the_daemon_code_and_reports_the_saved_name() {
        let (client, mut server) = tokio::net::UnixStream::pair().unwrap();
        let stage = Mutex::new(PairingSnapshot::default());
        let (result, ()) = tokio::join!(run_stream(client, None, None, &stage), async {
            assert!(matches!(
                read_message::<_, Request>(&mut server).await.unwrap(),
                Request::Pair {
                    remote: None,
                    code: None
                }
            ));
            write_message(
                &mut server,
                &PairingEvent::Listening {
                    code: "482 913".into(),
                },
            )
            .await
            .unwrap();
            tokio::task::yield_now().await;
            write_message(
                &mut server,
                &PairingEvent::Paired {
                    name: "MacBook".into(),
                },
            )
            .await
            .unwrap();
        });
        result.unwrap();
        let snapshot = stage.lock().unwrap().clone();
        assert_eq!(
            (snapshot.state, snapshot.name.as_deref()),
            ("paired", Some("MacBook"))
        );

        let (client, mut server) = tokio::net::UnixStream::pair().unwrap();
        let code = SetupCode::parse("482913").unwrap();
        let remote = Some("192.0.2.1:43120".parse().unwrap());
        let (result, ()) = tokio::join!(run_stream(client, remote, Some(code), &stage), async {
            assert!(matches!(
                read_message::<_, Request>(&mut server).await.unwrap(),
                Request::Pair { code: Some(code), .. } if code == "482913"
            ));
            write_message(
                &mut server,
                &PairingEvent::Error {
                    message: "That code does not match".into(),
                },
            )
            .await
            .unwrap();
        });
        assert!(result.unwrap_err().to_string().contains("does not match"));
    }

    #[test]
    fn cancellation_isolates_late_pairing_results() {
        let mut pairing = Pairing::default();
        let old = pairing.stage.clone();
        let (sender, mut receiver) = oneshot::channel();
        pairing.cancel = Some(sender);
        pairing.cancel();
        assert_eq!(receiver.try_recv(), Ok(()));
        old.lock().unwrap().state = "paired";
        assert_eq!(pairing.snapshot().state, "idle");
    }
}
