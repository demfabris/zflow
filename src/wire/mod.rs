//! Framing for the protocol domain model: two magic bytes, a family byte, then
//! the payload. Payloads are the core types in postcard, except desktop
//! metadata, which is JSON. The format has no version field of its own: the
//! QUIC ALPN names the protocol, so a peer on another version never gets here.

mod bounds;
mod codec;
mod error;

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::{
    core::{
        MotionFrame, NegotiatedSession, NegotiationOffer, ProbeMessage, ReliableControlMessage,
    },
    desktop::DesktopMessage,
};

pub use error::WireError;

const MAGIC: [u8; 2] = *b"ZF";
const HEADER_BYTES: usize = 3;

pub const MAX_NEGOTIATION_PAYLOAD_BYTES: usize = 4 * 1_024;
pub const MAX_RELIABLE_PAYLOAD_BYTES: usize = 32 * 1_024;
pub const MAX_MOTION_PAYLOAD_BYTES: usize = 8 * 1_024;
pub const MAX_PROBE_PAYLOAD_BYTES: usize = 128;
pub const MAX_HELLO_PAYLOAD_BYTES: usize = 1_024;
/// A computer's name fits one DNS label, so the mDNS record can carry it.
pub const MAX_NAME_BYTES: usize = 63;
pub const MAX_VERSION_BYTES: usize = 32;
/// Vouches a hello carries, real or random padding.
pub const HELLO_VOUCHES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Family {
    NegotiationOffer = 1,
    NegotiatedSession = 2,
    ReliableControl = 3,
    Motion = 4,
    Probe = 5,
    // 6 was the code pairing offer, before 0.3.0.
    Desktop = 7,
    Hello = 8,
}

impl Family {
    fn maximum_payload_bytes(self) -> usize {
        match self {
            Self::NegotiationOffer | Self::NegotiatedSession => MAX_NEGOTIATION_PAYLOAD_BYTES,
            Self::ReliableControl => MAX_RELIABLE_PAYLOAD_BYTES,
            Self::Motion => MAX_MOTION_PAYLOAD_BYTES,
            Self::Probe => MAX_PROBE_PAYLOAD_BYTES,
            Self::Desktop => crate::desktop::MAX_MESSAGE_BYTES,
            Self::Hello => MAX_HELLO_PAYLOAD_BYTES,
        }
    }
}

impl TryFrom<u8> for Family {
    type Error = WireError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::NegotiationOffer),
            2 => Ok(Self::NegotiatedSession),
            3 => Ok(Self::ReliableControl),
            4 => Ok(Self::Motion),
            5 => Ok(Self::Probe),
            7 => Ok(Self::Desktop),
            8 => Ok(Self::Hello),
            other => Err(WireError::UnknownFamily(other)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireMessage {
    NegotiationOffer(NegotiationOffer),
    NegotiatedSession(NegotiatedSession),
    ReliableControl(ReliableControlMessage),
    Motion(MotionFrame),
    Probe(ProbeMessage),
    Desktop(DesktopMessage),
    Hello(Hello),
}

impl WireMessage {
    pub fn family(&self) -> Family {
        match self {
            Self::NegotiationOffer(_) => Family::NegotiationOffer,
            Self::NegotiatedSession(_) => Family::NegotiatedSession,
            Self::ReliableControl(_) => Family::ReliableControl,
            Self::Motion(_) => Family::Motion,
            Self::Probe(_) => Family::Probe,
            Self::Desktop(_) => Family::Desktop,
            Self::Hello(_) => Family::Hello,
        }
    }
}

/// What a computer says about itself before either side trusts the other.
/// TLS proves the key that sent it; every field is only the sender's word.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Its host name, already reduced to plain text by the sender.
    pub name: String,
    pub os: Os,
    /// Its zflow version, so a person knows which computer to update.
    pub version: String,
    pub input_port: u16,
    /// Addresses it says it has, tried after the one it spoke from.
    pub candidates: Vec<SocketAddr>,
    /// Whether it already trusts the receiver's key. Anyone can claim it,
    /// so the receiver neither shows nor obeys it.
    pub trusts_you: bool,
    /// Room for introducing a third computer later. Random until then.
    pub vouches: [[u8; 16]; HELLO_VOUCHES],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Linux,
    Macos,
    // Append only: postcard encodes the existing variants by ordinal.
    Windows,
}

