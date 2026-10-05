#!/usr/bin/env bash
# Install published zflow binaries on Linux or macOS.
set -Eeuo pipefail

usage() {
    cat <<'USAGE'
Usage: bash install.sh [options]

  --version VERSION     Install a release tag (default: latest)
  --headless            Linux: install the service without GNOME integration
  --no-launch           Do not open the app after installation
  --update FROM_VERSION Linux: update this installed version without desktop setup
  --gui                 Linux update: require graphical administrator approval
  --yes                 Accepted for older command lines; changes nothing
  -h, --help            Show this help

Run as your normal user. The administrator password prompt is the only
confirmation; cancel it to stop without changing anything.
USAGE
}

die() { printf 'zflow install: %s\n' "$*" >&2; exit 1; }
say() { printf '\n%s\n' "$*"; }
require() { command -v "$1" >/dev/null 2>&1 || die "Required command is missing: $1"; }

version_at_least() {
    awk -v actual="$1" -v required="$2" 'BEGIN {
        split(actual, a, "."); split(required, r, ".");
        for (i = 1; i <= 3; i++) {
            if (a[i]+0 > r[i]+0) exit 0;
            if (a[i]+0 < r[i]+0) exit 1;
        }
        exit 0;
    }'
}

as_root() {
    if [[ "${gui:-false}" == true ]]; then
        require pkexec
        pkexec --disable-internal-agent "$@" </dev/null
    elif [[ "$platform" == Linux && -n "${DBUS_SESSION_BUS_ADDRESS:-}" && -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]] && command -v pkexec >/dev/null 2>&1; then
        pkexec --disable-internal-agent "$@" </dev/null
    else
        require sudo
        sudo -- "$@" </dev/null
    fi
}

fetch() {
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --silent --show-error \
        --location --retry 3 --connect-timeout 20 --max-time 600 --output "$2" "$1" </dev/null
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi | awk '{print $1}'
}

cleanup() {
    local status=$?
    trap - EXIT
    if [[ -n "${mac_stage:-}" ]]; then
        # Restore the previous bundle if installation stopped between the two renames.
        if [[ -d "$mac_stage/previous.app" && ! -e "$mac_destination" ]]; then
            if ! as_root mv "$mac_stage/previous.app" "$mac_destination"; then
                printf 'Previous app retained at %s/previous.app\n' "$mac_stage" >&2
                mac_stage=''
            fi
        fi
        if [[ -n "$mac_stage" ]]; then as_root rm -rf -- "$mac_stage" || true; fi
    fi
    if [[ -n "${work_dir:-}" ]]; then rm -rf -- "$work_dir"; fi
    exit "$status"
}

# Runtime packages the root step installs before an archive.
linux_dependencies() {
    if command -v apt-get >/dev/null 2>&1; then
        manager=apt-get
        packages=(kmod udev passwd util-linux curl)
        if [[ "$desktop" == true ]]; then packages+=(gjs gir1.2-gtk-4.0 gir1.2-adw-1 pkexec); fi
    elif command -v dnf >/dev/null 2>&1; then
        manager=dnf
        packages=(kmod systemd-udev shadow-utils util-linux curl)
        if [[ "$desktop" == true ]]; then packages+=(gjs gtk4 libadwaita polkit); fi
    elif command -v pacman >/dev/null 2>&1; then
        manager=pacman
        packages=(kmod systemd shadow util-linux curl)
        if [[ "$desktop" == true ]]; then packages+=(gjs gtk4 libadwaita polkit); fi
    else
        die 'Automatic dependency installation supports apt, dnf, and pacman. Install the runtime dependencies and use the installation script inside the release archive.'
    fi
}

