#!/usr/bin/env bash
# Build and install the Linux service, optionally with its desktop app.
set -Eeuo pipefail
IFS=$'\n\t'

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd -P)"
readonly SCRIPT_DIR REPO_ROOT
readonly SERVICE_USER="zflow"
readonly SERVICE_GROUP="zflow"
readonly BIN_DIR="/usr/local/bin"
readonly CONFIG_DIR="/etc/zflow"
readonly CONFIG_FILE="$CONFIG_DIR/zflow.toml"
readonly STATE_DIR="/var/lib/zflow"
readonly UNIT_FILE="/etc/systemd/system/zflowd.service"
readonly PRELOGIN_DROPIN_DIR="/etc/systemd/system/zflowd.service.d"
readonly PRELOGIN_DROPIN="$PRELOGIN_DROPIN_DIR/prelogin.conf"
readonly UDEV_RULE_DIR="/etc/udev/rules.d"
readonly VIRTUAL_RULE_FILE="$UDEV_RULE_DIR/70-zflow.rules"
readonly CAPTURE_RULE_FILE="$UDEV_RULE_DIR/71-zflow-capture.rules"
readonly MODULE_FILE="/etc/modules-load.d/zflow.conf"
readonly SLEEP_HOOK_FILE="/usr/lib/systemd/system-sleep/zflow"
readonly DESKTOP_FILE="/usr/local/share/applications/io.zflow.zflow.desktop"
install_built=false
install_gui=false

die() {
    printf 'install: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

require_regular_source() {
    [[ -f "$1" && ! -L "$1" ]] || die "missing or unsafe source file: $1"
}

refuse_symlink() {
    [[ ! -L "$1" ]] || die "refusing to replace symlink: $1"
}

ensure_service_account() {
    local group_record members passwd_record uid gid shell uid_min nologin

    if group_record="$(getent group "$SERVICE_GROUP")"; then
        IFS=: read -r _ _ _ members <<<"$group_record"
        [[ -z "$members" ]] || die "group $SERVICE_GROUP has members; remove them before installing"
    else
        groupadd --system "$SERVICE_GROUP"
    fi

    if passwd_record="$(getent passwd "$SERVICE_USER")"; then
        IFS=: read -r _ _ uid gid _ _ shell <<<"$passwd_record"
        uid_min="$(awk '$1 == "UID_MIN" { print $2; exit }' /etc/login.defs)"
        uid_min="${uid_min:-1000}"
        [[ "$uid" =~ ^[0-9]+$ && "$uid" -lt "$uid_min" ]] || die "$SERVICE_USER exists but is not a system account"
        [[ "$gid" == "$(getent group "$SERVICE_GROUP" | cut -d: -f3)" ]] || die "$SERVICE_USER does not use group $SERVICE_GROUP"
        case "$shell" in
            */nologin|*/false) ;;
            *) die "$SERVICE_USER has a login shell: $shell" ;;
        esac
        return
    fi

    nologin="$(command -v nologin || true)"
    [[ -n "$nologin" ]] || nologin="/usr/sbin/nologin"
    [[ -x "$nologin" ]] || die "could not find a nologin executable"
    useradd \
        --system \
        --gid "$SERVICE_GROUP" \
        --home-dir "$STATE_DIR" \
        --no-create-home \
        --shell "$nologin" \
        --comment "zflow input daemon" \
        "$SERVICE_USER"
}

build_binaries() {
    local cargo_args=(build --locked --release --manifest-path "$REPO_ROOT/Cargo.toml"
        --target-dir "$REPO_ROOT/target" --bin zflow --bin zflowd)
    if [[ "$install_gui" == true ]]; then
        printf 'Building zflow, zflowd, and the desktop app...\n'
        cargo_args+=(--features gui --bin zflow-gui)
    else
        printf 'Building zflow and zflowd...\n'
    fi
    cargo "${cargo_args[@]}"
}

for argument in "$@"; do
    case "$argument" in
        --gui) install_gui=true ;;
        --install-built) install_built=true ;;
        -h|--help)
            printf 'usage: ./scripts/install.sh [--gui]\n'
            printf 'Add --gui to install the desktop app alongside the service.\n'
            exit 0
            ;;
        *) die "usage: ./scripts/install.sh [--gui]" ;;
    esac
