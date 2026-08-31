//! Privacy-preserving local discovery.
//!
//! mDNS records are hints for locating a QUIC endpoint. They are never peer
//! identities and must not grant authority before the transport authenticates
//! the peer.

use std::{
    collections::BTreeSet,
    fmt,
    fs::File,
    io::Read,
    net::{IpAddr, SocketAddr, SocketAddrV6},
};

use mdns_sd::{
    DaemonEvent, DaemonStatus, Receiver, ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent,
    ServiceInfo, TxtProperties, UnregisterStatus,
};
use thiserror::Error;

use crate::{
    core::{InputCapabilities, InputCapability, ProtocolVersion},
    wire::CURRENT_PROTOCOL_VERSION,
};

pub const SERVICE_TYPE: &str = "_zflow._udp.local.";
pub const MAX_DISCOVERY_CANDIDATES: usize = 16;
pub const MAX_PROTOCOL_VERSIONS: usize = 16;
pub const MAX_TXT_PROPERTIES: usize = 3;
pub const MAX_TXT_BYTES: usize = 384;

const INSTANCE_PREFIX: &str = "zf-";
const INSTANCE_ENTROPY_BYTES: usize = 16;
const INSTANCE_HEX_BYTES: usize = INSTANCE_ENTROPY_BYTES * 2;
const TXT_PROTOCOL_VERSIONS: &str = "v";
const TXT_CAPABILITIES: &str = "cap";
const TXT_ROTATING_TOKEN: &str = "token";

/// Enumerates usable local addresses for the process-lifetime mDNS record.
/// Loopback and non-unicast addresses never help another machine connect.
pub fn local_unicast_addresses() -> Result<Vec<IpAddr>, DiscoveryError> {
    let addresses = if_addrs::get_if_addrs()
        .map_err(DiscoveryError::Interfaces)?
        .into_iter()
        .map(|interface| interface.ip());
    Ok(select_advertisable_addresses(addresses))
}

fn select_advertisable_addresses(addresses: impl IntoIterator<Item = IpAddr>) -> Vec<IpAddr> {
    addresses
        .into_iter()
        .filter(|address| !address.is_loopback() && validate_ip_address(*address).is_ok())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_DISCOVERY_CANDIDATES)
        .collect()
}

/// A per-process random name used only for multicast discovery.
///
/// It deliberately contains no certificate, machine, account, or configured
/// peer identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EphemeralInstanceId([u8; INSTANCE_ENTROPY_BYTES]);

impl EphemeralInstanceId {
    fn generate() -> Result<Self, DiscoveryError> {
        #[cfg(unix)]
        {
            let mut bytes = [0; INSTANCE_ENTROPY_BYTES];
            File::open("/dev/urandom")
                .and_then(|mut random| random.read_exact(&mut bytes))
                .map_err(DiscoveryError::Entropy)?;
            Ok(Self(bytes))
        }

        #[cfg(not(unix))]
        {
            Err(DiscoveryError::UnsupportedPlatform)
        }
    }

    fn parse(value: &str) -> Result<Self, CandidateParseError> {
        let Some(hex) = value.strip_prefix(INSTANCE_PREFIX) else {
            return Err(CandidateParseError::InvalidInstanceId);
        };
        if hex.len() != INSTANCE_HEX_BYTES {
            return Err(CandidateParseError::InvalidInstanceId);
        }

        let mut bytes = [0; INSTANCE_ENTROPY_BYTES];
        for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = decode_hex_pair(pair).ok_or(CandidateParseError::InvalidInstanceId)?;
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; INSTANCE_ENTROPY_BYTES] {
        &self.0
    }

    fn hostname(self) -> String {
        format!("{self}.local.")
    }

    fn fullname(self) -> String {
        format!("{self}.{SERVICE_TYPE}")
    }
}