# Every Linux system change, run as root in a shell of its own (see install_linux).
# It works on a root-owned copy of the download and checks the checksum again, so
# a process running as the user cannot swap files while the password prompt is open.
# Arguments: DOWNLOAD SHA256 FROM_VERSION INSTALLED_CLI, then apt options for a
# .deb, or the package manager and runtime packages for an archive.
root_install() {
    local source=$1 checksum=$2 previous=$3 installed_cli=$4 asset=${1##*/} manager
    shift 4
    update_lock=false
    if [[ -n "$previous" ]]; then
        # Serialize updates across login sessions. Recheck after a download or
        # password prompt so an older request cannot replace a newer install.
        umask 077
        exec 9>/run/zflow-update.lock
        flock --exclusive 9
        update_lock=true
        [[ "$("$installed_cli" --version)" == "zflow $previous" ]] \
            || die 'zflow changed while this update was downloading. Check for updates again.'
    fi
    root_stage=$(mktemp -d /tmp/zflow-install.XXXXXXXX)
    trap 'rm -rf -- "$root_stage"; if [[ "$update_lock" == true ]]; then flock --unlock 9; fi' EXIT
    cp -- "$source" "$root_stage/$asset"
    [[ "$(sha256_of "$root_stage/$asset")" == "$checksum" ]] || die "Checksum mismatch for $asset; nothing was installed."
    if [[ "$asset" == *.deb ]]; then
        # apt reads local packages as the unprivileged _apt user.
        chmod 0755 "$root_stage"
        chmod 0644 "$root_stage/$asset"
        apt-get update
        apt-get install -y "$@" "$root_stage/$asset"
        return
    fi
    manager=$1
    shift
    case "$manager" in
        apt-get) apt-get update; apt-get install -y "$@" ;;
        dnf) dnf install -y "$@" ;;
        # Do not refresh the package database without upgrading the whole system.
        pacman) pacman -S --needed --noconfirm "$@" ;;
        *) die "Unsupported package manager: $manager" ;;
    esac
    mkdir "$root_stage/payload"
    tar -xzf "$root_stage/$asset" --no-same-owner --strip-components=1 -C "$root_stage/payload"
    bash "$root_stage/payload/scripts/install.sh" --install-built
}

# Runs before anything changes. Tools such as groupadd live in /usr/sbin, which is not
# on a normal user's PATH everywhere (Debian), so the root-side setup checks those.
check_linux() {
    require udevadm
    udevadm --help | grep 'verify' >/dev/null || die 'udevadm verify is required (systemd 254 or newer).'
    if [[ "$desktop" == true ]]; then
        local versions gtk_version adw_version
        # Missing libraries are installed with zflow; only reject ones that are too old.
        # These template expressions belong to JavaScript.
        # shellcheck disable=SC2016
        if command -v gjs >/dev/null 2>&1 && versions=$(gjs -c 'imports.gi.versions.Gtk="4.0"; imports.gi.versions.Adw="1"; const {Gtk,Adw}=imports.gi; print(`${Gtk.get_major_version()}.${Gtk.get_minor_version()} ${Adw.get_major_version()}.${Adw.get_minor_version()}`);' 2>/dev/null); then
            IFS=' ' read -r gtk_version adw_version <<< "$versions"
            if ! version_at_least "$gtk_version" 4.12 || ! version_at_least "$adw_version" 1.5; then
                die 'GNOME settings require GTK 4.12+ and libadwaita 1.5+. Upgrade the distribution or use --headless.'
            fi
        fi
    fi
}

check_macos() {
    version_at_least "$(sw_vers -productVersion)" 26.0 || die 'The native app requires macOS 26 or newer.'
    require codesign
    require ditto
}

