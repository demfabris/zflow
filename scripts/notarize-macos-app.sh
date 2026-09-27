#!/usr/bin/env bash
# Notarize the signed release app, or the signed disk image built from it, and
# attach Apple's ticket before packaging. An empty KEYCHAIN_PATH uses the
# default keychains.
set -Eeuo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
profile=${1:?usage: notarize-macos-app.sh KEYCHAIN_PROFILE [KEYCHAIN_PATH [APP_OR_DMG]]}
auth=(--keychain-profile "$profile")
if [[ -n "${2:-}" ]]; then auth+=(--keychain "$2"); fi
item=${3:-$root/target/release/zflow.app}
output="$root/target/notarization"
if [[ "$item" == *.dmg ]]; then
    # Apple takes a disk image as is. Keep its results apart from the app's.
    output="$output/dmg"
    mkdir -p "$output"
    codesign --verify --strict "$item"
    upload=$item
else
    mkdir -p "$output"
    codesign --verify --strict --deep "$item"
    upload="$output/zflow.zip"
    ditto -c -k --keepParent "$item" "$upload"
fi
xcrun notarytool submit "$upload" "${auth[@]}" --output-format json > "$output/submission.json"
submission=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$output/submission.json")
printf 'Notarization submission: %s\n' "$submission"
xcrun notarytool wait "$submission" "${auth[@]}" --timeout 20m --output-format json > "$output/status.json"
xcrun notarytool log "$submission" "${auth[@]}" "$output/notary-log.json"
python3 - "$output/status.json" "$output/notary-log.json" "${item##*/}" <<'PY'
import json
import sys

status = json.load(open(sys.argv[1]))["status"]
log = json.load(open(sys.argv[2]))
if status != "Accepted":
    print(json.dumps(log, indent=2), file=sys.stderr)
    sys.exit(f"Notarization failed: {status}")
if log.get("issues"):
    print(json.dumps(log["issues"], indent=2))
print(f"Apple accepted {sys.argv[3]}.")
PY
xcrun stapler staple "$item"
xcrun stapler validate "$item"
if [[ "$item" == *.dmg ]]; then
    codesign --verify --strict "$item"
    spctl --assess --type open --context context:primary-signature --verbose=2 "$item"
else
    codesign --verify --strict --deep "$item"
    spctl --assess --type execute --verbose=2 "$item"
fi
