#!/usr/bin/env bash
# Remove installed Linux service files. Configuration and identity state stay by default.
set -Eeuo pipefail
IFS=$'\n\t'

readonly SERVICE_USER="zflow"
readonly SERVICE_GROUP="zflow"
readonly CONFIG_DIR="/etc/zflow"
readonly STATE_DIR="/var/lib/zflow"
readonly UNIT_FILE="/etc/systemd/system/zflowd.service"
readonly DROPIN_DIR="/etc/systemd/system/zflowd.service.d"
# Written by zflow setup --prelogin on; older installs always added it.
readonly PRELOGIN_DROPIN="$DROPIN_DIR/prelogin.conf"
readonly VIRTUAL_RULE_FILE="/etc/udev/rules.d/70-zflow.rules"
readonly CAPTURE_RULE_FILE="/etc/udev/rules.d/71-zflow-capture.rules"
readonly MODULE_FILE="/etc/modules-load.d/zflow.conf"
readonly SLEEP_HOOK_FILE="/usr/lib/systemd/system-sleep/zflow"
readonly SLEEP_MARKER="/run/zflowd-resume-after-sleep"
readonly ZFLOW_BIN="/usr/local/bin/zflow"
readonly ZFLOWD_BIN="/usr/local/bin/zflowd"
readonly LIB_DIR="/usr/local/lib/zflow"
readonly ZFLOW_GUI_BIN="/usr/local/bin/zflow-gui"
readonly FIREWALL_SCRIPT="$LIB_DIR/firewall.sh"
readonly DESKTOP_FILE="/usr/local/share/applications/io.zflow.zflow.desktop"
readonly DBUS_SERVICE_DIR="/usr/local/share/dbus-1/services"
readonly DBUS_SERVICE_FILE="$DBUS_SERVICE_DIR/io.zflow.Desktop.service"
readonly AUTOSTART_FILE="/etc/xdg/autostart/io.zflow.desktop-agent.desktop"
readonly ICON_THEME_DIR="/usr/local/share/icons/hicolor"
readonly APP_ICON_FILE="$ICON_THEME_DIR/scalable/apps/io.zflow.zflow.svg"
readonly SYMBOLIC_ICON_FILE="$ICON_THEME_DIR/symbolic/apps/io.zflow.zflow-symbolic.svg"

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
        printf 'usage: sudo %s [--purge]\n' "$0"
        printf 'Without --purge, configuration, identity state, and the service account remain.\n'
        exit 0
        ;;
    *) die "unknown argument: $1" ;;
esac
[[ $# -le 1 ]] || die "too many arguments"

[[ "$(uname -s)" == "Linux" ]] || die "Linux is required"
# The package owns /usr/lib/systemd/system-sleep/zflow and the service.
if command -v dpkg-query >/dev/null 2>&1 && [[ "$(dpkg-query -W -f='${Status}' zflow 2>/dev/null || true)" == 'install ok installed' ]]; then
    die 'zflow is managed by dpkg. Remove it with: sudo apt remove zflow'
fi
[[ "$EUID" -eq 0 ]] || die "run as root: sudo $0"

if command -v systemctl >/dev/null 2>&1; then
    if systemctl is-active --quiet zflowd.service; then
        systemctl stop zflowd.service || die "could not stop zflowd.service"
    fi
    systemctl disable zflowd.service >/dev/null 2>&1 || true
fi
if [[ -f "$FIREWALL_SCRIPT" ]]; then
    bash "$FIREWALL_SCRIPT" close
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
    "$ZFLOWD_BIN" \
    "$LIB_DIR/uninstall.sh" \
    "$FIREWALL_SCRIPT" \
    "$ZFLOW_GUI_BIN" \
    "$DESKTOP_FILE" \
    "$DBUS_SERVICE_FILE" \
    "$AUTOSTART_FILE" \
    "$APP_ICON_FILE" \
    "$SYMBOLIC_ICON_FILE"
if [[ -d "$ICON_THEME_DIR" ]] && command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache --quiet --force --ignore-theme-index "$ICON_THEME_DIR"
fi
rmdir -- "$LIB_DIR" "$DROPIN_DIR" 2>/dev/null || true
rmdir -- "$DBUS_SERVICE_DIR" "${DBUS_SERVICE_DIR%/*}" 2>/dev/null || true

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
    printf 'Removed zflow binaries, udev rules, service, and desktop launcher.\n'
    printf 'Preserved %s, %s, and the %s account.\n' \
        "$CONFIG_DIR" "$STATE_DIR" "$SERVICE_USER"
    printf 'Removed capture rules so the account no longer gains input-device access.\n'
    printf 'Run this script with --purge to delete the preserved data.\n'
fi
printf 'Desktop users keep the zflow GNOME extension until they remove it in Extensions.\n'