pub fn encode(message: &WireMessage) -> Result<Vec<u8>, WireError> {
    bounds::validate(message)?;
    frame(message)
}

pub fn decode(bytes: &[u8]) -> Result<WireMessage, WireError> {
    decode_inner(bytes, None)
}

/// Decodes only one family, rejecting a mismatch before the payload is read.
/// Fuzz targets use this to keep each decoder family independent.
pub fn decode_family(bytes: &[u8], expected: Family) -> Result<WireMessage, WireError> {
    decode_inner(bytes, Some(expected))
}

fn frame(message: &WireMessage) -> Result<Vec<u8>, WireError> {
    let family = message.family();
    let header = vec![MAGIC[0], MAGIC[1], family as u8];
    let bytes = match message {
        WireMessage::NegotiationOffer(offer) => codec::encode(offer, header)?,
        WireMessage::NegotiatedSession(session) => codec::encode(session, header)?,
        WireMessage::ReliableControl(message) => codec::encode(message, header)?,
        WireMessage::Motion(frame) => codec::encode(frame, header)?,
        WireMessage::Probe(probe) => codec::encode(probe, header)?,
        WireMessage::Hello(hello) => codec::encode(hello, header)?,
        WireMessage::Desktop(message) => {
            let mut bytes = header;
            serde_json::to_writer(&mut bytes, message)
                .map_err(|error| WireError::Invalid(error.to_string()))?;
            bytes
        }
    };
    check_payload_size(family, bytes.len() - HEADER_BYTES)?;
    Ok(bytes)
}

fn decode_inner(bytes: &[u8], expected: Option<Family>) -> Result<WireMessage, WireError> {
    let ([first, second, family], payload) = bytes
        .split_first_chunk::<HEADER_BYTES>()
        .ok_or(WireError::TooShort)?;
    if [*first, *second] != MAGIC {
        return Err(WireError::BadMagic);
    }
    let family = Family::try_from(*family)?;
    if let Some(expected) = expected
        && family != expected
    {
        return Err(WireError::FamilyMismatch {
            expected,
            actual: family,
        });
    }
    check_payload_size(family, payload.len())?;
    let message = match family {
        Family::NegotiationOffer => WireMessage::NegotiationOffer(codec::decode(payload)?),
        Family::NegotiatedSession => WireMessage::NegotiatedSession(codec::decode(payload)?),
        Family::ReliableControl => WireMessage::ReliableControl(codec::decode(payload)?),
        Family::Motion => WireMessage::Motion(codec::decode(payload)?),
        Family::Probe => WireMessage::Probe(codec::decode(payload)?),
        Family::Hello => WireMessage::Hello(codec::decode(payload)?),
        Family::Desktop => WireMessage::Desktop(
            serde_json::from_slice(payload)
                .map_err(|error| WireError::Invalid(error.to_string()))?,
        ),
    };
    bounds::validate(&message)?;
    Ok(message)
}

