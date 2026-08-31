# Spike D: Mac contact frames (stub)

**Question:** on the actual macbook and its current macOS version, does MultitouchSupport.framework deliver raw contact frames, including while a default-option (swallowing) CGEventTap is active?

**Kill criteria:** failure kills Mac-to-Linux gestures and moots the signed-baseline decision in SPEC.md.

**Plan:**

1. Fastest first check: build and run [OpenMultitouchSupport](https://github.com/Kyome22/OpenMultitouchSupport)'s demo, confirm contact frames with 3+ fingers.
2. Then a minimal tool linking the framework directly (headers: Karabiner-Elements `MultitouchPrivate.h`): `MTDeviceCreateList` + `MTRegisterContactFrameCallback`, print contact id/x/y/size per frame.
3. Add a CGEventTap with kCGEventTapOptionDefault that swallows mouse/scroll events, confirm contact frames still arrive while the tap eats the derived events (this is the zflow operating mode).
4. RESULT.md: macOS version, CPU family, framework symbols used, frame rate observed, and whether notarization of the test tool succeeds with the framework linked.
