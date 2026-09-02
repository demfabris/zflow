# Spike E result: macOS source to Ubuntu receiver

Date: 2026-09-01

## Result

The first foreground Mac source drove the existing Ubuntu receiver over the
desk LAN. Raw external Magic Trackpad contacts produced Linux pointer motion
and progressive libinput swipe gestures. A shared sender acknowledgement race
ended the first run after a few seconds; after fixing it, a second run delivered
at least 37 seconds of continuous observed input before an intentional process
kill. Ubuntu returned to idle and reported synthetic releases.

This qualifies the basic Mac-to-Linux raw-contact path on this hardware. It
does not yet qualify clean three-finger gestures because the captured trace was
reported as four-finger input, and libinput detected touch jumps.

## Systems

Mac source:

- Apple M4 Max, arm64;
- macOS 27.0 build `26A5421a`;
- external Bluetooth Magic Trackpad, MultitouchSupport family `0x81`, sensor
  `30x22`;
- Input Monitoring granted;
- Accessibility granted;
- foreground unsigned development binary.

Ubuntu receiver:

- Ubuntu 26.04.1 LTS;
- kernel `7.0.0-30-generic`, x86_64;
- LAN address `192.168.1.118`;
- `zflowd` active on UDP `43119`;
- `spotlight@nin` disabled;
- `zflow remote touchpad` at `/dev/input/event28`, `100x73mm`, with libinput
  `pointer gesture` capabilities.

## Raw Mac capture check

Commands:

```sh
cd spikes/d-multitouch-mac
clang -O2 -Wall -o mt_probe mt_probe.c \
  -framework CoreFoundation -framework ApplicationServices
clang -O2 -o mt_diag mt_diag.c -framework IOKit -framework CoreFoundation
./mt_diag
./mt_probe --secs 20
./mt_probe --tap --secs 20
```

TCC:

```text
ListenEvent (input monitoring): granted
PostEvent   (accessibility):    granted
```

The swallowing-tap run enumerated the built-in pad and external Trackpad, but
only the external device produced frames:

```text
SUMMARY: callbacks=583 touch_frames=579 multi3_frames=427 max_touches=4 active_span=9.16s rate=63.2Hz pos_in_range=yes swallowed=783
  device[0] frames=0
  device[1] frames=583
```

## Implementation

The implementation:

- moved capture records from the Linux adapter into a shared Rust module;
- made the protocol session actor available on Darwin while keeping `zflowd`,
  Linux ownership, and Linux injection Linux-only;
- added `zflow-macos-source`, a foreground source using the paired identity,
  pinned QUIC transport, shared session actor, and existing touch wire types;
- added a bounded native callback queue for the CGEvent session tap and
  dynamically loaded MultitouchSupport callbacks;
- suppresses derived Mac pointer and scroll events while raw touch is active,
  avoiding double delivery;
- restores the event tap before attempting the remote terminal release;
- emits an empty touch state after a 150 ms raw-frame timeout so device loss or
  a missing final frame cannot leave a contact held.

The explicit foreground escape is `Ctrl+Cmd+Backspace`. `Ctrl+C` is also
handled when delivered as a process signal.

## Pairing

Mac test state was stored outside the repository under:

```text
/Users/demfabris/Library/Application Support/zflow
```

The first two LAN pairing attempts exposed a CLI lifetime bug. The client had
derived its code, then stopped driving its single-thread Tokio runtime while
the confirmation prompt blocked. Ubuntu failed with:

```text
pairing metadata stream failed: server acknowledgement delivery failed: connection lost
```

The fix keeps the authenticated pairing connection and a one-worker runtime
alive through human confirmation. On the third attempt both sides independently
displayed `229407` before either saved trust.

Saved public identity fingerprints:

```text
macbook: 1972739c10df6b7196a6c3ea5a8daa69aff0f68ac78d5b0007c30e975d52b492
ubuntu:  6c0693441525c54bbaf2f220f3c534336cc4e3fcba2419095c5480121b8869a5
```

Ubuntu kept `prelogin=false`.

## Live result

Observer:

```sh
journalctl -u zflowd.service -f
sudo libinput debug-events --device /dev/input/event28
```

Source:

