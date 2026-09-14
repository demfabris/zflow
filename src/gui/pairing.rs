use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use eguicn::{Button, ButtonVariant, Card, Input, egui};
use tokio::sync::oneshot;

use crate::config::Config;
#[cfg(target_os = "linux")]
use crate::peer_view::PairingEvent;

#[derive(Clone)]
pub(super) enum PairingTarget {
    File { path: PathBuf, config: Box<Config> },
    Service,
}

#[derive(Clone, Default)]
enum Stage {
    #[default]
    Idle,
    Waiting,
    Confirm {
        code: String,
        label: Option<String>,
    },
    Saving,
    Paired,
    Failed(String),
}

#[derive(Default)]
pub(super) struct PairingUi {
    address: String,
    name: String,
    code: String,
    stage: Arc<Mutex<Stage>>,
    cancel: Option<oneshot::Sender<()>>,
    confirm: Option<oneshot::Sender<(String, String)>>,
}

impl PairingUi {
    pub fn is_active(&self) -> bool {
        matches!(
            *self.stage.lock().unwrap_or_else(|error| error.into_inner()),
            Stage::Waiting | Stage::Confirm { .. } | Stage::Saving
        )
    }

    pub fn set_address(&mut self, mut address: SocketAddr) {
        address.set_port(crate::pairing::DEFAULT_PAIRING_PORT);
        self.address = address.to_string();
    }

    /// Returns true once after adding a computer, so the caller can reload peers.
    pub fn show(&mut self, ui: &mut egui::Ui, target: PairingTarget, enabled: bool) -> bool {
        let stage = self
            .stage
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let mut paired = false;
        Card::new().padding(20).show(ui, |ui| {
            Card::header(ui, "Pair a computer", "Open zflow on both computers. Choose Allow pairing on Ubuntu, then connect from the Mac.");
            match &stage {
                Stage::Waiting => { ui.label("Waiting for the other computer. Pairing expires after two minutes."); }
                Stage::Confirm { code, label } => {
                    if self.name.is_empty() {
                        self.name = label.clone().unwrap_or_else(|| if matches!(target, PairingTarget::Service) { "Mac" } else { "Ubuntu" }.into());
                    }
                    ui.label("Compare this code with the code shown on the other computer:");
                    ui.label(egui::RichText::new(code).monospace().size(28.0));
                    super::field(ui, "Computer name", &mut self.name);
                    ui.label("Enter the code shown on the other computer");
                    ui.add(Input::new(&mut self.code).id(ui.id().with("pair-code")));
                    ui.label(if matches!(target, PairingTarget::Service) { "This computer may send input to Ubuntu while your session is unlocked." } else { "Allow your Mac to send input to this computer." });
                    if ui.add_enabled(self.code.len() == 6 && !self.name.trim().is_empty(), Button::new("Confirm pairing")).clicked() {
                        self.set_stage(Stage::Saving, ui.ctx());
                        if let Some(confirm) = self.confirm.take() {
                            let _ = confirm.send((self.name.trim().into(), self.code.trim().into()));
                        }
                    }
                }
                Stage::Saving => { ui.label("Saving the paired computer…"); }
                Stage::Paired => {
                    ui.label("Paired on this computer. Confirm the matching code on the other computer too.");
                    paired = true;
                    self.cancel.take();
                    self.confirm.take();
                    self.set_stage(Stage::Idle, ui.ctx());
                }
                Stage::Idle | Stage::Failed(_) => {
                    if let Stage::Failed(error) = &stage { ui.colored_label(ui.visuals().error_fg_color, error); }
                    if !enabled { ui.label("Save changes and stop sharing before pairing."); }
                    ui.add_enabled_ui(enabled, |ui| {
                        if ui.add(Button::new("Allow pairing").variant(ButtonVariant::Outline)).clicked() {
                            self.start(target.clone(), None, ui.ctx().clone());
                        }
                        super::field(ui, "Other computer's pairing address", &mut self.address);
                        ui.label("Use port 43120, for example 192.168.1.20:43120.");
                        if ui.add(Button::new("Pair with address")).clicked() {
                            match super::parse_address(&self.address) {
                                Ok(remote) => self.start(target, Some(remote), ui.ctx().clone()),
                                Err(error) => self.set_stage(Stage::Failed(format!("{error:#}")), ui.ctx()),
                            }
                        }
                    });
                }
            }
            if matches!(stage, Stage::Waiting | Stage::Confirm { .. }) && ui.add(Button::new("Cancel pairing").variant(ButtonVariant::Outline)).clicked() {
                self.stop();
            }
        });
        paired
    }

    fn set_stage(&self, stage: Stage, ctx: &egui::Context) {
        update(&self.stage, ctx, stage);
    }

