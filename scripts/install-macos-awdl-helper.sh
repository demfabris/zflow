#!/usr/bin/env bash
set -Eeuo pipefail
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
umask 077

die() {
    printf 'install-awdl: %s\n' "$*" >&2
    exit 1
}

[[ "$(uname -s)" == Darwin ]] || die 'macOS is required'
[[ "$EUID" -eq 0 ]] || die 'run this installer with sudo after building the helper'
[[ $# -eq 1 ]] || die 'usage: sudo bash scripts/install-macos-awdl-helper.sh target/debug/zflow-awdl-helper'
readonly source_binary="$1"
readonly install_dir=/Library/PrivilegedHelperTools
readonly destination="$install_dir/io.zflow.awdl-helper"
[[ -f "$source_binary" && ! -L "$source_binary" ]] || die 'helper source must be a regular file, not a symlink'
exec 3<"$source_binary"
[[ "$(stat -f '%HT' <&3)" == 'Regular File' ]] || die 'opened helper source is not a regular file'
source_owner="$(stat -f '%u' <&3)"
[[ "$source_owner" == 0 || "$source_owner" == "${SUDO_UID:-0}" ]] || die 'helper source belongs to another user'
source_mode="$(stat -f '%Lp' <&3)"
(( (8#$source_mode & 0022) == 0 )) || die 'helper source is group- or world-writable'

validate_directory() {
    [[ -d "$1" && ! -L "$1" ]] || die "unsafe installation directory: $1"
    [[ "$(stat -f '%u' "$1")" == 0 ]] || die "installation directory is not root-owned: $1"
    local directory_mode
    directory_mode="$(stat -f '%Lp' "$1")"
    (( (8#$directory_mode & 0022) == 0 )) || die "installation directory is group- or world-writable: $1"
    [[ "$(ls -lde "$1")" != *'+'* ]] || die "remove ACLs from the installation directory before installing: $1"
}

validate_directory /
validate_directory /Library
if [[ ! -e "$install_dir" && ! -L "$install_dir" ]]; then
    install -d -o root -g wheel -m 0755 "$install_dir"
fi
validate_directory "$install_dir"
[[ ! -L "$destination" ]] || die 'refusing to replace a symlink'
if [[ -e "$destination" ]]; then
    [[ -f "$destination" && "$(stat -f '%u' "$destination")" == 0 ]] || die 'existing helper is not a root-owned regular file'
fi

temporary_dir="$(mktemp -d "$install_dir/.zflow-install.XXXXXXXX")"
cleanup() {
    rm -f "$temporary_dir/helper"
    rmdir "$temporary_dir"
}
trap cleanup EXIT
install -o root -g wheel -m 0755 /dev/fd/3 "$temporary_dir/helper"
exec 3<&-
file -b "$temporary_dir/helper" | grep -q '^Mach-O .*executable' || die 'build the native macOS helper before installing'
chmod -N "$temporary_dir/helper"
chmod 4755 "$temporary_dir/helper"
mv -f "$temporary_dir/helper" "$destination"
printf 'Installed %s (root:wheel, mode 4755). No network settings changed.\n' "$destination"
