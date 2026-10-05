# zflow

zflow shares keyboard, pointer, and trackpad input over authenticated QUIC.
Windows now has a native WinUI 3 preview with keyboard and pointer sharing,
screen arrangement, clipboard sharing and a notification-area app. See
[Windows setup and build instructions](windows/README.md). This source version
is 0.5.0; update all participating computers together. Its monitor-aware
input protocol cannot connect to 0.4.0 or older installations.

The arrangement shows each active monitor separately, with its display name
and computer. Drag individual monitors to match your desk. Sizes use physical
dimensions when available, otherwise OS coordinate dimensions. Existing
computer tiles split automatically; disconnected monitors keep their saved
positions and return when reconnected. Mirrored displays share one cursor
surface. A touching monitor on another computer takes priority at that edge,
even when the OS places a local monitor across it. Other movement between local
monitors follows system display settings. A monitor connected to two computers appears once for each active
connection: choosing its visible input and automatic input switching are not
part of this version.

Linux runs a system service with a GNOME panel indicator and native
GTK4/libadwaita settings. macOS runs a native SwiftUI menu-bar app with a small
settings window. Input goes both ways: the Mac sends to Linux, and a paired
Linux computer can control the Mac. This is a working prototype. Live two-host
qualification in [TESTPLAN.md](TESTPLAN.md) still gates the alpha label, and
the two-way sitting there has not run yet.

## Install

Install a published release on each computer. No repository checkout or build
tools are needed. Once installed, use the app's update controls below.

Run as your normal user on Linux or macOS:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/demfabris/zflow/releases/latest/download/install.sh | bash
```

The installer downloads prebuilt GitHub release artifacts and verifies their
SHA-256 checksums. The command downloads the installer from the latest published
release. The installer resolves the latest tag once before downloading its files.
It needs no Rust, Swift, Xcode, or C compiler. Linux binaries
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
UDP port 43119 and says so. It never turns a firewall on.

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

On Windows, run the [Windows installer](https://github.com/demfabris/zflow/releases/latest/download/zflow-windows-x86_64-Setup.exe).
It installs for your account and adds a Start menu shortcut. See
[Windows instructions](windows/README.md) for portable copies and migration
from the earlier ZIP installation.

Installed releases can check for updates from their native settings:

- **macOS:** choose **Check for Updates…** from the zflow menu. Settings controls
  automatic checks and downloads. Sparkle verifies the signed update and
  handles installation and relaunch.
- **GNOME:** the **Updates** group checks when you open the window and every
  six hours while it stays open. **Install…** asks before interrupting sharing,
  then uses the system administrator prompt. Debian installations stay under
  apt; archive installations stay under `/usr/local`. Copies owned by other
  package managers direct you to that manager.
- **Windows:** **About zflow → Updates** checks, downloads, and offers
  **Restart and update**. **Settings → App updates** controls automatic checks.
  Portable copies link to the installer.

The first release with these controls must be installed through the command,
disk image, or Windows installer above. Later updates preserve settings and
paired computers. A restart interrupts input sharing, so update each computer
when you have local control of it.

Repeat the command to update. It keeps your configuration and the computers
you added; updating the Linux service interrupts an active connection.
Computers on 0.3.0 and on 0.2.0 or older cannot connect, so update every
computer. GNOME updates an
extension from extensions.gnome.org by itself when the Extensions app or
Extension Manager is installed; a bundled copy changes at your next login. If the extension and the app get too far apart,
the panel and settings say **Update zflow**. Until the extension is set up,
the zflow window shows a banner with the step that is left: **Install**,
**Turn On**, or **Log Out**.

Pass options after `bash -s --`:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/demfabris/zflow/releases/latest/download/install.sh | bash -s -- --version v0.3.0 --no-launch
```

Omit `--version` for the latest release. `--headless` skips GNOME integration.
`--yes` is still accepted, but there is no question left for it to answer.
The script does not fall back to compiling if a release is unavailable.

