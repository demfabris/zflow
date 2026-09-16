# zflow

> Specification v0.3, draft, 2026-08-31
>
> Implementation status: the headless Linux prototype is implemented. It is not yet a qualified alpha: the privileged, packaged two-host qualification run in [TESTPLAN.md](TESTPLAN.md) has not run.
> September 16 implementation: macOS uses a native SwiftUI menu-bar app linked to the Rust core through a C ABI. Linux uses a headless desktop agent and GNOME extension for cursor placement and return barriers. AWDL suppression uses a signed, same-team XPC service registered through SMAppService. Live two-host and signed-helper qualification remain outstanding; broader portal and macOS receiver matrices below are future work.
>
> v0.3 records three maintainer decisions after adversarial review: on-demand device grabs, raw-contact capture in the signed macOS baseline behind a flag, and the split of release matrices into [TESTPLAN.md](TESTPLAN.md).

zflow shares a keyboard, pointer, wheel, and selected trackpad data between Linux Wayland and macOS machines. It targets links with low baseline latency and short jitter bursts. The receiver preserves input state through loss, reordering, reconnects, and process failure.

The first product slice supports Linux-to-Linux keyboard, pointer, and high-resolution wheel input through explicit hotkey switching. Linux login-screen support, desktop edge switching, logged-in macOS support, native gestures, radio tuning, and path failover follow through measured gates.

Rust owns the core. Linux supports Wayland sessions and the kernel input path. zflow does not support an X11 host mode or Windows. Xwayland applications inside a Wayland session receive input through the compositor's normal path.

The experimental GNOME return path holds a Poll for up to 200 ms and responds
when the return barrier fires. The Mac starts the next poll after the reply,
with a 50 ms minimum start-to-start interval for receivers that reply at once.
The desktop broker uses seat state refreshed on its 250 ms tick for Poll;
Prepare, Finish and Snapshot refresh that state before and after the compositor
call. Existing request timeouts and the two-second desktop lease still apply.

## Reading this specification

The words MUST, MUST NOT, SHOULD, SHOULD NOT, and MAY define requirements.

Each platform claim has one of three statuses:

- **Required**: implementation work may rely on a standard, public API, or demonstrated kernel contract.
- **Provisional**: the design has credible evidence, but measurements must select parameters.
- **Experiment**: zflow makes no product promise until the named test passes.

Numeric bounds in this specification (the 250 ms checkpoint interval, the one-second lease cap, the two-second recovery default) are engineering defaults. Validation freezes final values before beta; the one-second lease cap alone is a hard ceiling.

Source links point to standards, platform documentation, upstream source, or a dated observation. Forum reports and issue trackers establish a reproduction to test; they do not establish a platform contract.

## Product contract

### Required behavior

zflow MUST:

1. Preserve key, button, modifier, and gesture lifecycle state across packet loss.
2. Release all receiver-owned input within one second of the last valid lease renewal.
3. Enqueue a reliable cumulative checkpoint within 250 ms after pointer or scroll totals change, apply it at its mapped playout deadline when delivered, and preserve the last acknowledged checkpoint after abrupt failure.
4. Reject events from an old session, old activation, revoked peer, or replayed handshake.
5. Return local input ownership within a configured, tested bound (default two seconds) after a killable userspace crash or hang while devices are grabbed.
6. Authenticate a peer before any network event reaches an input backend.
7. Require a separate, revocable permission before a peer may inject outside an unlocked, authenticated user session.
8. Measure latency, jitter, loss, reordering, recovery, and scheduler lateness without logging typed content.
9. Leave local input untouched while idle: no grabs, no event modification, and no local effect from a daemon crash or restart in the Idle state.

### Capability stages

| Capability | Status | Ship gate |
|---|---|---|
| Linux-to-Linux keyboard, pointer, wheel, hotkey switching | v1 baseline | Linux backbone matrix |
| Linux client at greeter, lock screen, and VT | required after validation | Linux pre-login matrix |
| Mid-session enrollment of new physical devices | v1 baseline | Linux backbone matrix |
| GNOME and KDE edge switching | provisional | Portal/EIS capture soak |
| wlroots and COSMIC edge switching | experiment | Layer-surface crossing matrix |
| Logged-in macOS capture and injection | planned | Signed macOS session matrix |
| macOS lock-screen and LoginWindow target injection | experiment | Per-backend signed secure-field matrix |
| macOS source capture during Secure Event Input | keyboard unsupported; pointer and scroll experiment | Per-event-class matrix |
| Linux virtual touchpad and native target gestures | experiment | libinput and compositor matrix |
| Raw Mac trackpad contact capture (signed build, feature flag) | experiment | Contact capture and replay matrix |
| AWDL suppression during sessions (macOS, opt-in) | planned, mechanism proven | Consent flow + re-apply loop |
| QoS marking | provisional | Radio and energy matrix |
| Automatic path failover | planned | Candidate-racing failure matrix |
| Clipboard and file transfer | deferred | Separate bulk transport |

Linux pre-login scope begins in real-root userspace once zflowd is active. Graphical-greeter support begins when the display manager starts; VT keyboard support may begin earlier. Firmware, initramfs, LUKS, macOS FileVault preboot, and other environments where zflow code cannot run remain excluded.

## Evidence and motivation

### The observed link

A typical failing trace has a clean baseline, no packet loss, and short latency bursts:

~~~~text
3.80 ms
90.1 ms
3.65 ms
137 ms
101 ms
151 ms
~~~~

The trace proves jitter. It does not identify the cause. Wi-Fi power saving, RF retries, scanning, roaming, AP queues, and Apple Wireless Direct Link coexistence can produce similar timing.

