use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const CONFIG_VERSION: u32 = 1;
pub const MAX_LEASE: Duration = Duration::from_secs(1);
pub const MAX_CHECKPOINT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub daemon: DaemonConfig,
    pub input: InputConfig,
    pub transport: TransportConfig,
    pub playout: PlayoutConfig,
    pub peers: BTreeMap<String, PeerConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            daemon: DaemonConfig::default(),
            input: InputConfig::default(),
            transport: TransportConfig::default(),
            playout: PlayoutConfig::default(),
            peers: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    pub state_dir: PathBuf,
    pub control_socket: PathBuf,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            state_dir: PathBuf::from("/var/lib/zflow"),
            control_socket: PathBuf::from("/run/zflow/zflowd.sock"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct InputConfig {
    pub capture_devices: Vec<DeviceSelector>,
    pub activation_chord: Vec<String>,
    pub escape_chord: Vec<String>,
    pub allow_prelogin_input: bool,
    pub experimental_touchpad: bool,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            capture_devices: Vec::new(),
            activation_chord: vec![
                "KEY_LEFTCTRL".into(),
                "KEY_LEFTMETA".into(),
                "KEY_F12".into(),
            ],
            escape_chord: vec![
                "KEY_LEFTCTRL".into(),
                "KEY_LEFTMETA".into(),
                "KEY_BACKSPACE".into(),
            ],
            allow_prelogin_input: false,
            experimental_touchpad: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct DeviceSelector {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phys: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TransportConfig {
    pub listen: SocketAddr,
    pub checkpoint_ms: u64,
    pub lease_ms: u64,
    pub discovery: bool,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 43119),
            checkpoint_ms: 250,
            lease_ms: 900,
            discovery: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct PlayoutConfig {
    pub mode: PlayoutMode,
    pub fixed_delay_ms: u64,
    pub minimum_delay_ms: u64,
    pub maximum_delay_ms: u64,
    pub percentile: f64,
}

impl Default for PlayoutConfig {
    fn default() -> Self {
        Self {
            mode: PlayoutMode::Adaptive,
            fixed_delay_ms: 8,
            minimum_delay_ms: 3,
            maximum_delay_ms: 35,
            percentile: 0.80,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlayoutMode {
    Fixed,
    Adaptive,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PeerConfig {
    /// Hex-encoded DER SubjectPublicKeyInfo pinned during authenticated pairing.
    pub spki_der_hex: String,
    #[serde(default)]
    pub addresses: Vec<SocketAddr>,
    #[serde(default)]
    pub permissions: PeerPermissions,
}

impl PeerConfig {
    pub fn from_spki(
        spki: &[u8],
        addresses: Vec<SocketAddr>,
        permissions: PeerPermissions,
    ) -> Result<Self, ConfigError> {
        validate_spki(spki)?;
        Ok(Self {
            spki_der_hex: encode_hex(spki),
            addresses,
            permissions,
        })
    }

    pub fn spki_der(&self) -> Result<Vec<u8>, ConfigError> {
        let spki = decode_hex(&self.spki_der_hex).ok_or(ConfigError::Invalid(
            "peer SPKI must be valid hexadecimal DER",
        ))?;
        validate_spki(&spki)?;
        Ok(spki)
    }

    pub fn fingerprint_hex(&self) -> Result<String, ConfigError> {
        Ok(encode_hex(&Sha256::digest(self.spki_der()?)))
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PeerPermissions {
    pub connect: bool,
    pub send_normal: bool,
    pub receive_normal: bool,
    pub inject_prelogin: bool,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let config: Self = toml::from_str(&text).map_err(ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        let text = toml::to_string_pretty(self).map_err(ConfigError::Serialize)?;
        save_text(path, &text)
    }
}

pub(crate) fn save_text(path: &Path, text: &str) -> Result<(), ConfigError> {
    let parent = path
        .parent()
        .ok_or_else(|| ConfigError::NoParent(path.to_owned()))?;
    fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
        path: parent.to_owned(),
        source,
    })?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ConfigError::InvalidPath(path.to_owned()))?;
    let temporary = parent.join(format!(".{file_name}.tmp-{}", std::process::id()));
    #[cfg(unix)]
    let existing_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Some(metadata),
        Ok(_) => return Err(ConfigError::UnsafeTarget(path.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|source| ConfigError::Write {
            path: temporary.clone(),
            source,
        })?;
    let write_result = (|| {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        #[cfg(unix)]
        if let Some(metadata) = existing_metadata {
            use std::os::unix::fs::{MetadataExt, PermissionsExt, chown};
            fs::set_permissions(
                &temporary,
                fs::Permissions::from_mode(metadata.mode() & 0o777),
            )?;
            chown(&temporary, Some(metadata.uid()), Some(metadata.gid()))?;
        }
        fs::rename(&temporary, path)?;
        Ok::<_, std::io::Error>(())
    })();
    if let Err(source) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(ConfigError::Write {
            path: path.to_owned(),
            source,
        });
    }
    Ok(())
}

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion(self.version));
        }
        if self.transport.listen.port() == 0 {
            return Err(ConfigError::Invalid(
                "transport.listen port must be non-zero",
            ));
        }
        if !(1..=MAX_LEASE.as_millis() as u64).contains(&self.transport.lease_ms) {
            return Err(ConfigError::Invalid(
                "transport.lease_ms must be between 1 and 1000",
            ));
        }
        if !(1..=MAX_CHECKPOINT.as_millis() as u64).contains(&self.transport.checkpoint_ms) {
            return Err(ConfigError::Invalid(
                "transport.checkpoint_ms must be between 1 and 250",
            ));
        }
        if self.input.activation_chord.is_empty() || self.input.escape_chord.is_empty() {
            return Err(ConfigError::Invalid(
                "activation and escape chords cannot be empty",
            ));
        }
        let activation = self.input.activation_chord.iter().collect::<BTreeSet<_>>();
        let escape = self.input.escape_chord.iter().collect::<BTreeSet<_>>();
        if activation.len() != self.input.activation_chord.len()
            || escape.len() != self.input.escape_chord.len()
        {
            return Err(ConfigError::Invalid(
                "input chords cannot contain duplicates",
            ));
        }
        if activation == escape || activation.is_subset(&escape) || escape.is_subset(&activation) {
            return Err(ConfigError::Invalid(
                "activation and escape chords cannot contain one another",
            ));
        }
        let unique: BTreeSet<_> = self.input.capture_devices.iter().collect();
        if unique.len() != self.input.capture_devices.len() {
            return Err(ConfigError::Invalid("capture devices must be unique"));
        }
        if !(0.5..=0.999).contains(&self.playout.percentile) {
            return Err(ConfigError::Invalid(
                "playout.percentile must be between 0.5 and 0.999",
            ));
        }
        if self.playout.minimum_delay_ms > self.playout.maximum_delay_ms {
            return Err(ConfigError::Invalid(
                "playout.minimum_delay_ms cannot exceed maximum_delay_ms",
            ));
        }
        for peer in self.peers.values() {
            peer.spki_der()?;
        }
        Ok(())
    }
}

fn validate_spki(spki: &[u8]) -> Result<(), ConfigError> {
    // The P-256 identities generated by zflow are 91 bytes today. Keep the
    // config format algorithm-agile while bounding allocations and junk data.
    if spki.is_empty() || spki.len() > 2048 {
        return Err(ConfigError::Invalid(
            "peer SPKI DER must contain between 1 and 2048 bytes",
        ));
    }
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            Some((high << 4) | low)
        })
        .collect()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid TOML: {0}")]
    Parse(toml::de::Error),
    #[error("could not serialize TOML: {0}")]
    Serialize(toml::ser::Error),
    #[error("configuration path has no parent: {0}")]
    NoParent(PathBuf),
    #[error("configuration path is invalid: {0}")]
    InvalidPath(PathBuf),
    #[error("refusing to replace non-regular configuration target: {0}")]
    UnsafeTarget(PathBuf),
    #[error("unsupported configuration version {0}")]
    UnsupportedVersion(u32),
    #[error("invalid configuration: {0}")]
    Invalid(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trips() {
        let encoded = toml::to_string(&Config::default()).unwrap();
        let decoded: Config = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded, Config::default());
        decoded.validate().unwrap();
    }

    #[test]
    fn packaged_configuration_matches_code_defaults() {
        let packaged: Config =
            toml::from_str(include_str!("../packaging/config/zflow.toml")).unwrap();
        assert_eq!(packaged, Config::default());
    }

    #[test]
    fn save_is_private_and_loads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        Config::default().save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn rejects_overlong_lease() {
        let mut config = Config::default();
        config.transport.lease_ms = 1001;
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn rejects_ambiguous_or_duplicate_chords() {
        let mut config = Config::default();
        config.input.escape_chord = config.input.activation_chord.clone();
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));

        let mut config = Config::default();
        config.input.activation_chord.push("KEY_F12".into());
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn peer_record_round_trips_the_pinned_spki() {
        let spki = b"test subject public key info";
        let peer = PeerConfig::from_spki(spki, Vec::new(), PeerPermissions::default()).unwrap();

        assert_eq!(peer.spki_der().unwrap(), spki);
        assert_eq!(peer.fingerprint_hex().unwrap().len(), 64);
    }

    #[test]
    fn peer_record_rejects_invalid_hex_and_empty_keys() {
        let mut config = Config::default();
        config.peers.insert(
            "desk".into(),
            PeerConfig {
                spki_der_hex: "not hex".into(),
                addresses: Vec::new(),
                permissions: PeerPermissions::default(),
            },
        );
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));

        assert!(PeerConfig::from_spki(&[], Vec::new(), PeerPermissions::default()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_existing_owner_group_and_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        Config::default().save(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let before = fs::metadata(&path).unwrap();

        let mut config = Config::load(&path).unwrap();
        config.transport.checkpoint_ms = 200;
        config.save(&path).unwrap();

        let after = fs::metadata(&path).unwrap();
        assert_eq!(after.uid(), before.uid());
        assert_eq!(after.gid(), before.gid());
        assert_eq!(after.mode() & 0o777, 0o640);
    }

    #[cfg(unix)]
    #[test]
    fn save_refuses_symlink_target() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real.toml");
        let link = directory.path().join("zflow.toml");
        Config::default().save(&real).unwrap();
        symlink(&real, &link).unwrap();
        assert!(matches!(
            Config::default().save(&link),
            Err(ConfigError::UnsafeTarget(_))
        ));
    }
}