impl fmt::Display for EphemeralInstanceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(INSTANCE_PREFIX)?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// The small, fixed-schema record zflow publishes over multicast DNS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertisement {
    quic_port: u16,
    addresses: Vec<IpAddr>,
    capability_summary: InputCapabilities,
    rotating_token: Option<[u8; 16]>,
}

impl Advertisement {
    pub fn new(
        quic_port: u16,
        addresses: impl IntoIterator<Item = IpAddr>,
        capabilities: impl IntoIterator<Item = InputCapability>,
    ) -> Result<Self, DiscoveryError> {
        if quic_port == 0 {
            return Err(DiscoveryError::InvalidAdvertisement(
                "QUIC port must be non-zero",
            ));
        }

        let mut unique_addresses = BTreeSet::new();
        for address in addresses {
            validate_ip_address(address).map_err(DiscoveryError::InvalidAdvertisement)?;
            unique_addresses.insert(address);
            if unique_addresses.len() > MAX_DISCOVERY_CANDIDATES {
                return Err(DiscoveryError::InvalidAdvertisement(
                    "too many connection candidates",
                ));
            }
        }
        if unique_addresses.is_empty() {
            return Err(DiscoveryError::InvalidAdvertisement(
                "at least one connection candidate is required",
            ));
        }

        let capability_summary = InputCapabilities::new(capabilities);
        if capability_summary.iter().next().is_none() {
            return Err(DiscoveryError::InvalidAdvertisement(
                "at least one input capability is required",
            ));
        }

        Ok(Self {
            quic_port,
            addresses: unique_addresses.into_iter().collect(),
            capability_summary,
            rotating_token: None,
        })
    }

    pub fn with_rotating_token(mut self, token: [u8; 16]) -> Self {
        self.rotating_token = Some(token);
        self
    }

    pub fn quic_port(&self) -> u16 {
        self.quic_port
    }

    pub fn addresses(&self) -> &[IpAddr] {
        &self.addresses
    }

    pub fn capability_summary(&self) -> &InputCapabilities {
        &self.capability_summary
    }

    pub fn rotating_token(&self) -> Option<[u8; 16]> {
        self.rotating_token
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateSource {
    Mdns,
    Explicit,
}

/// A bounded endpoint hint which has not been authenticated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrustedCandidate {
    source: CandidateSource,
    ephemeral_instance_id: Option<EphemeralInstanceId>,
    protocol_versions: Vec<ProtocolVersion>,
    capability_summary: InputCapabilities,
    socket_addresses: Vec<SocketAddr>,
    rotating_token: Option<[u8; 16]>,
}

impl UntrustedCandidate {
    /// Creates a candidate from a user-provided address without starting mDNS.
    pub fn explicit(address: SocketAddr) -> Result<Self, CandidateParseError> {
        validate_socket_address(address).map_err(CandidateParseError::InvalidAddress)?;
        Ok(Self {
            source: CandidateSource::Explicit,
            ephemeral_instance_id: None,
            protocol_versions: Vec::new(),
            capability_summary: InputCapabilities::default(),
            socket_addresses: vec![address],
            rotating_token: None,
        })
    }

    pub fn source(&self) -> CandidateSource {
        self.source
    }

    pub fn ephemeral_instance_id(&self) -> Option<EphemeralInstanceId> {
        self.ephemeral_instance_id
    }

    pub fn protocol_versions(&self) -> &[ProtocolVersion] {
        &self.protocol_versions
    }

    pub fn capability_summary(&self) -> &InputCapabilities {
        &self.capability_summary
    }

    pub fn socket_addresses(&self) -> &[SocketAddr] {
        &self.socket_addresses
    }

