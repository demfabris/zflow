use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use rcgen::{KeyPair, PublicKeyData};
use sha2::{Digest, Sha256};
use thiserror::Error;

const IDENTITY_FILE: &str = "identity.pk8";

#[derive(Debug)]
pub struct Identity {
    key_pair: KeyPair,
    spki: Vec<u8>,
    fingerprint: [u8; 32],
}

impl Identity {
    pub fn load_or_create(state_dir: &Path) -> Result<Self, IdentityError> {
        ensure_private_directory(state_dir)?;
        let path = state_dir.join(IDENTITY_FILE);
        if path.exists() {
            Self::load(&path)
        } else {
            Self::generate(&path)
        }
    }

    pub fn load(path: &Path) -> Result<Self, IdentityError> {
        ensure_private_file(path)?;
        let private = fs::read(path).map_err(|source| IdentityError::Read {
            path: path.to_owned(),
            source,
        })?;
        let key_pair = KeyPair::try_from(private).map_err(IdentityError::InvalidKey)?;
        Ok(Self::from_key_pair(key_pair))
    }

    fn generate(path: &Path) -> Result<Self, IdentityError> {
        let key_pair = KeyPair::generate().map_err(IdentityError::Generate)?;
        store_private_key(path, &key_pair.serialize_der())?;
        Ok(Self::from_key_pair(key_pair))
    }

    fn from_key_pair(key_pair: KeyPair) -> Self {
        let spki = key_pair.subject_public_key_info();
        let fingerprint: [u8; 32] = Sha256::digest(&spki).into();
        Self {
            key_pair,
            spki,
            fingerprint,
        }
    }

    pub fn key_pair(&self) -> &KeyPair {
        &self.key_pair
    }

    pub fn private_key_der(&self) -> Vec<u8> {
        self.key_pair.serialize_der()
    }

    pub fn spki(&self) -> &[u8] {
        &self.spki
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    pub fn fingerprint_hex(&self) -> String {
        hex(&self.fingerprint)
    }

    /// Six decimal digits derived from both identities and the pairing transcript.
    pub fn pairing_code(&self, peer_spki: &[u8], transcript: &[u8]) -> String {
        let (first, second) = if self.spki.as_slice() <= peer_spki {
            (self.spki.as_slice(), peer_spki)
        } else {
            (peer_spki, self.spki.as_slice())
        };
        let digest = Sha256::new()
            .chain_update(b"zflow pairing code v1\0")
            .chain_update((first.len() as u64).to_be_bytes())
            .chain_update(first)
            .chain_update((second.len() as u64).to_be_bytes())
            .chain_update(second)
            .chain_update((transcript.len() as u64).to_be_bytes())
            .chain_update(transcript)
            .finalize();
        let value = u32::from_be_bytes(digest[..4].try_into().unwrap()) % 1_000_000;
        format!("{value:06}")
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), IdentityError> {
    fs::create_dir_all(path).map_err(|source| IdentityError::Write {
        path: path.to_owned(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
            IdentityError::Write {
                path: path.to_owned(),
                source,
            }
        })?;
    }
    Ok(())
}

fn ensure_private_file(path: &Path) -> Result<(), IdentityError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(path).map_err(|source| IdentityError::Read {
            path: path.to_owned(),
            source,
        })?;
        if !metadata.file_type().is_file() || metadata.mode() & 0o077 != 0 {
            return Err(IdentityError::InsecurePermissions(path.to_owned()));
        }
    }
    Ok(())
}

fn store_private_key(path: &Path, bytes: &[u8]) -> Result<(), IdentityError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|source| IdentityError::Write {
        path: path.to_owned(),
        source,
    })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| IdentityError::Write {
            path: path.to_owned(),
            source,
        })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").unwrap();
    }
    output
}

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("could not generate identity: {0}")]
    Generate(rcgen::Error),
    #[error("identity key is invalid: {0}")]
    InvalidKey(rcgen::Error),
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
    #[error("identity key must be a regular file inaccessible to group and others: {0}")]
    InsecurePermissions(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_persists_and_has_stable_fingerprint() {
        let directory = tempfile::tempdir().unwrap();
        let first = Identity::load_or_create(directory.path()).unwrap();
        let fingerprint = first.fingerprint();
        let spki = first.spki().to_vec();
        drop(first);

        let second = Identity::load_or_create(directory.path()).unwrap();
        assert_eq!(second.fingerprint(), fingerprint);
        assert_eq!(second.spki(), spki);
    }

    #[test]
    fn pairing_code_is_symmetric_and_fixed_width() {
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let left = Identity::load_or_create(left_dir.path()).unwrap();
        let right = Identity::load_or_create(right_dir.path()).unwrap();
        let transcript = b"nonces and transport transcript";

        let left_code = left.pairing_code(right.spki(), transcript);
        let right_code = right.pairing_code(left.spki(), transcript);
        assert_eq!(left_code, right_code);
        assert_eq!(left_code.len(), 6);
        assert!(left_code.bytes().all(|byte| byte.is_ascii_digit()));
    }
}
