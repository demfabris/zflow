#!/usr/bin/env bash
# Install published zflow binaries on Linux or macOS.
set -Eeuo pipefail

usage() {
    cat <<'USAGE'
Usage: bash install.sh [options]

  --yes                 Accept installation and runtime dependencies
  --version VERSION     Install a release tag (default: latest)
  --headless            Linux: install the service without GNOME integration
  --no-launch           Do not open the app after installation
  -h, --help            Show this help

Run as your normal user. Only system changes request administrator access.
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
    if [[ "$platform" == Linux && -n "${DBUS_SESSION_BUS_ADDRESS:-}" && -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]] && command -v pkexec >/dev/null 2>&1; then
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

linux_dependencies() {
    local packages=()
    if command -v apt-get >/dev/null 2>&1; then
        packages=(acl kmod udev passwd util-linux)
        if [[ "$desktop" == true ]]; then packages+=(gjs gir1.2-gtk-4.0 gir1.2-adw-1); fi
        as_root apt-get update
        as_root apt-get install -y "${packages[@]}"
    elif command -v dnf >/dev/null 2>&1; then
        packages=(acl kmod systemd-udev shadow-utils util-linux)
        if [[ "$desktop" == true ]]; then packages+=(gjs gtk4 libadwaita); fi
        as_root dnf install -y "${packages[@]}"
    elif command -v pacman >/dev/null 2>&1; then
        packages=(acl kmod systemd shadow util-linux)
        if [[ "$desktop" == true ]]; then packages+=(gjs gtk4 libadwaita); fi
        # Do not refresh the package database without upgrading the whole system.
        as_root pacman -S --needed --noconfirm "${packages[@]}"
    else
        die 'Automatic dependency installation supports apt, dnf, and pacman. Install the runtime dependencies and use the installation script inside the release archive.'
    fi
}

check_linux() {
    require systemctl
    [[ -n "$(systemctl show --property=Version --value 2>/dev/null)" ]] || die 'Linux installation requires a running systemd system manager.'
    local command
    for command in getent groupadd useradd runuser setfacl modprobe systemd-analyze udevadm; do require "$command"; done
    udevadm --help | grep 'verify' >/dev/null || die 'udevadm verify is required (systemd 254 or newer).'
    if [[ "$desktop" == true ]]; then
        require gjs
        require gnome-extensions
        local versions gtk_version adw_version
        # These template expressions belong to JavaScript.
        # shellcheck disable=SC2016
        versions=$(gjs -c 'imports.gi.versions.Gtk="4.0"; imports.gi.versions.Adw="1"; const {Gtk,Adw}=imports.gi; print(`${Gtk.get_major_version()}.${Gtk.get_minor_version()} ${Adw.get_major_version()}.${Adw.get_minor_version()}`);') \
            || die 'Install the GTK4 and libadwaita GObject introspection libraries.'
        IFS=' ' read -r gtk_version adw_version <<< "$versions"
        if ! version_at_least "$gtk_version" 4.12 || ! version_at_least "$adw_version" 1.5; then
            die 'GNOME settings require GTK 4.12+ and libadwaita 1.5+. Upgrade the distribution or use --headless.'
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
    local expected actual
    expected=$(awk -v name="$asset" '$2 == name && NF == 2 {hash=$1; count++} END {if (count == 1) print hash; else exit 1}' "$work_dir/SHA256SUMS") \
        || die "The release has no unique checksum for $asset."
    [[ "$expected" =~ ^[0-9a-f]{64}$ ]] || die 'Invalid release checksum.'
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$work_dir/$asset" | awk '{print $1}')
    else
        actual=$(shasum -a 256 "$work_dir/$asset" | awk '{print $1}')
    fi
    [[ "$actual" == "$expected" ]] || die "Checksum mismatch for $asset; nothing was installed."
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
        x86_64|amd64) architecture=x86_64; deb_arch=amd64 ;;
        arm64|aarch64) architecture=aarch64; deb_arch=arm64 ;;
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
        if [[ "$use_deb" == true ]]; then asset="zflow_${version#v}_${deb_arch}.deb";
        else asset="zflow-${version}-${architecture}-unknown-linux-gnu.tar.gz"; fi
    else
        asset="zflow-${version}-${architecture}-apple-darwin.tar.gz"
    fi
}

