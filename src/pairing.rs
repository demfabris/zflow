use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};

use crate::{
    identity::Identity,
    transport::{
        PairingConnection, TransportError, accept_pairing, connect_pairing, pairing_client_config,
        pairing_server_config,
    },
    wire::PairingOffer,
};

pub const DEFAULT_PAIRING_PORT: u16 = 43120;
const MAX_LABEL_BYTES: usize = 255;
/// Leaves room for a " 99" suffix when two computers share a host name.
const MAX_BASE_NAME_BYTES: usize = MAX_LABEL_BYTES - 8;

/// Bounds the automatic part of pairing, which never waits for a person.
const PAIRING_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);
/// Wrong codes one listener tolerates. Each is one online guess in a million.
const MAX_WRONG_CODES: usize = 3;
/// Other failed connections one listener tolerates before it gives up.
const MAX_FAILED_CONNECTIONS: usize = 16;

/// A six-digit setup code. The listening computer shows it and the other
/// computer types it; SPAKE2 checks it without sending it anywhere.
#[derive(Clone, PartialEq, Eq)]
pub struct SetupCode([u8; 6]);

impl SetupCode {
    pub fn generate() -> Result<Self> {
        // Rejection sampling keeps every code equally likely.
        loop {
            let mut bytes = [0_u8; 4];
            getrandom::fill(&mut bytes)
                .map_err(|error| anyhow::anyhow!("could not generate a setup code: {error}"))?;
            let value = u32::from_be_bytes(bytes);
            if value < 4_294_000_000 {
                return Ok(Self::from_value(value % 1_000_000));
            }
        }
    }

    fn from_value(mut value: u32) -> Self {
        let mut digits = [b'0'; 6];
        for digit in digits.iter_mut().rev() {
            *digit = b'0' + (value % 10) as u8;
            value /= 10;
        }
        Self(digits)
    }

    /// Accepts what people type: six digits, optionally split by a space or dash.
    pub fn parse(input: &str) -> Result<Self> {
        let digits: Vec<u8> = input
            .trim()
            .bytes()
            .filter(|byte| !matches!(byte, b' ' | b'-'))
            .collect();
        match <[u8; 6]>::try_from(digits.as_slice()) {
            Ok(digits) if digits.iter().all(u8::is_ascii_digit) => Ok(Self(digits)),
            _ => bail!("Enter the six-digit code shown on the other computer"),
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The six digits without the display space, for sending to a service.
    pub fn digits(&self) -> &str {
        std::str::from_utf8(&self.0).expect("setup codes are ASCII digits")
    }
}

impl fmt::Display for SetupCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = self.digits();
        write!(formatter, "{} {}", &digits[..3], &digits[3..])
    }
}

impl fmt::Debug for SetupCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SetupCode([redacted])")
    }
}

/// Where to reach a pairing listener. A bare IP address means the default
/// pairing port, since an address is what people read off the other screen.
pub fn parse_pairing_address(input: &str) -> Result<SocketAddr> {
    let input = input.trim();
    if let Ok(address) = input.parse::<SocketAddr>() {
        return Ok(address);
    }
    let ip = input
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .with_context(|| format!("{input:?} is not an IP address"))?;
    Ok(SocketAddr::new(ip, DEFAULT_PAIRING_PORT))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingObservation {
    pub peer_spki: Vec<u8>,
    pub peer_label: Option<String>,
    pub peer_candidates: Vec<SocketAddr>,
}

/// A pairing that proved the setup code. A listener's session still owes the
/// initiator an answer about whether the peer was saved.
pub struct PairingSession {
    observation: PairingObservation,
    connection: PairingConnection,
    _endpoint: Option<quinn::Endpoint>,
}

impl PairingSession {
    pub fn observation(&self) -> &PairingObservation {
        &self.observation
    }

    /// A listener tells the initiator whether it saved the peer; the initiator
    /// waits for that before saving its side. An initiator's session has
    /// already heard the answer, so finishing it only closes the connection.
    pub async fn finish(mut self, saved: bool) {
        if let Err(error) = self.connection.finish(saved).await {
            tracing::debug!("pairing answer was not delivered: {error}");
        }
    }
}

impl std::ops::Deref for PairingSession {
    type Target = PairingObservation;

    fn deref(&self) -> &Self::Target {
        &self.observation
    }
}

pub fn make_offer(
    device_label: Option<String>,
    input_port: u16,
    input_candidates: Vec<SocketAddr>,
) -> Result<PairingOffer> {
    if input_port == 0 {
        bail!("input port must be non-zero");
    }
    validate_label(device_label.as_deref())?;
    Ok(PairingOffer {
        device_label,
        input_port,
        input_candidates: input_candidates
            .into_iter()
            .map(|candidate| candidate.to_string())
            .collect(),
    })
}

pub struct PairingListener<'identity> {
    endpoint: quinn::Endpoint,
    identity: &'identity Identity,
    local_offer: PairingOffer,
    code: SetupCode,
}

