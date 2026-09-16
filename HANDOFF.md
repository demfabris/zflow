# Handoff: Mac source to Ubuntu

Date: 2026-09-01

This is a historical handoff, not the current task list. As of September 16,
macOS uses a native SwiftUI menu-bar app backed by the Rust core. Linux uses
`zflow desktop-agent` for GNOME handoff and desktop announcements. The settings
window exposes pairing, layout, AWDL, and readiness actions; advanced settings
stay in TOML. The older GUI was removed. Earlier live measurements describe
their recorded builds and do not qualify the new interface or XPC helper.
See README.md and TESTPLAN.md for current commands and qualification work.
The push instructions and excluded features below apply to September 1 only;
they do not authorize actions in a later session.

You are the Codex session on fabrico's MacBook Pro. Build and test the first
Mac-to-Linux zflow path. Push each finished checkpoint to `origin/main` so the
Ubuntu session can inspect and test it.

## Start here

Use the existing checkout or clone the private repository, then confirm that
you pulled the Linux touch implementation from 2026-09-01:

```sh
git pull --ff-only origin main
git log -1 --oneline
git status --short
sw_vers
uname -m
```

The Ubuntu receiver is available at:

- LAN: `192.168.1.118`
- Tailscale: `100.120.229.99`
- input service: UDP `43119`
- pairing service: UDP `43120`

Use the LAN address for desk testing. The two Codex sessions cannot message
each other, so fabrico will relay coordination between terminals.

Read these files before changing code:

1. The `## macOS` section in `SPEC.md`
2. The `## Signed macOS package` section in `TESTPLAN.md`
3. `spikes/d-multitouch-mac/RESULT.md` and `mt_probe.c`
4. `src/linux/touch.rs`, `src/linux/uinput.rs`, `src/session.rs`, and the wire
   touch types

## Current state

The Linux source and receiver now forward complete type-B multitouch contact
snapshots. The protocol sends a reliable begin and end around datagram updates.
The Ubuntu receiver exposes a five-slot `zflow remote touchpad` through uinput.

An XPS laptop running CachyOS/KDE drove Ubuntu GNOME through this path:

- one-finger pointer movement worked;
- three-finger horizontal workspace movement tracked the fingers;
- three-finger vertical Overview movement tracked the fingers after fabrico
  disabled `spotlight@nin` on Ubuntu.

Spotlight removes GNOME's Overview search controller while Ubuntu Dock still
allocates it. Their collision caused the instant Overview transition. Keep
Spotlight disabled during gesture tests. GNOME Shell also logged some
`Touch jump detected and discarded` warnings for the virtual device. Capture
those warnings during the Mac test rather than treating the extension fix as
proof that the touch stream is clean.

Spike D passed on this MacBook Pro with an external Bluetooth Magic Trackpad:

- Apple M4 Max, macOS 27.0 build `26A5421a`;
- stable raw contact IDs and normalized positions;
- about 63 to 65 contact frames per second;
- raw frames continued while a default CGEventTap swallowed derived mouse and
  scroll events.

`MTDeviceCreateList` missed the awake Magic Trackpad in several early runs.
Enumerate devices when capture starts and again after device or Bluetooth
changes. Do not enumerate once at process startup.

The Mac-to-Ubuntu foreground path was implemented and exercised later on
2026-09-01. Raw contacts drove Ubuntu pointer and swipe behavior, pairing and a
delayed checkpoint-acknowledgement race were fixed, and abrupt Mac process death
returned Ubuntu to idle with synthetic releases. The remaining raw-touch defect
is reproducible libinput touch-jump warnings; the exact three-finger case and
the non-touch CGEvent path are still pending. See
`spikes/e-macos-ubuntu/RESULT.md` for commands, logs, metrics, and exclusions.

## Session goal

Build one foreground Mac source that connects to the existing Ubuntu receiver.
Prove these paths in order:

1. keyboard, pointer, and continuous scroll through a CGEvent session tap;
2. raw Magic Trackpad contacts through MultitouchSupport and the existing touch
   wire protocol;
3. clean release after process death, network loss, device removal, and finger
   lift.

This session does not need Mac target injection, CoreHID, LoginWindow support,
notarized packaging, AWDL control, clipboard support, or a GUI.

## Architecture constraints

Keep event capture in a logged-in user process. The future root LaunchDaemon
owns networking and console-session arbitration, but it must not create event
taps or call CGEventPost. A foreground session executable can stand in for the
LaunchAgent during this test.

Reuse zflow's identity, pairing, wire, transport, lease, and touch lifecycle.
Do not create a second protocol in C or Swift. The crate gates `runtime` and
`session` behind Linux today. Move the platform-neutral session pieces that
the Mac source needs. Keep Linux behavior and tests intact.

Load MultitouchSupport symbols at runtime. If the framework or a required
symbol disappears, report that raw touch is unavailable and retain the
keyboard, pointer, and scroll path. Keep raw contact capture behind the
existing experimental touch capability.

The event-tap and Multitouch callbacks must copy bounded event data into a
queue and return. Run protocol and network work outside those callbacks.
Re-enable a tap disabled by timeout or user input. The explicit zflow escape
path releases remote input and disables filtering.

Avoid double delivery. In raw-touch mode, the Magic Trackpad contacts should
drive Ubuntu's virtual touchpad. Do not also inject their derived CG mouse or
scroll events through the virtual pointer during the same test.

## First test sequence

Confirm the known private-framework path and TCC state before changing it:

```sh
cd spikes/d-multitouch-mac
clang -O2 -Wall -o mt_probe mt_probe.c \
  -framework CoreFoundation -framework ApplicationServices
clang -O2 -o mt_diag mt_diag.c -framework IOKit -framework CoreFoundation
./mt_diag
./mt_diag --request  # run if the check reports an unknown grant
./mt_probe --secs 20
./mt_probe --tap --secs 20
cd ../..
```

Then start with compilation and platform boundaries:

```sh
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
```

At handoff commit `69e9b46`, the crate did not compile on Darwin because
`src/lib.rs` exported `daemon` on all platforms while `src/daemon.rs` imported
Linux-only modules. The later Spike E work established the platform boundaries,
moved shared capture records, and added the real foreground Mac source described
above.

After the Mac source can pair, coordinate with fabrico and pair against the
Ubuntu host. The Ubuntu side runs:

```sh
sudo zflow pair listen macbook
```

The Mac connects to `192.168.1.118:43120`. Compare the six-digit codes before
either side saves trust.

For raw-contact testing, keep an Ubuntu terminal open with the daemon log and
libinput events visible:

```sh
journalctl -u zflowd.service -f
sudo libinput debug-events
```

Perform these gestures on the Magic Trackpad:

- slow one-finger pointer movement;
- slow three-finger horizontal movement with a pause halfway;
- slow three-finger swipe up and down with a pause halfway;
- rapid direction reversal followed by finger lift.

Ubuntu passes when GNOME follows the fingers, libinput reports progressive
updates, and the journal contains no new touch-jump warning. Stop the Mac
source during an active contact and during a held key. Ubuntu must release
both within the lease bound, and the Mac must regain local input.

## Record and deliver

Record the Mac build, TCC grants, input devices, frame rates, gesture results,
failure results, and known defects in a new result document under `spikes/`.
Include exact commands and log excerpts. Never commit private keys, pairing
state, machine certificates, or TCC database contents.

Run formatting and the tests available on the Mac before each checkpoint:

```sh
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
git diff --check
```

Commit coherent checkpoints and push them to `origin/main`. Tell fabrico the
commit hash so the Ubuntu session can pull and run its Linux regression suite.
