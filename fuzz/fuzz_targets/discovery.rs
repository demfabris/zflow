#![no_main]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use libfuzzer_sys::fuzz_target;
use mdns_sd::{ResolvedService, ScopedIp, ServiceInfo, TxtProperties, TxtProperty};
use zflow::discovery::{SERVICE_TYPE, parse_resolved_service};

// Any host on the LAN can send these records. Input layout: port (2 bytes),
// address count (1 byte), 17 bytes per address (a tag byte, then IPv4 in the
// next 4 bytes or IPv6 in the next 16), then service type, full name and host
// separated by NUL, then raw TXT record bytes.
fuzz_target!(|data: &[u8]| {
    if let Some(service) = resolved_service(data) {
        let _ = parse_resolved_service(&service);
    }
});

fn resolved_service(data: &[u8]) -> Option<ResolvedService> {
    let (&[high, low, count], rest) = data.split_first_chunk()?;
    let (addresses, rest) = rest.split_at_checked(usize::from(count) * 17)?;
    let mut fields = rest.splitn(4, |&byte| byte == 0);
    let mut text = || String::from_utf8_lossy(fields.next().unwrap_or_default()).into_owned();
    let (ty_domain, fullname, host) = (text(), text(), text());
    let txt = fields.next().unwrap_or_default();

    // ResolvedService is non_exhaustive, so start from a real one.
    let mut service = ServiceInfo::new(
        SERVICE_TYPE,
        "fuzz",
        "fuzz.local.",
        (),
        1,
        Vec::<TxtProperty>::new(),
    )
    .ok()?
    .as_resolved_service();
    service.ty_domain = ty_domain;
    service.fullname = fullname;
    service.host = host;
    service.port = u16::from_be_bytes([high, low]);
    service.addresses = addresses
        .chunks_exact(17)
        .map(|chunk| {
            let mut octets = [0_u8; 16];
            octets.copy_from_slice(&chunk[1..]);
            let address = if chunk[0] & 1 == 0 {
                IpAddr::V4(Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]))
            } else {
                IpAddr::V6(Ipv6Addr::from(octets))
            };
            ScopedIp::from(address)
        })
        .collect();
    service.txt_properties = TxtProperties::from(txt);
    Some(service)
}
