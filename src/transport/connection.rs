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
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use quinn::{Connection, Endpoint, Incoming, RecvStream, SendStream, VarInt};
use rustls::pki_types::CertificateDer;
use sha2::Sha256;
use spake2::{Ed25519Group, Spake2};
use tokio::sync::Notify;

use crate::{
    core::{
        MotionFrame, NegotiatedSession, NegotiationOffer, ProbeMessage, ReliableControlMessage,
    },
    wire::{
        Family, MAX_PAIRING_PAYLOAD_BYTES, MAX_RELIABLE_PAYLOAD_BYTES, PairingOffer, WireMessage,
        decode as decode_wire, decode_family, encode as encode_wire,
    },
};

use super::{
    INPUT_ALPN_PROTOCOL, InputClientConfig, InputServerConfig, PAIRING_ALPN_PROTOCOL,
    PairingClientConfig, TransportError,
};

const SERVER_NAME_PLACEHOLDER: &str = "zflow.invalid";
const CONTROL_STREAM_PREFACE: &[u8] = b"zflow-control\0";
const PAIRING_STREAM_PREFACE: &[u8] = b"zflow-pair\0";
const PAIRING_PASSWORD_LABEL: &[u8] = b"zflow pairing setup code v4\0";
const PAIRING_TRANSCRIPT_LABEL: &[u8] = b"zflow pairing transcript v4\0";
const PAIRING_CLIENT_PROOF_LABEL: &[u8] = b"zflow pairing client proof v4";
const PAIRING_SERVER_PROOF_LABEL: &[u8] = b"zflow pairing server proof v4";
/// One side byte plus a compressed Ed25519 point.
const PAIRING_KEY_EXCHANGE_BYTES: usize = 33;
const PAIRING_PROOF_BYTES: usize = 32;
const MAX_CONTROL_FRAME_BYTES: usize = MAX_RELIABLE_PAYLOAD_BYTES + 64;
const MAX_PAIRING_FRAME_BYTES: usize = MAX_PAIRING_PAYLOAD_BYTES + 64;
const CRITICAL_STREAM_ERROR: VarInt = VarInt::from_u32(0x100);
const PROTOCOL_ERROR: VarInt = VarInt::from_u32(0x101);
/// Tells the initiator its setup code was wrong rather than the network lost.
const PAIRING_CODE_MISMATCH: VarInt = VarInt::from_u32(0x102);
const PAIRING_EXPORTER_LABEL: &[u8] = b"EXPORTER-zflow-pairing-v2";
const CONTROL_WRITE_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputControlMessage {
    NegotiationOffer(NegotiationOffer),
    NegotiatedSession(NegotiatedSession),
    Reliable(ReliableControlMessage),
    Desktop(crate::desktop::DesktopMessage),
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
    clipboard: ClipboardChannel,
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

    pub fn into_channels(self) -> InputChannels {
        InputChannels {
            control_send: self.control_send,
            control_receive: self.control_receive,
            datagrams: self.datagrams,
            clipboard: self.clipboard,
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
    pub clipboard: ClipboardChannel,
}

/// The kind and length a clipboard stream announces, if it may be read.
fn clip_header(header: &[u8; 7]) -> Option<(crate::clipboard::ClipKind, usize)> {
    let length = u32::from_be_bytes(header[3..].try_into().expect("four bytes")) as usize;
    let kind = crate::clipboard::ClipKind::from_code(header[2])?;
    (header[..2] == *CLIPBOARD_PREFACE && length <= crate::clipboard::MAX_CLIP_BYTES)
        .then_some((kind, length))
}

/// Starts every clipboard stream, so a stray stream is refused at once.
const CLIPBOARD_PREFACE: &[u8; 2] = b"ZC";
/// Why a clipboard stream was stopped, for the other end's diagnostics.
const CLIPBOARD_REFUSED: u32 = 1;

/// The one-way streams that carry clipboard contents, one transfer each.
/// They send at a lower priority than the control stream, and QUIC puts
/// datagrams ahead of stream data in every packet, so a large clip does
/// not hold up input.
#[derive(Clone)]
pub struct ClipboardChannel {
    connection: Connection,
}

impl ClipboardChannel {
    /// Sends one clip and finishes its stream. Dropping the future part way
    /// ends the stream early, and the other end discards what it got.
    pub async fn send(&self, clip: &crate::clipboard::Clip) -> Result<(), TransportError> {
        let failed = |error: &dyn fmt::Display| TransportError::Clipboard(error.to_string());
        let mut stream = self.connection.open_uni().await?;
        stream.set_priority(-1).map_err(|error| failed(&error))?;
        let mut header = [0_u8; 7];
        header[..2].copy_from_slice(CLIPBOARD_PREFACE);
        header[2] = clip.kind().code();
        header[3..].copy_from_slice(&(clip.data().len() as u32).to_be_bytes());
        stream
            .write_all(&header)
            .await
            .map_err(|error| failed(&error))?;
        stream
            .write_all(clip.data())
            .await
            .map_err(|error| failed(&error))?;
        stream.finish().map_err(|error| failed(&error))?;
        Ok(())
    }

    /// Waits for the next clip from the peer. A bad or oversized clip is an
    /// error for that stream only; the connection stays up.
    pub async fn receive(&self) -> Result<crate::clipboard::Clip, TransportError> {
        use crate::clipboard::Clip;
        let failed = |error: &dyn fmt::Display| TransportError::Clipboard(error.to_string());
        let mut stream = self.connection.accept_uni().await?;
        let mut header = [0_u8; 7];
        stream
            .read_exact(&mut header)
            .await
            .map_err(|error| failed(&error))?;
        let Some((kind, length)) = clip_header(&header) else {
            let _ = stream.stop(VarInt::from_u32(CLIPBOARD_REFUSED));
            return Err(TransportError::Clipboard(
                "the peer sent an invalid or oversized clip".into(),
            ));
        };
        let mut data = vec![0_u8; length];
        stream
            .read_exact(&mut data)
            .await
            .map_err(|error| failed(&error))?;
        Clip::new(kind, data).map_err(|error| failed(&format!("{error:#}")))
    }
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

    pub async fn send_desktop(
        &mut self,
        message: crate::desktop::DesktopMessage,
    ) -> Result<(), TransportError> {
        self.send_wire(WireMessage::Desktop(message)).await
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
        match decoded {
            WireMessage::NegotiationOffer(offer) => {
                Ok(InputControlMessage::NegotiationOffer(offer))
            }
            WireMessage::NegotiatedSession(session) => {
                Ok(InputControlMessage::NegotiatedSession(session))
            }
            WireMessage::ReliableControl(message) => Ok(InputControlMessage::Reliable(message)),
            WireMessage::Desktop(message) => Ok(InputControlMessage::Desktop(message)),
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

    /// Latest-wins cumulative motion. New motion replaces only older pending
    /// motion and is drained before pending probes.
    pub fn send_motion(&self, frame: &MotionFrame) -> Result<(), TransportError> {
        self.send_wire(
            WireMessage::Motion(frame.clone()),
            PendingDatagramClass::Motion,
        )
    }

    /// Latest-wins application probe or echo.
    pub fn send_probe(&self, probe: &ProbeMessage) -> Result<(), TransportError> {
        self.send_wire(WireMessage::Probe(*probe), PendingDatagramClass::Probe)
    }

    /// Exact number of pending application datagrams replaced by fresher data.
    pub fn dropped_datagrams(&self) -> u64 {
        self.outgoing.dropped()
    }

    fn send_wire(
        &self,
        message: WireMessage,
        class: PendingDatagramClass,
    ) -> Result<(), TransportError> {
        let encoded = encode_wire(&message)?;
        self.check_size(encoded.len())?;
        if !self.outgoing.enqueue(class, Bytes::from(encoded)) {
            return Err(TransportError::DatagramQueueClosed);
        }
        Ok(())
    }

    pub async fn receive(&self) -> Result<InputDatagram, TransportError> {
        let bytes = self.connection.read_datagram().await?;
        // Our own path MTU says nothing about what the peer may send us.
        if let Err(error) = self.check_negotiated(bytes.len()) {
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
        match decoded {
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

    fn check_negotiated(&self, actual: usize) -> Result<(), TransportError> {
        let negotiated = self
            .negotiated_maximum()
            .ok_or(TransportError::DatagramSizeNotNegotiated)?;
        if actual > negotiated {
            return Err(TransportError::DatagramTooLarge { actual, negotiated });
        }
        Ok(())
    }

    fn check_size(&self, actual: usize) -> Result<(), TransportError> {
        self.check_negotiated(actual)?;
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

#[derive(Debug, Clone, Copy)]
enum PendingDatagramClass {
    Motion,
    Probe,
}

#[derive(Debug, Default)]
struct PendingDatagrams {
    motion: Option<Bytes>,
    probe: Option<Bytes>,
}

/// One latest-wins slot per datagram class. Motion is always drained before
/// probes, and each class can replace only its own pending payload. The pump
/// uses Quinn's waiting API, so Quinn never evicts an uncounted datagram.
struct LatestDatagramQueue {
    pending: Mutex<PendingDatagrams>,
    notify: Notify,
    dropped: AtomicU64,
    closed: AtomicBool,
}

impl LatestDatagramQueue {
    fn new() -> Self {
        Self {
            pending: Mutex::new(PendingDatagrams::default()),
            notify: Notify::new(),
            dropped: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        }
    }

    fn enqueue(&self, class: PendingDatagramClass, datagram: Bytes) -> bool {
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
        let replaced = match class {
            PendingDatagramClass::Motion => pending.motion.replace(datagram),
            PendingDatagramClass::Probe => pending.probe.replace(datagram),
        };
        if replaced.is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        drop(pending);
        self.notify.notify_one();
        true
    }

    fn take(&self) -> Option<Bytes> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.motion.take().or_else(|| pending.probe.take())
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.motion.take();
        pending.probe.take();
        drop(pending);
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
            connection: connection.clone(),
            negotiated_maximum,
            outgoing,
        },
        clipboard: ClipboardChannel { connection },
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
    role: PairingRole,
}

#[derive(Debug, Clone, Copy)]
enum PairingRole {
    Client,
    Server,
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

    /// A handshake-unique value that ties the pairing proofs to this TLS session.
    fn transcript_binding(&self) -> Result<[u8; 32], TransportError> {
        let mut binding = [0_u8; 32];
        self.connection
            .export_keying_material(&mut binding, PAIRING_EXPORTER_LABEL, b"")
            .map_err(|_| TransportError::PairingExporter)?;
        Ok(binding)
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.connection.remote_address()
    }

    /// Proves both sides know the setup code, then returns the peer's offer.
    ///
    /// SPAKE2 turns the six-digit code into a shared key without exposing the
    /// code to offline guessing. The initiator proves the key first and the
    /// listener answers only after checking that proof, so each connection
    /// tests one guess. Both proofs cover the TLS exporter, both presented
    /// keys, both offers and both key-exchange messages, so a relay between
    /// two TLS sessions cannot make them match.
    pub async fn authenticate(
        &mut self,
        local_spki: &[u8],
        local: &PairingOffer,
        code: &[u8],
    ) -> Result<PairingOffer, TransportError> {
        let local_frame = encode_wire(&WireMessage::Pairing(local.clone()))?;
        if local_frame.len() > MAX_PAIRING_FRAME_BYTES {
            return Err(TransportError::PairingFrameTooLarge {
                actual: local_frame.len(),
                maximum: MAX_PAIRING_FRAME_BYTES,
            });
        }
        let binding = self.transcript_binding()?;
        let peer_spki = self.peer_spki.clone();
        let (client_spki, server_spki) = match self.role {
            PairingRole::Client => (local_spki, peer_spki.as_ref()),
            PairingRole::Server => (peer_spki.as_ref(), local_spki),
        };
        let password = spake2::Password::new([PAIRING_PASSWORD_LABEL, code].concat());
        let client_id = spake2::Identity::new(client_spki);
        let server_id = spake2::Identity::new(server_spki);
        match self.role {
            PairingRole::Client => {
                let (exchange, client_message) =
                    Spake2::<Ed25519Group>::start_a(&password, &client_id, &server_id);
                self.write_frame(&local_frame).await?;
                self.write(&client_message).await?;
                let peer_frame = self.read_frame().await?;
                let peer = self.decode_offer(&peer_frame)?;
                let mut server_message = [0_u8; PAIRING_KEY_EXCHANGE_BYTES];
                self.read_exact(&mut server_message).await?;
                let proofs = self.proof_keys(exchange, &server_message, &binding)?;
                let transcript = pairing_transcript(
                    &binding,
                    [
                        client_spki,
                        server_spki,
                        &local_frame,
                        &peer_frame,
                        &client_message,
                        &server_message,
                    ],
                );
                self.write(&proofs.sign(PairingRole::Client, &transcript))
                    .await?;
                let mut proof = [0_u8; PAIRING_PROOF_BYTES];
                if let Err(error) = self.read_exact(&mut proof).await {
                    return Err(if self.closed_for_wrong_code() {
                        TransportError::PairingCodeMismatch
                    } else {
                        error
                    });
                }
                if !proofs.verify(PairingRole::Server, &transcript, &proof) {
                    close_protocol(&self.connection, b"pairing proof mismatch");
                    return Err(TransportError::PairingCodeMismatch);
                }
                Ok(peer)
            }
            PairingRole::Server => {
                let peer_frame = self.read_frame().await?;
                let peer = self.decode_offer(&peer_frame)?;
                let mut client_message = [0_u8; PAIRING_KEY_EXCHANGE_BYTES];
                self.read_exact(&mut client_message).await?;
                let (exchange, server_message) =
                    Spake2::<Ed25519Group>::start_b(&password, &client_id, &server_id);
                let proofs = self.proof_keys(exchange, &client_message, &binding)?;
                self.write_frame(&local_frame).await?;
                self.write(&server_message).await?;
                let transcript = pairing_transcript(
                    &binding,
                    [
                        client_spki,
                        server_spki,
                        &peer_frame,
                        &local_frame,
                        &client_message,
                        &server_message,
                    ],
                );
                let mut proof = [0_u8; PAIRING_PROOF_BYTES];
                self.read_exact(&mut proof).await?;
                if !proofs.verify(PairingRole::Client, &transcript, &proof) {
                    self.connection
                        .close(PAIRING_CODE_MISMATCH, b"wrong setup code");
                    return Err(TransportError::PairingCodeMismatch);
                }
                self.write(&proofs.sign(PairingRole::Server, &transcript))
                    .await?;
                Ok(peer)
            }
        }
    }

    /// The listener says whether it kept the pairing, so the initiator saves
    /// the peer only when both computers will trust each other. The listener
    /// then waits briefly for the initiator to close, so the answer arrives.
    pub async fn finish(&mut self, saved: bool) -> Result<(), TransportError> {
        match self.role {
            PairingRole::Client => self.close(),
            PairingRole::Server => {
                self.write(&[u8::from(saved)]).await?;
                let _ = self.send.finish();
                let _ =
                    tokio::time::timeout(Duration::from_secs(3), self.connection.closed()).await;
            }
        }
        Ok(())
    }

    /// Reads the listener's answer from [`Self::finish`].
    pub async fn read_saved(&mut self) -> Result<bool, TransportError> {
        let mut saved = [0_u8; 1];
        self.read_exact(&mut saved).await?;
        Ok(saved == [1])
    }

    fn proof_keys(
        &self,
        exchange: Spake2<Ed25519Group>,
        peer_message: &[u8],
        binding: &[u8; 32],
    ) -> Result<PairingProofKeys, TransportError> {
        let shared = exchange.finish(peer_message).map_err(|_| {
            close_protocol(&self.connection, b"invalid pairing key exchange");
            TransportError::PairingKeyExchange
        })?;
        Ok(PairingProofKeys::derive(&shared, binding))
    }

    fn closed_for_wrong_code(&self) -> bool {
        matches!(
            self.connection.close_reason(),
            Some(quinn::ConnectionError::ApplicationClosed(close))
                if close.error_code == PAIRING_CODE_MISMATCH
        )
    }

    fn decode_offer(&self, frame: &[u8]) -> Result<PairingOffer, TransportError> {
        // Only the pairing decoder is reachable before trust exists.
        match decode_family(frame, Family::Pairing)? {
            WireMessage::Pairing(offer) => Ok(offer),
            _ => {
                close_protocol(&self.connection, b"message on pairing-only stream");
                Err(TransportError::InvalidPairingFamily)
            }
        }
    }

    async fn write(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.send
            .write_all(bytes)
            .await
            .map_err(|error| TransportError::PairingStream(error.to_string()))
    }

    async fn write_frame(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        let length = u32::try_from(frame.len()).expect("pairing frame bound fits u32");
        self.write(&length.to_be_bytes()).await?;
        self.write(frame).await
    }

    async fn read_exact(&mut self, buffer: &mut [u8]) -> Result<(), TransportError> {
        self.receive
            .read_exact(buffer)
            .await
            .map_err(|error| TransportError::PairingStream(error.to_string()))
    }

    async fn read_frame(&mut self) -> Result<Vec<u8>, TransportError> {
        let mut length = [0_u8; 4];
        self.read_exact(&mut length).await?;
        let length = u32::from_be_bytes(length) as usize;
        if length == 0 || length > MAX_PAIRING_FRAME_BYTES {
            close_protocol(&self.connection, b"invalid pairing frame size");
            return Err(TransportError::PairingFrameTooLarge {
                actual: length,
                maximum: MAX_PAIRING_FRAME_BYTES,
            });
        }
        let mut frame = vec![0_u8; length];
        self.read_exact(&mut frame).await?;
        Ok(frame)
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
        role: PairingRole::Client,
    })
}

pub async fn accept_pairing(incoming: Incoming) -> Result<PairingConnection, TransportError> {
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
        role: PairingRole::Server,
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

/// Proof keys from the SPAKE2 secret, salted with the TLS exporter.
struct PairingProofKeys {
    client: [u8; 32],
    server: [u8; 32],
}

impl PairingProofKeys {
    fn derive(shared: &[u8], binding: &[u8; 32]) -> Self {
        let keys = Hkdf::<Sha256>::new(Some(binding), shared);
        let mut client = [0_u8; 32];
        let mut server = [0_u8; 32];
        keys.expand(PAIRING_CLIENT_PROOF_LABEL, &mut client)
            .expect("32 bytes is a valid HKDF-SHA256 output");
        keys.expand(PAIRING_SERVER_PROOF_LABEL, &mut server)
            .expect("32 bytes is a valid HKDF-SHA256 output");
        Self { client, server }
    }

    fn mac(&self, role: PairingRole, transcript: &[u8]) -> Hmac<Sha256> {
        let key = match role {
            PairingRole::Client => &self.client,
            PairingRole::Server => &self.server,
        };
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
        mac.update(transcript);
        mac
    }

    fn sign(&self, role: PairingRole, transcript: &[u8]) -> [u8; PAIRING_PROOF_BYTES] {
        self.mac(role, transcript).finalize().into_bytes().into()
    }

    fn verify(&self, role: PairingRole, transcript: &[u8], proof: &[u8]) -> bool {
        self.mac(role, transcript).verify_slice(proof).is_ok()
    }
}

fn pairing_transcript(binding: &[u8; 32], parts: [&[u8]; 6]) -> Vec<u8> {
    let mut transcript = [PAIRING_TRANSCRIPT_LABEL, binding].concat();
    for part in parts {
        transcript.extend_from_slice(&(part.len() as u64).to_be_bytes());
        transcript.extend_from_slice(part);
    }
    transcript
}

fn close_critical(connection: &Connection, reason: &'static [u8]) {
    connection.close(CRITICAL_STREAM_ERROR, reason);
}

fn close_protocol(connection: &Connection, reason: &'static [u8]) {
    connection.close(PROTOCOL_ERROR, reason);
}

#[cfg(test)]
mod tests {
    use super::{LatestDatagramQueue, PendingDatagramClass};

    #[test]
    fn a_clipboard_stream_announces_a_known_kind_within_the_cap() {
        use crate::clipboard::{ClipKind, MAX_CLIP_BYTES};
        let header = |prefix: &[u8; 2], kind: u8, length: usize| {
            let mut header = [0_u8; 7];
            header[..2].copy_from_slice(prefix);
            header[2] = kind;
            header[3..].copy_from_slice(&(length as u32).to_be_bytes());
            header
        };
        assert_eq!(
            super::clip_header(&header(b"ZC", 2, MAX_CLIP_BYTES)),
            Some((ClipKind::Png, MAX_CLIP_BYTES))
        );
        assert_eq!(
            super::clip_header(&header(b"ZC", 2, MAX_CLIP_BYTES + 1)),
            None
        );
        assert_eq!(super::clip_header(&header(b"ZC", 9, 5)), None);
        assert_eq!(super::clip_header(&header(b"XX", 1, 5)), None);
    }
    use bytes::Bytes;

    #[test]
    fn latest_datagram_queue_keeps_motion_and_probe_pending_independently() {
        let queue = LatestDatagramQueue::new();
        assert!(queue.enqueue(PendingDatagramClass::Motion, Bytes::from_static(b"motion")));
        assert!(queue.enqueue(PendingDatagramClass::Probe, Bytes::from_static(b"probe")));
        assert_eq!(queue.dropped(), 0);
        assert_eq!(queue.take(), Some(Bytes::from_static(b"motion")));
        assert_eq!(queue.take(), Some(Bytes::from_static(b"probe")));
    }

    #[test]
    fn latest_datagram_queue_prioritizes_latest_motion_over_probe() {
        let queue = LatestDatagramQueue::new();
        assert!(queue.enqueue(
            PendingDatagramClass::Motion,
            Bytes::from_static(b"old motion")
        ));
        assert!(queue.enqueue(PendingDatagramClass::Probe, Bytes::from_static(b"probe")));
        assert!(queue.enqueue(
            PendingDatagramClass::Motion,
            Bytes::from_static(b"new motion")
        ));
        assert_eq!(queue.dropped(), 1);
        assert_eq!(queue.take(), Some(Bytes::from_static(b"new motion")));
        assert_eq!(queue.take(), Some(Bytes::from_static(b"probe")));
    }

    #[test]
    fn latest_datagram_queue_counts_probe_replacements_without_displacing_motion() {
        let queue = LatestDatagramQueue::new();
        assert!(queue.enqueue(
            PendingDatagramClass::Probe,
            Bytes::from_static(b"old probe")
        ));
        assert!(queue.enqueue(PendingDatagramClass::Motion, Bytes::from_static(b"motion")));
        assert!(queue.enqueue(
            PendingDatagramClass::Probe,
            Bytes::from_static(b"new probe")
        ));
        assert_eq!(queue.dropped(), 1);
        assert_eq!(queue.take(), Some(Bytes::from_static(b"motion")));
        assert_eq!(queue.take(), Some(Bytes::from_static(b"new probe")));
    }

    #[test]
    fn latest_datagram_queue_closure_discards_both_classes() {
        let queue = LatestDatagramQueue::new();
        assert!(queue.enqueue(PendingDatagramClass::Motion, Bytes::from_static(b"motion")));
        assert!(queue.enqueue(PendingDatagramClass::Probe, Bytes::from_static(b"probe")));
        queue.close();
        assert_eq!(queue.take(), None);
        assert!(!queue.enqueue(
            PendingDatagramClass::Motion,
            Bytes::from_static(b"closed motion")
        ));
        assert!(!queue.enqueue(
            PendingDatagramClass::Probe,
            Bytes::from_static(b"closed probe")
        ));
    }
}

#[cfg(test)]
mod pairing_tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::{
        identity::Identity,
        transport::{pairing_client_config, pairing_server_config},
    };

    const LOOPBACK: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

    fn offer(label: &str) -> PairingOffer {
        PairingOffer {
            device_label: Some(label.into()),
            input_port: 43119,
            input_candidates: Vec::new(),
        }
    }

    fn identity() -> (tempfile::TempDir, Identity) {
        let directory = tempfile::tempdir().unwrap();
        let identity = Identity::load_or_create(directory.path()).unwrap();
        (directory, identity)
    }

    /// One pairing connection from `client` to a fresh listener for `server`.
    async fn connected(
        client: &Identity,
        server: &Identity,
    ) -> (PairingConnection, PairingConnection) {
        let server_config = pairing_server_config(server).unwrap();
        let server_endpoint = Endpoint::server(server_config.quinn_config(), LOOPBACK).unwrap();
        let address = server_endpoint.local_addr().unwrap();
        let client_endpoint = Endpoint::client(LOOPBACK).unwrap();
        let client_config = pairing_client_config(client).unwrap();
        let (client, server) = tokio::join!(
            connect_pairing(&client_endpoint, address, &client_config),
            async { accept_pairing(server_endpoint.accept().await.unwrap()).await }
        );
        (client.unwrap(), server.unwrap())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn matching_codes_authenticate_both_sides_and_report_the_save() {
        let (_c, client_identity) = identity();
        let (_s, server_identity) = identity();
        let (mut client, mut server) = connected(&client_identity, &server_identity).await;
        let (client_offer, server_offer) = (offer("client"), offer("server"));
        let (seen_by_client, seen_by_server) = tokio::join!(
            client.authenticate(client_identity.spki(), &client_offer, b"482913"),
            server.authenticate(server_identity.spki(), &server_offer, b"482913"),
        );
        assert_eq!(seen_by_client.unwrap(), offer("server"));
        assert_eq!(seen_by_server.unwrap(), offer("client"));
        let (saved, finished) = tokio::join!(
            async {
                let saved = client.read_saved().await;
                client.finish(true).await.unwrap();
                saved
            },
            server.finish(true)
        );
        assert!(saved.unwrap());
        finished.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wrong_code_fails_on_both_sides_as_a_code_mismatch() {
        let (_c, client_identity) = identity();
        let (_s, server_identity) = identity();
        let (mut client, mut server) = connected(&client_identity, &server_identity).await;
        let (client_offer, server_offer) = (offer("client"), offer("server"));
        let (client, server) = tokio::join!(
            client.authenticate(client_identity.spki(), &client_offer, b"482913"),
            server.authenticate(server_identity.spki(), &server_offer, b"482914"),
        );
        assert!(matches!(server, Err(TransportError::PairingCodeMismatch)));
        assert!(matches!(client, Err(TransportError::PairingCodeMismatch)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_relay_between_two_sessions_cannot_pair_even_with_the_right_code() {
        let (_c, client_identity) = identity();
        let (_s, server_identity) = identity();
        let (_m, relay_identity) = identity();
        // The relay terminates TLS on both sides and copies the pairing bytes.
        let (mut client, mut relay_server) = connected(&client_identity, &relay_identity).await;
        let (mut relay_client, mut server) = connected(&relay_identity, &server_identity).await;
        let (client_offer, server_offer) = (offer("client"), offer("server"));
        let relay = async {
            tokio::select! {
                _ = tokio::io::copy(&mut relay_server.receive, &mut relay_client.send) => {}
                _ = tokio::io::copy(&mut relay_client.receive, &mut relay_server.send) => {}
            }
            relay_server.close();
            relay_client.close();
        };
        let honest = async {
            tokio::join!(
                client.authenticate(client_identity.spki(), &client_offer, b"482913"),
                server.authenticate(server_identity.spki(), &server_offer, b"482913"),
            )
        };
        let ((client, server), ()) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(honest, relay)
        })
        .await
        .unwrap();
        assert!(matches!(server, Err(TransportError::PairingCodeMismatch)));
        assert!(client.is_err());
    }
}
