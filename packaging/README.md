# Installation

The root `install.sh` downloads release binaries for the detected OS and CPU,
verifies the selected file against that release's `SHA256SUMS`, and installs it.
It resolves `latest` to a specific tag before downloading either file. Use
`--version v0.1.0` to select a release. A failed download, missing checksum, or
unsupported platform stops installation before requesting administrator access.
On Linux, every system change then runs in one elevated shell. It copies the
download to a root-owned directory and checks the checksum again before
installing, so no process running as the user can swap files mid-install.

## Release artifacts

`.github/workflows/release.yml` builds these artifacts on native runners:

| Platform | Runner | Artifact |
| --- | --- | --- |
| Linux x86-64 | Ubuntu 24.04 | `zflow-vVERSION-x86_64-unknown-linux-gnu.tar.gz`, `zflow_VERSION_amd64.deb` |
| Linux ARM64 | Ubuntu 24.04 ARM | `zflow-vVERSION-aarch64-unknown-linux-gnu.tar.gz`, `zflow_VERSION_arm64.deb` |
| macOS Apple Silicon and Intel | macOS 26 | `zflow-vVERSION-macos.dmg`, `zflow-vVERSION-universal-apple-darwin.tar.gz` |

Linux uses Ubuntu 24.04 to keep the glibc floor at 2.39. The Mac job builds the
Rust library for both CPUs, merges the two with `lipo`, and builds the Swift
package for arm64 and x86_64, so both Mac files hold one universal app. The disk
image is for people; `install.sh` uses the archive. Releases up to v0.1.0 had
one Mac archive per CPU, and `install.sh` falls back to those names when a
release lists no universal archive. Rust is pinned in the workflow; the app's
minimum macOS version remains 26. After all builds and tests
pass, the publish job combines the artifacts, computes `SHA256SUMS`, uploads a
draft release, and publishes it. It refuses to replace an existing release.
The release also contains `install.sh`. Checksums detect corrupt or mismatched
downloads; they rely on the same GitHub/HTTPS trust as the artifacts.

To build without publishing, dispatch the **Release** workflow with `publish`
left off. To publish, update Cargo.toml/Cargo.lock to the intended version,
commit and push, then either push its matching `vVERSION` tag or dispatch from
`main` with `publish=true`. A dispatch from another branch builds but never
publishes. Manual publication creates the tag at the workflow's commit.
Prerelease version suffixes produce GitHub prereleases, which users select
with `--version`; the default `latest` selects a normal published release.

The release workflow signs the universal app with Developer ID, submits it to
Apple, and staples the accepted notarization ticket. It then packs the stapled
app into the disk image with `scripts/build-macos-dmg.sh`, signs the image, and
notarizes and staples the image too. It checks signatures, tickets, and
Gatekeeper again on the final archive and disk image. A signing or notarization
failure blocks publication, including manual builds with `publish` off.

The disk image holds `zflow.app` and a link to `/Applications`, and opens in
Finder's default view. It is plain `hdiutil` output, with no background picture
or saved icon positions, so it adds no build dependency.

Configure these GitHub Actions repository secrets before running the workflow:

| Secret | Value |
| --- | --- |
| `MACOS_CERTIFICATE_BASE64` | Base64-encoded `.p12` containing the Developer ID Application certificate and private key |
| `MACOS_CERTIFICATE_PASSWORD` | Password protecting that `.p12` |
| `MACOS_SIGNING_IDENTITY` | SHA-1 fingerprint shown by `security find-identity -v -p codesigning` |
| `APPLE_ID` | Apple Account email address |
| `APPLE_APP_SPECIFIC_PASSWORD` | App-specific password generated for that account |
| `APPLE_TEAM_ID` | Developer team ID matching the certificate |

