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

## Open question and next step

One capture of `sudo timeout 20 libinput debug-events --device <node>` while
`spike-c-virtual-touchpad --loop --slow` runs. If UPDATE deltas stream
continuously, the snap is mutter-side interpretation of this device and needs
comparison against a real touchpad's event stream (borrow a laptop, or replay
a libinput record from one); if UPDATEs are missing, the synthetic stream
lacks something libinput's gesture engine wants (candidate suspects:
perfectly synchronized finger landing in one frame, zero inter-finger jitter,
missing ABS_MT_TOUCH_MAJOR/PRESSURE).

## Consequence for the product

Forwarded real gestures replay human-paced contact frames from a real device
(spike D proved the source side at ~63 Hz), so the product path differs from
this synthetic test in exactly the dimension under suspicion. The feature is
not blocked: gesture recognition through uinput is proven; gesture *feel*
verification moves to the first end-to-end test with real Mac contact frames.
