use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::{
    identity::Identity,
    transport::{
        PairingConnection, TransportError, accept_pairing, connect_pairing, pairing_client_config,
        pairing_server_config,
    },
    wire::{PairingOffer, WireMessage, encode as encode_wire},
};

pub const DEFAULT_PAIRING_PORT: u16 = 43120;
const MAX_LABEL_BYTES: usize = 255;

/// Bounds the automatic part of pairing, which never waits for a person.
const PAIRING_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);
/// Failed attempts one listener tolerates. Each one lets an active attacker
/// try one more responder offer, so the cap stays small.
const MAX_PAIRING_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingObservation {
    pub peer_spki: Vec<u8>,
    pub peer_label: Option<String>,
    pub peer_candidates: Vec<SocketAddr>,
    pub authentication_code: String,
}

/// Keeps the authenticated pairing transport alive through user confirmation.
pub struct PairingSession {
    observation: PairingObservation,
    _connection: PairingConnection,
    _endpoint: Option<quinn::Endpoint>,
}

impl PairingSession {
    pub fn observation(&self) -> &PairingObservation {
        &self.observation
    }
}

impl std::ops::Deref for PairingSession {
    type Target = PairingObservation;

    fn deref(&self) -> &Self::Target {
        &self.observation
    }
}

fn fresh_nonce() -> Result<[u8; 32]> {
    let mut nonce = [0_u8; 32];
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow::anyhow!("could not generate the pairing nonce: {error}"))?;
    Ok(nonce)
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
        handshake_nonce: fresh_nonce()?,
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
}

impl<'identity> PairingListener<'identity> {
    pub fn bind(
        identity: &'identity Identity,
        address: SocketAddr,
        local_offer: PairingOffer,
    ) -> Result<Self> {
        let config = pairing_server_config(identity)?.quinn_config();
        let endpoint = quinn::Endpoint::server(config, address)
            .with_context(|| format!("could not bind the pairing listener at {address}"))?;
        Ok(Self {
            endpoint,
            identity,
            local_offer,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint
            .local_addr()
            .context("pairing listener has no local address")
    }

    /// Waits for a peer to finish the offer exchange. A peer that fails or
    /// stalls is dropped and the listener takes the next one, up to a cap.
    pub async fn accept(&self) -> Result<PairingSession> {
        let mut failures = 0;
        loop {
            let incoming = self
                .endpoint
                .accept()
                .await
                .context("pairing listener closed")?;
            // Each attempt needs a nonce no peer has seen. Otherwise a peer
            // could fail once after reading this offer, then search offline
            // for a commitment that makes its code match someone else's.
            let offer = PairingOffer {
                handshake_nonce: fresh_nonce()?,
                ..self.local_offer.clone()
            };
            let error = match exchange(self.identity, &offer, accept_pairing(incoming)).await {
                Ok((connection, observation)) => {
                    return Ok(PairingSession {
                        observation,
                        _connection: connection,
                        _endpoint: Some(self.endpoint.clone()),
                    });
                }
                Err(error) => error,
            };
            failures += 1;
            if failures == MAX_PAIRING_ATTEMPTS {
                return Err(error.context("too many failed pairing attempts"));
            }
            tracing::warn!("pairing attempt failed, still listening: {error:#}");
        }
    }
}

/// Opens a temporary pairing transport. The returned session owns its endpoint.
pub async fn begin(
    identity: &Identity,
    remote: Option<SocketAddr>,
    input_port: u16,
) -> Result<PairingSession> {
    let offer = make_offer(local_device_label(), input_port, Vec::new())?;
    if let Some(remote) = remote {
        crate::discovery::UntrustedCandidate::explicit(remote)?;
        connect(identity, remote, &offer).await
    } else {
        PairingListener::bind(
            identity,
            SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), DEFAULT_PAIRING_PORT),
            offer,
        )?
        .accept()
        .await
    }
}

