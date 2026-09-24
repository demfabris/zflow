#!/usr/bin/env bash
# Sign a built zflow.app and its AWDL helpers with the hardened runtime.
set -Eeuo pipefail
identity=${1:?usage: sign-macos-app.sh IDENTITY APP}
app=${2:?usage: sign-macos-app.sh IDENTITY APP}
codesign --force --options runtime --identifier io.zflow.awdl-client --sign "$identity" "$app/Contents/MacOS/zflow-awdl-client"
codesign --force --options runtime --identifier io.zflow.awdl-daemon --sign "$identity" "$app/Contents/MacOS/zflow-awdl-daemon"
codesign --force --options runtime --sign "$identity" "$app"
codesign --verify --strict --deep "$app"