    pub fn rotating_token(&self) -> Option<[u8; 16]> {
        self.rotating_token
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryEvent {
    Candidate(UntrustedCandidate),
    Removed(EphemeralInstanceId),
    Stopped,
}

/// Owns one mDNS daemon, one zflow registration, and one zflow browser.
pub struct Discovery {
    daemon: ServiceDaemon,
    monitor: Receiver<DaemonEvent>,
    instance_id: EphemeralInstanceId,
    registration: Option<Advertisement>,
    browser: Option<Receiver<ServiceEvent>>,
    shutdown: bool,
}

impl Discovery {
    pub fn new() -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new()?;
        let monitor = daemon.monitor()?;
        Ok(Self {
            daemon,
            monitor,
            instance_id: EphemeralInstanceId::generate()?,
            registration: None,
            browser: None,
            shutdown: false,
        })
    }

    pub fn instance_id(&self) -> EphemeralInstanceId {
        self.instance_id
    }

    pub fn register(&mut self, advertisement: Advertisement) -> Result<(), DiscoveryError> {
        if self.registration.is_some() {
            return Err(DiscoveryError::InvalidState(
                "a zflow service is already registered",
            ));
        }
        let service = build_service_info(self.instance_id, &advertisement)?;
        self.daemon.register(service)?;
        self.registration = Some(advertisement);
        Ok(())
    }

    /// Re-announces the same ephemeral service with a new or removed token.
    pub fn rotate_discovery_token(
        &mut self,
        token: Option<[u8; 16]>,
    ) -> Result<(), DiscoveryError> {
        let mut updated = self
            .registration
            .clone()
            .ok_or(DiscoveryError::InvalidState(
                "no zflow service is registered",
            ))?;
        updated.rotating_token = token;
        let service = build_service_info(self.instance_id, &updated)?;
        self.daemon.register(service)?;
        self.registration = Some(updated);
        Ok(())
    }

    pub fn browse(&mut self) -> Result<(), DiscoveryError> {
        if self.browser.is_some() {
            return Err(DiscoveryError::InvalidState(
                "zflow browsing is already active",
            ));
        }
        self.browser = Some(self.daemon.browse(SERVICE_TYPE)?);
        Ok(())
    }

    /// Receives the next valid bounded event. Malformed multicast records are
    /// ignored rather than allowed to terminate discovery.
    pub async fn next_event(&self) -> Result<DiscoveryEvent, DiscoveryError> {
        let receiver = self
            .browser
            .as_ref()
            .ok_or(DiscoveryError::InvalidState("zflow browsing is not active"))?;

        loop {
            let event = receiver
                .recv_async()
                .await
                .map_err(|_| DiscoveryError::ChannelClosed("browser"))?;
            match event {
                ServiceEvent::ServiceResolved(service) => {
                    if let Ok(candidate) = parse_resolved_service(&service) {
                        return Ok(DiscoveryEvent::Candidate(candidate));
                    }
                }
                ServiceEvent::ServiceRemoved(service_type, fullname)
                    if service_type.eq_ignore_ascii_case(SERVICE_TYPE) =>
                {
                    if let Ok(instance_id) = instance_from_fullname(&fullname) {
                        return Ok(DiscoveryEvent::Removed(instance_id));
                    }
                }
                ServiceEvent::SearchStopped(service_type)
                    if service_type.eq_ignore_ascii_case(SERVICE_TYPE) =>
                {
                    return Ok(DiscoveryEvent::Stopped);
                }
                _ => {}
            }
        }
    }

    /// Waits for the next error reported by the mdns-sd daemon thread.
    ///
    /// Daemon initialization errors such as multicast socket failures are
    /// asynchronous in mdns-sd and surface here.
    pub async fn next_daemon_error(&self) -> Result<mdns_sd::Error, DiscoveryError> {
        loop {
            let event = self
                .monitor
                .recv_async()
                .await
                .map_err(|_| DiscoveryError::ChannelClosed("daemon monitor"))?;
            if let DaemonEvent::Error(error) = event {
                return Ok(error);
            }
        }
    }

    pub fn stop_browse(&mut self) -> Result<(), DiscoveryError> {
        if self.browser.take().is_some() {
            self.daemon.stop_browse(SERVICE_TYPE)?;
        }
        Ok(())
    }

    pub async fn unregister(&mut self) -> Result<(), DiscoveryError> {
        if self.registration.is_none() {
            return Ok(());
        }
        let status = self
            .daemon
            .unregister(&self.instance_id.fullname())?
            .recv_async()
            .await
            .map_err(|_| DiscoveryError::ChannelClosed("unregister"))?;
        match status {
            UnregisterStatus::OK | UnregisterStatus::NotFound => {
                self.registration = None;
                Ok(())
            }
        }
    }

    pub async fn shutdown(mut self) -> Result<(), DiscoveryError> {
        self.stop_browse()?;
        self.unregister().await?;
        let status = self
            .daemon
            .shutdown()?
            .recv_async()
            .await
            .map_err(|_| DiscoveryError::ChannelClosed("shutdown"))?;
        if status != DaemonStatus::Shutdown {
            return Err(DiscoveryError::InvalidState(
                "mDNS daemon did not report shutdown",
            ));
        }
        self.shutdown = true;
        Ok(())
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        if self.browser.take().is_some() {
            let _ = self.daemon.stop_browse(SERVICE_TYPE);
        }
        if self.registration.take().is_some() {
            let _ = self.daemon.unregister(&self.instance_id.fullname());
        }
        if !self.shutdown {
            let _ = self.daemon.shutdown();
        }
    }
}

/// Strictly validates one resolved DNS-SD record into an untrusted hint.
pub fn parse_resolved_service(
    service: &ResolvedService,
) -> Result<UntrustedCandidate, CandidateParseError> {
    if !service.ty_domain.eq_ignore_ascii_case(SERVICE_TYPE) {
        return Err(CandidateParseError::WrongServiceType);
    }
    if service.port == 0 {
        return Err(CandidateParseError::InvalidAddress(
            "QUIC port must be non-zero",
        ));
    }
    if service.addresses.is_empty() {
        return Err(CandidateParseError::InvalidAddress(
            "at least one address is required",
        ));
    }
    if service.addresses.len() > MAX_DISCOVERY_CANDIDATES {
        return Err(CandidateParseError::TooManyCandidates);
    }

    let mut addresses = Vec::with_capacity(service.addresses.len());
    for scoped in &service.addresses {
        let address = match scoped {
            ScopedIp::V4(ipv4) => SocketAddr::new(IpAddr::V4(*ipv4.addr()), service.port),
            ScopedIp::V6(ipv6) => SocketAddr::V6(SocketAddrV6::new(
                *ipv6.addr(),
                service.port,
                0,
                ipv6.scope_id().index,
            )),
            _ => {
                return Err(CandidateParseError::InvalidAddress(
                    "unsupported address family",
                ));
            }
        };
        validate_socket_address(address).map_err(CandidateParseError::InvalidAddress)?;
        addresses.push(address);
    }
    addresses.sort_unstable();
    addresses.dedup();

    parse_candidate_fields(
        &service.fullname,
        &service.host,
        addresses,
        &service.txt_properties,
    )
}

fn parse_candidate_fields(
    fullname: &str,
    hostname: &str,
    socket_addresses: Vec<SocketAddr>,
    properties: &TxtProperties,
) -> Result<UntrustedCandidate, CandidateParseError> {
    if socket_addresses.is_empty() {
        return Err(CandidateParseError::InvalidAddress(
            "at least one address is required",
        ));
    }
    if socket_addresses.len() > MAX_DISCOVERY_CANDIDATES {
        return Err(CandidateParseError::TooManyCandidates);
    }
    for address in &socket_addresses {
        validate_socket_address(*address).map_err(CandidateParseError::InvalidAddress)?;
    }

    let instance_id = instance_from_fullname(fullname)?;
    if !hostname.eq_ignore_ascii_case(&instance_id.hostname()) {
        return Err(CandidateParseError::InvalidHostname);
    }
    let txt = parse_txt(properties)?;

    Ok(UntrustedCandidate {
        source: CandidateSource::Mdns,
        ephemeral_instance_id: Some(instance_id),
        protocol_versions: txt.protocol_versions,
        capability_summary: txt.capability_summary,
        socket_addresses,
        rotating_token: txt.rotating_token,
    })
}

fn build_service_info(
    instance_id: EphemeralInstanceId,
    advertisement: &Advertisement,
) -> Result<ServiceInfo, DiscoveryError> {
    let versions = CURRENT_PROTOCOL_VERSION.0.to_string();
    let capabilities = format_capabilities(&advertisement.capability_summary);
    let mut properties = vec![
        (TXT_PROTOCOL_VERSIONS.to_owned(), versions),
        (TXT_CAPABILITIES.to_owned(), capabilities),
    ];
    if let Some(token) = advertisement.rotating_token {
        properties.push((TXT_ROTATING_TOKEN.to_owned(), encode_hex(&token)));
    }
    debug_assert!(txt_size(&properties) <= MAX_TXT_BYTES);

    ServiceInfo::new(
        SERVICE_TYPE,
        &instance_id.to_string(),
        &instance_id.hostname(),
        advertisement.addresses.as_slice(),
        advertisement.quic_port,
        properties.as_slice(),
    )
    .map_err(DiscoveryError::Mdns)
}

struct ParsedTxt {
    protocol_versions: Vec<ProtocolVersion>,
    capability_summary: InputCapabilities,
    rotating_token: Option<[u8; 16]>,
}

fn parse_txt(properties: &TxtProperties) -> Result<ParsedTxt, CandidateParseError> {
    if properties.len() > MAX_TXT_PROPERTIES {
        return Err(CandidateParseError::TooManyTxtProperties);
    }

    let mut total_size = 0usize;
    let mut versions = None;
    let mut capabilities = None;
    let mut token = None;

    for property in properties.iter() {
        let value = property.val().ok_or(CandidateParseError::InvalidTxtValue)?;
        total_size = total_size
            .checked_add(1 + property.key().len() + 1 + value.len())
            .ok_or(CandidateParseError::TxtTooLarge)?;
        if total_size > MAX_TXT_BYTES {
            return Err(CandidateParseError::TxtTooLarge);
        }
        let value = std::str::from_utf8(value).map_err(|_| CandidateParseError::InvalidTxtValue)?;

        match property.key() {
            TXT_PROTOCOL_VERSIONS if versions.is_none() => {
                versions = Some(parse_protocol_versions(value)?);
            }
            TXT_CAPABILITIES if capabilities.is_none() => {
                capabilities = Some(parse_capabilities(value)?);
            }
            TXT_ROTATING_TOKEN if token.is_none() => {
                token = Some(parse_token(value)?);
            }
            TXT_PROTOCOL_VERSIONS | TXT_CAPABILITIES | TXT_ROTATING_TOKEN => {
                return Err(CandidateParseError::DuplicateTxtProperty);
            }
            _ => return Err(CandidateParseError::UnexpectedTxtProperty),
        }
    }

    Ok(ParsedTxt {
        protocol_versions: versions.ok_or(CandidateParseError::MissingTxtProperty(
            TXT_PROTOCOL_VERSIONS,
        ))?,
        capability_summary: capabilities
            .ok_or(CandidateParseError::MissingTxtProperty(TXT_CAPABILITIES))?,
        rotating_token: token,
    })
}

fn parse_protocol_versions(value: &str) -> Result<Vec<ProtocolVersion>, CandidateParseError> {
    if value.is_empty() || value.len() > MAX_PROTOCOL_VERSIONS * 6 {
        return Err(CandidateParseError::InvalidProtocolVersions);
    }
    let mut versions = BTreeSet::new();
    for raw in value.split(',') {
        if raw.is_empty() || (raw.len() > 1 && raw.starts_with('0')) {
            return Err(CandidateParseError::InvalidProtocolVersions);
        }
        let version = raw
            .parse::<u16>()
            .map_err(|_| CandidateParseError::InvalidProtocolVersions)?;
        if version == 0 || !versions.insert(ProtocolVersion(version)) {
            return Err(CandidateParseError::InvalidProtocolVersions);
        }
        if versions.len() > MAX_PROTOCOL_VERSIONS {
            return Err(CandidateParseError::InvalidProtocolVersions);
        }
    }
    Ok(versions.into_iter().collect())
}

fn format_capabilities(capabilities: &InputCapabilities) -> String {
    capabilities
        .iter()
        .map(capability_name)
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_capabilities(value: &str) -> Result<InputCapabilities, CandidateParseError> {
    if value.is_empty() || value.len() > 96 {
        return Err(CandidateParseError::InvalidCapabilities);
    }
    let mut capabilities = BTreeSet::new();
    for name in value.split(',') {
        let capability =
            capability_from_name(name).ok_or(CandidateParseError::InvalidCapabilities)?;
        if !capabilities.insert(capability) {
            return Err(CandidateParseError::InvalidCapabilities);
        }
    }
    Ok(InputCapabilities::new(capabilities))
}

fn capability_name(capability: InputCapability) -> &'static str {
    match capability {
        InputCapability::Keyboard => "keyboard",
        InputCapability::ConsumerControls => "consumer",
        InputCapability::Pointer => "pointer",
        InputCapability::Scroll => "scroll",
        InputCapability::Touch => "touch",
    }
}

fn capability_from_name(name: &str) -> Option<InputCapability> {
    match name {
        "keyboard" => Some(InputCapability::Keyboard),
        "consumer" => Some(InputCapability::ConsumerControls),
        "pointer" => Some(InputCapability::Pointer),
        "scroll" => Some(InputCapability::Scroll),
        "touch" => Some(InputCapability::Touch),
        _ => None,
    }
}

fn parse_token(value: &str) -> Result<[u8; 16], CandidateParseError> {
    if value.len() != 32 {
        return Err(CandidateParseError::InvalidToken);
    }
    let mut token = [0; 16];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        token[index] = decode_hex_pair(pair).ok_or(CandidateParseError::InvalidToken)?;
    }
    Ok(token)
}

fn instance_from_fullname(fullname: &str) -> Result<EphemeralInstanceId, CandidateParseError> {
    if fullname.len() > 255 {
        return Err(CandidateParseError::InvalidInstanceId);
    }
    let suffix = format!(".{SERVICE_TYPE}");
    let Some(instance) = fullname.strip_suffix(&suffix) else {
        return Err(CandidateParseError::WrongServiceType);
    };
    EphemeralInstanceId::parse(instance)
}

fn validate_ip_address(address: IpAddr) -> Result<(), &'static str> {
    if address.is_unspecified() {
        Err("unspecified addresses are not connection candidates")
    } else if address.is_multicast() {
        Err("multicast addresses are not connection candidates")
    } else if matches!(address, IpAddr::V4(address) if address.is_broadcast()) {
        Err("broadcast addresses are not connection candidates")
    } else {
        Ok(())
    }
}

