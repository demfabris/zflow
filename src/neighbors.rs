//! The zflow computers around this one: the mDNS records seen, the addresses
//! a person added, and the keys their hellos proved. Nothing here is trusted.
//! It is what the shelf of unplaced tiles and the pairing window are made
//! of, and where a link finds a trusted key's addresses now.
//!
//! Time comes from the caller, so tests run on paused time.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::{
    config::Config,
    hello::{hello_addresses, peer_name},
    identity::encode_hex,
    wire::{Hello, Os},
};

pub const MAX_INSTANCES: usize = 64;
pub const MAX_NEIGHBORS: usize = 32;
/// Addresses a person may add by hand, for computers mDNS cannot see.
pub const MAX_ADDED: usize = 8;
/// A computer stays this long after its last record went away and its last
/// hello, so a flapping network does not empty the shelf.
pub const NEIGHBOR_TTL: Duration = Duration::from_secs(60);
/// A hello this recent means the computer is here now.
pub const FRESH_HELLO: Duration = Duration::from_secs(30);
/// How soon a hello that got no answer is tried again.
const HELLO_RETRY: Duration = Duration::from_secs(10);
/// Hellos one IP address may start here per window, so a flood from one
/// host cannot keep this computer busy.
const HELLOS_PER_IP: u32 = 5;
const HELLO_WINDOW: Duration = Duration::from_secs(30);
const MAX_TRACKED_IPS: usize = 64;
const ADDED_PREFIX: &str = "address:";

/// The lowercase hex SHA-256 of a key, as config records and identities
/// print it.
pub fn fingerprint(spki: &[u8]) -> String {
    encode_hex(&Sha256::digest(spki))
}

/// A key's mark: 24 bits drawn as a small pattern beside its name, so two
/// computers with one name look different. It tells them apart; it does not
/// prove which is which.
pub fn mark(spki: &[u8]) -> String {
    let digest = Sha256::new()
        .chain_update(b"zflow mark v1")
        .chain_update(spki)
        .finalize();
    encode_hex(&digest[..3])
}

/// How a computer was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    Mdns,
    /// A person typed its address, as for one across Tailscale.
    Address,
}

/// A computer the board does not have yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unplaced {
    /// `key:<fingerprint>` once a hello proved its key, which is what
    /// `place` takes, else `instance:<record>`.
    pub id: String,
    pub name: String,
    pub os: Option<Os>,
    pub mark: Option<String>,
    pub version: Option<String>,
    pub state: UnplacedState,
    pub via: Via,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnplacedState {
    /// Found, and its hello has not come back yet.
    Identifying,
    Ready,
    /// It speaks another zflow version, so it cannot be placed.
    DifferentVersion,
    /// Another computer here has its name. It can be placed, never joined
    /// on its own.
    DuplicateName,
}

/// What the pairing window weighs: the computers nobody here trusts yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Strangers {
    pub present: Vec<Stranger>,
    /// Compatible records whose key is not known yet. One might be anyone.
    pub unidentified: usize,
    /// A record, hello or connection was turned away because a table was
    /// full, so a stranger may be missing from `present`.
    pub turned_away: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stranger {
    pub key: String,
    pub name: String,
    pub mark: String,
    /// Its key answered a hello this computer sent to an mDNS record or to
    /// an address a person added. A hello that only came in could be from
    /// anywhere, so it never makes a computer the one to join.
    pub answered: bool,
    /// Said hello within [`FRESH_HELLO`].
    pub fresh: bool,
    /// Shares its name with a trusted computer or another stranger.
    pub duplicate_name: bool,
}

/// A computer whose key a hello proved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Neighbor {
    pub spki: Vec<u8>,
    pub name: String,
    pub os: Os,
    pub version: String,
    pub mark: String,
    /// Where it takes input, best first.
    pub addresses: Vec<SocketAddr>,
    pub via: Via,
    last_hello: Instant,
    /// When its last record went away, or when it was learned without one.
    last_seen: Instant,
}

