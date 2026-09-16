# zflow test plan

> Release gates for beta and 1.0. The prototype gates (property tests, simulator, fuzz targets) live in SPEC.md under Validation. Snapshot 2026-08-31; refresh version claims before running a matrix.

Thresholds named "frozen" must be written down, with their measurement method, before the matrix that uses them runs.

## Cross-platform source installer, September 16

Run `just test-install` (also included in `just check`). The tests replace
package managers, privilege brokers, and builds with fixtures. They cover Linux
package selection, unprivileged builds, graphical/terminal authentication,
failed downloads and builds, GNOME activation after login, macOS prerequisites,
and app replacement with recovery of the previous bundle.

All 18 tests passed on the Linux host and under Bash 3.2.57 in an isolated
container. ShellCheck passed for both install scripts. The Linux release
binaries built with the locked dependency graph, and `just check` passed.
The tests did not replace the running service or install packages on the host.

Live installation checks remain:

- Install and update on current Ubuntu, Fedora, and Arch GNOME sessions.
  Check password approval/cancellation, first-login extension activation,
  preserved configuration/identity, and settings launch from Applications.
- Install headless from a terminal, including a machine without Rust.
- On macOS 26+, test missing Command Line Tools, ad hoc and Apple-signed
  builds, updating a running app, first-launch permissions, and Open at login
  from the installed `/Applications/zflow.app` path.

## Native GNOME app and panel, September 16

The GNOME extension retains cursor placement and return barriers and adds a
panel indicator. The native GTK4/libadwaita app and extension preferences share
one settings view. A session D-Bus service in the desktop agent forwards a
limited set of requests to the credential-checked daemon API.

Run `just check` and `just test-gtk`. The GTK check requires a Linux display
and runs on a private D-Bus session. It exercises actual widgets and D-Bus
messages against a fake service: pause/resume, rejected-write rollback, login,
pairing confirmation/cancellation, forgetting, loss of service, and cleanup.
Rust tests check pause authorization in both directions and the existing
pre-login boundary, plus daemon pairing acknowledgements and cancellation.

Implementation checks on GNOME Shell 50.1, GTK 4.22.4 and libadwaita 1.9.1:
`just check` passed (236 library tests, eight QUIC tests, both Node suites).
`just test-gtk` passed the isolated Rust session-service test and GTK controls.
The extension loaded in a separate headless GNOME Shell and returned desktop
geometry through its existing API. Native GTK rendering was inspected for the
settings window and pairing dialog. The live system daemon was not replaced;
two-computer pairing and input pause/release still need the checks below.

Live checks after installing the updated service and desktop integration:

- Open zflow from Applications, the panel, and GNOME Extensions preferences.
  Confirm status, native theme, keyboard focus, and one desktop agent.
- Pause while receiving and while sending. Held input must release and new
  connections must stay blocked. Resume must preserve the paired identities.
- Pair with a Mac and another Linux computer. Cancel before confirmation and
  enter a wrong code; neither should add trust. Forget an active computer and
  verify immediate cleanup. Keep pre-login permissions unchanged.
- Close settings and cross edges repeatedly. Disable/re-enable the extension,
  restart the daemon, lock/unlock, and change monitors; inspect recovery.
- Turn Start at Login off, log in again, and verify panel polling does not
  start the agent. Opening settings should start it. Restore the login setting.
- Edit the system configuration externally, then change sharing. Reject the
  stale write. Restart the service and confirm the UI reflects the saved state.

## Native macOS app and Linux desktop agent, September 16

Current architecture: SwiftUI MenuBarExtra and Settings, a Rust application
worker behind a C ABI, a Linux desktop agent, and an SMAppService AWDL
daemon authenticated through same-team XPC. The earlier egui desktop GUI is removed.
Historical sections below retain their dated measurements, not current commands.

