#!/usr/bin/env bash
# Sign a built zflow.app and its AWDL helper with the hardened runtime. The
# helper and the app each require the other's identifier from the same team.
set -Eeuo pipefail
identity=${1:?usage: sign-macos-app.sh IDENTITY APP}
app=${2:?usage: sign-macos-app.sh IDENTITY APP}
codesign --force --options runtime --identifier io.zflow.awdl-daemon --sign "$identity" "$app/Contents/MacOS/zflow-awdl-daemon"
codesign --force --options runtime --identifier io.zflow.zflow --sign "$identity" "$app"
codesign --verify --strict --deep "$app"
