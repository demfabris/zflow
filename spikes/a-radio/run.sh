#!/usr/bin/env bash
# Spike A: does steady keepalive traffic flatten WiFi RTT spikes?
#
# Methodology: the measurement ping is SPARSE (one probe every 1.13 s), because
# that is what an isolated input packet (a keystroke after idle) experiences.
# The keepalive traffic is the treatment. The odd 1.13 s interval avoids
# phase-locking with the round keepalive rates, so probes sample random phases
# of the keepalive cycle instead of always landing on a freshly woken radio.
# Steady-state motion smoothness is spike B's job, not this script's.
#
# Run it several times under different link conditions and label each arm:
#
#   ./run.sh macbook.local ps-on            # baseline, power save untouched
#   sudo iw dev <wlan> set power_save off
#   ./run.sh macbook.local ps-off
#   # on the mac: sudo ifconfig awdl0 down (re-enables itself, repeat as needed)
#   ./run.sh macbook.local ps-on-awdl-down
#
# ~45 s per arm, 5 arms per run. Results append to results.csv next to this
# script; failed arms are reported and skipped, never recorded as data.
# Verdict goes in RESULT.md.
#
# Caveat: ICMP keepalive proves the radio-wake mechanism, not zflow's exact
# traffic shape. Spike B re-checks the winning cadence with a real UDP stream.

set -euo pipefail
export LC_ALL=C

TARGET=${1:?usage: run.sh <target-host> <label>}
LABEL=${2:?usage: run.sh <target-host> <label>}
LABEL=${LABEL//,/-}   # keep the CSV parseable

MEAS_INTERVAL=1.13  # sparse, non-harmonic with all keepalive rates
MEAS_COUNT=40       # ~45 s per arm
# keepalive rates in Hz; "0" means no keepalive. Override order via env to
# control for link drift across arms: ZFLOW_RATES="100 50 20 10 0" ./run.sh ...
read -r -a RATES <<< "${ZFLOW_RATES:-0 10 20 50 100}"

DIR=$(cd "$(dirname "$0")" && pwd)
CSV="$DIR/results.csv"
STAMP=$(date +%Y-%m-%dT%H:%M:%S)

KEEPALIVE_PID=""
cleanup() {
  [[ -n "$KEEPALIVE_PID" ]] && kill "$KEEPALIVE_PID" 2>/dev/null || true
}
trap cleanup EXIT

IFACE=$(iw dev 2>/dev/null | awk '$1=="Interface"{print $2; exit}' || true)
if [[ -n "$IFACE" ]]; then
  PS_STATE=$(iw dev "$IFACE" get power_save 2>/dev/null | awk '{print $NF}' || echo "unknown")
  echo "wifi interface: $IFACE, power_save: $PS_STATE"
else
  echo "no wifi interface found via iw (wired? that's your control arm)"
fi

[[ -f "$CSV" ]] || echo "stamp,label,keepalive_hz,sent,recv,avg_ms,p95_ms,max_ms,mdev_ms,spikes_over_30ms,spikes_over_100ms" > "$CSV"

echo
printf "%-10s %6s %8s %8s %8s %8s %6s %6s\n" "keepalive" "recv" "avg" "p95" "max" "mdev" ">30ms" ">100ms"

for RATE in "${RATES[@]}"; do
  if [[ "$RATE" != "0" ]]; then
    INT=$(awk "BEGIN{printf \"%.3f\", 1/$RATE}")
    ping -q -i "$INT" "$TARGET" >/dev/null 2>&1 &
    KEEPALIVE_PID=$!
    sleep 2  # let the radio settle into the new traffic pattern
    if ! kill -0 "$KEEPALIVE_PID" 2>/dev/null; then
      echo "arm ${RATE}Hz FAILED: keepalive ping exited (interval ${INT}s rejected? old iputils needs root below 200ms) - skipping, not recorded"
      KEEPALIVE_PID=""
      continue
    fi
  fi

  OUT=$(ping -i "$MEAS_INTERVAL" -c "$MEAS_COUNT" "$TARGET" 2>&1 || true)

  if [[ -n "$KEEPALIVE_PID" ]]; then
    kill "$KEEPALIVE_PID" 2>/dev/null || true
    wait "$KEEPALIVE_PID" 2>/dev/null || true
    KEEPALIVE_PID=""
  fi

  STATS=$(echo "$OUT" | awk '
    /time=/ {
      for (i=1; i<=NF; i++) if ($i ~ /^time=/) {
        gsub("time=", "", $i); t[n++] = $i + 0
        if ($i+0 > 30) s30++
        if ($i+0 > 100) s100++
      }
    }
    /packets transmitted/ { sent = $1; recv = $4 }
    /rtt min\/avg\/max\/mdev/ {
      split($4, a, "/"); avg = a[2]; max = a[3]; mdev = a[4]
    }
    END {
      p95 = "-"
      if (n > 0) {
        # selection sort; mawk has no asort and n is tiny
        for (i = 0; i < n - 1; i++)
          for (j = i + 1; j < n; j++)
            if (t[j] < t[i]) { tmp = t[i]; t[i] = t[j]; t[j] = tmp }
        idx = int(n * 0.95); if (idx >= n) idx = n - 1
        p95 = t[idx]
      }
      printf "%s,%s,%s,%s,%s,%d,%d", sent+0, recv+0, avg, p95, max, s30+0, s100+0
      printf ",%s", mdev
    }')

  IFS=',' read -r SENT RECV AVG P95 MAX S30 S100 MDEV <<< "$STATS"
  if [[ -z "${RECV:-}" || "$RECV" == "0" || -z "${AVG:-}" ]]; then
    echo "arm ${RATE}Hz FAILED: no measurement replies parsed - skipping, not recorded"
    echo "--- ping output was:"
    echo "$OUT" | tail -5
    continue
  fi
  echo "$STAMP,$LABEL,$RATE,$SENT,$RECV,$AVG,$P95,$MAX,$MDEV,$S30,$S100" >> "$CSV"
  printf "%-10s %6s %8s %8s %8s %8s %6s %6s\n" "${RATE}Hz" "$RECV" "$AVG" "$P95" "$MAX" "$MDEV" "$S30" "$S100"
done

echo
echo "arm '$LABEL' appended to $CSV"