#[derive(Debug, Clone)]
struct Instance {
    addresses: Vec<SocketAddr>,
    compatible: bool,
    name: Option<String>,
    /// The key a hello to these addresses proved.
    key: Option<String>,
    /// When a hello last went to these addresses.
    asked: Option<Instant>,
}

#[derive(Debug)]
pub struct Neighbors {
    own: String,
    /// This computer's own mDNS record, which browsing finds too.
    own_record: Option<String>,
    instances: BTreeMap<String, Instance>,
    neighbors: BTreeMap<String, Neighbor>,
    added: Vec<String>,
    hellos: BTreeMap<IpAddr, (Instant, u32)>,
    /// Set for good once anything was turned away for want of room.
    turned_away: bool,
}

impl Neighbors {
    pub fn new(own_spki: &[u8]) -> Self {
        Self {
            own: fingerprint(own_spki),
            own_record: None,
            instances: BTreeMap::new(),
            neighbors: BTreeMap::new(),
            added: Vec::new(),
            hellos: BTreeMap::new(),
            turned_away: false,
        }
    }

    /// Notes that something was turned away because a table was full, here
    /// or elsewhere, such as a handshake while too many were under way. The
    /// computer turned away could be a second stranger, so the pairing
    /// window can no longer take one by itself.
    pub fn turned_one_away(&mut self) {
        self.turned_away = true;
    }

    /// Names this computer's own mDNS record. Its hellos would come back
    /// to this computer, which hangs up on them, so it is never said hello
    /// to and never holds the pairing window as a computer not known yet.
    pub fn own_record(&mut self, instance: Option<String>) {
        if let Some(instance) = &instance {
            self.instances.remove(instance);
        }
        self.own_record = instance;
    }

    /// An mDNS record was resolved. A record whose addresses changed gets a
    /// new hello, since another computer may have them now.
    pub fn instance_seen(
        &mut self,
        instance: &str,
        addresses: Vec<SocketAddr>,
        compatible: bool,
        name: Option<String>,
    ) {
        if self.own_record.as_deref() == Some(instance) {
            return;
        }
        if let Some(known) = self.instances.get_mut(instance) {
            if known.addresses != addresses {
                known.asked = None;
            }
            known.addresses = addresses;
            known.compatible = compatible;
            known.name = name;
        } else if self.instances.len() >= MAX_INSTANCES {
            self.turned_one_away();
        } else {
            self.instances.insert(
                instance.to_owned(),
                Instance {
                    addresses,
                    compatible,
                    name,
                    key: None,
                    asked: None,
                },
            );
        }
    }

    /// An mDNS record went away. Its computer stays a while in case it
    /// comes back.
    pub fn instance_gone(&mut self, instance: &str, now: Instant) {
        if let Some(Instance { key: Some(key), .. }) = self.instances.remove(instance)
            && let Some(neighbor) = self.neighbors.get_mut(&key)
        {
            neighbor.last_seen = now;
        }
    }

    /// A person added an address. It is said hello to like a record, and
    /// stays until more are added than [`MAX_ADDED`].
    pub fn add_address(&mut self, address: SocketAddr) {
        let id = format!("{ADDED_PREFIX}{address}");
        self.added.retain(|added| *added != id);
        self.added.push(id.clone());
        if self.added.len() > MAX_ADDED {
            let oldest = self.added.remove(0);
            self.instances.remove(&oldest);
        }
        self.instances.insert(
            id,
            Instance {
                addresses: vec![address],
                compatible: true,
                name: None,
                key: None,
                asked: None,
            },
        );
    }

