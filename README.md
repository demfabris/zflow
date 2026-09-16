# zflow

zflow shares keyboard, pointer, and trackpad input over authenticated QUIC.
Linux runs a headless system service. macOS runs a native SwiftUI menu-bar app
with a small settings window. The Mac currently sends input to Linux; receiving
input on macOS remains future work. This is a working prototype. Live two-host
qualification in [TESTPLAN.md](TESTPLAN.md) still gates the alpha label.

## Mac → Ubuntu setup

Build on macOS with Rust and Swift 6.2 or newer (Xcode 27 is supported).
The app requires macOS 26 or newer:

```sh
./scripts/build-macos-app.sh --sign "Apple Development: YOUR NAME (TEAM)"
open target/release/zflow.app
```

Move the bundle to `/Applications` before enrolling permissions or enabling
Open at login, so its installed path stays stable.

Omit `--sign` for an ad hoc development build. Input sharing can run in that
build, but the AWDL service requires an Apple-issued signing identity shared
by the app, client, and daemon. Use `--debug` for a debug bundle.

On Ubuntu, install the service, then install the GNOME integration as your
normal desktop user:

```sh
./scripts/install.sh
zflow desktop-agent --install
zflow desktop-agent
```

The installer preserves the service configuration and identity. Repeat it
after updating the code. A newly installed GNOME extension may require logging
out and back in, then enabling zflow in GNOME Extensions. The desktop agent
starts automatically on subsequent GNOME logins. Keep it running during use;
it supplies cursor placement, desktop dimensions, and return barriers.

1. On Ubuntu, start pairing with `sudo zflow pair listen mac`.
2. Open **Settings…** from the Mac's zflow menu-bar icon. Choose **Pair Computer…**
   and select the receiver, or enter its IP address with pairing port `43120`.
3. Enter the other computer's six-digit code on each side and confirm both.
   A network announcement alone never authorizes a computer.
4. Use the health badge to allow Accessibility access. Allow Local Network
   access when macOS asks. Arrange the computer tiles by dragging them until
   the desired edges touch. Changes save automatically.
5. Move through a touching edge with keys and mouse buttons released. Cross
   back from Ubuntu to return. **Ctrl+Cmd+Backspace** returns input and pauses
   sharing. Resume from the menu-bar menu when you are ready.

Closing Settings leaves sharing running. **Pause Sharing** returns input to
the Mac; **Quit zflow** stops the engine and finishes cleanup. The engine checks
the authenticated receiver before arming and again at each crossing. Failed
connections retry while sharing is enabled; emergency pause stays paused.
GNOME must be unlocked. Other Linux desktops do not yet supply this return path.

## Settings and configuration

The native window exposes computer pairing and arrangement, **Block AWDL while
sharing**, **Open at login**, and a health badge with permission and helper
actions. Network, pointer, scrolling, raw touch, and playout settings live in
TOML through **Open Configuration…**.

The Mac configuration is `~/Library/Application Support/zflow/zflow.toml`.
It is created on first launch. These are the Mac app defaults:

```toml
[macos]
sharing = true
block_awdl = false
```

Sharing arms only after a paired receiver, a valid touching layout, permissions,
and any requested helper are ready. GUI writes preserve unrelated settings and
comments. External edits reload automatically; invalid edits display an error
while the last valid configuration stays in use. Conflicting writes fail instead
of overwriting an external edit. Advanced Linux settings remain in
`/etc/zflow/zflow.toml` and are managed through the CLI or a text editor.

## Arrange computers

Each tile represents one computer's combined desktop. Drag in any direction,
including vertical offsets. Nearby edges snap together; overlapping tiles are
rejected. Only the touching portion of an edge permits crossing. Physical
monitor placement inside a computer remains the operating system's job.

