//! Mac virtual keycodes and HID usages, read one way when capturing and the
//! other way when posting.

use crate::core::{HidUsage, HidUsagePage};

/// Mac keycodes and the keyboard-page usage at the same position. 10 and 50
/// name ANSI positions. On ISO keyboards macOS swaps them, so capture_bridge.c
/// swaps them back and `hid_to_mac_keycode` swaps them again.
const MAC_KEYS: &[(u16, u16)] = &[
    (0, 0x04),
    (1, 0x16),
    (2, 0x07),
    (3, 0x09),
    (4, 0x0b),
    (5, 0x0a),
    (6, 0x1d),
    (7, 0x1b),
    (8, 0x06),
    (9, 0x19),
    (10, 0x64), // left of Z
    (11, 0x05),
    (12, 0x14),
    (13, 0x1a),
    (14, 0x08),
    (15, 0x15),
    (16, 0x1c),
    (17, 0x17),
    (18, 0x1e),
    (19, 0x1f),
    (20, 0x20),
    (21, 0x21),
    (22, 0x23),
    (23, 0x22),
    (24, 0x2e),
    (25, 0x26),
    (26, 0x24),
    (27, 0x2d),
    (28, 0x25),
    (29, 0x27),
    (30, 0x30),
    (31, 0x12),
    (32, 0x18),
    (33, 0x2f),
    (34, 0x0c),
    (35, 0x13),
    (36, 0x28),
    (37, 0x0f),
    (38, 0x0d),
    (39, 0x34),
    (40, 0x0e),
    (41, 0x33),
    (42, 0x31),
    (43, 0x36),
    (44, 0x38),
    (45, 0x11),
    (46, 0x10),
    (47, 0x37),
    (48, 0x2b),
    (49, 0x2c),
    (50, 0x35), // left of 1
    (51, 0x2a),
    (53, 0x29),
    (54, 0xe7),
    (55, 0xe3),
    (56, 0xe1),
    (57, 0x39),
    (58, 0xe2),
    (59, 0xe0),
    (60, 0xe5),
    (61, 0xe6),
    (62, 0xe4),
    (64, 0x6c), // F17
    (65, 0x63),
    (67, 0x55),
    (69, 0x57),
    (71, 0x53),
    (75, 0x54),
    (76, 0x58),
    (78, 0x56),
    (79, 0x6d), // F18
    (80, 0x6e), // F19
    (81, 0x67),
    (82, 0x62),
    (83, 0x59),
    (84, 0x5a),
    (85, 0x5b),
    (86, 0x5c),
    (87, 0x5d),
    (88, 0x5e),
    (89, 0x5f),
    (90, 0x6f), // F20
    (91, 0x60),
    (92, 0x61),
    (93, 0x89), // JIS yen
    (94, 0x87), // JIS underscore (ro)
    (95, 0x85), // JIS keypad comma
    (96, 0x3e),
    (97, 0x3f),
    (98, 0x40),
    (99, 0x3c),
    (100, 0x41),
    (101, 0x42),
    (102, 0x91), // JIS eisu
    (103, 0x44),
    (104, 0x90), // JIS kana
    (105, 0x68),
    (106, 0x6b),
    (107, 0x69),
    (109, 0x43),
    (110, 0x65), // context menu
    (111, 0x45),
    (113, 0x6a),
    (114, 0x49),
    (115, 0x4a),
    (116, 0x4b),
    (117, 0x4c),
    (118, 0x3d),
    (119, 0x4d),
    (120, 0x3b),
    (121, 0x4e),
    (122, 0x3a),
    (123, 0x50),
    (124, 0x4f),
    (125, 0x51),
    (126, 0x52),
];

const PRINT_SCREEN: u16 = 0x46;
const F13: u16 = 105;

/// Consumer-page usages and the `NX_KEYTYPE` macOS posts for them. Power and
/// the rest are dropped.
const MEDIA_KEYS: &[(u16, u32)] = &[
    (0xe9, 0),  // volume up
    (0xea, 1),  // volume down
    (0x6f, 2),  // brightness up
    (0x70, 3),  // brightness down
    (0xe2, 7),  // mute
    (0xb8, 14), // eject
    (0xcd, 16), // play or pause
    (0xb5, 17), // next track
    (0xb6, 18), // previous track
    (0xb3, 19), // fast forward
    (0xb4, 20), // rewind
];

const SHIFT: u64 = 0x0002_0000;
const CONTROL: u64 = 0x0004_0000;
const OPTION: u64 = 0x0008_0000;
const COMMAND: u64 = 0x0010_0000;
const NUMERIC_PAD: u64 = 0x0020_0000;
const HELP: u64 = 0x0040_0000;
const FUNCTION: u64 = 0x0080_0000;

