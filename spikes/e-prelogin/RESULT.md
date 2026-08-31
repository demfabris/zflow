# Spike E result: pre-login injection

**Verdict:** PASSED on GDM

**Date:** 2026-08-31

**Host:** Ubuntu 26.04.1 LTS, Linux 7.0.0-30-generic, systemd 259, GDM 50.1,
GNOME Shell 50.1 on Wayland, libinput 1.31.1

## Question

Can a root daemon create a correctly classified uinput keyboard before GDM and
deliver a sentinel to the greeter password field?

## First cold boot

The installed service started at 7.122 seconds after boot. The kernel registered
`zflow-spike-keyboard` at 7.149 seconds. GDM started at 7.579 seconds, reached
active state at 7.592 seconds, and opened its greeter session at 7.777 seconds.
This proves that the host can create a root-owned uinput keyboard before GDM.

The user session opened at 14.048 seconds. The harness waited 20 seconds and
sent its first `ZFLOW` plus five-Backspace cycle at 31.41 seconds. Fabricio saw
the text appear and disappear in the logged-in GNOME session. That observation
proves session input injection but says nothing about GDM's password field.

The service completed 24 cycles and removed the device at 203.30 seconds. It
exited with status 0.

## Classification check

A classification-only run created the same seven-key device as the boot
harness. `udevadm info` reported:

```text
ID_INPUT=1
ID_INPUT_KEY=1
```

It did not report `ID_INPUT_KEYBOARD=1`. The device advertised only Z, F, L, O,
W, Backspace, and Enter, so systemd's input classifier treated it as a generic
key device. The unprivileged `libinput list-devices` process could not open its
event node, so this run produced no libinput or seat result.

Spike B already proved `ID_INPUT_MOUSE=1` and GNOME pointer acceptance for the
virtual mouse. It does not cover this keyboard.

## Harness correction

The corrected harness advertises the conventional keyboard key-code range and
sends `READY=1` after it records udev and libinput classification. Its
`Type=notify` unit holds GDM behind that readiness barrier.

The process waits for GDM, emits one sentinel cycle without Enter, and exits. It
skips injection when an authenticated seat0 session exists.

A local classification-only run reported `ID_INPUT=1`, `ID_INPUT_KEY=1`, and
`ID_INPUT_KEYBOARD=1`. A transient `Type=notify` user service accepted the
`READY=1` message with `NotifyAccess=all`. A session-safety run found the active
seat0 user session and exited without emitting the sentinel. Root had to run the
boot service before the harness could capture its libinput block.

## Controlled GDM boot

The corrected service entered its startup phase at 7.263 seconds. The kernel
registered `zflow-spike-keyboard` at 7.291 seconds. The harness captured these
udev properties:

```text
ID_INPUT=1
ID_INPUT_KEY=1
ID_INPUT_KEYBOARD=1
```

Libinput listed `/dev/input/event24` on `seat0, default` with keyboard
capability. The harness sent `READY=1` at 8.53 seconds. Systemd started GDM at
8.533 seconds, and GDM opened its greeter session at 8.643 seconds.

The harness emitted one `ZFLOW` plus five-Backspace cycle from 13.64 through
15.89 seconds. Fabricio saw five password dots appear and disappear at GDM. GDM
opened Fabricio's authenticated session at 18.454 seconds, 2.56 seconds after
the harness removed its device. The service exited with status 0.

## Conclusion

A root service can create and classify a uinput keyboard before GDM, then send
keys to GDM's password field on this host. This passes the kernel-backbone
feasibility question for Ubuntu 26.04.1 and GDM 50.1.

The release matrix still needs lock-screen and VT coverage, plus SDDM, greetd,
and authorization tests.
