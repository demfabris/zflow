//! What the window says, in one place, so the window, the menu and the
//! notifications say it the same way.

use zflow::{
    app::api::{PeerState, State},
    core::KeyboardMode,
    neighbors::{Unplaced, UnplacedState},
    pairing_window::{Reason, State as WindowState, View as PairingWindow},
    wire::Os,
};

/// What this computer is called in the window.
pub const THIS_COMPUTER: &str = if cfg!(target_os = "macos") {
    "This Mac"
} else if cfg!(windows) {
    "This PC"
} else {
    "This Computer"
};

/// "this Mac" in a sentence.
pub const THIS_COMPUTER_LOWER: &str = if cfg!(target_os = "macos") {
    "this Mac"
} else if cfg!(windows) {
    "this PC"
} else {
    "this computer"
};

/// The key that takes input back from any computer.
pub const RETURN_KEYS: &str = if cfg!(target_os = "macos") {
    "⌃⌘⌫"
} else {
    "Ctrl+Super+Backspace"
};

pub const INSTALL_COMMAND: &str = "curl --proto '=https' --tlsv1.2 -fsSL https://github.com/demfabris/zflow/releases/latest/download/install.sh | bash";

/// A paired computer's state, short enough for a chip or a menu row.
pub const fn peer_state(state: PeerState) -> &'static str {
    match state {
        PeerState::Paired => "Not connected",
        PeerState::Connecting => "Connecting…",
        PeerState::Connected => "Connected",
        PeerState::ControllingThis => {
            if cfg!(target_os = "macos") {
                "Controlling this Mac"
            } else if cfg!(windows) {
                "Controlling this PC"
            } else {
                "Controlling this computer"
            }
        }
        PeerState::ControlledFromHere => "Controlled from here",
        PeerState::Unreachable => "Offline",
    }
}

/// Whether it is linked now: input can cross to it.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub const fn linked(state: PeerState) -> bool {
    matches!(
        state,
        PeerState::Connected | PeerState::ControllingThis | PeerState::ControlledFromHere
    )
}

/// The line under the status title.
pub fn status_hint(state: State, peer: Option<&str>) -> String {
    let peer = peer.unwrap_or("the other computer");
    match state {
        State::Ready => "Push the pointer past an edge to move onto another computer.".into(),
        State::Controlling => format!(
            "Move back across the edge to return. {RETURN_KEYS} brings input back and pauses sharing."
        ),
        State::Controlled => format!(
            "{peer} is using {THIS_COMPUTER_LOWER}. Its own keyboard and pointer still work."
        ),
        State::Paused => "Turn on Sharing to move between your computers.".into(),
        State::Checking => "Reaching your other computers…".into(),
        State::Setup => "Add a computer to share this keyboard and pointer.".into(),
        State::Attention => "Fix what is below to keep sharing.".into(),
    }
}

pub const fn keyboard_title(mode: KeyboardMode) -> &'static str {
    match mode {
        KeyboardMode::Standard => "Standard keys",
        KeyboardMode::PcPositions => "PC key positions",
        KeyboardMode::Mac => "Mac shortcuts",
    }
}

/// What the mode does here.
pub const fn keyboard_detail(mode: KeyboardMode) -> &'static str {
    match mode {
        KeyboardMode::Standard => "Keys arrive as sent: Super is Command and Alt is Option.",
        KeyboardMode::PcPositions => {
            "Alt and Super trade places, so each key acts like the Mac key in its spot."
        }
        KeyboardMode::Mac => {
            "Picked for a PC keyboard: Ctrl and Command trade places, so Ctrl+C copies, except in terminals."
        }
    }
}

pub const KEYBOARD_MODES: [KeyboardMode; 3] = [
    KeyboardMode::Standard,
    KeyboardMode::PcPositions,
    KeyboardMode::Mac,
];