Automated checks:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
just test-native
node tests/gnome_desktop_test.mjs
./scripts/build-macos-app.sh --debug --sign "SIGNING IDENTITY"
```

The Swift tests call the actual Rust bridge with isolated temporary files.
They check snapshot decoding, persistent AWDL settings, comment preservation,
last-valid settings after malformed edits, and Pause when saving is blocked.
Rust tests retain layout snapping, geometry validation, discovery filtering,
pairing over loopback QUIC, and external-write conflict coverage. The guardian
C tests use passed pipe descriptors and a fake radio backend.

Native UI and live matrix:

- Launch an isolated bundle configuration with sharing paused. Check menu
  actions, native Settings, keyboard focus, health popover, pairing sheet,
  AWDL switch, automatic window sizing, and light/dark system appearance.
- Pair through both six-digit confirmations; cancellation must never save trust.
- Drag computers in all directions, including offsets. Check snapping,
  overlap rejection, reopening, and external layout conflicts.
- Check first launch, malformed/deleted TOML, correction, GUI writes preserving
  comments, and the last-valid running configuration.
- Allow Accessibility and Local Network through macOS. Denied or revoked
  permission must produce useful health actions and stop capture.
- Install the signed helper from the GUI; test required approval, denial,
  repair, helper restart, an invalid client signature, and mismatched teams.
  Registration alone must not report readiness; authenticated ping must pass.
- Test Open at login and reboot with the final installed app path.
- Keep Settings closed and cross repeatedly. Repeat on another Space, through
  Pause, emergency return, display changes, receiver loss, and clean Quit.
- Run `zflow desktop-agent --install` as the Linux desktop user, then test
  GNOME login autostart, daemon restart/reconnect, and display advertisements.

Actual input capture, radio changes, OS authorization, and two-computer latency
remain live qualification steps. A successful build does not establish them.

September 16 implementation verification (macOS 27, Xcode 27, Swift 6.4,
Rust 1.97):

- Mac: 185 library tests, one CLI test, and eight QUIC integration tests passed;
  one native-desktop test remained ignored. Formatting and strict Clippy passed.
- Three Swift tests passed against the Rust bridge, including automatic reload
  without UI requests. Strict Swift formatting checks passed.
- Native C cursor and guardian tests passed with fake system APIs. Emergency
  return remains latched when the event queue is full; ordinary capture failure
  does not become a user pause. Guardian tests cover release, EOF, and expiry.
- GNOME compositor simulation passed. Linux binaries and tests cross-compiled
  and linked. In a network-disabled, read-only Debian amd64 container, 226
  library tests and eight QUIC integration tests passed; three hardware tests
  remained ignored. The emulated parallel run exposed a timestamp-test failure;
  that test passed alone and the complete library suite passed serially.
- Signed debug and release app bundles passed plist and strict signature checks.
  The native dark-mode Settings window, health actions, AWDL toggle, pairing
  sheet cancellation, accessible tile movement, saved layout, close/reopen, and
  clean Quit were checked with a disposable paused configuration.

No helper registration, login-item enrollment, radio suppression, actual input
capture, or two-computer pairing was performed during this implementation.
Light-mode appearance and mouse-drag interactions still need live inspection.

## Reliability fixes, September 15

Automated regressions cover these boundaries:

- Unsupported Linux HID usages and pointer buttons are rejected before backend
  injection, including checkpoint and takeover state. A loopback test holds a
  key, sends button 9, and verifies peer cleanup releases the key without sending
  the unsupported button to the backend.
- The Mac edge observer runs independently of rendering. Worker tests exercise
  polling with no UI calls, stop notification, cleanup and joined shutdown.
- Quiet sessions wait for checkpoint, lease, playout and probe deadlines instead
  of polling every millisecond. Desktop replies wake the actor directly. Pending
  controls and cumulative catch-up retain the 1 ms progress cadence.
- Touch reports preserve mapped capture timestamps through uinput. A synthetic
  16 ms contact stream reproduces a network burst with two reports delivered
  1 ms apart; their event timestamps remain 16 ms apart. This removes the
  resulting 24 mm normalized jump in libinput's detector without delaying
  delivery. Timestamp tests cover clock regression, stale input and cleanup.
- Seat queries use a persistent system D-Bus connection with fresh property
  reads and bounded timeouts. Classification, consistency, invalid properties,
  failed handshakes and reconnection have regression coverage.

Run `just check`. On an unlocked Linux desktop, run the read-only seat check:

```sh
cargo test --locked --lib linux::seat::tests::live_logind_matches_loginctl_and_reuses_connection -- --ignored --nocapture
```

That check matched this host's unlocked Wayland session and verified denial on
connection loss followed by reconnection. Twenty inspections took about 50 ms
through D-Bus versus 178 ms through `loginctl`; this is a local measurement.

Live qualification still requires matching Mac and Ubuntu builds. Enable edge
sharing, minimize or hide the Mac window, cross to Ubuntu, return, and repeat.
Check Stop/Escape, window close and display changes during preparation and
active sharing. Linux worker tests do not compile or exercise the macOS APIs.

Repeat touch gestures with a clean link and controlled jitter while recording
uinput/libinput events. Confirm nondecreasing timestamps, gesture behavior and
prompt release after link loss. Capture timestamps can still reflect source
queue processing time; unstamped begins and clock corrections can clamp an
interval. Backdated events can produce separate libinput processing-lag warnings.
The synthetic regression does not prove that all historical touch-jump warnings
are gone, or qualify GNOME gesture timers under burst delivery.

## Historical GUI pairing and Mac-to-GNOME handoff, September 14

The GUI now supports pairing, explicit Mac source start/stop, saved-layout edge
entry, and automatic return through the GNOME extension. These implementation
checks do not qualify the live workflow:

- `just check` includes the Rust GUI/session tests and `node tests/gnome_desktop_test.mjs`.
  The extension tests execute its actual JavaScript with simulated compositor
  operations; they do not prove GNOME API behavior on hardware.
- QUIC loopback tests exercise Prepare, input, Leave and Finish. Withholding the
  runtime's Leave acknowledgement must prevent Finish from completing. Cancelling
  Prepare and dropping a desktop broker must close the session and release input.
- Mapping checks cover all four edges, partial overlaps, negative desktop
  origins, monitor gaps and changed dimensions. A return point in a Mac monitor
  gap must not warp the cursor there.
- Native fake-capture tests cover GUI cancellation and admission: held keys or
  buttons, movement away from the entry point during connection setup, unmatched
  local key/button releases, and contacts collected before cursor isolation.
- GUI file-pairing tests use real loopback QUIC. They verify no peer record exists
  before matching confirmation, directional permissions, mismatch rejection and
  retry without replacing an existing identity or expanding permissions.

Return polling holds each GNOME request for up to 200 ms and replies on a barrier
hit. The Node compositor checks cover hold expiry, a hit during a pending poll,
pending-poll cancellation at Finish or lease expiry, and hold-timer removal.
For live verification, expect about 300 poll requests per minute, `seat_before_ms`
and `seat_after_ms` equal to zero for polls, and a return response within roughly
one network round trip of the barrier hit. The daemon refreshes cached seat state
on its 250 ms tick and keeps fresh seat checks around other desktop operations.
These are targets; the two-machine measurements remain pending.

For current live qualification, install matching Linux daemon/desktop-agent
builds and open the native Mac bundle. Follow README.md for CLI pairing on
Linux, native Mac pairing, GNOME extension setup, and OS permissions. Drag the
computer tiles and verify pointer, typing, scrolling, raw touch, and automatic
return across all four orientations and partial overlaps. Settings can close
while sharing continues. Pause, emergency return, and Quit must restore input.

Check held keys at entry, movement during admission, lock/logout, monitor
changes, receiver exit, and connection loss. Cancelled admission must keep
sharing armed. Emergency return must stay paused even if cleanup fails.
Connection failures may retry while sharing is enabled. Record the GNOME/macOS
versions and the exact bundle signature used; simulated checks do not qualify
the live workflow.

September 14 implementation verification: Mac passed 200 Rust tests with one
native-desktop test ignored, plus formatting, Clippy, native fake-capture tests
and the GNOME extension simulation. Linux GUI binaries and tests compiled and
linked with cargo-zigbuild. Those Linux tests ran in temporary Debian bookworm
amd64 containers: 238 library tests and eight QUIC integration tests passed;
the native-desktop and privileged input tests remained ignored. The containers
had no network and read-only access to the build artifacts. The Mac release app
bundle passed plist and signature checks. A native window inspection used a
disposable missing configuration with sharing off; it did not grant permissions,
pair real computers, or redirect input. The initial Ubuntu SSH attempt failed
host-key verification. Later investigation matched the LAN server's key to the
existing `ubuntu` known-host entry and connected as `demfabris` using
`ssh -o HostKeyAlias=ubuntu -o StrictHostKeyChecking=yes demfabris@192.168.1.118`.
Ubuntu ran a September 10 installed daemon alongside the September 14 GUI;
its journal rejected edge handoff with `unknown message family 7`. GNOME 50.1
had the extension files but had not loaded the new extension. The installer
now restarts an existing daemon after replacement, and `just install-linux`
builds and installs both binaries. The user completed installation and
logout/login; SSH confirmed the updated daemon and an ACTIVE GNOME extension.
The user reported a successful round trip followed by a failed reconnect.
Ubuntu logged `peer macbook already has an established input connection`.

The Mac source now closes and drains its QUIC endpoint before its per-crossing
runtime exits, including cancellation and negotiation failure paths. A bounded
shutdown failure prevents rearming. A regression uses three short-lived client
runtimes and confirms that the remote sees each disconnect. Fifteen Mac tests
and 48 GUI tests passed (one native desktop test ignored). Twenty consecutive
authenticated Snapshot connections to the real Ubuntu desktop passed with
orderly shutdown; these checks did not capture input. Receiver cleanup also
releases its lease before waiting for sessions and ignores obsolete broker IDs;
four Linux desktop tests passed, including the two new cleanup regressions.
The subsequent manual run still had abrupt transitions and stopped sharing;
the Mac reported cursor movement during connection preparation, and a later
Finish request timed out. This workflow remains unqualified. `just debug mac`
and `just debug linux` now save timestamped logs grouped by crossing and include
stage durations, cancellation displacement, capture-loop gaps and stop reasons.
`just debug-daemon` enables receiver request, seat-check and compositor timings
until reboot. Request diagnostics distinguish cancellation, timeout, closed
response channels and unavailable queues. Successful active polls below 350 ms
stay at trace level. Focused Mac/receiver tests and Clippy passed; the debug
launcher help path produced a private log file. Reproduce the same crossings with these builds and
compare both logs and the Ubuntu journal before changing admission limits.

The 17:17:16 UTC reproduction confirmed along-edge cancellation: the Mac stayed
at x=0 while y moved from 928.8125 to 917.8125 during preparation. Its GUI
cancelled at 40 ms; Ubuntu finished Prepare in 20 ms and then cleaned up after
the Mac disconnected. Admission now uses a narrow rectangle along the configured
crossing range, clipped to the entry monitor. It keeps the eight-pixel inward
allowance but accepts motion along that edge. GUI, Rust preflight, and both native
startup checks use the same rectangle. Fifteen Mac tests, four geometry tests,
and native fake-cursor tests cover the recorded motion, all four orientations,
partial ranges, monitor gaps, inward retreat and invalid rectangles. Live
crossing verification with this Mac build remains pending.

The next manual run improved crossing admission but exposed a delayed return
warp. At 17:23:00 UTC, native capture stopped at .207 while the GUI positioned
the Mac cursor at .811, after Finish and QUIC shutdown. Cursor positioning now
happens inside native capture cleanup, before reconnecting and showing the local
cursor. The GUI only rearms after network cleanup; it never positions the cursor
at that later point. Shared return mapping retains four-edge, partial-overlap
and monitor-gap checks. Native regressions assert warp-before-reconnect ordering,
one warp only, and input restoration after invalid, late or failed warp requests.
These checks passed along with the focused Mac/geometry tests and Clippy.
The user still needs to verify the returning cursor with the updated Mac build.

### Cursor transition follow-up, September 14

The Mac now samples the cursor again immediately before Prepare and maps it
through the same inverse mapping used at edge detection. Compare
`entry fraction sampled before desktop preparation` with `checking cursor before
capture` to measure the remaining movement during Prepare. Do not add a catch-up
motion frame until this measurement shows a visible residual jump; the receiver
applies libinput acceleration to relative motion.

The daemon rejects a second session from the same peer before the old session
closes, so the Mac still waits for the two-second-bounded QUIC drain before
rearming. With Reduce Wi-Fi latency enabled, AWDL acquisition overlaps connection
setup and release overlaps endpoint shutdown. Keep the helper lease renewed
while a connection is pending. The previous 17:32 Mac log shows about 15 ms for
acquisition and 25 ms for release with the switch on; measure the updated build
before claiming a reduction. The under-60-ms rearm target remains unverified.

Complete this live checklist with matching Mac, Linux desktop-agent, daemon and extension
builds. Use `just debug mac`, `just debug linux` and `just debug-daemon`:

- Cross five times each way, including one 60-second stay on Ubuntu. Record
  `connection_to_capture_ms` and request-ID growth; target about 300 polls/minute.
- Press Escape twice. Require `waiting for desktop poll before cleanup`, a
  successful `remote input release completed`, and successful Finish without
  `BackendUnavailable` from dropping the poll.
- Click during connect and overshoot the entry region. Require `crossing
  cancelled`, `enabled=true` when the worker finishes, and a successful next
  crossing after moving back inside the Mac. Stop must still disable sharing.
- Check daemon trace logs for zero `seat_before_ms` and `seat_after_ms` on polls.
  Seat refreshes should happen on the broker tick rather than on each poll.
- Measure return-edge report to `edge sharing rearmed`, with Reduce Wi-Fi latency
  both off and on. Try another crossing within 150 ms and record its result.
- Minimize the Mac window and cross, then repeat with its window on another Space.
  The application worker must continue observing without window rendering.

Automated verification for this follow-up: Mac passed 205 Rust tests with one
native-desktop test ignored; Ubuntu passed 251 with two hardware-dependent tests
ignored in `~/dev/zflow-review`. Both passed formatting/Clippy checks and GUI
builds; Ubuntu also built the daemon. The GNOME simulation, native fake-cursor
tests and AWDL guardian/pipe-process tests passed. Gate logs and task files are
under `target/plan-tasks` on the Mac. These builds have not replaced the running
apps or installed Ubuntu daemon/extension.

Live results for these changes remain pending. Existing logs describe the
previous build; automated checks do not establish crossing latency or hidden
window behavior. Keep `PLAN.md` until this checklist has measured results.

## Historical configuration GUI observations

The native September 16 matrix above replaces the old widget/editor harness.
The following September 10 observations describe the removed interface.

On September 10, the first native Mac window loaded the existing paired-Ubuntu
configuration and rendered the Computers and Mac source launch sections. No
settings were saved and no capture/helper process was started. Headless GUI
tests cover editing and persistence; Linux native-window QA and the wider
keyboard/accessibility matrix remain pending. This does not qualify monitor
edge switching, connection monitoring, GUI pairing, or service management.

The earlier Layout preview rendered the paired Mac/Ubuntu suggestion with a
shared-edge crossing zone on the native Mac window. The current editor detects
resolution and scaling and removes the manual size/position controls. For live discovery QA, confirm the running Linux daemon has
discovery enabled and compare Nearby with `dns-sd -B _zflow._udp local.` on Mac.
Check removal, Pause/Resume, malformed records, and multicast denial without
enabling input capture. A discovered address must never count as verified pairing.

On September 10, both Apple's `dns-sd` and the unchanged `NearbyBrowser` in a
terminal-launched diagnostic found the running Ubuntu receiver. The worker found
`192.168.1.118:43119` and two scoped IPv6 addresses within 500 ms. The temporary
app-bundle preview still showed no records. GUI Local Network permission/signing
is the remaining lead, not a confirmed denial. User permission verification and
live GUI discovery remain pending. Do not bypass Local Network protections to
qualify this test; allow access through the normal system UI and retry.

### Desktop service access and detected displays

The Linux desktop agent uses `/run/zflow-gui/peers.sock`, a separate desktop
endpoint. The original control socket and private config/state paths retain
their permissions. The endpoint accepts Snapshot, user-confirmed pairing and
an explicitly enabled desktop broker. It checks Unix credentials
against the service/root/active desktop UID, and returns public peer records
plus the discovery flag. It has a separate eight-client limit and three-second
initial-request timeout; pairing has its own bounded confirmation window and
the desktop broker rechecks authorization during its connection. The agent also
checks the server UID; it cannot edit service configuration.

Tests cover rejected mutation commands and unknown fields, socket credentials,
snapshot write refusal, invalid desktop records, ambiguous address matches,
detected geometry, preserved drag positions, and keeping layout saves separate
from service settings. Native tests still need to cover hotplug, rotated
displays, mixed scale factors, remote report removal, stale saved addresses,
and Local Network permission denial.

With the updated service installed, run `zflow desktop-agent` in the unlocked
GNOME session and open the native Mac app. Verify that one tile appears per
computer and that arranging tiles does not modify the protected Linux config.

September 10 implementation checks: Mac passed 174 tests; Ubuntu passed 221
with one ignored hardware test. Both passed formatting, Clippy, and GUI builds;
Ubuntu also built the headless daemon and passed systemd unit verification.
The earlier native Mac window displayed two local output tiles. This exposed a
model mismatch with Synergy's computer tiles. The earlier generic Ubuntu monitor
list also included an inactive output and rounded 133.33% scaling to 200%.

After user authorization, the release daemon and updated unit were installed on
Ubuntu and zflowd restarted at 12:36 local time. The desktop user queried the
new socket and received the saved macbook and xps peer records; the server UID
matched the zflow account. Config contents and the private config/state/control
permissions were unchanged. The previous daemon and unit remain in
`/var/tmp/zflow-rollback.kB4qiU` on Ubuntu for rollback.

The Mac pairing stores an IPv4-mapped IPv6 address. Display matching now treats
that form as equivalent to the plain IPv4 address in mDNS, with a regression
assertion that preserves ambiguous-peer rejection. The targeted tests and
Ubuntu GUI build/Clippy passed. The open GUIs still need reopening after the
computer-tile update; cross-machine display rendering remains pending.

### One tile per computer

The editor now groups outputs into one desktop per computer. Core Graphics
supplies active Mac bounds. GNOME's DisplayConfig supplies active logical
monitor groups, fractional scales, transforms and layout mode. Detection runs
off the UI thread, with one query in flight and a three-second GNOME timeout.
Other Linux desktops and dimensions above 16384 show an error. No fallback
claims a connected-output list is an active desktop.

Regression tests cover negative origins, mirrored/overlapping bounds, inactive
GNOME outputs, fractional scaling, rotation, physical layout mode, mixed-scale
positions, and invalid geometry. Layout tests cover one tile per computer,
rendered computer labels, old per-output grouping without disk writes, report
loss/reappearance, saved positions, and discard/reload. Automatic geometry
updates must not trigger unsaved-change prompts. Network tests reject old v1
records; v2 accepts one bounded desktop and retains ambiguous-address rejection.

Run the read-only native detector check in the desktop session:

```sh
cargo test --locked --lib native_desktop_geometry -- --ignored --nocapture
```

September 10 computer-tile checks: Mac passed 179 tests with one opt-in test
ignored; Ubuntu passed 231 with two ignored. Both passed formatting, Clippy
with warnings denied, and GUI builds. The opt-in native queries also passed
separately: Mac reported 5696 × 1692 and GNOME reported 2880 × 1620. Linux checks
used an isolated source snapshot; the Ubuntu checkout, running GUI, and service
were not changed. The rebuilt GUI binary is ready for the next approved launch.

Restart the native Mac app and Linux desktop agent. Confirm one tile per
computer, drag and reopen Settings, and verify persistence. Stop and restart
the desktop agent; its known tile should keep its position without duplication.
Use a paused configuration for layout-only checks.

## Headless Linux alpha qualification

Before calling the current prototype an alpha, install the package on two Linux hosts and exercise the real service, daemon account, evdev devices, uinput devices, and network path. The qualification run must cover:

- setup, udev permissions, `zflow doctor`, pairing, revocation, and both pre-login permission gates;
- CLI and hotkey ownership changes with keys, buttons, relative motion, and high-resolution wheel input;
- isolated loss, jitter bursts, a lost terminal frame, lease expiry, peer revocation, and an unknown or locked seat;
- an incomplete capture set, a competing grabber, device removal, and hotplug enrollment;
- daemon kill and `SIGSTOP` while Idle and Remote, with watchdog recovery inside the declared bound;
- suspend and resume, cold boot, and authorized GDM pre-login input;
- local metric output for latency variation, loss, drops, playout lateness, catch-up, and synthetic cleanup;
- an arming-leak measurement and a Quinn datagram timing comparison on the maintainer's jittery link, closing spikes G and F.

The run passes with no stuck or duplicated transitions, no unauthorized injection, exact final motion and wheel displacement, and local ownership restored inside the declared bound. Record hardware, software versions, commands, metrics, and failures. This qualifies the first Linux alpha on that tested combination; the broader Linux backbone matrix below still gates Linux beta.

## Linux backbone matrix (gates Linux beta)

Test GDM with GNOME, SDDM with KWin, and greetd with a wlroots compositor at: greeter, logged-in session, lock screen, VT, suspend and resume, display-manager restart, daemon restart.

For each stack:

- verify udev and libinput classification for the remote-injection keyboard and pointer, and for the experimental touchpad;
- verify allow_prelogin_input deny/allow behavior at each greeter, each lock screen, an unauthenticated VT, and a logged-in VT;
- hold modifiers, multiple keys, and buttons across every ownership transition;
- arming: neutral-state wait, all-or-none grab across composite multi-node devices, EBUSY release, chord conflict detection at setup;
- measure switch-time leakage (events reaching the local session between chord detection and grab) against the frozen threshold;
- hotplug: a device added to the capture-set configuration participates at the next activation without a daemon restart;
- test competing EVIOCGRAB holders;
- crash, kill, and SIGSTOP zflowd during Remote: the watchdog returns local input within the recovery bound; repeat while Idle: no local effect, and a restarted daemon is immediately functional;
- verify that evdev monitoring and capture reject every zflow virtual device role (no self-capture loops);
- suspend/resume during Remote closes the activation and releases receiver state.

The stack passes with zero stuck keys or buttons, zero missing or duplicate transitions across the routing boundary, zero self-capture, zero injection before authorization, correct final local and remote high-resolution wheel displacement, leakage under the frozen threshold, and local ownership within the recovery bound after crash, kill, or SIGSTOP. A cold boot proves uinput availability, permissions, and virtual-device enumeration with correct udev properties before the greeter appears. VT validation requires keyboard and media controls; pointer and wheel become VT requirements only where a console consumer exists.

The touchpad experiment adds Mutter, KWin, and wlroots gesture recognition plus raw contact conformance.

## Desktop edge switching (gates edge switching per desktop)

Freeze miss, false-activation, latency, and recovery thresholds before the soak. Run 10,000 portal crossings with high-speed motion, clicks during activation, monitor hotplug, ZonesChanged, fractional scaling, transforms, and lock/unlock.

Cover portal v2 selection, clean disablement on v1, denied or cancelled consent, missing capabilities, EIS disconnect, portal/backend restart, and helper death during Remote. Kill, SIGKILL, and SIGSTOP/watchdog zflowd during an active portal capture; repeat for zflow-session and restart both processes. Assert that the portal closes within the recovery bound, old activations never reconnect, and key, button, pointer, and wheel state has zero duplicate transition.

Verify that EIS never captures a zflow remote-injection device, that a portal activation and an evdev grab never coexist on the same capture set, and that a seat does not arm portal source capture while it receives peer input.

Test layer surfaces at 1, 2, and 4 logical pixels across selected wlroots compositors and COSMIC. Include fullscreen windows, panels, overlay conflicts, and shared internal monitor edges. A compositor becomes a supported automatic-switch target only after it meets the frozen thresholds.

## Signed macOS package (gates macOS beta)

### Developer source cursor qualification

Ask before redirecting input. First run the native fake-cursor tests and Rust
Mac tests; the native tests replace cursor mutations and event-tap creation.
They cover balanced hide/show, failed acquisition rollback, cleanup errors,
relative motion filtering, escape, and tap disablement without capturing input.

```sh
cargo test --lib macos::
xcrun --sdk macosx clang \
  -isysroot "$(xcrun --sdk macosx --show-sdk-path)" \
  -std=c11 -O2 -Wall -Wextra -Werror \
  tests/macos_capture_test.c \
  -framework ApplicationServices -framework CoreFoundation \
  -o target/debug/macos-capture-test
