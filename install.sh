#!/usr/bin/env bash
# Download, build, and install zflow on Linux or macOS.
set -Eeuo pipefail

usage() {
    cat <<'USAGE'
Usage: bash install.sh [options]

  --yes                 Accept installation of dependencies and Rust
  --ref REF             Git branch, tag, or commit to build (default: main)
  --source PATH         Build an existing checkout instead of downloading it
  --headless            Linux: install the service without GNOME integration
  --no-launch           Do not open the app after installation
  --skip-dependencies   Use the existing build tools and runtime dependencies
  --sign IDENTITY       macOS: sign with an installed Apple signing identity
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
        packages=(build-essential pkg-config curl ca-certificates acl kmod udev passwd util-linux)
        if [[ "$desktop" == true ]]; then packages+=(gjs gir1.2-gtk-4.0 gir1.2-adw-1); fi
        as_root apt-get update
        as_root apt-get install -y "${packages[@]}"
    elif command -v dnf >/dev/null 2>&1; then
        packages=(gcc gcc-c++ make pkgconf-pkg-config curl ca-certificates acl kmod systemd-udev shadow-utils util-linux)
        if [[ "$desktop" == true ]]; then packages+=(gjs gtk4 libadwaita); fi
        as_root dnf install -y "${packages[@]}"
    elif command -v pacman >/dev/null 2>&1; then
        packages=(base-devel pkgconf curl ca-certificates acl kmod systemd shadow util-linux)
        if [[ "$desktop" == true ]]; then packages+=(gjs gtk4 libadwaita); fi
        # Do not refresh the package database without upgrading the whole system.
        as_root pacman -S --needed --noconfirm "${packages[@]}"
    else
        die 'Automatic dependency installation supports apt, dnf, and pacman. Install the prerequisites and rerun with --skip-dependencies.'
    fi
}

check_linux() {
    require systemctl
    [[ -n "$(systemctl show --property=Version --value 2>/dev/null)" ]] || die 'Linux installation requires a running systemd system manager.'
    local command
    for command in cc make pkg-config getent groupadd useradd runuser setfacl modprobe systemd-analyze udevadm; do require "$command"; done
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
    local version swift_version sdk_version
    version=$(sw_vers -productVersion)
    version_at_least "$version" 26.0 || die 'The native app requires macOS 26 or newer.'
    if ! xcode-select -p >/dev/null 2>&1; then
        if [[ "$skip_dependencies" == false ]]; then xcode-select --install || true; fi
        die 'Install Apple Command Line Tools with Swift 6.2+ and the macOS 26 SDK, then rerun this installer.'
    fi
    swift_version=$(swift --version | sed -n 's/.*Swift version \([0-9][0-9.]*\).*/\1/p' | head -n 1)
    if [[ -z "$swift_version" ]] || ! version_at_least "$swift_version" 6.2; then
        die 'Swift 6.2 or newer is required. Update Xcode or Apple Command Line Tools.'
    fi
    sdk_version=$(xcrun --sdk macosx --show-sdk-version)
    version_at_least "$sdk_version" 26.0 || die 'Select an Apple toolchain containing the macOS 26 SDK or newer.'
    require python3
    require codesign
    require ditto
}

ensure_rust() {
    local minimum current rust_bin
    minimum=$(sed -n 's/^rust-version = "\([0-9.]*\)"/\1/p' "$source_dir/Cargo.toml")
    [[ -n "$minimum" ]] || die 'Could not read the required Rust version from Cargo.toml.'
    rust_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
    if ! command -v cargo >/dev/null 2>&1 && [[ -x "$rust_bin/cargo" ]]; then export PATH="$rust_bin:$PATH"; fi
    if command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1; then
        current=$(rustc --version | awk '{print $2}')
        if version_at_least "$current" "$minimum"; then return; fi
    fi
    [[ "$skip_dependencies" == false ]] || die "Rust $minimum or newer is required."
    say "Installing a current Rust toolchain for your user account."
    if command -v rustup >/dev/null 2>&1; then
        rustup toolchain install stable --profile minimal </dev/null
    else
        fetch https://sh.rustup.rs "$work_dir/rustup.sh"
        sh "$work_dir/rustup.sh" -y --profile minimal --default-toolchain stable --no-modify-path </dev/null
        export PATH="$rust_bin:$PATH"
    fi
    # Select stable for this build without changing an existing default toolchain.
    export RUSTUP_TOOLCHAIN=stable
    require cargo
    require rustc
    current=$(rustc --version | awk '{print $2}')
    version_at_least "$current" "$minimum" || die "Rust $minimum or newer is required."
}

