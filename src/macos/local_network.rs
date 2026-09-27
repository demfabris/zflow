//! macOS has no API that reports Local Network access. While it is off, and
//! while its alert waits for an answer, sends to the local network fail at
//! once with EHOSTUNREACH, so a send to the mDNS group tells. The kernel finds
//! the group's route without the socket's interface, so every interface gets
//! the same answer. See Apple's TN3179:
//! https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy

use std::{
    collections::BTreeSet,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket},
    os::fd::AsRawFd,
};

/// Local Network access as the last probe saw it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalNetwork {
    /// Not probed yet, or no interface could tell.
    #[default]
    Unknown,
    Allowed,
    Blocked,
}

const MDNS_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
const MDNS_PORT: u16 = 5353;
/// A DNS header with no questions. Responders read it and answer nothing.
const EMPTY_QUERY: [u8; 12] = [0; 12];

/// Sends an empty query to the mDNS group from each running interface until
/// one goes through. macOS shows its Local Network alert on the first send.
pub fn local_network_access() -> LocalNetwork {
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return LocalNetwork::Unknown;
    };
    let mut seen = BTreeSet::new();
    classify(
        interfaces
            .into_iter()
            // Like mdns-sd, leave AWDL alone: traffic there wakes the
            // peer-to-peer radio that costs Wi-Fi latency.
            .filter(|interface| {
                interface.is_oper_up()
                    && !interface.is_loopback()
                    && !interface.is_p2p()
                    && !["awdl", "llw"]
                        .iter()
                        .any(|p| interface.name.starts_with(p))
            })
            .filter_map(|interface| Some((interface.ip(), interface.index?)))
            .filter(|&(address, index)| seen.insert((address.is_ipv4(), index)))
            .map(|(address, index)| send(address, index)),
    )
}

/// Sends from one interface. Choosing it explicitly keeps a missing route
/// from failing like a denial.
fn send(address: IpAddr, index: u32) -> io::Result<()> {
    let (socket, group) = match address {
        IpAddr::V4(address) => {
            let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
            let interface = libc::in_addr {
                s_addr: u32::from(address).to_be(),
            };
            set_option(&socket, libc::IPPROTO_IP, libc::IP_MULTICAST_IF, &interface)?;
            (socket, SocketAddr::from((MDNS_V4, MDNS_PORT)))
        }
        IpAddr::V6(_) => {
            let socket = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))?;
            set_option(&socket, libc::IPPROTO_IPV6, libc::IPV6_MULTICAST_IF, &index)?;
            (
                socket,
                SocketAddrV6::new(MDNS_V6, MDNS_PORT, 0, index).into(),
            )
        }
    };
    // The caller is the engine's worker, so a full send buffer must not stall it.
    socket.set_nonblocking(true)?;
    socket.send_to(&EMPTY_QUERY, group).map(drop)
}

fn set_option<T>(socket: &UdpSocket, level: i32, name: i32, value: &T) -> io::Result<()> {
    // SAFETY: the option points to a live value of the length passed.
    let status = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            level,
            name,
            std::ptr::from_ref(value).cast(),
            size_of::<T>() as libc::socklen_t,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// One send that goes through settles it. EHOSTUNREACH is the privacy
/// check's answer; other errors, such as an interface without multicast,
/// say nothing about access.
fn classify(results: impl IntoIterator<Item = io::Result<()>>) -> LocalNetwork {
    let mut state = LocalNetwork::Unknown;
    for result in results {
        match result {
            Ok(()) => return LocalNetwork::Allowed,
            Err(error) if error.kind() == io::ErrorKind::HostUnreachable => {
                state = LocalNetwork::Blocked;
            }
            Err(_) => {}
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(code: i32) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(code))
    }

    #[test]
    fn host_unreachable_is_blocked_and_other_errors_are_unknown() {
        assert_eq!(
            classify([failed(libc::EHOSTUNREACH)]),
            LocalNetwork::Blocked
        );
        assert_eq!(
            classify([failed(libc::ENETUNREACH), failed(libc::EHOSTUNREACH)]),
            LocalNetwork::Blocked
        );
        assert_eq!(
            classify([failed(libc::ENETUNREACH), failed(libc::EADDRNOTAVAIL)]),
            LocalNetwork::Unknown
        );
        assert_eq!(classify([]), LocalNetwork::Unknown);
    }

    #[test]
    fn one_delivered_send_is_allowed_and_stops_the_probe() {
        assert_eq!(
            classify([failed(libc::EHOSTUNREACH), Ok(())]),
            LocalNetwork::Allowed
        );
        let mut sends = 0;
        let state = classify((0..3).map(|_| {
            sends += 1;
            Ok(())
        }));
        assert_eq!((state, sends), (LocalNetwork::Allowed, 1));
    }
}
