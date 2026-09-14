//! Bounded, self-describing framing for the protocol domain model.
//!
//! The envelope is codec-independent and is parsed before serde sees a byte:
//! magic, framing version, codec, family/type, flags, required-feature bits,
//! optional-field length, payload length, protocol version, then (for session
//! traffic) epoch/generation/activation and channel sequences. Unknown data is
//! accepted only inside the explicit optional-field TLV area.

mod bounds;
mod codec;
mod error;
mod model;

use std::collections::BTreeSet;

use crate::core::*;

use bounds::{BoundError, MAX_OPTIONAL_BYTES, MAX_OPTIONAL_FIELD_BYTES, MAX_OPTIONAL_FIELDS};
pub use error::WireError;
pub use model::{DiscoveryAnnouncement, PairingMethod, PairingOffer};
use model::{
    WireDiscoveryAnnouncement, WireMotionBody, WireNegotiatedSession, WireNegotiationOffer,
    WirePairingOffer, WireReliableControl,
};

const MAGIC: [u8; 2] = *b"ZF";
const FRAMING_VERSION: u8 = 1;
const FLAG_SESSION_CONTEXT: u8 = 1 << 0;
const KNOWN_FLAGS: u8 = FLAG_SESSION_CONTEXT;
const FIXED_HEADER_BYTES: usize = 17;

pub const CURRENT_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion(1);
pub const MAX_NEGOTIATION_PAYLOAD_BYTES: usize = 4 * 1_024;
pub const MAX_RELIABLE_PAYLOAD_BYTES: usize = 32 * 1_024;
pub const MAX_MOTION_PAYLOAD_BYTES: usize = 8 * 1_024;
pub const MAX_PROBE_PAYLOAD_BYTES: usize = 128;
pub const MAX_DISCOVERY_PAYLOAD_BYTES: usize = 8 * 1_024;
pub const MAX_PAIRING_PAYLOAD_BYTES: usize = 2 * 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    Postcard = 1,
    Bincode = 2,
}

impl TryFrom<u8> for Codec {
    type Error = WireError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Postcard),
            2 => Ok(Self::Bincode),
            other => Err(WireError::UnknownCodec(other)),
        }
    }
}

/// Postcard is the default because the representative valid-message corpus in
/// this module's tests is smaller with postcard than with bincode's standard
/// variable-integer serde encoding. Both codecs carry identical model values.
pub const DEFAULT_CODEC: Codec = Codec::Postcard;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Family {
    Negotiation = 1,
    ReliableControl = 2,
    Motion = 3,
    Probe = 4,
    Discovery = 5,
    Pairing = 6,
    Desktop = 7,
}

impl Family {
    fn maximum_payload_bytes(self) -> usize {
        match self {
            Self::Negotiation => MAX_NEGOTIATION_PAYLOAD_BYTES,
            Self::ReliableControl => MAX_RELIABLE_PAYLOAD_BYTES,
            Self::Motion => MAX_MOTION_PAYLOAD_BYTES,
            Self::Probe => MAX_PROBE_PAYLOAD_BYTES,
            Self::Discovery => MAX_DISCOVERY_PAYLOAD_BYTES,
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
            Self::ReliableControl => (1..=17).contains(&message_type),
            Self::Motion | Self::Discovery | Self::Pairing | Self::Desktop => message_type == 1,
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
            5 => Ok(Self::Discovery),
            6 => Ok(Self::Pairing),
            7 => Ok(Self::Desktop),
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
    Discovery(DiscoveryAnnouncement),
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
            Self::Discovery(_) => Family::Discovery,
            Self::Pairing(_) => Family::Pairing,
            Self::Desktop(_) => Family::Desktop,
        }
    }
}

/// An envelope extension. IDs are application-defined; unknown IDs are
/// preserved and ignored by the core decoder, while duplicate IDs are invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionalField {
    pub id: u16,
    pub value: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedMessage {
    pub message: WireMessage,
    pub optional_fields: Vec<OptionalField>,
}

