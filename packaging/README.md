# Installation

The root `install.sh` is the cross-platform source installer used by the curl
command in the main README. It downloads a source archive, installs prerequisites
when needed, and calls the platform scripts below. Use `./install.sh --source .`
to install a checkout, or add `--skip-dependencies` to manage prerequisites
yourself. `--headless` skips GNOME setup on Linux. On macOS, `--sign IDENTITY`
selects an installed Apple signing identity; the default is an ad hoc signature.
The installer does not publish or notarize builds.

## Linux service

`scripts/install.sh` builds both headless binaries and installs them under
`/usr/local/bin`. It creates a locked `zflow` account, loads `uinput`, installs
the service, udev rules, and a system-sleep hook, then starts `zflowd`.

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
