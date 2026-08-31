use std::{
    fmt,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use quinn::{Connection, Endpoint, Incoming, RecvStream, SendStream, VarInt};
use rustls::pki_types::CertificateDer;
use tokio::sync::Notify;

use crate::{
    core::{
        MotionFrame, NegotiatedSession, NegotiationOffer, ProbeMessage, ReliableControlMessage,
    },
    wire::{
        MAX_PAIRING_PAYLOAD_BYTES, MAX_RELIABLE_PAYLOAD_BYTES, PairingOffer, WireMessage,
        decode as decode_wire, encode as encode_wire,
    },
};

use super::{
    INPUT_ALPN_PROTOCOL, InputClientConfig, InputServerConfig, PAIRING_ALPN_PROTOCOL,
    PairingClientConfig, PairingServerConfig, TransportError,
};

const SERVER_NAME_PLACEHOLDER: &str = "zflow.invalid";
const CONTROL_STREAM_PREFACE: &[u8] = b"zflow-control-v1\0";
const PAIRING_STREAM_PREFACE: &[u8] = b"zflow-pair-v1\0";
const MAX_CONTROL_FRAME_BYTES: usize = MAX_RELIABLE_PAYLOAD_BYTES + 1_024 + 64;
const MAX_PAIRING_FRAME_BYTES: usize = MAX_PAIRING_PAYLOAD_BYTES + 64;
const CRITICAL_STREAM_ERROR: VarInt = VarInt::from_u32(0x100);
const PROTOCOL_ERROR: VarInt = VarInt::from_u32(0x101);
const PAIRING_EXPORTER_LABEL: &[u8] = b"EXPORTER-zflow-pairing-v1";
const CONTROL_WRITE_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputChannelKind {
    ReliableControl,
    CumulativeMotion,
    Probe,
}

/// The complete channel surface of the input connection. Bulk data must use a
/// future, separate connection and socket.
pub const INPUT_CHANNELS: [InputChannelKind; 3] = [
    InputChannelKind::ReliableControl,
    InputChannelKind::CumulativeMotion,
    InputChannelKind::Probe,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputControlMessage {
    NegotiationOffer(NegotiationOffer),
    NegotiatedSession(NegotiatedSession),
    Reliable(ReliableControlMessage),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputDatagram {
    Motion(MotionFrame),
    Probe(ProbeMessage),
}

/// A mutually authenticated input connection with exactly one critical stream.
///
/// The underlying Quinn connection is intentionally not exposed: input code can
/// only access the protocol's control, motion, and probe channels.
pub struct InputConnection {
    peer_spki: Arc<[u8]>,
    control_send: ControlSender,
    control_receive: ControlReceiver,
    datagrams: DatagramChannel,
}

impl fmt::Debug for InputConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputConnection")
            .field("peer_spki", &"[redacted]")
            .field(
                "remote_address",
                &self.datagrams.connection.remote_address(),
            )
            .finish_non_exhaustive()
    }
}

impl InputConnection {
    pub fn peer_spki(&self) -> &[u8] {
        &self.peer_spki
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.datagrams.connection.remote_address()
    }

    pub fn channels_mut(&mut self) -> (&mut ControlSender, &mut ControlReceiver, &DatagramChannel) {
        (
            &mut self.control_send,
            &mut self.control_receive,
            &self.datagrams,
        )
    }

    pub fn into_channels(self) -> InputChannels {
        InputChannels {
            control_send: self.control_send,
            control_receive: self.control_receive,
            datagrams: self.datagrams,
        }
    }

    pub fn close(&self) {
        self.datagrams.connection.close(VarInt::from_u32(0), b"");
    }

    pub async fn closed(&self) -> quinn::ConnectionError {
        self.datagrams.connection.closed().await
    }
}

pub struct InputChannels {
    pub control_send: ControlSender,
    pub control_receive: ControlReceiver,
    pub datagrams: DatagramChannel,
}

pub struct ControlSender {
    stream: SendStream,
    connection: Connection,
}

impl fmt::Debug for ControlSender {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ControlSender")
            .finish_non_exhaustive()
    }
}

