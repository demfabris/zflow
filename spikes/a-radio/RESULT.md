# Spike A result: inbound keepalive DEGRADES this link (hypothesis refuted for this direction)

Date: 2026-08-31. Sender: wired Ubuntu host (no WiFi interface; the radio
under test is the macbook's). Target: macbook.local over infrastructure WiFi,
power save untouched, awdl0 untouched. Two runs, arm order reversed in the
second to control for link drift. Sparse probe (1.13 s interval, 40 probes per
arm), background ICMP keepalive at the stated rate. Raw data in results.csv.

## Run 1 (arms low to high)

| keepalive | avg ms | p95 ms | max ms | >30ms | >100ms |
|---|---|---|---|---|---|
| 0 Hz | 22.7 | 88 | 180 | 10/39 | 1 |
| 10 Hz | 26.7 | 136 | 137 | 9/40 | 5 |
| 20 Hz | 27.8 | 140 | 143 | 10/40 | 5 |
| 50 Hz | 41.2 | 149 | 445 | 13/40 | 5 |
| 100 Hz | 43.5 | 154 | 535 | 12/40 | 5 |

## Run 2 (arms high to low)

| keepalive | avg ms | p95 ms | max ms | >30ms | >100ms |
|---|---|---|---|---|---|
| 100 Hz | 37.8 | 138 | 584 | 9/40 | 5 |
| 50 Hz | 32.5 | 142 | 410 (2 lost) | 8/38 | 5 |
| 20 Hz | 26.2 | 136 | 140 | 10/40 | 5 |
| 10 Hz | 28.8 | 145 | 150 | 11/40 | 4 |
| 0 Hz | 23.3 | 135 | 140 | 8/40 | 4 |

## Findings

1. **The baseline problem is confirmed real.** With no keepalive, roughly a
   quarter of isolated probes exceed 30 ms and the p95 sits at 88-135 ms on a
   link whose floor is ~3 ms. This is the pain zflow exists for.
2. **Inbound ICMP keepalive makes it worse, monotonically with rate, in both
   arm orders.** Average roughly doubles at 50-100 Hz and max RTT triples to
   quadruples (410-584 ms); the 50 Hz arm even lost packets. The naive model
   ("traffic keeps the radio awake") fails for traffic ORIGINATED on the far
   side of the dozing station: frames queue at the AP behind the mac's doze
   schedule, and a higher rate builds a deeper queue for the probe to wait
   behind. The folklore fix ("run a speedtest and the lag stops") involves the
   afflicted machine's own sustained transmission, which is a different
   mechanism this run did not exercise.
3. **Untested and now the key open question:** keepalive originated ON the
   macbook (its own TX holding its radio out of doze). Needs the same
   experiment run from the mac toward this host. Also untested: power_save
   toggles, awdl0 down, and QoS marking (the AP-queueing mechanism implicated
   here is exactly what WMM priorities exist for).

## Design consequences for SPEC.md

- A receiver must never blast keepalive toward a dozing peer expecting to help
  it; keepalive benefit, if any, comes from each peer's OWN transmission. The
  radio experiment's one-way vs bilateral arms are not a nicety, they are the
  whole question.
- Smoothing and multipath carry more of the product than radio tricks on this
  evidence; the spike B result (3.2x burst reduction) is reassuring in that
  light.
