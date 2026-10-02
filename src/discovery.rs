//! Privacy-preserving local discovery.
//!
//! mDNS records are hints for locating a QUIC endpoint. They are never peer
//! identities and must not grant authority before the transport authenticates
//! the peer.

use std::{
    collections::BTreeSet,
    fmt,
    net::{IpAddr, SocketAddr, SocketAddrV6},
};

use mdns_sd::{
    DaemonEvent, DaemonStatus, Receiver, ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent,
    ServiceInfo, TxtProperties, UnregisterStatus,
};
use thiserror::Error;

use crate::{
    core::{InputCapabilities, InputCapability},
    hello::peer_name,
    identity::encode_hex,
    transport::INPUT_ALPN_PROTOCOL,
};

pub const SERVICE_TYPE: &str = "_zflow._udp.local.";
pub const MAX_DISCOVERY_CANDIDATES: usize = 16;
/// What a record may carry. zflow sends three; the room is for keys a newer
/// version adds, which this one skips so it can still list that computer.
pub const MAX_TXT_PROPERTIES: usize = 8;
pub const MAX_TXT_BYTES: usize = 384;

const INSTANCE_PREFIX: &str = "zf-";
const INSTANCE_ENTROPY_BYTES: usize = 16;
const INSTANCE_HEX_BYTES: usize = INSTANCE_ENTROPY_BYTES * 2;
/// Carries the input ALPN, the only protocol version zflow has.
const TXT_PROTOCOL: &str = "v";
const TXT_CAPABILITIES: &str = "cap";
/// The computer's name, so its tile has one before its hello arrives.
const TXT_NAME: &str = "name";

/// Lists this host's current addresses. Loopback and non-unicast addresses
/// never help another machine connect.
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
        let mut bytes = [0; INSTANCE_ENTROPY_BYTES];
        getrandom::fill(&mut bytes).map_err(DiscoveryError::Entropy)?;
        Ok(Self(bytes))
    }

    fn parse(value: &str) -> Result<Self, CandidateParseError> {
        let Some(hex) = value.strip_prefix(INSTANCE_PREFIX) else {
            return Err(CandidateParseError::InvalidInstanceId);
        };
        if hex.len() != INSTANCE_HEX_BYTES {
            return Err(CandidateParseError::InvalidInstanceId);
        }

        let mut bytes = [0; INSTANCE_ENTROPY_BYTES];
        for (index, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            bytes[index] = decode_hex_pair(pair).ok_or(CandidateParseError::InvalidInstanceId)?;
        }
        Ok(Self(bytes))
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
        write!(formatter, "{INSTANCE_PREFIX}{}", encode_hex(&self.0))
    }
}

/// The small, fixed-schema record zflow publishes over multicast DNS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertisement {
    quic_port: u16,
    capability_summary: InputCapabilities,
    name: Option<String>,
}

impl Advertisement {
    /// mdns-sd follows the host's interfaces itself, so no addresses are taken.
    pub fn new(
        quic_port: u16,
        capabilities: impl IntoIterator<Item = InputCapability>,
    ) -> Result<Self, DiscoveryError> {
        if quic_port == 0 {
            return Err(DiscoveryError::InvalidAdvertisement(
                "QUIC port must be non-zero",
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
            capability_summary,
            name: None,
        })
    }

    /// Names the computer in the record, in the plain form a tile shows.
    pub fn with_name(mut self, name: &str) -> Self {
        self.name = Some(peer_name(Some(name)));
        self
    }
}

/// A bounded endpoint hint which has not been authenticated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrustedCandidate {
    ephemeral_instance_id: Option<EphemeralInstanceId>,
    compatible: bool,
    capability_summary: InputCapabilities,
    socket_addresses: Vec<SocketAddr>,
    name: Option<String>,
}

impl UntrustedCandidate {
    /// Creates a candidate from a user-provided address without starting mDNS.
    pub fn explicit(address: SocketAddr) -> Result<Self, CandidateParseError> {
        validate_socket_address(address).map_err(CandidateParseError::InvalidAddress)?;
        Ok(Self {
            ephemeral_instance_id: None,
            compatible: false,
            capability_summary: InputCapabilities::default(),
            socket_addresses: vec![address],
            name: None,
        })
    }

    pub fn ephemeral_instance_id(&self) -> Option<EphemeralInstanceId> {
        self.ephemeral_instance_id
    }

    /// Whether the record advertises the ALPN this build speaks.
    pub fn is_compatible(&self) -> bool {
        self.compatible
    }

    pub fn capability_summary(&self) -> &InputCapabilities {
        &self.capability_summary
    }

    pub fn socket_addresses(&self) -> &[SocketAddr] {
        &self.socket_addresses
    }

