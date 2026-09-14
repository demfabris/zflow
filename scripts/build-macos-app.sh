#!/usr/bin/env bash
# Build a Finder-launchable app without installing or starting it.
set -Eeuo pipefail
IFS=$'\n\t'

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd -P)"
readonly SCRIPT_DIR REPO_ROOT
profile=release
sign_identity=-
explicit_sign=false

die() {
    printf 'build-macos-app: %s\n' "$*" >&2
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --debug) profile=debug ;;
        --sign)
            [[ $# -ge 2 && -n "$2" ]] || die '--sign requires a signing identity'
            sign_identity="$2"
            explicit_sign=true
            shift
            ;;
        -h|--help)
            printf 'usage: ./scripts/build-macos-app.sh [--debug] [--sign IDENTITY]\n'
            printf 'Build target/release/zflow.app (target/debug with --debug).\n'
            printf 'Use an ad-hoc signature unless --sign selects a code-signing identity.\n'
            exit 0
            ;;
        *) die "unknown argument: $1" ;;
    esac
    shift
done

[[ "$(uname -s)" == Darwin ]] || die 'macOS is required'
command -v cargo >/dev/null 2>&1 || die 'cargo is required'
if [[ "$explicit_sign" == true ]]; then
    command -v codesign >/dev/null 2>&1 || die 'codesign is required for --sign'
fi

cargo_args=(build --locked --manifest-path "$REPO_ROOT/Cargo.toml"
    --target-dir "$REPO_ROOT/target" --features gui --bin zflow-gui)
if [[ "$profile" == release ]]; then
    cargo_args+=(--release)
fi
cargo "${cargo_args[@]}"

readonly output_dir="$REPO_ROOT/target/$profile"
readonly binary="$output_dir/zflow-gui"
readonly bundle="$output_dir/zflow.app"
[[ -f "$binary" && -x "$binary" && ! -L "$binary" ]] || die "missing native executable: $binary"
[[ ! -L "$bundle" ]] || die "refusing to replace a symlink: $bundle"
[[ ! -e "$bundle" || -d "$bundle" ]] || die "app output is not a directory: $bundle"

temporary_dir="$(mktemp -d "$output_dir/.zflow-app.XXXXXXXX")"
trap 'rm -rf -- "$temporary_dir"' EXIT
app="$temporary_dir/zflow.app"
install -d -m 0755 "$app/Contents/MacOS"
install -m 0755 "$binary" "$app/Contents/MacOS/zflow-gui"
install -m 0644 "$REPO_ROOT/packaging/macOS/Info.plist" "$app/Contents/Info.plist"
version="$("$binary" --version)"
version="${version##* }"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"
if command -v codesign >/dev/null 2>&1; then
    codesign --force --sign "$sign_identity" "$app"
    codesign --verify --strict "$app"
fi

rm -rf -- "$bundle"
mv -- "$app" "$bundle"
printf 'Built %s\n' "$bundle"
printf 'Open it in Finder, or run: open "%s"\n' "$bundle"