impl<'identity> PairingListener<'identity> {
    pub fn bind(
        identity: &'identity Identity,
        address: SocketAddr,
        local_offer: PairingOffer,
        code: SetupCode,
    ) -> Result<Self> {
        let config = pairing_server_config(identity)?.quinn_config();
        let endpoint = quinn::Endpoint::server(config, address)
            .with_context(|| format!("could not bind the pairing listener at {address}"))?;
        Ok(Self {
            endpoint,
            identity,
            local_offer,
            code,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint
            .local_addr()
            .context("pairing listener has no local address")
    }

    /// Waits for a computer that knows the setup code. Wrong codes and failed
    /// connections are dropped and the listener takes the next one, up to caps.
    pub async fn accept(&self) -> Result<PairingSession> {
        let (mut wrong_codes, mut failures) = (0, 0);
        loop {
            let incoming = self
                .endpoint
                .accept()
                .await
                .context("pairing listener closed")?;
            let authenticated = authenticate(
                self.identity,
                &self.local_offer,
                &self.code,
                accept_pairing(incoming),
            );
            let error = match authenticated.await {
                Ok((connection, observation)) => {
                    return Ok(PairingSession {
                        observation,
                        connection,
                        _endpoint: Some(self.endpoint.clone()),
                    });
                }
                Err(error) => error,
            };
            if is_wrong_code(&error) {
                wrong_codes += 1;
                if wrong_codes == MAX_WRONG_CODES {
                    bail!(
                        "Pairing stopped after {MAX_WRONG_CODES} wrong codes; start again for a new code"
                    );
                }
                tracing::warn!("pairing attempt used a wrong code, still listening");
            } else {
                failures += 1;
                if failures == MAX_FAILED_CONNECTIONS {
                    return Err(error.context("too many failed pairing connections"));
                }
                tracing::warn!("pairing attempt failed, still listening: {error:#}");
            }
        }
    }
}

/// Opens a temporary pairing transport. The returned session owns its endpoint.
pub async fn begin(
    identity: &Identity,
    remote: Option<SocketAddr>,
    input_port: u16,
    code: &SetupCode,
) -> Result<PairingSession> {
    let offer = make_offer(local_device_label(), input_port, Vec::new())?;
    if let Some(remote) = remote {
        crate::discovery::UntrustedCandidate::explicit(remote)?;
        connect(identity, remote, &offer, code).await
    } else {
        PairingListener::bind(
            identity,
            SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), DEFAULT_PAIRING_PORT),
            offer,
            code.clone(),
        )?
        .accept()
        .await
    }
}

/// Saves the peer this pairing authenticated and returns its name, taken from
/// the peer's host name. Pairing a known computer again only refreshes its
/// addresses, and a new key never replaces a trusted one.
pub fn add_paired_peer(
    config: &mut crate::config::Config,
    observation: &PairingObservation,
    receiver: bool,
) -> Result<String> {
    let record = crate::config::PeerConfig::from_spki(
        &observation.peer_spki,
        observation.peer_candidates.clone(),
        crate::config::PeerPermissions {
            connect: true,
            send_normal: receiver,
            receive_normal: !receiver,
            inject_prelogin: false,
        },
    )?;
    if let Some((name, existing)) = config
        .peers
        .iter_mut()
        .find(|(_, peer)| peer.spki_der_hex == record.spki_der_hex)
    {
        existing.addresses = record.addresses;
        return Ok(name.clone());
    }
    let base = observation
        .peer_label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or("Computer");
    let mut end = base.len().min(MAX_BASE_NAME_BYTES);
    while !base.is_char_boundary(end) {
        end -= 1;
    }
    let base = &base[..end];
    let name = std::iter::once(base.to_owned())
        .chain((2..).map(|number| format!("{base} {number}")))
        .find(|name| !config.peers.contains_key(name))
        .expect("some numbered name is free");
    config.peers.insert(name.clone(), record);
    Ok(name)
}