    /// The name the record claims, already plain. Versions before arrange
    /// to pair send none.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
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
    // Receivers advertise every interface address, and IPv6 privacy addresses
    // pile up, so keep the first few in sort order (IPv4, then global IPv6)
    // instead of dropping the whole record.
    let mut addresses = BTreeSet::new();
    for scoped in &service.addresses {
        let address = match scoped {
            ScopedIp::V4(ipv4) => SocketAddr::new(IpAddr::V4(*ipv4.addr()), service.port),
            ScopedIp::V6(ipv6) => SocketAddr::V6(SocketAddrV6::new(
                *ipv6.addr(),
                service.port,
                0,
                ipv6.scope_id().index,
            )),
            _ => continue,
        };
        if validate_socket_address(address).is_ok() {
            addresses.insert(address);
        }
    }
    let addresses = addresses
        .into_iter()
        .take(MAX_DISCOVERY_CANDIDATES)
        .collect();

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
        ephemeral_instance_id: Some(instance_id),
        compatible: txt.compatible,
        capability_summary: txt.capability_summary,
        socket_addresses,
        name: txt.name,
    })
}

fn build_service_info(
    instance_id: EphemeralInstanceId,
    advertisement: &Advertisement,
) -> Result<ServiceInfo, DiscoveryError> {
    let protocol = String::from_utf8_lossy(INPUT_ALPN_PROTOCOL).into_owned();
    let capabilities = format_capabilities(&advertisement.capability_summary);
    let mut properties = vec![
        (TXT_PROTOCOL.to_owned(), protocol),
        (TXT_CAPABILITIES.to_owned(), capabilities),
    ];
    if let Some(name) = &advertisement.name {
        properties.push((TXT_NAME.to_owned(), name.clone()));
    }
    debug_assert!(txt_size(&properties) <= MAX_TXT_BYTES);

    // No address list: with addr_auto, mdns-sd adds and drops addresses as
    // interfaces change, including ones that only appear after boot.
    ServiceInfo::new(
        SERVICE_TYPE,
        &instance_id.to_string(),
        &instance_id.hostname(),
        (),
        advertisement.quic_port,
        properties.as_slice(),
    )
    .map(ServiceInfo::enable_addr_auto)
    .map_err(DiscoveryError::Mdns)
}

struct ParsedTxt {
    compatible: bool,
    capability_summary: InputCapabilities,
    name: Option<String>,
}

