#!/usr/bin/env python3
"""Controlled-jitter validation of the receiver's playout modes.

Streams a synthetic sine motion (cursor wiggles +-300px horizontally, returns
near start) through a deterministic spike schedule: delivery freezes for 120 ms
at fixed points, then the backlog arrives at once, mimicking a WiFi power-save
burst. Runs each mode against the same schedule and reports, per mode:

  conservation   injected displacement == sent displacement?
  stall          longest gap between injections during motion
  burst10ms      max displacement inside any 10 ms window (teleport-ness)
  pacing p95     inter-injection interval during steady motion

Usage: python3 jitter_lab.py [receiver-binary] [outdir]
"""

import math
import os
import random
import socket
import struct
import subprocess
import sys
import time

RECEIVER = sys.argv[1] if len(sys.argv) > 1 else "./target/debug/receiver"
OUTDIR = sys.argv[2] if len(sys.argv) > 2 else "/tmp"
PORT = 5599
DUR = 8.0
RATE = 250
AMP = 300.0
PERIOD = 1.5
SPIKES = [(2.0, 0.12), (4.5, 0.12), (6.5, 0.12)]  # (start_s, hold_s)


def run_mode(mode: str) -> tuple[str, int]:
    log = os.path.join(OUTDIR, f"inject_{mode}.csv")
    if os.path.exists(log):
        os.unlink(log)
    recv = subprocess.Popen(
        [RECEIVER, "--port", str(PORT), "--mode", mode, "--log", log],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    time.sleep(0.5)

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sess = random.getrandbits(63)
    seq = 0
    held: list[bytes] = []
    t0 = time.monotonic()
    final_total = 0
    try:
        while True:
            t = time.monotonic() - t0
            if t >= DUR:
                break
            total = round(AMP * math.sin(2 * math.pi * t / PERIOD))
            final_total = total
            pkt = struct.pack("<QQQqq", sess, seq, int(t * 1e6), total, 0)
            seq += 1
            in_hold = any(s <= t < s + h for (s, h) in SPIKES)
            if in_hold:
                held.append(pkt)
            else:
                for p in held:
                    sock.sendto(p, ("127.0.0.1", PORT))
                held.clear()
                sock.sendto(pkt, ("127.0.0.1", PORT))
            time.sleep(1.0 / RATE)
        for p in held:
            sock.sendto(p, ("127.0.0.1", PORT))
        time.sleep(1.0)  # let playout drain
    finally:
        recv.terminate()
        recv.wait()
    return log, final_total


def analyze(log: str, final_total: int, mode: str) -> None:
    rows = []
    with open(log) as f:
        for line in f:
            t, dx, dy = line.strip().split(",")
            rows.append((int(t), int(dx)))
    if len(rows) < 10:
        print(f"{mode:9s} FAILED: only {len(rows)} injections logged")
        return
    injected = sum(dx for _, dx in rows)
    # skip the first injection when measuring gaps (session baseline timing)
    gaps = [(rows[i][0] - rows[i - 1][0]) / 1000.0 for i in range(2, len(rows))]
    stall = max(gaps)
    gaps.sort()
    p95_pace = gaps[int(len(gaps) * 0.95)]
    # max |displacement| within any 10ms sliding window
    burst, j = 0, 0
    acc = 0
    for i in range(len(rows)):
        acc += rows[i][1]
        while rows[i][0] - rows[j][0] > 10_000:
            acc -= rows[j][1]
            j += 1
        burst = max(burst, abs(acc))
    print(
        f"{mode:9s} injections {len(rows):5d}  conservation {injected:+5d}/{final_total:+5d}"
        f"  stall {stall:7.1f} ms  burst10ms {burst:4d} px  pace-p95 {p95_pace:5.1f} ms"
    )


def main() -> None:
    print(f"spike schedule: {SPIKES} (hold, then backlog arrives at once)")
    print("cursor will wiggle +-300px horizontally for ~8s per mode\n")
    for mode in ["raw", "fixed", "adaptive"]:
        log, final_total = run_mode(mode)
        analyze(log, final_total, mode)


if __name__ == "__main__":
    main()
