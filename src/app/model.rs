use std::{fs, io::ErrorKind, path::PathBuf};

use anyhow::{Context, Result, bail};

use crate::{config::Config, session::SessionOptions};

pub struct ConfigDocument {
    pub path: PathBuf,
    pub draft: Config,
    saved: Config,
    disk_contents: Option<Vec<u8>>,
}

impl ConfigDocument {
    pub fn open(path: PathBuf) -> Result<Self> {
        let path = std::path::absolute(path).context("Could not resolve configuration path")?;
        let disk_contents = read_contents(&path)?;
        let draft = match &disk_contents {
            Some(bytes) => {
                let text = std::str::from_utf8(bytes).context("Configuration is not UTF-8")?;
                let config: Config = toml::from_str(text)
                    .with_context(|| format!("Could not parse {}", path.display()))?;
                config.validate()?;
                config
            }
            None => new_config(&path),
        };
        Ok(Self {
            path,
            saved: draft.clone(),
            draft,
            disk_contents,
        })
    }

    pub fn is_dirty(&self) -> bool {
        self.draft != self.saved
    }

    pub fn saved(&self) -> &Config {
        &self.saved
    }

    pub fn is_new(&self) -> bool {
        self.disk_contents.is_none()
    }

    pub fn validate(&self) -> Result<()> {
        self.draft.validate()?;
        SessionOptions::from_config(&self.draft)?;
        #[cfg(target_os = "linux")]
        crate::runtime::LinuxRuntimeConfig::from_config(&self.draft).validate()?;
        Ok(())
    }

    pub fn save(&mut self) -> Result<()> {
        self.validate()?;
        let next = toml::to_string_pretty(&self.draft)?;
        let next_contents = if let Some(bytes) = &self.disk_contents {
            let mut document = std::str::from_utf8(bytes)?.parse::<toml_edit::DocumentMut>()?;
            let before = toml::to_string_pretty(&self.saved)?.parse::<toml_edit::DocumentMut>()?;
            let after = next.parse::<toml_edit::DocumentMut>()?;
            merge_changes(document.as_table_mut(), before.as_table(), after.as_table());
            document.to_string()
        } else {
            next
        };
        if read_contents(&self.path)? != self.disk_contents {
            bail!(
                "{} changed on disk. Reload it before saving; keep a copy of your edits first.",
                self.path.display()
            );
        }
        crate::config::save_text(&self.path, &next_contents)?;
        self.disk_contents = Some(next_contents.into_bytes());
        self.saved = self.draft.clone();
        Ok(())
    }

    pub fn reload(&mut self) -> Result<()> {
        let replacement = Self::open(self.path.clone())?;
        *self = replacement;
        Ok(())
    }
}

// Compare complete configurations, but patch only changed fields into the user's document.
fn merge_changes(
    target: &mut dyn toml_edit::TableLike,
    before: &toml_edit::Table,
    after: &toml_edit::Table,
) {
    for (key, _) in before {
        if !after.contains_key(key) {
            target.remove(key);
        }
    }
    for (key, value) in after {
        if before
            .get(key)
            .is_some_and(|previous| items_equal(previous, value))
        {
            continue;
        }
        if let (Some(previous), Some(next)) =
            (before.get(key).and_then(|v| v.as_table()), value.as_table())
        {
            if !target.contains_key(key) {
                target.insert(key, toml_edit::Item::Table(toml_edit::Table::new()));
            }
            if let Some(table) = target.get_mut(key).and_then(|v| v.as_table_like_mut()) {
                merge_changes(table, previous, next);
                continue;
            }
        }
        let mut replacement = value.clone();
        if let (Some(old), Some(new)) = (
            target.get(key).and_then(|v| v.as_value()),
            replacement.as_value_mut(),
        ) {
            *new.decor_mut() = old.decor().clone();
        }
        target.insert(key, replacement);
    }
}

fn items_equal(left: &toml_edit::Item, right: &toml_edit::Item) -> bool {
    match (left.as_table(), right.as_table()) {
        (Some(left), Some(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right
                        .get(key)
                        .is_some_and(|other| items_equal(value, other))
                })
        }
        (None, None) => left.to_string() == right.to_string(),
        _ => false,
    }
}

fn new_config(path: &std::path::Path) -> Config {
    let mut config = Config::default();
    if cfg!(target_os = "macos") {
        let directory = path.parent().expect("absolute configuration path");
        config.daemon.state_dir = directory.join("state");
        config.daemon.control_socket = directory.join("control.sock");
    }
    config
}

