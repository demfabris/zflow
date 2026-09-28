//! The settings both apps show: one snapshot and one set of requests.
//!
//! The Mac app reads this through the C FFI, and the GNOME window and panel
//! over D-Bus. The top of the settings window is the same on both, in the
//! order of the `Snapshot` fields. Each platform fills `platform` with the
//! rows under it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{layout_model::Layout, nearby::NearbyRecord, pairing::PairingSnapshot};
use crate::{config::PeerConfig, core::KeyboardMode};

#[derive(Serialize)]
pub(super) struct Snapshot<P> {
    pub status: Status,
    /// None while the part that shares input cannot be reached.
    pub sharing: Option<bool>,
    pub health: Vec<Health>,
    /// None while there is no layout to arrange.
    pub layout: Option<Layout>,
    pub peers: Vec<Peer>,
    pub pairing: PairingSnapshot,
    pub nearby: Vec<NearbyRecord>,
    pub shortcuts: Vec<Shortcut>,
    /// None where the app keeps it, as the Mac app does with its login item.
    pub autostart: Option<bool>,
    /// The file with the advanced settings.
    pub config_path: PathBuf,
    pub platform: P,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Status {
    pub state: State,
    /// The other computer while one controls the other.
    pub peer: Option<String>,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum State {
    Ready,
    /// This computer controls `peer`.
    Controlling,
    /// `peer` controls this computer.
    Controlled,
    Paused,
    Checking,
    /// Nothing is paired yet.
    Setup,
    Attention,
}

impl Status {
    /// The most important thing first: a pause or a live crossing says more
    /// than a problem with some other computer.
    pub fn new(sharing: Option<bool>, peers: &[Peer], health: &[Health], checking: bool) -> Self {
        let with = |state| peers.iter().find(|peer| peer.state == state);
        let (state, peer) = match sharing {
            None => (State::Attention, None),
            Some(false) => (State::Paused, None),
            Some(true) => {
                if let Some(peer) = with(PeerState::ControllingThis) {
                    (State::Controlled, Some(peer.name.clone()))
                } else if let Some(peer) = with(PeerState::ControlledFromHere) {
                    (State::Controlling, Some(peer.name.clone()))
                } else if peers.is_empty() {
                    (State::Setup, None)
                } else if health.iter().any(|row| row.level == Level::Error) {
                    (State::Attention, None)
                } else if checking {
                    (State::Checking, None)
                } else {
                    (State::Ready, None)
                }
            }
        };
        let title = match (state, &peer) {
            (State::Controlling, Some(peer)) => format!("Controlling {peer}"),
            (State::Controlled, Some(peer)) => format!("Controlled by {peer}"),
            (State::Ready, _) => "Ready".into(),
            (State::Paused, _) => "Paused".into(),
            (State::Checking, _) => "Checking…".into(),
            (State::Setup, _) => "Pair a computer".into(),
            _ => "Needs attention".into(),
        };
        Self { state, peer, title }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Peer {
    pub name: String,
    pub state: PeerState,
    /// What the row says under the name; an unreachable peer's error.
    pub detail: String,
    /// Whether it may control this computer.
    pub allow_control: bool,
    pub keyboard: KeyboardMode,
    /// Whether its scrolling is turned around here.
    pub reverse_scroll: bool,
}

// Linux does not see connection attempts or link errors yet.
#[cfg_attr(target_os = "linux", allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PeerState {
    Paired,
    Connecting,
    Connected,
    ControllingThis,
    ControlledFromHere,
    Unreachable,
}

impl Peer {
    pub fn new(name: &str, record: &PeerConfig, state: PeerState) -> Self {
        let detail = match state {
            PeerState::Paired => "Paired",
            PeerState::Connecting => "Connecting…",
            PeerState::Connected => "Connected",
            PeerState::ControllingThis => "Controlling this computer",
            PeerState::ControlledFromHere => "Controlled from here",
            PeerState::Unreachable => "Cannot reach this computer",
        };
        Self {
            name: name.to_owned(),
            state,
            detail: detail.into(),
            allow_control: record.permissions.send_normal,
            keyboard: record.keyboard,
            reverse_scroll: record.reverse_scroll,
        }
    }
}

/// One line of the health list. Rows that are fine stay in the list, so
/// people see what was checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Health {
    /// Stays the same between snapshots, so a window can keep the row.
    pub id: String,
    pub level: Level,
    pub title: String,
    pub detail: String,
    pub action: Option<Action>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Level {
    Ok,
    Warning,
    Error,
}

/// A fix button. The window sends `{"command": command}` unless it handles
/// that command itself, as the Mac app does when it opens System Settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Action {
    pub label: String,
    pub command: String,
}

impl Health {
    pub fn new(id: &str, level: Level, title: &str, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            level,
            title: title.into(),
            detail: detail.into(),
            action: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Shortcut {
    pub title: String,
    pub keys: String,
}

/// Everything a window or menu may ask for. A request the platform does not
/// have fails with an error rather than doing nothing.
// Each platform leaves the fields of the other's requests unread.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Request {
    Snapshot,
    SetSharing {
        enabled: bool,
    },
    /// Absent fields stay as they are; see [`crate::peer_view::set_peer`].
    SetPeer {
        name: String,
        allow_control: Option<bool>,
        keyboard: Option<KeyboardMode>,
        reverse_scroll: Option<bool>,
    },
    Forget {
        name: String,
    },
    MoveTile {
        id: String,
        x: i32,
        y: i32,
        tolerance: u32,
    },
    /// Without `address`, listens and shows a code. Otherwise connects to
    /// that IP address, port optional, with the code shown there.
    Pair {
        address: Option<String>,
        code: Option<String>,
    },
    /// Allows or declines the computer that proved this computer's code.
    PairRespond {
        allow: bool,
    },
    PairCancel,
    /// Checks the other computers again now.
    Retry,
    SetAutostart {
        enabled: bool,
    },
    /// Mac: reads the configuration files again.
    Reload,
    /// Mac: blocks AWDL while sharing.
    SetAwdl {
        enabled: bool,
    },
    /// Mac: whether the AWDL helper is installed and answering.
    HelperReady {
        ready: bool,
    },
    /// Mac: asks macOS for Accessibility.
    AllowAccessibility,
    /// Mac: checks Accessibility now instead of at the next tick.
    CheckAccessibility,
    /// Mac: starts looking for computers and checks Local Network access.
    Discover,
    /// Linux: opens the settings window.
    OpenSettings,
    /// Linux: installs and turns on the GNOME extension.
    InstallExtension,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PeerPermissions;

    fn record(allow_control: bool) -> PeerConfig {
        PeerConfig {
            spki_der_hex: "01".into(),
            addresses: Vec::new(),
            permissions: PeerPermissions {
                connect: true,
                send_normal: allow_control,
                receive_normal: true,
                inject_prelogin: false,
            },
            keyboard: KeyboardMode::Mac,
            reverse_scroll: false,
        }
    }

    #[test]
    fn status_puts_pauses_and_crossings_before_problems() {
        let broken = [Health::new("service", Level::Error, "Service", "down")];
        let desk = Peer::new("desk", &record(true), PeerState::Connected);
        let controlling = Peer::new("mac", &record(true), PeerState::ControllingThis);
        let controlled = Peer::new("mac", &record(true), PeerState::ControlledFromHere);
        let status = |sharing, peers: &[Peer], health: &[Health], checking| {
            let status = Status::new(sharing, peers, health, checking);
            (status.state, status.title)
        };
        assert_eq!(status(None, &[], &[], false).0, State::Attention);
        assert_eq!(
            status(
                Some(false),
                std::slice::from_ref(&controlling),
                &broken,
                false
            )
            .0,
            State::Paused
        );
        assert_eq!(
            status(Some(true), &[desk.clone(), controlling], &broken, false),
            (State::Controlled, "Controlled by mac".into())
        );
        assert_eq!(
            status(Some(true), &[controlled, desk.clone()], &[], false),
            (State::Controlling, "Controlling mac".into())
        );
        assert_eq!(status(Some(true), &[], &broken, false).0, State::Setup);
        let one = std::slice::from_ref(&desk);
        assert_eq!(status(Some(true), one, &broken, true).0, State::Attention);
        assert_eq!(status(Some(true), one, &[], true).0, State::Checking);
        let fine = [Health::new("service", Level::Ok, "Service", "Running")];
        assert_eq!(
            status(Some(true), one, &fine, false),
            (State::Ready, "Ready".into())
        );
    }

    #[test]
    fn peers_carry_their_permission_and_keyboard() {
        let peer = Peer::new("desk", &record(false), PeerState::ControlledFromHere);
        let value = serde_json::to_value(&peer).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "name": "desk", "state": "controlled_from_here", "detail": "Controlled from here",
                "allow_control": false, "keyboard": "mac", "reverse_scroll": false,
            })
        );
    }

