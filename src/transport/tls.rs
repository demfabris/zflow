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

pub const INPUT_ALPN_PROTOCOL: &[u8] = b"zflow/5";
/// Answered on the input port by computers that do not trust each other yet.
pub const HELLO_ALPN_PROTOCOL: &[u8] = b"zflow-hello/5";
const INPUT_KEEP_ALIVE: std::time::Duration = std::time::Duration::from_secs(5);
const INPUT_IDLE_TIMEOUT_MS: u32 = 15_000;
/// A hello is two short messages, so a silent peer is dropped quickly.
const HELLO_IDLE_TIMEOUT_MS: u32 = 5_000;

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

/// The one listener on the input port. It answers hellos from any key and
/// input connections only from keys on a fixed allowlist.
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
    /// [`quinn::Endpoint::set_server_config`]. Pass the same replacement to
    /// [`super::accept`], which checks the allowlist once the handshake is
    /// done, so a peer revoked while its handshake was in flight is refused
    /// too. Established [`super::InputConnection`] values remain
    /// authenticated until the caller closes them.
    pub fn quinn_config(&self) -> quinn::ServerConfig {
        self.quinn.clone()
    }

    pub(super) fn allows_peer(&self, spki: &[u8]) -> bool {
        self.allowed_peer_spkis
            .iter()
            .any(|allowed| allowed.as_ref() == spki)
    }
}

/// A client configuration for saying hello to a computer whose key is not
/// known yet. It carries no pin: connections made with it are exposed only
/// as [`super::HelloConnection`], which has no input surface.
#[derive(Clone)]
pub struct HelloClientConfig {
    pub(super) quinn: quinn::ClientConfig,
}

impl fmt::Debug for HelloClientConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HelloClientConfig")
            .finish_non_exhaustive()
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
/// The TLS 1.3 handshake proves the client holds the key it presents, and
/// [`super::accept`] then refuses input from a key absent from this
/// immutable snapshot before any input stream is accepted. An empty
/// allowlist still answers hellos. Empty keys are rejected. Install a newly
/// built snapshot with [`quinn::Endpoint::set_server_config`] when the local
/// peer set changes.
pub fn input_server_config_for_peers<I, S>(
    identity: &Identity,
    allowed_peer_spkis: I,
) -> Result<InputServerConfig, TransportError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<[u8]>,
{
    let allowed_peer_spkis = required_allowlist(allowed_peer_spkis)?;
    let tls = tls_server_config(identity, &[INPUT_ALPN_PROTOCOL, HELLO_ALPN_PROTOCOL])?;
    let crypto = QuicServerConfig::try_from(tls)
        .map_err(|error| TransportError::Configuration(error.to_string()))?;
    let mut quinn = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    quinn.transport_config(Arc::new(input_server_transport_config()));
    Ok(InputServerConfig {
        quinn,
        allowed_peer_spkis,
    })
}

pub fn hello_client_config(identity: &Identity) -> Result<HelloClientConfig, TransportError> {
    let tls = tls_client_config(identity, None, HELLO_ALPN_PROTOCOL)?;
    let crypto = QuicClientConfig::try_from(tls)
        .map_err(|error| TransportError::Configuration(error.to_string()))?;
    let mut quinn = quinn::ClientConfig::new(Arc::new(crypto));
    quinn.transport_config(Arc::new(hello_client_transport_config()));
    Ok(HelloClientConfig { quinn })
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

/// The handshake only proves the client holds the key it presents. Which
/// keys may do what is decided after it, per protocol.
fn tls_server_config(
    identity: &Identity,
    alpn_protocols: &[&[u8]],
) -> Result<TlsServerConfig, TransportError> {
    let provider = provider();
    let verifier = Arc::new(RpkClientVerifier::new(
        provider.signature_verification_algorithms,
    ));
    let resolver = Arc::new(AlwaysResolvesServerRawPublicKeys::new(certified_raw_key(
        identity,
    )?));
    let mut config = TlsServerConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&version::TLS13])
        .map_err(|error| TransportError::Configuration(error.to_string()))?
        .with_client_cert_verifier(verifier)
        .with_cert_resolver(resolver);
    config.alpn_protocols = alpn_protocols
        .iter()
        .map(|protocol| protocol.to_vec())
        .collect();
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
        // Each clipboard transfer is one stream, and one at a time.
        .max_concurrent_uni_streams(1_u8.into())
        .datagram_receive_buffer_size(Some(64 * 1_024))
        // A small Quinn queue bounds already-submitted stale data. The
        // application keeps one additional latest-wins slot and counts every
        // replacement before using send_datagram_wait.
        .datagram_send_buffer_size(4 * 1_024)
        // Sessions stay open between crossings. A ping when nothing else was
        // sent keeps an idle session alive, and a dead path ends it within
        // the idle timeout so the source reconnects.
        .keep_alive_interval(Some(INPUT_KEEP_ALIVE))
        .max_idle_timeout(Some(quinn::VarInt::from_u32(INPUT_IDLE_TIMEOUT_MS).into()));
    config
}

fn hello_client_transport_config() -> quinn::TransportConfig {
    let mut config = quinn::TransportConfig::default();
    // The initiator opens the one hello stream and accepts nothing.
    config
        .max_concurrent_bidi_streams(0_u8.into())
        .max_concurrent_uni_streams(0_u8.into())
        .datagram_receive_buffer_size(None)
        .datagram_send_buffer_size(0)
        .max_idle_timeout(Some(quinn::VarInt::from_u32(HELLO_IDLE_TIMEOUT_MS).into()));
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
                    "hello"
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
    roots: Vec<DistinguishedName>,
}

impl RpkClientVerifier {
    fn new(algorithms: WebPkiSupportedAlgorithms) -> Self {
        Self {
            algorithms,
            roots: Vec::new(),
        }
    }
}

impl fmt::Debug for RpkClientVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpkClientVerifier")
            .finish_non_exhaustive()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Each version's protocols: input, then the one computers use before
    /// they trust each other, which was code pairing until 0.3.0. Computers
    /// on different protocols cannot connect, so a protocol change needs a
    /// new version, or the installer hands out one that cannot talk to a
    /// build from main.
    const VERSIONS: &[(&str, &[u8], &[u8])] = &[
        ("0.1.0", b"zflow/1", b"zflow-pair/1"),
        ("0.2.0", b"zflow/3", b"zflow-pair/4"),
        ("0.3.0", b"zflow/4", b"zflow-hello/4"),
        ("0.4.0", b"zflow/5", b"zflow-hello/5"),
    ];

    #[test]
    fn a_protocol_change_comes_with_a_new_version() {
        let version = env!("CARGO_PKG_VERSION");
        let Some(&(_, input, first_contact)) =
            VERSIONS.iter().find(|(known, ..)| *known == version)
        else {
            panic!("add version {version} and its protocols to VERSIONS");
        };
        assert!(
            input == INPUT_ALPN_PROTOCOL && first_contact == HELLO_ALPN_PROTOCOL,
            "version {version} already shipped other protocols; raise the version in Cargo.toml"
        );
    }
}
