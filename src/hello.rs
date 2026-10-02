//! Arrange to pair: what a computer says about itself before anyone trusts
//! it, and how one a person placed becomes a trusted peer.
//!
//! A hello only describes. Trust comes from a person dropping a computer's
//! tile onto the board, or from the one-shot pairing window on a fresh
//! install (see [`crate::pairing_window`]).

use std::net::{IpAddr, SocketAddr};

use anyhow::{Result, bail};

use crate::{
    config::{Config, PeerConfig, PeerPermissions},
    discovery::{MAX_DISCOVERY_CANDIDATES, UntrustedCandidate},
    wire::{HELLO_VOUCHES, Hello, MAX_NAME_BYTES, MAX_VERSION_BYTES, Os},
};

/// The longest host name the OS label is cut to before it becomes a name.
const MAX_LABEL_BYTES: usize = 255;

/// This computer's hello. `trusts_you` says whether the computer it goes to
/// is already trusted here, which the other side only shows.
pub fn local_hello(input_port: u16, trusts_you: bool) -> Hello {
    make_hello(
        &local_name(),
        input_port,
        this_host_candidates(input_port),
        trusts_you,
    )
}

pub fn make_hello(
    name: &str,
    input_port: u16,
    candidates: Vec<SocketAddr>,
    trusts_you: bool,
) -> Hello {
    let mut unique = Vec::new();
    for candidate in candidates {
        if !unique.contains(&candidate) {
            unique.push(candidate);
        }
    }
    unique.truncate(MAX_DISCOVERY_CANDIDATES);
    let mut version = env!("CARGO_PKG_VERSION").to_owned();
    truncate(&mut version, MAX_VERSION_BYTES);
    // Random until introductions use them, so real ones will not stand out.
    let mut vouches = [[0_u8; 16]; HELLO_VOUCHES];
    for vouch in &mut vouches {
        let _ = getrandom::fill(vouch);
    }
    Hello {
        name: peer_name(Some(name)),
        os: this_os(),
        version,
        input_port,
        candidates: unique,
        trusts_you,
        vouches,
    }
}

pub fn this_os() -> Os {
    if cfg!(target_os = "macos") {
        Os::Macos
    } else {
        Os::Linux
    }
}

/// Where a computer that said hello takes input: the address it spoke from
/// on the port it named, then the addresses it claims. A dialer's source
/// port is not its listening port, so it is never used.
pub fn hello_addresses(remote: SocketAddr, hello: &Hello) -> Vec<SocketAddr> {
    let observed = SocketAddr::new(remote.ip().to_canonical(), hello.input_port);
    let mut addresses = vec![observed];
    for &candidate in &hello.candidates {
        if UntrustedCandidate::explicit(candidate).is_ok() && !addresses.contains(&candidate) {
            addresses.push(candidate);
        }
    }
    addresses
}

/// Saves a computer this one now trusts and returns its name, taken from the
/// name it gave. Either computer may control the other. A key already saved
/// only learns the addresses. A namesake with another key gets an entry of
/// its own, marked with part of its key: it is never taken for the saved
/// one, whatever address it uses.
pub fn trust_peer(
    config: &mut Config,
    spki: &[u8],
    name: &str,
    addresses: &[SocketAddr],
) -> Result<String> {
    let mut record = PeerConfig::from_spki(spki, Vec::new(), default_permissions())?;
    if let Some((name, existing)) = config
        .peers
        .iter_mut()
        .find(|(_, peer)| peer.spki_der_hex == record.spki_der_hex)
    {
        existing.learn_addresses(addresses.iter().copied());
        return Ok(name.clone());
    }
    record.learn_addresses(addresses.iter().copied());
    let base = peer_name(Some(name));
    let short = record.fingerprint_hex()?[..6].to_owned();
    let name = [base.clone(), format!("{base} ({short})")]
        .into_iter()
        .chain((2..).map(|number| format!("{base} ({short}) {number}")))
        .find(|name| !config.peers.contains_key(name))
        .expect("some numbered name is free");
    config.peers.insert(name.clone(), record);
    Ok(name)
}

/// Gives the saved computer `name` a new key, for one that was reset or
/// reinstalled and dropped onto its old tile. It keeps its settings and its
/// tile, but pre-login input was granted to the old key alone.
pub fn replace_key(
    config: &mut Config,
    name: &str,
    spki: &[u8],
    addresses: &[SocketAddr],
) -> Result<()> {
    let key = PeerConfig::from_spki(spki, Vec::new(), PeerPermissions::default())?.spki_der_hex;
    if let Some((other, _)) = config
        .peers
        .iter()
        .find(|(other, peer)| peer.spki_der_hex == key && other.as_str() != name)
    {
        bail!("That computer is already added as {other}");
    }
    let Some(existing) = config.peers.get_mut(name) else {
        bail!("No computer is called {name}");
    };
    if existing.spki_der_hex != key {
        existing.spki_der_hex = key;
        existing.permissions.inject_prelogin = false;
        // Where the old key answered says nothing about the new one.
        existing.addresses.clear();
    }
    existing.learn_addresses(addresses.iter().copied());
    Ok(())
}

