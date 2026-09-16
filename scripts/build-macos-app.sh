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
    --target-dir "$REPO_ROOT/target" --lib)
if [[ "$profile" == release ]]; then
    cargo_args+=(--release)
fi
MACOSX_DEPLOYMENT_TARGET=26.0 cargo "${cargo_args[@]}"

readonly output_dir="$REPO_ROOT/target/$profile"
swift_profile="$profile"
ZFLOW_RUST_LIB_DIR="$output_dir" swift build --package-path "$REPO_ROOT/macos" --configuration "$swift_profile"
swift_output="$(ZFLOW_RUST_LIB_DIR="$output_dir" swift build --package-path "$REPO_ROOT/macos" --configuration "$swift_profile" --show-bin-path)"
readonly binary="$swift_output/zflow-app"
readonly bundle="$output_dir/zflow.app"
[[ -f "$binary" && -x "$binary" && ! -L "$binary" ]] || die "missing native executable: $binary"
[[ ! -L "$bundle" ]] || die "refusing to replace a symlink: $bundle"
[[ ! -e "$bundle" || -d "$bundle" ]] || die "app output is not a directory: $bundle"

temporary_dir="$(mktemp -d "$output_dir/.zflow-app.XXXXXXXX")"
trap 'rm -rf -- "$temporary_dir"' EXIT
app="$temporary_dir/zflow.app"
install -d -m 0755 "$app/Contents/MacOS" "$app/Contents/Library/LaunchDaemons"
install -m 0755 "$binary" "$app/Contents/MacOS/zflow-app"
install -m 0755 "$swift_output/zflow-awdl-client" "$app/Contents/MacOS/zflow-awdl-client"
install -m 0755 "$swift_output/zflow-awdl-daemon" "$app/Contents/MacOS/zflow-awdl-daemon"
install -m 0644 "$REPO_ROOT/packaging/macOS/io.zflow.awdl.plist" "$app/Contents/Library/LaunchDaemons/io.zflow.awdl.plist"
install -m 0644 "$REPO_ROOT/packaging/macOS/Info.plist" "$app/Contents/Info.plist"
version="$(cargo metadata --no-deps --format-version 1 --manifest-path "$REPO_ROOT/Cargo.toml" | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"
if command -v codesign >/dev/null 2>&1; then
    codesign --force --options runtime --identifier io.zflow.awdl-client --sign "$sign_identity" "$app/Contents/MacOS/zflow-awdl-client"
    codesign --force --options runtime --identifier io.zflow.awdl-daemon --sign "$sign_identity" "$app/Contents/MacOS/zflow-awdl-daemon"
    codesign --force --options runtime --sign "$sign_identity" "$app"
    codesign --verify --strict --deep "$app"
fi

rm -rf -- "$bundle"
mv -- "$app" "$bundle"
printf 'Built %s\n' "$bundle"
printf 'Open it in Finder, or run: open "%s"\n' "$bundle"