The Mac job builds and tests the app with an ad-hoc signature first, so no
build script runs while the certificate is available. It then imports the
certificate into a temporary keychain, signs with `scripts/sign-macos-app.sh`,
notarizes, builds and notarizes the disk image, and deletes the keychain before
packaging, also on failure. Credentials stay in GitHub secrets and the runner's
temporary keychain. Notarization JSON results remain in separate workflow
artifacts for seven days; they do not enter the published release.

For a local signed and notarized build (`--universal` needs
`rustup target add aarch64-apple-darwin x86_64-apple-darwin`):

```sh
./scripts/build-macos-app.sh --universal --sign 'Developer ID Application: NAME (TEAM_ID)'
xcrun notarytool store-credentials zflow-notary --team-id TEAM_ID
./scripts/notarize-macos-app.sh zflow-notary
./scripts/build-macos-dmg.sh target/release/zflow.app target/dist/zflow-vVERSION-macos.dmg \
  --sign 'Developer ID Application: NAME (TEAM_ID)'
./scripts/notarize-macos-app.sh zflow-notary '' target/dist/zflow-vVERSION-macos.dmg
./scripts/package-release.sh universal-apple-darwin
```

The notarization script takes an optional keychain path as its second argument
(empty for your default keychains) and a disk image to notarize instead of the
app as its third. It waits up to 20 minutes. If Apple takes longer, the job fails
and retains the submission ID in `target/notarization/submission.json`
(`target/notarization/dmg/` for the disk image); inspect that submission with
`xcrun notarytool info ID --keychain-profile PROFILE` before submitting again.
Rebuilding or signing the app again requires another notarization and a new
disk image.

Local artifact assembly after building the native release binaries:

```sh
./scripts/package-release.sh x86_64-unknown-linux-gnu
./scripts/build-deb.sh   # Linux; requires build-essential and debhelper
```

Both commands package existing binaries under `target/release`; neither builds
them. Substitute the native target from the table for the archive command; on a
Mac that is `universal-apple-darwin`, which requires an app built with
`--universal`. Outputs go under `target/dist`.

## Debian packages

The package owns `/usr/bin/zflow`, `/usr/bin/zflowd`, vendor systemd/udev files
and `/usr/lib/zflow` helper scripts under `/usr/lib`, and the GNOME extension,
application launcher, and D-Bus activation file under `/usr/share`. It also
ships `/etc/xdg/autostart/io.zflow.desktop-agent.desktop`, which starts the
desktop agent in every GNOME session. debhelper handles service lifecycle and
respects `policy-rc.d`. Configuration and capture rules are generated only when
absent; updates keep daemon-written settings and pairing state.

On configure, `/usr/lib/zflow/firewall.sh open` allows UDP 43119 and 43120 when
ufw is active (application profile `/etc/ufw/applications.d/zflow`) or firewalld
is running (service `/etc/firewalld/services/zflow.xml` in the default zone). It
prints what it opened and never turns a firewall on. apt runs it again on every
upgrade, so a removed rule comes back with the next update.

`apt remove zflow` stops the service, closes those firewall rules, and moves
selected-device rules out of udev's active directory, retaining them for
reinstallation. dpkg keeps the autostart entry as a conffile until purge;
`TryExec` stops it from running without `/usr/bin/zflow`. `apt purge zflow`
also removes the autostart entry, generated configuration and device
selections. Both retain `/var/lib/zflow` identities and the locked system
account. Neither touches home folders, so a GNOME extension installed from
extensions.gnome.org stays until the user removes it.

An archive/source installation owns `/usr/local` binaries and `/etc` service
files that would shadow a Debian installation. The curl installer keeps using
archives for such a machine. To migrate deliberately, stop sharing, save
`/etc/udev/rules.d/71-zflow-capture.rules`, run the archive/source uninstaller
without `--purge`, install the `.deb`, then restore the capture rules and reload
udev. Configuration and identity remain in place. Then run
`zflow desktop-agent --install` as each desktop user: it removes the per-user
launcher, D-Bus file and extension copy that older versions wrote, which would
shadow the package's files.
The `.deb` rejects a remaining `/usr/local` installation before unpacking.

