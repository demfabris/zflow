# Handoff: zflow Mac session

You are an agent session running on fabrico's macbook. The project lives on
GitHub (private) and the primary session runs on the wired Linux box
(`ubuntu`, LAN `192.168.1.118`, also on tailscale `100.120.229.99`). The two
sessions cannot message each other; fabrico relays the "go" between terminals.
Push your results so the Linux side can pull them.

## What zflow is (30 seconds)

A software KVM (deskflow/lan-mouse alternative), Wayland + macOS only, whose
bet is staying smooth over jittery WiFi. Right now the project is in the
feasibility-spike phase: kill-risk experiments before implementation. Read, in
order: `SPIKES.md` (the experiment plan and kill criteria), then
`spikes/a-radio/RESULT.md` and `spikes/b-smoothing/RESULT.md` (findings so
far), then skim `SPEC.md` if you need design context. Don't refactor spike
code; it's throwaway by design. Findings are not: record everything in the
spike's RESULT.md, commit, push.

## Established findings you should not re-derive

- The link's baseline is genuinely bad: probing sparsely from the Linux box,
  p95 88-135 ms over a ~3 ms floor, a quarter of probes over 30 ms.
- Keepalive traffic sent TOWARD this mac made latency worse, monotonically
  with rate, in two order-controlled runs (AP queues frames behind this mac's
  WiFi doze schedule). Whether this mac's OWN transmission fixes its inbound
  path is YOUR job 2.
- The smoothing receiver was validated under synthetic jitter: exact
  displacement conservation, 3.2x smaller post-spike bursts in adaptive mode.
  The real-link feel verdict is YOUR job 1 (mac side is trivial).

## Job 1: relay for the real-link feel test (do this first, ~2 min of work)

The Linux box runs both sender and receiver; you bounce the frames so every
frame crosses the WiFi twice and picks up real radio jitter:

```
python3 spikes/b-smoothing/relay.py 5556 192.168.1.118:5555
```

Tell fabrico it's listening. The Linux session runs the rest and fabrico
drives the mouse. Leave it running until they say done. If port 5555 is wrong,
they'll give you the right one.

## Job 2: mac-originated keepalive arms (the decisive radio question)

Coordinate start with fabrico (the Linux side must be probing first, they'll
run `ping -D -i 1.13 macbook.local | tee probe.log`):

```
sudo ./spikes/a-radio/mac-keepalive.sh 192.168.1.118
```

Five arms, 60 s each, ~5.5 min total; the script prints UTC arm boundaries.
When it finishes, also capture context for the writeup:

```
pmset -g | grep -Ei 'powernap|sleep|network'
ifconfig awdl0 | grep status
```

Then, as separate labeled repeats worth doing if fabrico has time: rerun with
`sudo ifconfig awdl0 down` applied right before (it re-enables itself, note
whether it came back mid-run). Append everything, including the arm-boundary
timestamps, to `spikes/a-radio/RESULT.md` under a "mac-originated" heading;
the Linux side joins its probe log against your timestamps.

## Job 3: spike D, raw trackpad contact frames (the flagship-feature gate)

Read `spikes/d-multitouch-mac/README.md` for the plan and kill criteria.
Short version: prove the private MultitouchSupport.framework still delivers
raw contact frames on this exact machine and macOS version, with 3+ fingers,
and that frames keep arriving while a `kCGEventTapOptionDefault` event tap is
swallowing mouse/scroll events. Fast path: build and run the
OpenMultitouchSupport demo (github.com/Kyome22/OpenMultitouchSupport); needs
Xcode command line tools. Record macOS version, chip, frame rate observed, and
the tap-coexistence result in `spikes/d-multitouch-mac/RESULT.md`.

## Ground rules

- Results in RESULT.md files, committed and pushed; that's how the sessions
  share state.
- sudo actions: the two listed above (fast ping intervals, awdl0) are
  expected; ask fabrico before anything else privileged.
- Job 3's framework is private API; that's a settled, deliberate decision
  (see SPEC.md Gestures section), not something to relitigate.
- If something contradicts a documented finding, write down what you saw
  rather than adjusting the earlier numbers.
