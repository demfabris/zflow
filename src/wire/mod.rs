//! Bounded, self-describing framing for the protocol domain model.
//!
//! The envelope is parsed before serde sees a byte: magic, family/type,
//! payload length, then (for session traffic) epoch/generation/activation and
//! channel sequences. The format has no version field of its own: the QUIC
//! ALPN names the protocol, so a peer on another version never gets here.

mod bounds;
mod codec;
mod error;
mod model;

use crate::core::*;

use bounds::BoundError;
pub use error::WireError;
pub use model::PairingOffer;
use model::{
    WireMotionBody, WireNegotiatedSession, WireNegotiationOffer, WirePairingOffer,
    WireReliableControl,
};

const MAGIC: [u8; 2] = *b"ZF";
const FIXED_HEADER_BYTES: usize = 8;

pub const MAX_NEGOTIATION_PAYLOAD_BYTES: usize = 4 * 1_024;
pub const MAX_RELIABLE_PAYLOAD_BYTES: usize = 32 * 1_024;
pub const MAX_MOTION_PAYLOAD_BYTES: usize = 8 * 1_024;
pub const MAX_PROBE_PAYLOAD_BYTES: usize = 128;
pub const MAX_PAIRING_PAYLOAD_BYTES: usize = 2 * 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Family {
    Negotiation = 1,
    ReliableControl = 2,
    Motion = 3,
    Probe = 4,
    Pairing = 5,
    Desktop = 6,
}

impl Family {
    fn maximum_payload_bytes(self) -> usize {
        match self {
            Self::Negotiation => MAX_NEGOTIATION_PAYLOAD_BYTES,
            Self::ReliableControl => MAX_RELIABLE_PAYLOAD_BYTES,
            Self::Motion => MAX_MOTION_PAYLOAD_BYTES,
            Self::Probe => MAX_PROBE_PAYLOAD_BYTES,
            Self::Pairing => MAX_PAIRING_PAYLOAD_BYTES,
            Self::Desktop => crate::desktop::MAX_MESSAGE_BYTES + 8,
        }
    }

    fn requires_session(self) -> bool {
        matches!(self, Self::ReliableControl | Self::Motion | Self::Probe)
    }

    fn valid_message_type(self, message_type: u8) -> bool {
        match self {
            Self::Negotiation => matches!(message_type, 1 | 2),
            Self::ReliableControl => (1..=11).contains(&message_type),
            Self::Motion | Self::Pairing | Self::Desktop => message_type == 1,
            Self::Probe => matches!(message_type, 1 | 2),
        }
    }
}

impl TryFrom<u8> for Family {
    type Error = WireError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Negotiation),
            2 => Ok(Self::ReliableControl),
            3 => Ok(Self::Motion),
            4 => Ok(Self::Probe),
            5 => Ok(Self::Pairing),
            6 => Ok(Self::Desktop),
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
    Pairing(PairingOffer),
    Desktop(crate::desktop::DesktopMessage),
}

impl WireMessage {
    pub fn family(&self) -> Family {
        match self {
            Self::NegotiationOffer(_) | Self::NegotiatedSession(_) => Family::Negotiation,
            Self::ReliableControl(_) => Family::ReliableControl,
            Self::Motion(_) => Family::Motion,
            Self::Probe(_) => Family::Probe,
            Self::Pairing(_) => Family::Pairing,
            Self::Desktop(_) => Family::Desktop,
        }
    }
}

