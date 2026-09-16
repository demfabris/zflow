# Installation

The root `install.sh` downloads release binaries for the detected OS and CPU,
verifies the selected file against that release's `SHA256SUMS`, and installs it.
It resolves `latest` to a specific tag before downloading either file. Use
`--version v0.1.0` to select a release. A failed download, missing checksum, or
unsupported platform stops installation before requesting administrator access.

## Release artifacts

`.github/workflows/release.yml` builds these artifacts on native runners:

| Platform | Runner | Artifact |
| --- | --- | --- |
| Linux x86-64 | Ubuntu 24.04 | `zflow-vVERSION-x86_64-unknown-linux-gnu.tar.gz`, `zflow_VERSION_amd64.deb` |
| Linux ARM64 | Ubuntu 24.04 ARM | `zflow-vVERSION-aarch64-unknown-linux-gnu.tar.gz`, `zflow_VERSION_arm64.deb` |
| macOS Apple Silicon | macOS 26 | `zflow-vVERSION-aarch64-apple-darwin.tar.gz` |
| macOS Intel | macOS 26 Intel | `zflow-vVERSION-x86_64-apple-darwin.tar.gz` |

Linux uses Ubuntu 24.04 to keep the glibc floor at 2.39. Rust is pinned in the
workflow; the app's minimum macOS version remains 26. After all builds and tests
pass, the publish job combines the artifacts, computes `SHA256SUMS`, uploads a
draft release, and publishes it. It refuses to replace an existing release.
The release also contains `install.sh`. Checksums detect corrupt or mismatched
downloads; they rely on the same GitHub/HTTPS trust as the artifacts.

To build without publishing, dispatch the **Release** workflow with `publish`
left off. To publish, update Cargo.toml/Cargo.lock to the intended version,
commit and push, then either push its matching `vVERSION` tag or dispatch with
`publish=true`. Manual publication creates the tag at the workflow's commit.
Prerelease version suffixes produce GitHub prereleases, which users select
with `--version`; the default `latest` selects a normal published release.

Release Mac apps currently use ad hoc signatures. They are not notarized, and
the optional AWDL helper remains unavailable. To build a signed app locally,
use `scripts/build-macos-app.sh --sign IDENTITY` before packaging. Signing and
notarization credentials are not stored in the repository.

Local artifact assembly after building the native release binaries:

```sh
./scripts/package-release.sh x86_64-unknown-linux-gnu
./scripts/build-deb.sh   # Linux; requires build-essential and debhelper
```

Both commands package existing binaries under `target/release`; neither builds
them. Substitute the native target from the table for the archive command.
Outputs go under `target/dist`.

## Debian packages

The package owns `/usr/bin/zflow`, `/usr/bin/zflowd`, vendor systemd/udev files
under `/usr/lib`, and the GNOME extension, application launcher, and D-Bus entry
under `/usr/share`. debhelper handles service lifecycle and respects
`policy-rc.d`. Configuration and capture rules are generated only when absent;
updates keep daemon-written settings and pairing state.

`apt remove zflow` stops the service and moves selected-device rules out of
udev's active directory, retaining them for reinstallation. `apt purge zflow`
also removes generated configuration and device selections. Both retain
`/var/lib/zflow` identities and the locked system account. Per-user autostart
and desktop files, if created previously, stay in that user's account.

An archive/source installation owns `/usr/local` binaries and `/etc` service
files that would shadow a Debian installation. The curl installer keeps using
archives for such a machine. To migrate deliberately, stop sharing, save
`/etc/udev/rules.d/71-zflow-capture.rules`, run the archive/source uninstaller
without `--purge`, install the `.deb`, then restore the capture rules and reload
udev. Configuration and identity remain in place. Remove the old per-user
launcher, D-Bus service and extension listed below so the package's global
files take effect; update any autostart entry to use `/usr/bin/zflow`.
The `.deb` rejects a remaining `/usr/local` installation before unpacking.

## Linux service

