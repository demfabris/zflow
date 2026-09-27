#!/usr/bin/env bash
# Pack the GNOME extension for upload to extensions.gnome.org.
set -Eeuo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
output="$root/target/dist"
zip="$output/zflow@demfabris.shell-extension.zip"
# The files desktop-agent --install writes (EXTENSION_FILES in src/app/desktop.rs).
# pack adds metadata.json, extension.js and prefs.js on its own.
files=(client.js extension.js indicator.js metadata.json prefs.js settings.js)
command -v gnome-extensions >/dev/null 2>&1 || { echo 'Packing needs gnome-extensions from GNOME Shell.' >&2; exit 1; }
mkdir -p "$output"
gnome-extensions pack "$root/packaging/gnome-extension" --force --out-dir="$output" \
    --extra-source=indicator.js --extra-source=client.js --extra-source=settings.js
# pack leaves out a missing file without an error.
packed=$(unzip -Z1 "$zip" | sort)
if [[ "$packed" != "$(printf '%s\n' "${files[@]}")" ]]; then
    printf 'Unexpected files in %s:\n%s\n' "$zip" "$packed" >&2
    exit 1
fi
printf 'Created %s\n' "$zip"