done
[[ "$(uname -s)" == "Linux" ]] || die "Linux is required"

for source in \
    "$REPO_ROOT/Cargo.toml" \
    "$REPO_ROOT/Cargo.lock" \
    "$REPO_ROOT/packaging/config/zflow.toml" \
    "$REPO_ROOT/packaging/modules-load.d/zflow.conf" \
    "$REPO_ROOT/packaging/system-sleep/zflow" \
    "$REPO_ROOT/packaging/systemd/zflowd.service" \
    "$REPO_ROOT/packaging/systemd/zflowd-prelogin.conf" \
    "$REPO_ROOT/packaging/udev/70-zflow.rules" \
    "$REPO_ROOT/packaging/udev/71-zflow-capture.rules"; do
    require_regular_source "$source"
done
if [[ "$install_gui" == true ]]; then
    require_regular_source "$REPO_ROOT/packaging/linux/io.zflow.zflow.desktop"
fi

if [[ "$EUID" -ne 0 ]]; then
    [[ "$install_built" == false ]] || die "--install-built requires root"
    require_command cargo
    require_command sudo
    build_binaries
    if [[ "$install_gui" == true ]]; then
        exec sudo -- "$SCRIPT_DIR/install.sh" --install-built --gui
    fi
    exec sudo -- "$SCRIPT_DIR/install.sh" --install-built
fi

if [[ "$install_built" == false ]]; then
    require_command cargo
    build_binaries
fi

for command in awk cut getent grep groupadd install modprobe runuser setfacl systemctl systemd-analyze udevadm useradd; do
    require_command "$command"
done

[[ -x "$REPO_ROOT/target/release/zflow" ]] || die "cargo did not produce target/release/zflow"
[[ -x "$REPO_ROOT/target/release/zflowd" ]] || die "cargo did not produce target/release/zflowd"
if [[ "$install_gui" == true ]]; then
    [[ -x "$REPO_ROOT/target/release/zflow-gui" ]] || die "cargo did not produce target/release/zflow-gui"
    refuse_symlink "$BIN_DIR/zflow-gui"
    refuse_symlink "$DESKTOP_FILE"
fi

ensure_service_account

refuse_symlink "$CONFIG_DIR"
refuse_symlink "$CONFIG_FILE"
refuse_symlink "$STATE_DIR"
refuse_symlink "$UNIT_FILE"
refuse_symlink "$PRELOGIN_DROPIN_DIR"
refuse_symlink "$PRELOGIN_DROPIN"
refuse_symlink "$VIRTUAL_RULE_FILE"
refuse_symlink "$CAPTURE_RULE_FILE"
refuse_symlink "$MODULE_FILE"
refuse_symlink "$SLEEP_HOOK_FILE"

install -d -o root -g root -m 0755 \
    "$BIN_DIR" "$UDEV_RULE_DIR" /etc/modules-load.d /usr/lib/systemd/system-sleep \
    "$PRELOGIN_DROPIN_DIR"
install -d -o "$SERVICE_USER" -g "$SERVICE_GROUP" -m 0700 "$CONFIG_DIR" "$STATE_DIR"
install -o root -g root -m 0755 "$REPO_ROOT/target/release/zflow" "$BIN_DIR/zflow"
install -o root -g root -m 0755 "$REPO_ROOT/target/release/zflowd" "$BIN_DIR/zflowd"
install -o root -g root -m 0644 "$REPO_ROOT/packaging/systemd/zflowd.service" "$UNIT_FILE"
install -o root -g root -m 0644 \
    "$REPO_ROOT/packaging/systemd/zflowd-prelogin.conf" "$PRELOGIN_DROPIN"
