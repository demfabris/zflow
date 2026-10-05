#![cfg(target_os = "linux")]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

fn fixture(path: &Path, name: &str, script: &str) {
    let file = path.join(name);
    fs::write(&file, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(file, fs::Permissions::from_mode(0o700)).unwrap();
}

fn command(path: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zflow"));
    command.env("PATH", path).arg("update");
    command
}

#[test]
fn check_is_read_only_and_reports_the_running_version() {
    let directory = tempfile::tempdir().unwrap();
    fixture(
        directory.path(),
        "curl",
        "printf https://github.com/demfabris/zflow/releases/tag/v999.0.0",
    );
    // No bash, sudo, pkexec or package manager exists on this command's PATH.
    let output = command(directory.path()).arg("check").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["current"], env!("CARGO_PKG_VERSION"));
    assert_eq!(result["latest"], "v999.0.0");
    assert_eq!(result["available"], true);
    assert_eq!(result["can_install"], false);
    assert!(result["reason"].as_str().unwrap().contains("built locally"));

    fixture(directory.path(), "pacman", "printf zflow-git");
    let output = command(directory.path()).arg("check").output().unwrap();
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["can_install"], false);
    assert!(result["reason"].as_str().unwrap().contains("pacman"));
}

#[test]
fn unavailable_or_invalid_releases_are_errors_without_installing() {
    let directory = tempfile::tempdir().unwrap();
    for script in [
        "printf offline >&2; exit 7",
        "printf https://github.com/another/zflow/releases/tag/v999.0.0",
        "printf https://github.com/demfabris/zflow/releases/tag/v999.0.0-rc.1",
        "printf https://github.com/demfabris/zflow/releases/latest",
    ] {
        fixture(directory.path(), "curl", script);
        let output = command(directory.path()).arg("check").output().unwrap();
        assert!(!output.status.success(), "{script}");
        assert!(
            output.stdout.is_empty(),
            "no successful status after {script}"
        );
    }
}

#[test]
fn install_rejects_downgrades_invalid_versions_and_development_copies() {
    let directory = tempfile::tempdir().unwrap();
    let same = format!("v{}", env!("CARGO_PKG_VERSION"));
    for tag in [
        "v0.0.0",
        same.as_str(),
        env!("CARGO_PKG_VERSION"),
        "v999.0.0-rc.1",
        "../main",
        "v999.0.0",
    ] {
        let output = command(directory.path())
            .args(["install", "--version", tag, "--gui"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{tag}");
        assert!(output.stdout.is_empty(), "no installer for {tag}");
    }
}