Linux mac80211 uses a dynamic power-save timeout on drivers that support that mode; its documented default is 100 ms. A shorter traffic cadence can keep those devices awake, but driver and firmware behavior varies. See the [Linux wireless dynamic power-save documentation](https://wireless.docs.kernel.org/en/latest/en/users/documentation/dynamic-power-save.html).

Reverse-engineering research describes AWDL availability windows on the shared Wi-Fi radio. See the [AWDL security analysis](https://www.usenix.org/conference/usenixsecurity19/presentation/stute). Developers have also reported latency changes correlated with a private “realtimeMode.” Apple DTS confirmed that no documented API enables that mode; DTS did not confirm a universal cadence or causal explanation. See the [Apple Developer Forums report and DTS response](https://developer.apple.com/forums/thread/819926).

zflow treats radio wake traffic, QoS, and smoothing constants as measurements. Runtime diagnostics say that a trace is “consistent with power saving or radio coexistence” and offer controlled checks. They MUST NOT claim a cause from RTT alone.

### Existing tools

This table records a source snapshot from 2026-08-31. Maintainers must refresh it before external publication.

| Project | Transport and current finding | Relevance to zflow |
|---|---|---|
| [Deskflow](https://github.com/deskflow/deskflow) | Its [protocol reference](https://github.com/deskflow/deskflow/blob/master/docs/dev/protocol_reference.md) uses TCP port 24800. The project lists Wayland as supported. | One ordered byte stream can delay fresh input behind lost or bulk data. |
| [Input Leap](https://github.com/input-leap/input-leap) | The maintainers archived the repository in July 2026. It inherits the Synergy TCP protocol family. | zflow cannot depend on future maintenance there. |
| [Lan Mouse](https://github.com/feschber/lan-mouse/tree/6b1eddef7ab3ae0b06f2ed3d9b21165e03ea4cad) | Current main sends input as DTLS application datagrams. It tracks held keys and releases them on teardown, but it has no per-event delivery or periodic button reconciliation. An isolated lost release can persist until cleanup. | Its input-capture, input-emulation, input-event, and protocol crates may save platform work. |
| [rkvm](https://github.com/htrefil/rkvm) | It uses TCP with TLS and has seen little source activity since 2024. | Its hotkey model remains useful for session-free switching. |

### The unserved gap

The reproductions below establish demand, not contracts, but the pattern is consistent. Synergy's vendor documents its lag fix as [switching to an ethernet connection](https://symless.com/synergy/help/fix-lagging-issues-using-ethernet). A Barrier user reports that wireless lag disappears "unless a speedtest is running in the background" ([barrier #1755](https://github.com/debauchee/barrier/issues/1755)), which is the radio-wake mechanism zflow probes deliberately. A Lan Mouse contributor concludes that on lossy networks "there is no way around these types of problem (short of plugging the laptop into corded network)" ([lan-mouse #307](https://github.com/feschber/lan-mouse/issues/307)).

Interactive-input transport over lossy links has decades of working practice in games: [Quake 3's redundant unacked sends](https://fabiensanglard.net/quake3/network.php), [GGPO's per-packet input history](https://www.ggpo.net/), and [Source's interpolation delay](https://developer.valvesoftware.com/wiki/Source_Multiplayer_Networking). No software KVM in the table applies any of it. That gap is the product.

zflow's differentiators are bounded receiver-side state recovery, cumulative motion over unreliable delivery, measured playout, pre-login Linux injection, and explicit platform capability gates.

### Reuse gate

Before zflow writes Linux and macOS backends from zero, one bounded spike MUST adapt the zflow event model to Lan Mouse's input-event, input-capture, and input-emulation crates. The result must record:

- API and lifecycle fit;
- missing greeter, ownership, gesture, and failure semantics;
- maintenance cost against an independent backend;
- GPL-3.0 compatibility with zflow's chosen license.

The team will reuse code only after choosing a compatible license. It may still keep zflow as a separate product and protocol.

## Architecture

zflow separates system authority, session integration, and the pure protocol state machine.

~~~~text
                    authenticated QUIC
       ┌─────────────────────────────────────────┐
       │                                         │
┌──────▼─────────────────┐             ┌─────────▼──────────────┐
│ zflowd                 │             │ zflowd                 │
│                        │             │                        │
│ Linux peer/session     │             │ macOS peer/session     │
│ transport and probes   │             │ transport and probes   │
│ playout scheduler      │             │ console-session owner  │
│ on-demand evdev grabs  │             │ optional root vHID     │
│ uinput injection       │             │ client                 │
└──────────▲─────────────┘             └──────────▲─────────────┘
           │ authenticated local IPC              │
┌──────────▼─────────────┐             ┌──────────▼─────────────┐
│ Linux zflow-session    │             │ macOS session agent    │
│                        │             │                        │
│ geometry and UI        │             │ WindowServer capture   │
│ portal/EIS bridge      │             │ WindowServer posting   │
│ local consent          │             │ TCC-facing lifecycle   │
└────────────────────────┘             └────────────────────────┘
~~~~

The core state machine MUST run without an active desktop session. Platform adapters translate between native events and a neutral wire model. The transport never calls an input backend before the session and permission checks pass.

The implementation SHOULD begin as one Rust workspace with the fewest crate boundaries that preserve testability. Split platform helpers only where the operating system requires a separate process or language boundary.

## Input ownership

On Linux, physical devices stay on their normal kernel path while zflow is idle. zflowd monitors the configured capture set read-only to detect the activation chord and to track key state; monitoring never blocks, delays, or alters local delivery.

Each input source follows four routing states:

1. **Idle**: no grabs exist. Local input flows normally. A daemon crash, restart, or package upgrade in this state cannot affect local input, and a restarted daemon is immediately fully functional.
2. **Arming**: zflow has selected and authenticated a peer and an activation was requested (hotkey chord, or a portal edge event). For the hotkey path, zflowd waits until every key and button across the capture set is neutral (verified through EVIOCGKEY and tracked state), then takes EVIOCGRAB on every node in the set, all or none. EBUSY or any failure releases acquired grabs and returns to Idle with a diagnostic.
3. **Remote**: grabbed physical events serialize to the peer. The local escape chord works without network cooperation because the daemon reads every event.
4. **Releasing**: zflow queues a terminal state when the connection remains live, ungrabs the set at a complete SYN_REPORT boundary, and the physical devices return to normal kernel consumers. The receiver cleans up through that terminal state or its lease and lifecycle rules.

**Accepted arming race**: between chord detection and a successful grab, the compositor may observe the tail of the chord and in-flight motion. Arming at neutral bounds the leak to the chord itself plus sub-frame motion; the worst user-visible effect is the chord firing a local binding or a few pixels of local cursor motion at switch time. Setup tests the chosen chord and reports known local conflicts. The Linux backbone matrix measures leakage and freezes an acceptance threshold.

**Decision (2026-08-31)**: the alternative permanent-grab-and-relay design removes this race entirely, but it routes all local input through the daemon at all times, leaves sharing disabled after any daemon restart until the next reboot, and delays new-device sharing to the next boot. The maintainer chose on-demand grabs: the arming leak is bounded and cosmetic, and the operational costs of the alternative are not. Mid-session enrollment becomes baseline behavior as a direct consequence: a device added to the capture-set configuration participates at the next Arming transition.

Portal edge activation performs no evdev grab at all. The compositor diverts input into EIS, which is the sole capture source for a portal-controlled Remote phase. zflowd MUST NOT hold grabs on the same capture set while a portal activation is live, and MUST NOT forward a physical copy in parallel with EIS events.

The service-manager watchdog kills a killable hung daemon within the configured bound; closing the daemon removes its virtual devices and releases any physical grabs, returning the devices to normal kernel consumers. While Idle there is nothing to release.

## Session and wire protocol

### Versioning and limits

Each input-session message carries a protocol version, message type, payload length, session epoch, transport generation, and activation identifier. Control and motion messages add their channel-specific sequences. Pairing uses handshake-scoped nonces and transcript identifiers before an input session exists.

The decoder MUST:

- reject unknown required features;
- cap message, collection, contact, and string lengths before allocation;
- reject duplicate identifiers and invalid state transitions;
- tolerate unknown optional fields;
- expose a fuzz target for every message family.

The wire encoding remains open until the protocol simulator compares at least two bounded, schema-driven options. The logical model below is fixed for the first prototype.

Authenticated session negotiation selects one protocol version, maximum datagram size, input capabilities, pointer units, scroll fields, contact limit, receiver lease, and checkpoint bound. A required capability mismatch prevents activation.

### Identity, epoch, generation, and activation

A paired peer owns a long-term public identity key. A new process session creates a random 128-bit session epoch. A security-boundary transition such as a macOS console-session change also creates a new epoch and releases prior state. Each accepted transport within an epoch has a strictly increasing generation. Each transition from local to remote input creates a monotonic activation identifier within that epoch.

The receiver MUST release state from the prior epoch before accepting a new epoch. It keeps one accepted transport generation per live session and rejects traffic from older generations. It MUST ignore messages for a closed activation even if those messages pass QUIC authentication.

### Channel split

| Channel | QUIC primitive | Contents |
|---|---|---|
| Input control | One reliable ordered bidirectional stream | key and button transitions, ownership, scroll and touch lifecycle, state snapshots, acknowledgements, terminal anchors |
| Motion state | QUIC datagrams | cumulative pointer and scroll state, complete touch snapshots |
| Probe | QUIC datagrams | application probe and echo data |
| Bulk | separate best-effort QUIC connection and socket | clipboard or file data after v1 |

Input control MUST NOT share a stream with clipboard or file data. If zflow applies a socket service class, bulk traffic MUST use an unmarked endpoint.

QUIC retransmission cannot deliver an event after connection death. The receiver owns failure recovery.

### Reliable control

The input-control stream carries:

- Enter and Leave;
- KeyDown and KeyUp;
- ButtonDown and ButtonUp;
- ScrollBegin, ScrollEnd, and ScrollCancel;
- TouchBegin, TouchEnd, and TouchCancel;
- StateSnapshot;
- SnapshotAck;
- SessionTakeover and SessionClose.

Each transition carries a control sequence. Pointer-sensitive transitions, including button events and Leave, also carry a MotionAnchor:

~~~~text
MotionAnchor {
    activation_id
    through_motion_sequence
    sender_capture_time
    total_dx
    total_dy
    total_scroll_x
    total_scroll_y
    final_touch_state
}
~~~~

The receiver applies the anchor before the transition, so a click cannot overtake the motion that positioned it.

StateSnapshot contains the full pressed-key, pressed-button, modifier, active-scroll, and active-touch state plus a MotionAnchor. The sender enqueues a snapshot on the critical control stream within 250 ms after cumulative totals change and at least every 250 ms while an activation remains live. Repeated datagrams may repair state before that checkpoint. SnapshotAck names the snapshot control sequence and accepted transport generation.

The receiver:

1. Applies control events in stream order.
2. Reconciles each authoritative snapshot by difference and leaves matching held inputs unchanged.
3. Releases all owned state on SessionClose, stream reset, connection loss, lease expiry, backend teardown, or epoch replacement.
4. Performs release through its local backend. It does not depend on a “release-all” packet crossing a dead network.

The receiver advertises its held-state lease duration during authenticated session negotiation and repeats it in TakeoverAccepted. The duration MUST be at most one second. Applying a valid transition or authoritative snapshot that leaves state held sets the deadline from the receiver's monotonic clock. Sender timestamps never set it. On expiry, suspend, or resume, the receiver releases all owned state, closes the activation, and rejects later messages for that activation. Only a new activation may inject again.

The sender renews held state before one third of the lease duration and requires SnapshotAck. If it receives no acknowledgement before the advertised deadline, it exits Remote ownership. A peer may renew through valid snapshots but cannot increase the receiver-configured duration.

### Cumulative motion datagrams

Independent relative deltas cannot use latest-wins delivery. A lost delta loses displacement. Each MotionFrame therefore carries cumulative state from the start of its activation:

~~~~text
MotionFrame {
    session_epoch
    transport_generation
    activation_id
    motion_sequence
    control_watermark
    sender_capture_time
    total_dx
    total_dy
    total_scroll_x
    total_scroll_y
    optional_touch_snapshot
}
~~~~

The receiver maintains separate highest-seen and last-injected sequences and totals. It stores cumulative frames and computes displacement only when a frame matures; it never computes a delta against totals that have not reached the input backend. A later frame or reliable checkpoint repairs displacement omitted by a lost frame.

Applying a MotionAnchor retires queued frames through its sequence, reconciles exactly to its totals, advances last-injected state, then applies its control transition. A terminal anchor rejects all later frames through its cutoff. Totals use checked signed 64-bit counters; overflow closes the activation.

The control watermark names the highest control sequence whose semantic effect precedes the motion sample. The capture state machine snapshots totals, motion sequence, and watermark at one capture-ordering point. Transport task scheduling does not define causality. The receiver waits until it has applied the watermark before it applies the datagram.

Touch data uses complete contact snapshots with stable contact identifiers. A newer snapshot replaces an older snapshot. TouchEnd carries the last complete non-empty snapshot, terminal capture time, and motion cutoff; the receiver applies it and lifts those contacts as one operation. TouchCancel may discard remaining contacts. The negotiated contact count and encoding MUST fit one QUIC datagram's maximum size or the path disables touch forwarding.

The baseline does not discard stale cumulative displacement. The smoothing experiments may add an explicit rebase policy, but any rebase must:

- record the discarded displacement in metrics;
- terminate old motion history at a named sequence;
- prevent delayed frames from restoring discarded state;
- pass user testing before it becomes a default.

### Clock model

Sender monotonic timestamps do not share an origin with receiver timestamps. The receiver estimates an affine clock mapping with offset and skew from probe exchanges. It uses that mapping for playout order and delay variation, not for security decisions.

The clock estimator MUST expose offset, skew, residual error, and reset count. It resets after suspend, a monotonic discontinuity, or a session epoch change.

## Input semantics

### Keyboard and media controls

The wire uses USB HID usage page and usage identifiers:

- Keyboard/Keypad page for ordinary keys;
- Consumer page for media and brightness controls;
- a negotiated vendor extension for Fn or Globe only if a platform experiment proves a need.

Linux maps evdev codes at capture and injection boundaries. macOS maps CG virtual keycodes, system-defined media payloads, and HID usages at its boundaries.

The target interprets physical keys through its active layout. A US physical key sent to an ABNT target behaves as if the same physical keyboard were plugged into that target. Text transfer and layout synthesis remain outside v1.

### Pointer

Each source declares whether its deltas represent device-like unaccelerated motion or desktop-accelerated motion. A session MUST NOT mix those units without a new activation.

Linux captures EV_REL device deltas and injects them through a relative uinput device so the target input stack applies its policy. macOS exposes several delta fields; Apple documents CG mouse delta fields as change since the prior event, not as a raw-device guarantee. The macOS adapter must record and compare standard and unaccelerated fields before selecting a mapping. See [Apple's CGEventField documentation](https://developer.apple.com/documentation/coregraphics/cgeventfield).

### Scroll

The wire model carries:

- high-resolution horizontal and vertical totals;
- source unit and resolution;
- discrete step information when present;
- begin, update, end, cancel, and momentum phase when the source exposes them.

A backend advertises its supported subset. It MUST NOT invent phase or momentum for a source that lacks those semantics.

### Touch

A touch snapshot carries contact identifier, position, pressure, major/minor size, orientation, tool type, and source dimensions when available. The target backend maps that snapshot to a virtual touch device.

Touch forwarding remains an experiment because Linux libinput requires a correctly classified touchpad and macOS lacks a public high-level gesture injection constructor.

## Transport and path management

### QUIC

zflow uses QUIC through [Quinn](https://github.com/quinn-rs/quinn) for the first prototype. QUIC supplies TLS 1.3, reliable streams, unreliable datagrams, congestion control, and standard connection lifecycle.

[RFC 9221](https://www.rfc-editor.org/rfc/rfc9221.html) requires QUIC datagrams to follow congestion control. Low average bitrate does not bypass congestion, pacing, shared packet loss, or a reduced congestion window.

The input connection:

- enables datagrams;
- disables 0-RTT input and control;
- keeps the critical stream separate from bulk data;
- prefers fresh cumulative motion when the local datagram queue fills;
- closes the input session on a critical-stream reset.

Raw UDP plus a custom reliability and cryptographic layer is not a planned fallback. A benchmark may reopen that decision only if it identifies a measured QUIC limit that configuration or an upstream change cannot solve.

### Candidate racing and failover

QUIC v1 supports address validation and constrained connection migration. It does not provide the proposed arbitrary multipath system. See [RFC 9000 connection migration](https://www.rfc-editor.org/rfc/rfc9000.html#section-9), [RFC 9308](https://www.rfc-editor.org/rfc/rfc9308.html), and Quinn's [Endpoint rebind API](https://docs.rs/quinn/latest/quinn/struct.Endpoint.html#method.rebind).

zflow v1 uses application-level candidate racing:

1. Discovery yields candidate addresses.
2. Each local candidate gets an Endpoint and UDP socket bound to that address or interface.
3. The initiating peer races full authenticated QUIC handshakes across candidate pairs. V1 does not define a pre-QUIC probe protocol.
4. The peers select one input connection and may retain established alternatives for authenticated health probes.
5. A better or surviving connection proposes SessionTakeover within the same epoch and activation using the next transport generation.
6. The old connection closes after the receiver accepts the takeover.

Endpoint::rebind does not implement simultaneous racing because it replaces an endpoint's socket for all connections.

SessionTakeover names the prior generation, proposed next generation, random proposal nonce, last control sequence, final MotionAnchor, and authoritative held-state snapshot. The new connection cannot inject before TakeoverAccepted. Under one receiver lock, the receiver:

1. authenticates the same peer identity and capabilities;
2. accepts the first valid proposal for the exact next generation and rejects competing proposals for that generation;
3. reconciles the old and new held sets by difference without releasing unchanged inputs;
4. binds the activation to the new generation;
5. rejects traffic from older generations;
6. acknowledges the takeover.

The user may plug in Ethernet or Thunderbolt mid-session. zflow may move the session after it authenticates and validates a new connection. The product does not promise a single QUIC connection that “jumps” between arbitrary address pairs.

### Discovery

mDNS advertises an ephemeral instance identifier, protocol version, capability summary, and connection candidates. It MUST NOT advertise a long-lived certificate fingerprint.

DNS-SD data remains untrusted until pairing or known-peer authentication completes. Long-lived identifiers in multicast records expose device identity to passive observers; [RFC 8882](https://www.rfc-editor.org/rfc/rfc8882.html#section-3.2) describes that privacy risk.

Known peers may derive rotating discovery tokens. Discovery loss cannot revoke or authorize a peer.

## Radio behavior and QoS

### Application probes

Quinn's keepalive timer prevents idle timeout after inactivity; it does not create a fixed bilateral sampling stream. zflow uses an explicit authenticated Probe and ProbeEcho datagram when it needs RTT, path health, or a radio-wake cadence. See [Quinn TransportConfig](https://docs.rs/quinn/latest/quinn/struct.TransportConfig.html#method.keep_alive_interval).

The first radio experiment compares:

- no active probe;
- 5, 10, 20, 50, and 100 Hz;
- one-way and bilateral traffic;
- infrastructure Wi-Fi, wired control, Linux power save on/off, and observed Apple AWDL coexistence.

A 20-byte payload at 50 Hz consumes 8 kbit/s per direction before QUIC, UDP, IP, Wi-Fi framing, acknowledgements, retries, and radio wake cost.

The selected cadence MAY vary by path and platform. It runs only while a remote-input activation is live and stops on release.

Measured 2026-08-31 (spikes/a-radio/RESULT.md): on the reference link, keepalive DEGRADED latency in BOTH directions: wired-host-originated (monotonic with rate, both arm orders) and mac-originated (100 Hz made jitter worse and unstable). Traffic-based radio wake is dead on this evidence; a peer MUST NOT send wake traffic on behalf of the other side, and zflow ships no keepalive-as-treatment without a link-specific measurement proving it helps. Probes remain for RTT/path measurement only.

The same session identified the dominant jitter source: **AWDL**. `ifconfig awdl0 down` on the mac collapsed p80 delay variation 40x (to 1-3 ms) and stopped frame loss entirely, with the adaptive playout delay self-settling from 35 ms to 3 ms. The macOS radio layer is therefore **AWDL suppression**: the root daemon MAY hold awdl0 down while a remote-input session is active, strictly opt-in with a clear consent flow, restoring it on session end, with the documented cost that AirDrop, Handoff, and Universal Control are unavailable while suppressed. Diagnostics detect the AWDL stall signature and suggest enabling this. macOS may re-enable awdl0 on its own; the daemon re-applies while the session stays active.

### Service class

zflow starts with best-effort traffic. It tests a standards-aligned real-time interactive class, including DSCP CS4 / WMM AC_VI, against best effort and voice treatment. [RFC 8325](https://www.rfc-editor.org/rfc/rfc8325.html#section-4.2.4) recommends mapping real-time interactive CS4 to UP4/AC_VI and voice EF to UP6/AC_VO; host, AP maps, and network policy may produce different treatment.

Apple says network service types describe traffic characteristics and warns that marking bulk traffic as voice can cause loss in smaller queues. See [QA1934](https://developer.apple.com/library/archive/qa/qa1934/_index.html) and the current [XNU service-type definitions](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/sys/socket.h#L218-L308). Linux wireless mapping depends on socket fields, qdisc policy, driver behavior, and AP mapping; see the [Linux wireless QoS documentation](https://wireless.docs.kernel.org/en/latest/en/developers/documentation/qos.html).

A successful setsockopt call does not prove over-the-air priority. The experiment MUST capture DSCP plus radiotap user priority/access category in both directions and test concurrent bulk traffic. zflow ships QoS marking only on combinations where the capture and latency data show a benefit without starvation or excess loss.

## Receiver playout

The receiver queues motion against its mapped sender clock and uses a monotonic high-resolution scheduler. A system daemon has no portable display-frame callback, so the design does not depend on compositor frame events.

The baseline implements:

- no motion prediction;
- cumulative-state recovery;
- a fixed configurable playout delay;
- bounded catch-up that preserves the final cumulative position;
- immediate reliable key and button transitions after their motion anchor matures.

The adaptive experiment compares a fixed delay with a delay based on a recent packet-delay percentile plus scheduler margin. It measures fast growth, slow contraction, late frames, overshoot, and recovery. It does not freeze the original 1-5 ms, 20 ms, 30-40 ms, or 250 ms constants without trace evidence.

Field results so far (2026-08-31, spikes/b-smoothing/RESULT.md): adaptive with velocity-scaled catch-up beat raw and fixed at every tuning in a live hand test on a hostile link; the percentile and delay cap MUST be runtime-tunable (p80/35 ms beat p95/80 ms untreated); and on a treated link the delay self-settled to 3 ms, so a correctly adapting estimator needs no per-link configuration.

The [1 Euro filter](https://gery.casiez.net/1euro/) remains an optional clock-estimator experiment. zflow adds it only if trace replay improves residual clock error or catch-up behavior against a simpler affine fit.

## Linux

### Kernel backbone

Linux uses evdev for physical capture and uinput for remote injection. EVIOCGRAB makes one evdev handle the sole event recipient for a device; closing the handle releases the grab. See the [kernel input subsystem documentation](https://docs.kernel.org/driver-api/input.html). The kernel delivers uinput events to userspace and in-kernel consumers; see the [uinput documentation](https://docs.kernel.org/input/uinput.html).

zflowd grabs the configured capture set on demand at the Arming transition and releases it when Remote ends, per the Input ownership section. While Idle it holds no grabs and reads the set only to detect the activation chord and track state. This keeps the daemon out of the local input path, keeps upgrades and restarts harmless, and lets a hotplugged device join the set without a reboot; the cost is the bounded arming leak, measured by the backbone matrix.

zflowd runs as a dedicated service account unless a platform test proves that root is required. The account may read selected evdev nodes and open /dev/uinput. Membership in the broad input group grants keylogger-level access, so installers MUST NOT add interactive users to it.

The installer ensures that uinput exists as a built-in driver or loaded module, grants zflowd write access to /dev/uinput, read access to selected evdev nodes, and write access only to nodes that need negotiated LED feedback. It verifies those permissions after a cold boot.

The service starts from multi-user.target and retries network discovery without blocking the display manager on network-online.target. When pre-login support is enabled, zflowd orders before the enabled display-manager unit and uses Type=notify. It sends READY=1 only after /dev/uinput is accessible, the baseline virtual devices exist, and their matching udev add events expose the required properties, so a pre-login client can receive input before the greeter appears. Network availability does not delay readiness. A daemon start at any later time is equally functional; no state depends on starting before the session.

zflowd uses WatchdogSec, WatchdogSignal=SIGKILL, KillMode=control-group, and Restart=on-failure. The capture event loop emits watchdog notifications; a separate liveness thread MUST NOT mask a stalled input loop. Capture descriptors use O_CLOEXEC and helpers never inherit them. The default local recovery bound for a killable userspace hang while devices are grabbed is two seconds, including watchdog expiry. An uninterruptible kernel task or service-manager failure can exceed that bound. While Idle a hang affects nothing local. See the [systemd service documentation](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html).

Service hardening includes NoNewPrivileges, a protected state directory, filesystem restrictions, AF_UNIX, AF_INET, and AF_INET6. It retains AF_NETLINK when libudev monitors hotplug. PrivateDevices=yes is forbidden because it hides input nodes.

The effective daemon account owns its identity keys in a mode 0700 state directory with mode 0600 files.

### Virtual devices

The baseline creates a remote-injection keyboard and pointer pair. The touchpad remains behind an experiment flag. No local-relay devices exist; local input never passes through zflow.

When available, Linux touch reports carry receiver-mapped capture timestamps in CLOCK_MONOTONIC, including touch state from reliable anchors. The backend bounds timestamps by its previous report and the current time; unstamped begins and emergency cleanup use the current time. Delivery never waits for this metadata. This preserves capture intervals through network bursts without inventing intermediate contacts. Kernel timestamp limits still apply; see [uinput timestamp validation](https://github.com/torvalds/linux/blob/v7.0/drivers/input/misc/uinput.c#L615).

Each uinput device has stable zflow vendor, product, role, name, and physical identifiers. evdev monitoring and capture filters exclude those identifiers to prevent loops, and portal/EIS source capture MUST never capture a zflow remote-injection device.

Before UI_DEV_CREATE, each virtual device advertises every event code supported by its negotiated HID mapping. Session validation rejects an unsupported usage before backend injection, closes only the offending peer, and releases its held state. A backend I/O failure still stops the descriptor-owning runtime so device destruction can clear uncertain kernel state. A capability change requires device recreation while input is neutral. The pointer advertises REL_X, REL_Y, required buttons, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES, and matching legacy wheel events required by the [Linux event-code contract](https://docs.kernel.org/input/event-codes.html).

The installer and test matrix verify:

- ID_INPUT_KEYBOARD for the keyboard;
- ID_INPUT_MOUSE for the pointer;
- seat assignment;
- hotplug readiness after UI_DEV_CREATE;
- ID_INPUT_TOUCHPAD, contact slots, axes, resolution, dimensions, and input properties for the touchpad experiment.

libinput requires udev classification and device-specific properties; a uinput node alone does not guarantee a usable desktop device. See [libinput device configuration](https://wayland.freedesktop.org/libinput/doc/latest/device-configuration-via-udev.html) and [touchpad requirements](https://wayland.freedesktop.org/libinput/doc/latest/touchpads.html).

### Capture

A capture set is all-or-none: at Arming, zflowd opens and grabs every event node in the configured logical set, and EVIOCGRAB returning EBUSY or any setup failure releases the whole set and returns to Idle with a diagnostic.

The daemon handles:

- composite keyboards with several event nodes;
- aggregate key and button state across the set;
- competing grabbers;
- suspend and resume, which close any live activation and return to Idle;
- optional LED feedback: it MAY mirror negotiated receiver LED state to compatible physical devices during Remote.

Grabs begin and route changes occur at a complete SYN_REPORT boundary after aggregate neutral state. Remote release consumes the escape sequence, closes receiver state, then ungrabs.

A device added to the capture-set configuration participates at the next Arming transition; no reboot or daemon restart is required. Removing a captured device mid-activation closes the activation, removes that device from the set, and reconciles local and remote state.

A process crash closes physical and virtual descriptors. The kernel releases grabs and removes virtual devices, so normal consumers resume physical input at once. A stopped or wedged process retains its grabs only during Remote; the systemd watchdog terminates a killable process within the configured recovery bound.

### Session helper and portals

The current GNOME integration is `zflow desktop-agent`, connected to the existing
desktop API with Unix credential checks. The portal design below is future work.

zflow-session owns desktop geometry, consent UI, portal integration, and the future configuration UI. It authenticates to zflowd through filesystem mode and SO_PEERCRED.

For portal interface version 2 or newer, the freedesktop InputCapture lifecycle is:

~~~~text
CreateSession2 -> Start -> ConnectToEIS -> GetZones
-> SetPointerBarriers -> Enable -> Activated / EIS events
~~~~

During a portal activation, zflow-session requests keyboard and pointer capabilities and uses EIS as the sole remote capture source. It forwards each supported EIS event to zflowd and releases the portal when remote ownership ends. The compositor captures physical input directly; zflowd MUST NOT grab or forward the physical devices in parallel with a live portal activation. See the [InputCapture portal specification](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.InputCapture.html).

zflow-session binds each portal session to its authenticated daemon IPC connection and activation epoch. On daemon EOF, epoch replacement, heartbeat expiry, or authentication failure, it stops forwarding and closes EIS plus the portal session. It never reconnects an old activation. The session helper runs under a user-service watchdog whose event/IPC loop sends watchdog notifications; killing a wedged helper closes its D-Bus and EIS ownership. Portal support uses the same two-second local recovery bound as zflowd.

The v1 portal lifecycle remains disabled in the first implementation. zflow disables edge switching with a clear diagnostic when the public interface version is below 2.

Portal barriers cover the outside boundary of the union of exposed zones. They cannot place a remote screen between two touching local monitors. ZonesChanged requires a new zone query and new barriers.

GNOME added InputCapture support in [GNOME 45](https://release.gnome.org/45/). KDE announced portal support for [Plasma 6.1](https://kde.org/announcements/plasma/6/6.0.90/). zflow checks the public portal interface version, SupportedCapabilities, session capabilities, EIS device capabilities, and every method result. Backend package and version information serves diagnostics only.

[xdg-desktop-portal-wlr](https://github.com/emersion/xdg-desktop-portal-wlr) does not expose InputCapture in the audited snapshot. A transparent layer-shell edge surface receives normal pointer events but has no protocol guarantee of a global pointer barrier. See [wlr-layer-shell](https://wayland.app/protocols/wlr-layer-shell-unstable-v1). zflow labels layer-surface switching experimental until the crossing matrix passes.

### Greeter and lock behavior

A Linux target can inject through uinput without a user session after zflowd and its virtual devices start.

For this specification, allow_prelogin_input gates injection whenever zflowd cannot establish that the active seat has an unlocked, authenticated user session. This includes graphical greeters, lock screens, unauthenticated VTs, and unknown session state. An authenticated active VT follows normal-session permission. Unknown or contradictory login/lock state fails closed.

A Linux source at a greeter has no portal helper. It uses the configured local hotkey and the normal Arming transition.

zflow does not rely on libei as its system backbone. libei defines a Unix-socket protocol and may support future session paths; no standard greeter-wide EIS service exists. The audited libei documentation reports 1.6.0 while gesture APIs carry “Since 1.7” markers, and the author described 1.7 as forthcoming in July 2026. See the [libei documentation](https://libinput.pages.freedesktop.org/libei/) and [gesture announcement](https://who-t.blogspot.com/2026/07/libei-and-gesture-events.html).

ei_gestures carries recognized gestures rather than raw MT contacts. It cannot replace zflow's target-native raw-contact path.

## macOS

### Process model

The current source-only app runs capture and networking in its user process.
SwiftUI owns the menu and Settings window; a Rust worker owns engine lifetimes.
A separate SMAppService daemon handles only leased AWDL suppression. Its XPC
peers require the same signing team and exact client/daemon identifiers. The
build script bundles and signs these executables with hardened runtime enabled;
notarization and distribution qualification are separate release steps.

The broader receiver and LoginWindow process model below is future work.

A root LaunchDaemon owns networking, peer state, console-session arbitration, and the optional root-only Karabiner client. It never creates CGEvent taps or calls CGEventPost. A signed native Mach-O LaunchAgent owns CGEvent capture, filtering, and posting in each Aqua or LoginWindow session.

Each agent connection binds to its audit-session identifier, session type, and console UID. Both XPC peers enforce the expected Team ID, signing identifier, and designated requirement through peer code-signing requirements. Exactly one active WindowServer session may receive input. A console-session transition releases old state, advances the session epoch, and rejects stale-agent messages. See Apple's [XPC peer-validation guidance](https://developer.apple.com/forums/thread/681053).

The legacy LaunchAgent plist declares AssociatedBundleIdentifiers for the user-visible zflow app. Permission enrollment occurs in Aqua. A LoginWindow agent never attempts to prompt and remains unsupported unless the same final-path responsible-code identity retains its grant after logout and reboot. See Apple's [helper attribution guidance](https://developer.apple.com/documentation/servicemanagement/updating-helper-executables-from-earlier-versions-of-macos) and [TN3127](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements).

The CoreHID experiment must name and diagram the exact entitled process before implementation. No CoreHID result applies to the baseline process model until that signed process passes the gate.

Apple DTS recommends this daemon-plus-agent pattern for remote-control software, and a LoginWindow agent can communicate with WindowServer. See the [Apple DTS architecture guidance](https://developer.apple.com/forums/thread/814152) and [LoginWindow agent discussion](https://developer.apple.com/forums/thread/696859).

LoginWindow target injection remains an experiment until a signed final-path package passes the matrix in TESTPLAN.md. Source keyboard capture remains unavailable while Secure Event Input owns keyboard entry.

### Capture

The logged-in baseline uses one CGEvent session tap. For listen-only operation, the agent uses CGPreflightListenEventAccess and CGRequestListenEventAccess. Active filtering and CGEventPost use CGPreflightPostEventAccess and CGRequestPostEventAccess for Accessibility authority. zflow does not require a separate Input Monitoring grant when Accessibility already authorizes listening. An active tap suppresses local events only while zflow owns input.

The adapter:

- records standard and unaccelerated pointer fields before it selects wire semantics;
- compares active-filter discard, warp-to-anchor, and cursor disassociation only where that API's foreground precondition holds; no cursor strategy ships before the matrix selects it;
- preserves documented continuous-scroll, delta, phase, and momentum fields;
- decodes recognized legacy system-defined media payloads through a version-tested empirical path and maps them to HID Consumer usages; unknown payloads remain opaque;
- keeps the tap callback bounded and moves work off the callback thread;
- re-enables taps disabled by timeout or user input;
- recreates invalid taps after sleep, session change, or repeated failure;
- releases all forwarded state before reconnection.

Apple's [CGEvent tap API](https://developer.apple.com/documentation/coregraphics/cgevent/tapcreate%28tap%3Aplace%3Aoptions%3Aeventsofinterest%3Acallback%3Auserinfo%3A%29) and [CGEventField](https://developer.apple.com/documentation/coregraphics/cgeventfield) define the public surface.

Secure Event Input protects keyboard entry from interceptors. zflow treats that condition as “keyboard capture unavailable,” releases forwarded keys, and shows the reason. It tests pointer and scroll behavior as separate event classes. See [TN2150](https://developer.apple.com/library/archive/technotes/tn2150/_index.html).

### Injection

Logged-in v1 uses CGEventPost for keyboard, pointer, and scroll. System-defined media decoding and replay remain empirical compatibility paths; a virtual-HID backend prefers real Consumer reports.

The real-device experiment has two backends:

1. CoreHID HIDVirtualDevice on macOS 15+ with Apple's managed com.apple.developer.hid.virtual.device entitlement. The similarly named DriverKit entitlement is defunct for this path. See [CoreHID virtual-device guidance](https://developer.apple.com/documentation/corehid/creatingvirtualdevices), Apple's [current entitlement clarification](https://developer.apple.com/forums/thread/843327), and the [capability request process](https://developer.apple.com/help/account/capabilities/capability-requests).
2. The version-pinned upstream client for [Karabiner's virtual-HID driver](https://github.com/pqrs-org/Karabiner-DriverKit-VirtualHIDDevice) as an optional installed dependency.

zflow does not claim that either backend works at LoginWindow until the signed secure-field experiment passes. CoreHID may require a small signed Swift helper because its public API uses Swift concurrency.

### Gestures

macOS exposes no public high-level constructor for systemwide swipe, pinch, or rotate events. A standards-based CoreHID virtual touchpad may trigger native recognition, so zflow treats injection into macOS as an experiment rather than declaring the path impossible.

Raw contact capture uses the private, reverse-engineered MultitouchSupport.framework. Karabiner demonstrates the access in its [private header](https://github.com/pqrs-org/Karabiner-Elements/blob/main/src/apps/MultitouchExtension/src/MultitouchPrivate.h) and [implementation](https://github.com/pqrs-org/Karabiner-Elements/blob/main/src/apps/MultitouchExtension/src/MultitouchDeviceManager.swift).

**Decision (2026-08-31)**: raw contact capture ships in the signed baseline behind a feature flag. Precedent: BetterTouchTool and Karabiner's Multitouch Extension distribute the same framework access with Developer ID signing and notarization; notarization does not reject private API linkage today. The [Apple Developer Program License Agreement](https://developer.apple.com/support/terms/apple-developer-program-license-agreement/) language conflicts with private API use, and the maintainer accepts that distribution risk; App Store distribution was already out of scope. Any macOS update may break the framework, so the capability MUST degrade to pointer and scroll with a diagnostic instead of failing the session, and the flag disables it entirely. Contacts replay on the target through the Linux virtual-touchpad experiment, where libinput performs recognition.

### AWDL and Apple peer-to-peer Wi-Fi

Network.framework can enable Apple-to-Apple peer-to-peer Wi-Fi through includePeerToPeer. Apple does not document the wire protocol for cross-platform use. See [TN3151](https://developer.apple.com/documentation/technotes/tn3151-choosing-the-right-networking-api) and [NWParameters.includePeerToPeer](https://developer.apple.com/documentation/network/nwparameters/includepeertopeer).

This path cannot carry the general Quinn-based Mac-to-Linux transport and cannot request Apple's private realtime mode. zflow does not include it in the baseline.

### Distribution and preboot

The macOS package uses Developer ID signing, hardened runtime, and notarization. The team preserves Team ID, bundle identifiers, executable roles, installation paths, entitlements, and designated requirements across upgrades. TCC continuity depends on code identity and designated requirements, not one leaf certificate forever. See Apple's [code-signing identity guide](https://developer.apple.com/library/archive/documentation/Security/Conceptual/CodeSigningGuide/AboutCS/AboutCS.html) and [code requirement documentation](https://developer.apple.com/documentation/security/applying-code-requirements).

zflow code on the encrypted Data volume cannot run before FileVault unlock. The product describes FileVault preboot as outside its software-input scope.

## Security

### Threat model

zflow defends against:

- an active attacker on the local network during discovery, pairing, and normal use;
- replayed or reordered application messages;
- an unpaired peer sending input;
- a paired peer without pre-login permission;
- an unprivileged local process calling the daemon socket;
- malformed messages, event floods, and state exhaustion;
- process death while input remains held.

zflow does not defend a machine after its operating system, daemon account, or paired peer identity key has been compromised.

### Pairing and peer identity

Each peer creates a long-term identity key and presents it through a self-signed TLS certificate or an equivalent certificate binding. The user pairs while both machines have logged-in sessions.

Pairing MUST authenticate the transcript through one reviewed method:

- a QR code that binds both identities and the handshake transcript;
- a short authentication string compared on both displays;
- a PAKE such as [SPAKE2](https://www.rfc-editor.org/rfc/rfc9382.html).

Blind trust on first use is insufficient because it cannot detect a first-connection MITM. [RFC 7469](https://www.rfc-editor.org/rfc/rfc7469.html) documents that limitation.

After confirmation, zflow pins the peer's public identity key or SPKI rather than a replaceable leaf certificate. It supports revocation and identity rotation through a new authenticated pairing.

### Connection authorization

Every input-capable connection uses mutual authentication. Pairing connections may exist before trust, but they cannot reach an input backend.

Each peer has revocable capabilities:

- connect;
- receive normal-session input;
- send normal-session input;
- inject before login;
- use clipboard when that feature exists.

Pre-login injection defaults off. A local logged-in user must grant it.

QUIC 0-RTT MUST NOT carry pairing, input, control, permission, or session-takeover messages because an attacker can replay 0-RTT application data. See [RFC 9001](https://www.rfc-editor.org/rfc/rfc9001.html#section-9.2).

### Local authority

The local socket uses restrictive filesystem ownership plus peer credentials. zflowd checks the active seat and caller identity for each privileged command.

The network protocol exposes no remote configuration command. Local files and authenticated local IPC own configuration. A peer cannot disable the local escape chord, increase the receiver-configured lease duration, request arbitrary device grabs, or grant itself capabilities.

The decoder caps event rates, message sizes, concurrent contacts, open streams, candidate count, and pairing attempts. The receiver releases held state when any bound trips.

### Secrets and logs

The daemon account owns key files with mode 0600 in a mode 0700 state directory. Logs MUST NOT contain key content, clipboard content, typed keys, authentication strings after pairing, or raw event traces unless the user starts an explicit local diagnostic capture.

## Observability

Each session records bounded local metrics:

- capture-to-send and receive-to-inject processing time;
- RTT, delay variation, loss, reordering, and datagram queue drops;
- clock offset, skew, and residual error;
- playout delay, scheduler lateness, catch-up amount, and explicit rebases;
- input lease renewals, snapshot acknowledgements, synthetic releases, epoch/generation changes, and rejected stale events;
- arming-to-grab time and switch-time leakage events;
- path changes, service class, probe cadence, CPU wakeups, and energy data where the platform exposes them.

Reports use p50, p95, p99, p99.9, maximum, and burst length where sample count supports those values. A trace records event timing and numeric motion with user consent; it redacts key identities by default.

## Validation

**Decision (2026-08-31)**: the prototype gates on the protocol-level checks below. The full release matrices (Linux backbone, desktop edge switching, signed macOS package, radio/QoS/playout, path failover, pairing and authorization) moved to [TESTPLAN.md](TESTPLAN.md) and gate beta and 1.0, not the first working build.

The prototype requires:

- property tests over the protocol state machine: lease expiry, epoch/generation/activation ordering, anchor-before-transition, snapshot reconciliation by difference;
- fuzz targets for every decoder message family;
- one deterministic simulator run injecting: isolated and burst loss; duplicate and reordered datagrams; 40-150 ms jitter bursts; control/datagram cross-ordering; final-datagram loss followed by idle; delayed snapshots after lease expiry; sender and receiver process death; suspend/resume; connection replacement and old-epoch or old-generation traffic; held keys and buttons during each failure.

The simulator run passes when the sender enqueues a reliable cumulative checkpoint within 250 ms after totals change; keys, buttons, touches, and gestures release within the lease bound; a click never overtakes its motion anchor; old epochs and closed activations inject nothing; and bulk serialization, stream loss, and congestion state create no transport-level head-of-line blocking on the input connection.

## Naming, licensing, and repository state

The crates.io package name zflow belongs to an existing project. The project uses the available Cargo package name `zflow-kvm`; the installed binary remains `zflow`.

The repository uses GPL-3.0-or-later. This keeps source reuse from GPL-3.0-or-later projects compatible.

The repository contains the headless Rust package, protocol model, configuration layer, setup and control CLI, authenticated QUIC daemon, Linux evdev/uinput runtime, packaging, and isolated feasibility harnesses.

As of 2026-08-31, implementation steps 0 through 5 are code-complete and pass the rootless automated suite. Step 6 is the next product gate. It requires installing the package on two Linux hosts and running the alpha qualification in [TESTPLAN.md](TESTPLAN.md), including real evdev/uinput ownership, watchdog recovery, suspend and resume, pre-login authorization, and measurements on the maintainer's jittery link. Passing Spike E proves the GDM pre-login primitive; it does not replace that run. Steps 7 through 11 remain future work.

## Implementation sequence

0. Run the tier 0 and tier 1 feasibility spikes in [SPIKES.md](SPIKES.md). A tier 0 failure reopens this specification before any step below starts.
1. Choose the package name and license.
2. Build the protocol state machine, property tests, small deterministic simulator, and fuzz targets.
3. Run the bounded Lan Mouse backend reuse spike.
4. Implement Linux on-demand evdev capture, uinput injection, hotkey ownership, and watchdog recovery.
5. Add authenticated QUIC, cumulative motion, receiver lease, and metrics.
6. Daily-drive a Linux-to-Linux alpha on the maintainer's own jittery link; this is the first real smoothing measurement.
7. Measure radio probes, QoS, fixed playout, and adaptive playout.
8. Add portal/EIS capture and test desktop edge switching.
9. Build the signed macOS daemon and Aqua agent for logged-in capture/injection, including flagged raw-contact capture.
10. Run CoreHID, LoginWindow, touch, and path-failover experiments as separate gates.
11. Add a best-effort bulk connection before clipboard work.

Release matrices from TESTPLAN.md gate each beta along the way.

## Decision register

### Settled for the first prototype

- The Cargo package is `zflow-kvm`, the installed binary is `zflow`, and the project license is GPL-3.0-or-later (maintainer implementation decision, 2026-08-31).
- The Linux evdev/uinput backend remains independent after the bounded Lan Mouse reuse gate found that reuse would require a maintained fork without removing zflow's backend or protocol work. Lan Mouse remains a reference for later portal/EIS work (reuse gate, 2026-08-31).
- Linux uses evdev plus uinput as its system backbone.
- Linux grabs on demand at Arming after a neutral frame; the bounded switch-time leak is accepted (maintainer decision, 2026-08-31).
- Mid-session enrollment of new physical devices is baseline behavior.
- The wire uses USB HID usages rather than raw evdev codes.
- Input transitions use a reliable ordered stream.
- Motion uses cumulative latest-wins datagrams.
- The receiver owns release on lease or lifecycle failure.
- QUIC carries the first transport prototype.
- Bulk traffic uses a separate best-effort connection.
- Pairing authenticates a displayed or scanned transcript.
- Discovery does not publish a long-lived identity fingerprint.
- Path changes use candidate racing and application session takeover.
- Raw Mac trackpad contact capture ships in the signed baseline behind a feature flag with mandatory graceful degradation (maintainer decision, 2026-08-31).
- Release matrices live in TESTPLAN.md; the prototype gates on property tests, fuzzing, and the simulator (maintainer decision, 2026-08-31).

### Provisional until measurements

- Application probe cadence and direction.
- DSCP and platform service class.
- Fixed and adaptive playout delay.
- Catch-up and stale-motion policy.
- The switch-time leakage acceptance threshold.
- Portal edge switching on each desktop.
- Linux virtual-touchpad classification.

### Experiments with no product promise

- Layer-surface edge switching.
- macOS lock-screen and LoginWindow target injection.
- macOS pointer and scroll capture during Secure Event Input.
- CoreHID virtual touchpad recognition.
- Karabiner-backed LoginWindow injection.
- Apple-to-Apple peer-to-peer transport.
- Seamless path change while state remains held.

### Rejected from v0.1

- Permanent evdev grab with local relay routing: rejected 2026-08-31 for its restart-until-reboot and hotplug costs; revisit only if measured switch-time leakage exceeds the frozen threshold.
- Independent relative deltas with latest-wins delivery.
- Blind trust on first use.
- A stable certificate fingerprint in mDNS.
- Clipboard on the critical input stream.
- Voice marking for all traffic.
- Arbitrary Quinn multipath inside one connection.
- Raw UDP fallback without a measured QUIC blocker.
