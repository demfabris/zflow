use std::{fmt, sync::Arc};

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme,
    client::{
        AlwaysResolvesClientRawPublicKeys, ClientConfig as TlsClientConfig, Resumption,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    crypto::{
        CryptoProvider, WebPkiSupportedAlgorithms, ring, verify_tls13_signature_with_raw_key,
    },
    pki_types::{
        CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer,
        UnixTime,
    },
    server::{
        AlwaysResolvesServerRawPublicKeys, NoServerSessionStorage, ServerConfig as TlsServerConfig,
        danger::{ClientCertVerified, ClientCertVerifier},
    },
    sign::CertifiedKey,
    version,
};

use crate::identity::Identity;

use super::TransportError;

pub const INPUT_ALPN_PROTOCOL: &[u8] = b"zflow/1";
pub const PAIRING_ALPN_PROTOCOL: &[u8] = b"zflow-pair/1";

/// A Quinn client configuration that authenticates one exact peer SPKI.
#[derive(Clone)]
pub struct InputClientConfig {
    pub(super) quinn: quinn::ClientConfig,
    pub(super) expected_peer_spki: Arc<[u8]>,
}

impl fmt::Debug for InputClientConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputClientConfig")
            .field("expected_peer_spki", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl InputClientConfig {
    /// Clone the endpoint configuration for direct use with Quinn APIs.
    pub fn quinn_config(&self) -> quinn::ClientConfig {
        self.quinn.clone()
    }
}

/// A Quinn server configuration that requires a client SPKI from a fixed allowlist.
#[derive(Clone)]
pub struct InputServerConfig {
    pub(super) quinn: quinn::ServerConfig,
    pub(super) allowed_peer_spkis: Arc<[Arc<[u8]>]>,
}

impl fmt::Debug for InputServerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputServerConfig")
            .field("allowed_peer_count", &self.allowed_peer_spkis.len())
            .finish_non_exhaustive()
    }
}

impl InputServerConfig {
    /// Clone the endpoint configuration for [`quinn::Endpoint::server`].
    ///
    /// To add or revoke local authorization at runtime, build a replacement
    /// with [`input_server_config_for_peers`] and install this value with
    /// [`quinn::Endpoint::set_server_config`]. Quinn uses the replacement for
    /// new handshakes. Pass the same replacement to [`super::accept_input`] so
    /// its second check also rejects a revoked peer whose older handshake was
    /// already in flight. Established [`super::InputConnection`] values remain
    /// authenticated until the caller closes them. When the last peer is
    /// revoked, install `None` instead because an empty input-server allowlist
    /// is intentionally invalid.
    pub fn quinn_config(&self) -> quinn::ServerConfig {
        self.quinn.clone()
    }

    pub(super) fn allows_peer(&self, spki: &[u8]) -> bool {
        self.allowed_peer_spkis
            .iter()
            .any(|allowed| allowed.as_ref() == spki)
    }
}

/// A client configuration for a pairing-only RPK possession proof.
///
/// It deliberately carries no peer pin. Connections made with it are exposed
/// only as [`super::PairingConnection`], which has no input channel surface.
#[derive(Clone)]
pub struct PairingClientConfig {
    pub(super) quinn: quinn::ClientConfig,
}

impl fmt::Debug for PairingClientConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PairingClientConfig")
            .finish_non_exhaustive()
    }
}

/// A server configuration for a pairing-only RPK possession proof.
#[derive(Clone)]
pub struct PairingServerConfig {
    pub(super) quinn: quinn::ServerConfig,
}

impl fmt::Debug for PairingServerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PairingServerConfig")
            .finish_non_exhaustive()
    }
}

impl PairingServerConfig {
    /// Clone the pairing-only endpoint configuration for [`quinn::Endpoint::server`].
    pub fn quinn_config(&self) -> quinn::ServerConfig {
        self.quinn.clone()
    }
}

pub fn input_client_config(
    identity: &Identity,
    expected_peer_spki: &[u8],
) -> Result<InputClientConfig, TransportError> {
    let expected_peer_spki = required_pin(expected_peer_spki)?;
    let tls = tls_client_config(
        identity,
        Some(expected_peer_spki.clone()),
        INPUT_ALPN_PROTOCOL,
    )?;
    let crypto = QuicClientConfig::try_from(tls)
        .map_err(|error| TransportError::Configuration(error.to_string()))?;
    let mut quinn = quinn::ClientConfig::new(Arc::new(crypto));
    quinn.transport_config(Arc::new(input_client_transport_config()));
    Ok(InputClientConfig {
        quinn,
        expected_peer_spki,
    })
}

pub fn input_server_config(
    identity: &Identity,
    expected_peer_spki: &[u8],
) -> Result<InputServerConfig, TransportError> {
    input_server_config_for_peers(identity, [expected_peer_spki])
}