install -o root -g root -m 0644 "$REPO_ROOT/packaging/udev/70-zflow.rules" "$VIRTUAL_RULE_FILE"
install -o root -g root -m 0644 "$REPO_ROOT/packaging/modules-load.d/zflow.conf" "$MODULE_FILE"
install -o root -g root -m 0755 "$REPO_ROOT/packaging/system-sleep/zflow" "$SLEEP_HOOK_FILE"
if [[ "$install_gui" == true ]]; then
    install -d -o root -g root -m 0755 "$(dirname -- "$DESKTOP_FILE")"
    install -o root -g root -m 0755 "$REPO_ROOT/target/release/zflow-gui" "$BIN_DIR/zflow-gui"
    install -o root -g root -m 0644 "$REPO_ROOT/packaging/linux/io.zflow.zflow.desktop" "$DESKTOP_FILE"
fi

if [[ -e "$CONFIG_FILE" ]]; then
    [[ -f "$CONFIG_FILE" ]] || die "configuration is not a regular file: $CONFIG_FILE"
    chown "$SERVICE_USER:$SERVICE_GROUP" "$CONFIG_FILE"
    chmod 0600 "$CONFIG_FILE"
    printf 'Kept existing configuration: %s\n' "$CONFIG_FILE"
else
    install -o "$SERVICE_USER" -g "$SERVICE_GROUP" -m 0600 \
        "$REPO_ROOT/packaging/config/zflow.toml" "$CONFIG_FILE"
    printf 'Installed default configuration: %s\n' "$CONFIG_FILE"
fi

if [[ -e "$CAPTURE_RULE_FILE" ]]; then
    [[ -f "$CAPTURE_RULE_FILE" ]] || die "capture rules are not a regular file: $CAPTURE_RULE_FILE"
    chown root:root "$CAPTURE_RULE_FILE"
    chmod 0644 "$CAPTURE_RULE_FILE"
    printf 'Kept existing capture rules: %s\n' "$CAPTURE_RULE_FILE"
else
    install -o root -g root -m 0644 \
        "$REPO_ROOT/packaging/udev/71-zflow-capture.rules" "$CAPTURE_RULE_FILE"
fi

systemd-analyze verify "$UNIT_FILE"
udevadm verify "$VIRTUAL_RULE_FILE" "$CAPTURE_RULE_FILE"

modprobe uinput
udevadm control --reload-rules
udevadm trigger --action=change --subsystem-match=misc --sysname-match=uinput
udevadm trigger --action=change --subsystem-match=input
udevadm settle
[[ -c /dev/uinput ]] || die "uinput loaded but /dev/uinput is missing"
setfacl --remove-all /dev/uinput
chown "root:$SERVICE_GROUP" /dev/uinput
chmod 0660 /dev/uinput
runuser -u "$SERVICE_USER" -- /usr/bin/test -w /dev/uinput \
    || die "$SERVICE_USER cannot write /dev/uinput after applying udev rules"

shopt -s nullglob
for event_node in /dev/input/event*; do
    if udevadm info --query=property --name="$event_node" | grep -qx 'ZFLOW_CAPTURE=1'; then
        runuser -u "$SERVICE_USER" -- /usr/bin/test -r "$event_node" \
            || die "$SERVICE_USER cannot read selected device $event_node"
    fi
done
shopt -u nullglob

systemctl daemon-reload
systemctl enable zflowd.service
systemctl restart zflowd.service

printf '\nzflow is installed and zflowd is running.\n'
if [[ "$install_gui" == true ]]; then
    printf 'Open zflow from your applications menu to pair and arrange computers.\n'
    printf 'You can also launch it with: %s/zflow-gui\n' "$BIN_DIR"
fi
printf 'Next steps:\n'
printf '  1. List input devices:\n'
printf '     sudo %s/zflow devices\n' "$BIN_DIR"
printf '  2. Select the physical devices in one command (repeat --device):\n'
printf '     sudo %s/zflow setup --device /dev/input/eventX --udev-rules %s\n' \
    "$BIN_DIR" "$CAPTURE_RULE_FILE"
printf '  3. Apply permissions (the daemon rescans automatically):\n'
printf '     sudo udevadm control --reload-rules\n'
printf '     sudo udevadm trigger --action=change --subsystem-match=input\n'
printf '  4. Check the host:\n'
printf '     sudo %s/zflow doctor\n' "$BIN_DIR"
printf 'Inspect logs with: journalctl -u zflowd.service -f\n'
printf 'Pre-login input stays disabled until you grant it during setup.\n'