latest_version() {
    local url
    url=$(curl --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --silent --show-error \
        --head --location --retry 3 --connect-timeout 20 --max-time 60 \
        --output /dev/null --write-out '%{url_effective}' \
        https://github.com/demfabris/zflow/releases/latest </dev/null) \
        || die 'No release could be found. Check https://github.com/demfabris/zflow/releases.'
    [[ "$url" == https://github.com/demfabris/zflow/releases/tag/* ]] || die 'Unexpected release redirect.'
    printf '%s\n' "${url##*/}"
}

verify_download() {
    checksum=$(awk -v name="$asset" '$2 == name && NF == 2 {hash=$1; count++} END {if (count == 1) print hash; else exit 1}' "$work_dir/SHA256SUMS") \
        || die "The release has no unique checksum for $asset."
    [[ "$checksum" =~ ^[0-9a-f]{64}$ ]] || die 'Invalid release checksum.'
    [[ "$(sha256_of "$work_dir/$asset")" == "$checksum" ]] || die "Checksum mismatch for $asset; nothing was installed."
}

archive_install_present() {
    [[ -e /usr/local/bin/zflow || -e /usr/local/bin/zflowd || -e /etc/systemd/system/zflowd.service ]]
}

select_artifact() {
    local machine libc_version
    machine=$(uname -m)
    if [[ "$platform" == Darwin && "$machine" == x86_64 ]] && [[ "$(sysctl -in sysctl.proc_translated 2>/dev/null || true)" == 1 ]]; then
        machine=arm64
    fi
    case "$machine" in
        x86_64|amd64) architecture=x86_64 ;;
        arm64|aarch64) architecture=aarch64 ;;
        *) die "No release binary is available for $machine." ;;
    esac
    use_deb=false
    if [[ "$platform" == Linux ]]; then
        libc_version=$(getconf GNU_LIBC_VERSION 2>/dev/null) || die 'Linux release binaries require glibc 2.39 or newer; musl is not supported.'
        version_at_least "${libc_version#glibc }" 2.39 || die 'Linux release binaries require glibc 2.39 or newer.'
        # Keep existing source/archive installs in place instead of shadowing them with /usr/bin.
        if command -v apt-get >/dev/null 2>&1 && ! archive_install_present; then
            use_deb=true
        elif command -v dpkg-query >/dev/null 2>&1 && [[ "$(dpkg-query -W -f='${Status}' zflow 2>/dev/null || true)" == 'install ok installed' ]]; then
            use_deb=true
        fi
        if [[ "$use_deb" == true ]]; then
            # A 32-bit userland can run on a 64-bit kernel; the package must match dpkg.
            deb_arch=$(dpkg --print-architecture)
            case "$deb_arch" in
                amd64|arm64) asset="zflow_${version#v}_${deb_arch}.deb" ;;
                *) die "No release package is available for $deb_arch." ;;
            esac
        else
            asset="zflow-${version}-${architecture}-unknown-linux-gnu.tar.gz"
        fi
    else
        # Releases after v0.1.0 ship one universal Mac archive; older ones had
        # one per CPU, which main falls back to when SHA256SUMS lacks this name.
        asset="zflow-${version}-universal-apple-darwin.tar.gz"
        legacy_asset="zflow-${version}-${architecture}-apple-darwin.tar.gz"
    fi
}

# What happens next, printed before the password prompt that confirms it.
describe_linux() {
    if [[ "$use_deb" == true ]]; then
        printf '  - the zflow command and the zflowd service, from %s and apt\n' "$asset"
    else
        linux_dependencies
        printf '  - the zflow command and the zflowd service, under /usr/local\n'
        printf '  - runtime packages from %s: %s\n' "$manager" "${packages[*]}"
    fi
    printf '  - a locked zflow system account that types remote input through /dev/uinput\n'
    printf '  - an allow rule for UDP port 43119 when ufw or firewalld is on\n'
    if [[ "$desktop" == true ]]; then
        printf '  - for every GNOME user: the zflow launcher, and the desktop agent at login\n'
        printf '  - for you: the zflow GNOME extension (GNOME asks before it downloads it)\n'
    fi
}