pub fn encode(message: &WireMessage) -> Result<Vec<u8>, WireError> {
    let encoded = prepare_payload(message)?;
    if encoded.payload.len() > encoded.family.maximum_payload_bytes() {
        return Err(WireError::SizeLimit {
            what: "payload",
            actual: encoded.payload.len(),
            maximum: encoded.family.maximum_payload_bytes(),
        });
    }

    let payload_length =
        u32::try_from(encoded.payload.len()).map_err(|_| WireError::SizeLimit {
            what: "payload",
            actual: encoded.payload.len(),
            maximum: u32::MAX as usize,
        })?;

    let mut bytes = Vec::with_capacity(
        FIXED_HEADER_BYTES
            + encoded.session.map_or(0, |_| 32)
            + encoded.channel.encoded_len()
            + encoded.payload.len(),
    );
    bytes.extend_from_slice(&MAGIC);
    bytes.push(encoded.family as u8);
    bytes.push(encoded.message_type);
    bytes.extend_from_slice(&payload_length.to_le_bytes());

    if let Some(session) = encoded.session {
        bytes.extend_from_slice(&session.session_epoch.0);
        bytes.extend_from_slice(&session.transport_generation.0.to_le_bytes());
        bytes.extend_from_slice(&session.activation_id.0.to_le_bytes());
    }
    encoded.channel.encode(&mut bytes);
    bytes.extend_from_slice(&encoded.payload);
    Ok(bytes)
}

pub fn decode(bytes: &[u8]) -> Result<WireMessage, WireError> {
    decode_inner(bytes, None)
}

/// Decodes only one family, rejecting a mismatch immediately after the bounded
/// header parse. Fuzz targets use this to keep each decoder family independent.
pub fn decode_family(bytes: &[u8], expected: Family) -> Result<WireMessage, WireError> {
    decode_inner(bytes, Some(expected))
}

fn decode_inner(bytes: &[u8], expected: Option<Family>) -> Result<WireMessage, WireError> {
    let (header, payload) = parse_envelope(bytes)?;
    if let Some(expected) = expected
        && header.family != expected
    {
        return Err(WireError::FamilyMismatch {
            expected,
            actual: header.family,
        });
    }
    decode_payload(&header, payload)
}

struct EncodedPayload {
    family: Family,
    message_type: u8,
    session: Option<SessionContext>,
    channel: ChannelFields,
    payload: Vec<u8>,
}

fn prepare_payload(message: &WireMessage) -> Result<EncodedPayload, WireError> {
    let bounds = |error: BoundError| WireError::Bounds(error.to_string());
    Ok(match message {
        WireMessage::NegotiationOffer(offer) => {
            let value = WireNegotiationOffer::try_from(offer).map_err(bounds)?;
            EncodedPayload {
                family: Family::Negotiation,
                message_type: 1,
                session: None,
                channel: ChannelFields::None,
                payload: codec::encode(&value)?,
            }
        }
        WireMessage::NegotiatedSession(session) => {
            let value = WireNegotiatedSession::try_from(session).map_err(bounds)?;
            EncodedPayload {
                family: Family::Negotiation,
                message_type: 2,
                session: None,
                channel: ChannelFields::None,
                payload: codec::encode(&value)?,
            }
        }
        WireMessage::ReliableControl(message) => {
            validate_reliable_context(&message.payload, message.session)?;
            let value = WireReliableControl::try_from(&message.payload).map_err(bounds)?;
            let message_type = value.message_type();
            EncodedPayload {
                family: Family::ReliableControl,
                message_type,
                session: Some(message.session),
                channel: ChannelFields::Control(message.sequence),
                payload: codec::encode(&value)?,
            }
        }
        WireMessage::Motion(frame) => {
            let value = WireMotionBody::try_from(frame).map_err(bounds)?;
            EncodedPayload {
                family: Family::Motion,
                message_type: 1,
                session: Some(frame.session),
                channel: ChannelFields::Motion {
                    motion_sequence: frame.motion_sequence,
                    control_watermark: frame.control_watermark,
                },
                payload: codec::encode(&value)?,
            }
        }
        WireMessage::Probe(message) => {
            let message_type = match message.payload {
                ProbePayload::Probe { .. } => 1,
                ProbePayload::ProbeEcho { .. } => 2,
            };
            EncodedPayload {
                family: Family::Probe,
                message_type,
                session: Some(message.session),
                channel: ChannelFields::None,
                payload: codec::encode(&message.payload)?,
            }
        }
        WireMessage::Desktop(message) => {
            message
                .validate()
                .map_err(|e| WireError::Bounds(e.to_string()))?;
            let json =
                serde_json::to_string(message).map_err(|e| WireError::Bounds(e.to_string()))?;
            let value =
                bounds::BoundedString::<{ crate::desktop::MAX_MESSAGE_BYTES }>::try_from_string(
                    json, "desktop",
                )
                .map_err(bounds)?;
            EncodedPayload {
                family: Family::Desktop,
                message_type: 1,
                session: None,
                channel: ChannelFields::None,
                payload: codec::encode(&value)?,
            }
        }
        WireMessage::Pairing(pairing) => {
            let value = WirePairingOffer::try_from(pairing).map_err(bounds)?;
            EncodedPayload {
                family: Family::Pairing,
                message_type: 1,
                session: None,
                channel: ChannelFields::None,
                payload: codec::encode(&value)?,
            }
        }
    })
}

