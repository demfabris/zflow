use super::Family;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WireError {
    #[error("wire message is shorter than its header")]
    TooShort,
    #[error("invalid wire magic")]
    BadMagic,
    #[error("unknown message family {0}")]
    UnknownFamily(u8),
    #[error("expected {expected:?}, received {actual:?}")]
    FamilyMismatch { expected: Family, actual: Family },
    #[error("{what}: {actual} exceeds the limit of {maximum}")]
    SizeLimit {
        what: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("postcard codec error: {0}")]
    Codec(String),
    #[error("payload decoder left trailing data")]
    TrailingPayload,
    #[error("invalid message: {0}")]
    Invalid(String),
}
