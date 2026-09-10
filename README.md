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

### macOS source cursor capture

The experimental Mac source uses an active HID-level event tap. During remote
control it hides the Mac cursor and disconnects cursor position from physical
movement, while forwarding relative deltas and raw trackpad contacts. It does
not warp the cursor back to screen center. On exit it reconnects and shows the
cursor; if macOS disables the event tap, it ends forwarding and runs cleanup.
It handles Ctrl+Cmd+Backspace, SIGINT, SIGTERM, and SIGHUP.

For background cursor visibility, the CLI resolves the private
`SetsCursorInBackground` connection property at runtime, following Deskflow's
approach. Missing symbols or a failed cursor API call prevent activation.
Apple documents cursor disconnection for foreground apps; verify the behavior
on the target macOS version, with another app focused. API success alone does
not prove cursor immobility or suppression of native trackpad gestures.

On September 10, live tests on a Mac16,5 running macOS 27.0 (26A428) passed
cursor isolation and recovery after normal exit, SIGKILL, and SIGSTOP. Fabrico
confirmed cursor visibility and control after each run. A separate observer
measured cursor position; the earlier AWDL-only tests had not checked it.
These results cover that Mac and external Magic Trackpad, not the full macOS
matrix. The AWDL helper does not restore cursor state. Use an external timed
recovery command for failure tests: the source cannot run cleanup while stopped
or after a forced kill. See [TESTPLAN.md](TESTPLAN.md) for measurements and
remaining checks.

### macOS Wi-Fi latency

If macOS input stutters or rubber-bands, measure LAN latency before changing
zflow buffering. A low baseline with repeated spikes above one 60 Hz frame
(about 17 ms) can produce the symptom even when signal strength is excellent:

```sh
ping -c 20 RECEIVER_LAN_IP
```

On the September 10 Mac-to-Ubuntu test, p95 RTT fell from 70.8 ms to 5.5 ms
while a temporary loop held `awdl0` and `llw0` down. The motion-sequence loss
counter estimates missing updates, not Wi-Fi hardware packet drops. That test
did not isolate the two interfaces.

The foreground Mac source now offers opt-in, session-scoped AWDL suppression.
Build the source and its separate helper as your normal user:

```sh
cargo build --bin zflow-macos-source
xcrun --sdk macosx clang \
  -isysroot "$(xcrun --sdk macosx --show-sdk-path)" \
  -std=c11 -O2 -Wall -Wextra -Werror \
  src/macos/awdl_helper.c -o target/debug/zflow-awdl-helper
```

After reviewing the helper and installer, authorize its installation:

```sh
sudo bash scripts/install-macos-awdl-helper.sh target/debug/zflow-awdl-helper
```

This installs a root-owned setuid executable at
`/Library/PrivilegedHelperTools/io.zflow.awdl-helper`. It grants local users
the narrow ability to lease suppression of `awdl0`; it accepts no arbitrary
commands or interface names. Installation does not change network state.
The source itself must run without sudo:

```sh
./target/debug/zflow-macos-source \
  --config "$HOME/Library/Application Support/zflow/zflow.toml" \
  --peer ubuntu --reduce-wifi-latency
```

The helper remembers AWDL's initial up/down state. It holds AWDL down during
remote capture and restores that state on return, failed activation, or
disconnect. A pipe and a two-second renewable lease also cover sender crashes
and stalls. The helper acknowledges heartbeats; a missing acknowledgement ends
remote capture. Ctrl+Cmd+Backspace, Ctrl+C, and SIGTERM return local control.
Missing or unsafe helper installation fails before capture starts.

AirDrop and other Continuity features may disconnect while suppression is
active; restoring AWDL does not promise to resume an interrupted transfer.
This option leaves `llw0` and Bluetooth unchanged. Without the flag, zflow does
not launch the helper or change AWDL. The helper serializes leases across
source processes. An administrator killing or suspending the helper itself
can prevent restoration; the lease protects against sender failures, not
failure of the privileged helper. Stop the sender before manually recovering
with `sudo ifconfig awdl0 up` if AWDL was up before the session.

AWDLToggle's interface-monitoring approach informed this feature. No code was
copied: its repository had no detected license when inspected. This guardian
uses interface notifications plus a bounded 100 ms fallback check, with no
per-tick shell processes. A short AWDL-only run measured 5.93 ms p95 RTT with
`llw0` up, and fabrico reported smooth input. Normal return, sender crash, and
sender freeze restored AWDL in live tests. The longer radio soak and remaining
failure cases in [TESTPLAN.md](TESTPLAN.md) still gate broader qualification.

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