## Linux service

For development, `scripts/install.sh` builds both headless binaries and installs them under
`/usr/local/bin`. It creates a locked `zflow` account, loads `uinput`, installs
the service, udev rules, and a system-sleep hook, then starts `zflowd`. The
account, configuration, capture-rule, and uinput steps live in
`packaging/linux/host-setup.sh`, which the Debian package's postinst also runs.
It also installs the launcher and D-Bus activation file under
`/usr/local/share` and the autostart entry under `/etc/xdg/autostart`, with
`/usr/local/bin/zflow` in place of `/usr/bin/zflow`. The session bus and GNOME
search `/usr/local/share` through the default `XDG_DATA_DIRS`. The firewall step
is the same `firewall.sh` the package uses, kept at
`/usr/local/lib/zflow/firewall.sh` for the uninstaller.

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
`network-online.target`. `zflow setup --prelogin on` adds
`/etc/systemd/system/zflowd.service.d/prelogin.conf`, which orders daemon
readiness before the display manager; `--prelogin off` removes it. Pre-login
injection also needs the per-peer permission. Archive upgrades keep the drop-in
only while the configuration enables pre-login input. Purging the package or
running the uninstaller deletes it. The sleep hook stops an active daemon
before suspend and starts a fresh process after resume. This releases every
evdev grab and discards stale sessions and clock state.

The installer copies the uninstaller to `/usr/local/lib/zflow/uninstall.sh`.
It keeps configuration, identity state, and the service account, removes the
session files and firewall rules, and removes the capture rule to revoke
event-device access:

```sh
sudo /usr/local/lib/zflow/uninstall.sh
```

Use `--purge` only when you also want to delete those files and the account.

## Native macOS app

`scripts/build-macos-app.sh [--debug] [--universal] [--dmg] [--sign IDENTITY]`
builds the Rust static library and Swift package, then assembles
`target/{debug,release}/zflow.app`. It builds for the current CPU unless
`--universal` asks for Apple silicon and Intel in one app. `--dmg`, or `just dmg`,
also packs `zflow.dmg` beside the app, signed with the app's identity unless that
is ad hoc, and never notarized.
The app requires macOS 26 and Swift 6.2 or newer. It uses SwiftUI Settings and
MenuBarExtra with no Dock icon. The bundle contains the AWDL daemon and its
SMAppService launchd plist. All executables use hardened runtime signatures.
Without `--sign`, the script signs with the first Apple Development identity
in your keychain, so the Accessibility grant survives rebuilds. With no such
identity, or with `--sign -`, the build is ad hoc and macOS treats every
rebuild as a new app that needs a fresh grant.

Use an Apple-issued signing identity to install the privileged helper through
the app. The app talks to the daemon over XPC directly; each side requires the
same team and the other's exact identifier (`io.zflow.zflow` and
`io.zflow.awdl-daemon`). Ad hoc
builds leave AWDL installation unavailable and can still run input sharing.
The build script does not install services, launch the app, or notarize it.

## Linux desktop session

Both install paths give every GNOME user the launcher, the D-Bus activation
file for `io.zflow.Desktop`, and an autostart entry for the desktop agent. The
curl installer then runs this as the desktop user, and so can you:

```sh
zflow desktop-agent --install
```

It sets up the GNOME extension and starts the agent in the current session.
First it removes the per-user launcher and D-Bus file that older versions
wrote, since they would shadow the system files. Then:

1. If GNOME Shell has not loaded the extension this session, it calls
   `org.gnome.Shell.Extensions.InstallRemoteExtension("zflow@demfabris")`.
   GNOME shows its own dialog, downloads the extension from
   extensions.gnome.org, and loads it at once. GNOME 45 and later load a new
   extension without a logout only this way.
