# Spike I result: PASSED, the app can post input well enough to be a receiver

Verdict: PASS. No kill criterion was hit: the Mac's own trackpad kept working while the probe posted, posted motion lands exactly where it is sent with no acceleration, double-click and drag work, and modifiers reach the app. Two rows of ROADMAP section 4 change (Caps Lock, unmapped keys) and several guesses are now measured.

Date of the runs: 2026-09-28, 11:54 to 12:06 local (14:54Z onward).
Hardware: Mac16,5, MacBook Pro, Apple M4 Max. Pointer: external Magic Trackpad (Bluetooth) and the built-in trackpad. Keyboards: built-in (ANSI) and a Bluetooth Magic Keyboard (ANSI).
OS: macOS 27.0 (26A428)
Probe: `probe.swift` in this directory, launched with `run.sh` (LaunchServices, so the probe's own grant applies). The last runs used a build signed with the owner's Apple Development identity.
Grants given by hand: Accessibility only. Input Monitoring was not needed: with Accessibility, `CGPreflightListenEventAccess()` was true and the listen taps worked.
Lid: closed. One active display, an external 3008x1692 at 0,0.
Person at the Mac: fabrico, following the prompts in the probe window.

Raw output is in `out/` (git ignores it). The four runs:
1. `all`: `suppress` and `delta` ran. A tap while circling clicked outside the probe, it lost focus, and every later test stopped waiting for focus.
2. `click flags iso caps scroll keys`: `click` ran, then focus was lost again after the background arms.
3. After two fixes (the window floats above Finder, and the probe clicks its own window to get focus back): `flags iso caps scroll keys click`. All ran. The probe then crashed in the extra `background_ax_raise_then_body` arm (AppKit trapped in `makeKeyAndOrderFront` when the probe raised its own window through Accessibility from a background thread). That arm is a probe bug with no bearing on zflow and was removed. Everything was released: afterwards Caps Lock was off and the HID modifier state was 0.
4. `suppress keys media`: the second `suppress` sample, with fabrico watching for stalls.

## ROADMAP section 4, row by row

| Row | Design being checked | Result |
|---|---|---|
| Event source | HID system-state source at the HID tap, suppression 0, local events permitted | PASS. No stall in the confirmed run with any source, including the default one. Keep interval 0 as cheap insurance; the deprecated system-wide calls are not needed. |
| Pointer | Cursor plus delta, clamped to the displays; macOS adds no acceleration | PASS. The event location alone places the cursor, 1:1, with no acceleration. Delta fields do not move it. macOS does not clamp an off-screen location, so zflow must. |
| Drag and clicks | Drag types while held, click state, event number on 27 | PASS. The click-state field is required for a double-click; the event number is not. Drag events arrive. A posted click activates a background app. |
| Keys | Keycode table, ISO swap, flags on every event | PASS on ANSI. The keyboard-type field does not change characters, so any ISO swap must live in zflow's table. No ISO keyboard here. |
| Repeat | Made on the receiver | Confirmed: macOS does not repeat a held posted key. The autorepeat field marks repeats. |
| Caps Lock | Keycode 57, falling back to `IOHIDSetModifierLockState` | Changed: keycode 57 only fakes the flag. Use `IOHIDSetModifierLockState`. |
| Unmapped keys | PrintScreen, ScrollLock, Pause as F13 to F15 | Changed: F13 arrives; F14 and F15 never reach the app. |
| Media keys | NX_SYSDEFINED subtype 8 | PASS for volume and brightness. |
| Scroll | Line units for a wheel, pixel units for continuous | PASS. Scroll phases and momentum phases are accepted too. |
| Wake | `IOPMAssertionDeclareUserActivity` on Prepare | Not run (opt-in, skipped). |

## Raw numbers

### suppress

The probe posts a 1 point back-and-forth move every 8 ms (1000 posts in 8 s) while fabrico circles on the trackpad. "Local" counts mouse moves without the probe's marker at the session tap; the path is the sum of their deltas.

Run 1:

| Arm | Posted | Local moves | Local path px | Local vs baseline |
|---|---|---|---|---|
| baseline | 0 | 277 | 2493 | 1.00 |
| default_source | 1000 | 245 | 1396 | 0.88 |
| hid_permit (interval 0, local events permitted) | 1000 | 95 | 82 | 0.34 |
| global_permit (deprecated system-wide calls, all returned 0) | 1000 | 312 | 694 | 1.13 |

Run 2, fabrico reported no stall in any arm:

| Arm | Posted | Local moves | Local path px | Local vs baseline |
|---|---|---|---|---|
| baseline | 0 | 105 | 1132 | 1.00 |
| default_source | 1000 | 918 | 6674 | 8.74 |
| hid_permit | 1000 | 833 | 8409 | 7.93 |
| global_permit | 1000 | 810 | 7238 | 7.71 |

The counts follow how fast the person circles, so they only show that local moves keep flowing; the run 2 baseline was slow. Run 1's `hid_permit` dip happened right after the probe lost focus to a stray tap, did not repeat, and no stall was felt in run 2. Default suppression interval on both sources: 0.25 s.

### delta

| Step | Sent | Cursor |
|---|---|---|
| location and delta agree | +10 location, +10 delta | moved +10 |
| delta only | same location, +10 delta | did not move |
| off-screen location | x = 4008 on a 3008-wide display | reported at x = 4008, not clamped |
| one point | +1 | moved 1.000 |
| 20 fast points by location, 8 ms apart | +20 | moved 20.000 |
| 20 fast deltas | +100 in deltas, same location | moved -0.688 |

### click

| Arm | Down click counts | Result |
|---|---|---|
| single | 1 | PASS |
| double, click state 1 then 2, event number from the system counter | 1, 2 | double-click |
| double, click state set, no event number | 1, 2 | double-click |
| double, no click state, event number set | 1, 1 | no double-click |
| double, no click state, no event number | 1, 1 | no double-click |
| drag | 10 of 10 dragged events seen | PASS |

Background focus (Finder frontmost, then a posted click on the probe window's body):

| Arm | Probe active and key after |
|---|---|
| body, no event number | yes (runs 2 and 3) |
| body, event number from the counter | yes (runs 2 and 3) |
| body, event number 1 | yes (runs 2 and 3) |
| title bar, event number from the counter | yes (run 3); in run 2 Finder never took focus, so not measured |

Deskflow issue [#9852](https://github.com/deskflow/deskflow/issues/9852) reports that posted body clicks do not activate a window on macOS 27. That did not reproduce here.

### flags

| Step | Flags AppKit saw | Cmd present |
|---|---|---|
| Cmd down (flagsChanged, `0x100008`: Cmd plus the left-Cmd device bit) | `0x100008` | yes |
| click with flags | `0x100008` | yes |
| click without flags while Cmd held | `0x100008` | yes |
| Cmd+A with flags | `0x100008`, chars `a` | yes |
| A without flags while Cmd held | `0x100008`, chars `a` | yes |
| Cmd up, HID state after | `0x0` in the app, `0x20000000` in the HID state | released |

The HID system-state source merges a held modifier into later events, so Cmd+click works even without per-event flags. Setting flags on every event is still harmless.

### iso

| Keyboard type field | Keycode 10 gave | Keycode 50 gave |
|---|---|---|
| ANSI (40) | `§` | nothing (dead key) |
| ISO (41) | `§` | nothing (dead key) |
| JIS (42) | `§` | nothing (dead key) |

Input source: U.S. International - PC, where the grave key is a dead key, hence no characters for keycode 50. The real key left of 1 on the Magic Keyboard gave keycode 50 (kbtype 109, ANSI). Neither keyboard has a key left of Z. The type field on a posted event does not change the characters, so an ISO swap cannot be delegated to macOS: zflow's table must post keycode 10 or 50 for the receiving Mac's own keyboard type. Untested on an ISO Mac.

### caps

| Method | Real lock (IOHIDSystem) toggled | CG flag state | Typed `a` gave |
|---|---|---|---|
| flagsChanged 57 | no | set | `A` |
| keyDown 57 | no | unchanged | `a` |
| `IOHIDSetModifierLockState` | yes | set | `A` |
| Restored at the end | yes, off, both times | off | |

Keycode 57 as flagsChanged only sets the event-state flag: letters come out upper case while the real lock stays off, so the two disagree. `IOHIDSetModifierLockState` on an `IOHIDSystem` connection (`kIOHIDParamConnectType`) toggles the real lock. It needed no extra permission.

### scroll

| Arm | Seen | Precise | Phases | Momentum phases |
|---|---|---|---|---|
| line (1 line) | 1, scrollingDeltaY -3 | no | none | none |
| pixel (40 px) | 1, scrollingDeltaY -40 | yes | none | none |
| gesture, 16 events | 16 | yes | began, changed, ended | began, changed, ended |

### keys

- Posted repeats (autorepeat field set after the first keyDown): isARepeat 0, 1, 1, 1, 1, 1.
- A posted key held for 1 s without repeats: one keyDown, no repeats. The receiver must make its own.
- Rates: `InitialKeyRepeat` 15 and `KeyRepeat` 2 read as 225 ms and 30 ms; AppKit reports 250 ms and 33 ms. Which one the receiver should use is still open.
- F13 (keycode 105) arrived as U+F710. F14 (107) and F15 (113) never reached the app, in two runs. No side effect was noticed.

### media

- Volume: 60, after up 63, after down 56, restored to 60.
- Brightness up then down: fabrico saw the screen step up and back down. The built-in panel was off, so the probe could not read it (-1).

### wake, feel

Not run. `wake` was opt-in and skipped. `feel` needs a Linux mouse piped over ssh and is left for Phase 2, when real Linux motion reaches the Mac.

## What changes in ROADMAP.md

- Decision 7 stands: posted motion is not accelerated, so the receiver needs a curve in Rust. No virtual HID device is needed.
- Event source: the trackpad did not stall with any source. Keep the HID system-state source with interval 0; drop the note that the trackpad stalls without it.
- Pointer: zflow must clamp positions itself, and only the location field matters.
- Clicks: set the click-state field; the event number is optional. Background windows do focus from posted clicks.
- Keys: flags are merged from the HID state anyway. The ISO swap belongs in the keycode table, keyed on the receiving Mac's keyboard type.
- Caps Lock: `IOHIDSetModifierLockState`, not keycode 57.
- Unmapped keys: PrintScreen to F13 works. ScrollLock and Pause cannot use F14 and F15; drop them or try F16 to F19.
- Scroll: phases and momentum are accepted, so a later wire change could carry them.
