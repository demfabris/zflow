#!/usr/bin/env bash
# Sign zflow, Sparkle, and the AWDL helper from the inside out. The helper and
# the app each require the other's identifier from the same signing team.
set -Eeuo pipefail
identity=${1:?usage: sign-macos-app.sh IDENTITY APP [--no-timestamp]}
app=${2:?usage: sign-macos-app.sh IDENTITY APP [--no-timestamp]}
# Notarization requires a secure timestamp. Local development builds skip it so
# they work offline; an ad-hoc signature cannot carry one.
timestamp=--timestamp
if [[ "${3:-}" == --no-timestamp || "$identity" == - ]]; then timestamp=--timestamp=none; fi
options=(--options runtime)
# Ad-hoc builds have no signing team for library validation. Release builds use
# Developer ID and the hardened runtime for both the app and its nested code.
if [[ "$identity" == - ]]; then options=(--options 0); fi
framework="$app/Contents/Frameworks/Sparkle.framework"
[[ -d "$framework" ]] || { printf 'Missing Sparkle framework: %s\n' "$framework" >&2; exit 1; }
codesign --force "${options[@]}" "$timestamp" --sign "$identity" "$framework/Versions/B/XPCServices/Installer.xpc"
codesign --force "${options[@]}" "$timestamp" --preserve-metadata=entitlements --sign "$identity" "$framework/Versions/B/XPCServices/Downloader.xpc"
codesign --force "${options[@]}" "$timestamp" --sign "$identity" "$framework/Versions/B/Autoupdate"
codesign --force "${options[@]}" "$timestamp" --sign "$identity" "$framework/Versions/B/Updater.app"
codesign --force "${options[@]}" "$timestamp" --sign "$identity" "$framework"
codesign --force "${options[@]}" "$timestamp" --identifier io.zflow.awdl-daemon --sign "$identity" "$app/Contents/MacOS/zflow-awdl-daemon"
codesign --force "${options[@]}" "$timestamp" --identifier io.zflow.zflow --sign "$identity" "$app"
codesign --verify --strict --deep "$app"