2. Otherwise, or when that fails (not on extensions.gnome.org yet, offline, the
   `allow-extension-installation` policy, no version for this GNOME, or Cancel),
   it falls back to the bundled copy. A `.deb` already has it under
   `/usr/share/gnome-shell/extensions`, and an older per-user copy is removed so
   it can't shadow that one. An archive install writes it to
   `~/.local/share/gnome-shell/extensions`, as before. A copy that came from
   extensions.gnome.org (its `metadata.json` has `_generated`) is left to
   GNOME's own updates in both cases.
3. It adds the extension to `org.gnome.shell enabled-extensions` with
   `gsettings`, because `gnome-extensions enable` refuses an extension that
   Shell has not loaded yet. It prints "Log out and back in" when GNOME only
   loads it at the next login.
4. It starts the agent through D-Bus activation, or directly if the bus
   cannot, so the Mac can connect without waiting for the next login.

Opening zflow runs `zflow settings`. Its window shows a banner until the
extension runs: **Install** (asks the agent to do the steps above), **Turn On**
(for an extension GNOME loaded but that is off), or **Log Out** (for files GNOME
has not loaded yet; GNOME asks to confirm). The banner lives in `setup.js`,
outside the extension, because extensions.gnome.org discourages extensions that
install or enable extensions.

The extension adds a panel indicator with sharing and settings actions. Its
preferences and the standalone app use the same GTK4/libadwaita controls.
Install GJS, GTK 4.12+ and libadwaita 1.5+ for the window. The service and agent
remain usable without the GTK runtime.

The agent reconnects to the system service and serves desktop requests
without opening a window. It obtains public
peer/discovery settings through the credential-checked desktop API. It does not
read the protected system configuration directly. For a foreground agent, run
`zflow desktop-agent` and stop it with Ctrl+C.
Closing the settings window keeps the agent running. **Start at Login** is on
while the system autostart entry applies. Turning it off writes
`~/.config/autostart/io.zflow.desktop-agent.desktop` with `Hidden=true`, the
XDG way to override a system entry; turning it on again deletes that file.
Opening settings can still start the agent on demand. Panel status checks do
not activate a stopped agent.

The extension and the agent can come from different releases, since
extensions.gnome.org updates the extension on its own schedule. Both carry an
API level (`API` in `src/app/gnome.rs` and `packaging/gnome-extension/client.js`).
The agent sends its level with every request, and the extension refuses a
different one with a reason that starts with "Update zflow". The panel compares
the level in the agent's snapshot and shows **Update zflow**. Raise both only
when the agent and the extension stop understanding each other.

The system uninstaller and package removal do not touch home folders. The
GNOME extension, a `Hidden=true` autostart override, and the settings window
assets in `$XDG_CACHE_HOME/zflow/desktop` (default `~/.cache`) stay in each
account.

## GNOME extension on extensions.gnome.org

`scripts/pack-extension.sh` (or `just pack-extension`) runs
`gnome-extensions pack` and writes
`target/dist/zflow@demfabris.shell-extension.zip`. It needs GNOME Shell's
`gnome-extensions` tool. The zip holds exactly the files that
`desktop-agent --install` writes and the `.deb` installs: `metadata.json`,
`extension.js`, `indicator.js`, `client.js`, `settings.js` and `prefs.js`.
`app.js` and `setup.js` belong to the standalone window only.
`tests/install_test.py` checks that the three lists match.

`metadata.json` has no `version`: extensions.gnome.org sets it on upload. That
also keeps GNOME's update check away from a bundled copy, which has no version
to compare. List only released GNOME versions in `shell-version`.

Upload the zip at <https://extensions.gnome.org/upload/> while signed in with
the account that owns the `zflow@demfabris` UUID. Each upload becomes a new
version that waits for review. Reviewers follow the
[review guidelines](https://gjs.guide/extensions/review-guidelines/review-guidelines.html),
and expect the author to explain the code. Upload a new zip whenever a release
changes a file in the list above, or supports a new GNOME version. When a
release raises the API level, get the new extension approved before publishing
the release, so users do not see **Update zflow** in between.
