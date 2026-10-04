use std::{
    ffi::{CStr, c_char},
    future::Future,
    os::fd::{FromRawFd, OwnedFd},
    pin::Pin,
    sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::unix::pipe,
    sync::oneshot,
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval_at, timeout},
};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const RENEW_INTERVAL: Duration = Duration::from_millis(500);
/// How long the shared lease outlives its last user, so a crossing right
/// after a peer let go of this Mac, or the other way round, keeps it.
const LINGER: Duration = Duration::from_secs(1);
/// Failures in a row before the app shows them.
const TROUBLE_AFTER: u32 = 3;

unsafe extern "C" {
    fn zflow_awdl_lease(
        input: *mut i32,
        output: *mut i32,
        error: *mut c_char,
        error_size: usize,
    ) -> i32;
}

/// Owns the pipes that keep the helper's two-second AWDL lease alive. The
/// helper restores AWDL when the lease is released, expires, or its pipe closes.
pub(super) struct AwDlLease {
    input: Option<pipe::Sender>,
    output: pipe::Receiver,
}

impl AwDlLease {
    pub(super) async fn acquire() -> Result<Self> {
        // The XPC request blocks, so it runs off the async workers.
        let (input, output) = timeout(RESPONSE_TIMEOUT, tokio::task::spawn_blocking(request))
            .await
            .context("AWDL helper did not answer")???;
        Self::start(input, output, RESPONSE_TIMEOUT).await
    }

    async fn start(input: OwnedFd, output: OwnedFd, deadline: Duration) -> Result<Self> {
        let mut lease = Self {
            input: Some(pipe::Sender::from_owned_fd(input)?),
            output: pipe::Receiver::from_owned_fd(output)?,
        };
        timeout(deadline, async {
            lease.expect(b"READY\n").await?;
            lease.send(b'A').await?;
            lease.expect(b"ACTIVE\n").await
        })
        .await
        .context("AWDL helper activation timed out")??;
        Ok(lease)
    }

    pub(super) async fn renew(&mut self) -> Result<()> {
        timeout(RENEW_INTERVAL, async {
            self.send(b'H').await?;
            self.expect(b"HELD\n").await
        })
        .await
        .context("AWDL helper heartbeat timed out")?
    }

    pub(super) async fn release(mut self) -> Result<()> {
        timeout(RESPONSE_TIMEOUT, async {
            self.send(b'R').await?;
            self.input.take();
            self.expect(b"RELEASED\n")
                .await
                .context("AWDL helper could not restore its previous state")
        })
        .await
        .context("AWDL helper restoration timed out")?
    }

    async fn send(&mut self, command: u8) -> Result<()> {
        self.input
            .as_mut()
            .context("AWDL helper pipe is closed")?
            .write_all(&[command])
            .await
            .context("could not contact AWDL helper")
    }

    async fn expect(&mut self, expected: &[u8]) -> Result<()> {
        let mut received = vec![0; expected.len()];
        self.output
            .read_exact(&mut received)
            .await
            .context("AWDL helper closed before confirming the operation")?;
        if received != expected {
            bail!("unexpected AWDL helper response");
        }
        Ok(())
    }
}

fn request() -> Result<(OwnedFd, OwnedFd)> {
    let (mut input, mut output) = (-1, -1);
    let mut error = [0 as c_char; 256];
    // SAFETY: the bridge writes two descriptors and a NUL-terminated message
    // into storage owned by this frame.
    let status =
        unsafe { zflow_awdl_lease(&mut input, &mut output, error.as_mut_ptr(), error.len()) };
    if status != 0 {
        // SAFETY: the bridge always terminates the message within `error`.
        let message = unsafe { CStr::from_ptr(error.as_ptr()) };
        bail!("AWDL helper: {}", message.to_string_lossy());
    }
    // SAFETY: on success both descriptors are open and owned by the caller.
    Ok(unsafe { (OwnedFd::from_raw_fd(input), OwnedFd::from_raw_fd(output)) })
}

impl Drop for AwDlLease {
    fn drop(&mut self) {
        // EOF restores AWDL on early returns and task cancellation.
        self.input.take();
    }
}

