#!/usr/bin/env bash
# Print a directory with Sparkle's framework and tools, downloading and
# checking them the first time.
set -Eeuo pipefail
IFS=$'\n\t'

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
readonly version=2.10.0
# The checksum Sparkle's Package.swift pins for this release.
readonly checksum=17e28312b8e18ab7cdbbe09a6fb28cc55a5479ec6c371dbc07cdecd2a14fd959
readonly url="https://github.com/sparkle-project/Sparkle/releases/download/$version/Sparkle-for-Swift-Package-Manager.zip"
die() { printf 'fetch-sparkle: %s\n' "$*" >&2; exit 1; }
if [[ "${1:-}" == -h || "${1:-}" == --help ]]; then
    printf 'usage: ./scripts/fetch-sparkle.sh\n'
    printf 'Print a folder holding Sparkle %s, fetched into target/sparkle the first time.\n' "$version"
    exit 0
fi
[[ $# == 0 ]] || die 'no arguments expected'
dir="$root/target/sparkle/$version"
if [[ ! -d "$dir/bin" ]]; then
    command -v curl >/dev/null 2>&1 || die 'curl is required'
    command -v unzip >/dev/null 2>&1 || die 'unzip is required'
    mkdir -p -- "$root/target/sparkle"
    work=$(mktemp -d "$root/target/sparkle/.fetch.XXXXXXXX")
    trap 'rm -rf -- "$work"' EXIT
    curl --proto '=https' --tlsv1.2 -fsSL -o "$work/sparkle.zip" "$url"
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$work/sparkle.zip")
    else
        actual=$(shasum -a 256 "$work/sparkle.zip")
    fi
    [[ "${actual%% *}" == "$checksum" ]] || die "Sparkle $version does not match its published checksum"
    mkdir "$work/sparkle"
    unzip -q "$work/sparkle.zip" -d "$work/sparkle"
    rm -rf -- "$dir"
    mv -- "$work/sparkle" "$dir"
fi
printf '%s\n' "$dir"
