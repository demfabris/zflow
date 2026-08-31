#!/usr/bin/env bash
# Install the spike E pre-login injection service. Run with sudo.
# Reversible: uninstall.sh removes everything this touches.
set -euo pipefail

DIR=$(cd "$(dirname "$0")" && pwd)
BIN="$DIR/target/release/spike-e-prelogin"
UNIT=/etc/systemd/system/zflow-spike-e.service

[[ $EUID -eq 0 ]] || { echo "run with sudo"; exit 1; }
[[ -x "$BIN" ]] || { echo "build first: cargo build --release --manifest-path $DIR/Cargo.toml"; exit 1; }

# ensure uinput is loadable at boot
echo uinput > /etc/modules-load.d/zflow-spike-e.conf
modprobe uinput || true

sed "s#__BINARY__#$BIN#" "$DIR/zflow-spike-e.service" > "$UNIT"
: > /var/log/zflow-spike-e.log
chmod 644 /var/log/zflow-spike-e.log

systemctl daemon-reload
systemctl enable zflow-spike-e.service

echo "installed and enabled. reboot to run it."
echo "at GDM: focus the password field and wait for five dots to appear and disappear once."
echo "after login: read /var/log/zflow-spike-e.log, then uninstall the spike."
echo "remove with: sudo $DIR/uninstall.sh"