install_linux() {
    say 'Installing the service. An existing zflow connection will disconnect during its restart.'
    if [[ "$use_deb" == true ]]; then installed_cli=/usr/bin/zflow; else installed_cli=/usr/local/bin/zflow; fi
    local root_args=("$work_dir/$asset" "$checksum" "${update_from:-}" "$installed_cli")
    if [[ "$use_deb" == true ]]; then
        if [[ "$desktop" == true ]]; then root_args+=(--install-recommends); else root_args+=(--no-install-recommends); fi
    else
        linux_dependencies
        root_args+=("$manager" "${packages[@]}")
    fi
    # One elevated shell makes every system change: one password prompt, and
    # cancelling it leaves nothing half installed.
    as_root bash -c "set -euo pipefail; $(declare -f die sha256_of root_install); root_install \"\$@\"" \
        zflow-install "${root_args[@]}"
    # The running desktop agent detects its replaced executable and restarts.
    # Keep the GTK caller alive so it can show the result and reopen settings.
    if [[ "${update:-false}" == true ]]; then return; fi
    if [[ "$desktop" == false ]]; then
        say 'The Linux service is installed. Run zflow desktop-agent --install from a GNOME session to add the desktop app.'
        return
    fi
    say 'Setting up GNOME for your account…'
    # Adds the extension and starts the desktop agent in this session.
    "$installed_cli" desktop-agent --install </dev/null \
        || die 'The service is installed, but desktop setup failed. Run zflow desktop-agent --install after correcting the error above.'
    if [[ "$launch" == true ]]; then
        "$installed_cli" settings </dev/null >/dev/null 2>&1 &
    fi
}

install_macos() {
    local bundle="$payload_dir/zflow.app"
    [[ -d "$bundle" && ! -L "$bundle" ]] || die 'The release archive does not contain zflow.app.'
    codesign --verify --strict --deep "$bundle"
    [[ ! -L "$mac_destination" ]] || die "Refusing to replace a symlink at $mac_destination."
    if [[ -e "$mac_destination" ]]; then
        [[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$mac_destination/Contents/Info.plist")" == io.zflow.zflow ]] \
            || die "$mac_destination is not a zflow app bundle."
    fi
    if pgrep -x zflow-app >/dev/null; then
        say 'Closing zflow before updating the app…'
        osascript -e 'tell application id "io.zflow.zflow" to quit' </dev/null
        local attempt
        for ((attempt = 0; attempt < 20; attempt++)); do
            if ! pgrep -x zflow-app >/dev/null; then break; fi
            sleep 0.5
        done
        if pgrep -x zflow-app >/dev/null; then die 'zflow is still running. Quit it and rerun the installer.'; fi
    fi
    say "Installing ${mac_destination}…"
    mac_stage=$(as_root mktemp -d "${mac_destination%/*}/.zflow-install.XXXXXXXX")
    as_root ditto "$bundle" "$mac_stage/zflow.app"
    as_root codesign --verify --strict --deep "$mac_stage/zflow.app"
    if [[ -e "$mac_destination" ]]; then as_root mv "$mac_destination" "$mac_stage/previous.app"; fi
    as_root mv "$mac_stage/zflow.app" "$mac_destination"
    as_root rm -rf -- "$mac_stage"
    mac_stage=''
    say 'Installed zflow. Allow Accessibility and Local Network access when the app requests them.'
    if codesign --display --verbose=2 "$mac_destination" 2>&1 | grep 'Signature=adhoc' >/dev/null; then
        printf 'This build uses an ad-hoc signature. Input sharing works; AWDL helper installation requires an Apple signing identity.\n'
    fi
    if [[ "$launch" == true ]]; then open "$mac_destination"; fi
}

