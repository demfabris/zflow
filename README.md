# zflow

zflow shares keyboard, pointer, and trackpad input over authenticated QUIC.
Linux runs a system service with a GNOME panel indicator and native
GTK4/libadwaita settings. macOS runs a native SwiftUI menu-bar app with a small
settings window. The Mac currently sends input to Linux; receiving
input on macOS remains future work. This is a working prototype. Live two-host
qualification in [TESTPLAN.md](TESTPLAN.md) still gates the alpha label.

## Install

Run as your normal user on Linux or macOS:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/demfabris/zflow/main/install.sh | bash
```

The installer downloads prebuilt GitHub release artifacts and verifies their
SHA-256 checksums. It needs no Rust, Swift, Xcode, or C compiler. Linux binaries
support x86-64 and ARM64 with glibc 2.39+ and systemd 254+. Native Mac apps support
Intel and Apple Silicon on macOS 26+.

On Ubuntu/Debian, a fresh installation uses the `.deb` package. On other
supported Linux distributions, or when updating an existing `/usr/local`
installation, it uses the binary archive. Linux runtime dependencies come from
apt, dnf, or pacman. GNOME settings need GJS, GTK 4.12+, and libadwaita 1.5+.
The installer lists what it will change, then requests administrator access
once, through GNOME's password dialog when available, or `sudo` in the
terminal. That prompt is the only confirmation.

On GNOME, the installer then sets up your account and starts the desktop agent,
so the Mac can connect right away. GNOME asks to download the zflow extension
from extensions.gnome.org, then loads it without a logout. If it can't (the
extension isn't published there yet, no network, extension installs turned off
by policy, no version for your GNOME, or you choose Cancel), zflow uses the
copy it ships and asks you to log out and back in once. Every GNOME user on the computer gets the zflow launcher
and starts the desktop agent at login; **Start at Login** in Settings turns
that off for one account. When ufw or firewalld is on, the installer allows
UDP ports 43119 (input) and 43120 (pairing) and says so. It never turns a
firewall on.

On macOS, download `zflow-vVERSION-macos.dmg` from
[Releases](https://github.com/demfabris/zflow/releases/latest), open it, and
drag zflow onto Applications. One image covers Apple silicon and Intel. To
update, quit zflow and drag the new version over the old one. Releases after
v0.1.0 include the image. The curl command also works on macOS: it places
`zflow.app` in `/Applications` and replaces an older copy there.

The release workflow signs Mac apps with Developer ID and notarizes them, but
v0.1.0 was built before that: its Mac apps are ad-hoc signed. Input sharing works,
but they can't install the optional AWDL helper. Releases built by the signing
workflow can install it from the app with administrator approval.

Repeat the command to update. It keeps your configuration and paired identities;
updating the Linux service interrupts an active connection. GNOME updates an
extension from extensions.gnome.org by itself when the Extensions app or
Extension Manager is installed; a bundled copy changes at your next login. If the extension and the app get too far apart,
the panel and settings say **Update zflow**. Until the extension is set up,
the zflow window shows a banner with the step that is left: **Install**,
**Turn On**, or **Log Out**.

Pass options after `bash -s --`:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/demfabris/zflow/main/install.sh | bash -s -- --version v0.1.0 --no-launch
```

Omit `--version` for the latest release. `--headless` skips GNOME integration.
`--yes` is still accepted, but there is no question left for it to answer.
The script does not fall back to compiling if a release is unavailable.