pub fn encode(message: &WireMessage) -> Result<Vec<u8>, WireError> {
    encode_with_options(message, DEFAULT_CODEC, &[])
}

pub fn encode_with_codec(message: &WireMessage, codec: Codec) -> Result<Vec<u8>, WireError> {
    encode_with_options(message, codec, &[])
}

pub fn encode_with_options(
    message: &WireMessage,
    selected_codec: Codec,
    optional_fields: &[OptionalField],
) -> Result<Vec<u8>, WireError> {
    let encoded = prepare_payload(message, selected_codec)?;
    let optional = encode_optional_fields(optional_fields)?;
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
    let optional_length = u16::try_from(optional.len()).map_err(|_| WireError::SizeLimit {
        what: "optional fields",
        actual: optional.len(),
        maximum: u16::MAX as usize,
    })?;

    let mut bytes = Vec::with_capacity(
        FIXED_HEADER_BYTES
            + encoded.session.map_or(0, |_| 32)
            + encoded.channel.encoded_len()
            + optional.len()
            + encoded.payload.len(),
    );
    bytes.extend_from_slice(&MAGIC);
    bytes.push(FRAMING_VERSION);
    bytes.push(selected_codec as u8);
    bytes.push(encoded.family as u8);
    bytes.push(encoded.message_type);
    bytes.push(if encoded.session.is_some() {
        FLAG_SESSION_CONTEXT
    } else {
        0
    });
    bytes.extend_from_slice(&0_u16.to_le_bytes()); // no required feature bits in v1
    bytes.extend_from_slice(&optional_length.to_le_bytes());
    bytes.extend_from_slice(&payload_length.to_le_bytes());
    bytes.extend_from_slice(&encoded.protocol_version.0.to_le_bytes());

    if let Some(session) = encoded.session {
        bytes.extend_from_slice(&session.session_epoch.0);
        bytes.extend_from_slice(&session.transport_generation.0.to_le_bytes());
        bytes.extend_from_slice(&session.activation_id.0.to_le_bytes());
    }
    encoded.channel.encode(&mut bytes);
    bytes.extend_from_slice(&optional);
    bytes.extend_from_slice(&encoded.payload);
    Ok(bytes)
}

pub fn decode(bytes: &[u8]) -> Result<DecodedMessage, WireError> {
    decode_inner(bytes, None)
}

/// Decodes only one family, rejecting a mismatch immediately after the bounded
/// header parse. Fuzz targets use this to keep each decoder family independent.
pub fn decode_family(bytes: &[u8], expected: Family) -> Result<DecodedMessage, WireError> {
    decode_inner(bytes, Some(expected))
}

fn decode_inner(bytes: &[u8], expected: Option<Family>) -> Result<DecodedMessage, WireError> {
    let parsed = parse_envelope(bytes)?;
    if let Some(expected) = expected
        && parsed.header.family != expected
    {
        return Err(WireError::FamilyMismatch {
            expected,
            actual: parsed.header.family,
        });
    }

    let optional_fields = decode_optional_fields(parsed.optional)?;
    let message = decode_payload(&parsed.header, parsed.payload)?;
    Ok(DecodedMessage {
        message,
        optional_fields,
    })
}

struct EncodedPayload {
    family: Family,
    message_type: u8,
    protocol_version: ProtocolVersion,
    session: Option<SessionContext>,
    channel: ChannelFields,
    payload: Vec<u8>,
}

