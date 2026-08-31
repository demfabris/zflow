# Linux service packaging

`scripts/install.sh` builds both headless binaries and installs them under
`/usr/local/bin`. It creates a locked `zflow` account, loads `uinput`, installs
the service, udev rules, and a system-sleep hook, then starts `zflowd`.

The installer keeps two administrator-managed files on upgrades:

- `/etc/zflow/zflow.toml`
- `/etc/udev/rules.d/71-zflow-capture.rules`

`zflow setup` maintains the capture rule with stable device attributes. Each
selected event node receives group `zflow` and mode `0640`. The service account
does not join the broad `input` group. Pass every selected device in one setup
command by repeating `--device`.

Run the installer from the repository root:

```sh
./scripts/install.sh
sudo /usr/local/bin/zflow devices
sudo /usr/local/bin/zflow setup \
  --device /dev/input/eventX \
  --device /dev/input/eventY \
  --udev-rules /etc/udev/rules.d/71-zflow-capture.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --action=change --subsystem-match=input
sudo /usr/local/bin/zflow doctor
```

The default service starts at `multi-user.target` and does not wait for
`network-online.target`. Its drop-in orders daemon readiness before the display
manager, but pre-login injection remains blocked by the global config and
per-peer permission gates. The sleep hook stops an active daemon before
suspend and starts a fresh process after resume. This releases every evdev
grab and discards stale sessions and clock state.

The normal uninstaller keeps configuration, identity state, and the service
account. It removes the capture rule to revoke event-device access:

```sh
sudo ./scripts/uninstall.sh
```

Use `--purge` only when you also want to delete those files and the account.
