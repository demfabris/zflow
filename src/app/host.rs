//! The Mac engine on a thread of the app's own process. The window's
//! requests queue for that thread, which ticks the engine between them, so
//! no engine work runs on the UI thread.
use super::{NativeApp, api::Request};
use crate::lens::viewer::Viewer;
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{Arc, mpsc},
    thread::JoinHandle,
    time::Duration,
};

type Reply = mpsc::Sender<Result<Value>>;

/// How long a request waits for the engine thread before it gives up.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);
/// How often the engine ticks while no request comes.
const TICK: Duration = Duration::from_millis(12);

pub struct Engine {
    commands: Option<mpsc::Sender<(Request, Reply)>>,
    thread: Option<JoinHandle<()>>,
    /// Read and written straight from the UI thread, so a frame never waits
    /// for the engine thread.
    lens: Arc<Viewer>,
}

impl Engine {
    /// Starts the engine with the configuration at `path`.
    pub fn open(path: PathBuf) -> Result<Self> {
        // Input a peer holds is let go if the app is told to quit by a signal.
        crate::macos::install_exit_handlers();
        let (ready, started) = mpsc::channel();
        let (commands, requests) = mpsc::channel::<(Request, Reply)>();
        let thread = std::thread::Builder::new()
            .name("zflow-engine".into())
            .spawn(move || {
                let mut app = match std::panic::catch_unwind(|| NativeApp::open(path)) {
                    Ok(Ok(app)) => app,
                    Ok(Err(error)) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                    Err(_) => {
                        let _ = ready.send(Err(anyhow!("The sharing engine could not start")));
                        return;
                    }
                };
                let _ = ready.send(Ok(app.lens()));
                loop {
                    match requests.recv_timeout(TICK) {
                        Ok((request, reply)) => {
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    app.request(request)
                                }))
                                .unwrap_or_else(|_| {
                                    Err(anyhow!("The sharing engine stopped unexpectedly"))
                                });
                            let _ = reply.send(result);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    app.tick();
                }
            })?;
        let lens = started
            .recv()
            .map_err(|_| anyhow!("The sharing engine could not start"))??;
        Ok(Self {
            commands: Some(commands),
            thread: Some(thread),
            lens,
        })
    }

    /// Runs `request` on the engine thread and returns its snapshot.
    pub fn request(&self, request: Request) -> Result<Value> {
        let (reply, answer) = mpsc::channel();
        self.commands
            .as_ref()
            .context("The sharing engine has stopped")?
            .send((request, reply))
            .map_err(|_| anyhow!("The sharing engine has stopped"))?;
        answer
            .recv_timeout(REPLY_TIMEOUT)
            .map_err(|_| anyhow!("The sharing engine did not answer"))?
    }

    pub fn lens(&self) -> &Arc<Viewer> {
        &self.lens
    }
}

impl Drop for Engine {
    /// Closing the queue ends the engine thread, which lets go of input a
    /// peer holds before the app exits.
    fn drop(&mut self) {
        self.commands.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