impl ControlSender {
    /// Send the authenticated session offer on the critical ordered stream.
    ///
    /// Like Quinn's `write_all`, this future must not be cancelled mid-write.
    pub async fn send_negotiation_offer(
        &mut self,
        offer: &NegotiationOffer,
    ) -> Result<(), TransportError> {
        self.send_wire(WireMessage::NegotiationOffer(offer.clone()))
            .await
    }

    /// Send the selected authenticated session schema.
    ///
    /// Like Quinn's `write_all`, this future must not be cancelled mid-write.
    pub async fn send_negotiated_session(
        &mut self,
        session: &NegotiatedSession,
    ) -> Result<(), TransportError> {
        self.send_wire(WireMessage::NegotiatedSession(session.clone()))
            .await
    }

    /// Send one reliable input-control transition.
    ///
    /// Like Quinn's `write_all`, this future must not be cancelled mid-write.
    pub async fn send_control(
        &mut self,
        message: &ReliableControlMessage,
    ) -> Result<(), TransportError> {
        self.send_wire(WireMessage::ReliableControl(message.clone()))
            .await
    }

    async fn send_wire(&mut self, message: WireMessage) -> Result<(), TransportError> {
        let payload = encode_wire(&message)?;
        if payload.len() > MAX_CONTROL_FRAME_BYTES {
            return Err(TransportError::ControlFrameTooLarge {
                actual: payload.len(),
                maximum: MAX_CONTROL_FRAME_BYTES,
            });
        }
        let length = u32::try_from(payload.len()).expect("control frame bound fits u32");
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&length.to_be_bytes());
        frame.extend_from_slice(&payload);
        match tokio::time::timeout(CONTROL_WRITE_TIMEOUT, self.stream.write_all(&frame)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                close_critical(&self.connection, b"critical stream write failed");
                return Err(error.into());
            }
            Err(_) => {
                // write_all is not cancellation-safe. Closing the whole
                // connection makes a partially written frame unobservable and
                // drives the session actor through its receiver cleanup path.
                close_critical(&self.connection, b"critical stream write timed out");
                return Err(TransportError::ControlWriteTimedOut);
            }
        }
        Ok(())
    }
}

pub struct ControlReceiver {
    stream: RecvStream,
    connection: Connection,
    state: ControlReceiveState,
}

enum ControlReceiveState {
    Length { bytes: [u8; 4], filled: usize },
    Frame { bytes: Vec<u8>, filled: usize },
}

impl Default for ControlReceiveState {
    fn default() -> Self {
        Self::Length {
            bytes: [0; 4],
            filled: 0,
        }
    }
}

impl fmt::Debug for ControlReceiver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ControlReceiver")
            .finish_non_exhaustive()
    }
}

impl ControlReceiver {
    /// Receive one complete control message.
    ///
    /// This future is cancellation-safe. Partial length and payload reads stay
    /// in the receiver, so polling it from `tokio::select!` cannot discard
    /// bytes when another branch wins.
    pub async fn receive(&mut self) -> Result<InputControlMessage, TransportError> {
        let frame = loop {
            match &mut self.state {
                ControlReceiveState::Length { bytes, filled } if *filled < bytes.len() => {
                    match self.stream.read(&mut bytes[*filled..]).await {
                        Ok(Some(read)) => *filled += read,
                        Ok(None) => {
                            close_critical(&self.connection, b"critical stream ended");
                            return if *filled == 0 {
                                Err(TransportError::CriticalStreamClosed)
                            } else {
                                Err(quinn::ReadExactError::FinishedEarly(*filled).into())
                            };
                        }
                        Err(error) => {
                            close_critical(&self.connection, b"critical stream read failed");
                            return Err(quinn::ReadExactError::ReadError(error).into());
                        }
                    }
                }
                ControlReceiveState::Length { bytes, .. } => {
                    let length = u32::from_be_bytes(*bytes) as usize;
                    if length == 0 || length > MAX_CONTROL_FRAME_BYTES {
                        close_protocol(&self.connection, b"invalid critical stream frame size");
                        return Err(TransportError::ControlFrameTooLarge {
                            actual: length,
                            maximum: MAX_CONTROL_FRAME_BYTES,
                        });
                    }
                    self.state = ControlReceiveState::Frame {
                        bytes: vec![0; length],
                        filled: 0,
                    };
                }
                ControlReceiveState::Frame { bytes, filled } if *filled < bytes.len() => {
                    match self.stream.read(&mut bytes[*filled..]).await {
                        Ok(Some(read)) => *filled += read,
                        Ok(None) => {
                            close_critical(&self.connection, b"critical stream ended mid-frame");
                            return Err(quinn::ReadExactError::FinishedEarly(*filled).into());
                        }
                        Err(error) => {
                            close_critical(&self.connection, b"critical stream read failed");
                            return Err(quinn::ReadExactError::ReadError(error).into());
                        }
                    }
                }
                ControlReceiveState::Frame { .. } => {
                    let ControlReceiveState::Frame { bytes, .. } = std::mem::take(&mut self.state)
                    else {
                        unreachable!();
                    };
                    break bytes;
                }
            }
        };
        let decoded = match decode_wire(&frame) {
            Ok(decoded) => decoded,
            Err(error) => {
                close_protocol(&self.connection, b"invalid critical stream message");
                return Err(error.into());
            }
        };
        match decoded.message {
            WireMessage::NegotiationOffer(offer) => {
                Ok(InputControlMessage::NegotiationOffer(offer))
            }
            WireMessage::NegotiatedSession(session) => {
                Ok(InputControlMessage::NegotiatedSession(session))
            }
            WireMessage::ReliableControl(message) => Ok(InputControlMessage::Reliable(message)),
            _ => {
                close_protocol(&self.connection, b"message on wrong channel");
                Err(TransportError::InvalidControlFamily)
            }
        }
    }
}