    #[test]
    fn requests_use_the_shared_names_and_nothing_else() {
        let parse = |json| serde_json::from_str::<Request>(json);
        assert!(matches!(
            parse(r#"{"command":"pair","address":"192.0.2.7","code":"123456"}"#).unwrap(),
            Request::Pair {
                address: Some(_),
                code: Some(_)
            }
        ));
        assert!(matches!(
            parse(r#"{"command":"move_tile","id":"local","x":1,"y":2,"tolerance":8}"#).unwrap(),
            Request::MoveTile { .. }
        ));
        for old in [
            r#"{"command":"pair_start","address":null}"#,
            r#"{"command":"pair","remote":null}"#,
            r#"{"command":"move","id":"local","x":1,"y":2,"tolerance":8}"#,
            r#"{"command":"set_keyboard","name":"desk","mode":"mac"}"#,
            r#"{"command":"set_peer","name":"desk","inject_prelogin":true}"#,
            r#"{"command":"set_sharing","enabled":true,"permissions":{"inject_prelogin":true}}"#,
        ] {
            assert!(parse(old).is_err(), "{old}");
        }
    }

    #[test]
    fn the_snapshot_lists_the_shared_rows_in_window_order() {
        let snapshot = Snapshot {
            status: Status::new(Some(true), &[], &[], false),
            sharing: Some(true),
            health: Vec::new(),
            layout: None,
            peers: Vec::new(),
            pairing: PairingSnapshot::default(),
            nearby: Vec::new(),
            shortcuts: Vec::new(),
            autostart: None,
            config_path: "/etc/zflow/zflow.toml".into(),
            platform: (),
        };
        let text = serde_json::to_string(&snapshot).unwrap();
        let at = |key| text.find(&format!("\"{key}\":")).unwrap();
        let order = [
            "status",
            "sharing",
            "health",
            "layout",
            "peers",
            "pairing",
            "nearby",
            "shortcuts",
            "autostart",
            "config_path",
            "platform",
        ];
        assert!(
            order.windows(2).all(|pair| at(pair[0]) < at(pair[1])),
            "{text}"
        );
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value.as_object().unwrap().len(), order.len());
        assert_eq!(value["status"]["title"], "Pair a computer");
    }
}
