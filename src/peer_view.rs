//! Desktop peer metadata, setup-code pairing, and desktop session attachment.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    config::{Config, PeerConfig},
    core::KeyboardMode,
};

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
    /// Changes what a paired computer may do here; absent fields stay as
    /// they are. See [`set_peer`].
    SetPeer {
        name: String,
        /// Whether it may control this computer.
        allow_control: Option<bool>,
        keyboard: Option<KeyboardMode>,
    },
    /// Whether the focused desktop app is a terminal. The desktop agent sends
    /// this; it never leaves this computer.
    Focus {
        terminal: bool,
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

/// Applies a settings change to a paired computer. Any change also lets this
/// computer send to it, which brings a record saved by one-way pairing up to
/// two-way. Pre-login input stays a root-only setting.
pub fn set_peer(
    peer: &mut PeerConfig,
    allow_control: Option<bool>,
    keyboard: Option<KeyboardMode>,
) {
    peer.permissions.receive_normal = true;
    if let Some(allowed) = allow_control {
        peer.permissions.send_normal = allowed;
    }
    if let Some(mode) = keyboard {
        peer.keyboard = mode;
    }
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
    /// Evdev key names, for the shortcut rows. A service from before these
    /// fields leaves them out.
    #[serde(default)]
    pub activation_chord: Vec<String>,
    #[serde(default)]
    pub escape_chord: Vec<String>,
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
            activation_chord: config.input.activation_chord.clone(),
            escape_chord: config.input.escape_chord.clone(),
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
            r#"{"command":"set_keyboard","name":"desk","mode":"mac"}"#,
            r#"{"command":"set_peer","name":"desk","keyboard":"dvorak"}"#,
            r#"{"command":"set_peer","name":"desk","keyboard":"mac","permissions":{"inject_prelogin":true}}"#,
            r#"{"command":"set_peer","name":"desk","inject_prelogin":true}"#,
            r#"{"command":"set_peer","name":"desk","send_normal":true}"#,
            r#"{"command":"set_peer","name":"desk","allow_control":"yes"}"#,
            r#"{"command":"focus","terminal":true,"peer":"desk"}"#,
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
        assert_eq!(value.as_object().unwrap().len(), 8);
        assert!(value.get("peers").is_some());
        assert_eq!(value["discovery"], false);
        assert!(!value.to_string().contains("identity-location"));
    }

    #[test]
    fn focus_parses_and_stays_out_of_status() {
        assert!(matches!(
            serde_json::from_str(r#"{"command":"focus","terminal":true}"#).unwrap(),
            Request::Focus { terminal: true }
        ));
        assert!(matches!(
            serde_json::from_str(
                r#"{"command":"set_peer","name":"desk","keyboard":"pc_positions"}"#
            )
            .unwrap(),
            Request::SetPeer {
                allow_control: None,
                keyboard: Some(KeyboardMode::PcPositions),
                ..
            }
        ));
        let value = serde_json::to_value(DesktopStatus::from_config(&Config::default())).unwrap();
        for field in ["focus", "terminal"] {
            assert!(value.get(field).is_none());
        }
    }

    #[test]
    fn set_peer_makes_a_one_way_record_two_way_and_leaves_prelogin_alone() {
        use crate::config::PeerPermissions;
        let one_way = PeerPermissions {
            connect: true,
            send_normal: true,
            receive_normal: false,
            inject_prelogin: true,
        };
        let mut peer = PeerConfig {
            spki_der_hex: "01".into(),
            addresses: Vec::new(),
            permissions: one_way,
            keyboard: KeyboardMode::Standard,
        };
        set_peer(&mut peer, None, Some(KeyboardMode::Mac));
        assert_eq!(peer.keyboard, KeyboardMode::Mac);
        assert_eq!(
            peer.permissions,
            PeerPermissions {
                receive_normal: true,
                ..one_way
            }
        );
        set_peer(&mut peer, Some(false), None);
        assert!(!peer.permissions.send_normal && peer.permissions.inject_prelogin);
        assert_eq!(peer.keyboard, KeyboardMode::Mac);
        set_peer(&mut peer, Some(true), None);
        assert!(peer.permissions.send_normal);
    }
}
