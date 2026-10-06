//! Computers a person forgot here. An introduction never trusts one of them
//! again; placing it by hand does, and takes it off the list.

use std::path::{Path, PathBuf};

use crate::config::ConfigError;

const FILE_NAME: &str = "forgotten";
/// The newest this many are kept, more than a layout holds.
const MAX_FORGOTTEN: usize = 64;

#[derive(Debug)]
pub struct Forgotten {
    path: PathBuf,
    /// Key fingerprints, oldest first.
    keys: Vec<String>,
}

impl Forgotten {
    /// The list in `state_dir`. A missing or unreadable file is an empty
    /// list, and a line that is not a fingerprint is skipped.
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(FILE_NAME);
        let keys = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter(|line| is_fingerprint(line))
            .map(str::to_owned)
            .collect();
        Self { path, keys }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn contains(&self, key: &str) -> bool {
        self.keys.iter().any(|known| known == key)
    }

    /// Adds `key`, as the newest.
    pub fn forget(&mut self, key: &str) -> Result<(), ConfigError> {
        self.keys.retain(|known| known != key);
        self.keys.push(key.to_owned());
        let excess = self.keys.len().saturating_sub(MAX_FORGOTTEN);
        self.keys.drain(..excess);
        self.save()
    }

    /// Takes `key` off the list, when a person places it again.
    pub fn remember(&mut self, key: &str) -> Result<(), ConfigError> {
        if !self.contains(key) {
            return Ok(());
        }
        self.keys.retain(|known| known != key);
        self.save()
    }

    fn save(&self) -> Result<(), ConfigError> {
        let mut text = self.keys.join("\n");
        text.push('\n');
        crate::config::save_text(&self.path, &text)
    }
}

fn is_fingerprint(line: &str) -> bool {
    line.len() == 64
        && line
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_survives_a_restart_and_keeps_the_newest() {
        let directory = tempfile::tempdir().unwrap();
        let key = |n: usize| format!("{n:064x}");
        let mut forgotten = Forgotten::load(directory.path());
        assert!(!forgotten.contains(&key(1)));
        forgotten.forget(&key(1)).unwrap();
        forgotten.forget(&key(2)).unwrap();
        let mut again = Forgotten::load(directory.path());
        assert!(again.contains(&key(1)) && again.contains(&key(2)));

        again.remember(&key(1)).unwrap();
        assert!(!Forgotten::load(directory.path()).contains(&key(1)));

        for n in 10..10 + MAX_FORGOTTEN {
            again.forget(&key(n)).unwrap();
        }
        let full = Forgotten::load(directory.path());
        assert!(!full.contains(&key(2)), "the oldest went first");
        assert!(full.contains(&key(10 + MAX_FORGOTTEN - 1)));

        std::fs::write(directory.path().join(FILE_NAME), "not a key\n").unwrap();
        assert!(Forgotten::load(directory.path()).keys.is_empty());
    }
}