/// Renews a lease on its own task, so a slow Prepare or cleanup cannot outlast
/// the helper's two-second lease. Dropping it releases the lease too.
pub(super) struct HeldLease {
    release: oneshot::Sender<()>,
    task: JoinHandle<Result<()>>,
}

impl AwDlLease {
    pub(super) fn hold(mut self) -> HeldLease {
        let (release, mut released) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut renewal = interval_at(Instant::now() + RENEW_INTERVAL, RENEW_INTERVAL);
            renewal.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut released => return self.release().await,
                    _ = renewal.tick() => self.renew().await?,
                }
            }
        });
        HeldLease { release, task }
    }
}

impl HeldLease {
    /// Resolves only when renewal fails; the helper then restores AWDL itself.
    pub(super) async fn failed(&mut self) -> anyhow::Error {
        match (&mut self.task).await {
            Ok(Ok(())) => anyhow!("AWDL lease ended unexpectedly"),
            Ok(Err(error)) => error,
            Err(error) => anyhow!("AWDL lease task stopped: {error}"),
        }
    }

    pub(super) async fn release(self) -> Result<()> {
        let _ = self.release.send(());
        self.task.await.context("AWDL lease task stopped")?
    }

    /// The helper stopped answering heartbeats, and has given AWDL back.
    fn lapsed(&self) -> bool {
        self.task.is_finished()
    }
}

type Acquire =
    Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<HeldLease>> + Send>> + Send + Sync>;

/// One AWDL lease for the whole app. The helper grants one at a time, and
/// this Mac wants AWDL down while a peer controls it and while it sends,
/// which overlap when control changes hands. Everyone shares the lease, and
/// it goes back once nobody has wanted it for a moment. AWDL staying up only
/// costs Wi-Fi lag, so nothing here fails a crossing.
pub(super) struct Shared {
    acquire: Acquire,
    linger: Duration,
    users: Mutex<Users>,
    lease: tokio::sync::Mutex<Option<HeldLease>>,
    /// Failures in a row, and the last one.
    trouble: Mutex<(u32, String)>,
}

/// Who wants AWDL down now, and how many times nobody did.
#[derive(Default)]
struct Users {
    wanting: usize,
    emptied: u64,
}

/// The app's shared lease, from the real helper.
pub(super) fn shared() -> &'static Arc<Shared> {
    static SHARED: LazyLock<Arc<Shared>> = LazyLock::new(|| {
        let acquire: Acquire =
            Box::new(|| Box::pin(async { Ok(AwDlLease::acquire().await?.hold()) }));
        Shared::new(acquire, LINGER)
    });
    &SHARED
}

impl Shared {
    fn new(acquire: Acquire, linger: Duration) -> Arc<Self> {
        Arc::new(Self {
            acquire,
            linger,
            users: Mutex::default(),
            lease: tokio::sync::Mutex::new(None),
            trouble: Mutex::default(),
        })
    }

    fn users(&self) -> MutexGuard<'_, Users> {
        self.users.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wants AWDL down until the guard drops. [`Self::down`] takes it down.
    pub(super) fn want(self: &Arc<Self>) -> Wanted {
        self.users().wanting += 1;
        Wanted(self.clone())
    }

    /// Says why AWDL keeps staying up, once it has failed a few times in a
    /// row.
    pub(super) fn trouble(&self) -> Option<String> {
        let trouble = self.trouble.lock().unwrap_or_else(PoisonError::into_inner);
        (trouble.0 >= TROUBLE_AFTER).then(|| {
            format!(
                "AWDL could not be turned off {} times in a row ({}). Sharing works, with more Wi-Fi lag.",
                trouble.0, trouble.1
            )
        })
    }

    fn failed(&self, peer: &str, error: anyhow::Error) {
        let error = format!("{error:#}");
        tracing::warn!(%peer, %error, "AWDL stays on");
        let mut trouble = self.trouble.lock().unwrap_or_else(PoisonError::into_inner);
        *trouble = (trouble.0 + 1, error);
    }

