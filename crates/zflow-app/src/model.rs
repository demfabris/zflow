//! What the window shows and how it asks for changes. A worker thread owns
//! the line to the engine, polls it for snapshots and runs requests in order,
//! so nothing blocks the UI thread.

use std::{path::PathBuf, sync::mpsc, time::Duration};

use serde::Deserialize;
use zflow::app::{
    api::{self, Request},
    client::Client,
};
use zz_gpui::{App, AsyncApp, Context, Entity, Task, WeakEntity};

use crate::platform::Platform;

pub type Snapshot = api::Snapshot<Option<Platform>>;

/// How often the window asks for a fresh snapshot.
#[cfg(target_os = "macos")]
const POLL: Duration = Duration::from_millis(500);
#[cfg(not(target_os = "macos"))]
const POLL: Duration = Duration::from_secs(1);

/// What the window shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Page {
    Computers,
    Settings,
    /// A paired computer, by name.
    Computer(String),
}

type Reply = async_channel::Sender<Result<(), String>>;

enum Job {
    Call {
        request: Request,
        reply: Option<Reply>,
    },
    /// A question whose answer is not a snapshot, such as where a move
    /// would land.
    Ask {
        request: Request,
        reply: async_channel::Sender<Result<serde_json::Value, String>>,
    },
    Reconnect,
    /// Drops the line to the engine, which on a Mac stops it and lets go of
    /// input a peer holds, then answers.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Shutdown(async_channel::Sender<()>),
}

enum Event {
    Snapshot(Box<Snapshot>),
    /// A request failed. A poll failing is quiet: it says why there is no
    /// snapshot instead of popping up.
    Failed {
        error: String,
        quiet: bool,
    },
    #[cfg(target_os = "macos")]
    Lens(std::sync::Arc<zflow::lens::viewer::Viewer>),
}

pub struct Model {
    pub snapshot: Option<Snapshot>,
    /// Why there is no snapshot.
    pub error: Option<String>,
    /// The last request that failed, until the window takes it as a toast.
    pub failure: Option<String>,
    pub page: Page,
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub config: PathBuf,
    /// The window onto another computer's screen.
    #[cfg(target_os = "macos")]
    pub lens: Option<std::sync::Arc<zflow::lens::viewer::Viewer>>,
    jobs: mpsc::Sender<Job>,
    /// The newest notice seen, once a snapshot came.
    last_notice: Option<u64>,
    _events: Task<()>,
}

impl Model {
    pub fn new(config: PathBuf, cx: &mut Context<Self>) -> Self {
        let (jobs, queue) = mpsc::channel();
        let (events, inbox) = async_channel::unbounded();
        let worker_config = config.clone();
        std::thread::Builder::new()
            .name("zflow-window".into())
            .spawn(move || work(&worker_config, &queue, &events))
            .expect("the worker thread starts");
        let receive = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            while let Ok(event) = inbox.recv().await {
                if this
                    .update(cx, |model, cx| model.receive(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            snapshot: None,
            error: None,
            failure: None,
            page: Page::Computers,
            config,
            #[cfg(target_os = "macos")]
            lens: None,
            jobs,
            last_notice: None,
            _events: receive,
        }
    }

    /// A model with `snapshot` and no engine, for tests. What the window
    /// asks for queues on the returned [`Requests`].
    #[cfg(test)]
    pub fn fake(snapshot: Snapshot, _: &mut Context<Self>) -> (Self, Requests) {
        let (jobs, queue) = mpsc::channel();
        (
            Self {
                snapshot: Some(snapshot),
                error: None,
                failure: None,
                page: Page::Computers,
                config: PathBuf::from("/tmp/zflow.toml"),
                #[cfg(target_os = "macos")]
                lens: None,
                jobs,
                last_notice: None,
                _events: Task::ready(()),
            },
            Requests(queue),
        )
    }

    /// Sends `request`, and shows why if it fails.
    pub fn send(&self, request: Request) {
        let _ = self.jobs.send(Job::Call {
            request,
            reply: None,
        });
    }

    /// Sends `request` and resolves to why it failed, for a caller that
    /// shows the reason itself.
    pub fn attempt(&self, request: Request) -> async_channel::Receiver<Result<(), String>> {
        let (reply, answer) = async_channel::bounded(1);
        let _ = self.jobs.send(Job::Call {
            request,
            reply: Some(reply),
        });
        answer
    }

    /// Asks `request` and resolves to its raw answer.
    pub fn ask(
        &self,
        request: Request,
    ) -> async_channel::Receiver<Result<serde_json::Value, String>> {
        let (reply, answer) = async_channel::bounded(1);
        let _ = self.jobs.send(Job::Ask { request, reply });
        answer
    }

    /// Starts over after the engine could not be reached.
    pub fn reconnect(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        let _ = self.jobs.send(Job::Reconnect);
        cx.notify();
    }

    pub fn show(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.page != page {
            self.page = page;
            cx.notify();
        }
    }

    /// Shows why something failed, for a few seconds.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub fn report(&mut self, message: String, cx: &mut Context<Self>) {
        self.receive(
            Event::Failed {
                error: message,
                quiet: false,
            },
            cx,
        );
    }

    /// Stops the engine line; resolves once it is down.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn shutdown(&self) -> async_channel::Receiver<()> {
        let (done, finished) = async_channel::bounded(1);
        if self.jobs.send(Job::Shutdown(done.clone())).is_err() {
            let _ = done.try_send(());
        }
        finished
    }