/// Modifier keycodes with their aggregate flag and device bit, the masks
/// `zflow_mac_modifier_pressed` in capture_bridge.c reads back.
const MODIFIERS: &[(u16, u64, u64)] = &[
    (59, CONTROL, 0x0000_0001),
    (56, SHIFT, 0x0000_0002),
    (60, SHIFT, 0x0000_0004),
    (55, COMMAND, 0x0000_0008),
    (54, COMMAND, 0x0000_0010),
    (58, OPTION, 0x0000_0020),
    (61, OPTION, 0x0000_0040),
    (62, CONTROL, 0x0000_2000),
];

/// Bundle identifiers of terminals, where Ctrl keeps meaning Ctrl.
const TERMINALS: &[&str] = &[
    "com.apple.Terminal",
    "com.googlecode.iterm2",
    "com.mitchellh.ghostty",
    "net.kovidgoyal.kitty",
    "org.alacritty",
    "com.github.wez.wezterm",
    "dev.warp.Warp-Stable",
    "co.zeit.hyper",
];

pub fn mac_keycode_to_hid(code: u16) -> Option<HidUsage> {
    MAC_KEYS
        .iter()
        .find(|&&(mac, _)| mac == code)
        .map(|&(_, usage)| HidUsage::keyboard(usage))
}

/// The keycode to post for a keyboard-page usage. `iso` is this Mac's own
/// keyboard type, because the type field on a posted event does not change
/// the characters it types.
pub fn hid_to_mac_keycode(usage: HidUsage, iso: bool) -> Option<u16> {
    if usage.page != HidUsagePage::KEYBOARD_KEYPAD {
        return None;
    }
    // Macs have no PrintScreen. F13 sits in its place on Apple's extended
    // keyboards and reaches apps; F14 and F15 do not, so ScrollLock and Pause
    // map to nothing.
    let code = if usage.usage.0 == PRINT_SCREEN {
        F13
    } else {
        MAC_KEYS.iter().find(|&&(_, hid)| hid == usage.usage.0)?.0
    };
    Some(match code {
        10 | 50 if iso => 60 - code,
        _ => code,
    })
}

/// The `NX_KEYTYPE` to post for a consumer-page usage.
pub fn hid_to_media_key(usage: HidUsage) -> Option<u32> {
    if usage.page != HidUsagePage::CONSUMER {
        return None;
    }
    MEDIA_KEYS
        .iter()
        .find(|&&(hid, _)| hid == usage.usage.0)
        .map(|&(_, key)| key)
}

pub fn is_modifier(code: u16) -> bool {
    MODIFIERS.iter().any(|&(modifier, _, _)| modifier == code)
}

/// Event flags for the held keycodes: each modifier's aggregate flag and its
/// left or right device bit. Other keycodes add nothing.
pub fn modifier_flags(held: impl IntoIterator<Item = u16>) -> u64 {
    let mut flags = 0;
    for code in held {
        if let Some(&(_, aggregate, device)) = MODIFIERS.iter().find(|&&(m, _, _)| m == code) {
            flags |= aggregate | device;
        }
    }
    flags
}

/// Flags a key carries of its own, as CGEventCreateKeyboardEvent sets them
/// on macOS 27: keypad keys are on the numeric pad, function and navigation
/// keys carry Fn, and arrows carry both. Setting an event's flags replaces
/// them, so they go back in.
pub fn key_flags(code: u16) -> u64 {
    match code {
        123..=126 => NUMERIC_PAD | FUNCTION,
        114 => HELP | FUNCTION,
        65 | 67 | 69 | 75 | 76 | 78 | 81..=89 | 91 | 92 => NUMERIC_PAD,
        64 | 71 | 79 | 80 | 96..=101 | 103 | 105..=107 | 109 | 111 | 113 | 115..=122 => FUNCTION,
        _ => 0,
    }
}