./target/debug/macos-capture-test
```

After consent, use a short bounded run with an external recovery command.
Measure Mac cursor coordinates from a separate process while moving on Ubuntu;
require a stationary hidden Mac cursor and continuing remote motion, including
after long strokes that would reach a Mac display edge. Test with a different
Mac app focused. Check local clicks, typing, scrolling, and native gestures as
separate results, in raw-touch and `--no-touch` modes.

Repeat activation and return. Verify cursor visibility and movement after
Escape, SIGINT, SIGTERM, SIGHUP, disconnect, and event-tap disablement. Force
SIGKILL and SIGSTOP in separate approved runs with timed external recovery;
do not infer cursor recovery from AWDL state or receiver key releases.

#### Observed results, 2026-09-10

Tested an unsigned debug source on Mac16,5, macOS 27.0 (26A428), with an external
Magic Trackpad forwarding raw contacts to Ubuntu/GNOME. Each run used
`--reduce-wifi-latency`, with AWDL up beforehand and `llw0` left up. Fabrico
approved each run. A separate Python process sampled `CGEventGetLocation`
about every 20 ms, checked `ifconfig awdl0`, and controlled the exact child PID.

- Normal return: after 15 seconds, the supervisor sent SIGTERM. All 719 active
  cursor samples matched the anchor. The source exited 0 and restored AWDL.
  Fabrico confirmed cursor hiding/return and Ubuntu movement and gestures.
- Crash: after eight seconds, the supervisor sent SIGKILL. All 383 active
  samples matched the anchor. The observer saw AWDL up after 26.8 ms and Mac
  cursor movement after 987 ms. Fabrico confirmed cursor visibility and control.
  Neither the cursor-reconnect fallback nor source cleanup ran.
- Freeze: after eight seconds, the supervisor sent SIGSTOP, then SIGCONT four
  seconds later. All 384 pre-freeze samples matched the anchor. The observer
  saw cursor movement after 1036 ms, before resume, and AWDL up after 2016 ms
  while `ps` still reported the source as stopped. Fabrico confirmed that the
  cursor reappeared during the pause. The source exited 1 after resume because
  its AWDL helper lease had expired. Neither forced-exit nor cursor-reconnect
  fallback ran; no source or helper process remained.

Cursor timings describe the first observed physical movement, not an exact OS
recovery deadline. The source's unconfirmed-AWDL warning after freeze reflects
a closed acknowledgement pipe; the observer verified restoration independently.
These runs qualify cursor isolation and the three return paths on this setup.
Separate local click/key/scroll/gesture leakage checks, `--no-touch`, sleep/wake,
other macOS versions, and the signed build matrix remain pending. Earlier AWDL
tests predated cursor disconnection and do not establish cursor isolation.

### Signed build matrix

Test final-path Developer ID and notarized builds on clean supported macOS versions and both CPU families where hardware exists. Cover:

- absent, denied, granted, and reset TCC state on clean virtual machines, with correct zflow attribution in prompts and System Settings;
- Aqua enrollment followed by lock, logout, LoginWindow, reboot, and fast-user switching;
- an upgrade with the same Team ID and designated requirement but a replacement Developer ID leaf certificate;
- Secure Event Input with immediate keyboard release and separate pointer/scroll results;
- forced callback overrun until tapDisabledByTimeout, tapDisabledByUserInput, Mach-port invalidation, sleep/wake, and session replacement, with 100 cycles per failure mode and receiver/local-state recovery inside the declared bound;
- standard and unaccelerated pointer fields;
- active-filter discard, warp-to-anchor, and cursor disassociation where its foreground precondition holds, measuring cursor escape, edge saturation, warp-induced spikes, feedback, and release restoration;
- wheel, Magic Mouse, and trackpad scroll phases;
- version-tested media decoding and replay.

Raw contact capture (MultitouchSupport, feature-flagged in the signed baseline):

- device enumeration and contact-frame validation on both CPU families;
- the flag's off path, and graceful degradation to pointer/scroll with a diagnostic when the framework is absent or changed after an OS update; a broken framework MUST NOT fail the session;
- notarization of the final-path build with the framework linked;
- end-to-end replay onto the Linux virtual touchpad with correct libinput classification and recognized 3/4-finger gestures on each Linux stack.

For CoreHID, verify the effective entitlement and embedded provisioning profile on the exact caller. Create keyboard, relative-pointer, Consumer, and experimental touchpad descriptors; dispatch reports and assert OS input. Destroy and recreate devices across helper restart, sleep/wake, logout, lock, and LoginWindow.

For Karabiner, use the pinned upstream client from the named root process. Verify root-only socket access and protocol negotiation, inject keyboard, pointer, and Consumer reports, and test daemon/driver death, mismatch, reconnect, lock, and LoginWindow. Record each backend separately; CGEventPost success does not qualify a virtual-HID backend.

zflow keeps macOS lock-screen and LoginWindow target injection out of the product promise until the selected backend passes keyboard and pointer injection in actual secure password fields. LoginWindow source keyboard capture remains unavailable while Secure Event Input is active; the matrix evaluates pointer and scroll as separate event classes.

## Radio, QoS, and playout (gates the frozen smoothing constants)

### Session-scoped macOS AWDL control

Do not redirect input or change interface state without the tester's consent.
Build and run `tests/macos_awdl_helper_test.c` without privileges; its fake
backend must not access network interfaces. Run the Rust Mac tests as well.

```sh
cargo test --all-targets
xcrun --sdk macosx clang \
  -isysroot "$(xcrun --sdk macosx --show-sdk-path)" \
  -std=c11 -O2 -Wall -Wextra -Werror \
  tests/macos_awdl_helper_test.c -o target/debug/macos-awdl-helper-test
