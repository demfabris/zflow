#!/usr/bin/env bash
# Remove everything install.sh created. Run with sudo.
set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "run with sudo"; exit 1; }

systemctl disable zflow-spike-e.service 2>/dev/null || true
systemctl stop zflow-spike-e.service 2>/dev/null || true
rm -f /etc/systemd/system/zflow-spike-e.service
rm -f /etc/modules-load.d/zflow-spike-e.conf
systemctl daemon-reload
echo "removed. /var/log/zflow-spike-e.log kept for reference (delete manually if you want)."
