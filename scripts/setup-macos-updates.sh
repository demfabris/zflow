#!/usr/bin/env bash
# Keep the Sparkle signing key in Keychain and configure release credentials.
set +x
set -Eeuo pipefail
IFS=$'\n\t'

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
repository=demfabris/zflow
account=io.zflow.zflow
die() { printf 'setup-macos-updates: %s\n' "$*" >&2; exit 1; }
if [[ "${1:-}" == -h || "${1:-}" == --help ]]; then
    printf 'usage: ./scripts/setup-macos-updates.sh\n'
    printf 'Create or reuse the zflow Sparkle key in macOS Keychain and configure %s release credentials.\n' "$repository"
    exit 0
fi
[[ $# == 0 ]] || die 'no arguments expected'
[[ "$(uname -s)" == Darwin ]] || die 'macOS is required for Sparkle Keychain tools'
command -v gh >/dev/null 2>&1 || die 'GitHub CLI (gh) is required'
gh auth status >/dev/null
configured=$(gh variable list --repo "$repository" --json name,value \
    --jq '.[] | select(.name == "SPARKLE_PUBLIC_ED_KEY") | .value')
swift package --package-path "$root/macos" resolve
keytool="$root/macos/.build/artifacts/sparkle/Sparkle/bin/generate_keys"
[[ -x "$keytool" ]] || die 'SwiftPM did not download Sparkle key tools'
if [[ -n "$configured" ]]; then
    public=$("$keytool" --account "$account" -p) || die 'Import the existing release key into Keychain before configuring this Mac'
    [[ "$public" == "$configured" ]] || die 'The Keychain key differs from the release key; key rotation requires a separate release'
else
    "$keytool" --account "$account" >/dev/null
    public=$("$keytool" --account "$account" -p)
fi
umask 077
work=$(mktemp -d "${TMPDIR:-/tmp}/zflow-update-key.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
"$keytool" --account "$account" -x "$work/private-key"
gh secret set SPARKLE_PRIVATE_ED_KEY --repo "$repository" < "$work/private-key"
gh variable set SPARKLE_PUBLIC_ED_KEY --repo "$repository" --body "$public"
printf 'Configured signed macOS updates for %s. The private key remains in your login Keychain.\n' "$repository"
