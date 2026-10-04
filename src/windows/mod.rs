//! Windows interactive-session engine and its per-user control channel.
mod clipboard;
mod desktop;
pub(crate) mod engine;
pub(crate) mod input;
pub(crate) mod ipc;
mod keys;
pub(crate) mod security;

use std::path::PathBuf;
pub fn data_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("LOCALAPPDATA").expect("Windows must provide LOCALAPPDATA"))
        .join("zflow")
}
pub fn config_path() -> PathBuf {
    data_dir().join("zflow.toml")
}