You can also download a `.deb` from [Releases](https://github.com/demfabris/zflow/releases)
and install it with `sudo apt install ./zflow_0.3.0_amd64.deb` (use `arm64` on ARM).
Then open zflow from Applications and follow its banner, or run
`zflow desktop-agent --install` as yourself for the same setup the installer does.
See [packaging/README.md](packaging/README.md) for migration from a source/archive
installation, package removal, and building releases.

To remove zflow from Linux, run `sudo apt remove zflow` on a `.deb` install
(`apt purge zflow` also deletes the configuration and device selections). An
archive install keeps its uninstaller at `/usr/local/lib/zflow/uninstall.sh`;
run it with `sudo`, adding `--purge` to also delete the configuration, keys,
and the service account. Both close the firewall port the installer opened. The GNOME extension stays in each account until you remove
it in Extensions.

## Mac → Ubuntu setup

Install zflow on both computers using the command above. Open zflow from
Applications on GNOME and `/Applications/zflow.app` on macOS. Keep the GNOME
desktop agent running during use; it supplies cursor placement, desktop
dimensions, and return barriers.

On Ubuntu, open zflow. Computers running zflow on the same network show up
under **Found on your network**, below the arrangement.

The Mac app lives in the menu bar. While no computer is added, it opens its
window at launch; otherwise choose **Open zflow** from the menu-bar icon, or
open zflow again from Applications. When zflow runs from its disk image or
another temporary place, it first asks you to drag it into Applications and
open it from there. Until a computer is added, **Computers** walks through the
first one:

1. **Let zflow move the pointer here**, shown until Accessibility is on:
   choose **Allow…** and turn on zflow in the list.
2. **Add your other computers**: zflow starts looking for computers here, so
   macOS asks for Local Network access now. Choose **Allow**. If access stays
   off, the page says where to turn it on. Computers running zflow on the
   network show up beside the arrangement, each with its name and a mark of
   four colored squares. Drag one next to a screen to add it. For a computer
   not listed, the page shows the install command with a copy button.

Each computer adds the other: drag the Mac into place on Ubuntu too. Until
then, the Mac's row for Ubuntu says **Hasn't added this computer yet**, and
Ubuntu's shelf tile for the Mac says **Added you**.

Two fresh installs can skip the dragging. For its first 10 minutes with
someone at the desktop, a fresh install adds a new computer by itself when it
is the only new one around, after 5 seconds, and says **NAME joined**. If that
is not your computer, forget it. When two new computers show up at once, or
two share a name, nothing is added by itself: compare the marks on both
screens and drag the one you want. An update, or an install over ssh with
nobody at the desktop, never adds a computer by itself.

Once added, **Computers** shows the arrangement, with the new tile where you
dropped it. **Start at login** and **Reduce Wi-Fi lag**, which installs the
AWDL helper described below, are in **Settings** (⌘,).

To add another computer later, drag it in from **Found on your network** on
**Computers**; the menu-bar menu lists it with **Place…**. For one zflow
cannot find on the network, such as a computer across Tailscale, use **+**
(**Add a Computer by Address**) and type its IP address: it joins the shelf
once it answers. A computer that was reset or reinstalled shows up with a new
key and its old row says **Reset or reinstalled. Drag its new tile onto its
old one.** Doing that keeps its name, settings and place.

Move through a touching edge with keys and mouse buttons released. With
**Pause at edges** on in Settings, the pointer has to rest against the edge
for 250 ms before it crosses, and moving away first cancels.
Cross back from Ubuntu to return. **Ctrl+Cmd+Backspace** returns input and
pauses sharing. Turn **Sharing** back on from the menu-bar icon when you are
ready. Later problems, such as Accessibility or Local Network access being
turned off, show as banners on **Computers**, and **Settings** lists each
permission with an **Allow…** button.

Ubuntu can control the Mac over the same connection. On Ubuntu, press
Ctrl+Super+F12, or push the pointer through the edge that touches the Mac's
tile; Ctrl+Super+Backspace brings input back. The Mac needs Accessibility for
this too. In the Mac's window, each paired computer has a page in the sidebar
with three rows:

- **NAME can control this Mac**, on for a newly added computer. Turning it off ends
  control at once.
- **Keys from NAME**: how that computer's keys act on the Mac. See Keyboard
  modes below.
- **Reverse scrolling**: turns that computer's scrolling around on the Mac.

While Ubuntu controls the Mac, the Mac's own keyboard and trackpad still work,
and the Mac starts no crossing of its own. The Mac listens on UDP port 43119
for computers that connect first, also while sharing is paused, so the
others still find it and never hear that it has not added them. If macOS
asks whether zflow may accept incoming connections, choose **Allow**; if it can't listen, a health row says
so, the Mac tries again every 2 seconds, and Ubuntu still controls the Mac
over the connection the Mac opens.

Closing the window leaves sharing running from the menu bar. Turning
**Sharing** off returns input to the Mac; **Quit zflow** stops the engine and
finishes cleanup. While sharing is
on, the engine keeps one authenticated connection open to each paired receiver,
reads its desktop, and reuses the connection for every crossing. A lost
connection reconnects on its own; emergency pause stays paused.
GNOME must be unlocked. Other Linux desktops do not yet supply this return path.

## Keyboard modes

Each receiver keeps a keyboard mode for each paired computer. A change
applies from that computer's next crossing.

- **Standard keys** (`standard`, the default): keys arrive as sent. A Mac's
  Cmd is Super and Option is Alt.
- **PC key positions** (`pc-positions`): Option and Cmd trade places, so each
  key does what the PC key in that spot does.
- **Mac shortcuts** (`mac`): shortcuts act as the receiver's own. On Linux,
  Cmd acts as Ctrl, plus common macOS shortcuts such as Cmd+Tab,
  Option+arrows and Cmd+arrows. With the GNOME integration running, Cmd+C and
  Cmd+V in a terminal copy and paste with Ctrl+Shift+C and Ctrl+Shift+V.
  Without it, Cmd+C in a terminal is Ctrl+C. On a Mac, Ctrl and Cmd trade
  places, so a PC's Ctrl+C copies, except in Terminal, iTerm2, Ghostty,
  kitty, Alacritty, WezTerm, Warp and Hyper, where Ctrl stays Ctrl.

Choose the mode from the dropdown beside each computer in the GNOME settings
window, from **Keys from NAME** on that computer's page in the Mac window, or
from the Linux CLI:

```sh
sudo zflow peer keyboard desk mac
```

Toshy, keyd, xremap and kanata also grab `zflow remote keyboard`, so use
`standard` with them or keep them off it. Toshy takes it for a PC keyboard; set
`keyboards_UserCustom_dct = {'zflow remote keyboard': 'Apple'}` in its config.
To keep keyd off it, add `-1209:5a01` under `[ids]`. `zflow doctor` warns when
it finds one of them while a computer uses another mode.

## Settings and configuration

The Mac window exposes finding, adding and arranging computers, each computer's
settings, **Reduce Wi-Fi lag**, **Start at login**, banners for problems with
their fixes, and the permissions zflow needs. Network, pointer, scrolling, raw
touch, and playout settings live in TOML, which **zflow › Open Configuration…**
opens.

The Mac configuration is `~/Library/Application Support/zflow/zflow.toml`.
It is created on first launch. These are the Mac app defaults:

```toml
[macos]
sharing = true
block_awdl = false
```

Sharing arms only after a paired receiver, a valid touching layout, and
permissions are ready. A missing AWDL helper does not hold it back; sharing
then runs without blocking AWDL and the **Reduce Wi-Fi lag** row says so. GUI writes preserve unrelated settings and
comments. External edits reload automatically; invalid edits display an error
while the last valid configuration stays in use. Conflicting writes fail instead
of overwriting an external edit.

The GNOME window exposes sharing, the arrangement with the computers found
on the network, **Add by Address…**, keyboard modes, forgetting computers,
**Share Clipboard**, and **Start at Login**. Forget asks nothing: the computer
goes back on the shelf. Each paired computer
shows as Connected, Connecting, or why it cannot be reached, such as
**Different zflow version. Update both computers.** or **Reset or
reinstalled. Drag its new tile onto its old one.** Only those two make the status say
**Needs attention**; a computer that is asleep or away is a warning, and
zflow keeps trying. A **Retry** button tries again without waiting. Its
panel menu shows
the current sender or receiver and a sharing switch. The extension
preferences show the same GTK settings. Closing either window leaves the
desktop agent running.

With **Share Clipboard** on at both ends, the clipboard goes with the
pointer: the computer the pointer leaves sends its text, or one PNG image,
to the computer it enters. Files never go, and a clip over 3 MB stays put
with a notice. The switch is stored in `[clipboard].share`. The Mac's
**Settings** has the same **Share clipboard** switch; a clip too large to
share shows as a banner on **Computers** until the next one goes. A copy a
password manager marks as concealed or transient stays on the Mac.

Linux stores the sharing switch in `[daemon].sharing`; pausing closes active
input sessions and blocks sending and receiving, including pre-login input,
until you resume. The service still answers hellos and keeps every added
computer's key, so the others see it as paused rather than as a stranger.
Added computers and their permissions stay unchanged. Set
`sharing = true` in that section and restart the service to resume from the CLI.

Advanced Linux settings remain in
`/etc/zflow/zflow.toml` and are managed through the CLI or a text editor.

## Arrange computers

Each tile represents one computer's combined desktop. Drag in any direction,
including vertical offsets. Nearby edges snap together; overlapping tiles are
rejected. Only the touching portion of an edge permits crossing. Physical
monitor placement inside a computer remains the operating system's job.

Paired computers share one layout: a tile moved on one computer moves on the
other, and each computer writes its own tile's size. Until the Mac first
connects, it arranges tiles from each paired computer's desktop size. It keeps
the shared layout in `zflow.toml.shared-layout.toml` and its own view of it in
`zflow.toml.layout.toml`, beside the configuration. The receiver's geometry is
checked again at each crossing. Moving, resizing, or removing a tile stops the
old arrangement before the updated layout arms.

## Find computers

Each computer advertises its input port and its name over mDNS and browses
for others. A record is never trusted: the name only labels a tile on the
shelf. zflow says hello to each zflow computer it finds, on the same UDP port
43119, and the hello proves which key answered. The computer then shows on the
shelf with its name, system, version and mark, or as **Different zflow
version** if it cannot be added. Adding one is always a person's choice,
apart from a fresh install's first 10 minutes described above.

zflow finds an added computer by its key, not its address, so a new DHCP
lease or another network reconnects on its own once the computer answers.
It saves the last addresses each computer used, so a computer across a VPN
such as Tailscale is still reached after it was added by address. While
sharing is on, each computer keeps a connection open to every added computer
it found, so the first crossing does not wait for a handshake, and when both
dial at once both keep the same one. When a crossing or the chord finds no
connection up, it dials the saved addresses too, with that computer's pinned
key. macOS asks for Local Network access the first time the Mac looks for
computers, so the Mac waits for the first-computer page. Once a computer is
added, it looks from launch, and keeps looking while the app runs, even with
its window closed.

If nothing shows up, check zflow under **System Settings → Privacy & Security
→ Local Network**, or add the computer by address. Terminal tools have different
permission rules from app bundles. See [Apple's local-network guidance](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy).

## Install and select devices

With `capture_devices` empty, the default, a Linux machine that sends
captures every keyboard, mouse, and touchpad. The packaged udev rule lets the
zflow account read them. A device another program already grabbed is skipped:
keyd, for example, holds the physical keyboard and types through `keyd virtual
keyboard`, and zflow captures that one instead. `sudo zflow doctor` lists what
it would capture.

To capture only some devices, list them:

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

A listed device must be free: if another program grabbed it, every crossing
fails. List the remapper's virtual device instead.

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

## Add computers from the command line

On a Linux computer without a desktop, list the zflow computers the service
found:

```sh
sudo zflow nearby
```

Each line shows a name, its mark, its system and version, and whether it can
be added. `--add 192.0.2.10` says hello to an address mDNS cannot reach first.
Add one by name, or by mark when two share a name:

```sh
sudo zflow trust desk
```

That trusts it as dragging its tile would. The other computer still has to
add this one. The service listens on UDP port 43119 for both hellos and input.

Adding a computer never grants pre-login input. Grant that permission separately
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
capture and while a paired computer controls the Mac, and restores it on
return, failed activation, or disconnect. A pipe and
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

These commands are for developing and publishing zflow. Installing or updating
it on another computer uses the published downloads and app controls above.

Use just 1.45 or newer. Grouped commands follow `just <action> <target> [args]`.
Run `just` to list actions, `just test` to list test targets, or
`just --list --list-submodules` to see all commands. Opening a group lists its
commands without running them.

```sh
just build mac               # Package the native release app
just build linux             # Build the service and native app launcher
just run mac                 # Build and open the native debug app
just run linux               # Open native GNOME settings
just debug mac               # Native app diagnostics under target/logs
just debug linux             # Desktop-agent diagnostics
just debug daemon            # Linux daemon diagnostics until reboot
just install linux           # Install/update the Linux service
just package dmg             # Local Mac disk image (--universal for both CPUs)
just package extension       # Zip for extensions.gnome.org (Linux with GNOME Shell)
just test rust               # Rust tests
just test native             # Swift bridge tests (macOS)
just test desktop            # GNOME extension tests (Node.js)
just test gtk                # Native GTK controls and D-Bus tests (Linux display)
just test install            # Binary installer and recovery tests (Python 3)
just test package            # Debian lifecycle tests in Docker (build a .deb first)
just test release            # Release command and macOS updater packaging tests
just fmt apply               # Format Rust code
just fmt check               # Check Rust formatting
just lint                    # Clippy with warnings denied
just check                   # Formatting, Clippy, Rust, GNOME, installer, release tests
just release check           # Validate the committed version and GitHub state
just release publish         # Push main, require CI, tag, and wait for publication
```

Before the first update-enabled release, run `just release setup` on Linux or
macOS with GitHub repository administration access to configure Sparkle signing.
Linux maintainers need Python 3 with `cryptography`; macOS uses Swift and Keychain.
For each release, update `Cargo.toml`, `Cargo.lock`, the protocol version table
when needed, and `packaging/RELEASE_NOTES.md`; commit on `main`, then run
`just release publish`. The command releases the committed Cargo version and
requires a clean checkout. See [release setup](packaging/README.md) for keys,
native validation and recovering a failed workflow.

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
`just debug mac` and `just debug linux` capture logs with private file permissions.
Read Linux daemon logs with
`journalctl -u zflowd -o short-iso-precise --since "10 minutes ago"`.

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

See [SPEC.md](SPEC.md) for protocol and safety invariants,
[TESTPLAN.md](TESTPLAN.md) for the hardware and desktop validation matrix, and
[ROADMAP.md](ROADMAP.md) for the plan to make input work both ways.

zflow is licensed under GPL-3.0-or-later.
