# Spike C: virtual touchpad gestures (stub)

**Question:** does a uinput type-B multitouch touchpad, fed synthetic 3-finger contact frames, make GNOME fire its native workspace swipe?

**Kill criteria:** failure kills target-native gestures until compositors ship ei_gestures; spike D loses its consumer. See SPIKES.md.

**Plan:**

1. Record real touchpad MT frames with `libinput record` (or evemu-record) from any laptop touchpad.
2. Replay them through a uinput device built with: ABS_MT_SLOT, ABS_MT_TRACKING_ID, ABS_MT_POSITION_X/Y with resolution set, BTN_TOOL_FINGER/DOUBLETAP/TRIPLETAP, BTN_TOUCH, INPUT_PROP_POINTER. Verify udev assigns ID_INPUT_TOUCHPAD (`udevadm info`), then `libinput debug-events` must show GESTURE_SWIPE_BEGIN on replay.
3. Only after recorded replay works, synthesize frames from scratch (three contacts translating together) and check GNOME reacts (workspace swipe overview).
4. RESULT.md: GNOME + libinput versions, the device descriptor that worked, and which properties were required vs optional.

Replaying a recording first separates classification failures (device not accepted as a touchpad) from synthesis bugs (bad contact math).