    /// Takes AWDL down for `peer`, on the lease already held if there is
    /// one, while anyone wants it down. Gives up after `limit`, and logs
    /// why AWDL stays up instead of failing.
    pub(super) async fn down(&self, peer: &str, limit: Duration) {
        let attempt = async {
            let mut lease = self.lease.lock().await;
            if self.users().wanting == 0 {
                return Ok(false);
            }
            if let Some(mut lapsed) = lease.take_if(|held| held.lapsed()) {
                let error = format!("{:#}", lapsed.failed().await);
                tracing::warn!(%error, "AWDL lease lapsed");
            }
            if lease.is_some() {
                tracing::debug!(%peer, "AWDL already off");
                return Ok(false);
            }
            *lease = Some((self.acquire)().await?);
            Ok(true)
        };
        match timeout(limit, attempt).await {
            Ok(Ok(true)) => {
                tracing::info!(%peer, "AWDL off");
                self.trouble
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .0 = 0;
            }
            Ok(Ok(false)) => {}
            Ok(Err(error)) => self.failed(peer, error),
            Err(_) => self.failed(
                peer,
                anyhow!("the AWDL helper did not grant a lease in time"),
            ),
        }
    }

    /// Gives AWDL back once nobody has wanted it for the linger time since
    /// it was `emptied`. Someone who came and went meanwhile waits anew, so
    /// quick crossings back and forth keep AWDL down until the last one.
    async fn restore(&self, emptied: u64) {
        tokio::time::sleep(self.linger).await;
        let mut lease = self.lease.lock().await;
        {
            let users = self.users();
            if users.wanting > 0 || users.emptied != emptied {
                return;
            }
        }
        if let Some(held) = lease.take() {
            match held.release().await {
                Ok(()) => tracing::info!("AWDL back on"),
                Err(error) => {
                    let error = format!("{error:#}");
                    tracing::warn!(%error, "AWDL restoration failed; the helper restores it when the lease lapses");
                }
            }
        }
    }
}

/// Wants AWDL down while it lives.
pub(super) struct Wanted(Arc<Shared>);

impl Drop for Wanted {
    fn drop(&mut self) {
        let emptied = {
            let mut users = self.0.users();
            users.wanting -= 1;
            (users.wanting == 0).then(|| {
                users.emptied += 1;
                users.emptied
            })
        };
        // Outside a runtime nothing can give it back; the helper restores
        // AWDL once the lease lapses.
        if let (Some(emptied), Ok(runtime)) = (emptied, tokio::runtime::Handle::try_current()) {
            let shared = self.0.clone();
            runtime.spawn(async move { shared.restore(emptied).await });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, process::Stdio};
    use tokio::process::{Child, Command};

    // This fake peer exercises the lease protocol without privileges or
    // network interfaces.
    pub(super) const PEER: &str = r#"
printf 'READY\n'
IFS= read -r -n 1 command || exit 1
[ "$command" = A ] || exit 2
printf 'ACTIVE\n'
while IFS= read -r -n 1 command; do
    case "$command" in H) printf 'HELD\n' ;; R) break ;; *) exit 3 ;; esac
done
[ -z "$1" ] || : > "$1"
printf 'RELEASED\n'
"#;

    // Like the real helper, this one gives up when a heartbeat is late.
    const EXPIRING_PEER: &str = r#"
printf 'READY\n'
IFS= read -r -n 1 command || exit 1
[ "$command" = A ] || exit 2
printf 'ACTIVE\n'
while IFS= read -r -t 1 -n 1 command; do
    case "$command" in H) printf 'HELD\n' ;; R) printf 'RELEASED\n'; exit 0 ;; *) exit 3 ;; esac