./target/debug/macos-awdl-helper-test
```

After installing the signed helper through the native GUI and macOS
authorization, qualify **Block AWDL while sharing** with:

- switch off: no lease and no AWDL changes;
- helper missing, denied, or incorrectly signed: refuse activation before capture;
- AWDL initially up and initially down: restore the original state on return;
- repeated activation/return, Escape, Ctrl+C, SIGTERM, and a failed capture;
- sender SIGKILL and pipe closure: restore without sender cleanup;
- sender SIGSTOP: restore within the two-second lease plus scheduling and ioctl
  overhead, and do not reacquire on stale queued heartbeats after resume;
- network loss and `OutboundEnded`: stop capture and release suppression;
- macOS reactivating AWDL during a lease: reassert down without a busy loop;
- a second sender: refuse its lease without changing the first sender's state;
- helper failure: restore local input, report any unconfirmed AWDL restoration,
  and use manual recovery if an administrator killed or suspended the helper;
- AirDrop/Continuity availability after return, with Bluetooth and `llw0`
  untouched throughout the run.

Compare latency and gesture behavior with AWDL alone suppressed against the
earlier two-interface diagnostic. Do not claim AWDL-only latency qualification
from fake-backend tests. Check held-key release separately from AWDL restoration.

On September 10, the short AWDL-only live run measured RTT p50 3.091 ms, p95
5.932 ms, and p99 13.725 ms with `llw0` up. Fabrico reported smooth input.
The cursor qualification results above record subsequent normal-exit, crash,
and freeze recovery with the installed helper. These short runs do not replace
the ten-minute radio matrix or qualify AirDrop transfer resumption.

### Radio measurements

Run each radio configuration for at least ten minutes on the target hardware. Record:

- probe cadence and direction (no probe; 5, 10, 20, 50, 100 Hz; one-way and bilateral);
- Wi-Fi power-save state;
- observed AWDL coexistence state; intentionally created Apple-to-Apple includePeerToPeer traffic forms a separate test arm, and no pass criterion depends on private realtimeMode control;
- best effort, real-time interactive, and voice service classes;
- simultaneous bulk traffic;
- wired control;
- RTT percentiles, loss, reordering, burst length, Wi-Fi retries/scans/roams, CPU wakeups, and energy.

Capture DSCP and over-the-air WMM mapping. A socket-option return value does not pass the gate.

Replay the captured traces through no buffer, fixed delay, and adaptive delay. Compare final-position error, event lateness, overshoot, recovery time, and processing cost. Freeze probe cadence, delay bounds, catch-up limits, and any filter only after this result.

## Path failover (gates automatic failover)

Test:

- client-local address migration where Quinn supports it;
- both peers gaining a direct-cable address;
- active-path unplug;
- stale packets from the old connection;
- session takeover while keys and buttons remain held;
- simultaneous candidate and identity spoofing.

The gate requires bounded input release, one accepted session owner, no duplicate transition, and no old-path event after takeover.

## Pairing and authorization (gates 1.0)

Test an active LAN MITM, spoofed mDNS instances, transcript mismatch, pairing-window expiry, revoked identities, replayed 0-RTT data, malformed floods, and unauthorized local-socket callers.

Verify that a normal paired peer cannot inject at a greeter, lock screen, unauthenticated VT, or unknown seat state until a local logged-in user grants allow_prelogin_input.