```sh
cargo run --quiet --bin zflow-macos-source -- \
  --config '/Users/demfabris/Library/Application Support/zflow/zflow.toml' \
  --peer ubuntu \
  --address 192.168.1.118:43119
```

The source reported:

```text
connecting to ubuntu at 192.168.1.118:43119
*** Recognized (0x81) family*** (30 cols X 22 rows)
remote input active: raw_touch=true; escape with Ctrl+Cmd+Backspace or Ctrl+C
```

Ubuntu produced continuous one-finger pointer events and gesture streams such
as:

```text
GESTURE_SWIPE_BEGIN          4
GESTURE_SWIPE_UPDATE         4 -18.84/-1.93
GESTURE_SWIPE_UPDATE         4 -26.15/-3.14
GESTURE_SWIPE_UPDATE         4 -37.92/-3.14
GESTURE_SWIPE_END            4
```

The first live run closed with:

```text
snapshot acknowledgement does not name an emitted snapshot
```

The sender was deleting an earlier unacknowledged neutral checkpoint whenever
it emitted a newer checkpoint. A normal delayed acknowledgement then looked
forged. Unacknowledged checkpoints are now retained until an ordered ACK removes
them, with a hard cap of 64. A focused regression test sends a second neutral
checkpoint before acknowledging the first.

After that fix, observed input continued from libinput timestamp `+158.888s`
through at least `+196.558s` before the source was intentionally terminated.

## Failure cleanup

The exact foreground process was terminated with `SIGTERM`. Ubuntu logged:

```text
Sep 01 12:44:53 ubuntu zflowd[671992]: input session closed peer=macbook reason=critical control stream read failed: connection lost
```

Post-failure status:

```text
"activation_id": null
"ownership": "idle"
"selected_peer": null
"session_epoch": null
"transport_generation": null
"synthetic_releases": 17
```

This proves receiver cleanup after process death for the exercised held state.
The run did not separately time the release edge against the 900 ms lease.

## Checks

Darwin checks completed during the session:

```text
cargo test --all-targets
107 library tests passed
8 QUIC integration tests passed

cargo test core::sender::tests
8 sender tests passed

cargo clippy --all-targets --all-features -- -D warnings
passed
```

## Known defects and untested cases

- libinput emitted five `Touch jump detected and discarded` warnings across
  the two runs, then rate-limited further messages. Large discontinuities are
  also visible in some gesture and pointer deltas.
- The observed swipe streams were four-finger. An isolated three-finger pass
  was not recorded, so GNOME workspace and Overview tracking remain unqualified.
- Keyboard, derived pointer, and continuous scroll through the `--no-touch`
  CGEvent path were not exercised end to end.
- The physical `Ctrl+Cmd+Backspace` escape was not exercised. A synthetic PTY
  Ctrl+C did not deliver a signal; explicit `SIGTERM` did terminate the source.
- Held-key failure cleanup, live network loss, live Bluetooth removal, and
  Trackpad reconnect were not separately exercised.
- The source enumerates raw devices during capability preflight and again when
  capture starts. It does not yet re-enumerate after a Bluetooth change while
  the process remains active.
- Signed, hardened, notarized packaging remains out of scope for this result.

## Follow-up: macOS Wi-Fi latency

A later interactive run reproduced stutter and rubber-banding while the Ubuntu
receiver was wired and the Mac source used Wi-Fi. Bidirectional pings had a
2-4 ms baseline with matching 31-76 ms stalls. Receiver metrics reported 72 ms
p95 RTT, 82 ms p99 delay variation, and 198 missing datagrams. The Mac was on
5 GHz channel 40 with -37 dBm signal and an 866 Mbps transmit rate, so weak
signal and 2.4 GHz Bluetooth coexistence did not explain the stalls.

Both Apple peer-to-peer interfaces, `awdl0` and `llw0`, were active. The user
reported that temporarily disabling both greatly improved trackpad behavior.
Future troubleshooting should offer this as a reversible, opt-in diagnostic
after measuring latency, with a warning that AirDrop and Continuity features
may be interrupted. Ethernet remains the clean comparison. Any remaining large
coordinate jumps should be investigated separately from transport jitter.
