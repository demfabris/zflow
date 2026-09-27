//! Desktop peer metadata, setup-code pairing, and desktop session attachment.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::{Config, PeerConfig};

pub const SOCKET_PATH: &str = "/run/zflow-gui/peers.sock";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Status {},
    SetSharing {
        enabled: bool,
    },
    Forget {
        name: String,
    },
    Desktop {},
    /// Listens and shows a fresh setup code, or connects to `remote` with the
    /// code shown on it.
    Pair {
        remote: Option<std::net::SocketAddr>,
        code: Option<String>,
    },
    /// Answers [`PairingEvent::Confirm`] on the same connection.
    PairRespond {
        allow: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum PairingEvent {
    /// The setup code this computer shows while it waits.
    Listening {
        code: String,
    },
    /// A computer proved the code; the person here allows it or not.
    Confirm {
        name: String,
        address: String,
    },
    /// Connected with the code; the other computer's user has to allow it.
    Approving,
    /// Both computers proved the code and this one saved the other as `name`.
    Paired {
        name: String,
    },
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

/// What desktop users may see: sharing state and public peer records, never
/// private paths or keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopStatus {
    pub sharing: bool,
    pub receiving_from: Option<String>,
    pub sending_to: Option<String>,
    pub connected: Vec<String>,
    pub peers: BTreeMap<String, PeerConfig>,
    pub discovery: bool,
}

impl DesktopStatus {
    pub fn from_config(config: &Config) -> Self {
        Self {
            sharing: config.daemon.sharing,
            receiving_from: None,
            sending_to: None,
            connected: Vec::new(),
            peers: config.peers.clone(),
            discovery: config.transport.discovery,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case", deny_unknown_fields)]
pub enum DesktopReply {
    Status(DesktopStatus),
    Ack,
    Error { message: String },
}

#[cfg(target_os = "linux")]
pub async fn request(request: &Request) -> anyhow::Result<DesktopReply> {
    use anyhow::{Context, bail};
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut stream = connect_service().await?;
        crate::control::write_message(&mut stream, request).await?;
        let reply = crate::control::read_message(&mut stream).await
            .context("The service denied access. Use the active, unlocked desktop session and update the service")?;
        if let DesktopReply::Error { message } = reply { bail!("{message}"); }
        Ok(reply)
    }).await.context("The desktop API did not respond within five seconds")?
}

#[cfg(target_os = "linux")]
pub async fn status() -> anyhow::Result<DesktopStatus> {
    match request(&Request::Status {}).await? {
        DesktopReply::Status(status) => Ok(status),
        _ => anyhow::bail!("Unexpected response; update the zflow service"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_protocol_rejects_arbitrary_control_keys_and_permissions() {
        for json in [
            r#"{"command":"set_sharing","enabled":true,"permissions":{"inject_prelogin":true}}"#,
            r#"{"command":"forget","name":"desk","path":"/etc/zflow"}"#,
            r#"{"command":"activate","peer":"desk"}"#,
            r#"{"command":"local"}"#,
            r#"{"command":"reload_config"}"#,
            r#"{"command":"status","path":"/etc/zflow"}"#,
            r#"{"command":"snapshot"}"#,
            r#"{"command":"pair","remote":null,"identity":"attacker"}"#,
            r#"{"command":"pair","remote":null,"code":null,"name":"desk","permissions":{"inject_prelogin":true}}"#,
        ] {
            assert!(serde_json::from_str::<Request>(json).is_err());
        }
    }

    #[test]
    fn status_carries_discovery_but_no_private_configuration() {
        let mut config = Config::default();
        config.daemon.state_dir = "/private/identity-location".into();
        config.transport.discovery = false;
        let value = serde_json::to_value(DesktopStatus::from_config(&config)).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 6);
        assert!(value.get("peers").is_some());
        assert_eq!(value["discovery"], false);
        assert!(!value.to_string().contains("identity-location"));
    }
}