fn check_payload_size(family: Family, actual: usize) -> Result<(), WireError> {
    let maximum = family.maximum_payload_bytes();
    if actual > maximum {
        return Err(WireError::SizeLimit {
            what: "payload bytes",
            actual,
            maximum,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::core::*;

    fn session() -> SessionContext {
        SessionContext {
            session_epoch: SessionEpoch([0x11; 16]),
            transport_generation: TransportGeneration(7),
            activation_id: ActivationId(9),
        }
    }

    fn contact(id: u32) -> TouchContact {
        TouchContact {
            id: ContactId(id),
            x: 300,
            y: 400,
            pressure: None,
            major: None,
            minor: None,
            orientation_millidegrees: None,
            tool: TouchTool::Finger,
            source_dimensions: None,
        }
    }

    fn touch_state() -> TouchState {
        TouchState::new([
            TouchContact {
                id: ContactId(1),
                x: 100,
                y: 200,
                pressure: Some(512),
                major: Some(8),
                minor: Some(6),
                orientation_millidegrees: Some(1_500),
                tool: TouchTool::Finger,
                source_dimensions: Some(SourceDimensions {
                    width: 1_920,
                    height: 1_080,
                }),
            },
            contact(2),
        ])
        .unwrap()
    }

    fn anchor() -> MotionAnchor {
        MotionAnchor {
            activation_id: session().activation_id,
            through_motion_sequence: MotionSequence(12),
            sender_capture_time: MonotonicTimeMicros(1_234_567),
            totals: CumulativeMotion::new(100, -90, 8, -7),
            final_touch_state: touch_state(),
            kind: AnchorKind::Checkpoint,
        }
    }

    fn control(payload: ReliableControl) -> WireMessage {
        WireMessage::ReliableControl(ReliableControlMessage {
            session: session(),
            sequence: ControlSequence(33),
            payload,
        })
    }

    fn motion(touch_snapshot: Option<TouchState>) -> MotionFrame {
        MotionFrame {
            session: session(),
            motion_sequence: MotionSequence(34),
            control_watermark: ControlSequence(33),
            sender_capture_time: MonotonicTimeMicros(1_234_890),
            totals: CumulativeMotion::new(120, -80, 9, -6),
            touch_snapshot,
        }
    }

    fn offer() -> NegotiationOffer {
        NegotiationOffer {
            maximum_datagram_size: 1_200,
            supported_capabilities: InputCapabilities::from([
                InputCapability::Keyboard,
                InputCapability::Pointer,
                InputCapability::Scroll,
                InputCapability::Touch,
            ]),
            required_capabilities: InputCapabilities::from([InputCapability::Keyboard]),
            pointer_units: BTreeSet::from([PointerUnit::DeviceUnaccelerated]),
            scroll_fields: ScrollFields {
                high_resolution: true,
                discrete_steps: true,
                ..ScrollFields::default()
            },
            maximum_contacts: 10,
            maximum_receiver_lease_ms: 1_000,
            maximum_checkpoint_bound_ms: 250,
        }
    }

    fn hello() -> Hello {
        Hello {
            name: "workstation".into(),
            os: Os::Linux,
            version: "0.3.0".into(),
            input_port: 43119,
            candidates: vec![
                "192.0.2.1:43119".parse().unwrap(),
                "[2001:db8::1]:43119".parse().unwrap(),
            ],
            trusts_you: true,
            vouches: [[0x5a; 16]; HELLO_VOUCHES],
        }
    }

    fn corpus() -> Vec<WireMessage> {
        vec![
            WireMessage::NegotiationOffer(offer()),
            WireMessage::NegotiatedSession(NegotiatedSession {
                maximum_datagram_size: 1_200,
                capabilities: offer().supported_capabilities,
                pointer_unit: Some(PointerUnit::DeviceUnaccelerated),
                scroll_fields: offer().scroll_fields,
                contact_limit: 10,
                receiver_lease_ms: 900,
                checkpoint_bound_ms: 200,
            }),
            control(ReliableControl::StateSnapshot(StateSnapshot {
                held: HeldState {
                    pressed_keys: BTreeSet::from([HidUsage::keyboard(4)]),
                    pressed_buttons: BTreeSet::from([PointerButton::PRIMARY]),
                    active_touch: touch_state(),
                },
                motion_anchor: anchor(),
            })),
            WireMessage::Motion(motion(Some(touch_state()))),
            WireMessage::Probe(ProbeMessage {
                session: session(),
                payload: ProbePayload::ProbeEcho {
                    sequence: ProbeSequence(22),
                    probe_sent_at: MonotonicTimeMicros(100),
                    received_at: MonotonicTimeMicros(110),
                    echoed_at: MonotonicTimeMicros(111),
                },
            }),
            WireMessage::Hello(hello()),
        ]
    }

    #[test]
    fn every_family_round_trips() {
        for message in corpus() {
            let bytes = encode(&message).unwrap();
            assert_eq!(decode(&bytes).unwrap(), message);
        }
    }

    #[test]
    fn every_control_message_round_trips() {
        let terminal = MotionAnchor {
            kind: AnchorKind::Terminal,
            ..anchor()
        };
        for payload in [
            ReliableControl::Enter,
            ReliableControl::KeyDown {
                key: HidUsage::keyboard(4),
            },
            ReliableControl::KeyUp {
                key: HidUsage::consumer(0xe9),
            },
            ReliableControl::ButtonDown {
                button: PointerButton::PRIMARY,
                anchor: anchor(),
            },
            ReliableControl::ButtonUp {
                button: PointerButton::SECONDARY,
                anchor: anchor(),
            },
            ReliableControl::TouchBegin {
                initial_state: touch_state(),
            },
            ReliableControl::TouchEnd { anchor: terminal },
            ReliableControl::TouchCancel { anchor: anchor() },
            ReliableControl::SnapshotAck(SnapshotAck {
                snapshot_sequence: ControlSequence(4),
                accepted_generation: TransportGeneration(7),
            }),
            ReliableControl::SessionClose {
                reason: SessionCloseReason::PermissionRevoked,
                final_anchor: None,
            },
        ] {
            let message = control(payload);
            assert_eq!(decode(&encode(&message).unwrap()).unwrap(), message);
        }
    }

    #[test]
    fn malformed_headers_and_trailing_bytes_are_rejected() {
        let bytes = encode(&corpus().remove(4)).unwrap();
        assert_eq!(decode(&bytes[..2]), Err(WireError::TooShort));

        let mut bad_magic = bytes.clone();
        bad_magic[0] = b'X';
        assert_eq!(decode(&bad_magic), Err(WireError::BadMagic));

        let mut unknown = bytes.clone();
        unknown[2] = 0xff;
        assert_eq!(decode(&unknown), Err(WireError::UnknownFamily(0xff)));

        let mut trailing = bytes;
        trailing.push(0);
        assert_eq!(decode(&trailing), Err(WireError::TrailingPayload));
    }

    #[test]
    fn family_mismatch_is_rejected_before_payload_decode() {
        let bytes = encode(&corpus().remove(4)).unwrap();
        assert_eq!(
            decode_family(&bytes, Family::Motion),
            Err(WireError::FamilyMismatch {
                expected: Family::Motion,
                actual: Family::Probe,
            })
        );
    }

    #[test]
    fn oversize_payload_is_rejected_before_codec_work() {
        let mut bytes = vec![MAGIC[0], MAGIC[1], Family::Probe as u8];
        bytes.resize(HEADER_BYTES + MAX_PROBE_PAYLOAD_BYTES + 1, 0);
        assert_eq!(
            decode(&bytes),
            Err(WireError::SizeLimit {
                what: "payload bytes",
                actual: MAX_PROBE_PAYLOAD_BYTES + 1,
                maximum: MAX_PROBE_PAYLOAD_BYTES,
            })
        );
    }

    /// A hostile peer skips validation, so every limit must hold again after
    /// decoding.
    #[test]
    fn decoded_messages_over_a_limit_are_rejected() {
        let crowd = || TouchState::new((0..=bounds::MAX_CONTACTS as u32).map(contact)).unwrap();
        let cases = [
            (
                WireMessage::NegotiationOffer(NegotiationOffer {
                    maximum_contacts: bounds::MAX_CONTACTS as u16 + 1,
                    ..offer()
                }),
                "maximum contacts",
            ),
            (
                control(ReliableControl::TouchBegin {
                    initial_state: crowd(),
                }),
                "touch contacts",
            ),
            (
                control(ReliableControl::StateSnapshot(StateSnapshot {
                    held: HeldState {
                        pressed_keys: (0..=bounds::MAX_HELD_KEYS as u16)
                            .map(HidUsage::keyboard)
                            .collect(),
                        ..HeldState::default()
                    },
                    motion_anchor: anchor(),
                })),
                "pressed keys",
            ),
            (WireMessage::Motion(motion(Some(crowd()))), "touch contacts"),
            (
                WireMessage::Hello(Hello {
                    name: "x".repeat(MAX_NAME_BYTES + 1),
                    ..hello()
                }),
                "name bytes",
            ),
            (
                WireMessage::Hello(Hello {
                    version: "9".repeat(MAX_VERSION_BYTES + 1),
                    ..hello()
                }),
                "version bytes",
            ),
            (
                WireMessage::Hello(Hello {
                    candidates: (0..=bounds::MAX_DISCOVERY_CANDIDATES as u16)
                        .map(|port| SocketAddr::from(([192, 0, 2, 1], port + 1)))
                        .collect(),
                    ..hello()
                }),
                "hello candidates",
            ),
        ];
        for (message, limit) in cases {
            assert!(
                matches!(encode(&message), Err(WireError::SizeLimit { what, .. }) if what == limit)
            );
            let bytes = frame(&message).unwrap();
            assert!(
                matches!(decode(&bytes), Err(WireError::SizeLimit { what, .. }) if what == limit)
            );
        }
    }

    #[test]
    fn a_hello_fits_its_family_and_needs_an_input_port() {
        let largest = WireMessage::Hello(Hello {
            name: "n".repeat(MAX_NAME_BYTES),
            version: "v".repeat(MAX_VERSION_BYTES),
            candidates: vec![
                "[2001:db8::1]:65535".parse().unwrap();
                bounds::MAX_DISCOVERY_CANDIDATES
            ],
            ..hello()
        });
        let bytes = encode(&largest).unwrap();
        assert!(bytes.len() - HEADER_BYTES <= MAX_HELLO_PAYLOAD_BYTES);
        // Family 6 carried the code pairing offer and is no longer read.
        let mut retired = bytes.clone();
        retired[2] = 6;
        assert_eq!(decode(&retired), Err(WireError::UnknownFamily(6)));

        let portless = WireMessage::Hello(Hello {
            input_port: 0,
            ..hello()
        });
        assert!(matches!(encode(&portless), Err(WireError::Invalid(_))));
        assert!(matches!(
            decode(&frame(&portless).unwrap()),
            Err(WireError::Invalid(_))
        ));
    }

    #[test]
    fn repeated_touch_contact_ids_are_rejected() {
        // A motion frame is its fields in order, so a tuple can carry a
        // contact list that TouchState itself cannot hold.
        let raw = |contacts: Vec<TouchContact>| {
            let frame = motion(None);
            let body = (
                frame.session,
                frame.motion_sequence,
                frame.control_watermark,
                frame.sender_capture_time,
                frame.totals,
                Some(contacts),
            );
            codec::encode(&body, vec![MAGIC[0], MAGIC[1], Family::Motion as u8]).unwrap()
        };
        let distinct = TouchState::new([contact(1), contact(2)]).unwrap();
        assert_eq!(
            decode(&raw(vec![contact(1), contact(2)])),
            Ok(WireMessage::Motion(motion(Some(distinct))))
        );
        assert!(matches!(
            decode(&raw(vec![contact(1), contact(1)])),
            Err(WireError::Codec(_))
        ));
    }

    #[test]
    fn anchor_activation_must_match_the_session() {
        let message = control(ReliableControl::TouchEnd {
            anchor: MotionAnchor {
                activation_id: ActivationId(999),
                ..anchor()
            },
        });
        let mismatch =
            WireError::Invalid("motion anchor activation differs from the session".into());
        assert_eq!(encode(&message).unwrap_err(), mismatch);
        assert_eq!(decode(&frame(&message).unwrap()).unwrap_err(), mismatch);
    }

    #[test]
    fn desktop_metadata_round_trips_with_bounded_json() {
        use crate::desktop::*;
        let message = WireMessage::Desktop(DesktopMessage::Request {
            id: 1,
            request: DesktopRequest::Prepare {
                monitor: None,
                token: MAX_TOKEN,
                edge: Edge::Right,
                start: 0,
                end: FRACTION_MAX,
                position: 500_000,
            },
        });
        let bytes = encode(&message).unwrap();
        assert_eq!(decode(&bytes).unwrap(), message);
        let mut invalid = bytes;
        // Unknown families fail before metadata is interpreted.
        invalid[2] = 255;
        assert!(matches!(
            decode(&invalid),
            Err(WireError::UnknownFamily(255))
        ));
        assert!(
            encode(&WireMessage::Desktop(DesktopMessage::Request {
                id: 1,
                request: DesktopRequest::Poll {
                    token: MAX_TOKEN + 1
                }
            }))
            .is_err()
        );
        assert!(
            encode(&WireMessage::Desktop(DesktopMessage::Response {
                id: 1,
                response: DesktopResponse::Unavailable {
                    reason: "x".repeat(2000)
                }
            }))
            .is_err()
        );
    }
}
