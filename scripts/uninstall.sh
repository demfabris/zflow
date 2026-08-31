#!/usr/bin/env bash
# Remove installed Linux service files. Configuration and identity state stay by default.
set -Eeuo pipefail
IFS=$'\n\t'

readonly SERVICE_USER="zflow"
readonly SERVICE_GROUP="zflow"
readonly CONFIG_DIR="/etc/zflow"
readonly STATE_DIR="/var/lib/zflow"
readonly UNIT_FILE="/etc/systemd/system/zflowd.service"
readonly PRELOGIN_DROPIN="/etc/systemd/system/zflowd.service.d/prelogin.conf"
readonly DROPIN_DIR="/etc/systemd/system/zflowd.service.d"
readonly VIRTUAL_RULE_FILE="/etc/udev/rules.d/70-zflow.rules"
readonly CAPTURE_RULE_FILE="/etc/udev/rules.d/71-zflow-capture.rules"
readonly MODULE_FILE="/etc/modules-load.d/zflow.conf"
readonly SLEEP_HOOK_FILE="/usr/lib/systemd/system-sleep/zflow"
readonly SLEEP_MARKER="/run/zflowd-resume-after-sleep"
readonly ZFLOW_BIN="/usr/local/bin/zflow"
readonly ZFLOWD_BIN="/usr/local/bin/zflowd"

purge=false

die() {
    printf 'uninstall: %s\n' "$*" >&2
    exit 1
}

remove_tree() {
    local path="$1"
    case "$path" in
        /etc/zflow|/var/lib/zflow) ;;
        *) die "refusing to purge unexpected path: $path" ;;
    esac
    [[ ! -e "$path" && ! -L "$path" ]] || find "$path" -xdev -depth -delete
}

case "${1:-}" in
    "") ;;
    --purge) purge=true ;;
    -h|--help)
        printf 'usage: sudo ./scripts/uninstall.sh [--purge]\n'
        printf 'Without --purge, configuration, identity state, and the service account remain.\n'
        exit 0
        ;;
    *) die "unknown argument: $1" ;;
esac
[[ $# -le 1 ]] || die "too many arguments"

[[ "$(uname -s)" == "Linux" ]] || die "Linux is required"
[[ "$EUID" -eq 0 ]] || die "run as root: sudo ./scripts/uninstall.sh"

if command -v systemctl >/dev/null 2>&1; then
    if systemctl is-active --quiet zflowd.service; then
        systemctl stop zflowd.service || die "could not stop zflowd.service"
    fi
    systemctl disable zflowd.service >/dev/null 2>&1 || true
fi

rm -f -- \
    "$UNIT_FILE" \
    "$PRELOGIN_DROPIN" \
    "$VIRTUAL_RULE_FILE" \
    "$CAPTURE_RULE_FILE" \
    "$MODULE_FILE" \
    "$SLEEP_HOOK_FILE" \
    "$SLEEP_MARKER" \
    "$ZFLOW_BIN" \
    "$ZFLOWD_BIN"
rmdir -- "$DROPIN_DIR" 2>/dev/null || true

if [[ "$purge" == true ]]; then
    for command in find getent groupdel userdel; do
        command -v "$command" >/dev/null 2>&1 || die "missing required command: $command"
    done
    remove_tree "$CONFIG_DIR"
    remove_tree "$STATE_DIR"
    if getent passwd "$SERVICE_USER" >/dev/null 2>&1; then
        userdel "$SERVICE_USER"
    fi
    if getent group "$SERVICE_GROUP" >/dev/null 2>&1; then
        groupdel "$SERVICE_GROUP"
    fi
fi

if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload
    systemctl reset-failed zflowd.service >/dev/null 2>&1 || true
fi
if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules
    udevadm trigger --action=change --subsystem-match=misc --sysname-match=uinput || true
    udevadm trigger --action=change --subsystem-match=input || true
    udevadm settle || true
fi

if [[ "$purge" == true ]]; then
    printf 'Removed zflow binaries, service, rules, configuration, state, and service account.\n'
else
    printf 'Removed zflow binaries, udev rules, and service.\n'
    printf 'Preserved %s, %s, and the %s account.\n' \
        "$CONFIG_DIR" "$STATE_DIR" "$SERVICE_USER"
    printf 'Removed capture rules so the account no longer gains input-device access.\n'
    printf 'Run this script with --purge to delete the preserved data.\n'
fi
