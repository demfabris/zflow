//! Release checks and the Linux install transaction used by GTK settings.
use std::{path::Path, process::Command};

use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use serde::Serialize;

const RELEASES: &str = "https://github.com/demfabris/zflow/releases/";
const INSTALLER: &str = include_str!("../../install.sh");

#[derive(Serialize)]
struct Check {
    current: &'static str,
    latest: String,
    available: bool,
    can_install: bool,
    reason: Option<String>,
}

/// Only stable tags produced by the release workflow are automatic updates.
fn release_version(tag: &str) -> Result<Version> {
    let version = Version::parse(tag.strip_prefix('v').context("Invalid release tag")?)?;
    ensure!(
        version.pre.is_empty() && version.build.is_empty(),
        "Automatic updates require a stable release"
    );
    Ok(version)
}

fn newer(current: &str, tag: &str) -> Result<bool> {
    Ok(release_version(tag)?
        .cmp_precedence(&Version::parse(current)?)
        .is_gt())
}

fn release_tag(url: &str) -> Result<String> {
    let tag = url
        .strip_prefix(RELEASES)
        .and_then(|path| path.strip_prefix("tag/"))
        .context("GitHub returned an unexpected release address")?;
    release_version(tag)?;
    Ok(tag.to_owned())
}

fn latest() -> Result<String> {
    let output = Command::new("curl")
        .args([
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--silent",
            "--show-error",
            "--head",
            "--location",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "--output",
            "/dev/null",
            "--write-out",
            "%{url_effective}",
        ])
        .arg(format!("{RELEASES}latest"))
        .output()
        .context("Install curl to check for zflow updates")?;
    ensure!(
        output.status.success(),
        "Could not check for updates. Check your internet connection and try again. {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    release_tag(std::str::from_utf8(&output.stdout)?.trim())
}

fn owns(program: &str, arguments: &[&str], executable: &Path) -> Option<String> {
    let output = Command::new(program)
        .args(arguments)
        .arg(executable)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn install_reason(path: &Path, owner: Option<(&str, &str)>) -> Option<String> {
    match (path.to_str(), owner) {
        (Some("/usr/bin/zflow"), Some(("dpkg", "zflow")))
        | (Some("/usr/local/bin/zflow"), None) => None,
        (_, Some((manager, package))) => Some(format!(
            "Update {package} through {manager}, which manages this installation."
        )),
        _ => Some(
            "This copy was built locally. Install a zflow release to enable updates here.".into(),
        ),
    }
}

fn installation() -> Result<Option<String>> {
    let path = std::env::current_exe()?;
    // Ask who owns the executable, rather than assuming that a package manager
    // installed on this computer owns zflow too.
    for (program, arguments, manager) in [
        ("rpm", &["-qf", "--qf", "%{NAME}"][..], "rpm"),
        ("pacman", &["-Qqo"][..], "pacman"),
    ] {
        if let Some(package) = owns(program, arguments, &path) {
            return Ok(install_reason(&path, Some((manager, &package))));
        }
    }
    let package = owns("dpkg-query", &["-S"], &path);
    let package = package
        .as_deref()
        .and_then(|line| line.split_once(": "))
        .and_then(|(name, _)| name.split(':').next());
    Ok(install_reason(&path, package.map(|name| ("dpkg", name))))
}

pub fn check() -> Result<()> {
    let tag = latest()?;
    let reason = installation()?;
    let value = Check {
        current: env!("CARGO_PKG_VERSION"),
        available: newer(env!("CARGO_PKG_VERSION"), &tag)?,
        latest: tag,
        can_install: reason.is_none(),
        reason,
    };
    println!("{}", serde_json::to_string(&value)?);
    Ok(())
}

pub fn install(tag: &str, gui: bool) -> Result<()> {
    ensure!(
        !nix::unistd::Uid::effective().is_root(),
        "Run updates as your desktop user, without sudo"
    );
    ensure!(
        newer(env!("CARGO_PKG_VERSION"), tag)?,
        "This release is not newer than the installed version"
    );
    if let Some(reason) = installation()? {
        bail!("{reason}");
    }
    // Run the install code bundled with this binary. The only downloads are
    // the selected release and its checksums, both from the fixed repository.
    let mut command = Command::new("bash");
    command.args([
        "-c",
        INSTALLER,
        "zflow-update",
        "--update",
        env!("CARGO_PKG_VERSION"),
        "--no-launch",
        "--version",
        tag,
    ]);
    if gui {
        command.arg("--gui");
    }
    let status = command
        .status()
        .context("Could not start the zflow installer")?;
    if status.code() == Some(126) {
        bail!("Update cancelled. Nothing was installed.");
    }
    ensure!(
        status.success(),
        "The update did not finish. See the installer message above and try again."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_follow_semver_and_only_use_stable_tags() {
        for (current, tag, expected) in [
            ("0.5.0", "v0.5.1", true),
            ("0.9.0", "v0.10.0", true),
            ("0.5.0", "v0.5.0", false),
            ("0.5.1", "v0.5.0", false),
            ("0.5.0-rc.1", "v0.5.0", true),
            ("0.6.0-rc.1", "v0.5.0", false),
            ("0.5.0+local", "v0.5.0", false),
        ] {
            assert_eq!(newer(current, tag).unwrap(), expected, "{current} -> {tag}");
        }
        for tag in [
            "0.5.1",
            "v0.5",
            "v01.5.1",
            "v0.5.1-rc.1",
            "v0.5.1+build",
            "v0.5.1/other",
            "v0.5.1?query",
            "v0.5.1\n",
        ] {
            assert!(newer("0.5.0", tag).is_err(), "{tag}");
        }
    }

    #[test]
    fn only_the_projects_release_redirect_is_accepted() {
        assert_eq!(
            release_tag(&format!("{RELEASES}tag/v0.6.0")).unwrap(),
            "v0.6.0"
        );
        for url in [
            "http://github.com/demfabris/zflow/releases/tag/v0.6.0",
            "https://github.com/other/zflow/releases/tag/v0.6.0",
            "https://github.com/demfabris/zflow/releases/latest",
            "https://github.com/demfabris/zflow/releases/tag/v0.6.0/download",
        ] {
            assert!(release_tag(url).is_err(), "{url}");
        }
    }

    #[test]
    fn updates_preserve_package_ownership() {
        assert!(install_reason(Path::new("/usr/local/bin/zflow"), None).is_none());
        assert!(install_reason(Path::new("/usr/bin/zflow"), Some(("dpkg", "zflow"))).is_none());
        for (path, owner) in [
            ("/usr/bin/zflow", None),
            ("/tmp/zflow", None),
            ("/usr/bin/zflow", Some(("dpkg", "another-package"))),
            ("/usr/bin/zflow", Some(("pacman", "zflow"))),
            ("/usr/local/bin/zflow", Some(("rpm", "zflow"))),
            ("/usr/local/bin/zflow", Some(("dpkg", "zflow"))),
        ] {
            assert!(install_reason(Path::new(path), owner).is_some(), "{path}");
        }
    }
}
