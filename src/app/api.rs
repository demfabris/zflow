//! The settings both apps show: one snapshot and one set of requests.
//!
//! The Mac app reads this through the C FFI, and the GNOME window and panel
//! over D-Bus. The top of the settings window is the same on both, in the
//! order of the `Snapshot` fields. Each platform fills `platform` with the
//! rows under it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::layout_model::Layout;
use crate::{
    config::PeerConfig, core::KeyboardMode, hello::Notice, neighbors::Unplaced,
    pairing_window::View as PairingWindow,
};

#[derive(Serialize)]
pub(super) struct Snapshot<P> {
    pub status: Status,
    /// None while the part that shares input cannot be reached.
    pub sharing: Option<bool>,
    pub health: Vec<Health>,
    /// The one-shot window in which a fresh install accepts one computer.
    pub pairing_window: PairingWindow,
    /// None while there is no layout to arrange.
    pub layout: Option<Layout>,
    /// This computer's key mark, drawn on its own tile. None until known.
    pub own_mark: Option<String>,
    /// Computers found but not on the board, for the shelf under it.
    pub unplaced: Vec<Unplaced>,
    pub peers: Vec<Peer>,
    /// None where crossings cannot pause yet.
    pub pause_at_edges: Option<bool>,
    pub shortcuts: Vec<Shortcut>,
    /// None where clipboards are not shared yet.
    pub share_clipboard: Option<bool>,
    /// None where the app keeps it, as the Mac app does with its login item.
    pub autostart: Option<bool>,
    /// The file with the advanced settings.
    pub config_path: PathBuf,
    /// Things to tell people once, such as a computer that joined. Ids grow,
    /// so a window posts each one it has not posted before.
    pub notices: Vec<Notice>,
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
    /// No computer is added yet.
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
            (State::Setup, _) => "Add a computer".into(),
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
    /// Its key's mark, as its tile on the shelf showed it.
    pub mark: String,
}

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
            mark: record
                .spki_der()
                .map(|spki| crate::neighbors::mark(&spki))
                .unwrap_or_default(),
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
    /// Drops a computer from the shelf onto the board, which trusts it. `id`
    /// is its shelf tile's `key:<fingerprint>`; the point is where it landed.
    Place {
        id: String,
        x: i32,
        y: i32,
        tolerance: u32,
    },
    /// Says hello to an IP address, port optional, so a computer mDNS
    /// cannot see shows up on the shelf.
    AddAddress {
        address: String,
    },
    /// Checks the other computers again now.
    Retry,
    SetSwitching {
        pause_at_edges: bool,
    },
    SetClipboard {
        share: bool,
    },
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
    /// Linux: follows the service's log in a terminal.
    OpenLogs,
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
                "mark": crate::neighbors::mark(&[1]),
            })
        );
    }

    #[test]
    fn requests_use_the_shared_names_and_nothing_else() {
        let parse = |json| serde_json::from_str::<Request>(json);
        assert!(matches!(
            parse(r#"{"command":"move_tile","id":"local","x":1,"y":2,"tolerance":8}"#).unwrap(),
            Request::MoveTile { .. }
        ));
        assert!(matches!(
            parse(r#"{"command":"set_clipboard","share":true}"#).unwrap(),
            Request::SetClipboard { share: true }
        ));
        assert!(matches!(
            parse(r#"{"command":"place","id":"key:ab","x":1,"y":2,"tolerance":8}"#).unwrap(),
            Request::Place { .. }
        ));
        assert!(matches!(
            parse(r#"{"command":"add_address","address":"100.64.0.7"}"#).unwrap(),
            Request::AddAddress { .. }
        ));
        for old in [
            r#"{"command":"pair_start","address":null}"#,
            r#"{"command":"pair","address":"192.0.2.7","code":"123456"}"#,
            r#"{"command":"pair_respond","allow":true}"#,
            r#"{"command":"pair_cancel"}"#,
            r#"{"command":"move","id":"local","x":1,"y":2,"tolerance":8}"#,
            r#"{"command":"set_keyboard","name":"desk","mode":"mac"}"#,
            r#"{"command":"set_peer","name":"desk","inject_prelogin":true}"#,
            r#"{"command":"set_sharing","enabled":true,"permissions":{"inject_prelogin":true}}"#,
            r#"{"command":"set_clipboard","share":"yes"}"#,
            r#"{"command":"set_clipboard","share":true,"files":true}"#,
            r#"{"command":"place","id":"key:ab","x":1,"y":2,"tolerance":8,"name":"desk"}"#,
            r#"{"command":"place","id":"key:ab","x":1,"y":2,"tolerance":8,"trust":true}"#,
            r#"{"command":"place","id":"key:ab","x":1,"y":2}"#,
            r#"{"command":"add_address","address":"100.64.0.7","port":43119}"#,
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
            pairing_window: PairingWindow::default(),
            layout: None,
            own_mark: None,
            unplaced: Vec::new(),
            peers: Vec::new(),
            pause_at_edges: None,
            shortcuts: Vec::new(),
            share_clipboard: None,
            autostart: None,
            config_path: "/etc/zflow/zflow.toml".into(),
            notices: Vec::new(),
            platform: (),
        };
        let text = serde_json::to_string(&snapshot).unwrap();
        let at = |key| text.find(&format!("\"{key}\":")).unwrap();
        let order = [
            "status",
            "sharing",
            "health",
            "pairing_window",
            "layout",
            "own_mark",
            "unplaced",
            "peers",
            "pause_at_edges",
            "shortcuts",
            "share_clipboard",
            "autostart",
            "config_path",
            "notices",
            "platform",
        ];
        assert!(
            order.windows(2).all(|pair| at(pair[0]) < at(pair[1])),
            "{text}"
        );
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value.as_object().unwrap().len(), order.len());
        assert_eq!(value["status"]["title"], "Add a computer");
        // Before a platform fills them, the new rows say nothing is going on.
        assert_eq!(value["pairing_window"]["state"], "never");
        assert_eq!(value["unplaced"], serde_json::json!([]));
        assert_eq!(value["notices"], serde_json::json!([]));
    }
}
