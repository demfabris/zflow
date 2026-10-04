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
universal=false
dmg=false

die() {
    printf 'build-macos-app: %s\n' "$*" >&2
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --debug) profile=debug ;;
        --universal) universal=true ;;
        --dmg) dmg=true ;;
        --sign)
            [[ $# -ge 2 && -n "$2" ]] || die '--sign requires a signing identity'
            sign_identity="$2"
            explicit_sign=true
            shift
            ;;
        -h|--help)
            printf 'usage: ./scripts/build-macos-app.sh [--debug] [--universal] [--dmg] [--sign IDENTITY]\n'
            printf 'Build target/release/zflow.app (target/debug with --debug).\n'
            printf 'Sign with the first Apple Development identity, else ad-hoc (--sign - forces ad-hoc).\n'
            printf '%s\n' '--universal builds for Apple silicon and Intel; --dmg also packs zflow.dmg beside the app.'
            exit 0
            ;;
        *) die "unknown argument: $1" ;;
    esac
    shift
done

[[ "$(uname -s)" == Darwin ]] || die 'macOS is required'
command -v cargo >/dev/null 2>&1 || die 'cargo is required'
xcrun --find actool >/dev/null 2>&1 || die 'Xcode 26 or later with actool is required to build the app icon'
if [[ "$explicit_sign" == true ]]; then
    command -v codesign >/dev/null 2>&1 || die 'codesign is required for --sign'
elif development_identity="$(security find-identity -v -p codesigning 2>/dev/null |
    awk '/"Apple Development: / { print $2; exit }')" && [[ -n "$development_identity" ]]; then
    # macOS keys Accessibility grants to the signature. An ad-hoc signature changes
    # on every build, so each rebuild would need a fresh grant. Sign by hash
    # because renewed certificates share a name.
    sign_identity="$development_identity"
fi

readonly output_dir="$REPO_ROOT/target/$profile"
lib_dir="$output_dir"
cargo_args=(build --locked --manifest-path "$REPO_ROOT/Cargo.toml"
    --target-dir "$REPO_ROOT/target" --lib)
swift_args=(--package-path "$REPO_ROOT/macos" --configuration "$profile")
if [[ "$profile" == release ]]; then
    cargo_args+=(--release)
fi
if [[ "$universal" == true ]]; then
    cargo_args+=(--target aarch64-apple-darwin --target x86_64-apple-darwin)
    swift_args+=(--arch arm64 --arch x86_64)
    lib_dir="$REPO_ROOT/target/universal-apple-darwin/$profile"
fi
MACOSX_DEPLOYMENT_TARGET=26.0 cargo "${cargo_args[@]}"
if [[ "$universal" == true ]]; then
    # The linker takes each architecture's slice from one fat library.
    mkdir -p -- "$lib_dir"
    lipo -create -output "$lib_dir/libzflow.a" \
        "$REPO_ROOT/target/aarch64-apple-darwin/$profile/libzflow.a" \
        "$REPO_ROOT/target/x86_64-apple-darwin/$profile/libzflow.a"
fi

swift_output="$(ZFLOW_RUST_LIB_DIR="$lib_dir" swift build "${swift_args[@]}" --show-bin-path)"
readonly binary="$swift_output/zflow-app"
# SwiftPM's native build system does not track a library linked through
# unsafeFlags, so a newer libzflow.a may not relink the app. Removing the stale
# binary forces the link.
if [[ "$lib_dir/libzflow.a" -nt "$binary" ]]; then
    rm -f -- "$binary"
fi
ZFLOW_RUST_LIB_DIR="$lib_dir" swift build "${swift_args[@]}"
readonly bundle="$output_dir/zflow.app"
[[ -f "$binary" && -x "$binary" && ! -L "$binary" ]] || die "missing native executable: $binary"
[[ ! -L "$bundle" ]] || die "refusing to replace a symlink: $bundle"
[[ ! -e "$bundle" || -d "$bundle" ]] || die "app output is not a directory: $bundle"

temporary_dir="$(mktemp -d "$output_dir/.zflow-app.XXXXXXXX")"
trap 'rm -rf -- "$temporary_dir"' EXIT
app="$temporary_dir/zflow.app"
install -d -m 0755 "$app/Contents/MacOS" "$app/Contents/Library/LaunchDaemons" "$app/Contents/Resources"
install -m 0755 "$binary" "$app/Contents/MacOS/zflow-app"
install -m 0755 "$swift_output/zflow-awdl-daemon" "$app/Contents/MacOS/zflow-awdl-daemon"
if [[ "$universal" == true ]]; then
    for executable in "$app"/Contents/MacOS/*; do
        for arch in arm64 x86_64; do
            lipo "$executable" -verify_arch "$arch" || die "${executable##*/} has no $arch code"
        done
    done
fi
install -m 0644 "$REPO_ROOT/packaging/macOS/io.zflow.awdl.plist" "$app/Contents/Library/LaunchDaemons/io.zflow.awdl.plist"
install -m 0644 "$REPO_ROOT/packaging/macOS/Info.plist" "$app/Contents/Info.plist"
# Keep the layered light, dark and tinted Icon Composer appearances in the app.
xcrun actool "$REPO_ROOT/assets/zflow.icon" \
    --compile "$app/Contents/Resources" \
    --output-format human-readable-text --notices --warnings \
    --output-partial-info-plist "$temporary_dir/icon-info.plist" \
    --app-icon zflow --target-device mac \
    --minimum-deployment-target 26.0 --platform macosx
/usr/libexec/PlistBuddy -c "Merge '$temporary_dir/icon-info.plist'" "$app/Contents/Info.plist"
version="$(cargo metadata --no-deps --format-version 1 --manifest-path "$REPO_ROOT/Cargo.toml" | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"
if command -v codesign >/dev/null 2>&1; then
    sign_args=("$sign_identity" "$app")
    if [[ "$explicit_sign" == false ]]; then sign_args+=(--no-timestamp); fi
    "$SCRIPT_DIR/sign-macos-app.sh" "${sign_args[@]}"
fi

rm -rf -- "$bundle"
mv -- "$app" "$bundle"
printf 'Built %s\n' "$bundle"
printf 'Open it in Finder, or run: open "%s"\n' "$bundle"
if [[ "$dmg" == true ]]; then
    dmg_args=("$bundle" "$output_dir/zflow.dmg")
    # An ad-hoc signature on the image would prove nothing.
    if [[ "$sign_identity" != - ]]; then dmg_args+=(--sign "$sign_identity"); fi
    "$SCRIPT_DIR/build-macos-dmg.sh" "${dmg_args[@]}"
fi
