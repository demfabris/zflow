#!/usr/bin/env bash
# Build and install the Linux service and desktop agent.
set -Eeuo pipefail
IFS=$'\n\t'

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd -P)"
readonly SCRIPT_DIR REPO_ROOT
readonly BIN_DIR="/usr/local/bin"
readonly LIB_DIR="/usr/local/lib/zflow"
readonly UNINSTALLER="$LIB_DIR/uninstall.sh"
readonly UNIT_FILE="/etc/systemd/system/zflowd.service"
readonly DROPIN_DIR="/etc/systemd/system/zflowd.service.d"
# Older installs ordered zflowd before the display manager on every boot.
readonly LEGACY_PRELOGIN_DROPIN="$DROPIN_DIR/prelogin.conf"
readonly UDEV_RULE_DIR="/etc/udev/rules.d"
readonly VIRTUAL_RULE_FILE="$UDEV_RULE_DIR/70-zflow.rules"
readonly CAPTURE_RULE_FILE="$UDEV_RULE_DIR/71-zflow-capture.rules"
readonly MODULE_FILE="/etc/modules-load.d/zflow.conf"
readonly SLEEP_HOOK_FILE="/usr/lib/systemd/system-sleep/zflow"
install_built=false

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

build_binaries() {
    local cargo_args=(build --locked --release --manifest-path "$REPO_ROOT/Cargo.toml"
        --target-dir "$REPO_ROOT/target" --bin zflow --bin zflowd)
    printf 'Building zflow and zflowd...\n'
    cargo "${cargo_args[@]}"
}

for argument in "$@"; do
    case "$argument" in
        --install-built) install_built=true ;;
        -h|--help)
            printf 'usage: ./scripts/install.sh [--install-built]\n'
            exit 0
            ;;
        *) die "usage: ./scripts/install.sh" ;;
    esac
done
[[ "$(uname -s)" == "Linux" ]] || die "Linux is required"

for source in \
    "$SCRIPT_DIR/uninstall.sh" \
    "$REPO_ROOT/packaging/linux/host-setup.sh" \
    "$REPO_ROOT/packaging/config/zflow.toml" \
    "$REPO_ROOT/packaging/modules-load.d/zflow.conf" \
    "$REPO_ROOT/packaging/system-sleep/zflow" \
    "$REPO_ROOT/packaging/systemd/zflowd.service" \
    "$REPO_ROOT/packaging/udev/70-zflow.rules" \
    "$REPO_ROOT/packaging/udev/71-zflow-capture.rules"; do
    require_regular_source "$source"
done

if [[ "$EUID" -ne 0 ]]; then
    [[ "$install_built" == false ]] || die "--install-built requires root"
    require_command cargo
    require_command sudo
    build_binaries
    exec sudo -- "$SCRIPT_DIR/install.sh" --install-built
fi

if [[ "$install_built" == false ]]; then
    require_command cargo
    build_binaries
fi

for command in install systemctl systemd-analyze; do
    require_command "$command"
done

binary_dir="$REPO_ROOT/target/release"
if [[ -d "$REPO_ROOT/bin" ]]; then binary_dir="$REPO_ROOT/bin"; fi
for binary in zflow zflowd; do
    require_regular_source "$binary_dir/$binary"
    [[ -x "$binary_dir/$binary" ]] || die "binary is not executable: $binary"
done
if command -v dpkg-query >/dev/null 2>&1 && [[ "$(dpkg-query -W -f='${Status}' zflow 2>/dev/null || true)" == 'install ok installed' ]]; then
    die 'zflow is managed by dpkg. Install the release .deb instead of an archive/source build.'
fi

refuse_symlink "$UNIT_FILE"
refuse_symlink "$VIRTUAL_RULE_FILE"
refuse_symlink "$MODULE_FILE"
refuse_symlink "$SLEEP_HOOK_FILE"
refuse_symlink "$LIB_DIR"
refuse_symlink "$UNINSTALLER"

install -d -o root -g root -m 0755 \
    "$BIN_DIR" "$LIB_DIR" "$UDEV_RULE_DIR" /etc/modules-load.d /usr/lib/systemd/system-sleep
install -o root -g root -m 0755 "$binary_dir/zflow" "$BIN_DIR/zflow"
install -o root -g root -m 0755 "$binary_dir/zflowd" "$BIN_DIR/zflowd"
# Release archives are extracted to a temporary directory, so keep a copy of the uninstaller.
install -o root -g root -m 0755 "$SCRIPT_DIR/uninstall.sh" "$UNINSTALLER"
# Retire the old desktop launcher when upgrading an existing installation.
rm -f -- "$BIN_DIR/zflow-gui" /usr/local/share/applications/io.zflow.zflow.desktop
# zflow setup --prelogin on installs its own drop-in when pre-login input is wanted.
rm -f -- "$LEGACY_PRELOGIN_DROPIN"
rmdir -- "$DROPIN_DIR" 2>/dev/null || true
install -o root -g root -m 0644 "$REPO_ROOT/packaging/systemd/zflowd.service" "$UNIT_FILE"
install -o root -g root -m 0644 "$REPO_ROOT/packaging/udev/70-zflow.rules" "$VIRTUAL_RULE_FILE"
install -o root -g root -m 0644 "$REPO_ROOT/packaging/modules-load.d/zflow.conf" "$MODULE_FILE"
install -o root -g root -m 0755 "$REPO_ROOT/packaging/system-sleep/zflow" "$SLEEP_HOOK_FILE"

# Run through bash so a noexec temporary directory still works.
bash "$REPO_ROOT/packaging/linux/host-setup.sh" "$REPO_ROOT/packaging/config/zflow.toml" \
    "$REPO_ROOT/packaging/udev/71-zflow-capture.rules" "$VIRTUAL_RULE_FILE"

systemd-analyze verify "$UNIT_FILE"
systemctl daemon-reload
systemctl enable zflowd.service
systemctl restart zflowd.service

printf '\nzflow is installed and zflowd is running.\n'
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
printf 'Remove zflow with: sudo %s\n' "$UNINSTALLER"
printf 'Pre-login input stays disabled until you grant it during setup.\n'

printf 'For GNOME handoff, run as your desktop user (without sudo):\n'
printf '  %s/zflow desktop-agent --install\n' "$BIN_DIR"
printf '  %s/zflow settings\n' "$BIN_DIR"