#[derive(Clone)]
pub struct DatagramChannel {
    connection: Connection,
    negotiated_maximum: Arc<AtomicUsize>,
    outgoing: Arc<LatestDatagramQueue>,
}

impl fmt::Debug for DatagramChannel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatagramChannel")
            .field("negotiated_maximum", &self.negotiated_maximum())
            .finish_non_exhaustive()
    }
}

impl DatagramChannel {
    /// Fix the immutable application datagram limit selected during negotiation.
    pub fn configure_maximum(&self, maximum: u32) -> Result<(), TransportError> {
        let requested = maximum as usize;
        let path_maximum = self.connection.max_datagram_size();
        if requested == 0 || path_maximum.is_none_or(|path| requested > path) {
            return Err(TransportError::InvalidDatagramSize {
                requested,
                path_maximum,
            });
        }
        match self.negotiated_maximum.compare_exchange(
            0,
            requested,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(current) if current == requested => Ok(()),
            Err(current) => {
                Err(TransportError::DatagramSizeAlreadyNegotiated { current, requested })
            }
        }
    }

    pub fn negotiated_maximum(&self) -> Option<usize> {
        match self.negotiated_maximum.load(Ordering::Acquire) {
            0 => None,
            maximum => Some(maximum),
        }
    }

    /// Latest-wins cumulative motion. Quinn discards older unsent datagrams to
    /// make room; this intentionally never waits for old motion.
    pub fn send_motion(&self, frame: &MotionFrame) -> Result<(), TransportError> {
        self.send_wire(WireMessage::Motion(frame.clone()))
    }

    /// Latest-wins application probe or echo.
    pub fn send_probe(&self, probe: &ProbeMessage) -> Result<(), TransportError> {
        self.send_wire(WireMessage::Probe(*probe))
    }

    /// Exact number of pending application datagrams replaced by fresher data.
    pub fn dropped_datagrams(&self) -> u64 {
        self.outgoing.dropped()
    }

    fn send_wire(&self, message: WireMessage) -> Result<(), TransportError> {
        let encoded = encode_wire(&message)?;
        self.check_size(encoded.len())?;
        if !self.outgoing.enqueue(Bytes::from(encoded)) {
            return Err(TransportError::DatagramQueueClosed);
        }
        Ok(())
    }

    pub async fn receive(&self) -> Result<InputDatagram, TransportError> {
        let bytes = self.connection.read_datagram().await?;
        if let Err(error) = self.check_size(bytes.len()) {
            close_protocol(&self.connection, b"datagram exceeded negotiated maximum");
            return Err(error);
        }
        let decoded = match decode_wire(&bytes) {
            Ok(decoded) => decoded,
            Err(error) => {
                close_protocol(&self.connection, b"invalid input datagram");
                return Err(error.into());
            }
        };
        match decoded.message {
            WireMessage::Motion(frame) => Ok(InputDatagram::Motion(frame)),
            WireMessage::Probe(probe) => Ok(InputDatagram::Probe(probe)),
            _ => {
                close_protocol(&self.connection, b"message on wrong channel");
                Err(TransportError::InvalidDatagramFamily)
            }
        }
    }