The app detects desktop sizes and saves positions beside the configuration in
`zflow.toml.layout.toml`. Display advertisements help arrange known paired
computers; authenticated receiver geometry is checked before capture. No
unpaired announcement creates a trusted computer. Moving, resizing, or removing
a tile stops the old arrangement before the updated layout arms.

## Discover nearby computers

Pairing lists receivers advertised on the local network. Discovery does not
verify identity; both sides must confirm the six-digit codes. Manual addresses
remain available. The Linux daemon advertises the receiver and the desktop
agent advertises its desktop dimensions. Browsing continues while the Mac app
is running, even with Settings closed.

If the list stays empty, check zflow under **System Settings → Privacy & Security
→ Local Network**, or use a manual address. Terminal tools have different
permission rules from app bundles. See [Apple's local-network guidance](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy).

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

Enable **Block AWDL while sharing** in Settings. If the background helper is
missing, use **Install…**; if macOS needs approval, use **Allow…**. macOS owns the
authorization prompt. zflow never reads or stores an administrator password.

The signed bundle registers its daemon with `SMAppService`. The helper and its
client enforce matching Team IDs and exact signing identifiers through XPC.
The privileged service can only check `awdl0` or lease its suppression. It
accepts no shell commands or arbitrary interface names. Installation and health
checks do not change radio state.

The helper remembers AWDL's initial up/down state, suppresses it during remote
capture, and restores it on return, failed activation, or disconnect. A pipe and
a two-second renewable lease cover sender crashes and stalls. Missing heartbeat
acknowledgements end remote capture. The daemon serializes leases across clients.

AirDrop and Continuity may disconnect while suppression is active. Restoring
AWDL does not resume interrupted transfers. `llw0` and Bluetooth are unchanged.
AWDL blocking defaults off. Killing or suspending the privileged daemon itself
can prevent restoration; the lease protects against sender failures. Stop the
sender before manually recovering with `sudo ifconfig awdl0 up` if AWDL was up
before the session.

The earlier setuid helper is no longer used or installed. If you installed it
manually, an administrator can remove
`/Library/PrivilegedHelperTools/io.zflow.awdl-helper` after stopping old builds.

Historical measurements from the previous helper transport follow; they do not
qualify the new signed XPC installation path.

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

```sh
just run mac                 # Build and open the native debug app
just build mac               # Package the native release app
just install-linux           # Install/update the Linux service
just run linux               # Run the GNOME desktop agent
just debug mac               # Native app diagnostics under target/logs
just debug linux             # Desktop-agent diagnostics
just debug-daemon            # Linux daemon diagnostics until reboot
just test                    # Rust tests
just test-native             # Swift bridge tests (macOS)
just test-desktop            # GNOME extension tests (Node.js)
just check                   # Rust formatting, Clippy, tests, GNOME tests
```

Mac builds produce `target/{debug,release}/zflow.app`. The Swift sources are in
`macos/`, and `src/app/` contains UI-independent application services. A small C
ABI links the Rust static library into the Swift app. The Rust worker owns input
sharing independently of window visibility. No web view or embedded browser is
used. Native system controls follow the current macOS appearance.

For an isolated configuration, launch the bundle executable with
`--config /absolute/path/zflow.toml`. `RUST_LOG` controls Rust diagnostics.
`just debug` captures logs with private file permissions. Read Linux daemon logs
with `journalctl -u zflowd -o short-iso-precise --since "10 minutes ago"`.

```sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo run --bin zflow -- simulate
cargo check --manifest-path fuzz/Cargo.toml --bins
```

The AWDL guardian's fake-backend tests exercise explicit release, sender EOF,
and lease expiry without changing a network interface:

```sh
clang -std=c11 -Wall -Wextra -Werror tests/macos_awdl_helper_test.c -o target/awdl-test
./target/awdl-test
```

See [SPEC.md](SPEC.md) for protocol and safety invariants, and
[TESTPLAN.md](TESTPLAN.md) for the hardware and desktop validation matrix.

zflow is licensed under GPL-3.0-or-later.
