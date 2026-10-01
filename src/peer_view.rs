//! Desktop peer metadata, setup-code pairing, and desktop session attachment.

use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};

use crate::{
    app::layout_model::Layout,
    clipboard::{ClipKind, MAX_CLIP_BYTES},
    config::{Config, PeerConfig},
    core::KeyboardMode,
    desktop::{DesktopRequest, DesktopResponse, Edge, FRACTION_MAX, Point},
};

pub const SOCKET_PATH: &str = "/run/zflow-gui/peers.sock";
/// The largest message between the service and the desktop agent: a whole
/// clip in base64, plus room for the JSON around it. Every other local
/// stream keeps [`crate::control::MAX_CONTROL_MESSAGE`].
pub const MAX_AGENT_MESSAGE: usize =
    MAX_CLIP_BYTES.div_ceil(3) * 4 + crate::control::MAX_CONTROL_MESSAGE;
/// The longest notice the service asks the desktop to show.
const MAX_NOTICE: usize = 256;

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
        reverse_scroll: Option<bool>,
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
    SetSwitching {
        pause_at_edges: bool,
    },
    SetClipboard {
        share: bool,
    },
    /// The pointer pushed against an edge that leads to another computer.
    /// `position` is a fraction of the desktop along that edge, out of
    /// [`FRACTION_MAX`].
    EdgeHit {
        edge: Edge,
        position: u32,
    },
    /// Moves a computer in the shared layout. `id` names a tile of
    /// [`DesktopStatus::layout`]; it lands at `x`, `y` in layout units, or
    /// against an edge within `tolerance` of there.
    MoveTile {
        id: String,
        x: i32,
        y: i32,
        tolerance: u32,
    },
    /// Tries each paired computer that is not connected again now, instead
    /// of after its link's wait.
    Retry {},
}

/// What the service asks the desktop agent to do through GNOME Shell. Only
/// handoff requests can come from another computer. Local requests come from
/// this one, and a session never parses them, so a peer cannot move the
/// pointer or place barriers here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentRequest {
    Handoff(DesktopRequest),
    Local(LocalRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalRequest {
    /// Where the pointer leaves for another computer. Replaces the last set.
    /// With `pause_ms`, the pointer has to rest against an edge that long.
    Edges {
        edges: Vec<OutboundEdge>,
        #[serde(default)]
        pause_ms: u32,
    },
    /// Hides the pointer and keeps the session awake while this computer's
    /// input goes to another one.
    Sending { active: bool },
    /// Puts the pointer back where a crossing returned.
    Warp { position: Point },
    /// Reads the clipboard for the computer the pointer went to. The answer
    /// is an [`AgentReply::Clipboard`].
    ReadClipboard,
    /// Puts a clip from the computer the pointer came from on the clipboard.
    WriteClipboard { kind: ClipKind, data: ClipData },
    /// Shows the person a short notice, such as why a clip stayed here.
    Notify { message: String },
}

/// A range of one outer edge of this desktop, as fractions of its length
/// out of [`FRACTION_MAX`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutboundEdge {
    pub edge: Edge,
    pub start: u32,
    pub end: u32,
}

impl AgentRequest {
    /// A short name for logs.
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Handoff(request) => crate::session::desktop_operation(request),
            Self::Local(LocalRequest::Edges { .. }) => "edges",
            Self::Local(LocalRequest::Sending { .. }) => "sending",
            Self::Local(LocalRequest::Warp { .. }) => "warp",
            Self::Local(LocalRequest::ReadClipboard) => "read_clipboard",
            Self::Local(LocalRequest::WriteClipboard { .. }) => "write_clipboard",
            Self::Local(LocalRequest::Notify { .. }) => "notify",
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Handoff(request) => request.validate(),
            Self::Local(LocalRequest::Edges { edges, .. }) => {
                anyhow::ensure!(
                    edges.len() <= 64
                        && edges
                            .iter()
                            .all(|edge| edge.start < edge.end && edge.end <= FRACTION_MAX),
                    "Invalid outbound edges"
                );
                Ok(())
            }
            Self::Local(LocalRequest::Notify { message }) => {
                anyhow::ensure!(
                    !message.is_empty() && message.len() <= MAX_NOTICE,
                    "Invalid notice"
                );
                Ok(())
            }
            // The agent checks a clip's bytes before writing them.
            Self::Local(_) => Ok(()),
        }
    }
}

/// Clipboard bytes, as base64 in JSON. Debug shows only the size, because
/// logs must never hold clipboard content.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ClipData(pub Vec<u8>);