    pub fn close(&self) {
        self.connection.close(VarInt::from_u32(0), b"");
    }

    pub async fn closed(&self) -> quinn::ConnectionError {
        self.connection.closed().await
    }

    fn check_size(&self, actual: usize) -> Result<(), TransportError> {
        let negotiated = self
            .negotiated_maximum()
            .ok_or(TransportError::DatagramSizeNotNegotiated)?;
        if actual > negotiated {
            return Err(TransportError::DatagramTooLarge { actual, negotiated });
        }
        let path_maximum = self.connection.max_datagram_size();
        if path_maximum.is_none_or(|path| actual > path) {
            return Err(TransportError::InvalidDatagramSize {
                requested: actual,
                path_maximum,
            });
        }
        Ok(())
    }
}

/// One pending application datagram. The pump uses Quinn's waiting API, so
/// Quinn never evicts an uncounted datagram; producers replace only this slot.
struct LatestDatagramQueue {
    pending: Mutex<Option<Bytes>>,
    notify: Notify,
    dropped: AtomicU64,
    closed: AtomicBool,
}

impl LatestDatagramQueue {
    fn new() -> Self {
        Self {
            pending: Mutex::new(None),
            notify: Notify::new(),
            dropped: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        }
    }

    fn enqueue(&self, datagram: Bytes) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        if pending.replace(datagram).is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        drop(pending);
        self.notify.notify_one();
        true
    }

    fn take(&self) -> Option<Bytes> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        self.notify.notify_waiters();
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

fn pump_datagrams(connection: Connection, outgoing: Arc<LatestDatagramQueue>) {
    tokio::spawn(async move {
        loop {
            let notified = outgoing.notify.notified();
            if let Some(datagram) = outgoing.take() {
                if connection.send_datagram_wait(datagram).await.is_err() {
                    break;
                }
                continue;
            }
            tokio::select! {
                () = notified => {}
                _ = connection.closed() => break,
            }
        }
        outgoing.close();
    });
}

pub async fn connect_input(
    endpoint: &Endpoint,
    remote: SocketAddr,
    config: &InputClientConfig,
) -> Result<InputConnection, TransportError> {
    // Full handshake only. The early-data conversion path is intentionally absent.
    let connection = endpoint
        .connect_with(config.quinn.clone(), remote, SERVER_NAME_PLACEHOLDER)?
        .await?;
    verify_connection(
        &connection,
        Some(&config.expected_peer_spki),
        INPUT_ALPN_PROTOCOL,
    )?;
    let (mut send, receive) = connection.open_bi().await?;
    if let Err(error) = send.write_all(CONTROL_STREAM_PREFACE).await {
        close_critical(&connection, b"critical stream preface failed");
        return Err(error.into());
    }
    Ok(input_connection(
        connection,
        send,
        receive,
        config.expected_peer_spki.clone(),
    ))
}

pub async fn accept_input(
    incoming: Incoming,
    config: &InputServerConfig,
) -> Result<InputConnection, TransportError> {
    let connection = incoming.await?;
    let peer_spki = verify_connection(&connection, None, INPUT_ALPN_PROTOCOL)?;
    // TLS rejects keys outside the allowlist. Recheck the snapshot supplied by
    // the caller so a runtime revocation also covers handshakes already in
    // flight when Endpoint::set_server_config replaced the TLS configuration.
    if !config.allows_peer(&peer_spki) {
        close_protocol(&connection, b"peer RPK is no longer allowed");
        return Err(TransportError::PeerIdentityMismatch);
    }
    let (send, mut receive) = connection.accept_bi().await?;
    let mut preface = vec![0_u8; CONTROL_STREAM_PREFACE.len()];
    if let Err(error) = receive.read_exact(&mut preface).await {
        close_critical(&connection, b"critical stream preface failed");
        return Err(error.into());
    }
    if preface != CONTROL_STREAM_PREFACE {
        close_protocol(&connection, b"invalid critical stream preface");
        return Err(TransportError::InvalidControlPreface);
    }
    Ok(input_connection(connection, send, receive, peer_spki))
}