install_linux() {
    say 'Installing the service. An existing zflow connection will disconnect during its restart.'
    if [[ "$use_deb" == true ]]; then
        # apt drops privileges to _apt while reading the verified local package.
        chmod 0755 "$work_dir"
        chmod 0644 "$work_dir/$asset"
        as_root apt-get update
        if [[ "$desktop" == true ]]; then as_root apt-get install -y "$work_dir/$asset";
        else as_root apt-get install -y --no-install-recommends "$work_dir/$asset"; fi
        installed_cli=/usr/bin/zflow
    else
        linux_dependencies
        check_linux
        as_root bash "$payload_dir/scripts/install.sh" --install-built
        installed_cli=/usr/local/bin/zflow
    fi
    if [[ "$desktop" == true ]]; then
        say 'Installing GNOME integration for your desktop account…'
        check_linux
        local output
        if [[ "$use_deb" == true ]]; then
            # The package owns the desktop files and extension under /usr/share.
            gnome-extensions enable zflow@demfabris 2>/dev/null || true
            say 'Enable Start at Login in Settings to start sharing on future logins.'
        elif output=$("$installed_cli" desktop-agent --install </dev/null 2>&1); then
            printf '%s\n' "$output"
        else
            printf '%s\n' "$output" >&2
            # The CLI reports this only after writing all assets, if Shell cannot enable them yet.
            [[ "$output" == *'Integration installed.'* ]] \
                || die 'The service is installed, but desktop setup failed. Run zflow desktop-agent --install after correcting the error above.'
        fi
        say 'Log out and back in to load the GNOME extension. Enable zflow in GNOME Extensions if needed, then open it from Applications.'
        if [[ "$launch" == true ]]; then
            "$installed_cli" settings </dev/null >/dev/null 2>&1 &
        fi
    else
        say 'The Linux service is installed. Run zflow desktop-agent --install from a GNOME session to add the desktop app.'
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
    local assume_yes=false headless=false command base_url
    platform=$(uname -s)
    version=latest
    launch=true
    desktop=false
    work_dir=''
    mac_stage=''
    mac_destination=/Applications/zflow.app
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --yes) assume_yes=true ;;
            --headless) headless=true ;;
            --no-launch) launch=false ;;
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
    for command in curl tar awk mktemp; do require "$command"; done
    if ! command -v sha256sum >/dev/null 2>&1; then require shasum; fi
    if [[ "$version" == latest ]]; then version=$(latest_version); fi
    case "$version" in v*) ;; *) version="v$version";; esac
    [[ "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$ ]] || die 'Invalid release version.'
    if [[ "$platform" == Linux ]]; then
        require systemctl
        [[ -n "$(systemctl show --property=Version --value 2>/dev/null)" ]] || die 'Linux installation requires a running systemd system manager.'
        if [[ "$headless" == false && -n "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
            case ":${XDG_CURRENT_DESKTOP:-}:" in *:GNOME:*|*:gnome:*) desktop=true;; esac
        fi
    else
        check_macos
    fi
    select_artifact
    say "zflow binary installer ($version, $platform $architecture)"
    printf 'Download: %s\n' "$asset"
    printf 'Existing service/app configuration and paired identities are preserved.\n'
    if [[ "$assume_yes" == false ]]; then
        local answer
        printf 'Install zflow and its runtime dependencies? [y/N] ' >/dev/tty 2>/dev/null || die 'No terminal is available; pass --yes to accept installation.'
        IFS= read -r answer </dev/tty || die 'Could not read confirmation.'
        case "$answer" in y|Y|yes|YES) ;; *) say 'Installation cancelled.'; return;; esac
    fi
    work_dir=$(mktemp -d "${TMPDIR:-/tmp}/zflow-install.XXXXXXXX")
    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    base_url="https://github.com/demfabris/zflow/releases/download/$version"
    fetch "$base_url/SHA256SUMS" "$work_dir/SHA256SUMS" || die "Checksums are unavailable for $version."
    fetch "$base_url/$asset" "$work_dir/$asset" || die "No downloadable artifact for $platform $architecture in $version."
    verify_download
    if [[ "$use_deb" == false ]]; then
        payload_dir="$work_dir/payload"
        mkdir "$payload_dir"
        tar -xzf "$work_dir/$asset" --strip-components=1 -C "$payload_dir"
        if [[ "$platform" == Linux ]]; then
            [[ -x "$payload_dir/bin/zflow" && -x "$payload_dir/bin/zflowd" && -f "$payload_dir/scripts/install.sh" ]] \
                || die 'The release archive is missing the Linux binaries or installer.'
            "$payload_dir/bin/zflow" --version
        fi
    fi
    if [[ "$platform" == Linux ]]; then install_linux; else install_macos; fi
    say 'zflow installation finished.'
}

# Keep the final invocation after all definitions so a truncated curl download cannot start setup.
if [[ -z "${BASH_SOURCE[0]:-}" || "${BASH_SOURCE[0]:-}" == "$0" ]]; then main "$@"; fi