#[derive(Debug, Clone, Copy)]
enum ChannelFields {
    None,
    Control(ControlSequence),
    Motion {
        motion_sequence: MotionSequence,
        control_watermark: ControlSequence,
    },
}

impl ChannelFields {
    fn encoded_len(self) -> usize {
        match self {
            Self::None => 0,
            Self::Control(_) => 8,
            Self::Motion { .. } => 16,
        }
    }

    fn encode(self, bytes: &mut Vec<u8>) {
        match self {
            Self::None => {}
            Self::Control(sequence) => bytes.extend_from_slice(&sequence.0.to_le_bytes()),
            Self::Motion {
                motion_sequence,
                control_watermark,
            } => {
                bytes.extend_from_slice(&motion_sequence.0.to_le_bytes());
                bytes.extend_from_slice(&control_watermark.0.to_le_bytes());
            }
        }
    }
}

struct Header {
    family: Family,
    message_type: u8,
    session: Option<SessionContext>,
    channel: ChannelFields,
}

fn parse_envelope(bytes: &[u8]) -> Result<(Header, &[u8]), WireError> {
    if bytes.len() < FIXED_HEADER_BYTES {
        return Err(WireError::TooShort);
    }
    let mut cursor = Cursor::new(bytes);
    if cursor.take(2)? != MAGIC {
        return Err(WireError::BadMagic);
    }
    let family = Family::try_from(cursor.u8()?)?;
    let message_type = cursor.u8()?;
    if !family.valid_message_type(message_type) {
        return Err(WireError::UnknownMessageType {
            family,
            message_type,
        });
    }
    let payload_length = usize::try_from(cursor.u32()?).map_err(|_| WireError::SizeLimit {
        what: "payload",
        actual: usize::MAX,
        maximum: family.maximum_payload_bytes(),
    })?;
    if payload_length > family.maximum_payload_bytes() {
        return Err(WireError::SizeLimit {
            what: "payload",
            actual: payload_length,
            maximum: family.maximum_payload_bytes(),
        });
    }
    let session = if family.requires_session() {
        let mut epoch = [0_u8; 16];
        epoch.copy_from_slice(cursor.take(16)?);
        Some(SessionContext {
            session_epoch: SessionEpoch(epoch),
            transport_generation: TransportGeneration(cursor.u64()?),
            activation_id: ActivationId(cursor.u64()?),
        })
    } else {
        None
    };

    let channel = match family {
        Family::ReliableControl => ChannelFields::Control(ControlSequence(cursor.u64()?)),
        Family::Motion => ChannelFields::Motion {
            motion_sequence: MotionSequence(cursor.u64()?),
            control_watermark: ControlSequence(cursor.u64()?),
        },
        _ => ChannelFields::None,
    };

    if cursor.remaining() != payload_length {
        return Err(WireError::LengthMismatch);
    }
    let header = Header {
        family,
        message_type,
        session,
        channel,
    };
    Ok((header, cursor.take(payload_length)?))
}