    /// Records that need a hello now, at most `room` of them: those never
    /// asked since their addresses changed, and unknown ones again after a
    /// while. Each is marked asked. Pass the record back to [`Self::hello`].
    pub fn take_due_hellos(&mut self, room: usize, now: Instant) -> Vec<(String, Vec<SocketAddr>)> {
        let mut due = Vec::new();
        for (id, instance) in &mut self.instances {
            if due.len() == room {
                break;
            }
            let retry = instance.key.is_none()
                && instance
                    .asked
                    .is_some_and(|asked| now.duration_since(asked) >= HELLO_RETRY);
            if instance.compatible && (instance.asked.is_none() || retry) {
                instance.asked = Some(now);
                due.push((id.clone(), instance.addresses.clone()));
            }
        }
        due
    }

    /// Whether to answer one more hello from `ip` now. One from loopback or
    /// from this computer's own addresses, `local`, comes from a process
    /// here rather than another computer, and is never answered.
    pub fn allow_hello(&mut self, ip: IpAddr, local: &[IpAddr], now: Instant) -> bool {
        let ip = ip.to_canonical();
        if ip.is_loopback() || local.contains(&ip) {
            return false;
        }
        self.hellos
            .retain(|_, (start, _)| now.duration_since(*start) < HELLO_WINDOW);
        if !self.hellos.contains_key(&ip) && self.hellos.len() >= MAX_TRACKED_IPS {
            self.turned_one_away();
            return false;
        }
        let (_, count) = self.hellos.entry(ip).or_insert((now, 0));
        *count += 1;
        *count <= HELLOS_PER_IP
    }

    /// A hello came back from a record, or arrived from `remote`. Returns
    /// the key's fingerprint, or None when it is this computer's own key or
    /// there is no room for another computer.
    pub fn hello(
        &mut self,
        spki: &[u8],
        remote: SocketAddr,
        hello: &Hello,
        instance: Option<&str>,
        now: Instant,
    ) -> Option<String> {
        let key = fingerprint(spki);
        let instance = instance.and_then(|id| Some((id, self.instances.get_mut(id)?)));
        let via = match &instance {
            Some((id, _)) if id.starts_with(ADDED_PREFIX) => Via::Address,
            _ => Via::Mdns,
        };
        if let Some((_, instance)) = instance {
            instance.key = Some(key.clone());
        }
        if key == self.own {
            return None;
        }
        self.expire(now);
        if !self.neighbors.contains_key(&key) && self.neighbors.len() >= MAX_NEIGHBORS {
            self.turned_one_away();
            return None;
        }
        let neighbor = self
            .neighbors
            .entry(key.clone())
            .or_insert_with(|| Neighbor {
                spki: spki.to_vec(),
                name: String::new(),
                os: hello.os,
                version: String::new(),
                mark: mark(spki),
                addresses: Vec::new(),
                via,
                last_hello: now,
                last_seen: now,
            });
        neighbor.name = peer_name(Some(&hello.name));
        neighbor.os = hello.os;
        neighbor.version = hello
            .version
            .chars()
            .filter(char::is_ascii_graphic)
            .collect();
        neighbor.addresses = hello_addresses(remote, hello);
        if via == Via::Address {
            neighbor.via = via;
        }
        neighbor.last_hello = now;
        neighbor.last_seen = now;
        Some(key)
    }

    pub fn neighbor(&self, key: &str) -> Option<&Neighbor> {
        self.neighbors.get(key)
    }

    /// Where the computer with this key takes input now: the addresses of
    /// every record its hellos came from, then what its hello claimed. A
    /// link pins the key, so a wrong guess only fails to connect.
    pub fn addresses_for_key(&self, key: &str) -> Vec<SocketAddr> {
        let mut addresses: Vec<SocketAddr> = self
            .instances
            .values()
            .filter(|instance| instance.key.as_deref() == Some(key))
            .flat_map(|instance| instance.addresses.iter().copied())
            .collect();
        if let Some(neighbor) = self.neighbors.get(key) {
            addresses.extend(&neighbor.addresses);
        }
        let mut seen = BTreeSet::new();
        addresses.retain(|address| seen.insert(*address));
        addresses
    }

