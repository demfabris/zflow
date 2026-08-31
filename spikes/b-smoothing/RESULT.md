# Spike B result: harness validation (real-link feel test still pending)

Date: 2026-08-31. Host: Ubuntu 26.4, GNOME Shell 50.1, Wayland, wired. This
records the loopback/synthetic-jitter validation of the harness itself, plus
one algorithm finding. The kill-criteria verdict (feel on the real link vs
lan-mouse) still needs the two-machine run.

## Setup

`jitter_lab.py`: synthetic sine motion (amplitude 300 px, period 1.5 s,
250 Hz frames) streamed over loopback through a deterministic spike schedule
(delivery frozen 120 ms at t=2.0/4.5/6.5 s, backlog released at once), one run
per mode, metrics computed from the receiver's `--log` injection log.

## Results

| mode | conservation | stall | burst10ms | pace p95 |
|---|---|---|---|---|
| raw | +261/+261 exact | 125.8 ms | 160 px | 4.1 ms |
| fixed 8 ms | +260/+260 exact | 118.0 ms | 147 px | 4.3 ms |
| adaptive | +261/+261 exact | 124.3 ms | 50 px | 4.3 ms |

- **Conservation is exact in every mode**: cumulative-total motion loses zero
  displacement through three 120 ms delivery freezes. The core protocol claim
  works.
- **Stall is identical across modes**, as theory says it must be: no playout
  buffer fills a 120 ms hole; smoothing shapes the recovery, not the gap.
- **Adaptive cuts the post-spike teleport 3.2x** (160 px → 50 px inside a
  10 ms window) at equal pacing. This is the measurable version of the
  product bet.

## Algorithm finding

The first capped-catch-up design (fixed drain fraction with absolute
floor/ceiling) measured 156 px burst10ms, indistinguishable from raw: absolute
constants compress a backlog ~13x, which reads as a teleport. The fix that
produced the 50 px result scales the per-tick cap to CATCHUP_SPEEDUP (3x) times
the sender's motion velocity, measured from frame **capture timestamps**, so
network bunching and output capping cannot inflate the estimate (no feedback
loop). This velocity-scaled rule is the candidate for SPEC's catch-up policy.

## Also verified in-session

- uinput virtual mouse gets `ID_INPUT_MOUSE=1` from udev and its events move
  the live GNOME Wayland cursor (udev → libinput → mutter accepted).
- Sender-restart detection via session id: reset logged, no backwards jump.
- Runtime mode switching resets playout state cleanly.

## Still open (needs two Linux machines on the jittery link)

The actual kill criteria: side-by-side feel vs raw and vs lan-mouse on real
WiFi with real hand motion. The harness is ready for it.
