# Lan Mouse backend reuse gate

**Verdict:** retain an independent zflow backend and protocol

**Date:** 2026-08-31

**Upstream snapshot:** `feschber/lan-mouse` at `6b1eddef7ab3ae0b06f2ed3d9b21165e03ea4cad`

## Question

Can zflow reuse Lan Mouse's `input-event`, `input-capture`, or
`input-emulation` crates without carrying a fork or weakening zflow's input
model and failure contract?

## API adaptation

Lan Mouse can represent keyboard events, pointer motion, buttons, and wheel
updates. An adapter can translate those values into a subset of zflow's model.
The crates use Linux evdev codes as their common keyboard and button vocabulary,
while zflow uses USB HID page and usage pairs. Lan Mouse events also omit source
resolution, scroll lifecycle, complete touch snapshots, and gesture state.

The public capture API models screen-edge sessions. Lan Mouse keeps its backend
trait private and provides no evdev capture implementation. Its emulation crate
keeps that backend trait private too and provides no uinput implementation.
Adding zflow's Linux backbone would require a fork of both crates.

The Lan Mouse protocol carries individual DTLS datagrams. It has no session
epoch, transport generation, activation identifier, control sequence, motion
anchor, cumulative checkpoint, held-state lease, or authoritative snapshot.
zflow would replace the protocol rather than extend it.

## Lifecycle gaps

Lan Mouse assumes a graphical session for its Linux capture and emulation
paths. It does not provide the pre-login evdev/uinput path that Spike E proved.
It also lacks zflow's Idle, Arming, Remote, and Releasing ownership states,
aggregate neutral check, all-or-none EVIOCGRAB rollback, and complete
SYN_REPORT release boundary.

Lan Mouse tracks held keys and releases them during teardown. It does not track
held buttons, reconcile authoritative snapshots, expire a per-activation lease,
or reject traffic from old epochs and generations. zflow must implement those
rules in its own core regardless of backend reuse.

## Maintenance cost

Reusing the audited crates would add a maintained fork. zflow would still own
evdev capture, uinput injection, HID translation, protocol recovery, daemon
authority, and pre-login behavior. A direct backend has a smaller project
surface because it implements the selected kernel contracts without adapting
Lan Mouse's edge-sharing lifecycle.

The later portal/EIS phase can recheck Lan Mouse's compositor work as a source
reference. Direct `ashpd` and `reis` integration remains available if the
public Lan Mouse API still hides portal lifecycle and capability results.

## License

The audited Lan Mouse crates declare GPL-3.0-or-later. zflow uses the same SPDX
license, so license compatibility does not block source reuse. The API and
lifecycle mismatch decides this gate.

## Sources

- [`input-event/src/lib.rs`](https://github.com/feschber/lan-mouse/blob/6b1eddef7ab3ae0b06f2ed3d9b21165e03ea4cad/input-event/src/lib.rs)
- [`input-capture/src/lib.rs`](https://github.com/feschber/lan-mouse/blob/6b1eddef7ab3ae0b06f2ed3d9b21165e03ea4cad/input-capture/src/lib.rs)
- [`input-emulation/src/lib.rs`](https://github.com/feschber/lan-mouse/blob/6b1eddef7ab3ae0b06f2ed3d9b21165e03ea4cad/input-emulation/src/lib.rs)
- [`lan-mouse-proto/src/lib.rs`](https://github.com/feschber/lan-mouse/blob/6b1eddef7ab3ae0b06f2ed3d9b21165e03ea4cad/lan-mouse-proto/src/lib.rs)
- [`LICENSE`](https://github.com/feschber/lan-mouse/blob/6b1eddef7ab3ae0b06f2ed3d9b21165e03ea4cad/LICENSE)