install_linux() {
    say 'Building the Linux service and desktop app…'
    cargo build --locked --release --manifest-path "$source_dir/Cargo.toml" \
        --target-dir "$source_dir/target" --bin zflow --bin zflowd </dev/null
    say 'Installing the service. An existing zflow connection will disconnect during its restart.'
    as_root bash "$source_dir/scripts/install.sh" --install-built
    if [[ "$desktop" == true ]]; then
        say 'Installing GNOME integration for your desktop account…'
        local output
        if output=$(/usr/local/bin/zflow desktop-agent --install </dev/null 2>&1); then
            printf '%s\n' "$output"
        else
            printf '%s\n' "$output" >&2
            # The CLI reports this only after writing all assets, if Shell cannot enable them yet.
            [[ "$output" == *'Integration installed.'* ]] \
                || die 'The service is installed, but desktop setup failed. Run zflow desktop-agent --install after correcting the error above.'
        fi
        say 'Log out and back in to load the GNOME extension. Enable zflow in GNOME Extensions if needed, then open it from Applications.'
        if [[ "$launch" == true ]]; then
            /usr/local/bin/zflow settings </dev/null >/dev/null 2>&1 &
        fi
    else
        say 'The Linux service is installed. Run zflow desktop-agent --install from a GNOME session to add the desktop app.'
    fi
}