/// The line under a found computer's name on the shelf.
pub fn unplaced_detail(computer: &Unplaced) -> String {
    match computer.state {
        UnplacedState::Identifying => "Identifying…".into(),
        UnplacedState::DifferentVersion => "Different zflow version".into(),
        UnplacedState::DuplicateName => "Same name as another".into(),
        UnplacedState::Ready => computer.os.map(os_name).unwrap_or_default().into(),
    }
}

pub const fn os_name(os: Os) -> &'static str {
    match os {
        Os::Linux => "Linux",
        Os::Macos => "macOS",
        Os::Windows => "Windows",
    }
}

/// Only a computer whose key is known, on this version, can be placed.
pub const fn placeable(computer: &Unplaced) -> bool {
    matches!(
        computer.state,
        UnplacedState::Ready | UnplacedState::DuplicateName
    )
}

/// What a fresh install's pairing window means for the person: while it is
/// open, the other computer can finish on its own.
pub fn finish_note(
    window: &PairingWindow,
    other: Option<&str>,
    host: &str,
) -> Option<(String, String)> {
    match window.state {
        WindowState::Open => {
            if let Some(holding) = &window.holding {
                return Some((
                    format!("Adding {}…", holding.name),
                    format!(
                        "It is the only new computer around, so {THIS_COMPUTER_LOWER} accepts it by itself."
                    ),
                ));
            }
            let minutes = window.seconds_left.unwrap_or(0).div_ceil(60).max(1);
            let plural = if minutes == 1 { "minute" } else { "minutes" };
            Some((
                format!("Or finish from {}", other.unwrap_or("the other computer")),
                format!(
                    "Drag {host} into place in zflow on {}. {} is new, so for the next {minutes} {plural} it accepts that by itself.",
                    other.unwrap_or("it"),
                    capitalized(THIS_COMPUTER_LOWER),
                ),
            ))
        }
        WindowState::Closed if window.reason == Some(Reason::Rival) => Some((
            "Drag the one you want".into(),
            format!(
                "More than one new computer showed up, so {THIS_COMPUTER_LOWER} waits for you to choose."
            ),
        )),
        WindowState::Closed if window.reason == Some(Reason::Failed) => Some((
            "Drag the new computer into place".into(),
            format!(
                "{} could not add it by itself.",
                capitalized(THIS_COMPUTER_LOWER)
            ),
        )),
        _ => None,
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// A screen's size: its diagonal in inches, from millimeters, then its pixels.
pub fn screen_size(width_mm: u32, height_mm: u32, width: u32, height: u32) -> String {
    let diagonal = (f64::from(width_mm).hypot(f64::from(height_mm)) / 25.4).round();
    if width_mm == 0 || height_mm == 0 {
        format!("{width} × {height}")
    } else {
        format!("{diagonal}″ · {width} × {height}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zflow::pairing_window::Holding;

    #[test]
    fn the_pairing_note_counts_whole_minutes() {
        let open = PairingWindow {
            state: WindowState::Open,
            seconds_left: Some(61),
            ..PairingWindow::default()
        };
        let (title, detail) = finish_note(&open, Some("desk"), "laptop").unwrap();
        assert_eq!(title, "Or finish from desk");
        assert!(detail.contains("for the next 2 minutes"), "{detail}");
        let last = PairingWindow {
            seconds_left: Some(20),
            ..open.clone()
        };
        assert!(
            finish_note(&last, None, "laptop")
                .unwrap()
                .1
                .contains("1 minute ")
        );
        let holding = PairingWindow {
            holding: Some(Holding {
                name: "desk".into(),
                mark: "abcdef".into(),
                ms_left: 3000,
            }),
            ..open
        };
        assert_eq!(
            finish_note(&holding, None, "laptop").unwrap().0,
            "Adding desk…"
        );
        assert_eq!(finish_note(&PairingWindow::default(), None, "laptop"), None);
    }

    #[test]
    fn screens_say_their_diagonal_when_they_know_it() {
        assert_eq!(screen_size(600, 340, 2560, 1440), "27″ · 2560 × 1440");
        assert_eq!(screen_size(0, 0, 1920, 1080), "1920 × 1080");
    }
}