    /// The failure not shown yet, once.
    pub fn take_failure(&mut self) -> Option<String> {
        self.failure.take()
    }

    /// Notices not posted yet, oldest first, about computers still paired.
    /// Each comes once. The Linux agent outlives the window, keeps its
    /// earlier notices and posted them itself, so the first snapshot's are
    /// taken as seen.
    pub fn take_notices(&mut self) -> Vec<zflow::hello::Notice> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let newest = snapshot.notices.iter().map(|notice| notice.id).max();
        let seen = self.last_notice.unwrap_or_else(|| newest.unwrap_or(0));
        self.last_notice = Some(newest.unwrap_or(seen).max(seen));
        snapshot
            .notices
            .iter()
            .filter(|notice| notice.id > seen)
            .filter(|notice| {
                snapshot
                    .peers
                    .iter()
                    .any(|peer| peer.name == notice.name && peer.mark == notice.mark)
            })
            .cloned()
            .collect()
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn platform(&self) -> Platform {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.platform.clone())
            .unwrap_or_default()
    }

    fn receive(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Snapshot(snapshot) => {
                if let Page::Computer(name) = &self.page
                    && !snapshot.peers.iter().any(|peer| &peer.name == name)
                {
                    self.page = Page::Computers;
                }
                self.error = None;
                if self.snapshot.as_ref() != Some(&snapshot) {
                    self.snapshot = Some(*snapshot);
                    cx.notify();
                }
            }
            Event::Failed { error, quiet } => {
                if self.snapshot.is_none() || quiet {
                    if self.snapshot.is_none() {
                        self.error = Some(error);
                    }
                } else {
                    self.failure = Some(error);
                }
                cx.notify();
            }
            #[cfg(target_os = "macos")]
            Event::Lens(lens) => {
                self.lens = Some(lens);
                cx.notify();
            }
        }
    }
}