    /// Drops computers whose records are gone and which have not said hello
    /// for [`NEIGHBOR_TTL`]. Call it on each tick.
    pub fn expire(&mut self, now: Instant) {
        let mapped: BTreeSet<&String> = self
            .instances
            .values()
            .filter_map(|instance| instance.key.as_ref())
            .collect();
        self.neighbors.retain(|key, neighbor| {
            mapped.contains(key)
                || now.duration_since(neighbor.last_hello.max(neighbor.last_seen)) < NEIGHBOR_TTL
        });
    }

    /// The tiles for the shelf under the board: every computer around that
    /// is not trusted here, by name.
    pub fn unplaced(&self, config: &Config) -> Vec<Unplaced> {
        let trusted = trusted_keys(config);
        let strangers = self.strangers_of(config, &trusted, None);
        let mut tiles: Vec<Unplaced> = self
            .instances
            .iter()
            .filter(|(_, instance)| instance.key.is_none())
            .map(|(id, instance)| Unplaced {
                id: format!("instance:{id}"),
                name: instance
                    .name
                    .clone()
                    .or_else(|| id.strip_prefix(ADDED_PREFIX).map(str::to_owned))
                    .unwrap_or_else(|| "Computer".into()),
                os: None,
                mark: None,
                version: None,
                state: if instance.compatible {
                    UnplacedState::Identifying
                } else {
                    UnplacedState::DifferentVersion
                },
                via: if id.starts_with(ADDED_PREFIX) {
                    Via::Address
                } else {
                    Via::Mdns
                },
            })
            .collect();
        tiles.extend(strangers.iter().map(|stranger| {
            let neighbor = &self.neighbors[&stranger.key];
            Unplaced {
                id: format!("key:{}", stranger.key),
                name: neighbor.name.clone(),
                os: Some(neighbor.os),
                mark: Some(neighbor.mark.clone()),
                version: Some(neighbor.version.clone()),
                state: if stranger.duplicate_name {
                    UnplacedState::DuplicateName
                } else {
                    UnplacedState::Ready
                },
                via: neighbor.via,
            }
        }));
        tiles.sort_by(|left, right| (&left.name, &left.id).cmp(&(&right.name, &right.id)));
        tiles
    }

    /// The computers nobody here trusts yet, for the pairing window.
    pub fn strangers(&self, config: &Config, now: Instant) -> Strangers {
        let trusted = trusted_keys(config);
        Strangers {
            present: self.strangers_of(config, &trusted, Some(now)),
            unidentified: self
                .instances
                .values()
                .filter(|instance| instance.compatible && instance.key.is_none())
                .count(),
            turned_away: self.turned_away,
        }
    }

    fn strangers_of(
        &self,
        config: &Config,
        trusted: &BTreeSet<String>,
        now: Option<Instant>,
    ) -> Vec<Stranger> {
        let untrusted: Vec<(&String, &Neighbor)> = self
            .neighbors
            .iter()
            .filter(|(key, _)| !trusted.contains(*key))
            .collect();
        let answered: BTreeSet<&String> = self
            .instances
            .values()
            .filter_map(|instance| instance.key.as_ref())
            .collect();
        untrusted
            .iter()
            .map(|(key, neighbor)| {
                let namesakes = untrusted
                    .iter()
                    .filter(|(_, other)| other.name == neighbor.name)
                    .count();
                let duplicate_name = namesakes > 1
                    || config
                        .peers
                        .keys()
                        .any(|name| peer_name(Some(name)) == neighbor.name);
                Stranger {
                    key: (*key).clone(),
                    name: neighbor.name.clone(),
                    mark: neighbor.mark.clone(),
                    answered: answered.contains(*key),
                    fresh: now
                        .is_some_and(|now| now.duration_since(neighbor.last_hello) < FRESH_HELLO),
                    duplicate_name,
                }
            })
            .collect()
    }
}