fn validate_socket_address(address: SocketAddr) -> Result<(), &'static str> {
    if address.port() == 0 {
        return Err("QUIC port must be non-zero");
    }
    if matches!(address, SocketAddr::V6(address) if address.ip().is_unicast_link_local() && address.scope_id() == 0)
    {
        return Err("link-local IPv6 candidates require an interface scope");
    }
    validate_ip_address(address.ip())
}

fn txt_size(properties: &[(String, String)]) -> usize {
    properties
        .iter()
        .map(|(key, value)| 1 + key.len() + 1 + value.len())
        .sum()
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn decode_hex_pair(pair: &[u8]) -> Option<u8> {
    if pair.len() != 2 {
        return None;
    }
    Some(hex_digit(pair[0])? * 16 + hex_digit(pair[1])?)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("mDNS operation failed: {0}")]
    Mdns(#[from] mdns_sd::Error),
    #[error("failed to obtain entropy for an ephemeral discovery identifier: {0}")]
    Entropy(std::io::Error),
    #[error("failed to enumerate local network interfaces: {0}")]
    Interfaces(std::io::Error),
    #[cfg(not(unix))]
    #[error("secure ephemeral discovery identifiers are unsupported on this platform")]
    UnsupportedPlatform,
    #[error("invalid discovery advertisement: {0}")]
    InvalidAdvertisement(&'static str),
    #[error("invalid discovery lifecycle state: {0}")]
    InvalidState(&'static str),
    #[error("mDNS {0} channel closed")]
    ChannelClosed(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CandidateParseError {
    #[error("record is not a zflow service")]
    WrongServiceType,
    #[error("ephemeral instance identifier is malformed")]
    InvalidInstanceId,
    #[error("ephemeral hostname does not match the instance identifier")]
    InvalidHostname,
    #[error("too many connection candidates")]
    TooManyCandidates,
    #[error("invalid connection candidate: {0}")]
    InvalidAddress(&'static str),
    #[error("too many TXT properties")]
    TooManyTxtProperties,
    #[error("TXT record exceeds the zflow discovery bound")]
    TxtTooLarge,
    #[error("TXT property has an invalid value")]
    InvalidTxtValue,
    #[error("unexpected TXT property")]
    UnexpectedTxtProperty,
    #[error("duplicate TXT property")]
    DuplicateTxtProperty,
    #[error("missing TXT property {0}")]
    MissingTxtProperty(&'static str),
    #[error("protocol version summary is malformed")]
    InvalidProtocolVersions,
    #[error("capability summary is malformed")]
    InvalidCapabilities,
    #[error("rotating discovery token is malformed")]
    InvalidToken,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn test_instance() -> EphemeralInstanceId {
        EphemeralInstanceId([0xab; 16])
    }

    fn advertisement() -> Advertisement {
        Advertisement::new(
            43_119,
            [IpAddr::V4(Ipv4Addr::LOCALHOST)],
            [
                InputCapability::Keyboard,
                InputCapability::Pointer,
                InputCapability::Scroll,
            ],
        )
        .unwrap()
        .with_rotating_token([0xcd; 16])
    }

    #[test]
    fn advertised_names_and_txt_are_ephemeral_and_fixed_schema() {
        let service = build_service_info(test_instance(), &advertisement()).unwrap();
        assert_eq!(
            service.get_fullname(),
            "zf-abababababababababababababababab._zflow._udp.local."
        );
        assert_eq!(
            service.get_hostname(),
            "zf-abababababababababababababababab.local."
        );

        let properties = service.get_properties();
        assert!(properties.len() <= MAX_TXT_PROPERTIES);
        assert_eq!(properties.get_property_val_str("v"), Some("1"));
        assert_eq!(
            properties.get_property_val_str("cap"),
            Some("keyboard,pointer,scroll")
        );
        assert_eq!(
            properties.get_property_val_str("token"),
            Some("cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd")
        );

        let rendered = format!("{service:?}").to_ascii_lowercase();
        for forbidden in [
            "spki",
            "fingerprint",
            "certificate",
            "config_path",
            "/home/",
            "/var/lib/",
            "username",
            "account",
        ] {
            assert!(!rendered.contains(forbidden), "leaked marker {forbidden}");
        }
    }

    #[test]
    fn advertisement_enforces_candidate_bounds() {
        let too_many = (1..=MAX_DISCOVERY_CANDIDATES + 1)
            .map(|last| IpAddr::V4(Ipv4Addr::new(192, 0, 2, last as u8)));
        assert!(matches!(
            Advertisement::new(43_119, too_many, [InputCapability::Keyboard]),
            Err(DiscoveryError::InvalidAdvertisement(
                "too many connection candidates"
            ))
        ));
        assert!(
            Advertisement::new(
                0,
                [IpAddr::V4(Ipv4Addr::LOCALHOST)],
                [InputCapability::Keyboard]
            )
            .is_err()
        );
        assert!(
            Advertisement::new(
                43_119,
                [IpAddr::V4(Ipv4Addr::UNSPECIFIED)],
                [InputCapability::Keyboard]
            )
            .is_err()
        );
    }

    #[test]
    fn deterministic_record_parsing_preserves_only_bounded_hints() {
        let service = build_service_info(test_instance(), &advertisement()).unwrap();
        let addresses = vec![
            SocketAddr::from((Ipv4Addr::LOCALHOST, 43_119)),
            SocketAddr::from((Ipv6Addr::LOCALHOST, 43_119)),
        ];
        let parsed = parse_candidate_fields(
            service.get_fullname(),
            service.get_hostname(),
            addresses.clone(),
            service.get_properties(),
        )
        .unwrap();

        assert_eq!(parsed.source(), CandidateSource::Mdns);
        assert_eq!(parsed.ephemeral_instance_id(), Some(test_instance()));
        assert_eq!(parsed.protocol_versions(), &[CURRENT_PROTOCOL_VERSION]);
        assert_eq!(parsed.socket_addresses(), addresses);
        assert!(
            parsed
                .capability_summary()
                .contains(InputCapability::Keyboard)
        );
        assert_eq!(parsed.rotating_token(), Some([0xcd; 16]));
    }

    #[test]
    fn parser_rejects_identity_bearing_and_oversized_txt() {
        let id = test_instance();
        let forbidden = ServiceInfo::new(
            SERVICE_TYPE,
            &id.to_string(),
            &id.hostname(),
            [IpAddr::V4(Ipv4Addr::LOCALHOST)].as_slice(),
            43_119,
            &[("v", "1"), ("cap", "keyboard"), ("spki", "stable-key")][..],
        )
        .unwrap();
        assert_eq!(
            parse_candidate_fields(
                forbidden.get_fullname(),
                forbidden.get_hostname(),
                vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 43_119))],
                forbidden.get_properties(),
            ),
            Err(CandidateParseError::UnexpectedTxtProperty)
        );

        let oversized_value = "x".repeat(MAX_TXT_BYTES);
        let oversized = ServiceInfo::new(
            SERVICE_TYPE,
            &id.to_string(),
            &id.hostname(),
            [IpAddr::V4(Ipv4Addr::LOCALHOST)].as_slice(),
            43_119,
            &[("v", "1"), ("cap", oversized_value.as_str())][..],
        );
        // mdns-sd itself refuses a single DNS-SD string over 255 bytes.
        assert!(oversized.is_err());
    }

    #[test]
    fn explicit_address_path_needs_no_mdns_daemon() {
        let address = SocketAddr::from((Ipv4Addr::new(192, 0, 2, 10), 43_119));
        let candidate = UntrustedCandidate::explicit(address).unwrap();
        assert_eq!(candidate.source(), CandidateSource::Explicit);
        assert_eq!(candidate.socket_addresses(), &[address]);
        assert_eq!(candidate.ephemeral_instance_id(), None);
        assert!(candidate.protocol_versions().is_empty());

        assert!(
            UntrustedCandidate::explicit(SocketAddr::from((
                "fe80::1".parse::<Ipv6Addr>().unwrap(),
                43_119,
            )))
            .is_err()
        );
    }

    #[test]
    fn interface_candidates_drop_loopback_wildcard_and_duplicates() {
        let usable = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 4));
        assert_eq!(
            select_advertisable_addresses([
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                usable,
                usable,
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ]),
            vec![usable]
        );
    }
}