/// The worker: connects, then runs each queued request and polls between
/// them. A request whose answer carries no snapshot is followed by a poll,
/// so the window shows its effect at once.
fn work(
    config: &std::path::Path,
    jobs: &mpsc::Receiver<Job>,
    events: &async_channel::Sender<Event>,
) {
    let send = |event| events.send_blocking(event).is_ok();
    let mut client: Option<Client> = None;
    // A fresh connection asks for a snapshot at once.
    let mut fresh = false;
    loop {
        let Some(line) = &client else {
            match Client::connect(config) {
                Ok(connected) => {
                    #[cfg(target_os = "macos")]
                    if !send(Event::Lens(connected.lens().clone())) {
                        return;
                    }
                    client = Some(connected);
                    fresh = true;
                }
                Err(error) => {
                    if !send(Event::Failed {
                        error: format!("{error:#}"),
                        quiet: true,
                    }) {
                        return;
                    }
                    match jobs.recv() {
                        Ok(Job::Call {
                            reply: Some(reply), ..
                        }) => {
                            let _ = reply.send_blocking(Err(format!("{error:#}")));
                        }
                        Ok(Job::Ask { reply, .. }) => {
                            let _ = reply.send_blocking(Err(format!("{error:#}")));
                        }
                        Ok(Job::Shutdown(done)) => {
                            let _ = done.send_blocking(());
                            return;
                        }
                        Ok(_) => {}
                        Err(_) => return,
                    }
                }
            }
            continue;
        };
        let next = if std::mem::take(&mut fresh) {
            Err(mpsc::RecvTimeoutError::Timeout)
        } else {
            jobs.recv_timeout(POLL)
        };
        let (request, reply) = match next {
            Ok(Job::Call { request, reply }) => (request, reply),
            Ok(Job::Ask { request, reply }) => {
                let answer = line.call(&request).map_err(|error| format!("{error:#}"));
                if reply.send_blocking(answer).is_err() {
                    log::debug!("a question's asker went away");
                }
                continue;
            }
            Ok(Job::Reconnect) => {
                client = None;
                continue;
            }
            Ok(Job::Shutdown(done)) => {
                drop(client.take());
                let _ = done.send_blocking(());
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => (Request::Snapshot, None),
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let polling = matches!(request, Request::Snapshot) && reply.is_none();
        let answer = line.call(&request).and_then(|answer| {
            if matches!(request, Request::Snapshot) || zflow::app::client::ANSWERS_WITH_SNAPSHOT {
                Ok(answer)
            } else {
                line.call(&Request::Snapshot)
            }
        });
        let outcome = answer.and_then(|value| {
            Snapshot::deserialize(value).map_err(|error| {
                anyhow::anyhow!("zflow sent a status this window cannot read: {error}")
            })
        });
        let ok = match outcome {
            Ok(snapshot) => {
                if let Some(reply) = &reply {
                    let _ = reply.send_blocking(Ok(()));
                }
                send(Event::Snapshot(Box::new(snapshot)))
            }
            Err(error) => {
                let error = format!("{error:#}");
                match &reply {
                    Some(reply) => reply.send_blocking(Err(error)).is_ok(),
                    None => send(Event::Failed {
                        error,
                        quiet: polling,
                    }),
                }
            }
        };
        if !ok {
            return;
        }
    }
}

/// What a fake model was asked, in order.
#[cfg(test)]
pub struct Requests(mpsc::Receiver<Job>);

#[cfg(test)]
impl Requests {
    /// The next request, and whether it was a question; a question gets
    /// `answer`.
    pub fn next(&self, answer: serde_json::Value) -> Option<(Request, bool)> {
        match self.0.try_recv().ok()? {
            Job::Call { request, reply } => {
                if let Some(reply) = reply {
                    let _ = reply.try_send(Ok(()));
                }
                Some((request, false))
            }
            Job::Ask { request, reply } => {
                let _ = reply.try_send(Ok(answer));
                Some((request, true))
            }
            Job::Reconnect | Job::Shutdown(_) => None,
        }
    }

    pub fn drain(&self) -> Vec<(Request, bool)> {
        std::iter::from_fn(|| self.next(serde_json::Value::Null)).collect()
    }
}

/// The model, as a global the tray and menus reach.
pub struct Shared(pub Entity<Model>);

impl zz_gpui::Global for Shared {}

pub fn model(cx: &App) -> Entity<Model> {
    cx.global::<Shared>().0.clone()
}

#[cfg(test)]
mod tests {
    use zflow::hello::{Notice, NoticeKind};
    use zz_gpui::{AppContext as _, TestAppContext};

    use super::Model;

    fn joined(id: u64, name: &str, mark: &str) -> Notice {
        Notice {
            id,
            kind: NoticeKind::Joined,
            name: name.into(),
            mark: mark.into(),
        }
    }

    #[zz_gpui::test]
    fn notices_come_once_and_only_about_paired_computers(cx: &mut TestAppContext) {
        let mut snapshot = crate::board::tests::snapshot();
        snapshot.notices = vec![joined(4, "mac", "996615")];
        let model = cx.new(|cx| Model::fake(snapshot.clone(), cx).0);
        let names = |model: &mut Model| -> Vec<String> {
            model
                .take_notices()
                .into_iter()
                .map(|notice| notice.name)
                .collect()
        };

        // The first snapshot's notices were posted before the window opened.
        assert!(model.update(cx, |model, _| names(model)).is_empty());

        // A later join is posted once.
        snapshot.notices.push(joined(6, "mac", "996615"));
        model.update(cx, |model, _| model.snapshot = Some(snapshot.clone()));
        assert_eq!(model.update(cx, |model, _| names(model)), ["mac"]);
        assert!(model.update(cx, |model, _| names(model)).is_empty());

        // Not about a paired computer: a namesake with another key, or one
        // forgotten since.
        snapshot
            .notices
            .extend([joined(7, "mac", "123456"), joined(8, "box", "a4d876")]);
        model.update(cx, |model, _| model.snapshot = Some(snapshot.clone()));
        assert!(model.update(cx, |model, _| names(model)).is_empty());
    }
}