impl std::fmt::Debug for ClipData {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "ClipData({} bytes)", self.0.len())
    }
}

impl Serialize for ClipData {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&BASE64.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for ClipData {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        BASE64
            .decode(text)
            .map(Self)
            .map_err(|_| serde::de::Error::custom("clipboard data is not base64"))
    }
}

/// What the desktop agent found on the clipboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "clipboard", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClipboardContents {
    Clip {
        kind: ClipKind,
        data: ClipData,
    },
    Empty,
    /// Over [`MAX_CLIP_BYTES`]; only the size leaves GNOME.
    TooLarge {
        bytes: usize,
    },
}

/// What the desktop agent answers the service. Only the agent sends these,
/// on its own stream. A peer's handoff request still gets a plain
/// [`DesktopResponse`], which reads the same here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentReply {
    Desktop(DesktopResponse),
    Clipboard(ClipboardContents),
}

impl AgentReply {
    /// A short name for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Desktop(response) => crate::session::desktop_response_kind(response),
            Self::Clipboard(ClipboardContents::Clip { .. }) => "clip",
            Self::Clipboard(ClipboardContents::Empty) => "empty",
            Self::Clipboard(ClipboardContents::TooLarge { .. }) => "too_large",
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Desktop(response) => response.validate(),
            Self::Clipboard(ClipboardContents::Clip { data, .. }) => {
                anyhow::ensure!(data.0.len() <= MAX_CLIP_BYTES, "Clip over the limit");
                Ok(())
            }
            Self::Clipboard(_) => Ok(()),
        }
    }
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
    reverse_scroll: Option<bool>,
) {
    peer.permissions.receive_normal = true;
    if let Some(allowed) = allow_control {
        peer.permissions.send_normal = allowed;
    }
    if let Some(mode) = keyboard {
        peer.keyboard = mode;
    }
    if let Some(reverse) = reverse_scroll {
        peer.reverse_scroll = reverse;
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
    /// Whether the pointer has to rest against an edge before it crosses.
    #[serde(default)]
    pub pause_at_edges: bool,
    #[serde(default)]
    pub activation_chord: Vec<String>,
    #[serde(default)]
    pub escape_chord: Vec<String>,
    /// This computer's view of the shared layout, with its own tile named
    /// "local". A service from before the layout editor leaves it out.
    #[serde(default)]
    pub layout: Option<Layout>,
    /// Whether the clipboard goes along with the pointer.
    #[serde(default)]
    pub share_clipboard: bool,
    /// The links to paired computers that have no session yet. A computer
    /// in neither this nor `connected` is not dialed: nothing listens where
    /// it was paired. A service from before live links leaves it out.
    #[serde(default)]
    pub links: BTreeMap<String, LinkStatus>,
}

/// A link to a paired computer that has no session yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum LinkStatus {
    Connecting,
    /// The last attempt failed. The link keeps retrying, and `reason` says
    /// why in words for people.
    Unreachable {
        reason: String,
        /// A person has to act, as opposed to a computer that is off or
        /// out of reach.
        #[serde(default)]
        needs_fix: bool,
    },
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
            pause_at_edges: config.switching.pause_at_edges,
            activation_chord: config.input.activation_chord.clone(),
            escape_chord: config.input.escape_chord.clone(),
            layout: None,
            share_clipboard: config.clipboard.share,
            links: BTreeMap::new(),
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
            r#"{"command":"edge_hit","edge":"right","position":5,"peer":"desk"}"#,
            r#"{"command":"warp","position":{"x":1,"y":2}}"#,
            // A move names a tile and a spot, never the shared layout's
            // version, editor, keys or sizes.
            r#"{"command":"move_tile","id":"local","x":1,"y":2,"tolerance":8,"version":9}"#,
            r#"{"command":"move_tile","id":"local","x":1,"y":2,"tolerance":8,"editor":"00"}"#,
            r#"{"command":"move_tile","id":"local","x":1,"y":2,"tolerance":8,"key":"00"}"#,
            r#"{"command":"move_tile","id":"local","x":1,"y":2,"tolerance":8,"width":1}"#,
            r#"{"command":"move_tile","id":"local","x":1,"y":2,"tolerance":8,"peer":"desk"}"#,
            r#"{"command":"move_tile","id":"local","x":1,"y":2,"tolerance":-1}"#,
            r#"{"command":"move_tile","id":"local","x":1,"y":2}"#,
            r#"{"command":"set_clipboard","share":"yes"}"#,
            r#"{"command":"set_clipboard","share":true,"peer":"desk"}"#,
            r#"{"command":"retry","peer":"desk"}"#,
            // Only the service asks the agent for these, on its own stream.
            r#"{"command":"read_clipboard"}"#,
            r#"{"command":"write_clipboard","kind":"text","data":"aGk="}"#,
            r#"{"command":"notify","message":"hi"}"#,
        ] {
            assert!(serde_json::from_str::<Request>(json).is_err(), "{json}");
        }
        assert!(matches!(
            serde_json::from_str(r#"{"command":"set_clipboard","share":true}"#).unwrap(),
            Request::SetClipboard { share: true }
        ));
    }

    #[test]
    fn move_tile_parses_and_an_older_status_without_a_layout_still_reads() {
        assert!(matches!(
            serde_json::from_str(
                r#"{"command":"move_tile","id":"peer:mac","x":-1920,"y":40,"tolerance":150}"#
            )
            .unwrap(),
            Request::MoveTile {
                x: -1920,
                y: 40,
                tolerance: 150,
                ..
            }
        ));
        let mut value =
            serde_json::to_value(DesktopStatus::from_config(&Config::default())).unwrap();
        assert!(value["layout"].is_null());
        value.as_object_mut().unwrap().remove("layout");
        let old: DesktopStatus = serde_json::from_value(value).unwrap();
        assert!(old.layout.is_none());
    }

    #[test]
    fn a_peer_cannot_send_local_desktop_requests() {
        for json in [
            r#"{"command":"warp","position":{"x":1,"y":2}}"#,
            r#"{"command":"edges","edges":[],"pause_ms":0}"#,
            r#"{"command":"sending","active":true}"#,
            r#"{"command":"read_clipboard"}"#,
            r#"{"command":"write_clipboard","kind":"png","data":"iVBORw0KGgo="}"#,
            r#"{"command":"notify","message":"Clipboard not shared"}"#,
        ] {
            assert!(
                serde_json::from_str::<DesktopRequest>(json).is_err(),
                "{json}"
            );
            let local = serde_json::from_str::<AgentRequest>(json).unwrap();
            assert!(matches!(local, AgentRequest::Local(_)));
            assert_eq!(serde_json::to_string(&local).unwrap(), json);
        }
        let handoff =
            serde_json::from_str::<AgentRequest>(r#"{"command":"poll","token":7}"#).unwrap();
        assert_eq!(
            handoff,
            AgentRequest::Handoff(DesktopRequest::Poll { token: 7 })
        );
        let edges = |start, end| {
            AgentRequest::Local(LocalRequest::Edges {
                edges: vec![OutboundEdge {
                    edge: Edge::Right,
                    start,
                    end,
                }],
                pause_ms: 0,
            })
        };
        assert!(edges(0, FRACTION_MAX).validate().is_ok());
        assert!(edges(5, 5).validate().is_err());
        assert!(edges(0, FRACTION_MAX + 1).validate().is_err());
        let notice = |message: &str| {
            AgentRequest::Local(LocalRequest::Notify {
                message: message.into(),
            })
            .validate()
        };
        assert!(notice(&crate::clipboard::too_large(5 << 20)).is_ok());
        assert!(notice("").is_err());
        assert!(notice(&"a".repeat(MAX_NOTICE + 1)).is_err());
    }

    #[test]
    fn a_clip_crosses_the_agent_stream_as_base64_and_only_that_stream_fits_it() {
        // RFC 4648's test vectors.
        for (bytes, text) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            let data = ClipData(bytes.into());
            assert_eq!(serde_json::to_value(&data).unwrap(), text);
            assert_eq!(
                serde_json::from_value::<ClipData>(text.into()).unwrap(),
                data
            );
        }
        // Only what the encoder writes: no stray padding, characters or bits.
        for text in [
            "Zg=", "Zg", "Z===", "Zg==Zg==", "Zm9v\n", "Zm-v", "Zh==", "Zm9=", "====",
        ] {
            assert!(
                serde_json::from_value::<ClipData>(text.into()).is_err(),
                "{text}"
            );
        }
        assert_eq!(
            format!("{:?}", ClipData(b"secret".to_vec())),
            "ClipData(6 bytes)",
            "logs never hold clipboard content"
        );
        let full = AgentRequest::Local(LocalRequest::WriteClipboard {
            kind: ClipKind::Png,
            data: ClipData(vec![0xff; MAX_CLIP_BYTES]),
        });
        let length = serde_json::to_vec(&full).unwrap().len();
        assert!(length > crate::control::MAX_CONTROL_MESSAGE && length <= MAX_AGENT_MESSAGE);
        let reply = AgentReply::Clipboard(ClipboardContents::Clip {
            kind: ClipKind::Text,
            data: ClipData(vec![b'a'; MAX_CLIP_BYTES]),
        });
        assert!(serde_json::to_vec(&reply).unwrap().len() <= MAX_AGENT_MESSAGE);
        assert!(reply.validate().is_ok());
        let over = AgentReply::Clipboard(ClipboardContents::Clip {
            kind: ClipKind::Text,
            data: ClipData(vec![b'a'; MAX_CLIP_BYTES + 1]),
        });
        assert!(over.validate().is_err());
    }

    #[test]
    fn handoff_replies_read_the_same_and_a_clip_is_never_one() {
        use crate::desktop::{Geometry, Rect};
        let geometry = Geometry {
            monitors: vec![Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }],
        };
        for response in [
            DesktopResponse::Snapshot {
                geometry: geometry.clone(),
                position: Point { x: 5, y: 5 },
            },
            DesktopResponse::Prepared {
                geometry,
                position: Point { x: 3, y: 540 },
            },
            DesktopResponse::Active,
            DesktopResponse::Returned { position: 7 },
            DesktopResponse::Finished,
            DesktopResponse::unavailable("GNOME integration unavailable"),
        ] {
            let text = serde_json::to_string(&response).unwrap();
            let reply = AgentReply::Desktop(response);
            assert_eq!(serde_json::to_string(&reply).unwrap(), text);
            assert_eq!(serde_json::from_str::<AgentReply>(&text).unwrap(), reply);
        }
        for (contents, kind) in [
            (
                ClipboardContents::Clip {
                    kind: ClipKind::Text,
                    data: ClipData(b"hi".to_vec()),
                },
                "clip",
            ),
            (ClipboardContents::Empty, "empty"),
            (ClipboardContents::TooLarge { bytes: 5 << 20 }, "too_large"),
        ] {
            let text = serde_json::to_string(&contents).unwrap();
            assert!(
                serde_json::from_str::<DesktopResponse>(&text).is_err(),
                "{text}"
            );
            let reply = serde_json::from_str::<AgentReply>(&text).unwrap();
            assert_eq!(reply.kind(), kind);
            assert_eq!(reply, AgentReply::Clipboard(contents));
        }
        assert_eq!(
            serde_json::to_string(&ClipboardContents::Clip {
                kind: ClipKind::Png,
                data: ClipData(vec![0x89, b'P']),
            })
            .unwrap(),
            r#"{"clipboard":"clip","kind":"png","data":"iVA="}"#
        );
    }

    #[test]
    fn status_carries_discovery_but_no_private_configuration() {
        let mut config = Config::default();
        config.daemon.state_dir = "/private/identity-location".into();
        config.transport.discovery = false;
        config.clipboard.share = true;
        let value = serde_json::to_value(DesktopStatus::from_config(&config)).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 12);
        assert!(value.get("peers").is_some());
        assert_eq!(value["discovery"], false);
        assert_eq!(value["share_clipboard"], true);
        assert!(!value.to_string().contains("identity-location"));
        // A service from before clipboard sharing or live links leaves them out.
        let mut old = value;
        old.as_object_mut().unwrap().remove("share_clipboard");
        old.as_object_mut().unwrap().remove("links");
        let old = serde_json::from_value::<DesktopStatus>(old).unwrap();
        assert!(!old.share_clipboard && old.links.is_empty());
        let down = LinkStatus::Unreachable {
            reason: "Reset or reinstalled. Pair it again.".into(),
            needs_fix: true,
        };
        assert_eq!(
            serde_json::to_string(&down).unwrap(),
            r#"{"state":"unreachable","reason":"Reset or reinstalled. Pair it again.","needs_fix":true}"#
        );
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
            reverse_scroll: false,
        };
        set_peer(&mut peer, None, Some(KeyboardMode::Mac), None);
        assert_eq!(peer.keyboard, KeyboardMode::Mac);
        assert_eq!(
            peer.permissions,
            PeerPermissions {
                receive_normal: true,
                ..one_way
            }
        );
        set_peer(&mut peer, Some(false), None, None);
        assert!(!peer.permissions.send_normal && peer.permissions.inject_prelogin);
        assert_eq!(peer.keyboard, KeyboardMode::Mac);
        set_peer(&mut peer, Some(true), None, None);
        assert!(peer.permissions.send_normal);
        assert!(!peer.reverse_scroll);
        set_peer(&mut peer, None, None, Some(true));
        assert!(peer.reverse_scroll && peer.permissions.send_normal);
    }
}