pub async fn connect(
    identity: &Identity,
    remote: SocketAddr,
    local_offer: &PairingOffer,
    code: &SetupCode,
) -> Result<PairingSession> {
    let bind = match remote.ip() {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };
    let endpoint = quinn::Endpoint::client(bind)
        .with_context(|| format!("could not bind a pairing client for {remote}"))?;
    let config = pairing_client_config(identity)?;
    let (mut connection, observation) = authenticate(
        identity,
        local_offer,
        code,
        connect_pairing(&endpoint, remote, &config),
    )
    .await
    .map_err(|error| {
        if is_wrong_code(&error) {
            anyhow::anyhow!("That code does not match the one on the other computer")
        } else {
            error
        }
    })?;
    // The listener saves first, so this side keeps a peer only when both will.
    let saved = tokio::time::timeout(PAIRING_EXCHANGE_TIMEOUT, connection.read_saved())
        .await
        .context("the other computer did not confirm the pairing in time")??;
    ensure!(saved, "The other computer could not save the pairing");
    Ok(PairingSession {
        observation,
        connection,
        _endpoint: Some(endpoint),
    })
}

async fn authenticate(
    identity: &Identity,
    local_offer: &PairingOffer,
    code: &SetupCode,
    connection: impl Future<Output = Result<PairingConnection, TransportError>>,
) -> Result<(PairingConnection, PairingObservation)> {
    tokio::time::timeout(PAIRING_EXCHANGE_TIMEOUT, async {
        let mut connection = connection.await?;
        let observation = complete(identity, local_offer, code, &mut connection).await?;
        Ok((connection, observation))
    })
    .await
    .context("pairing peer did not finish the exchange in time")?
}

fn is_wrong_code(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<TransportError>(),
        Some(TransportError::PairingCodeMismatch)
    )
}

async fn complete(
    identity: &Identity,
    local_offer: &PairingOffer,
    code: &SetupCode,
    connection: &mut PairingConnection,
) -> Result<PairingObservation> {
    let peer_spki = connection.peer_spki().to_vec();
    if peer_spki == identity.spki() {
        bail!("refusing to pair an identity with itself");
    }
    let remote_address = connection.remote_address();
    let peer_offer = connection
        .authenticate(identity.spki(), local_offer, code.as_bytes())
        .await?;
    if peer_offer.input_port == 0 {
        bail!("peer advertised an invalid input port");
    }
    validate_label(peer_offer.device_label.as_deref())?;

    let mut peer_candidates = peer_offer
        .input_candidates
        .iter()
        .map(|candidate| {
            let address = candidate
                .parse::<SocketAddr>()
                .with_context(|| format!("peer advertised invalid candidate {candidate:?}"))?;
            crate::discovery::UntrustedCandidate::explicit(address)
                .with_context(|| format!("peer advertised unusable candidate {candidate:?}"))?;
            Ok(address)
        })
        .collect::<Result<Vec<_>>>()?;
    let observed = SocketAddr::new(remote_address.ip(), peer_offer.input_port);
    if !peer_candidates.contains(&observed) {
        peer_candidates.push(observed);
    }
    peer_candidates.sort_unstable();
    peer_candidates.dedup();

    Ok(PairingObservation {
        peer_spki,
        peer_label: peer_offer.device_label,
        peer_candidates,
    })
}

