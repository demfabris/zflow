# Spike I: Mac injection

**Question.** Can the logged-in zflow app post input with `CGEventPost` well enough to be a receiver? That means pointer motion, clicks, drags, keys with modifiers, repeat, Caps Lock, scroll, media keys and display wake on macOS 27, without stalling the Mac's own trackpad.

**Kill criteria.** The spike fails if any of these hold:

- The Mac's own trackpad stalls while the probe posts, under every suppression setting: `suppress.hid_permit` and `suppress.global_permit` both FAIL, and the cursor feels stuck.
- Posted motion is accelerated or lands somewhere other than where it was sent, and no field turns that off (`delta.*`).
- A double-click or a drag cannot be produced (`click.double`, `click.drag`).
- Modifiers set on an event do not reach the app (`flags.click_with_flags`, `flags.key_with_flags`).

A fail means the Mac receiver needs a virtual HID device: Karabiner's DriverKit driver, or CoreHID with Apple's entitlement (`SPEC.md:581-590`). Decision 7 in ROADMAP.md changes with it. Every other check (background focus, Caps Lock, F13 to F15, media keys, wake) can only change a row of ROADMAP section 4, not the approach.

## What the probe does

`probe.swift` builds into a normal app, `build/zflow-inject-probe.app` (bundle id `dev.zflow.spike.inject-probe`), so you grant Accessibility to the probe alone and not to your terminal. It opens one window with a red ring in the middle. Every event the window receives is logged: type, click count, modifier flags, keycode, characters, scroll deltas, precise flag, phase, momentum phase, and whether the app and window were active.

Safety rules the code keeps:

- Every posted event carries a marker in `kCGEventSourceUserData`, so the log and the taps can tell posted input from your hands.
- A click or scroll is posted only after a hit test finds the probe window under that point. A key is posted only while the probe window is key. Otherwise the test stops with FAIL.
- Held buttons, keys, modifiers and the Caps Lock state are tracked and put back when each test ends, when the 60 s per-test watchdog fires, on Cmd+Q or closing the window, on `SIGINT`, `SIGTERM` and `SIGHUP`, and on a crash (best effort).

## Build

```
bash spikes/i-mac-inject/build.sh
```

It compiles with `swiftc -warnings-as-errors`, writes the Info.plist, signs ad-hoc (`codesign -s - --force`) and never runs the app.

## Grant Accessibility (by hand)

1. System Settings > Privacy & Security > Accessibility.
2. Click +, pick `spikes/i-mac-inject/build/zflow-inject-probe.app`, switch it on.
3. If `suppress` reports that it could not create its taps, add the app under Input Monitoring the same way.

**Every rebuild invalidates the grant.** The ad-hoc signature changes with each build, and macOS ties the grant to it. After a rebuild, remove the entry with the minus button and add the app again. To keep one grant across rebuilds, sign with a development identity: `SIGN_IDENTITY="Apple Development: ..." bash spikes/i-mac-inject/build.sh`.

Do not run `build/zflow-inject-probe.app/Contents/MacOS/zflow-inject-probe <test>` from a terminal. macOS would then check the terminal's grants, not the probe's, and a terminal with Accessibility would post for real. Only `--help` and `--dry` are safe to run that way: they post nothing and open no window.

## Run

`run.sh` launches the app with `open -n -W`, waits for it to quit, prints its output and keeps a copy in `out/` (ignored by git):

```
bash spikes/i-mac-inject/run.sh <test>...
```

The raw form, if you prefer it:

```
open -n -W --stdout "$PWD/out.txt" --stderr "$PWD/err.txt" spikes/i-mac-inject/build/zflow-inject-probe.app --args <test>
```

Before a run, keep other windows away from the middle of the screen, where the probe window opens. Keep your hands off the keyboard and trackpad unless the test asks for them. To stop early, press Cmd+Q in the probe, close its window, or run `pkill -TERM -f zflow-inject-probe`.

