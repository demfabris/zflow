use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{Context, Result, bail};

use crate::{
    identity::Identity,
    transport::{
        PairingConnection, PairingServerConfig, accept_pairing, connect_pairing,
        pairing_client_config, pairing_server_config,
    },
    wire::{PairingMethod, PairingOffer, WireMessage, encode as encode_wire},
};

pub const DEFAULT_PAIRING_PORT: u16 = 43120;

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

pub fn make_offer(
    device_label: Option<String>,
    input_port: u16,
    input_candidates: Vec<SocketAddr>,
) -> Result<PairingOffer> {
    if input_port == 0 {
        bail!("input port must be non-zero");
    }
    validate_label(device_label.as_deref())?;
    let mut handshake_nonce = [0_u8; 32];
    getrandom::fill(&mut handshake_nonce)
        .map_err(|error| anyhow::anyhow!("could not generate the pairing nonce: {error}"))?;
    Ok(PairingOffer {
        handshake_nonce,
        method: PairingMethod::ShortAuthenticationString,
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
    server_config: PairingServerConfig,
    identity: &'identity Identity,
    local_offer: PairingOffer,
}

impl<'identity> PairingListener<'identity> {
    pub fn bind(
        identity: &'identity Identity,
        address: SocketAddr,
        local_offer: PairingOffer,
    ) -> Result<Self> {
        let server_config = pairing_server_config(identity)?;
        let endpoint = quinn::Endpoint::server(server_config.quinn_config(), address)
            .with_context(|| format!("could not bind the pairing listener at {address}"))?;
        Ok(Self {
            endpoint,
            server_config,
            identity,
            local_offer,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint
            .local_addr()
            .context("pairing listener has no local address")
    }

    pub async fn accept(&self) -> Result<PairingSession> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .context("pairing listener closed")?;
        let mut connection = accept_pairing(incoming, &self.server_config).await?;
        let observation = complete(self.identity, &self.local_offer, &mut connection).await?;
        Ok(PairingSession {
            observation,
            _connection: connection,
            _endpoint: Some(self.endpoint.clone()),
        })
    }
}

/// Opens a temporary pairing transport. The returned session owns its endpoint.
pub async fn begin(
    identity: &Identity,
    remote: Option<SocketAddr>,
    input_port: u16,
) -> Result<PairingSession> {
    let label = std::env::var("HOSTNAME")
        .ok()
        .filter(|label| label.len() <= 255 && validate_label(Some(label)).is_ok());
    let offer = make_offer(label, input_port, Vec::new())?;
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
    if name.len() > 255 {
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
    let mut connection = connect_pairing(&endpoint, remote, &config).await?;
    let observation = complete(identity, local_offer, &mut connection).await?;
    Ok(PairingSession {
        observation,
        _connection: connection,
        _endpoint: Some(endpoint),
    })
}

async fn complete(
    identity: &Identity,
    local_offer: &PairingOffer,
    connection: &mut PairingConnection,
) -> Result<PairingObservation> {
    if local_offer.method != PairingMethod::ShortAuthenticationString {
        bail!("the CLI supports only short authentication string pairing");
    }
    let peer_spki = connection.peer_spki().to_vec();
    if peer_spki == identity.spki() {
        bail!("refusing to pair an identity with itself");
    }
    let binding = connection.transcript_binding()?;
    let remote_address = connection.remote_address();
    let peer_offer = connection.exchange_offer(local_offer).await?;
    if peer_offer.method != PairingMethod::ShortAuthenticationString {
        bail!("peer selected an unsupported pairing method");
    }
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
