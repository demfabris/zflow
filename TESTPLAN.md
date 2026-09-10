# zflow test plan

> Release gates for beta and 1.0. The prototype gates (property tests, simulator, fuzz targets) live in SPEC.md under Validation. Snapshot 2026-08-31; refresh version claims before running a matrix.

Thresholds named "frozen" must be written down, with their measurement method, before the matrix that uses them runs.

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

After a separate admin-approved helper installation, qualify
`zflow-macos-source --reduce-wifi-latency` with:

- flag absent: no helper process and no AWDL changes;
- helper missing or unsafe: refuse activation before local input capture;
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
