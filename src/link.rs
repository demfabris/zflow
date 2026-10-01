//! What the Mac app and the Linux daemon share about keeping a session to
//! each paired computer: how long to wait between attempts, and how to tell
//! people why one failed.

use std::time::Duration;

use crate::transport::TransportError;

const MAX_RETRY_DELAY: Duration = Duration::from_secs(15);
/// A session that lasted this long was working, so its loss is retried at
/// once rather than after a wait.
pub const STABLE_SESSION: Duration = Duration::from_secs(10);

/// Reconnects at once after a stable session drops, then backs off while
/// attempts fail.
pub fn retry_delay(failures: u32) -> Duration {
    match failures {
        0 => Duration::ZERO,
        failures => Duration::from_secs(1 << (failures - 1).min(4)).min(MAX_RETRY_DELAY),
    }
}

/// A link failure that a person has to fix, rather than a network in the way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fix {
    /// The other computer speaks another zflow protocol version.
    Update,
    /// The other computer's key is not the one paired here.
    PairAgain,
}

impl Fix {
    pub fn of(error: &anyhow::Error) -> Option<Self> {
        error
            .chain()
            .find_map(|cause| match cause.downcast_ref::<TransportError>()? {
                TransportError::InvalidAlpn => Some(Self::Update),
                TransportError::PeerIdentityMismatch => Some(Self::PairAgain),
                _ => None,
            })
    }
}

/// Why `peer` could not be reached, worded for people. Errors without a
/// known fix keep their own text.
pub fn reason(peer: &str, error: &anyhow::Error) -> String {
    match Fix::of(error) {
        Some(Fix::Update) => format!("Update zflow on {peer}"),
        Some(Fix::PairAgain) => format!("{peer} was reset or reinstalled. Pair it again."),
        None => format!("{error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn retries_start_at_once_and_back_off_to_a_cap() {
        let delays = [0, 1, 2, 3, 4, 5, 40].map(retry_delay);
        assert_eq!(delays, [0, 1, 2, 4, 8, 15, 15].map(Duration::from_secs));
    }

    #[test]
    fn version_and_key_mismatches_say_what_to_do() {
        let failed = |error: TransportError| {
            Err::<(), _>(error)
                .context("could not connect to 192.0.2.7:43119")
                .unwrap_err()
        };
        assert_eq!(
            reason("desk", &failed(TransportError::InvalidAlpn)),
            "Update zflow on desk"
        );
        assert_eq!(
            reason("desk", &failed(TransportError::PeerIdentityMismatch)),
            "desk was reset or reinstalled. Pair it again."
        );
        let timeout = anyhow::anyhow!("input connection to 192.0.2.7:43119 timed out");
        assert_eq!(Fix::of(&timeout), None);
        assert_eq!(reason("desk", &timeout), timeout.to_string());
        assert_eq!(
            reason("desk", &failed(TransportError::CriticalStreamClosed)),
            "could not connect to 192.0.2.7:43119: critical control stream ended"
        );
    }
}