fn decode_payload(header: &Header, payload: &[u8]) -> Result<WireMessage, WireError> {
    let bounds = |error: BoundError| WireError::Bounds(error.to_string());
    match header.family {
        Family::Negotiation => match header.message_type {
            1 => {
                let value: WireNegotiationOffer = codec::decode(payload)?;
                Ok(WireMessage::NegotiationOffer(
                    value.try_into().map_err(bounds)?,
                ))
            }
            2 => {
                let value: WireNegotiatedSession = codec::decode(payload)?;
                Ok(WireMessage::NegotiatedSession(
                    value.try_into().map_err(bounds)?,
                ))
            }
            _ => unreachable!("message type checked during header parsing"),
        },
        Family::ReliableControl => {
            let value: WireReliableControl = codec::decode(payload)?;
            if value.message_type() != header.message_type {
                return Err(WireError::InvalidEnvelope(
                    "reliable control type disagrees with its payload",
                ));
            }
            let session = header.session.ok_or(WireError::InvalidEnvelope(
                "reliable control lacks session context",
            ))?;
            let sequence = match header.channel {
                ChannelFields::Control(sequence) => sequence,
                _ => return Err(WireError::InvalidEnvelope("control sequence is absent")),
            };
            let payload: ReliableControl = value.try_into().map_err(bounds)?;
            validate_reliable_context(&payload, session)?;
            Ok(WireMessage::ReliableControl(ReliableControlMessage {
                session,
                sequence,
                payload,
            }))
        }
        Family::Motion => {
            let value: WireMotionBody = codec::decode(payload)?;
            let session = header
                .session
                .ok_or(WireError::InvalidEnvelope("motion lacks session context"))?;
            let (motion_sequence, control_watermark) = match header.channel {
                ChannelFields::Motion {
                    motion_sequence,
                    control_watermark,
                } => (motion_sequence, control_watermark),
                _ => return Err(WireError::InvalidEnvelope("motion sequences are absent")),
            };
            Ok(WireMessage::Motion(
                value
                    .into_model(session, motion_sequence, control_watermark)
                    .map_err(bounds)?,
            ))
        }
        Family::Probe => {
            let value: ProbePayload = codec::decode(payload)?;
            let value_type = match value {
                ProbePayload::Probe { .. } => 1,
                ProbePayload::ProbeEcho { .. } => 2,
            };
            if value_type != header.message_type {
                return Err(WireError::InvalidEnvelope(
                    "probe type disagrees with its payload",
                ));
            }
            Ok(WireMessage::Probe(ProbeMessage {
                session: header
                    .session
                    .ok_or(WireError::InvalidEnvelope("probe lacks session context"))?,
                payload: value,
            }))
        }
        Family::Desktop => {
            let value: bounds::BoundedString<{ crate::desktop::MAX_MESSAGE_BYTES }> =
                codec::decode(payload)?;
            let message: crate::desktop::DesktopMessage =
                serde_json::from_str(&value.into_string())
                    .map_err(|e| WireError::Bounds(e.to_string()))?;
            message
                .validate()
                .map_err(|e| WireError::Bounds(e.to_string()))?;
            Ok(WireMessage::Desktop(message))
        }
        Family::Pairing => {
            let value: WirePairingOffer = codec::decode(payload)?;
            Ok(WireMessage::Pairing(value.try_into().map_err(bounds)?))
        }
    }
}

