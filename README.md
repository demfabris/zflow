# zflow

zflow is a headless input-sharing service for Linux. It captures an explicit
set of physical evdev devices, sends bounded input state over authenticated
QUIC, and injects it through stable uinput devices on the other machine.

This is a working prototype, not yet a qualified alpha. The supported Linux
launch path is the packaged systemd service. The desktop app provides pairing
and experimental Mac-to-GNOME edge switching. The Mac sends input; it cannot
receive input from Linux. The two-host qualification run in `TESTPLAN.md`
still gates the alpha label.

## Mac → Ubuntu setup

Build the Mac app with `./scripts/build-macos-app.sh`, then open
`target/release/zflow.app`. On Ubuntu, install the updated service and app with
`./scripts/install.sh --gui`, then open zflow from the application menu.
Building the desktop app requires Rust 1.95 or newer. The Mac build uses an
ad-hoc signature for local development; `--sign IDENTITY` selects your signing
identity. OS permission behavior still needs validation for the chosen signature.

1. On a new Mac setup, choose **Save configuration**. The app chooses a state
   directory beside the configuration file; it does not require a Linux path.
2. In Computers on Ubuntu, choose **Allow pairing**. On Mac, choose Ubuntu in
   Nearby, or enter its IP address with port 43120, then **Pair with address**.
3. Compare the six-digit codes and enter the other computer's code on each
   screen. Confirm on both computers. Each side saves its own confirmation;
   retry with the same computer name if one side cancels or expires.
4. On Ubuntu, choose **Install GNOME integration**. A first installation may
   require logging out and back in before GNOME loads the extension. Then
   choose **Enable desktop handoff**. This requires an unlocked GNOME session.
5. Keep both apps open. On the Mac, arrange the computer tiles in Layout and
   choose **Save layout**. Grant the Mac app Accessibility and Local Network
   permissions through System Settings when needed.
6. On Mac, choose **Enable edge sharing**. Move through a configured edge with
   keys and mouse buttons released. Cross the matching Ubuntu edge to return.
   **Ctrl+Cmd+Backspace** returns input and turns sharing off. Closing the Mac
   app also stops sharing.

Ubuntu's GNOME extension supplies actual cursor position, entry placement,
and directional return barriers while zflow keeps its existing uinput devices.
The source authenticates the receiver and checks its current desktop size
before capture. Return waits for Ubuntu's input-release acknowledgement.
Desktop changes, expired handoffs, permission loss, or connection failure stop
sharing. Refresh and save the layout after display changes, then enable again.
Other Linux desktops do not provide this automatic return path yet.

Sharing starts disabled. Layout saves and discovery do not start capture.
Each crossing opens a fresh authenticated connection; after a failed crossing,
resolve the displayed error and enable sharing again. This implementation has
automated geometry, protocol, cancellation and simulated GNOME checks. The
new full workflow still requires live two-computer qualification.

## Configuration window