fn read_contents(path: &std::path::Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Could not read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DeviceSelector, PeerConfig, PeerPermissions};

    #[test]
    fn gui_changes_preserve_inline_table_comments() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let original = "macos = { sharing = true, block_awdl = false } # radio\n";
        fs::write(&path, original).unwrap();
        let mut document = ConfigDocument::open(path.clone()).unwrap();
        document.draft.macos.block_awdl = true;
        document.save().unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original.replace("block_awdl = false", "block_awdl = true")
        );
    }

    #[test]
    fn gui_changes_preserve_comments_and_unrelated_toml() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let original = "# My desk\nversion = 1\n\n[transport] # network\ncheckpoint_ms = 25 # keep this\n\n[macos]\nblock_awdl = false # radio\n";
        fs::write(&path, original).unwrap();
        let mut document = ConfigDocument::open(path.clone()).unwrap();
        document.draft.macos.block_awdl = true;
        document.save().unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original.replace("block_awdl = false", "block_awdl = true")
        );
        assert!(Config::load(&path).unwrap().macos.block_awdl);
    }

    #[test]
    fn missing_file_stays_missing_until_explicit_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("new/zflow.toml");
        let mut document = ConfigDocument::open(path.clone()).unwrap();
        assert!(document.is_new());
        assert!(!document.is_dirty());
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
        document.save().unwrap();
        assert!(!document.is_new());
        assert!(!document.is_dirty());
        assert_eq!(Config::load(&path).unwrap(), new_config(&path));
    }

    #[test]
    fn save_preserves_peer_identity_and_device_attributes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let mut original = Config::default();
        original.peers.insert(
            "ubuntu".into(),
            PeerConfig::from_spki(
                b"test peer identity",
                vec!["192.0.2.1:43119".parse().unwrap()],
                PeerPermissions {
                    connect: true,
                    receive_normal: true,
                    ..PeerPermissions::default()
                },
            )
            .unwrap(),
        );
        original.input.capture_devices.push(DeviceSelector {
            path: "/dev/input/event1".into(),
            name: Some("Trackpad".into()),
            phys: Some("test/input0".into()),
            vendor: Some(0x05ac),
            product: Some(0x030e),
        });
        original.save(&path).unwrap();
        let mut document = ConfigDocument::open(path.clone()).unwrap();
        document.draft.playout.fixed_delay_ms = 12;
        assert!(document.is_dirty());
        document.save().unwrap();
        assert!(!document.is_dirty());
        let saved = Config::load(&path).unwrap();
        assert_eq!(saved.peers, original.peers);
        assert_eq!(saved.input.capture_devices, original.input.capture_devices);
        assert_eq!(saved.playout.fixed_delay_ms, 12);
    }

    #[test]
    fn invalid_save_leaves_disk_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        Config::default().save(&path).unwrap();
        let before = fs::read(&path).unwrap();
        let mut document = ConfigDocument::open(path.clone()).unwrap();
        document.draft.transport.lease_ms = 1001;
        assert!(document.save().is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        document.draft.transport.lease_ms = 900;
        document.draft.playout.fixed_delay_ms = u64::MAX;
        assert!(document.draft.validate().is_ok());
        assert!(document.save().is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn external_edit_or_deletion_prevents_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        Config::default().save(&path).unwrap();
        let mut document = ConfigDocument::open(path.clone()).unwrap();
        document.draft.transport.checkpoint_ms = 100;
        let externally_edited = b"# outside edit\nversion = 1\n";
        fs::write(&path, externally_edited).unwrap();
        assert!(document.save().is_err());
        assert_eq!(fs::read(&path).unwrap(), externally_edited);
        fs::remove_file(&path).unwrap();
        assert!(document.save().is_err());
        assert!(!path.exists());
        assert!(document.is_dirty());
    }

    #[test]
    fn external_creation_prevents_new_file_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let mut document = ConfigDocument::open(path.clone()).unwrap();
        let outside = b"# created elsewhere\nversion = 1\n";
        fs::write(&path, outside).unwrap();
        assert!(document.save().is_err());
        assert_eq!(fs::read(&path).unwrap(), outside);
        assert!(document.is_new());
    }

    #[test]
    fn malformed_file_cannot_open_or_replace_current_draft_on_reload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        Config::default().save(&path).unwrap();
        let mut document = ConfigDocument::open(path.clone()).unwrap();
        document.draft.transport.checkpoint_ms = 100;
        let malformed = b"version = [unfinished";
        fs::write(&path, malformed).unwrap();
        assert!(ConfigDocument::open(path.clone()).is_err());
        assert!(document.reload().is_err());
        assert_eq!(document.draft.transport.checkpoint_ms, 100);
        assert!(document.is_dirty());
        assert!(document.save().is_err());
        assert_eq!(fs::read(&path).unwrap(), malformed);
    }

    #[test]
    fn reload_replaces_draft_and_save_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        Config::default().save(&path).unwrap();
        let mut document = ConfigDocument::open(path.clone()).unwrap();
        document.draft.transport.checkpoint_ms = 100;
        let mut outside = Config::default();
        outside.transport.checkpoint_ms = 200;
        outside.save(&path).unwrap();
        document.reload().unwrap();
        assert_eq!(document.draft, outside);
        assert!(!document.is_dirty());
        document.draft.transport.checkpoint_ms = 150;
        document.save().unwrap();
        assert_eq!(Config::load(&path).unwrap().transport.checkpoint_ms, 150);
    }
}
