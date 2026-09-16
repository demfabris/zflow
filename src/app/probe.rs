use super::displays::Desktop;
use crate::config::Config;
use anyhow::Result;
use std::{collections::BTreeMap, sync::mpsc, thread::JoinHandle};

#[derive(Default)]
pub(super) struct ProbeResult {
    pub desktops: BTreeMap<String, Desktop>,
    pub errors: Vec<String>,
}

pub(super) struct Probe {
    thread: JoinHandle<()>,
    result: mpsc::Receiver<Result<ProbeResult>>,
}
impl Probe {
    pub fn start(config: Config) -> Result<Self> {
        let (sender, result) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("zflow-receiver-check".into())
            .spawn(move || {
                let outcome = (|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?
                        .block_on(async {
                            let mut result = ProbeResult::default();
                            for name in config.peers.keys() {
                                match crate::macos::receiver_snapshot(&config, name)
                                    .await
                                    .and_then(|g| g.bounds())
                                {
                                    Ok(bounds) => {
                                        result.desktops.insert(
                                            name.clone(),
                                            Desktop {
                                                width: bounds.width,
                                                height: bounds.height,
                                            },
                                        );
                                    }
                                    Err(error) => result.errors.push(format!("{name}: {error:#}")),
                                }
                            }
                            Ok(result)
                        })
                })();
                let _ = sender.send(outcome);
            })?;
        Ok(Self { thread, result })
    }
    pub fn finished(&self) -> bool {
        self.thread.is_finished()
    }
    pub fn finish(self) -> Result<ProbeResult> {
        self.thread
            .join()
            .map_err(|_| anyhow::anyhow!("Receiver check stopped unexpectedly"))?;
        self.result
            .recv()
            .map_err(|_| anyhow::anyhow!("Receiver check returned no result"))?
    }
}