/// Build one input listener configuration for every currently allowed peer.
///
/// Authentication happens inside the TLS 1.3 handshake: a client whose raw
/// public key is absent from this immutable snapshot is rejected before any
/// input stream is accepted. Empty allowlists and empty keys are rejected.
/// Install a newly built snapshot with [`quinn::Endpoint::set_server_config`]
/// when the local peer set changes.
pub fn input_server_config_for_peers<I, S>(
    identity: &Identity,
    allowed_peer_spkis: I,
) -> Result<InputServerConfig, TransportError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<[u8]>,
{
    let allowed_peer_spkis = required_allowlist(allowed_peer_spkis)?;
    let tls = tls_server_config(
        identity,
        Some(allowed_peer_spkis.clone()),
        INPUT_ALPN_PROTOCOL,
    )?;
    let crypto = QuicServerConfig::try_from(tls)
        .map_err(|error| TransportError::Configuration(error.to_string()))?;
    let mut quinn = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    quinn.transport_config(Arc::new(input_server_transport_config()));
    Ok(InputServerConfig {
        quinn,
        allowed_peer_spkis,
    })
}

pub fn pairing_client_config(identity: &Identity) -> Result<PairingClientConfig, TransportError> {
    let tls = tls_client_config(identity, None, PAIRING_ALPN_PROTOCOL)?;
    let crypto = QuicClientConfig::try_from(tls)
        .map_err(|error| TransportError::Configuration(error.to_string()))?;
    let mut quinn = quinn::ClientConfig::new(Arc::new(crypto));
    quinn.transport_config(Arc::new(pairing_client_transport_config()));
    Ok(PairingClientConfig { quinn })
}

pub fn pairing_server_config(identity: &Identity) -> Result<PairingServerConfig, TransportError> {
    let tls = tls_server_config(identity, None, PAIRING_ALPN_PROTOCOL)?;
    let crypto = QuicServerConfig::try_from(tls)
        .map_err(|error| TransportError::Configuration(error.to_string()))?;
    let mut quinn = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    quinn.transport_config(Arc::new(pairing_server_transport_config()));
    Ok(PairingServerConfig { quinn })
}

fn required_pin(spki: &[u8]) -> Result<Arc<[u8]>, TransportError> {
    if spki.is_empty() {
        return Err(TransportError::Configuration(
            "peer SPKI pin cannot be empty".into(),
        ));
    }
    Ok(Arc::from(spki))
}

fn required_allowlist<I, S>(spkis: I) -> Result<Arc<[Arc<[u8]>]>, TransportError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<[u8]>,
{
    let mut spkis = spkis
        .into_iter()
        .map(|spki| required_pin(spki.as_ref()))
        .collect::<Result<Vec<_>, _>>()?;
    if spkis.is_empty() {
        return Err(TransportError::Configuration(
            "input server peer allowlist cannot be empty".into(),
        ));
    }
    spkis.sort_unstable_by(|left, right| left.as_ref().cmp(right.as_ref()));
    spkis.dedup_by(|left, right| left.as_ref() == right.as_ref());
    Ok(spkis.into())
}

fn provider() -> CryptoProvider {
    ring::default_provider()
}

fn certified_raw_key(identity: &Identity) -> Result<Arc<CertifiedKey>, TransportError> {
    let provider = provider();
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.private_key_der()));
    let signing_key = provider
        .key_provider
        .load_private_key(private_key)
        .map_err(|error| TransportError::Configuration(error.to_string()))?;
    let loaded_spki = signing_key.public_key().ok_or_else(|| {
        TransportError::Configuration("identity key provider did not expose an SPKI".into())
    })?;
    if loaded_spki.as_ref() != identity.spki() {
        return Err(TransportError::Configuration(
            "identity private key and SPKI do not match".into(),
        ));
    }

    // With RFC 7250 the single Certificate entry contains SPKI DER, not X.509.
    Ok(Arc::new(CertifiedKey::new(
        vec![CertificateDer::from(identity.spki().to_vec())],
        signing_key,
    )))
}

fn tls_client_config(
    identity: &Identity,
    expected_peer_spki: Option<Arc<[u8]>>,
    alpn_protocol: &[u8],
) -> Result<TlsClientConfig, TransportError> {
    let provider = provider();
    let verifier = Arc::new(RpkServerVerifier::new(
        provider.signature_verification_algorithms,
        expected_peer_spki,
    ));
    let resolver = Arc::new(AlwaysResolvesClientRawPublicKeys::new(certified_raw_key(
        identity,
    )?));
    let mut config = TlsClientConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&version::TLS13])
        .map_err(|error| TransportError::Configuration(error.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_cert_resolver(resolver);
    config.alpn_protocols = vec![alpn_protocol.to_vec()];
    config.enable_sni = false;
    config.enable_early_data = false;
    config.resumption = Resumption::disabled();
    Ok(config)
}

fn tls_server_config(
    identity: &Identity,
    allowed_peer_spkis: Option<Arc<[Arc<[u8]>]>>,
    alpn_protocol: &[u8],
) -> Result<TlsServerConfig, TransportError> {
    let provider = provider();
    let verifier = Arc::new(RpkClientVerifier::new(
        provider.signature_verification_algorithms,
        allowed_peer_spkis,
    ));
    let resolver = Arc::new(AlwaysResolvesServerRawPublicKeys::new(certified_raw_key(
        identity,
    )?));
    let mut config = TlsServerConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&version::TLS13])
        .map_err(|error| TransportError::Configuration(error.to_string()))?
        .with_client_cert_verifier(verifier)
        .with_cert_resolver(resolver);
    config.alpn_protocols = vec![alpn_protocol.to_vec()];
    config.max_early_data_size = 0;
    config.send_half_rtt_data = false;
    config.send_tls13_tickets = 0;
    config.session_storage = Arc::new(NoServerSessionStorage {});
    Ok(config)
}