fn parse_txt(properties: &TxtProperties) -> Result<ParsedTxt, CandidateParseError> {
    if properties.len() > MAX_TXT_PROPERTIES {
        return Err(CandidateParseError::TooManyTxtProperties);
    }

    let mut total_size = 0usize;
    let mut compatible = None;
    let mut capabilities = None;
    let mut name = None;

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
            TXT_PROTOCOL if compatible.is_none() => {
                compatible = Some(value.as_bytes() == INPUT_ALPN_PROTOCOL);
            }
            TXT_CAPABILITIES if capabilities.is_none() => {
                capabilities = Some(parse_capabilities(value)?);
            }
            TXT_NAME if name.is_none() => name = Some(peer_name(Some(value))),
            TXT_PROTOCOL | TXT_CAPABILITIES | TXT_NAME => {
                return Err(CandidateParseError::DuplicateTxtProperty);
            }
            // Another version's key. Rejecting the record would hide that
            // computer instead of showing that it needs an update.
            _ => {}
        }
    }

    Ok(ParsedTxt {
        compatible: compatible.ok_or(CandidateParseError::MissingTxtProperty(TXT_PROTOCOL))?,
        capability_summary: capabilities
            .ok_or(CandidateParseError::MissingTxtProperty(TXT_CAPABILITIES))?,
        name,
    })
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
    // A name this version does not know is a capability from a newer one.
    for capability in value.split(',').filter_map(capability_from_name) {
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
    Entropy(getrandom::Error),
    #[error("failed to enumerate local network interfaces: {0}")]
    Interfaces(std::io::Error),
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
    #[error("duplicate TXT property")]
    DuplicateTxtProperty,
    #[error("missing TXT property {0}")]
    MissingTxtProperty(&'static str),
    #[error("capability summary is malformed")]
    InvalidCapabilities,
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
            [
                InputCapability::Keyboard,
                InputCapability::Pointer,
                InputCapability::Scroll,
            ],
        )
        .unwrap()
        .with_name("Fabricios-MacBook-Pro.local")
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
        assert!(service.is_addr_auto());
        assert!(service.get_addresses().is_empty());

        let properties = service.get_properties();
        assert!(properties.len() <= MAX_TXT_PROPERTIES);
        assert_eq!(
            properties.get_property_val_str("v").map(str::as_bytes),
            Some(INPUT_ALPN_PROTOCOL)
        );
        assert_eq!(
            properties.get_property_val_str("cap"),
            Some("keyboard,pointer,scroll")
        );
        // The name is the one thing that says which computer this is, and
        // the hello repeats it over TLS, where the key is proven.
        assert_eq!(
            properties.get_property_val_str("name"),
            Some("Fabricios-MacBook-Pro")
        );

        let rendered = format!("{service:?}").to_ascii_lowercase();
        for forbidden in [
            "spki",
            "fingerprint",
            "certificate",
            "vouch",
            "mark",
            "config_path",
            "/home/",
            "/var/lib/",
            "username",
            "account",
        ] {
            assert!(!rendered.contains(forbidden), "leaked marker {forbidden}");
        }
        // Nothing in the record looks like a key's hash either.
        assert!(
            properties
                .iter()
                .all(|property| property.val().is_none_or(|value| value.len() < 32)),
            "{properties:?}"
        );
    }

    #[test]
    fn the_advertised_name_is_plain_and_fits_a_dns_label() {
        let long = advertisement().with_name(&format!("{}\u{202e}", "a".repeat(300)));
        let service = build_service_info(test_instance(), &long).unwrap();
        let name = service
            .get_properties()
            .get_property_val_str("name")
            .unwrap();
        assert_eq!(name, "a".repeat(crate::wire::MAX_NAME_BYTES));

        let parsed = parse_txt_record(&[
            ("v", "zflow/x"),
            ("cap", "keyboard"),
            ("name", "desk\u{200b} pc.local"),
        ])
        .unwrap();
        assert_eq!(parsed.name(), Some("desk pc"));
        assert_eq!(
            parse_txt_record(&[("v", "1"), ("cap", "keyboard")])
                .unwrap()
                .name(),
            None
        );
    }

    #[test]
    fn advertisement_needs_a_port_but_no_addresses() {
        assert!(Advertisement::new(0, [InputCapability::Keyboard]).is_err());
        // Registering before the network is up is fine; mdns-sd adds addresses later.
        assert!(Advertisement::new(43_119, [InputCapability::Keyboard]).is_ok());
    }

    #[test]
    fn a_receiver_with_many_addresses_keeps_ipv4_and_the_first_few() {
        let mut service = build_service_info(test_instance(), &advertisement())
            .unwrap()
            .as_resolved_service();
        service.port = 43_119;
        service.addresses = (1..=40_u16)
            .map(|index| {
                ScopedIp::from(IpAddr::V6(Ipv6Addr::new(
                    0x2001, 0xdb8, 0, 0, 0, 0, 0, index,
                )))
            })
            .chain([ScopedIp::from(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)))])
            .collect();

        let parsed = parse_resolved_service(&service).unwrap();
        assert_eq!(parsed.socket_addresses().len(), MAX_DISCOVERY_CANDIDATES);
        assert_eq!(
            parsed.socket_addresses()[0],
            SocketAddr::from((Ipv4Addr::new(192, 0, 2, 10), 43_119))
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

        assert_eq!(parsed.ephemeral_instance_id(), Some(test_instance()));
        assert!(parsed.is_compatible());
        assert_eq!(parsed.socket_addresses(), addresses);
        assert!(
            parsed
                .capability_summary()
                .contains(InputCapability::Keyboard)
        );
    }

    fn parse_txt_record(txt: &[(&str, &str)]) -> Result<UntrustedCandidate, CandidateParseError> {
        let id = test_instance();
        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &id.to_string(),
            &id.hostname(),
            [IpAddr::V4(Ipv4Addr::LOCALHOST)].as_slice(),
            43_119,
            txt,
        )
        .unwrap();
        parse_candidate_fields(
            service.get_fullname(),
            service.get_hostname(),
            vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 43_119))],
            service.get_properties(),
        )
    }

    #[test]
    fn computers_on_other_versions_stay_listed_as_incompatible() {
        // What v0.1.0 announces. Rejecting its extra key hid it from the
        // pairing list, so nobody was told to update it.
        let old = parse_txt_record(&[
            ("v", "1"),
            ("cap", "keyboard,pointer,scroll"),
            ("token", "0123abcd"),
        ])
        .unwrap();
        assert!(!old.is_compatible());

        let alpn = std::str::from_utf8(INPUT_ALPN_PROTOCOL).unwrap();
        let newer =
            parse_txt_record(&[("v", alpn), ("cap", "keyboard,hologram"), ("x", "y")]).unwrap();
        assert!(newer.is_compatible());
        assert!(
            newer
                .capability_summary()
                .contains(InputCapability::Keyboard)
        );

        assert_eq!(
            parse_txt_record(&[("cap", "keyboard")]).err(),
            Some(CandidateParseError::MissingTxtProperty("v"))
        );
    }

    #[test]
    fn parser_rejects_oversized_txt() {
        let id = test_instance();
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
        assert_eq!(candidate.socket_addresses(), &[address]);
        assert_eq!(candidate.ephemeral_instance_id(), None);
        assert!(!candidate.is_compatible());

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