fn default_permissions() -> PeerPermissions {
    PeerPermissions {
        connect: true,
        send_normal: true,
        receive_normal: true,
        inject_prelogin: false,
    }
}

/// The name a computer is shown and saved under. It comes from the other
/// computer, so only plain ASCII survives: invisible, bidirectional and
/// lookalike characters could make two computers look the same in a list.
/// It fits one DNS label, so the mDNS record can carry it too.
pub fn peer_name(label: Option<&str>) -> String {
    let label = label.unwrap_or_default().trim();
    let label = label
        .len()
        .checked_sub(".local".len())
        .filter(|&end| label.is_char_boundary(end) && label[end..].eq_ignore_ascii_case(".local"))
        .map_or(label, |end| &label[..end]);
    let kept: String = label
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, ' ' | '.' | '-' | '_' | '\'')
        })
        .collect();
    let mut name = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&mut name, MAX_NAME_BYTES);
    let name = name.trim_end();
    if name.is_empty() {
        "Computer".into()
    } else {
        name.into()
    }
}

/// This computer's name, as hellos and mDNS carry it.
pub fn local_name() -> String {
    peer_name(local_device_label().as_deref())
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
    let mut name = String::from_utf8_lossy(&buffer[..end]).into_owned();
    truncate(&mut name, MAX_LABEL_BYTES);
    let label = name.trim();
    (!label.is_empty() && !label.chars().any(char::is_control)).then(|| label.to_owned())
}

/// This computer's addresses on its input port. Sent in a hello, they let
/// the other computer reach this one after either leaves the network they
/// met on, for example over Tailscale.
pub fn this_host_candidates(input_port: u16) -> Vec<SocketAddr> {
    let addresses = crate::discovery::local_unicast_addresses().unwrap_or_else(|error| {
        tracing::debug!("could not list this computer's addresses: {error}");
        Vec::new()
    });
    candidates_on_port(addresses, input_port)
}

fn candidates_on_port(addresses: Vec<IpAddr>, input_port: u16) -> Vec<SocketAddr> {
    addresses
        .into_iter()
        // A link-local address only works on the link it came from, and the
        // other computer refuses an IPv6 one without its interface scope.
        .filter(|address| match address {
            IpAddr::V4(address) => !address.is_link_local(),
            IpAddr::V6(address) => !address.is_unicast_link_local(),
        })
        .map(|address| SocketAddr::new(address, input_port))
        .collect()
}

