# Spike B: smoothing feel

Does receiver-side playout buffering feel better than raw apply on the real jittery link? Kill criteria in `../../SPIKES.md`.

## Build

```
cargo build --release
```

## Run

On the machine you sit at (needs read access to /dev/input, i.e. `input` group or sudo):

```
./target/release/sender <receiver-ip>:5555 --grab
```

`--grab` freezes the local cursor so only the remote one moves (ctrl+c to get it back; the keyboard is never grabbed). Omit it to see both cursors.

On the target machine (needs write access to /dev/uinput; on a systemd desktop the seated user often has it via ACL, otherwise sudo):

```
./target/release/receiver --port 5555 --mode adaptive
```

Switch modes live by typing on the receiver's stdin (works over ssh):

```
raw          apply the instant a frame arrives (the lan-mouse baseline)
fixed        fixed playout delay, full jump after a burst
adaptive     p95-jitter delay + capped catch-up (the zflow bet)
delay 8      set the fixed-mode delay in ms
stats        print counters
```

## Protocol for the feel test

1. Leave WiFi power save ON so the link keeps its spikes (this spike wants the bad link).
2. Drive circles and small precise motions in each mode for a minute; flip modes several times in varied order. A mode switch resets playout state, so the remote cursor pauses for an instant right after switching; judge the motion after that, not the switch itself.
3. Same day, same link: repeat with lan-mouse and compare.
4. Record the verdict, the link's spike profile that day (spike A numbers), the receiver's printed counters per mode (gaps, queue depth, delay), and tuning notes in RESULT.md. If the subjective calls feel too close, the follow-up is a trace-replay harness (record one evdev session, replay it through all modes) rather than more live trials.

## What this is not

No encryption, no auth, no keyboard, no reliability layer: it exists to answer one feel question. The wire format is 32-byte frames of cumulative totals, matching the SPEC's MotionFrame idea, and nothing else survives into the real implementation.
