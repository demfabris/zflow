# Spike D result: PASSED, contact frames survive a swallowing event tap

Date: 2026-08-31 evening. Hardware: MacBook Pro, Apple M4 Max, macOS 27.0
(build 26A5421a). Input device actually exercised: external Magic Trackpad
(Bluetooth). Tool: `mt_probe.c` in this directory, plain unsigned CLI built
with clang (Xcode 26 CLT), plus `mt_diag.c` for the TCC check.

## Method

The framework binary lives in the dyld shared cache, not on disk, so the
probe resolves everything at runtime: `dlopen` of
`/System/Library/PrivateFrameworks/MultitouchSupport.framework/MultitouchSupport`
then `dlsym`. Contact frame callback registered on every device returned by
`MTDeviceCreateList`, then `MTDeviceStart(dev, 0)` and a CFRunLoop. A
listen-only CGEventTap counts CG-level mouse/scroll/key events so the log
shows what kind of input flowed alongside (or instead of) MT callbacks. The
`--tap` mode adds the zflow operating mode: a second CGEventTap,
`kCGEventTapOptionDefault` at `kCGSessionEventTap` head insert, that returns
NULL for mouse-moved, left/right drag, and scroll-wheel events (cursor
visibly frozen while it runs, hard 20 s timeout as the escape hatch).

## Raw numbers

Enumeration (once the Magic Trackpad was awake, see gotcha below):

- device[0]: family 0x6f (111), builtin=1, sensor 30x22, the laptop's own pad
- device[1]: family 0x81 (129), builtin=0, sensor 30x22, the Magic Trackpad
- both: `MTDeviceStart` status 0, `MTDeviceIsRunning` 1, `MTDeviceIsAvailable` 1

Run 1, no tap, fabrico wiggling 3 fingers on the Magic Trackpad:

- 453 callbacks, 452 frames with contacts, 411 frames with 3 contacts
- 64.7 Hz over the 6.99 s active span
- normalized positions all inside [0,1], contact ids stable across frames,
  state transitions 1 -> 3 -> 4 (start-in-range, make-touch, touching) as per
  the known state enum, sizes 0.4-1.5

Run 2, swallowing tap active (the kill-criteria run):

- 1170 mouse/scroll events swallowed over the window; cursor confirmed dead
- MT frames kept arriving the whole time: 705 callbacks, 702 with contacts,
  427 with 3 contacts, 62.7 Hz, positions in range
- per-device attribution: device[1] (Magic Trackpad) 705, device[0] 0

Kill criteria: PASSED. Raw contact frames with 3+ fingers, on this exact
machine and macOS version, and frame delivery is completely indifferent to a
default-option event tap eating the derived mouse/scroll events.

## Symbols used

`MTDeviceCreateList`, `MTDeviceCreateDefault`, `MTRegisterContactFrameCallback`,
`MTDeviceStart`, `MTDeviceStop`, `MTDeviceIsRunning`, `MTDeviceIsAvailable`,
`MTDeviceIsBuiltIn`, `MTDeviceGetFamilyID`, `MTDeviceGetDeviceID`,
`MTDeviceGetSensorDimensions` all resolve on macOS 27.0
(`MTEasyInstallPrintCallbacks` resolves too, unused in the final runs).
Struct layout is the long-standing one from Karabiner/OpenMultitouchSupport,
unchanged: field-for-field identical behavior observed.

## Gotchas worth carrying into implementation

- **`MTDeviceCreateList` is dynamic and can miss the device the user is
  actually touching.** Three early runs enumerated only the built-in pad and
  delivered zero frames while the Magic Trackpad was in active use driving
  the cursor; the external pad only showed up in the list on later runs
  (Bluetooth session state, presumably). Cost the session ~40 minutes of
  chasing a framework that was working fine. zflow must re-enumerate on
  hot-plug (or at gesture-capture start), never once at daemon startup.
- Frame rate from the Magic Trackpad over Bluetooth is ~63-65 Hz. The
  built-in pad is reputedly faster but went unmeasured: fabrico drives this
  mac with the Magic Trackpad, so the external pad is the operative device
  here. Budget for ~60 Hz, treat anything more as a bonus.
- TCC context: Input Monitoring and Accessibility were already granted to the
  terminal's process tree before any MT call (verified with `mt_diag.c` /
  `IOHIDCheckAccess`, both "granted"). Whether contact frames flow WITHOUT
  Input Monitoring is untested; assume it is required on this macOS.
- 4-5 finger frames were not captured (3 is what fabrico ran); the criterion
  said 3+ and 3 is proven. The 30x22 sensor grid reports fine.
- Notarization of a tool linking the framework: untested, no signing identity
  on this session. The unsigned ad-hoc CLI works as-is.