fn trusted_keys(config: &Config) -> BTreeSet<String> {
    config
        .peers
        .values()
        .filter_map(|peer| peer.fingerprint_hex().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::PeerConfig, hello::make_hello};

    fn spki(byte: u8) -> Vec<u8> {
        vec![byte; 91]
    }

    fn at(address: &str) -> SocketAddr {
        address.parse().unwrap()
    }

    fn trusting(config: &mut Config, name: &str, key: &[u8]) {
        let record =
            PeerConfig::from_spki(key, Vec::new(), crate::config::PeerPermissions::default())
                .unwrap();
        config.peers.insert(name.into(), record);
    }

    #[test]
    fn marks_are_short_stable_and_differ_by_key() {
        assert_eq!(mark(&spki(1)), mark(&spki(1)));
        assert_eq!(mark(&spki(1)).len(), 6);
        assert_ne!(mark(&spki(1)), mark(&spki(2)));
        assert_eq!(fingerprint(&spki(1)).len(), 64);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hello_maps_its_record_to_the_key_and_links_find_it_by_key() {
        let mut around = Neighbors::new(&spki(0));
        let desk = vec![at("192.0.2.7:43119"), at("[2001:db8::7]:43119")];
        around.instance_seen("zf-desk", desk.clone(), true, Some("desk".into()));
        around.instance_seen("zf-old", vec![at("192.0.2.9:43119")], false, None);
        let now = Instant::now();
        // Only the compatible record is asked, once.
        assert_eq!(
            around.take_due_hellos(4, now),
            [("zf-desk".to_owned(), desk.clone())]
        );
        assert!(around.take_due_hellos(4, now).is_empty());

        let hello = make_hello("desk", 43119, vec![at("100.64.0.7:43119")], true);
        let key = around
            .hello(&spki(1), desk[0], &hello, Some("zf-desk"), now)
            .unwrap();
        assert_eq!(key, fingerprint(&spki(1)));
        // The record's addresses first, then what the hello claimed; the
        // addresses do not have to overlap anything saved.
        assert_eq!(
            around.addresses_for_key(&key),
            [desk[0], desk[1], at("100.64.0.7:43119")]
        );
        let neighbor = around.neighbor(&key).unwrap();
        assert_eq!(neighbor.name, "desk");
        assert_eq!(neighbor.mark, mark(&spki(1)));
        assert!(around.addresses_for_key(&fingerprint(&spki(2))).is_empty());

        // New addresses for the record mean a new hello.
        let moved = vec![at("192.0.2.70:43119")];
        around.instance_seen("zf-desk", moved.clone(), true, Some("desk".into()));
        assert_eq!(around.take_due_hellos(4, now), [("zf-desk".into(), moved)]);

        // This computer's own record maps to its own key and stays hidden.
        around.instance_seen("zf-me", vec![at("192.0.2.2:43119")], true, None);
        around.take_due_hellos(4, now);
        assert_eq!(
            around.hello(&spki(0), at("192.0.2.2:43119"), &hello, Some("zf-me"), now),
            None
        );
        let shelf = around.unplaced(&Config::default());
        assert_eq!(
            shelf
                .iter()
                .map(|tile| tile.id.as_str())
                .collect::<Vec<_>>(),
            ["instance:zf-old", format!("key:{key}").as_str()]
        );
        assert_eq!(shelf[0].name, "Computer");
        assert_eq!(shelf[0].state, UnplacedState::DifferentVersion);
        assert_eq!(shelf[1].state, UnplacedState::Ready);
        // It claimed to trust this computer, which anyone can claim, so the
        // shelf does not repeat it.
        assert!(hello.trusts_you);
        let tile = serde_json::to_value(&shelf[1]).unwrap();
        assert!(tile.get("trusts_you").is_none(), "{tile}");
    }

    #[tokio::test(start_paused = true)]
    async fn unanswered_records_are_asked_again_after_a_while_and_in_batches() {
        let mut around = Neighbors::new(&spki(0));
        for index in 1..=6 {
            let address = SocketAddr::from(([192, 0, 2, index], 43119));
            around.instance_seen(&format!("zf-{index}"), vec![address], true, None);
        }
        let start = Instant::now();
        assert_eq!(around.take_due_hellos(4, start).len(), 4);
        assert_eq!(around.take_due_hellos(4, start).len(), 2);
        tokio::time::advance(HELLO_RETRY).await;
        assert_eq!(around.take_due_hellos(10, Instant::now()).len(), 6);
        assert_eq!(around.strangers(&Config::default(), start).unidentified, 6);
    }

    #[tokio::test(start_paused = true)]
    async fn this_computers_own_record_is_never_asked_and_holds_nothing_up() {
        let mut around = Neighbors::new(&spki(0));
        let own = vec![at("192.0.2.2:43119"), at("[2001:db8::2]:43119")];
        around.instance_seen("zf-me", own.clone(), true, Some("me".into()));
        // Seen before its own advertisement was known, and dropped once it is.
        around.own_record(Some("zf-me".into()));
        around.instance_seen("zf-me", own, true, Some("me".into()));
        around.instance_seen("zf-desk", vec![at("192.0.2.7:43119")], true, None);
        let now = Instant::now();
        let due = around.take_due_hellos(4, now);
        assert_eq!(due, [("zf-desk".into(), vec![at("192.0.2.7:43119")])]);
        let hello = make_hello("desk", 43119, Vec::new(), false);
        around.hello(
            &spki(1),
            at("192.0.2.7:43119"),
            &hello,
            Some("zf-desk"),
            now,
        );
        let strangers = around.strangers(&Config::default(), now);
        assert_eq!((strangers.present.len(), strangers.unidentified), (1, 0));
        assert!(
            around
                .unplaced(&Config::default())
                .iter()
                .all(|tile| tile.name != "me")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn computers_expire_a_minute_after_their_record_and_hello() {
        let mut around = Neighbors::new(&spki(0));
        around.instance_seen("zf-desk", vec![at("192.0.2.7:43119")], true, None);
        let hello = make_hello("desk", 43119, Vec::new(), false);
        let key = around
            .hello(
                &spki(1),
                at("192.0.2.7:43119"),
                &hello,
                Some("zf-desk"),
                Instant::now(),
            )
            .unwrap();
        // While its record is up it never expires.
        tokio::time::advance(NEIGHBOR_TTL * 2).await;
        around.expire(Instant::now());
        assert!(around.neighbor(&key).is_some());

        around.instance_gone("zf-desk", Instant::now());
        tokio::time::advance(NEIGHBOR_TTL - Duration::from_millis(1)).await;
        around.expire(Instant::now());
        assert!(around.neighbor(&key).is_some());
        tokio::time::advance(Duration::from_millis(1)).await;
        around.expire(Instant::now());
        assert!(around.neighbor(&key).is_none());
        assert!(around.unplaced(&Config::default()).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn computers_and_hellos_are_capped() {
        let mut around = Neighbors::new(&spki(0));
        let now = Instant::now();
        let hello = make_hello("x", 43119, Vec::new(), false);
        for byte in 1..=MAX_NEIGHBORS as u8 {
            assert!(
                around
                    .hello(&spki(byte), at("192.0.2.7:43119"), &hello, None, now)
                    .is_some()
            );
        }
        assert!(
            around
                .hello(&spki(200), at("192.0.2.7:43119"), &hello, None, now)
                .is_none()
        );
        // A known one still refreshes.
        assert!(
            around
                .hello(&spki(1), at("192.0.2.7:43119"), &hello, None, now)
                .is_some()
        );

        let ip: IpAddr = "192.0.2.7".parse().unwrap();
        for _ in 0..HELLOS_PER_IP {
            assert!(around.allow_hello(ip, &[], now));
        }
        assert!(!around.allow_hello(ip, &[], now));
        assert!(around.allow_hello("192.0.2.8".parse().unwrap(), &[], now));
        tokio::time::advance(HELLO_WINDOW).await;
        assert!(around.allow_hello(ip, &[], Instant::now()));

        for index in 0..=MAX_INSTANCES {
            let address = SocketAddr::from(([198, 51, 100, (index % 250) as u8], 43119));
            around.instance_seen(&format!("zf-{index}"), vec![address], true, None);
        }
        assert_eq!(around.instances.len(), MAX_INSTANCES);
    }

    #[test]
    fn hellos_from_this_computer_are_never_answered() {
        let mut around = Neighbors::new(&spki(0));
        let now = Instant::now();
        let own: IpAddr = "192.0.2.2".parse().unwrap();
        for ip in [
            "127.0.0.1",
            "127.0.0.53",
            "::1",
            "::ffff:127.0.0.1",
            "192.0.2.2",
            "::ffff:192.0.2.2",
        ] {
            assert!(
                !around.allow_hello(ip.parse().unwrap(), &[own], now),
                "{ip}"
            );
        }
        assert!(around.allow_hello("192.0.2.3".parse().unwrap(), &[own], now));
    }

    #[tokio::test(start_paused = true)]
    async fn only_a_key_found_by_this_computers_own_hello_has_answered() {
        let mut around = Neighbors::new(&spki(0));
        let now = Instant::now();
        let desk = at("192.0.2.7:43119");
        let hello = make_hello("desk", 43119, Vec::new(), false);
        let answered =
            |around: &Neighbors| around.strangers(&Config::default(), now).present[0].answered;
        // A hello that came in shows a tile, but could be from anywhere.
        let key = around.hello(&spki(1), desk, &hello, None, now).unwrap();
        assert!(!answered(&around));
        assert_eq!(
            around.unplaced(&Config::default())[0].id,
            format!("key:{key}")
        );
        // The answer to a hello sent to its record proves where it is.
        around.instance_seen("zf-desk", vec![desk], true, Some("desk".into()));
        around.take_due_hellos(4, now);
        around.hello(&spki(1), desk, &hello, Some("zf-desk"), now);
        assert!(answered(&around));
        // Without the record, it is back to a hello that came in.
        around.instance_gone("zf-desk", now);
        assert!(!answered(&around));
        // An address a person added counts like a record.
        around.add_address(desk);
        let added = format!("address:{desk}");
        around.hello(&spki(1), desk, &hello, Some(&added), now);
        assert!(answered(&around));
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_table_says_something_was_turned_away() {
        let now = Instant::now();
        let turned_away =
            |around: &Neighbors| around.strangers(&Config::default(), now).turned_away;
        let hello = make_hello("x", 43119, Vec::new(), false);
        let remote = at("192.0.2.7:43119");

        let mut around = Neighbors::new(&spki(0));
        for index in 0..MAX_INSTANCES {
            let address = SocketAddr::from(([198, 51, 100, (index % 250) as u8], 43119));
            around.instance_seen(&format!("zf-{index}"), vec![address], true, None);
        }
        // A known record still updates; only a new one is turned away.
        around.instance_seen("zf-0", vec![remote], true, None);
        assert!(!turned_away(&around));
        around.instance_seen("zf-new", vec![remote], true, None);
        assert!(turned_away(&around));

        let mut around = Neighbors::new(&spki(0));
        for byte in 1..=MAX_NEIGHBORS as u8 {
            around.hello(&spki(byte), remote, &hello, None, now);
        }
        assert!(!turned_away(&around));
        around.hello(&spki(200), remote, &hello, None, now);
        assert!(turned_away(&around));

        let mut around = Neighbors::new(&spki(0));
        for host in 0..MAX_TRACKED_IPS as u32 {
            let ip = std::net::Ipv4Addr::from(0xc633_6400 + host);
            assert!(around.allow_hello(ip.into(), &[], now));
        }
        // Too many hellos from one address is that address's own limit.
        let first: IpAddr = "198.51.100.0".parse().unwrap();
        for _ in 0..HELLOS_PER_IP {
            around.allow_hello(first, &[], now);
        }
        assert!(!turned_away(&around));
        assert!(!around.allow_hello("192.0.2.99".parse().unwrap(), &[], now));
        assert!(turned_away(&around));

        // Elsewhere, such as too many handshakes at once, counts the same.
        let mut around = Neighbors::new(&spki(0));
        around.turned_one_away();
        assert!(turned_away(&around));
    }

    #[tokio::test(start_paused = true)]
    async fn the_shelf_leaves_out_trusted_keys_and_flags_namesakes() {
        let mut around = Neighbors::new(&spki(0));
        let now = Instant::now();
        let say = |name: &str| make_hello(name, 43119, Vec::new(), false);
        let remote = at("192.0.2.7:43119");
        let desk = around
            .hello(&spki(1), remote, &say("desk"), None, now)
            .unwrap();
        let twin = around
            .hello(&spki(2), remote, &say("desk"), None, now)
            .unwrap();
        let laptop = around
            .hello(&spki(3), remote, &say("laptop"), None, now)
            .unwrap();
        let mut config = Config::default();
        let shelf = around.unplaced(&config);
        assert_eq!(shelf.len(), 3);
        let state = |shelf: &[Unplaced], key: &str| {
            shelf
                .iter()
                .find(|tile| tile.id == format!("key:{key}"))
                .map(|tile| tile.state)
        };
        assert_eq!(state(&shelf, &desk), Some(UnplacedState::DuplicateName));
        assert_eq!(state(&shelf, &laptop), Some(UnplacedState::Ready));

        // Trusting one takes it off the shelf. Its namesake is still one.
        trusting(&mut config, "desk", &spki(1));
        let shelf = around.unplaced(&config);
        assert_eq!(state(&shelf, &desk), None);
        assert_eq!(state(&shelf, &twin), Some(UnplacedState::DuplicateName));
        // Forgetting it puts it back at once.
        config.peers.clear();
        assert_eq!(around.unplaced(&config).len(), 3);
        let strangers = around.strangers(&config, now);
        assert_eq!(strangers.present.len(), 3);
        assert!(strangers.present.iter().all(|stranger| stranger.fresh));
        tokio::time::advance(FRESH_HELLO).await;
        let stale = around.strangers(&config, Instant::now());
        assert!(stale.present.iter().all(|stranger| !stranger.fresh));
    }

    #[tokio::test(start_paused = true)]
    async fn an_added_address_is_asked_like_a_record_and_found_by_key() {
        let mut around = Neighbors::new(&spki(0));
        let tailnet = at("100.64.0.7:43119");
        around.add_address(tailnet);
        let shelf = around.unplaced(&Config::default());
        assert_eq!(shelf[0].name, "100.64.0.7:43119");
        assert_eq!(shelf[0].via, Via::Address);
        let now = Instant::now();
        let due = around.take_due_hellos(4, now);
        assert_eq!(due, [(format!("address:{tailnet}"), vec![tailnet])]);
        let hello = make_hello("far", 43119, Vec::new(), false);
        let key = around
            .hello(&spki(1), tailnet, &hello, Some(&due[0].0), now)
            .unwrap();
        assert_eq!(around.neighbor(&key).unwrap().via, Via::Address);
        assert_eq!(around.addresses_for_key(&key), [tailnet]);

        for host in 1..=MAX_ADDED as u8 {
            around.add_address(SocketAddr::from(([100, 64, 1, host], 43119)));
        }
        // The oldest added address made room, and its computer stays only
        // as long as its hellos do.
        assert!(around.addresses_for_key(&key).contains(&tailnet));
        assert!(!around.instances.contains_key(&format!("address:{tailnet}")));
    }
}