fn input_connection(
    connection: Connection,
    send: SendStream,
    receive: RecvStream,
    peer_spki: Arc<[u8]>,
) -> InputConnection {
    monitor_send_half(send.stopped(), connection.clone());
    let negotiated_maximum = Arc::new(AtomicUsize::new(0));
    let outgoing = Arc::new(LatestDatagramQueue::new());
    pump_datagrams(connection.clone(), outgoing.clone());
    InputConnection {
        peer_spki,
        control_send: ControlSender {
            stream: send,
            connection: connection.clone(),
        },
        control_receive: ControlReceiver {
            stream: receive,
            connection: connection.clone(),
            state: ControlReceiveState::default(),
        },
        datagrams: DatagramChannel {
            connection,
            negotiated_maximum,
            outgoing,
        },
    }
}

fn monitor_send_half(
    stopped: impl Future<Output = Result<Option<VarInt>, quinn::StoppedError>> + Send + 'static,
    connection: Connection,
) {
    tokio::spawn(async move {
        if stopped.await.is_ok() {
            close_critical(&connection, b"critical stream stopped");
        }
    });
}

/// Pairing-only result. It proves possession of the presented RPK and exposes
/// a TLS-exporter transcript binding, but deliberately has no input API.
pub struct PairingConnection {
    connection: Connection,
    peer_spki: Arc<[u8]>,
    send: SendStream,
    receive: RecvStream,
}

impl fmt::Debug for PairingConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PairingConnection")
            .field("peer_spki", &"[redacted]")
            .field("remote_address", &self.connection.remote_address())
            .finish_non_exhaustive()
    }
}

impl PairingConnection {
    pub fn peer_spki(&self) -> &[u8] {
        &self.peer_spki
    }

    /// A handshake-unique value for the pairing transcript/SAS calculation.
    pub fn transcript_binding(&self) -> Result<[u8; 32], TransportError> {
        let mut binding = [0_u8; 32];
        self.connection
            .export_keying_material(&mut binding, PAIRING_EXPORTER_LABEL, b"")
            .map_err(|_| TransportError::PairingExporter)?;
        Ok(binding)
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.connection.remote_address()
    }

    /// Exchange one bounded metadata offer on the pairing-only stream.
    pub async fn exchange_offer(
        &mut self,
        local: &PairingOffer,
    ) -> Result<PairingOffer, TransportError> {
        let encoded = encode_wire(&WireMessage::Pairing(local.clone()))?;
        if encoded.len() > MAX_PAIRING_FRAME_BYTES {
            return Err(TransportError::PairingFrameTooLarge {
                actual: encoded.len(),
                maximum: MAX_PAIRING_FRAME_BYTES,
            });
        }
        let length = u32::try_from(encoded.len()).expect("pairing frame bound fits u32");
        self.send
            .write_all(&length.to_be_bytes())
            .await
            .map_err(|error| TransportError::PairingStream(error.to_string()))?;
        self.send
            .write_all(&encoded)
            .await
            .map_err(|error| TransportError::PairingStream(error.to_string()))?;

        let mut length = [0_u8; 4];
        self.receive
            .read_exact(&mut length)
            .await
            .map_err(|error| TransportError::PairingStream(error.to_string()))?;
        let length = u32::from_be_bytes(length) as usize;
        if length == 0 || length > MAX_PAIRING_FRAME_BYTES {
            close_protocol(&self.connection, b"invalid pairing frame size");
            return Err(TransportError::PairingFrameTooLarge {
                actual: length,
                maximum: MAX_PAIRING_FRAME_BYTES,
            });
        }
        let mut frame = vec![0_u8; length];
        self.receive
            .read_exact(&mut frame)
            .await
            .map_err(|error| TransportError::PairingStream(error.to_string()))?;
        // Both peers finish and consume the whole stream before either caller
        // closes the connection. Without this barrier, the faster side can
        // send CONNECTION_CLOSE while the peer's offer is still unread.
        self.send
            .finish()
            .map_err(|error| TransportError::PairingStream(error.to_string()))?;
        self.receive
            .read_to_end(0)
            .await
            .map_err(|error| TransportError::PairingStream(error.to_string()))?;
        match self.send.stopped().await {
            Ok(None) => {}
            Ok(Some(code)) => {
                return Err(TransportError::PairingStream(format!(
                    "peer stopped the pairing stream with code {code}"
                )));
            }
            Err(error) => return Err(TransportError::PairingStream(error.to_string())),
        }
        match decode_wire(&frame)?.message {
            WireMessage::Pairing(offer) => Ok(offer),
            _ => {
                close_protocol(&self.connection, b"message on pairing-only stream");
                Err(TransportError::InvalidPairingFamily)
            }
        }
    }