main() {
    local headless=false command base_url
    platform=$(uname -s)
    version=latest
    launch=true
    desktop=false
    update=false
    update_from=''
    gui=false
    work_dir=''
    mac_stage=''
    mac_destination=/Applications/zflow.app
    while [[ $# -gt 0 ]]; do
        case "$1" in
            # The password prompt is the confirmation; --yes stays valid for old commands.
            --yes) ;;
            --headless) headless=true ;;
            --no-launch) launch=false ;;
            --update)
                [[ $# -ge 2 && -n "$2" && "$2" != --* ]] || die '--update requires the installed version'
                update=true; update_from="$2"; shift ;;
            --gui) gui=true ;;
            --version)
                [[ $# -ge 2 && -n "$2" && "$2" != --* ]] || die '--version requires a release version'
                version="$2"; shift ;;
            -h|--help) usage; return ;;
            *) die "Unknown option: $1" ;;
        esac
        shift
    done
    case "$platform" in Linux|Darwin) ;; *) die "Unsupported operating system: $platform.";; esac
    [[ "$(id -u)" != 0 ]] || die 'Run this installer as your normal user, without sudo. It requests administrator access when needed.'
    [[ -n "${HOME:-}" && "$HOME" == /* ]] || die 'HOME must be an absolute path.'
    [[ "$platform" == Linux || "$headless" == false ]] || die '--headless is only available on Linux.'
    [[ "$platform" == Linux || "$update" == false && "$gui" == false ]] || die '--update and --gui are only available on Linux.'
    if [[ "$gui" == true ]]; then
        [[ "$update" == true ]] || die '--gui requires --update.'
        [[ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ]] || die 'Open updates from your desktop session.'
        require pkexec
    fi
    for command in curl tar awk mktemp; do require "$command"; done
    if ! command -v sha256sum >/dev/null 2>&1; then require shasum; fi
    if [[ "$version" == latest ]]; then version=$(latest_version); fi
    case "$version" in v*) ;; *) version="v$version";; esac
    [[ "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$ ]] || die 'Invalid release version.'
    if [[ "$platform" == Linux ]]; then
        if [[ "$update" == true ]]; then require flock; fi
        require systemctl
        [[ -n "$(systemctl show --property=Version --value 2>/dev/null)" ]] || die 'Linux installation requires a running systemd system manager.'
        if [[ "$headless" == false && -n "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
            case ":${XDG_CURRENT_DESKTOP:-}:" in *:GNOME:*|*:gnome:*) desktop=true;; esac
        fi
        check_linux
    else
        check_macos
    fi
    select_artifact
    say "zflow binary installer ($version, $platform $architecture)"
    printf 'Download: %s\n' "$asset"
    if [[ "$platform" == Linux ]]; then
        printf 'This installs:\n'
        describe_linux
    fi
    printf 'Existing service/app configuration and paired identities are preserved.\n'
    printf 'The administrator password prompt confirms the installation.\n'
    work_dir=$(mktemp -d "${TMPDIR:-/tmp}/zflow-install.XXXXXXXX")
    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    base_url="https://github.com/demfabris/zflow/releases/download/$version"
    fetch "$base_url/SHA256SUMS" "$work_dir/SHA256SUMS" || die "Checksums are unavailable for $version."
    if [[ "$platform" == Darwin ]] && ! awk -v name="$asset" '$2 == name {found = 1} END {exit !found}' "$work_dir/SHA256SUMS"; then
        asset=$legacy_asset
        printf 'This release has no universal archive. Download: %s\n' "$asset"
    fi
    fetch "$base_url/$asset" "$work_dir/$asset" || die "No downloadable artifact for $platform $architecture in $version."
    verify_download
    if [[ "$use_deb" == false ]]; then
        payload_dir="$work_dir/payload"
        mkdir "$payload_dir"
        tar -xzf "$work_dir/$asset" --strip-components=1 -C "$payload_dir"
        if [[ "$platform" == Linux ]]; then
            # Only a check before asking for a password; the root step extracts its own copy.
            [[ -x "$payload_dir/bin/zflow" && -x "$payload_dir/bin/zflowd" && -f "$payload_dir/scripts/install.sh" ]] \
                || die 'The release archive is missing the Linux binaries or installer.'
        fi
    fi
    if [[ "$platform" == Linux ]]; then install_linux; else install_macos; fi
    say 'zflow installation finished.'
}

# Keep the final invocation after all definitions so a truncated curl download cannot start setup.
if [[ -z "${BASH_SOURCE[0]:-}" || "${BASH_SOURCE[0]:-}" == "$0" ]]; then main "$@"; fi