You can also download a `.deb` from [Releases](https://github.com/demfabris/zflow/releases)
and install it with `sudo apt install ./zflow_0.1.0_amd64.deb` (use `arm64` on ARM).
Then open zflow from Applications and follow its banner, or run
`zflow desktop-agent --install` as yourself for the same setup the installer does.
See [packaging/README.md](packaging/README.md) for migration from a source/archive
installation, package removal, and building releases.

To remove zflow from Linux, run `sudo apt remove zflow` on a `.deb` install
(`apt purge zflow` also deletes the configuration and device selections). An
archive install keeps its uninstaller at `/usr/local/lib/zflow/uninstall.sh`;
run it with `sudo`, adding `--purge` to also delete the configuration, paired
identities, and the service account. Both close the firewall ports the
installer opened. The GNOME extension stays in each account until you remove
it in Extensions.

## Mac → Ubuntu setup

Install zflow on both computers using the command above. Open zflow from
Applications on GNOME and `/Applications/zflow.app` on macOS. Keep the GNOME
desktop agent running during use; it supplies cursor placement, desktop
dimensions, and return barriers.

On Ubuntu, open zflow. A fresh install opens **Pair Computer** by itself and
shows a six-digit setup code; otherwise choose **Pair Computer…**.

While no computer is paired, the Mac app opens a setup window at launch. Open
it again from the menu-bar icon with **Set Up…**. Each step checks itself off
and moves on, and **Back** returns to an earlier one:

1. **Move**, only when zflow runs from its disk image or another temporary
   place: drag it into Applications and open it from there.
2. **Accessibility**: choose **Allow…** and turn on zflow in the list.
3. **Local Network**: zflow starts looking for computers here, so macOS asks
   now. Choose **Allow**. If access stays off, the step says where to turn it on.
4. **Pair**: Linux computers running zflow nearby are listed by address.
   Choose **Pair…** and type the code Ubuntu shows, or use **Enter an
   Address…**. Ubuntu then asks whether to allow your Mac; choose **Allow**.
   Each side names the other after its host name. A network announcement or a
   code alone never authorizes a computer. While none is found, the step shows
   the install command with a copy button.
5. **Try It**: move the pointer off the edge that leads to the Linux computer.
   The receiver's tile is placed against the Mac's right edge, so that is the
   right edge until you rearrange the tiles in Settings. The check turns green
   once you control it. **Done** saves **Open zflow at login**, on by default,
   and **Reduce Wi-Fi lag**, which is off by default and installs the AWDL
   helper described below.

To pair another computer later, use **Pair Computer…** in **Settings…**. Move
through a touching edge with keys and mouse buttons released. Cross back from
Ubuntu to return. **Ctrl+Cmd+Backspace** returns input and pauses sharing.
Resume from the menu-bar menu when you are ready. Later problems, such as
Accessibility or Local Network access being turned off, show on the health
badge in Settings.

Closing Settings leaves sharing running. **Pause Sharing** returns input to
the Mac; **Quit zflow** stops the engine and finishes cleanup. While sharing is
on, the engine keeps one authenticated connection open to each paired receiver,
reads its desktop, and reuses the connection for every crossing. A lost
connection reconnects on its own; emergency pause stays paused.
GNOME must be unlocked. Other Linux desktops do not yet supply this return path.

## Keyboard modes

The Linux receiver keeps a keyboard mode for each paired computer. A change
applies from that computer's next crossing.

- **Standard keys** (`standard`, the default): keys arrive as sent. A Mac's
  Cmd is Super and Option is Alt.
- **PC key positions** (`pc-positions`): Option and Cmd trade places, so each
  key does what the PC key in that spot does.
- **Mac shortcuts** (`mac`): Cmd acts as Ctrl, plus common macOS shortcuts
  such as Cmd+Tab, Option+arrows and Cmd+arrows. With the GNOME integration
  running, Cmd+C and Cmd+V in a terminal copy and paste with Ctrl+Shift+C and
  Ctrl+Shift+V. Without it, Cmd+C in a terminal is Ctrl+C.

Choose the mode from the dropdown beside each computer in the GNOME settings
window, or from the CLI:

```sh
sudo zflow peer keyboard desk mac
```

Toshy, keyd, xremap and kanata also grab `zflow remote keyboard`, so use
`standard` with them or keep them off it. Toshy takes it for a PC keyboard; set
`keyboards_UserCustom_dct = {'zflow remote keyboard': 'Apple'}` in its config.
To keep keyd off it, add `-1209:5a01` under `[ids]`. `zflow doctor` warns when
it finds one of them while a computer uses another mode.

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

Sharing arms only after a paired receiver, a valid touching layout, and
permissions are ready. A missing AWDL helper does not hold it back; sharing
then runs without blocking AWDL and the health badge says so. GUI writes preserve unrelated settings and
comments. External edits reload automatically; invalid edits display an error
while the last valid configuration stays in use. Conflicting writes fail instead
of overwriting an external edit.

The GNOME window exposes sharing, pairing, keyboard modes, forgetting
computers, and **Start at Login**. Its panel menu shows the current sender or
receiver and a sharing switch. The extension preferences show the same GTK
settings. Closing either window leaves the desktop agent running. Pairing
closes when its dialog closes.

Linux stores the sharing switch in `[daemon].sharing`; pausing closes active
input sessions and blocks sending and receiving, including pre-login input,
until you resume. Paired identities and permissions stay unchanged. Set
`sharing = true` in that section and restart the service to resume from the CLI.

Advanced Linux settings remain in
`/etc/zflow/zflow.toml` and are managed through the CLI or a text editor.

## Arrange computers

Each tile represents one computer's combined desktop. Drag in any direction,
including vertical offsets. Nearby edges snap together; overlapping tiles are
rejected. Only the touching portion of an edge permits crossing. Physical
monitor placement inside a computer remains the operating system's job.

The app reads each paired computer's desktop size over its authenticated
connection and saves positions beside the configuration in
`zflow.toml.layout.toml`. The receiver's geometry is checked again at each
crossing. Moving, resizing, or removing a tile stops the old arrangement before
the updated layout arms.

## Discover nearby computers

Pairing lists receivers advertised on the local network. Discovery does not
verify identity; only the setup code shown on the receiver does. Manual
addresses remain available. The Linux daemon advertises the receiver. macOS
asks for Local Network access the first time the Mac browses, so the Mac waits
for the setup's Local Network step or **Pair Computer…** in Settings. Once a
computer is paired, it browses from launch. Browsing then continues while the
Mac app is running, even with Settings closed.

If the list stays empty, check zflow under **System Settings → Privacy & Security
→ Local Network**, or use a manual address. Terminal tools have different
permission rules from app bundles. See [Apple's local-network guidance](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy).

## Install and select devices

After installing zflow on both Linux machines:

```sh
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

Raw touchpad forwarding is experimental. Enable it on both machines. A running
daemon applies it without a restart: it creates the virtual touchpad and ends
open sessions, so the next connection negotiates touch:

```sh
sudo zflow setup --experimental-touchpad on
sudo zflow doctor
```

`zflow doctor` should report the keyboard, pointer, and experimental touchpad
as ready. On the receiving machine, `sudo libinput list-devices` should list
`zflow remote touchpad` with `pointer gesture` capabilities and a size of
`200x150mm`. Contacts keep their real size on it, centered.

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

On the first machine, which prints a six-digit setup code:

```sh
sudo zflow pair listen laptop
```

On the second machine, connect to the first machine's LAN address and type
that code at the prompt (or pass `--code`):

```sh
sudo zflow pair connect desk 192.0.2.10:43120
```

The code never crosses the network: both sides prove they know it through
SPAKE2, so a wrong code writes no trust record. The listener then asks whether
to allow the connecting computer (pass `--yes` to skip the question in
scripts). It accepts three wrong codes, then stops and needs a new code.

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
cursor. If macOS disables the event tap after a callback timeout, it re-enables
the tap and releases on the other computer any key or button let go meanwhile.
If user input disables the tap or macOS invalidates it, forwarding ends, cleanup
runs, and sharing retries without pausing. Ctrl+Cmd+Backspace returns input and
pauses sharing.

For background cursor visibility, the app resolves the private
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

The signed bundle registers its daemon with `SMAppService`. The app connects to
it over XPC directly, and each side requires the same Team ID and the other's
exact signing identifier, so only the signed zflow app can take a lease.
The privileged service can only check `awdl0` or lease its suppression. It
accepts no shell commands or arbitrary interface names. Installation and health
checks do not change radio state.

The helper remembers AWDL's initial up/down state, suppresses it during remote
capture, and restores it on return, failed activation, or disconnect. A pipe and
a two-second renewable lease cover sender crashes and stalls. Missing heartbeat
acknowledgements end remote capture. The daemon grants one lease at a time.

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
rechecks `awdl0` every 100 ms while leased, with no per-tick shell processes.
A short AWDL-only run measured 5.93 ms p95 RTT with
`llw0` up, and fabrico reported smooth input. Normal return, sender crash, and
sender freeze restored AWDL in live tests. The longer radio soak and remaining
failure cases in [TESTPLAN.md](TESTPLAN.md) still gate broader qualification.

If latency spikes remain, compare with the Mac on Ethernet before attributing
the problem to capture or playout behavior.

## Develop

```sh
just run mac                 # Build and open the native debug app
just build mac               # Package the native release app
just dmg                     # Pack it into a local disk image (--universal for both CPUs)
just install-linux           # Install/update the Linux service
just run linux               # Open native GNOME settings
just debug mac               # Native app diagnostics under target/logs
just debug linux             # Desktop-agent diagnostics
just debug-daemon            # Linux daemon diagnostics until reboot
just test                    # Rust tests
just test-native             # Swift bridge tests (macOS)
just test-desktop            # GNOME extension tests (Node.js)
just test-gtk                # Native GTK controls and D-Bus tests (Linux display)
just pack-extension          # Zip for extensions.gnome.org (Linux with GNOME Shell)
just test-install            # Binary installer and recovery tests (Python 3)
just test-package            # Debian lifecycle tests in Docker (build a .deb first)
just check                   # Formatting, Clippy, Rust, GNOME, installer tests
```

CI (`.github/workflows/ci.yml`) runs these checks plus ShellCheck on Linux, and
Clippy, Rust tests, the C harnesses, and Swift bridge tests on macOS, for every
pull request and push to `main`.

Linux settings require GJS, GTK 4.12 or newer, and libadwaita 1.5 or newer.
On Ubuntu, install `gjs gir1.2-gtk-4.0 gir1.2-adw-1`; on Arch, install
`gjs gtk4 libadwaita`. The GNOME extension supplies the panel/tray icon, without
an AppIndicator extension. Settings use native GTK widgets through GJS.
`zflow settings` starts one desktop agent per session; the daemon retains input
ownership and privileged configuration. Install the updated system service
before using the new controls.

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