Build the desktop editor with Rust 1.95 or newer. It uses
[eguicn](https://github.com/demfabris/eguicn) at a pinned Git revision, with
eframe as the native window backend. Headless builds do not enable GUI dependencies.

```sh
cargo run --features gui --bin zflow-gui
# Open a specific configuration or use the dark theme:
cargo run --features gui --bin zflow-gui -- --config /path/to/zflow.toml --dark
```

On macOS, the default file is
`~/Library/Application Support/zflow/zflow.toml`. On Linux, launch without
`--config` to read paired computers from the local service. The desktop API
returns public pairing metadata, supports user-confirmed pairing, and connects
the explicitly enabled GNOME desktop integration. It cannot return private
identity keys or edit arbitrary service settings. The GUI checks the
service's Unix credentials, and the service checks the active desktop user's
credentials. Config, identity, and input-control socket permissions stay unchanged.
Install the updated systemd unit and daemon before using this mode.

Linux service mode provides Layout, Nearby, pairing, and desktop handoff controls.
Layouts save to `$XDG_CONFIG_HOME/zflow/zflow.toml.layout.toml`, or
`~/.config/zflow/zflow.toml.layout.toml` when XDG_CONFIG_HOME is unset.
GUI pairing refreshes the computer list after confirmation. Use `--config PATH` on
either platform to open the file editor. Opening a missing file shows defaults without writing
anything. Save creates it after validation. New Mac files use a `state` directory
beside their configuration; existing identity paths remain unchanged.

The file editor provides:

- one draggable tile per computer, with edge snapping and partial-edge crossing zones;
- a live Nearby list of local zflow receiver announcements;
- paired-peer addresses and permissions, with read-only identity fingerprints;
- experimental touchpad forwarding, Linux shortcuts, and login-screen access;
- discovery, listen address, receiver buffering, checkpoint interval, and lease;
- identity and control-socket paths;
- Mac edge sharing and status, plus an Advanced launch-command builder for peer selection, address override,
  `--no-touch`, and opt-in AWDL suppression.

Save configuration writes the selected settings file. It does not restart a service, reload a daemon,
capture input, or change network interfaces. Restart an affected Linux daemon;
Mac GUI sharing reads the saved settings on its next activation. Stop sharing
before editing settings. The editor checks for on-disk changes before saving, refuses
invalid settings, and asks before discarding unsaved edits. Saving preserves
peer identities and device attributes, but rewrites TOML formatting and comments.
It does not elevate privileges; use the CLI for administrator-owned Linux files.

Settings apply to the computer whose file you edit. Receiver buffering on the
Mac does not tune Ubuntu. The configurable shortcuts apply to Linux; the Mac
source still uses Ctrl+Cmd+Backspace to return input. Mac launch options last
for the current window, and Copy launch command does not execute anything.

Revocation and Linux device enrollment still use the CLI. The editor
shows existing capture devices without replacing their hardware identities or
udev permissions. Linux service start/restart and protected configuration edits
still use administrator tools. `install.sh --gui` includes the desktop app;
the default installer continues to install the headless binaries only.

### Arrange computers

Open Layout and drag each computer to match your desk. Nearby edges snap together;
their shared length defines a crossing zone in both directions. A half-height
overlap connects only that half of each edge. Gaps and corner contact do not
create crossing zones. Overlapping drops return to the previous position.
You can also nudge a focused computer
with arrow keys (10 pixels, or 1 with Shift).

Keep the updated GUI open on both computers. Each tile represents a computer's
combined active desktop, even with several attached monitors. macOS detection
uses Core Graphics active display bounds; GNOME detection uses Mutter's current
logical monitor configuration, including fractional scaling and rotation. A
3840 × 2160 GNOME output at 133.33% contributes 2880 × 1620 desktop coordinates.
Detection runs in the background every two seconds. Other Linux desktops show
an unavailable message; dimensions above 16384 are unsupported. Detection does
not select monitor inputs or change OS display settings.

Tiles show computer names. Hover for desktop dimensions. There are no manual
size, coordinate, ownership, or add/remove controls. Detection does not measure
physical panel dimensions in inches. A waiting tile keeps its last known size
and position; a new remote tile appears after receiving its desktop report.

With saved discovery enabled, the GUIs exchange desktop sizes through a separate
`_zflow-display._udp.local.` TXT-only service. Version 2 records contain one
desktop width/height and a random instance name, without machine names, keys, or fingerprints. The
GUI matches reported IP addresses to a single saved peer; ambiguous or unknown
addresses do not add computers. Remote sizes remain unverified visual hints and
cannot authorize input. Update stale peer addresses through configuration if
DHCP changes them. Missing reports show a waiting message. Both GUIs need this
version; older per-output announcements are ignored. Closing the GUI or
disabling discovery stops announcements.

Save layout writes `<config-filename>.layout.toml` beside the main config,
for example `zflow.toml.layout.toml`. The files have separate Save actions.
This keeps old daemon binaries compatible with the main settings file. The
layout editor checks geometry and on-disk conflicts before saving. Earlier
per-output layouts appear grouped by computer; the file changes only on explicit
Save layout. Detection updates alone do not count as unsaved user edits.

Saving does **not** activate capture. **Enable edge sharing** on the Mac uses
the saved crossing zones with the enabled GNOME desktop integration. Its
authenticated geometry check rejects stale display sizes and entry points in
monitor gaps. The layout follows the screen-arrangement and
edge-range concepts in [Deskflow](https://github.com/deskflow/deskflow/blob/adb4f89453c890033288bf2ed6f36fa76f5caec5/src/lib/server/Config.cpp);
the Rust geometry and egui canvas are independent implementations, not copied
Deskflow code. The canvas stays in zflow; eguicn supplies its shadcn-style controls.

### Discover nearby computers

The native GUI starts a background `_zflow._udp.local.` browser when the saved
discovery setting is on. Pause/Resume controls the current browser; saving the
Connection discovery option also starts or stops it. Closing the GUI stops
browsing. Headless UI tests do not start networking.

The Nearby list shows advertised addresses and protocol compatibility. Names
like `zf-…` are temporary discovery IDs, not computer identities. Even an address
that matches a saved peer remains unverified until the QUIC connection checks
the pinned key. Discovery never pairs, grants access, changes stored peer
addresses, or starts input capture. On Mac, **Pair this computer** fills the
pairing address; the user still compares and confirms the codes on both screens.

Run `zflowd` with discovery enabled on the other computer. The Mac source and
configuration GUI do not advertise a receiver because neither can receive input.
Multicast must reach both computers; you can still enter an address if discovery
is unavailable. The list holds at most 64 announcements and removes records on
mDNS removal events. No router, firewall, AWDL, or Bluetooth settings change.

On macOS, check the app's permission in System Settings > Privacy & Security >
Local Network, then Pause/Resume discovery after allowing access. An empty list
can mean absent receivers, blocked multicast, or missing permission. Terminal
tools and GUI apps have different permission rules; finding a receiver with a
terminal diagnostic does not prove that the app can reach it. See
[Apple's local-network guidance](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy).

## Install and select devices

Run the installer on both Linux machines:

```sh
./scripts/install.sh
sudo zflow devices
```

Select every keyboard, mouse, and touchpad node that must move together. Repeat
`--device` in one command so the capture set is updated atomically:

```sh
sudo zflow setup \
  --device /dev/input/eventX \
  --device /dev/input/eventY \
  --udev-rules /etc/udev/rules.d/71-zflow-capture.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --action=change --subsystem-match=input
sudo zflow doctor
```

Raw touchpad forwarding is experimental. Enable it on both machines, then
restart the daemons so they create and negotiate the virtual touchpads:

```sh
sudo zflow setup --experimental-touchpad on
sudo systemctl restart zflowd.service
sudo zflow doctor
```

`zflow doctor` should report the keyboard, pointer, and experimental touchpad
as ready. On the receiving machine, `sudo libinput list-devices` should list
`zflow remote touchpad` with `pointer gesture` capabilities.

Setup records stable physical attributes. It refuses to write a broad udev
rule for hardware without a unique physical path. The service account is not
placed in the general `input` group.

The default activation chord is Ctrl+Super+F12. Ctrl+Super+Backspace always
returns ownership to the local machine. You can replace either chord during
setup by repeating `--activation-key` or `--escape-key` with evdev names such
as `KEY_LEFTCTRL`.

## Pair two machines

Pairing uses a temporary listener on UDP port 43120. The normal input service
listens on UDP port 43119.

On the first machine:

```sh
sudo zflow pair listen laptop
```

On the second machine, connect to the first machine's LAN address:

```sh
sudo zflow pair connect desk 192.0.2.10:43120
```

Both commands display a six-digit code. Compare the codes in person, then
enter the peer's code at each prompt. A mismatch writes no trust record.

Normal pairing never grants pre-login input. Grant that permission separately
only if you need input at a greeter or lock screen:

```sh
sudo zflow peer allow-prelogin desk on
```

The global pre-login gate must also be enabled with
`zflow setup --prelogin on`. Unknown seat state always denies injection.

## Use and inspect it

```sh
sudo zflow switch desk
sudo zflow local
sudo zflow status
sudo zflow status --json
sudo zflow peers
journalctl -u zflowd.service -f
```

The source waits for a complete evdev frame and a neutral ownership boundary
before grabbing the configured set. Loss of a device, transport, lease,
process, or authorization releases held input. The packaged sleep hook stops
an active daemon before suspend and starts a fresh process after resume.
Alternate launchers must provide an equivalent suspend boundary; running
`zflowd` manually is intended for development.

Revocation is local and immediate:

```sh
sudo zflow peer revoke desk
```

### macOS source cursor capture

The experimental Mac source uses an active HID-level event tap. During remote
control it hides the Mac cursor and disconnects cursor position from physical
movement, while forwarding relative deltas and raw trackpad contacts. It does
not warp the cursor back to screen center. On exit it reconnects and shows the
cursor; if macOS disables the event tap, it ends forwarding and runs cleanup.
It handles Ctrl+Cmd+Backspace, SIGINT, SIGTERM, and SIGHUP.

For background cursor visibility, the CLI resolves the private
`SetsCursorInBackground` connection property at runtime, following Deskflow's
approach. Missing symbols or a failed cursor API call prevent activation.
Apple documents cursor disconnection for foreground apps; verify the behavior
on the target macOS version, with another app focused. API success alone does
not prove cursor immobility or suppression of native trackpad gestures.

On September 10, live tests on a Mac16,5 running macOS 27.0 (26A428) passed
cursor isolation and recovery after normal exit, SIGKILL, and SIGSTOP. Fabrico
confirmed cursor visibility and control after each run. A separate observer
measured cursor position; the earlier AWDL-only tests had not checked it.
These results cover that Mac and external Magic Trackpad, not the full macOS
matrix. The AWDL helper does not restore cursor state. Use an external timed
recovery command for failure tests: the source cannot run cleanup while stopped
or after a forced kill. See [TESTPLAN.md](TESTPLAN.md) for measurements and
remaining checks.

### macOS Wi-Fi latency

If macOS input stutters or rubber-bands, measure LAN latency before changing
zflow buffering. A low baseline with repeated spikes above one 60 Hz frame
(about 17 ms) can produce the symptom even when signal strength is excellent:

```sh
ping -c 20 RECEIVER_LAN_IP
```

On the September 10 Mac-to-Ubuntu test, p95 RTT fell from 70.8 ms to 5.5 ms
while a temporary loop held `awdl0` and `llw0` down. The motion-sequence loss
counter estimates missing updates, not Wi-Fi hardware packet drops. That test
did not isolate the two interfaces.

The foreground Mac source now offers opt-in, session-scoped AWDL suppression.
Build the source and its separate helper as your normal user:

```sh
cargo build --bin zflow-macos-source
xcrun --sdk macosx clang \
  -isysroot "$(xcrun --sdk macosx --show-sdk-path)" \
  -std=c11 -O2 -Wall -Wextra -Werror \
  src/macos/awdl_helper.c -o target/debug/zflow-awdl-helper
```

After reviewing the helper and installer, authorize its installation:

```sh
sudo bash scripts/install-macos-awdl-helper.sh target/debug/zflow-awdl-helper
```

This installs a root-owned setuid executable at
`/Library/PrivilegedHelperTools/io.zflow.awdl-helper`. It grants local users
the narrow ability to lease suppression of `awdl0`; it accepts no arbitrary
commands or interface names. Installation does not change network state.
The source itself must run without sudo:

```sh
./target/debug/zflow-macos-source \
  --config "$HOME/Library/Application Support/zflow/zflow.toml" \
  --peer ubuntu --reduce-wifi-latency
```

The helper remembers AWDL's initial up/down state. It holds AWDL down during
remote capture and restores that state on return, failed activation, or
disconnect. A pipe and a two-second renewable lease also cover sender crashes
and stalls. The helper acknowledges heartbeats; a missing acknowledgement ends
remote capture. Ctrl+Cmd+Backspace, Ctrl+C, and SIGTERM return local control.
Missing or unsafe helper installation fails before capture starts.

AirDrop and other Continuity features may disconnect while suppression is
active; restoring AWDL does not promise to resume an interrupted transfer.
This option leaves `llw0` and Bluetooth unchanged. Without the flag, zflow does
not launch the helper or change AWDL. The helper serializes leases across
source processes. An administrator killing or suspending the helper itself
can prevent restoration; the lease protects against sender failures, not
failure of the privileged helper. Stop the sender before manually recovering
with `sudo ifconfig awdl0 up` if AWDL was up before the session.

AWDLToggle's interface-monitoring approach informed this feature. No code was
copied: its repository had no detected license when inspected. This guardian
uses interface notifications plus a bounded 100 ms fallback check, with no
per-tick shell processes. A short AWDL-only run measured 5.93 ms p95 RTT with
`llw0` up, and fabrico reported smooth input. Normal return, sender crash, and
sender freeze restored AWDL in live tests. The longer radio soak and remaining
failure cases in [TESTPLAN.md](TESTPLAN.md) still gate broader qualification.

If latency spikes remain, compare with the Mac on Ethernet before attributing
the problem to capture or playout behavior.

## Develop

With [just](https://just.systems/) installed, run these from the repository:

```sh
just                         # List commands
just run mac                 # Open the GUI on macOS
just run linux               # Open the GUI on Linux
just run mac --dark --config "/path with spaces/zflow.toml"
just build mac               # Build the macOS GUI
just build linux --release   # Build the Linux GUI in release mode
just test                    # Run tests, including the GUI
just test-desktop            # Test the GNOME extension with Node.js
just fmt                     # Format Rust code
just lint                    # Run Clippy
just check                   # Check formatting, lint, and run tests
```

Run `mac` commands on macOS and `linux` commands on Linux. These commands use
the host toolchain; they do not cross-compile or connect over SSH. Both GUI
builds need Rust 1.95 or newer. `run` opens the configuration window without
starting input capture. `build` produces `target/debug/zflow-gui`, or
`target/release/zflow-gui` with `--release`; it does not package or install an app.

`just check` requires Node.js for the GNOME extension tests. The protocol model and Linux runtime have deterministic tests that do not need
root. The privileged runtime check needs read access to the selected
event devices and write access to `/dev/uinput`.

```sh
cargo test --all-targets
cargo test --features gui gui:: --lib
cargo clippy --all-targets --all-features -- -D warnings
cargo run --bin zflow -- simulate
cargo check --manifest-path fuzz/Cargo.toml --bins
```

See [SPEC.md](SPEC.md) for protocol and safety invariants, and
[TESTPLAN.md](TESTPLAN.md) for the hardware and desktop validation matrix.

zflow is licensed under GPL-3.0-or-later.
