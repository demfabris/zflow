use super::{Codec, Family};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WireError {
    #[error("wire envelope is shorter than its fixed header")]
    TooShort,
    #[error("invalid wire magic")]
    BadMagic,
    #[error("unsupported framing version {0}")]
    UnsupportedFramingVersion(u8),
    #[error("unsupported protocol version {0}")]
    UnsupportedProtocolVersion(u16),
    #[error("unknown codec identifier {0}")]
    UnknownCodec(u8),
    #[error("unknown message family {0}")]
    UnknownFamily(u8),
    #[error("unknown message type {message_type} for {family:?}")]
    UnknownMessageType { family: Family, message_type: u8 },
    #[error("unknown required feature bits 0x{0:04x}")]
    UnknownRequiredFeatures(u16),
    #[error("invalid envelope flags 0x{0:02x}")]
    InvalidFlags(u8),
    #[error("{what} length {actual} exceeds the cap of {maximum}")]
    SizeLimit {
        what: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("declared envelope length does not match the input")]
    LengthMismatch,
    #[error("payload decoder left trailing data")]
    TrailingPayload,
    #[error("invalid envelope: {0}")]
    InvalidEnvelope(&'static str),
    #[error("optional field id {0} appears more than once")]
    DuplicateOptionalField(u16),
    #[error("{codec:?} codec error: {detail}")]
    Codec { codec: Codec, detail: String },
    #[error("wire bound violation: {0}")]
    Bounds(String),
    #[error("expected {expected:?}, received {actual:?}")]
    FamilyMismatch { expected: Family, actual: Family },
}
