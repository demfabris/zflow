//! Desktop peer metadata, user-confirmed pairing, and desktop session attachment.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::{Config, PeerConfig};

pub const SOCKET_PATH: &str = "/run/zflow-gui/peers.sock";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Snapshot {},
    Desktop {},
    Pair {
        remote: Option<std::net::SocketAddr>,
    },
    PairConfirm {
        name: String,
        authentication_code: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum PairingEvent {
    Ready,
    Confirm {
        peer_label: Option<String>,
        authentication_code: String,
    },
    Paired,
    Error {
        message: String,
    },
}

#[cfg(target_os = "linux")]
pub async fn connect_service() -> anyhow::Result<tokio::net::UnixStream> {
    use anyhow::{Context, bail};
    let expected = nix::unistd::User::from_name("zflow")?
        .context("The zflow service account is not installed")?
        .uid
        .as_raw();
    let stream = tokio::net::UnixStream::connect(SOCKET_PATH).await.context(
        "Cannot reach the desktop API. Install the updated zflow service and restart it",
    )?;
    let uid = crate::control::peer_uid(&stream)?;
    if uid != expected && uid != 0 {
        bail!("The desktop API is not owned by the zflow service");
    }
    Ok(stream)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub peers: BTreeMap<String, PeerConfig>,
    pub discovery: bool,
}

impl Snapshot {
    pub fn from_config(config: &Config) -> Self {
        Self {
            peers: config.peers.clone(),
            discovery: config.transport.discovery,
        }
    }

    pub fn into_display_config(self) -> Config {
        Config {
            peers: self.peers,
            transport: crate::config::TransportConfig {
                discovery: self.discovery,
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

#[cfg(target_os = "linux")]
pub async fn fetch() -> anyhow::Result<Snapshot> {
    use anyhow::Context;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let mut stream = connect_service().await?;
        crate::control::write_message(&mut stream, &Request::Snapshot {}).await?;
        let snapshot: Snapshot = crate::control::read_message(&mut stream).await
            .context("The service denied access or returned an invalid response. Use the active local desktop session")?;
        snapshot.clone().into_display_config().validate()?;
        Ok(snapshot)
    }).await.context("The desktop API did not respond within three seconds")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_protocol_rejects_arbitrary_control_keys_and_permissions() {
        for json in [
            r#"{"command":"activate","peer":"desk"}"#,
            r#"{"command":"local"}"#,
            r#"{"command":"reload_config"}"#,
            r#"{"command":"snapshot","path":"/etc/zflow"}"#,
            r#"{"command":"pair","remote":null,"identity":"attacker"}"#,
            r#"{"command":"pair_confirm","name":"desk","authentication_code":"123456","permissions":{"inject_prelogin":true}}"#,
        ] {
            assert!(serde_json::from_str::<Request>(json).is_err());
        }
    }

    #[test]
    fn snapshot_contains_only_peer_metadata_and_discovery() {
        let mut config = Config::default();
        config.daemon.state_dir = "/private/identity-location".into();
        let value = serde_json::to_value(Snapshot::from_config(&config)).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert!(value.get("peers").is_some());
        assert!(!value.to_string().contains("identity-location"));
    }
}
