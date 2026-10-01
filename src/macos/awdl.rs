use std::{
    ffi::{CStr, c_char},
    os::fd::{FromRawFd, OwnedFd},
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
}

/// A held lease from a fake helper that creates `restored` once it gives
/// AWDL back.
#[cfg(test)]
pub(super) async fn fake_held_lease(restored: &std::path::Path) -> HeldLease {
    let (_helper, lease) = tests::lease(tests::PEER, Some(restored)).await;
    lease.hold()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
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
}