fn prepare_payload(
    message: &WireMessage,
    selected_codec: Codec,
) -> Result<EncodedPayload, WireError> {
    let bounds = |error: BoundError| WireError::Bounds(error.to_string());
    Ok(match message {
        WireMessage::NegotiationOffer(offer) => {
            let value = WireNegotiationOffer::try_from(offer).map_err(bounds)?;
            EncodedPayload {
                family: Family::Negotiation,
                message_type: 1,
                protocol_version: CURRENT_PROTOCOL_VERSION,
                session: None,
                channel: ChannelFields::None,
                payload: codec::encode(selected_codec, &value)?,
            }
        }
        WireMessage::NegotiatedSession(session) => {
            check_protocol_version(session.protocol_version)?;
            let value = WireNegotiatedSession::try_from(session).map_err(bounds)?;
            EncodedPayload {
                family: Family::Negotiation,
                message_type: 2,
                protocol_version: session.protocol_version,
                session: None,
                channel: ChannelFields::None,
                payload: codec::encode(selected_codec, &value)?,
            }
        }
        WireMessage::ReliableControl(message) => {
            check_protocol_version(message.session.protocol_version)?;
            validate_reliable_context(&message.payload, message.session)?;
            let value = WireReliableControl::try_from(&message.payload).map_err(bounds)?;
            let message_type = value.message_type();
            EncodedPayload {
                family: Family::ReliableControl,
                message_type,
                protocol_version: message.session.protocol_version,
                session: Some(message.session),
                channel: ChannelFields::Control(message.sequence),
                payload: codec::encode(selected_codec, &value)?,
            }
        }
        WireMessage::Motion(frame) => {
            check_protocol_version(frame.session.protocol_version)?;
            let value = WireMotionBody::try_from(frame).map_err(bounds)?;
            EncodedPayload {
                family: Family::Motion,
                message_type: 1,
                protocol_version: frame.session.protocol_version,
                session: Some(frame.session),
                channel: ChannelFields::Motion {
                    motion_sequence: frame.motion_sequence,
                    control_watermark: frame.control_watermark,
                },
                payload: codec::encode(selected_codec, &value)?,
            }
        }
        WireMessage::Probe(message) => {
            check_protocol_version(message.session.protocol_version)?;
            let message_type = match message.payload {
                ProbePayload::Probe { .. } => 1,
                ProbePayload::ProbeEcho { .. } => 2,
            };
            EncodedPayload {
                family: Family::Probe,
                message_type,
                protocol_version: message.session.protocol_version,
                session: Some(message.session),
                channel: ChannelFields::None,
                payload: codec::encode(selected_codec, &message.payload)?,
            }
        }
        WireMessage::Discovery(announcement) => {
            let value = WireDiscoveryAnnouncement::try_from(announcement).map_err(bounds)?;
            EncodedPayload {
                family: Family::Discovery,
                message_type: 1,
                protocol_version: CURRENT_PROTOCOL_VERSION,
                session: None,
                channel: ChannelFields::None,
                payload: codec::encode(selected_codec, &value)?,
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
                protocol_version: CURRENT_PROTOCOL_VERSION,
                session: None,
                channel: ChannelFields::None,
                payload: codec::encode(selected_codec, &value)?,
            }
        }
        WireMessage::Pairing(pairing) => {
            let value = WirePairingOffer::try_from(pairing).map_err(bounds)?;
            EncodedPayload {
                family: Family::Pairing,
                message_type: 1,
                protocol_version: CURRENT_PROTOCOL_VERSION,
                session: None,
                channel: ChannelFields::None,
                payload: codec::encode(selected_codec, &value)?,
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
    codec: Codec,
    family: Family,
    message_type: u8,
    protocol_version: ProtocolVersion,
    session: Option<SessionContext>,
    channel: ChannelFields,
}

struct ParsedEnvelope<'a> {
    header: Header,
    optional: &'a [u8],
    payload: &'a [u8],
}

fn parse_envelope(bytes: &[u8]) -> Result<ParsedEnvelope<'_>, WireError> {
    if bytes.len() < FIXED_HEADER_BYTES {
        return Err(WireError::TooShort);
    }
    let mut cursor = Cursor::new(bytes);
    if cursor.take(2)? != MAGIC {
        return Err(WireError::BadMagic);
    }
    let framing_version = cursor.u8()?;
    if framing_version != FRAMING_VERSION {
        return Err(WireError::UnsupportedFramingVersion(framing_version));
    }
    let selected_codec = Codec::try_from(cursor.u8()?)?;
    let family = Family::try_from(cursor.u8()?)?;
    let message_type = cursor.u8()?;
    if !family.valid_message_type(message_type) {
        return Err(WireError::UnknownMessageType {
            family,
            message_type,
        });
    }
    let flags = cursor.u8()?;
    if flags & !KNOWN_FLAGS != 0 {
        return Err(WireError::InvalidFlags(flags));
    }
    let required_features = cursor.u16()?;
    if required_features != 0 {
        return Err(WireError::UnknownRequiredFeatures(required_features));
    }
    let optional_length = usize::from(cursor.u16()?);
    if optional_length > MAX_OPTIONAL_BYTES {
        return Err(WireError::SizeLimit {
            what: "optional fields",
            actual: optional_length,
            maximum: MAX_OPTIONAL_BYTES,
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
    let protocol_version = ProtocolVersion(cursor.u16()?);
    check_protocol_version(protocol_version)?;

    let has_session = flags & FLAG_SESSION_CONTEXT != 0;
    if has_session != family.requires_session() {
        return Err(WireError::InvalidEnvelope(
            "session context presence does not match the family",
        ));
    }
    let session = if has_session {
        let mut epoch = [0_u8; 16];
        epoch.copy_from_slice(cursor.take(16)?);
        Some(SessionContext {
            protocol_version,
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

    let remaining_length = optional_length
        .checked_add(payload_length)
        .ok_or(WireError::LengthMismatch)?;
    if cursor.remaining() != remaining_length {
        return Err(WireError::LengthMismatch);
    }
    let optional = cursor.take(optional_length)?;
    let payload = cursor.take(payload_length)?;
    Ok(ParsedEnvelope {
        header: Header {
            codec: selected_codec,
            family,
            message_type,
            protocol_version,
            session,
            channel,
        },
        optional,
        payload,
    })
}

fn decode_payload(header: &Header, payload: &[u8]) -> Result<WireMessage, WireError> {
    let bounds = |error: BoundError| WireError::Bounds(error.to_string());
    match header.family {
        Family::Negotiation => match header.message_type {
            1 => {
                let value: WireNegotiationOffer = codec::decode(header.codec, payload)?;
                Ok(WireMessage::NegotiationOffer(
                    value.try_into().map_err(bounds)?,
                ))
            }
            2 => {
                let value: WireNegotiatedSession = codec::decode(header.codec, payload)?;
                Ok(WireMessage::NegotiatedSession(
                    value.into_model(header.protocol_version).map_err(bounds)?,
                ))
            }
            _ => unreachable!("message type checked during header parsing"),
        },
        Family::ReliableControl => {
            let value: WireReliableControl = codec::decode(header.codec, payload)?;
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
            let value: WireMotionBody = codec::decode(header.codec, payload)?;
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
            let value: ProbePayload = codec::decode(header.codec, payload)?;
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
        Family::Discovery => {
            let value: WireDiscoveryAnnouncement = codec::decode(header.codec, payload)?;
            Ok(WireMessage::Discovery(value.try_into().map_err(bounds)?))
        }
        Family::Desktop => {
            let value: bounds::BoundedString<{ crate::desktop::MAX_MESSAGE_BYTES }> =
                codec::decode(header.codec, payload)?;
            let message: crate::desktop::DesktopMessage =
                serde_json::from_str(&value.into_string())
                    .map_err(|e| WireError::Bounds(e.to_string()))?;
            message
                .validate()
                .map_err(|e| WireError::Bounds(e.to_string()))?;
            Ok(WireMessage::Desktop(message))
        }
        Family::Pairing => {
            let value: WirePairingOffer = codec::decode(header.codec, payload)?;
            Ok(WireMessage::Pairing(value.try_into().map_err(bounds)?))
        }
    }
}

fn check_protocol_version(version: ProtocolVersion) -> Result<(), WireError> {
    if version == CURRENT_PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(WireError::UnsupportedProtocolVersion(version.0))
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

fn encode_optional_fields(fields: &[OptionalField]) -> Result<Vec<u8>, WireError> {
    if fields.len() > MAX_OPTIONAL_FIELDS {
        return Err(WireError::SizeLimit {
            what: "optional field count",
            actual: fields.len(),
            maximum: MAX_OPTIONAL_FIELDS,
        });
    }
    let mut ids = BTreeSet::new();
    let mut encoded = Vec::new();
    for field in fields {
        if field.id == 0 {
            return Err(WireError::InvalidEnvelope(
                "optional field id zero is reserved",
            ));
        }
        if !ids.insert(field.id) {
            return Err(WireError::DuplicateOptionalField(field.id));
        }
        if field.value.len() > MAX_OPTIONAL_FIELD_BYTES {
            return Err(WireError::SizeLimit {
                what: "optional field",
                actual: field.value.len(),
                maximum: MAX_OPTIONAL_FIELD_BYTES,
            });
        }
        let length = u16::try_from(field.value.len()).expect("field cap fits u16");
        encoded.extend_from_slice(&field.id.to_le_bytes());
        encoded.extend_from_slice(&length.to_le_bytes());
        encoded.extend_from_slice(&field.value);
    }
    if encoded.len() > MAX_OPTIONAL_BYTES {
        return Err(WireError::SizeLimit {
            what: "optional fields",
            actual: encoded.len(),
            maximum: MAX_OPTIONAL_BYTES,
        });
    }
    Ok(encoded)
}

fn decode_optional_fields(bytes: &[u8]) -> Result<Vec<OptionalField>, WireError> {
    let mut cursor = Cursor::new(bytes);
    let mut ids = BTreeSet::new();
    let mut fields = Vec::new();
    while cursor.remaining() != 0 {
        if fields.len() == MAX_OPTIONAL_FIELDS {
            return Err(WireError::SizeLimit {
                what: "optional field count",
                actual: fields.len() + 1,
                maximum: MAX_OPTIONAL_FIELDS,
            });
        }
        let id = cursor.u16()?;
        if id == 0 {
            return Err(WireError::InvalidEnvelope(
                "optional field id zero is reserved",
            ));
        }
        if !ids.insert(id) {
            return Err(WireError::DuplicateOptionalField(id));
        }
        let length = usize::from(cursor.u16()?);
        if length > MAX_OPTIONAL_FIELD_BYTES {
            return Err(WireError::SizeLimit {
                what: "optional field",
                actual: length,
                maximum: MAX_OPTIONAL_FIELD_BYTES,
            });
        }
        fields.push(OptionalField {
            id,
            value: cursor.take(length)?.to_vec(),
        });
    }
    Ok(fields)
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

    fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("length checked"),
        ))
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
    use super::*;

    fn session() -> SessionContext {
        SessionContext {
            protocol_version: CURRENT_PROTOCOL_VERSION,
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
                protocol_versions: vec![CURRENT_PROTOCOL_VERSION],
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
                protocol_version: CURRENT_PROTOCOL_VERSION,
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
                        modifiers: BTreeSet::from([Modifier::LeftShift]),
                        active_scroll: None,
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
            WireMessage::Discovery(DiscoveryAnnouncement {
                ephemeral_instance_id: "ephemeral-7f31".into(),
                protocol_versions: vec![CURRENT_PROTOCOL_VERSION],
                capability_summary: InputCapabilities::from([
                    InputCapability::Keyboard,
                    InputCapability::Pointer,
                ]),
                candidates: vec!["192.0.2.1:43122".into(), "[2001:db8::1]:43122".into()],
                rotating_token: Some([0x77; 16]),
            }),
            WireMessage::Pairing(PairingOffer {
                handshake_nonce: [0x33; 32],
                method: PairingMethod::ShortAuthenticationString,
                device_label: Some("workstation".into()),
                input_port: 43119,
                input_candidates: vec!["192.0.2.1:43119".into()],
            }),
        ]
    }

    #[test]
    fn both_codecs_round_trip_every_family() {
        for selected_codec in [Codec::Postcard, Codec::Bincode] {
            for message in corpus() {
                let bytes = encode_with_codec(&message, selected_codec).unwrap();
                let decoded = decode(&bytes).unwrap();
                assert_eq!(decoded.message, message);
                assert!(decoded.optional_fields.is_empty());
            }
        }
    }

    #[test]
    fn postcard_is_selected_from_measured_valid_message_sizes() {
        let messages = corpus();
        let postcard_bytes: usize = messages
            .iter()
            .map(|message| encode_with_codec(message, Codec::Postcard).unwrap().len())
            .sum();
        let bincode_bytes: usize = messages
            .iter()
            .map(|message| encode_with_codec(message, Codec::Bincode).unwrap().len())
            .sum();

        assert!(
            postcard_bytes < bincode_bytes,
            "postcard={postcard_bytes}, bincode={bincode_bytes}"
        );
        assert_eq!(DEFAULT_CODEC, Codec::Postcard);
    }

    #[test]
    fn unknown_explicit_optional_fields_are_preserved() {
        let message = corpus().remove(4);
        let options = [
            OptionalField {
                id: 41,
                value: vec![1, 2, 3],
            },
            OptionalField {
                id: 900,
                value: vec![4, 5],
            },
        ];
        let bytes = encode_with_options(&message, Codec::Postcard, &options).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.message, message);
        assert_eq!(decoded.optional_fields, options);
    }

    #[test]
    fn invalid_family_and_trailing_envelope_data_are_rejected() {
        let mut bytes = encode(&corpus().remove(4)).unwrap();
        bytes[4] = 0xff;
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
        bytes[11..15].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            decode(&bytes),
            Err(WireError::SizeLimit {
                what: "payload",
                ..
            })
        ));
    }

    #[test]
    fn collection_bound_is_enforced_by_both_decoders() {
        for selected_codec in [Codec::Postcard, Codec::Bincode] {
            let offer = corpus().remove(0);
            let mut bytes = encode_with_codec(&offer, selected_codec).unwrap();
            // Negotiation has no session/channel header. The first body value is
            // the protocol_versions sequence length; 17 is the fixed header size.
            bytes[FIXED_HEADER_BYTES] = (bounds::MAX_PROTOCOL_VERSIONS as u8) + 1;
            assert!(decode(&bytes).is_err());
        }
    }

    #[test]
    fn strings_and_duplicate_optional_ids_are_bounded() {
        let message = WireMessage::Pairing(PairingOffer {
            handshake_nonce: [0; 32],
            method: PairingMethod::QrTranscript,
            device_label: Some("x".repeat(bounds::MAX_STRING_BYTES + 1)),
            input_port: 43119,
            input_candidates: Vec::new(),
        });
        assert!(matches!(encode(&message), Err(WireError::Bounds(_))));

        let probe = corpus().remove(4);
        let duplicate = [
            OptionalField {
                id: 7,
                value: vec![],
            },
            OptionalField {
                id: 7,
                value: vec![],
            },
        ];
        assert_eq!(
            encode_with_options(&probe, Codec::Postcard, &duplicate),
            Err(WireError::DuplicateOptionalField(7))
        );
    }

    #[test]
    fn control_type_and_anchor_context_are_structurally_checked() {
        let message = WireMessage::ReliableControl(ReliableControlMessage {
            session: session(),
            sequence: ControlSequence(1),
            payload: ReliableControl::Leave {
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
        bytes[5] = 1;
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
        for codec in [Codec::Postcard, Codec::Bincode] {
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
            let bytes = encode_with_codec(&message, codec).unwrap();
            assert_eq!(decode(&bytes).unwrap().message, message);
            let mut invalid = bytes;
            // Unknown families fail before metadata is interpreted.
            invalid[4] = 255;
            assert!(matches!(
                decode(&invalid),
                Err(WireError::UnknownFamily(255))
            ));
        }
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