| Test | Takes | What you do |
|---|---|---|
| `suppress` | 50 s | Four 8 s arms with a countdown in the window. During each arm, circle slowly and steadily on the Magic Trackpad. Afterwards, note whether the cursor stalled in arm 2 and kept up in arms 3 and 4. |
| `delta` | 5 s | Hands off. |
| `click` | 25 s | Hands off. Finder takes focus five times and the probe tries to click itself back. If the window asks you to click it, do. |
| `flags` | 5 s | Hands off. |
| `iso` | 25 s | When asked, press the key left of 1, then the key between left Shift and Z if your keyboard has one. |
| `caps` | 10 s | Watch the Caps Lock light. The built-in keyboard only shows it with the lid open; the Magic Keyboard has its own. |
| `scroll` | 3 s | Hands off. |
| `keys` | 5 s | Hands off. Note whether brightness or anything else changed while F13 to F15 went out. |
| `all` | 3 min | The eight tests above, in that order. |
| `media` | 10 s | Opt-in. Watch the volume and brightness overlays. Volume is read before and after, and put back if it drifted. |
| `wake` | 20 s | Opt-in. The display sleeps and the screen may lock. Touch nothing for 15 s, then unlock. |
| `feel` | up to 55 s | Opt-in, with a pipe from the Ubuntu box. See below. |

`--dry` prints the environment and the plan without posting anything or opening a window:

```
spikes/i-mac-inject/build/zflow-inject-probe.app/Contents/MacOS/zflow-inject-probe --dry
```

The built-in display was not active on 2026-09-28 (only the external 3008x1692 screen was), so run `media` with the lid open if you want the brightness half to mean anything.

## feel: real Linux mouse deltas, 1:1

`feel` reads lines of `dx dy` from stdin and posts each one as a cursor move of exactly that many points, clamped to the displays. Feed it a real mouse on the Ubuntu box to judge how unaccelerated motion feels on the Mac. `open` takes a stdin file, not a pipe, so the stream goes through a FIFO. This pipe is written but untested:

```
fifo=/tmp/zflow-feel.fifo
mkfifo "$fifo"
ssh ubuntu 'sudo -n stdbuf -oL evtest /dev/input/by-id/<your-mouse>-event-mouse' \
  | awk '/\(REL_X\)/ {dx += $NF} /\(REL_Y\)/ {dy += $NF}
         /SYN_REPORT/ {if (dx || dy) {print dx, dy; fflush(); dx = dy = 0}}' \
  > "$fifo" &
STDIN="$fifo" bash spikes/i-mac-inject/run.sh feel --secs 50
kill %1 2>/dev/null; rm -f "$fifo"
```

Notes:

- Pick a mouse, which reports `REL_X` and `REL_Y`. A touchpad reports absolute positions and sends nothing here.
- `sudo -n` needs passwordless sudo for evtest; otherwise add your user to the `input` group and drop the sudo.
- `evtest --grab` keeps the Linux cursor still while you test.
- Move the Linux mouse and watch the Mac cursor. Then write down whether it felt slow, fast or jumpy compared with the Magic Trackpad.

## Reading the output

- `TEST <test> key=value ...`: measurements.
- `VERDICT <check> PASS|FAIL|MANUAL <note>`: one per check. MANUAL means you judge it.
- `EVENT ...`: every event the window received. `mark=1` means the probe posted it.
- `SAY ...`: what the window asked you to do.
- `RELEASE`, `RESTORE`, `WATCHDOG`, `SIGNAL`: the safety paths, which should stay quiet.
- `SUMMARY`: counts and every verdict again, at the end.

`rg '^(TEST|VERDICT)' spikes/i-mac-inject/out/*.txt` gives what goes into RESULT.md.

## What this is not

It does not test gestures, the login window, fast user switching, Secure Input, latency, or clamping across two real displays. Code here is throwaway; the numbers in RESULT.md are the point.
