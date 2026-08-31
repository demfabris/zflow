# Spike E: pre-login injection

**Question:** can a root daemon create a correctly classified uinput keyboard
before GDM starts and deliver keys to the greeter password field?

**Status:** passed on Ubuntu 26.04.1 with GDM 50.1. The controlled 2026-08-31
boot proved pre-GDM creation, keyboard classification, seat0 assignment, and
delivery to GDM's password field. See [`RESULT.md`](RESULT.md) for the timing
evidence.

## Reproduce the GDM run

Build and install the corrected one-shot harness:

```bash
cargo build --release --manifest-path spikes/e-prelogin/Cargo.toml
sudo spikes/e-prelogin/install.sh
sudo reboot
```

At GDM, select the user and leave the password field focused. Wait for five
password dots to appear and disappear once. The harness never sends Enter and
refuses to inject after an authenticated seat0 session becomes active.

After login, preserve the evidence and remove the boot service:

```bash
sudo spikes/e-prelogin/uninstall.sh
sed -n '1,240p' /var/log/zflow-spike-e.log
```

The log records udev properties, the matching `libinput list-devices` block,
systemd readiness, and the injection timestamp.

The GDM run answers the narrow feasibility question. Lock screen, VT, SDDM,
greetd, and authorization behavior belong to the wider matrix in
[`TESTPLAN.md`](../../TESTPLAN.md).