pub fn is_terminal(bundle_id: &str) -> bool {
    TERMINALS.contains(&bundle_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(usage: u16) -> Option<u16> {
        hid_to_mac_keycode(HidUsage::keyboard(usage), false)
    }

    #[test]
    fn every_key_round_trips() {
        for &(code, usage) in MAC_KEYS {
            assert_eq!(mac_keycode_to_hid(code), Some(HidUsage::keyboard(usage)));
            assert_eq!(post(usage), Some(code));
        }
    }

    #[test]
    fn maps_main_return_to_hid_return() {
        assert_eq!(mac_keycode_to_hid(36), Some(HidUsage::keyboard(0x28)));
        assert_eq!(post(0x28), Some(36));
    }

    #[test]
    fn maps_iso_jis_context_menu_and_high_function_keys() {
        for (code, usage) in [
            (10, 0x64),
            (64, 0x6c),
            (79, 0x6d),
            (80, 0x6e),
            (90, 0x6f),
            (93, 0x89),
            (94, 0x87),
            (95, 0x85),
            (102, 0x91),
            (104, 0x90),
            (110, 0x65),
        ] {
            assert_eq!(mac_keycode_to_hid(code), Some(HidUsage::keyboard(usage)));
            assert_eq!(post(usage), Some(code));
        }
    }

    #[test]
    fn iso_swaps_only_the_keys_left_of_1_and_z() {
        for &(code, usage) in MAC_KEYS {
            let iso = hid_to_mac_keycode(HidUsage::keyboard(usage), true);
            let expected = match usage {
                0x35 => 10,
                0x64 => 50,
                _ => code,
            };
            assert_eq!(iso, Some(expected), "usage {usage:#x}");
        }
        assert_eq!(post(0x35), Some(50));
        assert_eq!(post(0x64), Some(10));
    }

    #[test]
    fn print_screen_posts_f13_and_keys_macs_lack_post_nothing() {
        assert_eq!(post(0x46), Some(105));
        assert_eq!(post(0x68), Some(105));
        assert_eq!(post(0x69), Some(107));
        assert_eq!(post(0x6a), Some(113));
        for usage in [0x47, 0x48, 0x70, 0x71, 0x72, 0x73] {
            assert_eq!(post(usage), None, "usage {usage:#x}");
        }
        assert_eq!(hid_to_mac_keycode(HidUsage::consumer(0x28), false), None);
    }

    #[test]
    fn media_keys_map_to_nx_key_types() {
        for (usage, key) in [
            (0xe9, 0),
            (0xea, 1),
            (0xe2, 7),
            (0xcd, 16),
            (0xb5, 17),
            (0xb6, 18),
            (0xb3, 19),
            (0xb4, 20),
            (0xb8, 14),
            (0x6f, 2),
            (0x70, 3),
        ] {
            assert_eq!(hid_to_media_key(HidUsage::consumer(usage)), Some(key));
        }
        assert_eq!(hid_to_media_key(HidUsage::consumer(0x30)), None);
        assert_eq!(hid_to_media_key(HidUsage::keyboard(0xe9)), None);
    }

    #[test]
    fn modifier_flags_carry_aggregate_and_side_bits() {
        let pairs = [
            (55, 54, 0x0010_0000, 0x0000_0008, 0x0000_0010),
            (56, 60, 0x0002_0000, 0x0000_0002, 0x0000_0004),
            (58, 61, 0x0008_0000, 0x0000_0020, 0x0000_0040),
            (59, 62, 0x0004_0000, 0x0000_0001, 0x0000_2000),
        ];

        for (left_keycode, right_keycode, aggregate, left, right) in pairs {
            assert!(is_modifier(left_keycode) && is_modifier(right_keycode));
            assert_eq!(modifier_flags([left_keycode]), aggregate | left);
            assert_eq!(modifier_flags([right_keycode]), aggregate | right);
            assert_eq!(
                modifier_flags([left_keycode, right_keycode]),
                aggregate | left | right
            );
        }
        assert_eq!(modifier_flags([55, 56]), 0x0012_000a);
        assert_eq!(modifier_flags([0, 57]), 0);
        assert_eq!(modifier_flags([]), 0);
        assert!(!is_modifier(0) && !is_modifier(57));
    }

    #[test]
    fn keys_keep_the_flags_macos_gives_them() {
        assert_eq!(key_flags(0), 0);
        assert_eq!(key_flags(36), 0);
        assert_eq!(key_flags(55), 0);
        assert_eq!(key_flags(123), 0x00a0_0000);
        assert_eq!(key_flags(126), 0x00a0_0000);
        assert_eq!(key_flags(82), 0x0020_0000);
        assert_eq!(key_flags(76), 0x0020_0000);
        assert_eq!(key_flags(122), 0x0080_0000);
        assert_eq!(key_flags(105), 0x0080_0000);
        assert_eq!(key_flags(117), 0x0080_0000);
        assert_eq!(key_flags(114), 0x00c0_0000);
    }

    #[test]
    fn knows_terminals_by_bundle_id() {
        assert!(is_terminal("com.apple.Terminal"));
        assert!(is_terminal("com.mitchellh.ghostty"));
        assert!(!is_terminal("com.apple.TextEdit"));
        assert!(!is_terminal(""));
    }
}
