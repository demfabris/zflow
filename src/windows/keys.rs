//! USB HID usages to Windows set-1 scan codes. Extended keys keep their E0 bit.
use crate::core::HidUsage;

// (HID keyboard usage, scan code including E0 in bit 8).
const KEYS: &[(u16, u16)] = &[
    (4, 0x1e),
    (5, 0x30),
    (6, 0x2e),
    (7, 0x20),
    (8, 0x12),
    (9, 0x21),
    (10, 0x22),
    (11, 0x23),
    (12, 0x17),
    (13, 0x24),
    (14, 0x25),
    (15, 0x26),
    (16, 0x32),
    (17, 0x31),
    (18, 0x18),
    (19, 0x19),
    (20, 0x10),
    (21, 0x13),
    (22, 0x1f),
    (23, 0x14),
    (24, 0x16),
    (25, 0x2f),
    (26, 0x11),
    (27, 0x2d),
    (28, 0x15),
    (29, 0x2c),
    (30, 0x02),
    (31, 0x03),
    (32, 0x04),
    (33, 0x05),
    (34, 0x06),
    (35, 0x07),
    (36, 0x08),
    (37, 0x09),
    (38, 0x0a),
    (39, 0x0b),
    (40, 0x1c),
    (41, 0x01),
    (42, 0x0e),
    (43, 0x0f),
    (44, 0x39),
    (45, 0x0c),
    (46, 0x0d),
    (47, 0x1a),
    (48, 0x1b),
    (49, 0x2b),
    (51, 0x27),
    (52, 0x28),
    (53, 0x29),
    (54, 0x33),
    (55, 0x34),
    (56, 0x35),
    (57, 0x3a),
    (58, 0x3b),
    (59, 0x3c),
    (60, 0x3d),
    (61, 0x3e),
    (62, 0x3f),
    (63, 0x40),
    (64, 0x41),
    (65, 0x42),
    (66, 0x43),
    (67, 0x44),
    (68, 0x57),
    (69, 0x58),
    (70, 0x137),
    (71, 0x46),
    (73, 0x152),
    (74, 0x147),
    (75, 0x149),
    (76, 0x153),
    (77, 0x14f),
    (78, 0x151),
    (79, 0x14d),
    (80, 0x14b),
    (81, 0x150),
    (82, 0x148),
    (83, 0x145),
    (84, 0x135),
    (85, 0x37),
    (86, 0x4a),
    (87, 0x4e),
    (88, 0x11c),
    (89, 0x4f),
    (90, 0x50),
    (91, 0x51),
    (92, 0x4b),
    (93, 0x4c),
    (94, 0x4d),
    (95, 0x47),
    (96, 0x48),
    (97, 0x49),
    (98, 0x52),
    (99, 0x53),
    (100, 0x56),
    (101, 0x15d),
    (104, 0x64),
    (105, 0x65),
    (106, 0x66),
    (107, 0x67),
    (108, 0x68),
    (109, 0x69),
    (110, 0x6a),
    (111, 0x6b),
    (112, 0x6c),
    (113, 0x6d),
    (114, 0x6e),
    (115, 0x76),
    (135, 0x73),
    (137, 0x7d),
    (224, 0x1d),
    (225, 0x2a),
    (226, 0x38),
    (227, 0x15b),
    (228, 0x11d),
    (229, 0x36),
    (230, 0x138),
    (231, 0x15c),
];
const MEDIA: &[(u16, u16)] = &[
    (0xe2, 0xad),
    (0xe9, 0xaf),
    (0xea, 0xae),
    (0xcd, 0xb3),
    (0xb5, 0xb0),
    (0xb6, 0xb1),
    (0xb7, 0xb2),
];

pub fn captured(scan: u16, extended: bool, vk: u16) -> Option<HidUsage> {
    if vk == 0x13 {
        return Some(HidUsage::keyboard(72));
    }
    if let Some((usage, _)) = MEDIA.iter().find(|(_, v)| *v == vk) {
        return Some(HidUsage::consumer(*usage));
    }
    let scan = scan | if extended { 0x100 } else { 0 };
    KEYS.iter()
        .find(|(_, s)| *s == scan)
        .map(|(h, _)| HidUsage::keyboard(*h))
}

/// (native kind, code, extended). Virtual keys are needed only for Pause/media.
pub fn posted(key: HidUsage) -> Option<(i32, i32, i32)> {
    if key == HidUsage::keyboard(72) {
        return Some((2, 0x13, 0));
    }
    if key.page == crate::core::HidUsagePage::CONSUMER {
        return MEDIA
            .iter()
            .find(|(h, _)| *h == key.usage.0)
            .map(|(_, v)| (2, i32::from(*v), 0));
    }
    if key.page != crate::core::HidUsagePage::KEYBOARD_KEYPAD {
        return None;
    }
    KEYS.iter()
        .find(|(h, _)| *h == key.usage.0)
        .map(|(_, s)| (1, i32::from(s & 255), i32::from(s >> 8)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn physical_keys_round_trip_without_layout_translation() {
        for &(hid, scan) in KEYS {
            let key = HidUsage::keyboard(hid);
            assert_eq!(captured(scan & 255, scan > 255, 0), Some(key));
            assert_eq!(
                posted(key),
                Some((1, i32::from(scan & 255), i32::from(scan >> 8)))
            );
        }
        assert_ne!(
            posted(HidUsage::keyboard(40)),
            posted(HidUsage::keyboard(88))
        );
        assert_ne!(
            posted(HidUsage::keyboard(224)),
            posted(HidUsage::keyboard(228))
        );
    }
    #[test]
    fn media_pause_and_unknown_pages() {
        for &(hid, vk) in MEDIA {
            assert_eq!(captured(0, false, vk), Some(HidUsage::consumer(hid)));
        }
        assert_eq!(posted(HidUsage::keyboard(72)), Some((2, 0x13, 0)));
        assert_eq!(posted(HidUsage::consumer(4)), None);
    }
}