    pub fn close(&self) {
        self.connection.close(VarInt::from_u32(0), b"");
    }
}

pub async fn connect_pairing(
    endpoint: &Endpoint,
    remote: SocketAddr,
    config: &PairingClientConfig,
) -> Result<PairingConnection, TransportError> {
    let connection = endpoint
        .connect_with(config.quinn.clone(), remote, SERVER_NAME_PLACEHOLDER)?
        .await?;
    let peer_spki = verify_connection(&connection, None, PAIRING_ALPN_PROTOCOL)?;
    let (mut send, receive) = connection.open_bi().await?;
    send.write_all(PAIRING_STREAM_PREFACE)
        .await
        .map_err(|error| TransportError::PairingStream(error.to_string()))?;
    Ok(PairingConnection {
        connection,
        peer_spki,
        send,
        receive,
    })
}

pub async fn accept_pairing(
    incoming: Incoming,
    _config: &PairingServerConfig,
) -> Result<PairingConnection, TransportError> {
    let connection = incoming.await?;
    let peer_spki = verify_connection(&connection, None, PAIRING_ALPN_PROTOCOL)?;
    let (send, mut receive) = connection.accept_bi().await?;
    let mut preface = vec![0_u8; PAIRING_STREAM_PREFACE.len()];
    receive
        .read_exact(&mut preface)
        .await
        .map_err(|error| TransportError::PairingStream(error.to_string()))?;
    if preface != PAIRING_STREAM_PREFACE {
        close_protocol(&connection, b"invalid pairing stream preface");
        return Err(TransportError::InvalidPairingPreface);
    }
    Ok(PairingConnection {
        connection,
        peer_spki,
        send,
        receive,
    })
}

fn verify_connection(
    connection: &Connection,
    expected_spki: Option<&[u8]>,
    expected_alpn: &[u8],
) -> Result<Arc<[u8]>, TransportError> {
    let handshake = connection
        .handshake_data()
        .ok_or(TransportError::InvalidAlpn)?
        .downcast::<quinn::crypto::rustls::HandshakeData>()
        .map_err(|_| TransportError::InvalidAlpn)?;
    if handshake.protocol.as_deref() != Some(expected_alpn) {
        close_protocol(connection, b"ALPN mismatch");
        return Err(TransportError::InvalidAlpn);
    }

    let identity = connection
        .peer_identity()
        .ok_or(TransportError::MissingPeerIdentity)?
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| TransportError::MissingPeerIdentity)?;
    if identity.len() != 1 || identity[0].is_empty() {
        close_protocol(connection, b"invalid peer RPK identity");
        return Err(TransportError::MissingPeerIdentity);
    }
    let presented: Arc<[u8]> = Arc::from(identity[0].as_ref());
    if expected_spki.is_some_and(|expected| expected != presented.as_ref()) {
        close_protocol(connection, b"peer RPK pin mismatch");
        return Err(TransportError::PeerIdentityMismatch);
    }
    Ok(presented)
}

fn close_critical(connection: &Connection, reason: &'static [u8]) {
    connection.close(CRITICAL_STREAM_ERROR, reason);
}

fn close_protocol(connection: &Connection, reason: &'static [u8]) {
    connection.close(PROTOCOL_ERROR, reason);
}

#[cfg(test)]
mod tests {
    use super::LatestDatagramQueue;
    use bytes::Bytes;

    #[test]
    fn latest_datagram_queue_counts_replaced_pending_payloads() {
        let queue = LatestDatagramQueue::new();
        assert!(queue.enqueue(Bytes::from_static(b"old")));
        assert!(queue.enqueue(Bytes::from_static(b"new")));
        assert_eq!(queue.dropped(), 1);
        assert_eq!(queue.take(), Some(Bytes::from_static(b"new")));
        queue.close();
        assert!(!queue.enqueue(Bytes::from_static(b"closed")));
    }
}