For development, `scripts/install.sh` builds both headless binaries and installs them under
`/usr/local/bin`. It creates a locked `zflow` account, loads `uinput`, installs
the service, udev rules, and a system-sleep hook, then starts `zflowd`.

Binary archives contain the same installer and Linux service assets alongside
`bin/zflow` and `bin/zflowd`. Their installer runs with `--install-built` and
needs no source checkout or compiler.

The installer keeps two administrator-managed files on upgrades:

- `/etc/zflow/zflow.toml`
- `/etc/udev/rules.d/71-zflow-capture.rules`

`zflow setup` maintains the capture rule with stable device attributes. Each
selected event node receives group `zflow` and mode `0640`. The service account
does not join the broad `input` group. Pass every selected device in one setup
command by repeating `--device`.

Run the installer from the repository root:

```sh
./scripts/install.sh
sudo /usr/local/bin/zflow devices
sudo /usr/local/bin/zflow setup \
  --device /dev/input/eventX \
  --device /dev/input/eventY \
  --udev-rules /etc/udev/rules.d/71-zflow-capture.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --action=change --subsystem-match=input
sudo /usr/local/bin/zflow doctor
```

The default service starts at `multi-user.target` and does not wait for
`network-online.target`. Its drop-in orders daemon readiness before the display
manager, but pre-login injection remains blocked by the global config and
per-peer permission gates. The sleep hook stops an active daemon before
suspend and starts a fresh process after resume. This releases every evdev
grab and discards stale sessions and clock state.

The normal uninstaller keeps configuration, identity state, and the service
account. It removes the capture rule to revoke event-device access:

```sh
sudo ./scripts/uninstall.sh
```

Use `--purge` only when you also want to delete those files and the account.

## Native macOS app

`scripts/build-macos-app.sh [--debug] [--sign IDENTITY]` builds the Rust static
library and Swift package, then assembles `target/{debug,release}/zflow.app`.
The app requires macOS 26 and Swift 6.2 or newer. It uses SwiftUI Settings and
MenuBarExtra with no Dock icon. The bundle contains the AWDL client, daemon,
and SMAppService launchd plist. All executables use hardened runtime signatures.

Use an Apple-issued signing identity to install the privileged helper through
the app. XPC requires matching teams and exact client/daemon identifiers. Ad hoc
builds leave AWDL installation unavailable and can still run input sharing.
The build script does not install services, launch the app, or notarize it.

## Linux desktop session

After installing the system service, run as the logged-in desktop user:

```sh
zflow desktop-agent --install
zflow settings
```

The install command writes the GNOME extension, the Applications launcher,
a D-Bus activation entry for `io.zflow.Desktop`, and
`$XDG_CONFIG_HOME/autostart/io.zflow.desktop-agent.desktop` (falling back to
`~/.config/autostart`). Log out and back in after installing or updating the
extension so GNOME loads the new code. The extension adds a panel indicator with sharing and settings actions.
Its preferences and the standalone app use the same GTK4/libadwaita controls.
Install GJS, GTK 4.12+ and libadwaita 1.5+ for the window. The service and agent
remain usable without the GTK runtime.

The agent reconnects to the system service, serves desktop requests,
and advertises display dimensions without opening a window. It obtains public
peer/discovery settings through the credential-checked desktop API. It does not
read the protected system configuration directly. For a foreground agent, run
`zflow desktop-agent` and stop it with Ctrl+C.
Closing the settings window keeps the agent running. **Start at Login**
controls the autostart entry; opening settings can still start it on demand.
Panel status checks do not activate a stopped agent.

The system uninstaller does not delete per-user extension or autostart files.
Remove those from the desktop account when removing desktop integration, plus
`$XDG_DATA_HOME/applications/io.zflow.zflow.desktop`,
`$XDG_DATA_HOME/dbus-1/services/io.zflow.Desktop.service`, and
`$XDG_CACHE_HOME/zflow/desktop`. The data/cache defaults are `~/.local/share`
and `~/.cache`.