/// Cuts `text` to at most `bytes`, on a character boundary.
fn truncate(text: &mut String, bytes: usize) {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    fn identity() -> (tempfile::TempDir, Identity) {
        let directory = tempfile::tempdir().unwrap();
        let identity = Identity::load_or_create(directory.path()).unwrap();
        (directory, identity)
    }

    #[test]
    fn names_keep_only_plain_ascii_and_fit_a_dns_label() {
        assert_eq!(
            peer_name(Some("Fabricios-MacBook-Pro.local")),
            "Fabricios-MacBook-Pro"
        );
        assert_eq!(peer_name(Some("  desk   pc  ")), "desk pc");
        assert_eq!(peer_name(Some("Mac\u{202e}kooB")), "MackooB");
        assert_eq!(peer_name(Some("<span>x</span>")), "spanxspan");
        assert_eq!(peer_name(Some("Café")), "Caf");
        assert_eq!(peer_name(Some("\u{200b}")), "Computer");
        assert_eq!(peer_name(Some(".local")), "Computer");
        assert_eq!(peer_name(None), "Computer");
        assert_eq!(peer_name(Some(&"a".repeat(400))).len(), MAX_NAME_BYTES);
        // A cut that lands after a space does not leave it dangling.
        let spaced = format!("{} b", "a".repeat(MAX_NAME_BYTES - 1));
        assert_eq!(peer_name(Some(&spaced)), "a".repeat(MAX_NAME_BYTES - 1));
        assert_eq!(peer_name(Some(&local_name())), local_name());
    }

    #[test]
    fn the_device_label_comes_from_the_os_host_name() {
        // cargo, like launchd and systemd, does not pass $HOSTNAME along.
        let label = local_device_label().expect("this host has a name");
        assert!(label.len() <= MAX_LABEL_BYTES);
        assert!(!label.chars().any(char::is_control));
    }

    #[test]
    fn a_hello_carries_every_address_the_other_computer_can_use() {
        let addresses = [
            "192.168.1.215",
            "100.114.101.60",
            "169.254.3.4",
            "fe80::1",
            "fd7a:115c:a1e0::1",
        ]
        .map(|address| address.parse().unwrap());
        assert_eq!(
            candidates_on_port(addresses.to_vec(), 43119),
            [
                "192.168.1.215:43119",
                "100.114.101.60:43119",
                "[fd7a:115c:a1e0::1]:43119",
            ]
            .map(|address| address.parse::<SocketAddr>().unwrap())
        );
        for candidate in this_host_candidates(43119) {
            UntrustedCandidate::explicit(candidate).unwrap();
        }

        let advertised: SocketAddr = "203.0.113.7:43119".parse().unwrap();
        let many = std::iter::repeat_n(advertised, 2)
            .chain((1..=20).map(|host| SocketAddr::from(([192, 0, 2, host], 43119))))
            .collect();
        let hello = make_hello("desk\u{1b}", 43119, many, false);
        assert_eq!(hello.name, "desk");
        assert_eq!(hello.os, this_os());
        assert_eq!(hello.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(hello.candidates.len(), MAX_DISCOVERY_CANDIDATES);
        assert_eq!(hello.candidates[0], advertised);
        assert_ne!(hello.candidates[1], advertised);
        // It encodes, so the bounds hold.
        crate::wire::encode(&crate::wire::WireMessage::Hello(hello)).unwrap();
        let other = local_hello(43119, true);
        assert!(other.trusts_you);
        assert_ne!(other.vouches, local_hello(43119, true).vouches);
    }

    #[test]
    fn a_hellos_addresses_start_where_it_spoke_from_on_its_input_port() {
        let hello = make_hello(
            "desk",
            43119,
            vec![
                "100.64.0.7:43119".parse().unwrap(),
                "0.0.0.0:43119".parse().unwrap(),
                "192.0.2.7:43119".parse().unwrap(),
            ],
            false,
        );
        assert_eq!(
            hello_addresses("[::ffff:192.0.2.7]:50123".parse().unwrap(), &hello),
            ["192.0.2.7:43119", "100.64.0.7:43119"].map(|a| a.parse::<SocketAddr>().unwrap())
        );
    }

    #[test]
    fn trusted_peers_are_named_from_the_host_and_namesakes_stay_apart() {
        let (_other_directory, other) = identity();
        let (_directory, known) = identity();
        let at = |address: &str| vec![address.parse::<SocketAddr>().unwrap()];
        let mut config = Config::default();
        assert_eq!(
            trust_peer(&mut config, known.spki(), "ubuntu", &at("192.0.2.1:43119")).unwrap(),
            "ubuntu"
        );
        let permissions = config.peers["ubuntu"].permissions;
        assert!(permissions.connect && permissions.send_normal && permissions.receive_normal);
        assert!(!permissions.inject_prelogin);

        // The same key again only learns where it is now, newest first, and
        // keeps its settings whatever it calls itself.
        config
            .peers
            .get_mut("ubuntu")
            .unwrap()
            .permissions
            .receive_normal = false;
        let permissions = config.peers["ubuntu"].permissions;
        assert_eq!(
            trust_peer(&mut config, known.spki(), "renamed", &at("192.0.2.9:43119")).unwrap(),
            "ubuntu"
        );
        assert_eq!(config.peers.len(), 1);
        assert_eq!(config.peers["ubuntu"].permissions, permissions);
        assert_eq!(
            config.peers["ubuntu"].addresses,
            ["192.0.2.9:43119", "192.0.2.1:43119"].map(|a| a.parse::<SocketAddr>().unwrap())
        );

        // Another key with the same name, even from the same address, is a
        // different computer: no reinstall guess replaces the saved key.
        let expected = format!("ubuntu ({})", &other.fingerprint_hex()[..6]);
        assert_eq!(
            trust_peer(
                &mut config,
                other.spki(),
                "ubun\u{200b}tu",
                &at("192.0.2.9:43119")
            )
            .unwrap(),
            expected
        );
        assert_eq!(config.peers["ubuntu"].spki_der().unwrap(), known.spki());

        let mut fresh = Config::default();
        assert_eq!(
            trust_peer(&mut fresh, known.spki(), "", &[]).unwrap(),
            "Computer"
        );
    }

    #[test]
    fn dropping_a_reinstalled_computer_on_its_tile_replaces_only_the_key() {
        let (_old_directory, old) = identity();
        let (_new_directory, new) = identity();
        let (_twin_directory, twin) = identity();
        let mut config = Config::default();
        let saved = ["192.0.2.1:43119".parse().unwrap()];
        trust_peer(&mut config, old.spki(), "ubuntu", &saved).unwrap();
        trust_peer(&mut config, twin.spki(), "laptop", &[]).unwrap();
        {
            let ubuntu = config.peers.get_mut("ubuntu").unwrap();
            ubuntu.keyboard = crate::core::KeyboardMode::Mac;
            ubuntu.permissions.inject_prelogin = true;
        }

        let now = ["192.0.2.50:43119".parse().unwrap()];
        replace_key(&mut config, "ubuntu", new.spki(), &now).unwrap();
        let ubuntu = &config.peers["ubuntu"];
        assert_eq!(ubuntu.spki_der().unwrap(), new.spki());
        assert_eq!(ubuntu.keyboard, crate::core::KeyboardMode::Mac);
        assert!(!ubuntu.permissions.inject_prelogin);
        assert_eq!(ubuntu.addresses, now);

        // A key saved under another name stays where it is.
        assert!(replace_key(&mut config, "ubuntu", twin.spki(), &[]).is_err());
        assert!(replace_key(&mut config, "nobody", old.spki(), &[]).is_err());
        assert_eq!(config.peers.len(), 2);
    }
}
