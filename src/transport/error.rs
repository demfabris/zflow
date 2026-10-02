use thiserror::Error;

use crate::wire::WireError;

/// The close code a listener sends when it refuses input from a key it has
/// not added.
pub(super) const NOT_TRUSTED: quinn::VarInt = quinn::VarInt::from_u32(0x103);

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("TLS/QUIC configuration failed: {0}")]
    Configuration(String),
    #[error("could not start QUIC connection: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("QUIC connection failed: {0}")]
    Connection(quinn::ConnectionError),
    #[error("critical control stream write failed: {0}")]
    ControlWrite(quinn::WriteError),
    #[error("critical control stream write exceeded its safety bound")]
    ControlWriteTimedOut,
    #[error("critical control stream read failed: {0}")]
    ControlRead(quinn::ReadExactError),
    #[error("datagram sender is closed")]
    DatagramQueueClosed,
    #[error("wire message is invalid: {0}")]
    Wire(#[from] WireError),
    #[error("peer runs a different zflow protocol version")]
    InvalidAlpn,
    #[error("peer did not present exactly one raw public key")]
    MissingPeerIdentity,
    #[error("peer raw public key did not match the configured authorization")]
    PeerIdentityMismatch,
    #[error("the other computer has not added this one")]
    NotTrusted,
    #[error("critical control stream ended")]
    CriticalStreamClosed,
    #[error("first bidirectional stream was not the zflow critical control stream")]
    InvalidControlPreface,
    #[error("critical control stream frame is {actual} bytes; maximum is {maximum}")]
    ControlFrameTooLarge { actual: usize, maximum: usize },
    #[error("message family is not allowed on the critical control stream")]
    InvalidControlFamily,
    #[error("message family is not allowed in an input datagram")]
    InvalidDatagramFamily,
    #[error("datagram size has not been negotiated")]
    DatagramSizeNotNegotiated,
    #[error("negotiated datagram size {requested} is invalid; path maximum is {path_maximum:?}")]
    InvalidDatagramSize {
        requested: usize,
        path_maximum: Option<usize>,
    },
    #[error("datagram size is already negotiated as {current}, not {requested}")]
    DatagramSizeAlreadyNegotiated { current: usize, requested: usize },
    #[error("datagram is {actual} bytes; negotiated maximum is {negotiated}")]
    DatagramTooLarge { actual: usize, negotiated: usize },
    #[error("could not derive the pairing transcript binding")]
    PairingExporter,
    #[error("pairing metadata stream failed: {0}")]
    PairingStream(String),
    #[error("first pairing stream did not have the zflow pairing preface")]
    InvalidPairingPreface,
    #[error("pairing frame is {actual} bytes; maximum is {maximum}")]
    PairingFrameTooLarge { actual: usize, maximum: usize },
    #[error("message family is not allowed on the pairing-only stream")]
    InvalidPairingFamily,
    #[error("the setup code did not match")]
    PairingCodeMismatch,
    #[error("peer sent an invalid pairing key exchange")]
    PairingKeyExchange,
    #[error("clipboard stream failed: {0}")]
    Clipboard(String),
    #[error("could not derive the hello transcript binding")]
    HelloExporter,
    #[error("hello stream failed: {0}")]
    HelloStream(String),
    #[error("first hello stream did not have the zflow hello preface")]
    InvalidHelloPreface,
    #[error("hello frame is {actual} bytes; maximum is {maximum}")]
    HelloFrameTooLarge { actual: usize, maximum: usize },
}

impl From<quinn::ConnectionError> for TransportError {
    fn from(error: quinn::ConnectionError) -> Self {
        // TLS alert 120 (no_application_protocol) means the peers share no
        // ALPN, which is how two zflow versions tell each other apart.
        let code = match &error {
            quinn::ConnectionError::ConnectionClosed(close) => Some(close.error_code),
            quinn::ConnectionError::TransportError(error) => Some(error.code),
            _ => None,
        };
        // Alert 49 (access_denied) raised here is this computer's key check
        // refusing the peer's key. Sent by the peer, it means the reverse,
        // which stays a plain connection error.
        let raised_here = matches!(error, quinn::ConnectionError::TransportError(_));
        if code == Some(quinn::TransportErrorCode::crypto(120)) {
            Self::InvalidAlpn
        } else if raised_here && code == Some(quinn::TransportErrorCode::crypto(49)) {
            Self::PeerIdentityMismatch
        } else if refused(&error) {
            Self::NotTrusted
        } else {
            Self::Connection(error)
        }
    }
}

// The refusal reaches a dialer as whichever stream call notices it first, and
// it says more than the stream error that carried it.
impl From<quinn::WriteError> for TransportError {
    fn from(error: quinn::WriteError) -> Self {
        match &error {
            quinn::WriteError::ConnectionLost(lost) if refused(lost) => Self::NotTrusted,
            _ => Self::ControlWrite(error),
        }
    }
}

impl From<quinn::ReadExactError> for TransportError {
    fn from(error: quinn::ReadExactError) -> Self {
        match &error {
            quinn::ReadExactError::ReadError(quinn::ReadError::ConnectionLost(lost))
                if refused(lost) =>
            {
                Self::NotTrusted
            }
            _ => Self::ControlRead(error),
        }
    }
}

/// Whether the other computer closed because it has not added this one.
fn refused(error: &quinn::ConnectionError) -> bool {
    matches!(error, quinn::ConnectionError::ApplicationClosed(close) if close.error_code == NOT_TRUSTED)
}
