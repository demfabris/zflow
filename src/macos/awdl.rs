use std::{fs, os::unix::fs::MetadataExt, path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, Command},
    time::timeout,
};

const HELPER_PATH: &str = "/Library/PrivilegedHelperTools/io.zflow.awdl-helper";
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
pub(super) const RENEW_INTERVAL: Duration = Duration::from_millis(500);

/// Owns the pipe that keeps the helper's two-second AWDL lease alive.
pub(super) struct AwDlLease {
    child: Child,
    input: Option<ChildStdin>,
}

impl AwDlLease {
    pub(super) async fn acquire() -> Result<Self> {
        validate_helper(Path::new(HELPER_PATH)).context(
            "AWDL helper is not installed safely; see README.md for the admin setup step",
        )?;
        let mut command = Command::new(HELPER_PATH);
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

fn validate_helper(path: &Path) -> Result<()> {
    let file = fs::symlink_metadata(path)?;
    if !file.is_file() || file.uid() != 0 || file.mode() & 0o6022 != 0o4000 {
        bail!("AWDL helper must be a root-owned setuid file without group or other write access");
    }
    for directory in path.ancestors().skip(1) {
        let metadata = fs::symlink_metadata(directory)?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            bail!("unsafe AWDL helper directory: {}", directory.display());
        }
    }
    Ok(())
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
    async fn restoration_failure_is_reported() {
        let script = PEER.replace("printf 'RELEASED\\n'", "exit 1");
        let lease = AwDlLease::start(peer(&script), RESPONSE_TIMEOUT)
            .await
            .unwrap();
        assert!(lease.release().await.is_err());
    }

    #[test]
    fn refuses_an_unprivileged_helper() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(validate_helper(file.path()).is_err());
    }
}
