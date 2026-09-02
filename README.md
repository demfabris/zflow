# zflow

zflow is a headless input-sharing service for Linux. It captures an explicit
set of physical evdev devices, sends bounded input state over authenticated
QUIC, and injects it through stable uinput devices on the other machine.

This is a working headless prototype, not yet a qualified alpha. The supported
Linux launch path is the packaged systemd service. There is no GUI yet and
automatic edge switching is not implemented. The macOS source is an
experimental foreground developer tool. The two-host qualification run in
`TESTPLAN.md` still gates the alpha label.

## Install and select devices

Run the installer on both Linux machines:

```sh
./scripts/install.sh
sudo zflow devices
```

Select every keyboard, mouse, and touchpad node that must move together. Repeat
`--device` in one command so the capture set is updated atomically:

```sh
sudo zflow setup \
  --device /dev/input/eventX \
  --device /dev/input/eventY \
  --udev-rules /etc/udev/rules.d/71-zflow-capture.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --action=change --subsystem-match=input
sudo zflow doctor
```

Raw touchpad forwarding is experimental. Enable it on both machines, then
restart the daemons so they create and negotiate the virtual touchpads:

```sh
sudo zflow setup --experimental-touchpad on
sudo systemctl restart zflowd.service
sudo zflow doctor
```

`zflow doctor` should report the keyboard, pointer, and experimental touchpad
as ready. On the receiving machine, `sudo libinput list-devices` should list
`zflow remote touchpad` with `pointer gesture` capabilities.

Setup records stable physical attributes. It refuses to write a broad udev
rule for hardware without a unique physical path. The service account is not
placed in the general `input` group.

The default activation chord is Ctrl+Super+F12. Ctrl+Super+Backspace always
returns ownership to the local machine. You can replace either chord during
setup by repeating `--activation-key` or `--escape-key` with evdev names such
as `KEY_LEFTCTRL`.

## Pair two machines

Pairing uses a temporary listener on UDP port 43120. The normal input service
listens on UDP port 43119.

On the first machine:

```sh
sudo zflow pair listen laptop
```

On the second machine, connect to the first machine's LAN address:

```sh
sudo zflow pair connect desk 192.0.2.10:43120
```

Both commands display a six-digit code. Compare the codes in person, then
enter the peer's code at each prompt. A mismatch writes no trust record.

Normal pairing never grants pre-login input. Grant that permission separately
only if you need input at a greeter or lock screen:

```sh
sudo zflow peer allow-prelogin desk on
```

The global pre-login gate must also be enabled with
`zflow setup --prelogin on`. Unknown seat state always denies injection.

## Use and inspect it

```sh
sudo zflow switch desk
sudo zflow local
sudo zflow status
sudo zflow status --json
sudo zflow peers
journalctl -u zflowd.service -f
```

The source waits for a complete evdev frame and a neutral ownership boundary
before grabbing the configured set. Loss of a device, transport, lease,
process, or authorization releases held input. The packaged sleep hook stops
an active daemon before suspend and starts a fresh process after resume.
Alternate launchers must provide an equivalent suspend boundary; running
`zflowd` manually is intended for development.

Revocation is local and immediate:

```sh
sudo zflow peer revoke desk
```

### macOS Wi-Fi latency

If macOS input stutters or rubber-bands, measure LAN latency before changing
zflow buffering. A low baseline with repeated spikes above one 60 Hz frame
(about 17 ms) can produce the symptom even when signal strength is excellent:

```sh
ping -c 20 RECEIVER_LAN_IP
```

On one Mac-to-Ubuntu run, a 2-4 ms baseline repeatedly jumped to 31-76 ms.
zflow measured 72 ms p95 RTT, 82 ms p99 delay variation, and 198 missing
datagrams. Temporarily disabling Apple's peer-to-peer Wi-Fi interfaces greatly
improved the input on that machine:

```sh
sudo ifconfig awdl0 down
sudo ifconfig llw0 down
```

This disables AirDrop and may interrupt Continuity features. Treat it as an
opt-in diagnostic, not a default setup step. Restore the interfaces after the
test (or reboot macOS):

```sh
sudo ifconfig awdl0 up
sudo ifconfig llw0 up
```

If latency spikes remain, compare with the Mac on Ethernet before attributing
the problem to capture or playout behavior.

## Develop

The protocol model and Linux runtime have deterministic tests that do not need
root. The one privileged runtime smoke test needs read access to the selected
event devices and write access to `/dev/uinput`.

```sh
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo run --bin zflow -- simulate
cargo check --manifest-path fuzz/Cargo.toml --bins
```

See [SPEC.md](SPEC.md) for protocol and safety invariants, and
[TESTPLAN.md](TESTPLAN.md) for the hardware and desktop validation matrix.

zflow is licensed under GPL-3.0-or-later.
