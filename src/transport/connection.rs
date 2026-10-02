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
        Family, Hello, MAX_HELLO_PAYLOAD_BYTES, MAX_RELIABLE_PAYLOAD_BYTES, WireMessage,
        decode as decode_wire, decode_family, encode as encode_wire,
    },
};

use super::{
    HELLO_ALPN_PROTOCOL, HelloClientConfig, INPUT_ALPN_PROTOCOL, InputClientConfig,
    InputServerConfig, TransportError, error::NOT_TRUSTED,
};

const SERVER_NAME_PLACEHOLDER: &str = "zflow.invalid";
const CONTROL_STREAM_PREFACE: &[u8] = b"zflow-control\0";
const HELLO_STREAM_PREFACE: &[u8] = b"zflow-hello\0";
const HELLO_EXPORTER_LABEL: &[u8] = b"EXPORTER-zflow-hello-v1";
const MAX_HELLO_FRAME_BYTES: usize = MAX_HELLO_PAYLOAD_BYTES + 64;
/// How long a listener keeps a hello open for the initiator to read the
/// answer and close.
const HELLO_LINGER: Duration = Duration::from_secs(3);
const MAX_CONTROL_FRAME_BYTES: usize = MAX_RELIABLE_PAYLOAD_BYTES + 64;
const CRITICAL_STREAM_ERROR: VarInt = VarInt::from_u32(0x100);
const PROTOCOL_ERROR: VarInt = VarInt::from_u32(0x101);
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

/// What arrived on the input port.
#[derive(Debug)]
pub enum Accepted {
    /// A trusted computer's input connection.
    Input(InputConnection),
    /// Any computer saying hello, trusted or not.
    Hello(HelloConnection),
    /// A key off the allowlist asked for input. Its connection is already
    /// closed with a code that tells the other computer why.
    NotTrusted {
        peer_spki: Vec<u8>,
        remote_address: SocketAddr,
    },
}

/// Finishes one handshake on the input port and sorts it by protocol and key.
/// Pass the configuration the endpoint uses now: a peer revoked while its
/// handshake was in flight is refused too.
pub async fn accept(
    incoming: Incoming,
    config: &InputServerConfig,
) -> Result<Accepted, TransportError> {
    let connection = incoming.await?;
    if negotiated_protocol(&connection).as_deref() == Some(HELLO_ALPN_PROTOCOL) {
        let peer_spki = verify_connection(&connection, None, HELLO_ALPN_PROTOCOL)?;
        let (send, mut receive) = connection.accept_bi().await?;
        let mut preface = vec![0_u8; HELLO_STREAM_PREFACE.len()];
        receive
            .read_exact(&mut preface)
            .await
            .map_err(|error| TransportError::HelloStream(error.to_string()))?;
        if preface != HELLO_STREAM_PREFACE {
            close_protocol(&connection, b"invalid hello stream preface");
            return Err(TransportError::InvalidHelloPreface);
        }
        return Ok(Accepted::Hello(HelloConnection {
            connection,
            peer_spki,
            send,
            receive,
            role: Role::Server,
        }));
    }
    let peer_spki = verify_connection(&connection, None, INPUT_ALPN_PROTOCOL)?;
    if !config.allows_peer(&peer_spki) {
        connection.close(NOT_TRUSTED, b"this computer has not added yours");
        return Ok(Accepted::NotTrusted {
            peer_spki: peer_spki.to_vec(),
            remote_address: connection.remote_address(),
        });
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
    Ok(Accepted::Input(input_connection(
        connection, send, receive, peer_spki,
    )))
}

/// [`accept`] for a caller that takes only input: a hello is closed, and a
/// key off the allowlist is an error.
pub async fn accept_input(
    incoming: Incoming,
    config: &InputServerConfig,
) -> Result<InputConnection, TransportError> {
    match accept(incoming, config).await? {
        Accepted::Input(connection) => Ok(connection),
        Accepted::Hello(hello) => {
            hello.close();
            Err(TransportError::InvalidAlpn)
        }
        Accepted::NotTrusted { .. } => Err(TransportError::NotTrusted),
    }
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

/// A connection between computers that may not trust each other yet. TLS
/// proved each side holds the key it presented; the two trade one [`Hello`]
/// and close. It has no input, clipboard or desktop surface.
pub struct HelloConnection {
    connection: Connection,
    peer_spki: Arc<[u8]>,
    send: SendStream,
    receive: RecvStream,
    role: Role,
}

impl fmt::Debug for HelloConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HelloConnection")
            .field("peer_spki", &"[redacted]")
            .field("remote_address", &self.connection.remote_address())
            .finish_non_exhaustive()
    }
}

