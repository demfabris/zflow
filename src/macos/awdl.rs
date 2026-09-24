use std::{process::Stdio, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, Command},
    sync::oneshot,
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval_at, timeout},
};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const RENEW_INTERVAL: Duration = Duration::from_millis(500);

/// Owns the pipe that keeps the helper's two-second AWDL lease alive.
pub(super) struct AwDlLease {
    child: Child,
    input: Option<ChildStdin>,
}

impl AwDlLease {
    pub(super) async fn acquire() -> Result<Self> {
        let executable = std::env::current_exe()?;
        let client = executable
            .parent()
            .context("Missing app directory")?
            .join("zflow-awdl-client");
        let mut command = Command::new(client);
        command.env_clear().current_dir("/");
        Self::start(command, RESPONSE_TIMEOUT).await
    }

    async fn start(mut command: Command, deadline: Duration) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .process_group(0)
            // The helper must survive sender termination long enough to restore AWDL.
            .kill_on_drop(false)
            .spawn()
            .context("could not start AWDL helper")?;
        let input = child.stdin.take();
        let mut lease = Self { child, input };
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
        if let Some(status) = self.child.try_wait()? {
            bail!("AWDL helper exited during remote control: {status}");
        }
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
            self.expect(b"RELEASED\n").await?;
            let status = self.child.wait().await?;
            if !status.success() {
                bail!("AWDL helper could not restore its previous state: {status}");
            }
            Ok(())
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
        self.child
            .stdout
            .as_mut()
            .context("AWDL helper has no response pipe")?
            .read_exact(&mut received)
            .await
            .context("AWDL helper closed before confirming the operation")?;
        if received != expected {
            bail!("unexpected AWDL helper response");
        }
        Ok(())
    }
}

impl Drop for AwDlLease {
    fn drop(&mut self) {
        // EOF restores AWDL on early returns and task cancellation, without a shell.
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

#[cfg(test)]
mod tests {
    use super::*;

    // This fake peer exercises IPC without privileges or network interfaces.
    const PEER: &str = r#"
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

    fn peer(script: &str) -> Command {
        let mut command = Command::new("/bin/bash");
        command.args(["-c", script, "awdl-test"]);
        command
    }

    #[tokio::test]
    async fn lease_acquires_renews_and_releases() {
        let mut lease = AwDlLease::start(peer(PEER), RESPONSE_TIMEOUT)
            .await
            .unwrap();
        lease.renew().await.unwrap();
        lease.release().await.unwrap();
    }

    #[tokio::test]
    async fn dropping_lease_closes_the_pipe_for_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let restored = directory.path().join("restored");
        let mut command = peer(PEER);
        command.arg(&restored);
        let lease = AwDlLease::start(command, RESPONSE_TIMEOUT).await.unwrap();
        drop(lease);
        timeout(RESPONSE_TIMEOUT, async {
            while !restored.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn unresponsive_helper_has_a_bounded_startup() {
        let result =
            AwDlLease::start(peer("IFS= read -r -n 1 command"), Duration::from_millis(30)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn helper_failure_is_not_reported_as_active() {
        assert!(
            AwDlLease::start(peer("printf 'READY\n'; exit 1"), RESPONSE_TIMEOUT)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn helper_exit_during_remote_control_ends_renewal() {
        let mut lease = AwDlLease::start(
            peer("printf 'READY\n'; IFS= read -r -n 1 command; printf 'ACTIVE\n'; exit 1"),
            RESPONSE_TIMEOUT,
        )
        .await
        .unwrap();
        lease.child.wait().await.unwrap();
        assert!(lease.renew().await.is_err());
        assert!(lease.release().await.is_err());
    }

    #[tokio::test]
    async fn a_live_helper_must_acknowledge_renewal() {
        let script = PEER.replace("H) printf 'HELD\\n'", "H) :");
        let mut lease = AwDlLease::start(peer(&script), RESPONSE_TIMEOUT)
            .await
            .unwrap();
        assert!(lease.renew().await.is_err());
    }

    #[tokio::test]
    async fn held_lease_outlives_waits_longer_than_the_lease() {
        let idle = AwDlLease::start(peer(EXPIRING_PEER), RESPONSE_TIMEOUT)
            .await
            .unwrap();
        let held = AwDlLease::start(peer(EXPIRING_PEER), RESPONSE_TIMEOUT)
            .await
            .unwrap()
            .hold();
        // Longer than the fake lease, like a slow Prepare or cleanup.
        tokio::time::sleep(Duration::from_millis(1600)).await;
        assert!(idle.release().await.is_err());
        held.release().await.unwrap();
    }

    #[tokio::test]
    async fn held_lease_reports_a_helper_that_exits() {
        let mut held = AwDlLease::start(
            peer("printf 'READY\n'; IFS= read -r -n 1 command; printf 'ACTIVE\n'; exit 1"),
            RESPONSE_TIMEOUT,
        )
        .await
        .unwrap()
        .hold();
        timeout(RESPONSE_TIMEOUT, held.failed()).await.unwrap();
    }

    #[tokio::test]
    async fn dropping_a_held_lease_releases_it() {
        let directory = tempfile::tempdir().unwrap();
        let restored = directory.path().join("restored");
        let mut command = peer(PEER);
        command.arg(&restored);
        let held = AwDlLease::start(command, RESPONSE_TIMEOUT)
            .await
            .unwrap()
            .hold();
        drop(held);
        timeout(RESPONSE_TIMEOUT, async {
            while !restored.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn restoration_failure_is_reported() {
        let script = PEER.replace("printf 'RELEASED\\n'", "exit 1");
        let lease = AwDlLease::start(peer(&script), RESPONSE_TIMEOUT)
            .await
            .unwrap();
        assert!(lease.release().await.is_err());
    }
}