fn validate_reliable_context(
    payload: &ReliableControl,
    session: SessionContext,
) -> Result<(), WireError> {
    if payload
        .motion_anchor()
        .is_some_and(|anchor| anchor.activation_id != session.activation_id)
    {
        return Err(WireError::InvalidEnvelope(
            "motion anchor activation differs from the envelope",
        ));
    }
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.position)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], WireError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(WireError::LengthMismatch)?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or(WireError::LengthMismatch)?;
        self.position = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("length checked"),
        ))
    }

    fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("length checked"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn session() -> SessionContext {
        SessionContext {
            session_epoch: SessionEpoch([0x11; 16]),
            transport_generation: TransportGeneration(7),
            activation_id: ActivationId(9),
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
            TouchContact {
                id: ContactId(2),
                x: 300,
                y: 400,
                pressure: None,
                major: None,
                minor: None,
                orientation_millidegrees: None,
                tool: TouchTool::Finger,
                source_dimensions: None,
            },
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

    fn corpus() -> Vec<WireMessage> {
        let capabilities = InputCapabilities::from([
            InputCapability::Keyboard,
            InputCapability::Pointer,
            InputCapability::Scroll,
            InputCapability::Touch,
        ]);
        vec![
            WireMessage::NegotiationOffer(NegotiationOffer {
                maximum_datagram_size: 1_200,
                supported_capabilities: capabilities.clone(),
                required_capabilities: InputCapabilities::from([InputCapability::Keyboard]),
                pointer_units: BTreeSet::from([PointerUnit::DeviceUnaccelerated]),
                scroll_fields: ScrollFields {
                    high_resolution: true,
                    source_unit: true,
                    source_resolution: true,
                    discrete_steps: true,
                    phase: true,
                    momentum_phase: true,
                },
                maximum_contacts: 10,
                maximum_receiver_lease_ms: 1_000,
                maximum_checkpoint_bound_ms: 250,
            }),
            WireMessage::NegotiatedSession(NegotiatedSession {
                maximum_datagram_size: 1_200,
                capabilities,
                pointer_unit: Some(PointerUnit::DeviceUnaccelerated),
                scroll_fields: ScrollFields {
                    high_resolution: true,
                    source_unit: true,
                    source_resolution: true,
                    discrete_steps: false,
                    phase: true,
                    momentum_phase: false,
                },
                contact_limit: 10,
                receiver_lease_ms: 900,
                checkpoint_bound_ms: 200,
            }),
            WireMessage::ReliableControl(ReliableControlMessage {
                session: session(),
                sequence: ControlSequence(33),
                payload: ReliableControl::StateSnapshot(StateSnapshot {
                    held: HeldState {
                        pressed_keys: BTreeSet::from([HidUsage::keyboard(4)]),
                        pressed_buttons: BTreeSet::from([PointerButton::PRIMARY]),
                        active_touch: touch_state(),
                    },
                    motion_anchor: anchor(),
                }),
            }),
            WireMessage::Motion(MotionFrame {
                session: session(),
                motion_sequence: MotionSequence(34),
                control_watermark: ControlSequence(33),
                sender_capture_time: MonotonicTimeMicros(1_234_890),
                totals: CumulativeMotion::new(120, -80, 9, -6),
                touch_snapshot: Some(touch_state()),
            }),
            WireMessage::Probe(ProbeMessage {
                session: session(),
                payload: ProbePayload::ProbeEcho {
                    sequence: ProbeSequence(22),
                    probe_sent_at: MonotonicTimeMicros(100),
                    received_at: MonotonicTimeMicros(110),
                    echoed_at: MonotonicTimeMicros(111),
                },
            }),
            WireMessage::Pairing(PairingOffer {
                handshake_nonce: [0x33; 32],
                device_label: Some("workstation".into()),
                input_port: 43119,
                input_candidates: vec!["192.0.2.1:43119".into()],
            }),
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
    fn every_control_message_round_trips_under_its_own_type() {
        let anchor = |kind| MotionAnchor {
            activation_id: session().activation_id,
            through_motion_sequence: MotionSequence(3),
            sender_capture_time: MonotonicTimeMicros(1_000),
            totals: CumulativeMotion::new(5, -2, 0, 7),
            final_touch_state: TouchState::default(),
            kind,
        };
        let held = HeldState {
            pressed_keys: BTreeSet::from([HidUsage::keyboard(4)]),
            pressed_buttons: BTreeSet::from([PointerButton::PRIMARY]),
            active_touch: TouchState::default(),
        };
        let checkpoint = anchor(AnchorKind::Checkpoint);
        let cases = [
            ReliableControl::Enter,
            ReliableControl::KeyDown {
                key: HidUsage::keyboard(4),
            },
            ReliableControl::KeyUp {
                key: HidUsage::consumer(0xe9),
            },
            ReliableControl::ButtonDown {
                button: PointerButton::PRIMARY,
                anchor: checkpoint.clone(),
            },
            ReliableControl::ButtonUp {
                button: PointerButton::SECONDARY,
                anchor: checkpoint.clone(),
            },
            ReliableControl::TouchBegin {
                initial_state: touch_state(),
            },
            ReliableControl::TouchEnd {
                anchor: checkpoint.clone(),
            },
            ReliableControl::TouchCancel {
                anchor: checkpoint.clone(),
            },
            ReliableControl::StateSnapshot(StateSnapshot {
                held,
                motion_anchor: checkpoint,
            }),
            ReliableControl::SnapshotAck(SnapshotAck {
                snapshot_sequence: ControlSequence(4),
                accepted_generation: TransportGeneration(7),
            }),
            ReliableControl::SessionClose {
                reason: SessionCloseReason::LocalRelease,
                final_anchor: Some(anchor(AnchorKind::Terminal)),
            },
        ];
        for (message_type, payload) in (1..).zip(cases) {
            let message = WireMessage::ReliableControl(ReliableControlMessage {
                session: session(),
                sequence: ControlSequence(5),
                payload,
            });
            let bytes = encode(&message).unwrap();
            assert_eq!(bytes[3], message_type);
            assert_eq!(decode(&bytes).unwrap(), message);
        }
    }

    #[test]
    fn unknown_control_types_are_rejected() {
        let mut bytes = encode(&corpus().remove(2)).unwrap();
        for unknown in [0, 12, 255] {
            bytes[3] = unknown;
            assert_eq!(
                decode(&bytes),
                Err(WireError::UnknownMessageType {
                    family: Family::ReliableControl,
                    message_type: unknown,
                })
            );
        }
    }

    #[test]
    fn invalid_family_and_trailing_envelope_data_are_rejected() {
        let mut bytes = encode(&corpus().remove(4)).unwrap();
        bytes[2] = 0xff;
        assert_eq!(decode(&bytes), Err(WireError::UnknownFamily(0xff)));

        let mut bytes = encode(&corpus().remove(4)).unwrap();
        bytes.push(0);
        assert_eq!(decode(&bytes), Err(WireError::LengthMismatch));
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
    fn declared_oversize_payload_is_rejected_before_length_or_codec_work() {
        let mut bytes = encode(&corpus().remove(4)).unwrap();
        bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            decode(&bytes),
            Err(WireError::SizeLimit {
                what: "payload",
                ..
            })
        ));
    }

    #[test]
    fn collection_bound_is_enforced_by_the_decoder() {
        let mut bytes = encode(&corpus().remove(0)).unwrap();
        // Negotiation has no session/channel header. The body starts with the
        // two-byte datagram size varint, then the capability count.
        bytes[FIXED_HEADER_BYTES + 2] = (bounds::MAX_CAPABILITIES as u8) + 1;
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn strings_are_bounded() {
        let message = WireMessage::Pairing(PairingOffer {
            handshake_nonce: [0; 32],
            device_label: Some("x".repeat(bounds::MAX_STRING_BYTES + 1)),
            input_port: 43119,
            input_candidates: Vec::new(),
        });
        assert!(matches!(encode(&message), Err(WireError::Bounds(_))));
    }

    #[test]
    fn control_type_and_anchor_context_are_structurally_checked() {
        let message = WireMessage::ReliableControl(ReliableControlMessage {
            session: session(),
            sequence: ControlSequence(1),
            payload: ReliableControl::TouchEnd {
                anchor: MotionAnchor {
                    activation_id: ActivationId(999),
                    ..anchor()
                },
            },
        });
        assert_eq!(
            encode(&message),
            Err(WireError::InvalidEnvelope(
                "motion anchor activation differs from the envelope"
            ))
        );

        let mut bytes = encode(&corpus().remove(2)).unwrap();
        bytes[3] = 1;
        assert_eq!(
            decode(&bytes),
            Err(WireError::InvalidEnvelope(
                "reliable control type disagrees with its payload"
            ))
        );
    }
    #[test]
    fn desktop_metadata_round_trips_with_bounded_json() {
        use crate::desktop::*;
        let message = WireMessage::Desktop(DesktopMessage::Request {
            id: 1,
            request: DesktopRequest::Prepare {
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
