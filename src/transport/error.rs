use thiserror::Error;

use crate::wire::WireError;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("TLS/QUIC configuration failed: {0}")]
    Configuration(String),
    #[error("could not start QUIC connection: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("QUIC connection failed: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("critical control stream write failed: {0}")]
    ControlWrite(#[from] quinn::WriteError),
    #[error("critical control stream write exceeded its safety bound")]
    ControlWriteTimedOut,
    #[error("critical control stream read failed: {0}")]
    ControlRead(#[from] quinn::ReadExactError),
    #[error("datagram send failed: {0}")]
    DatagramSend(#[from] quinn::SendDatagramError),
    #[error("datagram sender is closed")]
    DatagramQueueClosed,
    #[error("wire message is invalid: {0}")]
    Wire(#[from] WireError),
    #[error("peer did not negotiate the required zflow ALPN")]
    InvalidAlpn,
    #[error("peer did not present exactly one raw public key")]
    MissingPeerIdentity,
    #[error("peer raw public key did not match the configured authorization")]
    PeerIdentityMismatch,
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
}
