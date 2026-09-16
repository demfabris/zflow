#!/usr/bin/env bash
# Notarize the signed release app and attach Apple's ticket before packaging.
set -Eeuo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
profile=${1:?usage: notarize-macos-app.sh KEYCHAIN_PROFILE [KEYCHAIN_PATH]}
auth=(--keychain-profile "$profile")
if [[ $# -ge 2 ]]; then auth+=(--keychain "$2"); fi
app="$root/target/release/zflow.app"
output="$root/target/notarization"
mkdir -p "$output"
codesign --verify --strict --deep "$app"
ditto -c -k --keepParent "$app" "$output/zflow.zip"
xcrun notarytool submit "$output/zflow.zip" "${auth[@]}" --output-format json > "$output/submission.json"
submission=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$output/submission.json")
printf 'Notarization submission: %s\n' "$submission"
xcrun notarytool wait "$submission" "${auth[@]}" --timeout 20m --output-format json > "$output/status.json"
xcrun notarytool log "$submission" "${auth[@]}" "$output/notary-log.json"
python3 - "$output/status.json" "$output/notary-log.json" <<'PY'
import json
import sys

status = json.load(open(sys.argv[1]))["status"]
log = json.load(open(sys.argv[2]))
if status != "Accepted":
    print(json.dumps(log, indent=2), file=sys.stderr)
    sys.exit(f"Notarization failed: {status}")
if log.get("issues"):
    print(json.dumps(log["issues"], indent=2))
print("Apple accepted the app.")
PY
xcrun stapler staple "$app"
xcrun stapler validate "$app"
codesign --verify --strict --deep "$app"
spctl --assess --type execute --verbose=2 "$app"
