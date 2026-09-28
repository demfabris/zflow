#!/usr/bin/env bash
# Builds build/zflow-inject-probe.app from probe.swift. It never runs it.
# A separate app so the owner grants Accessibility to the probe alone.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
app="$here/build/zflow-inject-probe.app"
# Ad-hoc by default. Set SIGN_IDENTITY to an "Apple Development" identity to
# keep the Accessibility grant across rebuilds.
identity="${SIGN_IDENTITY:--}"

[[ "$(uname -s)" == Darwin ]] || { echo 'macOS only' >&2; exit 1; }
rm -rf -- "$app"
mkdir -p "$app/Contents/MacOS"

xcrun swiftc -O -swift-version 5 -parse-as-library -warnings-as-errors \
    -o "$app/Contents/MacOS/zflow-inject-probe" "$here/probe.swift"

cat > "$app/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>dev.zflow.spike.inject-probe</string>
    <key>CFBundleName</key>
    <string>zflow inject probe</string>
    <key>CFBundleExecutable</key>
    <string>zflow-inject-probe</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSUIElement</key>
    <false/>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF
plutil -lint "$app/Contents/Info.plist" >/dev/null

codesign -s "$identity" --force "$app"
codesign --verify --strict "$app"

echo "built $app"
if [[ "$identity" == - ]]; then
    echo 'ad-hoc signed: remove and re-add the app under Privacy & Security > Accessibility before the next run'
fi