fn input_client_transport_config() -> quinn::TransportConfig {
    let mut config = datagram_transport_config();
    // Only the initiator opens the single critical stream.
    config.max_concurrent_bidi_streams(0_u8.into());
    config
}

fn input_server_transport_config() -> quinn::TransportConfig {
    let mut config = datagram_transport_config();
    config.max_concurrent_bidi_streams(1_u8.into());
    config
}

fn datagram_transport_config() -> quinn::TransportConfig {
    let mut config = quinn::TransportConfig::default();
    config
        .max_concurrent_uni_streams(0_u8.into())
        .datagram_receive_buffer_size(Some(64 * 1_024))
        // A small Quinn queue bounds already-submitted stale data. The
        // application keeps one additional latest-wins slot and counts every
        // replacement before using send_datagram_wait.
        .datagram_send_buffer_size(4 * 1_024)
        .keep_alive_interval(None);
    config
}

fn pairing_client_transport_config() -> quinn::TransportConfig {
    let mut config = pairing_transport_config();
    config.max_concurrent_bidi_streams(0_u8.into());
    config
}

fn pairing_server_transport_config() -> quinn::TransportConfig {
    let mut config = pairing_transport_config();
    config.max_concurrent_bidi_streams(1_u8.into());
    config
}

fn pairing_transport_config() -> quinn::TransportConfig {
    let mut config = quinn::TransportConfig::default();
    // The initiator opens one type-safe metadata stream. Pairing still has no
    // input channels, datagrams, unidirectional streams, or bulk surface.
    config
        .max_concurrent_uni_streams(0_u8.into())
        .datagram_receive_buffer_size(None)
        .datagram_send_buffer_size(0)
        .keep_alive_interval(None);
    config
}

struct RpkServerVerifier {
    algorithms: WebPkiSupportedAlgorithms,
    expected: Option<Arc<[u8]>>,
}

impl RpkServerVerifier {
    fn new(algorithms: WebPkiSupportedAlgorithms, expected: Option<Arc<[u8]>>) -> Self {
        Self {
            algorithms,
            expected,
        }
    }
}

impl fmt::Debug for RpkServerVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpkServerVerifier")
            .field(
                "mode",
                &if self.expected.is_some() {
                    "pinned"
                } else {
                    "pairing"
                },
            )
            .finish()
    }
}

impl ServerCertVerifier for RpkServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        verify_presented_spki(end_entity, intermediates, self.expected.as_deref())?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::General("TLS 1.2 is disabled".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        spki: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_rpk_signature(message, spki, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

struct RpkClientVerifier {
    algorithms: WebPkiSupportedAlgorithms,
    allowed: Option<Arc<[Arc<[u8]>]>>,
    roots: Vec<DistinguishedName>,
}

impl RpkClientVerifier {
    fn new(algorithms: WebPkiSupportedAlgorithms, allowed: Option<Arc<[Arc<[u8]>]>>) -> Self {
        Self {
            algorithms,
            allowed,
            roots: Vec::new(),
        }
    }
}

impl fmt::Debug for RpkClientVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpkClientVerifier")
            .field(
                "mode",
                &if self.allowed.is_some() {
                    "allowlist"
                } else {
                    "pairing"
                },
            )
            .finish()
    }
}

impl ClientCertVerifier for RpkClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.roots
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        verify_presented_spki(end_entity, intermediates, None)?;
        if self.allowed.as_ref().is_some_and(|allowed| {
            !allowed
                .iter()
                .any(|expected| expected.as_ref() == end_entity.as_ref())
        }) {
            return Err(TlsError::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ));
        }
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::General("TLS 1.2 is disabled".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        spki: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_rpk_signature(message, spki, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

fn verify_presented_spki(
    presented: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
    expected: Option<&[u8]>,
) -> Result<(), TlsError> {
    if !intermediates.is_empty() || presented.is_empty() {
        return Err(TlsError::InvalidCertificate(CertificateError::BadEncoding));
    }
    if let Some(expected) = expected
        && presented.as_ref() != expected
    {
        return Err(TlsError::InvalidCertificate(
            CertificateError::ApplicationVerificationFailure,
        ));
    }
    Ok(())
}

fn verify_rpk_signature(
    message: &[u8],
    spki: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
    algorithms: &WebPkiSupportedAlgorithms,
) -> Result<HandshakeSignatureValid, TlsError> {
    let spki = SubjectPublicKeyInfoDer::from(spki.as_ref());
    verify_tls13_signature_with_raw_key(message, &spki, dss, algorithms)
}
