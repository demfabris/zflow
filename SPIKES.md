# zflow feasibility spikes

Kill-risk experiments that run before the implementation sequence in SPEC.md. Each spike answers one question that, answered "no", forces a rethink rather than a bugfix. Code here is throwaway; the evidence is not.

Convention: each spike lives in `spikes/<letter>-<name>/` with a `RESULT.md` recording verdict (pass / fail / partial), the raw numbers, hardware and OS versions, and the date. The adversarial review audits RESULT.md files, not claims.

| Spike | Question | Tier | Status |
|---|---|---|---|
| A | Does steady traffic flatten the real link's RTT spikes? | 0 | two order-controlled runs: inbound keepalive DEGRADES the link (RESULT.md); mac-originated arm pending |
| B | Does receiver-side smoothing feel better than raw on the real link? | 0 | harness validated with synthetic jitter (RESULT.md); real-link feel test pending |
| C | Does a uinput virtual touchpad trigger real GNOME gestures? | 1 | stub |
| D | Does MultitouchSupport deliver contact frames on the actual macbook? | 1 | stub |
| E | Does pre-login uinput injection work at the greeter? | 1 | classification half verified live (ID_INPUT_MOUSE + GNOME accepts); greeter half pending reboot test |
| F | Do quinn datagrams stay timely under jitter vs raw UDP? | 2 | pending |
| G | How big is the grab-on-demand arming leak? | 2 | pending |
| H | Does QoS marking change the over-the-air access category? | 2 | pending |

Order: A first (zero code, calibrates everything else), then B, C, D, E in parallel. Tier 2 only as their spec questions come due; H only if A shows traffic alone is not enough.

## Tier 0: the premise

### Spike A: radio wake (`spikes/a-radio/`)

**Question.** On the real ubuntu-to-macbook link, does keepalive traffic at 10-100 Hz remove the 40-150 ms RTT spikes, and how does that compare to `iw ... set power_save off` and to the Mac's awdl0 being down?

**Kill criteria.** If no arm gets max RTT under ~30 ms, smoothing must hide 150 ms alone, which no playout buffer does without floatiness. zflow survives, but "Sidecar feel on WiFi" demotes to "graceful on WiFi, great on cable" and multipath becomes the headline. Also feeds the radio-experiment constants in SPEC.md.

**Method.** `run.sh` measures with a sparse probe (one ping every 1.13 s, the keystroke-after-idle case, de-phased from the round keepalive rates) while a background ping generates each keepalive rate; repeat runs with power_save off and awdl0 down are labeled arms, and failed arms are rejected rather than recorded. Caveat: ICMP keepalive proves the radio-wake mechanism, not zflow's exact traffic; spike B's UDP stream re-checks the winner.

### Spike B: smoothing feel (`spikes/b-smoothing/`)

**Question.** With real mouse input streamed over the real link as cumulative totals, does a playout buffer with capped catch-up feel better than raw apply during a jitter burst, and better than lan-mouse on the same link?

**Kill criteria.** If the tuned adaptive mode still feels rubbery or laggy compared to raw apply, the core differentiator is gone and the rethink happens now. This is the product bet; nothing else proceeds past a failure here.

**Method.** `sender` reads the physical mouse via evdev and streams timestamped cumulative totals over UDP; `receiver` injects through a uinput virtual mouse in three runtime-switchable modes: `raw`, `fixed` delay, `adaptive` delay with capped catch-up. Sit at the sender machine, drive the receiver machine's cursor, flip modes over ssh while the link misbehaves (leave power_save on to keep the spikes). Compare against lan-mouse on the same link the same day.

## Tier 1: the flagship features

### Spike C: virtual touchpad gestures (`spikes/c-virtual-touchpad/`)

**Question.** Does a uinput type-B multitouch touchpad, fed synthetic 3-finger contact frames, make GNOME fire its native workspace swipe?

**Kill criteria.** Failure kills target-native gestures until compositors implement ei_gestures; the capability demotes and the Mac contact-capture spike (D) loses its consumer. libinput's touchpad requirements (udev ID_INPUT_TOUCHPAD, resolution, INPUT_PROP_POINTER, slot protocol) are the risky part, so the spike must replay recorded real-touchpad frames before synthetic ones to separate classification failures from synthesis bugs.

### Spike D: Mac contact frames (`spikes/d-multitouch-mac/`)

**Question.** On the actual macbook and its current macOS version, does MultitouchSupport.framework deliver contact frames, including while a CGEventTap with default (swallowing) options is active?

**Kill criteria.** Failure kills Mac-to-Linux gestures; the signed-baseline decision in SPEC.md becomes moot. Start from OpenMultitouchSupport or Karabiner's MultitouchExtension source; record framework symbols used, macOS version, and CPU family in RESULT.md.

### Spike E: pre-login injection (`spikes/e-prelogin/`)

**Question.** Can a root daemon's uinput keyboard, created before the display manager starts, type a password at the GDM/SDDM greeter, and do the virtual devices get correct udev/libinput classification?

**Kill criteria.** Failure would gut the backbone rationale (the portal path would return as a primary candidate). Expected to pass; it justifies too much of the architecture to leave untested. A systemd unit ordering check plus an injection script is enough.

## Tier 2: settles open spec questions

- **F: transport bench.** quinn datagrams vs raw UDP on the real link plus netem-induced jitter; watch whether quinn's congestion controller delays datagrams after a spike. Settles the raw-UDP reopening condition in SPEC.md. Reuse spike B's sender/receiver with a quinn transport swapped in.
- **G: arming leak.** Monitor a mouse and keyboard, wait for neutral, EVIOCGRAB, count events that reached the session between chord detection and grab. Feeds the leakage threshold TESTPLAN.md wants frozen.
- **H: QoS over the air.** Only if A shows traffic alone is insufficient: radiotap capture while toggling `SO_PRIORITY` / `NET_SERVICE_TYPE`, check the WMM access category actually changes on this AP.

## Explicitly not spiked

CGEventTap capture, portal/EIS capture, and the Karabiner vHID client protocol are proven daily by deskflow, lan-mouse, and Kanata; their risk is integration effort, not feasibility. The macOS LoginWindow TCC question and the CoreHID entitlement belong to capabilities already marked experiment in SPEC.md and wait for the macOS phase.
