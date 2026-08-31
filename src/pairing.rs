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

    pub async fn accept(&self) -> Result<PairingObservation> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .context("pairing listener closed")?;
        let mut connection = accept_pairing(incoming, &self.server_config).await?;
        complete(self.identity, &self.local_offer, &mut connection).await
    }
}

pub async fn connect(
    identity: &Identity,
    remote: SocketAddr,
    local_offer: &PairingOffer,
) -> Result<PairingObservation> {
    let bind = match remote.ip() {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };
    let endpoint = quinn::Endpoint::client(bind)
        .with_context(|| format!("could not bind a pairing client for {remote}"))?;
    let config = pairing_client_config(identity)?;
    let mut connection = connect_pairing(&endpoint, remote, &config).await?;
    complete(identity, local_offer, &mut connection).await
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
    connection.close();
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
}