impl HelloConnection {
    pub fn peer_spki(&self) -> &[u8] {
        &self.peer_spki
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.connection.remote_address()
    }

    /// A value both ends derive from this TLS session and no other, for
    /// vouching for a third computer inside this hello alone.
    pub fn binding(&self) -> Result<[u8; 32], TransportError> {
        let mut binding = [0_u8; 32];
        self.connection
            .export_keying_material(&mut binding, HELLO_EXPORTER_LABEL, b"")
            .map_err(|_| TransportError::HelloExporter)?;
        Ok(binding)
    }

    /// Sends this computer's hello and returns the other's. The initiator
    /// closes once it has the answer. The listener keeps the connection a
    /// moment longer in the background, so its own hello is not cut off.
    pub async fn exchange(mut self, local: &Hello) -> Result<Hello, TransportError> {
        let frame = encode_wire(&WireMessage::Hello(local.clone()))?;
        if frame.len() > MAX_HELLO_FRAME_BYTES {
            return Err(TransportError::HelloFrameTooLarge {
                actual: frame.len(),
                maximum: MAX_HELLO_FRAME_BYTES,
            });
        }
        let length = u32::try_from(frame.len()).expect("hello frame bound fits u32");
        let failed = |error: quinn::WriteError| TransportError::HelloStream(error.to_string());
        self.send
            .write_all(&length.to_be_bytes())
            .await
            .map_err(failed)?;
        self.send.write_all(&frame).await.map_err(failed)?;
        let _ = self.send.finish();
        let peer = self.read_hello().await;
        match self.role {
            Role::Client => self.close(),
            Role::Server => {
                let connection = self.connection.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(HELLO_LINGER, connection.closed()).await;
                    connection.close(VarInt::from_u32(0), b"");
                });
            }
        }
        peer
    }

    async fn read_hello(&mut self) -> Result<Hello, TransportError> {
        let failed = |error: quinn::ReadExactError| TransportError::HelloStream(error.to_string());
        let mut length = [0_u8; 4];
        self.receive.read_exact(&mut length).await.map_err(failed)?;
        let length = u32::from_be_bytes(length) as usize;
        if length == 0 || length > MAX_HELLO_FRAME_BYTES {
            close_protocol(&self.connection, b"invalid hello frame size");
            return Err(TransportError::HelloFrameTooLarge {
                actual: length,
                maximum: MAX_HELLO_FRAME_BYTES,
            });
        }
        let mut frame = vec![0_u8; length];
        self.receive.read_exact(&mut frame).await.map_err(failed)?;
        // Only the hello decoder is reachable before trust exists.
        match decode_family(&frame, Family::Hello) {
            Ok(WireMessage::Hello(hello)) => Ok(hello),
            Ok(_) => unreachable!("decode_family returns only the family asked for"),
            Err(error) => {
                close_protocol(&self.connection, b"invalid hello");
                Err(error.into())
            }
        }
    }

    pub fn close(&self) {
        self.connection.close(VarInt::from_u32(0), b"");
    }
}

/// Says hello to whatever answers at `remote`, whatever its key.
pub async fn connect_hello(
    endpoint: &Endpoint,
    remote: SocketAddr,
    config: &HelloClientConfig,
) -> Result<HelloConnection, TransportError> {
    let connection = endpoint
        .connect_with(config.quinn.clone(), remote, SERVER_NAME_PLACEHOLDER)?
        .await?;
    let peer_spki = verify_connection(&connection, None, HELLO_ALPN_PROTOCOL)?;
    let (mut send, receive) = connection.open_bi().await?;
    send.write_all(HELLO_STREAM_PREFACE)
        .await
        .map_err(|error| TransportError::HelloStream(error.to_string()))?;
    Ok(HelloConnection {
        connection,
        peer_spki,
        send,
        receive,
        role: Role::Client,
    })
}

#[derive(Debug, Clone, Copy)]
enum Role {
    Client,
    Server,
}

fn negotiated_protocol(connection: &Connection) -> Option<Vec<u8>> {
    connection
        .handshake_data()?
        .downcast::<quinn::crypto::rustls::HandshakeData>()
        .ok()?
        .protocol
}

fn verify_connection(
    connection: &Connection,
    expected_spki: Option<&[u8]>,
    expected_alpn: &[u8],
) -> Result<Arc<[u8]>, TransportError> {
    if negotiated_protocol(connection).as_deref() != Some(expected_alpn) {
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
