use super::model::ConfigDocument;
use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;

#[derive(Clone, Default, Serialize)]
pub(super) struct PairingSnapshot {
    pub state: &'static str,
    pub code: Option<String>,
    pub name: Option<String>,
    pub error: Option<String>,
}

pub(super) struct Pairing {
    stage: Arc<Mutex<PairingSnapshot>>,
    cancel: Option<oneshot::Sender<()>>,
    confirm: Option<oneshot::Sender<(String, String)>>,
}

impl Default for Pairing {
    fn default() -> Self {
        Self {
            stage: Arc::new(Mutex::new(PairingSnapshot {
                state: "idle",
                ..Default::default()
            })),
            cancel: None,
            confirm: None,
        }
    }
}

impl Pairing {
    pub fn snapshot(&self) -> PairingSnapshot {
        self.stage.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn active(&self) -> bool {
        matches!(self.snapshot().state, "waiting" | "confirm" | "saving")
    }
    pub fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        self.confirm.take();
        self.stage = Arc::new(Mutex::new(PairingSnapshot {
            state: "idle",
            ..Default::default()
        }));
    }
    pub fn start(&mut self, path: PathBuf, remote: Option<SocketAddr>) -> Result<()> {
        self.cancel();
        self.stage.lock().unwrap().state = "waiting";
        let (cancel, cancelled) = oneshot::channel();
        let (confirm, confirmation) = oneshot::channel();
        self.cancel = Some(cancel);
        self.confirm = Some(confirm);
        let stage = self.stage.clone();
        std::thread::Builder::new().name("zflow-pairing".into()).spawn(move || {
            let result=(|| {
                tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
                    tokio::select! {
                        _=cancelled => Ok(()),
                        result=tokio::time::timeout(Duration::from_secs(125),run(path,remote,confirmation,&stage)) => result.context("Pairing expired; try again")?,
                    }
                })
            })();
            if let Err(error)=result {
                *stage.lock().unwrap_or_else(|e|e.into_inner())=PairingSnapshot { state:"failed",error:Some(format!("{error:#}")),..Default::default() };
            }
        })?;
        Ok(())
    }
    pub fn confirm(&mut self, name: String, code: String) -> Result<()> {
        anyhow::ensure!(
            self.snapshot().state == "confirm",
            "No pairing is waiting for confirmation"
        );
        anyhow::ensure!(!name.trim().is_empty(), "Enter a computer name");
        anyhow::ensure!(
            code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()),
            "Enter the six-digit code from the other computer"
        );
        self.stage.lock().unwrap_or_else(|e| e.into_inner()).state = "saving";
        self.confirm
            .take()
            .context("Pairing expired")?
            .send((name, code))
            .map_err(|_| anyhow::anyhow!("Pairing expired"))?;
        Ok(())
    }
}
impl Drop for Pairing {
    fn drop(&mut self) {
        self.cancel();
    }
}

async fn run(
    path: PathBuf,
    remote: Option<SocketAddr>,
    confirmation: oneshot::Receiver<(String, String)>,
    stage: &Mutex<PairingSnapshot>,
) -> Result<()> {
    let mut document = ConfigDocument::open(path)?;
    let identity = crate::identity::Identity::load_or_create(&document.draft.daemon.state_dir)?;
    let session =
        crate::pairing::begin(&identity, remote, document.draft.transport.listen.port()).await?;
    *stage.lock().unwrap_or_else(|e| e.into_inner()) = PairingSnapshot {
        state: "confirm",
        code: Some(session.authentication_code.clone()),
        name: session.peer_label.clone(),
        error: None,
    };
    let (name, code) = confirmation.await.context("Pairing cancelled")?;
    crate::pairing::add_confirmed_peer(
        &mut document.draft,
        session.observation(),
        name.trim(),
        code.trim(),
        false,
    )?;
    document.save()?;
    stage.lock().unwrap_or_else(|e| e.into_inner()).state = "paired";
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[tokio::test]
    async fn file_pairing_persists_only_after_matching_confirmation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("local-identity");
        config.save(&path).unwrap();
        let remote_identity =
            crate::identity::Identity::load_or_create(&directory.path().join("remote-identity"))
                .unwrap();
        let offer = crate::pairing::make_offer(Some("Ubuntu".into()), 43119, vec![]).unwrap();
        let listener = crate::pairing::PairingListener::bind(
            &remote_identity,
            "127.0.0.1:0".parse().unwrap(),
            offer,
        )
        .unwrap();
        let (confirm, confirmation) = oneshot::channel();
        let stage = Mutex::new(PairingSnapshot::default());
        let (result, ()) = tokio::join!(
            run(
                path.clone(),
                Some(listener.local_addr().unwrap()),
                confirmation,
                &stage
            ),
            async {
                let session = listener.accept().await.unwrap();
                assert!(Config::load(&path).unwrap().peers.is_empty());
                confirm
                    .send(("Ubuntu".into(), session.authentication_code.clone()))
                    .unwrap();
            }
        );
        result.unwrap();
        assert_eq!(
            Config::load(&path).unwrap().peers["Ubuntu"]
                .spki_der()
                .unwrap(),
            remote_identity.spki()
        );
        assert_eq!(stage.lock().unwrap().state, "paired");
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