install_macos() {
    local args=() bundle
    if [[ -n "$sign_identity" ]]; then args+=(--sign "$sign_identity"); fi
    # Bash 3.2 treats an empty array as unset under nounset.
    if [[ ${#args[@]} -gt 0 ]]; then bash "$source_dir/scripts/build-macos-app.sh" "${args[@]}" </dev/null;
    else bash "$source_dir/scripts/build-macos-app.sh" </dev/null; fi
    bundle="$source_dir/target/release/zflow.app"
    [[ -d "$bundle" && ! -L "$bundle" ]] || die "The build did not produce $bundle"
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
    say "Installing $mac_destination…"
    mac_stage=$(as_root mktemp -d "${mac_destination%/*}/.zflow-install.XXXXXXXX")
    as_root ditto "$bundle" "$mac_stage/zflow.app"
    as_root codesign --verify --strict --deep "$mac_stage/zflow.app"
    if [[ -e "$mac_destination" ]]; then as_root mv "$mac_destination" "$mac_stage/previous.app"; fi
    as_root mv "$mac_stage/zflow.app" "$mac_destination"
    as_root rm -rf -- "$mac_stage"
    mac_stage=''
    say 'Installed zflow. Allow Accessibility and Local Network access when the app requests them.'
    if [[ -z "$sign_identity" || "$sign_identity" == - ]]; then
        printf 'This build uses an ad-hoc signature. Input sharing works; AWDL helper installation requires an Apple signing identity.\n'
    fi
    if [[ "$launch" == true ]]; then open "$mac_destination"; fi
}

main() {
    local ref=main source_option='' assume_yes=false headless=false command
    platform=$(uname -s)
    launch=true
    skip_dependencies=false
    sign_identity=''
    desktop=false
    work_dir=''
    mac_stage=''
    mac_destination=/Applications/zflow.app
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --yes) assume_yes=true ;;
            --headless) headless=true ;;
            --no-launch) launch=false ;;
            --skip-dependencies) skip_dependencies=true ;;
            --ref|--source|--sign)
                [[ $# -ge 2 && -n "$2" && "$2" != --* ]] || die "$1 requires a value"
                case "$1" in --ref) ref="$2";; --source) source_option="$2";; --sign) sign_identity="$2";; esac
                shift ;;
            -h|--help) usage; return ;;
            *) die "Unknown option: $1" ;;
        esac
        shift
    done
    case "$platform" in Linux|Darwin) ;; *) die "Unsupported operating system: $platform (Linux and macOS are supported).";; esac
    [[ "$(id -u)" != 0 ]] || die 'Run this installer as your normal user, without sudo. It requests administrator access when needed.'
    [[ -n "${HOME:-}" && "$HOME" == /* ]] || die 'HOME must be an absolute path.'
    [[ "$ref" =~ ^[A-Za-z0-9][A-Za-z0-9._/-]*$ && "$ref" != *..* ]] || die 'Invalid Git ref.'
    [[ "$platform" == Darwin || -z "$sign_identity" ]] || die '--sign is only available on macOS.'
    [[ "$platform" == Linux || "$headless" == false ]] || die '--headless is only available on Linux.'
    if [[ "$platform" == Linux && "$headless" == false ]]; then
        case ":${XDG_CURRENT_DESKTOP:-}:" in *:GNOME:*|*:gnome:*) desktop=true;; esac
        if [[ "$desktop" == true && -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then desktop=false; fi
    fi
    for command in curl tar awk sed mktemp; do require "$command"; done
    if [[ -n "$source_option" ]]; then
        source_dir=$(cd -- "$source_option" && pwd -P) || die 'The source checkout does not exist.'
    fi
    say "zflow source installer ($platform)"
    printf 'Source: %s\n' "${source_option:-https://github.com/demfabris/zflow/tree/$ref}"
    if [[ "$platform" == Linux ]]; then
        printf 'Install/update the system service; GNOME integration: %s.\n' "$desktop"
    else
        printf 'Build the native app and install/update /Applications/zflow.app.\n'
    fi
    if [[ "$skip_dependencies" == false ]]; then printf 'Install missing build/runtime prerequisites and Rust as needed.\n'; fi
    printf 'Existing service/app configuration and paired identities are preserved.\n'
    if [[ "$assume_yes" == false ]]; then
        local answer
        printf 'Continue? [y/N] ' >/dev/tty 2>/dev/null || die 'No terminal is available; pass --yes to accept installation.'
        IFS= read -r answer </dev/tty || die 'Could not read confirmation.'
        case "$answer" in y|Y|yes|YES) ;; *) say 'Installation cancelled.'; return;; esac
    fi
    work_dir=$(mktemp -d "${TMPDIR:-/tmp}/zflow-install.XXXXXXXX")
    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    if [[ "$platform" == Darwin ]]; then check_macos; fi
    if [[ -z "$source_option" ]]; then
        say "Downloading zflow ($ref)…"
        fetch "https://codeload.github.com/demfabris/zflow/tar.gz/$ref" "$work_dir/source.tar.gz" \
            || die 'Could not download the source. Check the ref and repository access; for a private checkout, use --source PATH.'
        mkdir "$work_dir/source"
        tar -xzf "$work_dir/source.tar.gz" --strip-components=1 -C "$work_dir/source"
        source_dir="$work_dir/source"
    fi
    [[ -f "$source_dir/Cargo.toml" && -f "$source_dir/scripts/install.sh" && -f "$source_dir/scripts/build-macos-app.sh" ]] \
        || die 'The selected source does not contain the zflow installers.'
    if [[ "$platform" == Linux ]]; then
        require systemctl
        [[ -n "$(systemctl show --property=Version --value 2>/dev/null)" ]] || die 'Linux installation requires a running systemd system manager.'
        if [[ "$skip_dependencies" == false ]]; then linux_dependencies; fi
        check_linux
    fi
    ensure_rust
    if [[ "$platform" == Linux ]]; then install_linux; else install_macos; fi
    say 'zflow installation finished.'
}

# Keep the final invocation after all definitions so a truncated curl download cannot start setup.
if [[ -z "${BASH_SOURCE[0]:-}" || "${BASH_SOURCE[0]:-}" == "$0" ]]; then main "$@"; fi
