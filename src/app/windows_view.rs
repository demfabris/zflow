//! The Windows engine's pipe, seen as the shared snapshot and requests.
//!
//! The engine answers `status` with its own fields and takes its own
//! commands (`src/windows/ipc.rs`). The window speaks [`super::api`], so this
//! turns one into the other. The translation is plain data, so it is tested
//! on every platform; only [`Pipe`] needs Windows.
use super::api::{Health, Level, Peer, PeerState, Request, Shortcut, Snapshot, Status};
use crate::{
    app::layout_model::{Layout, Seam},
    config::{PeerConfig, PeerPermissions},
    core::KeyboardMode,
    neighbors::Unplaced,
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

/// What `status` answers, as `src/windows/engine.rs` writes it.
#[derive(Debug, Deserialize)]
struct EngineStatus {
    name: String,
    mark: Option<String>,
    #[serde(default)]
    elevated: bool,
    sharing: bool,
    clipboard: bool,
    available: bool,
    #[serde(default)]
    peers: Vec<EnginePeer>,
    #[serde(default)]
    nearby: Vec<Unplaced>,
    layout: Option<Layout>,
    #[serde(default)]
    seams: Vec<Seam>,
    sending: Option<String>,
    receiving: Option<String>,
    notice: Option<String>,
    pause_at_edges: bool,
    listen: Option<Value>,
    #[serde(default)]
    addresses: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct EnginePeer {
    name: String,
    mark: Option<String>,
    connected: bool,
    /// Why the last attempt to reach it failed.
    status: Option<String>,
    keyboard: KeyboardMode,
    #[serde(default)]
    reverse_scroll: bool,
}

/// The rows the Windows window adds under the shared ones.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowsPlatform {
    pub name: String,
    /// Whether the engine runs as administrator, so input reaches
    /// administrator windows.
    pub elevated: bool,
    /// Whether input can be sent from this desktop now.
    pub available: bool,
    pub listen: Option<String>,
    pub addresses: Vec<String>,
    /// A notice from the engine about a link.
    pub notice: Option<String>,
}

/// The engine's shortcuts, which `src/windows/input.c` holds fixed.
fn shortcuts() -> Vec<Shortcut> {
    vec![
        Shortcut {
            title: "Switch to the other computer".into(),
            keys: "Ctrl+Win+F12".into(),
        },
        Shortcut {
            title: "Return input to this computer".into(),
            keys: "Ctrl+Win+Backspace".into(),
        },
    ]
}

fn text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

/// The shared snapshot for an engine `status` answer.
pub fn snapshot(status: Value, config_path: &Path) -> Result<Snapshot<WindowsPlatform>> {
    let status: EngineStatus = serde_json::from_value(status)?;
    let is = |peer: &Option<String>, name: &str| peer.as_deref() == Some(name);
    let peers: Vec<Peer> = status
        .peers
        .iter()
        .map(|peer| {
            let state = if is(&status.receiving, &peer.name) {
                PeerState::ControllingThis
            } else if is(&status.sending, &peer.name) {
                PeerState::ControlledFromHere
            } else if peer.connected {
                PeerState::Connected
            } else if peer.status.is_some() {
                PeerState::Unreachable
            } else {
                PeerState::Connecting
            };
            let record = PeerConfig {
                spki_der_hex: String::new(),
                addresses: Vec::new(),
                permissions: PeerPermissions {
                    connect: true,
                    send_normal: true,
                    receive_normal: true,
                    inject_prelogin: false,
                },
                keyboard: peer.keyboard,
                reverse_scroll: peer.reverse_scroll,
                share_screen: false,
            };
            let mut row = Peer::new(&peer.name, &record, state);
            if let (PeerState::Unreachable, Some(reason)) = (state, &peer.status) {
                row.detail.clone_from(reason);
            }
            row.mark = peer.mark.clone().unwrap_or_default();
            row
        })
        .collect();
    let mut health = vec![Health::new("engine", Level::Ok, "Input engine", "Running")];
    health.push(if status.available {
        Health::new("desktop", Level::Ok, "Windows desktop", "Ready")
    } else {
        Health::new(
            "desktop",
            Level::Warning,
            "Windows desktop",
            "Waiting for an unlocked Windows desktop",
        )
    });
    if let Some(notice) = &status.notice {
        health.push(Health::new(
            "notice",
            Level::Warning,
            "Connection notice",
            notice.clone(),
        ));
    }
    let connected = peers.iter().any(|peer| {
        matches!(
            peer.state,
            PeerState::Connected | PeerState::ControllingThis | PeerState::ControlledFromHere
        )
    });
    let checking = !connected && peers.iter().any(|peer| peer.state == PeerState::Connecting);
    Ok(Snapshot {
        status: Status::new(Some(status.sharing), &peers, &health, checking),
        sharing: Some(status.sharing),
        health,
        pairing_window: crate::pairing_window::View::default(),
        layout: status.layout,
        seams: status.seams,
        own_mark: status.mark,
        unplaced: status.nearby,
        peers,
        pause_at_edges: Some(status.pause_at_edges),
        shortcuts: shortcuts(),
        share_clipboard: Some(status.clipboard),
        // The app keeps the login item, as the Mac app does.
        autostart: None,
        config_path: config_path.to_owned(),
        notices: Vec::new(),
        platform: WindowsPlatform {
            name: status.name,
            elevated: status.elevated,
            available: status.available,
            listen: status.listen.as_ref().map(text),
            addresses: status.addresses.iter().map(text).collect(),
            notice: status.notice,
        },
    })
}

/// The engine commands that carry out `request`, in order. The last one
/// answers with the status the window shows. `names` maps a found
/// computer's shelf id to its name, for a placed computer's first move.
pub fn commands(request: &Request, name_of: impl Fn(&str) -> Option<String>) -> Result<Vec<Value>> {
    let status = json!({"command": "status"});
    Ok(match request {
        Request::Snapshot | Request::Retry => vec![status],
        Request::SetSharing { enabled } => vec![json!({"command": "sharing", "enabled": enabled})],
        Request::SetPeer {
            name,
            allow_control,
            keyboard,
            reverse_scroll,
            share_screen,
        } => {
            if allow_control.is_some() || share_screen.is_some() {
                bail!("Not available on this computer");
            }
            let mut commands = Vec::new();
            if let Some(mode) = keyboard {
                commands.push(json!({"command": "keyboard", "peer": name, "mode": mode}));
            }
            if let Some(enabled) = reverse_scroll {
                commands
                    .push(json!({"command": "reverse_scroll", "peer": name, "enabled": enabled}));
            }
            commands.push(status);
            commands
        }
        Request::Forget { name } => vec![json!({"command": "forget", "peer": name})],
        Request::MoveTile {
            id,
            x,
            y,
            tolerance,
            computer,
        } => vec![json!({
            "command": "move_tile", "id": id, "x": x, "y": y, "tolerance": tolerance, "computer": computer,
        })],
        Request::Preview {
            id,
            x,
            y,
            tolerance,
            computer,
        } => vec![json!({
            "command": "preview", "id": id, "x": x, "y": y, "tolerance": tolerance, "computer": computer,
        })],
        // Trusting puts its screen beside this one; the move puts it where
        // it was dropped.
        Request::Place {
            id,
            x,
            y,
            tolerance,
        } => {
            let key = id.strip_prefix("key:").unwrap_or(id);
            let mut commands = vec![json!({"command": "trust", "key": key})];
            if let Some(name) = name_of(id) {
                commands.push(json!({
                    "command": "move_tile", "id": format!("peer:{name}"), "x": x, "y": y,
                    "tolerance": tolerance, "computer": true,
                }));
            }
            commands
        }
        Request::AddAddress { address } => {
            vec![json!({"command": "nearby", "address": address}), status]
        }
        Request::SetSwitching { pause_at_edges } => {
            vec![json!({"command": "pause_at_edges", "enabled": pause_at_edges})]
        }
        Request::SetClipboard { share } => vec![json!({"command": "clipboard", "enabled": share})],
        Request::TakeControl { name } => vec![json!({"command": "activate", "peer": name})],
        Request::ReturnHere => vec![json!({"command": "local"})],
        Request::SetAutostart { .. }
        | Request::Reload
        | Request::SetAwdl { .. }
        | Request::HelperReady { .. }
        | Request::AllowAccessibility
        | Request::CheckAccessibility
        | Request::Discover
        | Request::Lens { .. }
        | Request::OpenSettings
        | Request::OpenLogs
        | Request::InstallExtension => bail!("Not available on this computer"),
    })
}

/// The engine's pipe for the configuration at `config`.
#[cfg(windows)]
pub struct Pipe {
    config: std::path::PathBuf,
    runtime: tokio::runtime::Runtime,
}

#[cfg(windows)]
impl Pipe {
    pub fn new(config: &Path) -> Result<Self> {
        Ok(Self {
            config: config.to_owned(),
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
        })
    }

    /// Runs `request` and answers with the shared snapshot, or a move's
    /// preview as the engine gave it.
    pub fn call(&self, request: &Request) -> Result<Value> {
        let names = |id: &str| -> Option<String> {
            let status = self.send(json!({"command": "status"})).ok()?;
            status["nearby"]
                .as_array()?
                .iter()
                .find(|computer| computer["id"] == id)
                .and_then(|computer| computer["name"].as_str().map(str::to_owned))
        };
        let commands = commands(request, names)?;
        let mut answer = Value::Null;
        for command in commands {
            answer = self.send(command)?;
        }
        if matches!(request, Request::Preview { .. }) {
            return Ok(answer);
        }
        if answer.get("peers").is_none() {
            answer = self.send(json!({"command": "status"}))?;
        }
        Ok(serde_json::to_value(snapshot(answer, &self.config)?)?)
    }

    /// The engine's own status, if it answers within `wait`.
    pub fn probe(&self, wait: std::time::Duration) -> Option<Value> {
        let command: crate::windows::ipc::Command =
            serde_json::from_value(json!({"command": "status"})).ok()?;
        self.runtime
            .block_on(async {
                tokio::time::timeout(wait, crate::windows::ipc::request(&self.config, command))
                    .await
            })
            .ok()?
            .ok()
    }

    /// Asks the engine to stop, which lets go of input a peer holds.
    pub fn quit(&self) -> Result<()> {
        self.send(json!({"command": "quit"})).map(drop)
    }

    fn send(&self, command: Value) -> Result<Value> {
        let command: crate::windows::ipc::Command = serde_json::from_value(command)?;
        self.runtime
            .block_on(crate::windows::ipc::request(&self.config, command))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> Value {
        json!({
            "name": "pc", "key": "ab", "mark": "f04ee5", "pid": 7, "elevated": false,
            "sharing": true, "clipboard": true, "available": true,
            "peers": [
                {"name": "mac", "key": "cd", "mark": "996615", "connected": true, "status": null,
                 "keyboard": "mac", "reverse_scroll": false, "addresses": []},
                {"name": "box", "key": "ef", "mark": "a4d876", "connected": false,
                 "status": "timed out", "keyboard": "standard", "reverse_scroll": true, "addresses": []},
            ],
            "nearby": [], "layout": {"monitors": []}, "seams": [], "sending": "mac", "receiving": null,
            "notice": null, "layout_version": 3, "pause_at_edges": false,
            "listen": "0.0.0.0:43119", "addresses": ["192.0.2.4"],
        })
    }

    #[test]
    fn the_engine_status_reads_as_the_shared_snapshot() {
        let snapshot = snapshot(status(), Path::new("C:/zflow.toml")).unwrap();
        assert_eq!(snapshot.status.title, "Controlling mac");
        assert_eq!(snapshot.peers[0].state, PeerState::ControlledFromHere);
        assert_eq!(snapshot.peers[0].mark, "996615");
        assert_eq!(snapshot.peers[1].state, PeerState::Unreachable);
        assert_eq!(snapshot.peers[1].detail, "timed out");
        assert!(snapshot.peers[1].reverse_scroll);
        assert_eq!(snapshot.share_clipboard, Some(true));
        assert_eq!(snapshot.autostart, None);
        assert_eq!(snapshot.platform.listen.as_deref(), Some("0.0.0.0:43119"));
        assert_eq!(snapshot.platform.addresses, vec!["192.0.2.4"]);
        assert_eq!(snapshot.shortcuts.len(), 2);
        // It reads back as the window reads every snapshot.
        let value = serde_json::to_value(&snapshot).unwrap();
        let back: Snapshot<WindowsPlatform> = serde_json::from_value(value).unwrap();
        assert_eq!(back, snapshot);
    }

    #[test]
    fn a_locked_desktop_is_a_warning() {
        let mut locked = status();
        locked["available"] = json!(false);
        let snapshot = snapshot(locked, Path::new("x")).unwrap();
        assert!(
            snapshot
                .health
                .iter()
                .any(|row| row.level == Level::Warning)
        );
    }

    #[test]
    fn requests_become_the_engine_commands() {
        let none = |_: &str| None;
        let commands = |request: &Request| commands(request, none).unwrap();
        assert_eq!(
            commands(&Request::SetPeer {
                name: "mac".into(),
                allow_control: None,
                keyboard: Some(KeyboardMode::Standard),
                reverse_scroll: Some(true),
                share_screen: None,
            }),
            vec![
                json!({"command": "keyboard", "peer": "mac", "mode": "standard"}),
                json!({"command": "reverse_scroll", "peer": "mac", "enabled": true}),
                json!({"command": "status"}),
            ]
        );
        assert_eq!(
            commands(&Request::Forget { name: "mac".into() }),
            vec![json!({"command": "forget", "peer": "mac"})]
        );
        let placed = super::commands(
            &Request::Place {
                id: "key:ab".into(),
                x: 10,
                y: 20,
                tolerance: 8,
            },
            |_| Some("box".into()),
        )
        .unwrap();
        assert_eq!(placed[0], json!({"command": "trust", "key": "ab"}));
        assert_eq!(placed[1]["id"], "peer:box");
        assert!(super::commands(&Request::Lens { name: "mac".into() }, none).is_err());
        assert!(
            super::commands(
                &Request::SetPeer {
                    name: "mac".into(),
                    allow_control: Some(false),
                    keyboard: None,
                    reverse_scroll: None,
                    share_screen: None,
                },
                none
            )
            .is_err()
        );
    }

    #[test]
    fn every_command_is_one_the_engine_takes() {
        // The engine's parser is Windows-only, so its commands are checked
        // by name and fields here, as src/windows/ipc.rs spells them.
        for command in commands(
            &Request::MoveTile {
                id: "local".into(),
                x: 1,
                y: 2,
                tolerance: 3,
                computer: true,
            },
            |_| None,
        )
        .unwrap()
        {
            assert_eq!(command["command"], "move_tile");
            assert_eq!(command["computer"], true);
        }
    }
}