/// Adds only the identity observed on this authenticated transport after a
/// matching code. A pairing request cannot replace an existing trusted peer.
pub fn add_confirmed_peer(
    config: &mut crate::config::Config,
    observation: &PairingObservation,
    name: &str,
    code: &str,
    receiver: bool,
) -> Result<()> {
    let name = name.trim();
    validate_label(Some(name))?;
    if name.len() > MAX_LABEL_BYTES {
        bail!("Computer names must be at most 255 bytes");
    }
    if code != observation.authentication_code {
        bail!("Pairing codes do not match; no computer was added");
    }
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
    if let Some(existing) = config.peers.get_mut(name) {
        if existing.spki_der_hex != record.spki_der_hex {
            bail!("A different computer named {name} is already paired; choose another name");
        }
        // Retrying after cancelling on the other computer keeps permissions.
        existing.addresses = record.addresses;
        return Ok(());
    }
    if let Some((existing_name, _)) = config
        .peers
        .iter()
        .find(|(_, peer)| peer.spki_der_hex == record.spki_der_hex)
    {
        bail!("This computer is already paired as {existing_name}; use that name to pair again");
    }
    config.peers.insert(name.to_owned(), record);
    Ok(())
}

pub async fn connect(
    identity: &Identity,
    remote: SocketAddr,
    local_offer: &PairingOffer,
) -> Result<PairingSession> {
    let bind = match remote.ip() {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };
    let endpoint = quinn::Endpoint::client(bind)
        .with_context(|| format!("could not bind a pairing client for {remote}"))?;
    let config = pairing_client_config(identity)?;
    let (connection, observation) = exchange(
        identity,
        local_offer,
        connect_pairing(&endpoint, remote, &config),
    )
    .await?;
    Ok(PairingSession {
        observation,
        _connection: connection,
        _endpoint: Some(endpoint),
    })
}

async fn exchange(
    identity: &Identity,
    local_offer: &PairingOffer,
    connection: impl Future<Output = Result<PairingConnection, TransportError>>,
) -> Result<(PairingConnection, PairingObservation)> {
    tokio::time::timeout(PAIRING_EXCHANGE_TIMEOUT, async {
        let mut connection = connection.await?;
        let observation = complete(identity, local_offer, &mut connection).await?;
        Ok((connection, observation))
    })
    .await
    .context("pairing peer did not finish the exchange in time")?
}