done
exit 4
"#;

    /// Runs a fake helper and returns the pipe ends the real one hands over.
    fn peer(script: &str, argument: Option<&std::path::Path>) -> (Child, OwnedFd, OwnedFd) {
        let mut command = Command::new("/bin/bash");
        command.args(["-c", script, "awdl-test"]);
        command.args(argument);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap().into_owned_fd().unwrap();
        let output = child.stdout.take().unwrap().into_owned_fd().unwrap();
        (child, input, output)
    }

    pub(super) async fn lease(
        script: &str,
        argument: Option<&std::path::Path>,
    ) -> (Child, AwDlLease) {
        let (child, input, output) = peer(script, argument);
        let lease = AwDlLease::start(input, output, RESPONSE_TIMEOUT)
            .await
            .unwrap();
        (child, lease)
    }

    async fn restored(marker: &std::path::Path) {
        timeout(RESPONSE_TIMEOUT, async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn lease_acquires_renews_and_releases() {
        let (_child, mut lease) = lease(PEER, None).await;
        lease.renew().await.unwrap();
        lease.release().await.unwrap();
    }

    #[tokio::test]
    async fn dropping_lease_closes_the_pipe_for_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("restored");
        let (_child, lease) = lease(PEER, Some(&marker)).await;
        drop(lease);
        restored(&marker).await;
    }

    #[tokio::test]
    async fn unresponsive_helper_has_a_bounded_startup() {
        let (_child, input, output) = peer("IFS= read -r -n 1 command", None);
        let result = AwDlLease::start(input, output, Duration::from_millis(30)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn helper_failure_is_not_reported_as_active() {
        let (_child, input, output) = peer("printf 'READY\n'; exit 1", None);
        assert!(
            AwDlLease::start(input, output, RESPONSE_TIMEOUT)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn helper_exit_during_remote_control_ends_renewal() {
        let script = "printf 'READY\n'; IFS= read -r -n 1 command; printf 'ACTIVE\n'; exit 1";
        let (mut child, mut lease) = lease(script, None).await;
        child.wait().await.unwrap();
        assert!(lease.renew().await.is_err());
        assert!(lease.release().await.is_err());
    }

    #[tokio::test]
    async fn a_live_helper_must_acknowledge_renewal() {
        let script = PEER.replace("H) printf 'HELD\\n'", "H) :");
        let (_child, mut lease) = lease(&script, None).await;
        assert!(lease.renew().await.is_err());
    }

    #[tokio::test]
    async fn held_lease_outlives_waits_longer_than_the_lease() {
        let (_idle_child, idle) = lease(EXPIRING_PEER, None).await;
        let (_held_child, held) = lease(EXPIRING_PEER, None).await;
        let held = held.hold();
        // Longer than the fake lease, like a slow Prepare or cleanup.
        tokio::time::sleep(Duration::from_millis(1600)).await;
        assert!(idle.release().await.is_err());
        held.release().await.unwrap();
    }

    #[tokio::test]
    async fn held_lease_reports_a_helper_that_exits() {
        let script = "printf 'READY\n'; IFS= read -r -n 1 command; printf 'ACTIVE\n'; exit 1";
        let (_child, lease) = lease(script, None).await;
        let mut held = lease.hold();
        timeout(RESPONSE_TIMEOUT, held.failed()).await.unwrap();
    }

    #[tokio::test]
    async fn dropping_a_held_lease_releases_it() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("restored");
        let (_child, lease) = lease(PEER, Some(&marker)).await;
        drop(lease.hold());
        restored(&marker).await;
    }

    #[tokio::test]
    async fn restoration_failure_is_reported() {
        let script = PEER.replace("printf 'RELEASED\\n'", "exit 1");
        let (_child, lease) = lease(&script, None).await;
        assert!(lease.release().await.is_err());
    }

    /// A helper like the real one: one lease at a time, until it has given
    /// AWDL back. Each lease leaves a marker in `directory` once it has.
    fn one_at_a_time(directory: &std::path::Path) -> (Acquire, Arc<Mutex<Vec<PathBuf>>>) {
        let leases = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
        let (directory, given) = (directory.to_owned(), leases.clone());
        let acquire: Acquire = Box::new(move || {
            let (directory, given) = (directory.clone(), given.clone());
            Box::pin(async move {
                let marker = {
                    let mut given = given.lock().unwrap();
                    if given.iter().any(|marker| !marker.exists()) {
                        bail!("AWDL helper: AWDL is already in use");
                    }
                    let marker = directory.join(given.len().to_string());
                    given.push(marker.clone());
                    marker
                };
                let (_helper, lease) = lease(PEER, Some(&marker)).await;
                Ok(lease.hold())
            })
        });
        (acquire, leases)
    }

    #[tokio::test]
    async fn a_crossing_right_after_control_ends_shares_the_lease() {
        let directory = tempfile::tempdir().unwrap();
        let (acquire, leases) = one_at_a_time(directory.path());
        let shared = Shared::new(acquire, Duration::from_millis(200));
        let controlled = shared.want();
        shared.down("xps", RESPONSE_TIMEOUT).await;
        // As in the live test: xps lets go, and 15 ms later this Mac crosses
        // back to it. A second lease would be refused.
        drop(controlled);
        tokio::time::sleep(Duration::from_millis(15)).await;
        let crossing = shared.want();
        shared.down("xps", RESPONSE_TIMEOUT).await;
        assert_eq!(shared.trouble.lock().unwrap().0, 0, "nothing failed");
        let first = leases.lock().unwrap().clone();
        assert_eq!(first.len(), 1, "one lease for both");
        // Past the linger time, the crossing still holds AWDL down.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!first[0].exists(), "AWDL stayed down throughout");
        drop(crossing);
        restored(&first[0]).await;
        // Once given back, the next one takes a new lease.
        let again = shared.want();
        shared.down("xps", RESPONSE_TIMEOUT).await;
        let second = leases.lock().unwrap()[1].clone();
        drop(again);
        restored(&second).await;
    }

    #[tokio::test]
    async fn awdl_stays_down_until_the_last_of_quick_crossings_ends() {
        let directory = tempfile::tempdir().unwrap();
        let (acquire, leases) = one_at_a_time(directory.path());
        let linger = Duration::from_secs(2);
        let shared = Shared::new(acquire, linger);
        let first = shared.want();
        shared.down("ubuntu", RESPONSE_TIMEOUT).await;
        drop(first);
        let first_ended = tokio::time::Instant::now();
        // As in the live test: the person crosses again before AWDL is back.
        tokio::time::sleep(linger / 2).await;
        let second = shared.want();
        shared.down("ubuntu", RESPONSE_TIMEOUT).await;
        drop(second);
        let marker = leases.lock().unwrap()[0].clone();
        // The first crossing's wait is over, the second's is not.
        tokio::time::sleep_until(first_ended + linger + linger / 4).await;
        assert!(!marker.exists(), "AWDL came back mid-burst");
        assert_eq!(leases.lock().unwrap().len(), 1);
        restored(&marker).await;
    }

    #[tokio::test]
    async fn a_lease_that_comes_after_its_user_left_goes_back() {
        let directory = tempfile::tempdir().unwrap();
        let (slow, leases) = one_at_a_time(directory.path());
        let acquire: Acquire = Box::new(move || {
            let lease = slow();
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                lease.await
            })
        });
        let shared = Shared::new(acquire, Duration::from_millis(10));
        let wanted = shared.want();
        let taking = {
            let shared = shared.clone();
            tokio::spawn(async move { shared.down("xps", RESPONSE_TIMEOUT).await })
        };
        tokio::time::sleep(Duration::from_millis(5)).await;
        drop(wanted);
        taking.await.unwrap();
        let marker = leases.lock().unwrap()[0].clone();
        restored(&marker).await;
    }

    #[tokio::test]
    async fn an_awdl_failure_is_logged_and_shown_once_it_keeps_happening() {
        let acquire: Acquire =
            Box::new(|| Box::pin(async { Err(anyhow!("AWDL helper: AWDL is already in use")) }));
        let shared = Shared::new(acquire, Duration::from_millis(10));
        let wanted = shared.want();
        for _ in 1..TROUBLE_AFTER {
            // Returns, so the crossing goes on without the tweak.
            shared.down("xps", RESPONSE_TIMEOUT).await;
            assert_eq!(shared.trouble(), None);
        }
        shared.down("xps", RESPONSE_TIMEOUT).await;
        let trouble = shared.trouble().unwrap();
        assert!(trouble.contains("already in use"), "{trouble}");
        drop(wanted);
    }
}
