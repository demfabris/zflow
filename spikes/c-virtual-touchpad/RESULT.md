# Spike C result: PARTIAL PASS, recognition fires, 1:1 tracking unproven

Date: 2026-08-31 evening. Host: Ubuntu 26.4, GNOME Shell 50.1, Wayland,
no physical touchpad on this machine (recorded-replay step skipped; verdicts
from udev classification plus visible GNOME behavior).

## What passed

- udev classifies the virtual pad `ID_INPUT_TOUCHPAD=1` on the first try.
  Device: type-B MT, 100x73mm at 30 units/mm, slots 0-4, INPUT_PROP_POINTER +
  BUTTONPAD, BTN_TOOL_TRIPLETAP lifecycle, ABS_X/Y mirroring slot 0.
- GNOME opens and closes the Activities overview from synthetic 3-finger
  vertical swipes. The kill criteria ("make GNOME fire its native workspace
  gesture") is met: the compositor treats phantom contacts as a touchpad
  gesture with zero special configuration.

## What did not pass (yet)

The overview transition is instant rather than tracking the fingers 1:1, and
stayed instant with a slow 1.3 s swipe holding 400 ms at the midpoint.
GNOME animations are enabled (`enable-animations true`), so that explanation
is eliminated. The decisive diagnostic (whether libinput emits a steady
GESTURE_SWIPE_UPDATE stream or collapses the gesture into begin/end) needs
`sudo libinput debug-events` on the virtual node and was not captured before
the session wrapped.

## Diagnostic captured (same evening): libinput layer is PERFECT

`sudo libinput debug-events --device /dev/input/event26` (libinput-tools
1.31.1, the same libinput mutter links) during the slow loop:

- `GESTURE_SWIPE_BEGIN 3`, then **76 continuous GESTURE_SWIPE_UPDATE events
  at exact 16 ms spacing**, steady dy ~-5.96 (dx 0.00), the scripted 400 ms
  mid-gesture hold plainly visible between updates 36 and 37 (+17.448 s ->
  +17.864 s), then a clean `GESTURE_SWIPE_END`.

The synthetic pad and libinput's gesture engine are exonerated: this is a
textbook progressive stream. Meanwhile gnome-shell's journal logged JS errors
at exactly the swipe times: `Invalid overview shown transition from HIDDEN to
HIDING` and `SHOWING to SHOWING`. So the gesture demonstrably reaches Shell's
overview machinery and the failure is in Shell's presentation/state layer.
Caveat against over-reading: the test loop blindly alternates up/down swipes
every few seconds, which itself produces illegal transitions (a hide gesture
from HIDDEN, a second show mid-animation); part of the error spam is the
loop's fault, not a single gesture's.

## Remaining next steps

1. A single clean up-swipe from a settled desktop (no loop), watching whether
   one isolated gesture tracks 1:1. The tool supports it (run without
   `--loop`); nobody was watching during the one single-shot run.
2. The definitive product test regardless: replay real Mac contact frames
   (spike D's ~63 Hz stream has natural stagger and jitter) end to end.

## Consequence for the product

Forwarded real gestures replay human-paced contact frames from a real device
(spike D proved the source side at ~63 Hz), so the product path differs from
this synthetic test in exactly the dimension under suspicion. The feature is
not blocked: gesture recognition through uinput is proven; gesture *feel*
verification moves to the first end-to-end test with real Mac contact frames.