async fn complete(
    identity: &Identity,
    local_offer: &PairingOffer,
    connection: &mut PairingConnection,
) -> Result<PairingObservation> {
    let peer_spki = connection.peer_spki().to_vec();
    if peer_spki == identity.spki() {
        bail!("refusing to pair an identity with itself");
    }
    let binding = connection.transcript_binding()?;
    let remote_address = connection.remote_address();
    let peer_offer = connection.exchange_offer(local_offer).await?;
    if peer_offer.input_port == 0 {
        bail!("peer advertised an invalid input port");
    }
    validate_label(peer_offer.device_label.as_deref())?;

    let local_encoded = encode_wire(&WireMessage::Pairing(local_offer.clone()))?;
    let peer_encoded = encode_wire(&WireMessage::Pairing(peer_offer.clone()))?;
    let (first, second) = if identity.spki() < peer_spki.as_slice() {
        (&local_encoded, &peer_encoded)
    } else {
        (&peer_encoded, &local_encoded)
    };
    let mut transcript = Vec::with_capacity(32 + 16 + first.len() + second.len());
    transcript.extend_from_slice(&binding);
    transcript.extend_from_slice(&(first.len() as u64).to_be_bytes());
    transcript.extend_from_slice(first);
    transcript.extend_from_slice(&(second.len() as u64).to_be_bytes());
    transcript.extend_from_slice(second);

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

    let observation = PairingObservation {
        peer_spki: peer_spki.clone(),
        peer_label: peer_offer.device_label,
        peer_candidates,
        authentication_code: identity.pairing_code(&peer_spki, &transcript),
    };
    Ok(observation)
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn both_sides_derive_the_same_code_and_observed_candidate() {
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let left = Identity::load_or_create(left_dir.path()).unwrap();
        let right = Identity::load_or_create(right_dir.path()).unwrap();
        let left_offer = make_offer(Some("left".into()), 43119, Vec::new()).unwrap();
        let right_offer = make_offer(Some("right".into()), 43121, Vec::new()).unwrap();
        let listener = PairingListener::bind(
            &right,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            right_offer,
        )
        .unwrap();
        let address = listener.local_addr().unwrap();

        let (seen_by_left, seen_by_right) =
            tokio::join!(connect(&left, address, &left_offer), listener.accept());
        let seen_by_left = seen_by_left.unwrap();
        let seen_by_right = seen_by_right.unwrap();

        assert_eq!(
            seen_by_left.authentication_code,
            seen_by_right.authentication_code
        );
        assert_eq!(seen_by_left.peer_spki, right.spki());
        assert_eq!(seen_by_right.peer_spki, left.spki());
        assert_eq!(seen_by_left.peer_label.as_deref(), Some("right"));
        assert_eq!(seen_by_right.peer_label.as_deref(), Some("left"));
        assert!(
            seen_by_left
                .peer_candidates
                .contains(&SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 43121))
        );
        assert!(
            seen_by_right
                .peer_candidates
                .contains(&SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 43119))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn listener_outlives_failed_attempts_up_to_a_cap() {
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let left = Identity::load_or_create(left_dir.path()).unwrap();
        let right = Identity::load_or_create(right_dir.path()).unwrap();
        let offer = make_offer(None, 43119, Vec::new()).unwrap();
        let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

        // Connecting with the listener's own identity fails on both ends.
        let listener = PairingListener::bind(&right, loopback, offer.clone()).unwrap();
        let address = listener.local_addr().unwrap();
        let clients = async {
            assert!(connect(&right, address, &offer).await.is_err());
            connect(&left, address, &offer).await
        };
        let (seen_by_left, seen_by_right) = tokio::join!(clients, listener.accept());
        assert_eq!(
            seen_by_left.unwrap().authentication_code,
            seen_by_right.unwrap().authentication_code
        );

        let listener = PairingListener::bind(&right, loopback, offer.clone()).unwrap();
        let address = listener.local_addr().unwrap();
        let clients = async {
            for _ in 0..MAX_PAIRING_ATTEMPTS {
                assert!(connect(&right, address, &offer).await.is_err());
            }
        };
        let ((), result) = tokio::join!(clients, listener.accept());
        assert!(result.is_err());
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
    fn gui_pairing_requires_code_and_retries_without_replacing_trust() {
        let directory = tempfile::tempdir().unwrap();
        let identity = Identity::load_or_create(directory.path()).unwrap();
        let observation = PairingObservation {
            peer_spki: identity.spki().to_vec(),
            peer_label: None,
            peer_candidates: vec!["192.0.2.1:43119".parse().unwrap()],
            authentication_code: "123456".into(),
        };
        let mut config = crate::config::Config::default();
        assert!(add_confirmed_peer(&mut config, &observation, "Ubuntu", "000000", false).is_err());
        assert!(config.peers.is_empty());
        add_confirmed_peer(&mut config, &observation, "Ubuntu", "123456", false).unwrap();
        let saved = config.clone();
        let permissions = config.peers["Ubuntu"].permissions;
        assert!(permissions.connect && permissions.receive_normal);
        assert!(!permissions.send_normal && !permissions.inject_prelogin);
        add_confirmed_peer(&mut config, &observation, "Ubuntu", "123456", true).unwrap();
        assert!(
            add_confirmed_peer(&mut config, &observation, "Duplicate", "123456", false).is_err()
        );
        assert_eq!(config, saved);
        let other_directory = tempfile::tempdir().unwrap();
        let other_identity = Identity::load_or_create(other_directory.path()).unwrap();
        let other_observation = PairingObservation {
            peer_spki: other_identity.spki().to_vec(),
            ..observation.clone()
        };
        assert!(
            add_confirmed_peer(&mut config, &other_observation, "Ubuntu", "123456", false).is_err()
        );
        assert_eq!(config, saved);

        let mut receiver = crate::config::Config::default();
        add_confirmed_peer(&mut receiver, &observation, "Mac", "123456", true).unwrap();
        let permissions = receiver.peers["Mac"].permissions;
        assert!(permissions.connect && permissions.send_normal);
        assert!(!permissions.receive_normal && !permissions.inject_prelogin);
    }
}