    pub fn stop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        self.confirm.take();
        self.stage = Arc::new(Mutex::new(Stage::Idle));
    }

    fn start(&mut self, target: PairingTarget, remote: Option<SocketAddr>, ctx: egui::Context) {
        self.stop();
        self.name.clear();
        self.code.clear();
        self.set_stage(Stage::Waiting, &ctx);
        let (cancel, cancelled) = oneshot::channel();
        let (confirm, confirmation) = oneshot::channel();
        self.cancel = Some(cancel);
        self.confirm = Some(confirm);
        let stage = self.stage.clone();
        let worker_ctx = ctx.clone();
        if let Err(error) = std::thread::Builder::new().name("zflow-pairing".into()).spawn(move || {
            let result = (|| {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                runtime.block_on(async {
                    tokio::select! {
                        _ = cancelled => Ok(()),
                        result = tokio::time::timeout(Duration::from_secs(125), run(target, remote, confirmation, &stage, &worker_ctx)) => result.context("Pairing expired; try again")?,
                    }
                })
            })();
            if let Err(error) = result { update(&stage, &worker_ctx, Stage::Failed(format!("{error:#}"))); }
        }) {
            self.set_stage(Stage::Failed(format!("Could not start pairing: {error}")), &ctx);
        }
    }
}

impl Drop for PairingUi {
    fn drop(&mut self) {
        self.stop();
    }
}

fn update(stage: &Mutex<Stage>, ctx: &egui::Context, next: Stage) {
    *stage.lock().unwrap_or_else(|error| error.into_inner()) = next;
    ctx.request_repaint();
}

async fn run(
    target: PairingTarget,
    remote: Option<SocketAddr>,
    confirmation: oneshot::Receiver<(String, String)>,
    stage: &Mutex<Stage>,
    ctx: &egui::Context,
) -> Result<()> {
    match target {
        PairingTarget::File { path, config } => {
            let mut document = super::model::ConfigDocument::open(path)?;
            if document.draft != *config {
                bail!("Settings changed; reload before pairing");
            }
            let identity = crate::identity::Identity::load_or_create(&config.daemon.state_dir)?;
            let session =
                crate::pairing::begin(&identity, remote, config.transport.listen.port()).await?;
            update(
                stage,
                ctx,
                Stage::Confirm {
                    code: session.authentication_code.clone(),
                    label: session.peer_label.clone(),
                },
            );
            let (name, code) = confirmation.await.context("Pairing cancelled")?;
            crate::pairing::add_confirmed_peer(
                &mut document.draft,
                session.observation(),
                &name,
                &code,
                cfg!(target_os = "linux"),
            )?;
            document.save()?;
        }
        PairingTarget::Service => service_pair(remote, confirmation, stage, ctx).await?,
    }
    update(stage, ctx, Stage::Paired);
    Ok(())
}

#[cfg(target_os = "linux")]
async fn service_pair(
    remote: Option<SocketAddr>,
    confirmation: oneshot::Receiver<(String, String)>,
    stage: &Mutex<Stage>,
    ctx: &egui::Context,
) -> Result<()> {
    use crate::{
        control::{read_message, write_message},
        peer_view::Request,
    };
    let mut stream = crate::peer_view::connect_service().await?;
    write_message(&mut stream, &Request::Pair { remote }).await?;
    let mut confirmation = Some(confirmation);
    loop {
        match read_message(&mut stream).await? {
            PairingEvent::Ready => {}
            PairingEvent::Confirm {
                peer_label,
                authentication_code,
            } => {
                update(
                    stage,
                    ctx,
                    Stage::Confirm {
                        code: authentication_code,
                        label: peer_label,
                    },
                );
                let (name, authentication_code) = confirmation
                    .take()
                    .context("Unexpected repeated pairing confirmation")?
                    .await
                    .context("Pairing cancelled")?;
                write_message(
                    &mut stream,
                    &Request::PairConfirm {
                        name,
                        authentication_code,
                    },
                )
                .await?;
            }
            PairingEvent::Paired => return Ok(()),
            PairingEvent::Error { message } => bail!("{message}"),
        }
    }
}

#[cfg(not(target_os = "linux"))]
async fn service_pair(
    _remote: Option<SocketAddr>,
    _confirmation: oneshot::Receiver<(String, String)>,
    _stage: &Mutex<Stage>,
    _ctx: &egui::Context,
) -> Result<()> {
    bail!("The desktop service API is Linux-only")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let stage = Mutex::new(Stage::Idle);
        let ctx = egui::Context::default();
        let target = PairingTarget::File {
            path: path.clone(),
            config: Box::new(config),
        };
        let (result, ()) = tokio::join!(
            run(
                target,
                Some(listener.local_addr().unwrap()),
                confirmation,
                &stage,
                &ctx
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
        assert!(matches!(*stage.lock().unwrap(), Stage::Paired));
    }

    #[test]
    fn cancellation_closes_channels_and_isolates_old_workers() {
        let mut pairing = PairingUi::default();
        let old_stage = pairing.stage.clone();
        let (sender, mut cancelled) = oneshot::channel();
        pairing.cancel = Some(sender);
        pairing.stop();
        assert_eq!(cancelled.try_recv(), Ok(()));
        *old_stage.lock().unwrap() = Stage::Paired;
        assert!(matches!(*pairing.stage.lock().unwrap(), Stage::Idle));
    }
}