/// The OS host name, cut to the label bound. GUI apps and services do not
/// inherit the shell's `$HOSTNAME`, so this asks the OS.
pub(crate) fn local_device_label() -> Option<String> {
    let mut buffer = [0_u8; 256];
    // SAFETY: the pointer and length describe `buffer`, which outlives the call.
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    let name = String::from_utf8_lossy(&buffer[..end]);
    let mut end = name.len().min(MAX_LABEL_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let label = name[..end].trim();
    validate_label(Some(label))
        .is_ok()
        .then(|| label.to_owned())
}

fn validate_label(label: Option<&str>) -> Result<()> {
    if label.is_some_and(|label| label.is_empty() || label.chars().any(char::is_control)) {
        bail!("pairing device labels must be non-empty printable text");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOOPBACK: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

    fn identity() -> (tempfile::TempDir, Identity) {
        let directory = tempfile::tempdir().unwrap();
        let identity = Identity::load_or_create(directory.path()).unwrap();
        (directory, identity)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn both_sides_pair_with_the_setup_code_and_learn_candidates() {
        let (_left_dir, left) = identity();
        let (_right_dir, right) = identity();
        let code = SetupCode::generate().unwrap();
        let left_offer = make_offer(Some("left".into()), 43119, Vec::new()).unwrap();
        let right_offer = make_offer(Some("right".into()), 43121, Vec::new()).unwrap();
        let listener = PairingListener::bind(&right, LOOPBACK, right_offer, code.clone()).unwrap();
        let address = listener.local_addr().unwrap();

        let (seen_by_left, seen_by_right) =
            tokio::join!(connect(&left, address, &left_offer, &code), async {
                let session = listener.accept().await.unwrap();
                let observation = session.observation().clone();
                session.finish(true).await;
                observation
            });
        let seen_by_left = seen_by_left.unwrap();

        assert_eq!(seen_by_left.peer_spki, right.spki());
        assert_eq!(seen_by_right.peer_spki, left.spki());
        assert_eq!(seen_by_left.peer_label.as_deref(), Some("right"));
        assert_eq!(seen_by_right.peer_label.as_deref(), Some("left"));
        assert!(
            seen_by_left
                .peer_candidates
                .contains(&"127.0.0.1:43121".parse().unwrap())
        );
        assert!(
            seen_by_right
                .peer_candidates
                .contains(&"127.0.0.1:43119".parse().unwrap())
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_listener_takes_a_right_code_after_wrong_ones_up_to_a_cap() {
        let (_left_dir, left) = identity();
        let (_right_dir, right) = identity();
        let code = SetupCode::parse("482913").unwrap();
        let wrong = SetupCode::parse("482914").unwrap();
        let offer = make_offer(None, 43119, Vec::new()).unwrap();

        let listener =
            PairingListener::bind(&right, LOOPBACK, offer.clone(), code.clone()).unwrap();
        let address = listener.local_addr().unwrap();
        let clients = async {
            for _ in 1..MAX_WRONG_CODES {
                let error = connect(&left, address, &offer, &wrong).await.err().unwrap();
                assert!(error.to_string().contains("does not match"));
            }
            connect(&left, address, &offer, &code).await
        };
        let (seen_by_left, ()) = tokio::join!(clients, async {
            listener.accept().await.unwrap().finish(true).await;
        });
        assert_eq!(seen_by_left.unwrap().peer_spki, right.spki());

        let listener =
            PairingListener::bind(&right, LOOPBACK, offer.clone(), code.clone()).unwrap();
        let address = listener.local_addr().unwrap();
        let clients = async {
            for _ in 0..MAX_WRONG_CODES {
                assert!(connect(&left, address, &offer, &wrong).await.is_err());
            }
        };
        let ((), result) = tokio::join!(clients, listener.accept());
        assert!(result.err().unwrap().to_string().contains("wrong codes"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_initiator_keeps_nothing_when_the_listener_does_not_save() {
        let (_left_dir, left) = identity();
        let (_right_dir, right) = identity();
        let code = SetupCode::generate().unwrap();
        let offer = make_offer(None, 43119, Vec::new()).unwrap();
        let listener =
            PairingListener::bind(&right, LOOPBACK, offer.clone(), code.clone()).unwrap();
        let address = listener.local_addr().unwrap();
        let (result, ()) = tokio::join!(connect(&left, address, &offer, &code), async {
            listener.accept().await.unwrap().finish(false).await;
        });
        assert!(result.err().unwrap().to_string().contains("could not save"));
    }

    #[test]
    fn setup_codes_accept_what_people_type() {
        for typed in ["482913", "482 913", "482-913", " 482913 "] {
            assert_eq!(SetupCode::parse(typed).unwrap().as_bytes(), b"482913");
        }
        for typed in ["", "48291", "4829134", "48a913", "４８２９１３"] {
            assert!(SetupCode::parse(typed).is_err(), "{typed:?}");
        }
        assert_eq!(SetupCode::parse("007001").unwrap().to_string(), "007 001");
        assert_eq!(SetupCode::from_value(417).as_bytes(), b"000417");
        let code = SetupCode::generate().unwrap();
        assert!(code.as_bytes().iter().all(u8::is_ascii_digit));
        assert!(!format!("{code:?}").contains(std::str::from_utf8(code.as_bytes()).unwrap()));
    }

    #[test]
    fn pairing_addresses_default_to_the_pairing_port() {
        let parse = |input| parse_pairing_address(input).unwrap().to_string();
        assert_eq!(parse("192.168.1.20"), "192.168.1.20:43120");
        assert_eq!(parse(" 192.168.1.20:5000 "), "192.168.1.20:5000");
        assert_eq!(parse("fe80::1"), "[fe80::1]:43120");
        assert_eq!(parse("[fd00::7]"), "[fd00::7]:43120");
        assert_eq!(parse("[fd00::7]:43120"), "[fd00::7]:43120");
        assert!(parse_pairing_address("ubuntu.local").is_err());
    }

    #[test]
    fn device_label_comes_from_the_os_host_name() {
        // cargo, like launchd and systemd, does not pass $HOSTNAME along.
        let label = local_device_label().expect("this host has a name");
        assert!(label.len() <= MAX_LABEL_BYTES);
        validate_label(Some(&label)).unwrap();
    }

    #[test]
    fn offer_requires_a_real_input_port() {
        assert!(make_offer(None, 0, Vec::new()).is_err());
        assert!(make_offer(Some("bad\u{1b}label".into()), 43119, Vec::new()).is_err());
    }

    #[test]
    fn paired_peers_are_named_from_the_host_and_never_replace_trust() {
        let (_other_directory, other) = identity();
        let (_directory, known) = identity();
        let observation = PairingObservation {
            peer_spki: known.spki().to_vec(),
            peer_label: Some("ubuntu".into()),
            peer_candidates: vec!["192.0.2.1:43119".parse().unwrap()],
        };
        let mut config = crate::config::Config::default();
        assert_eq!(
            add_paired_peer(&mut config, &observation, false).unwrap(),
            "ubuntu"
        );
        let permissions = config.peers["ubuntu"].permissions;
        assert!(permissions.connect && permissions.receive_normal);
        assert!(!permissions.send_normal && !permissions.inject_prelogin);

        // Pairing the same computer again refreshes its addresses and keeps
        // its permissions, whatever it calls itself now.
        let moved = PairingObservation {
            peer_label: Some("renamed".into()),
            peer_candidates: vec!["192.0.2.9:43119".parse().unwrap()],
            ..observation.clone()
        };
        assert_eq!(
            add_paired_peer(&mut config, &moved, true).unwrap(),
            "ubuntu"
        );
        assert_eq!(config.peers.len(), 1);
        assert_eq!(config.peers["ubuntu"].permissions, permissions);
        assert_eq!(
            config.peers["ubuntu"].addresses,
            vec!["192.0.2.9:43119".parse().unwrap()]
        );

        // A different computer with the same host name gets its own entry.
        let twin = PairingObservation {
            peer_spki: other.spki().to_vec(),
            ..observation.clone()
        };
        assert_eq!(
            add_paired_peer(&mut config, &twin, false).unwrap(),
            "ubuntu 2"
        );
        assert_eq!(config.peers["ubuntu"].spki_der().unwrap(), known.spki());

        let mut receiver = crate::config::Config::default();
        let unnamed = PairingObservation {
            peer_label: None,
            ..observation
        };
        assert_eq!(
            add_paired_peer(&mut receiver, &unnamed, true).unwrap(),
            "Computer"
        );
        let permissions = receiver.peers["Computer"].permissions;
        assert!(permissions.connect && permissions.send_normal);
        assert!(!permissions.receive_normal && !permissions.inject_prelogin);
    }
}
