#!/usr/bin/env bash
# Sign the final notarized DMG and describe it in Sparkle's appcast.
set +x
set -Eeuo pipefail
IFS=$'\n\t'

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
usage='usage: ./scripts/macos-update-feed.sh FINAL.dmg VERSION [OUTPUT.xml]'
if [[ "${1:-}" == -h || "${1:-}" == --help ]]; then
    printf '%s\n' "$usage"
    printf '%s\n' 'Requires SPARKLE_PRIVATE_ED_KEY; run after signing, notarizing and stapling the DMG.'
    exit 0
fi
[[ $# -ge 2 && $# -le 3 ]] || { printf '%s\n' "$usage" >&2; exit 1; }
dmg=$1
version=$2
output=${3:-$root/target/dist/appcast.xml}
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || { printf 'Invalid release version\n' >&2; exit 1; }
[[ -f "$dmg" && ! -L "$dmg" && "${dmg##*/}" == "zflow-v$version-macos.dmg" ]] || {
    printf 'Expected final release disk image zflow-v%s-macos.dmg\n' "$version" >&2; exit 1;
}
: "${SPARKLE_PRIVATE_ED_KEY:?Missing SPARKLE_PRIVATE_ED_KEY}"
generator="$root/macos/.build/artifacts/sparkle/Sparkle/bin/generate_appcast"
[[ -x "$generator" ]] || { printf 'Build the macOS app first to fetch Sparkle tools\n' >&2; exit 1; }
codesign --verify --strict "$dmg"
xcrun stapler validate "$dmg"
mkdir -p -- "$(dirname -- "$output")"
work=$(mktemp -d "$(dirname -- "$output")/.appcast.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
mkdir "$work/archives"
cp "$dmg" "$work/archives/"
prefix="https://github.com/demfabris/zflow/releases/download/v$version/"
# Sparkle reads the private key from stdin, never from command-line arguments.
printf '%s\n' "$SPARKLE_PRIVATE_ED_KEY" | "$generator" --ed-key-file - \
    --download-url-prefix "$prefix" --maximum-deltas 0 \
    --link "https://github.com/demfabris/zflow/releases/tag/v$version" \
    -o "$work/appcast.xml" "$work/archives"
python3 - "$work/appcast.xml" "$dmg" "$version" "$prefix" <<'PY'
import base64
from pathlib import Path
import sys
import xml.etree.ElementTree as ET

feed, disk_image, version, prefix = sys.argv[1:]
ns = {"sparkle": "http://www.andymatuschak.org/xml-namespaces/sparkle"}
items = ET.parse(feed).findall("./channel/item")
if len(items) != 1 or items[0].findtext("sparkle:version", namespaces=ns) != version:
    sys.exit("Appcast must contain exactly the version being released")
enclosure = items[0].find("enclosure")
image = Path(disk_image)
if enclosure is None or enclosure.get("url") != prefix + image.name:
    sys.exit("Appcast does not reference the release disk image")
if enclosure.get("length") != str(image.stat().st_size):
    sys.exit("Appcast disk image size does not match")
signature = enclosure.get("{" + ns["sparkle"] + "}edSignature", "")
try:
    valid = len(base64.b64decode(signature, validate=True)) == 64
except ValueError:
    valid = False
if not valid:
    sys.exit("Appcast has no valid EdDSA signature; check that the release signing keys match")
PY
mv -f -- "$work/appcast.xml" "$output"
printf 'Created %s\n' "$output"
