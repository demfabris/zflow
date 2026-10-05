#!/usr/bin/env bash
# Keep the Sparkle signing key locally and configure release credentials.
set +x
set -Eeuo pipefail
IFS=$'\n\t'

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
repository=demfabris/zflow
account=io.zflow.zflow
die() { printf 'setup-updates: %s\n' "$*" >&2; exit 1; }
if [[ "${1:-}" == -h || "${1:-}" == --help ]]; then
    printf 'usage: ./scripts/setup-updates.sh\n'
    printf 'Create or reuse a local Sparkle key and configure %s release credentials.\n' "$repository"
    printf 'Maintainers need gh and either macOS Swift/Keychain or Linux Python 3 with cryptography.\n'
    exit 0
fi
[[ $# == 0 ]] || die 'no arguments expected'
platform=$(uname -s)
[[ "$platform" == Darwin || "$platform" == Linux ]] || die 'run setup on macOS or Linux'
command -v gh >/dev/null 2>&1 || die 'GitHub CLI (gh) is required'
gh auth status >/dev/null
configured=$(gh variable list --repo "$repository" --json name,value \
    --jq '.[] | select(.name == "SPARKLE_PUBLIC_ED_KEY") | .value')
umask 077
if [[ "$platform" == Darwin ]]; then
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
    work=$(mktemp -d "${TMPDIR:-/tmp}/zflow-update-key.XXXXXXXX")
    trap 'rm -rf -- "$work"' EXIT
    private_file="$work/private-key"
    "$keytool" --account "$account" -x "$private_file"
else
    command -v python3 >/dev/null 2>&1 || die 'maintainer setup needs Python 3 with the cryptography package'
    key_directory="${XDG_CONFIG_HOME:-$HOME/.config}/zflow-release"
    private_file="$key_directory/sparkle-private-key"
    public=$(python3 - "$key_directory" "$root" "$configured" <<'PY'
import base64
import os
from pathlib import Path
import stat
import sys
import tempfile

def fail(message):
    sys.exit(f"setup-updates: {message}")

try:
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
except ImportError:
    fail("maintainer setup needs Python's cryptography package; install it for python3 and retry")

directory = Path(sys.argv[1])
checkout = Path(sys.argv[2]).resolve()
configured = sys.argv[3]
if not directory.is_absolute() or directory.is_symlink():
    fail("the update key directory must be an absolute path, not a symbolic link")
if checkout == directory.resolve() or checkout in directory.resolve().parents:
    fail("XDG_CONFIG_HOME must keep release signing keys outside the checkout")
private_file = directory / "sparkle-private-key"

try:
    if configured and not private_file.exists():
        fail(f"restore the existing release key to {private_file} before configuring this computer")
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    if directory.stat().st_uid != os.getuid():
        fail("the update key directory must belong to the current user")
    directory.chmod(0o700)
    if not private_file.exists() and not private_file.is_symlink():
        key = Ed25519PrivateKey.generate()
        # Sparkle 2.10 exports the base64-encoded, 32-byte Ed25519 seed.
        seed = key.private_bytes(serialization.Encoding.Raw, serialization.PrivateFormat.Raw,
                                 serialization.NoEncryption())
        descriptor, temporary = tempfile.mkstemp(prefix=".sparkle-key-", dir=directory)
        try:
            with os.fdopen(descriptor, "wb") as stream:
                stream.write(base64.b64encode(seed))
                stream.flush()
                os.fsync(stream.fileno())
            try:
                os.link(temporary, private_file)
            except FileExistsError:
                pass  # Another setup process published its complete key first.
        finally:
            os.unlink(temporary)
    metadata = private_file.lstat()
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid():
        fail("the signing key must be a regular file owned by the current user")
    private_file.chmod(0o600)
    seed = base64.b64decode(private_file.read_bytes().strip(), validate=True)
    key = Ed25519PrivateKey.from_private_bytes(seed)
    public = base64.b64encode(key.public_key().public_bytes(
        serialization.Encoding.Raw, serialization.PublicFormat.Raw)).decode()
    if configured and public != configured:
        fail("the local key differs from the release key; key rotation requires a separate release")
except ValueError:
    fail("the local signing key must contain a base64-encoded 32-byte Ed25519 seed")
except OSError as error:
    fail(f"cannot access the local signing key: {error.strerror}")
print(public)
PY
    )
fi
gh secret set SPARKLE_PRIVATE_ED_KEY --repo "$repository" < "$private_file"
gh variable set SPARKLE_PUBLIC_ED_KEY --repo "$repository" --body "$public"
printf 'Configured signed macOS updates for %s.\n' "$repository"
if [[ "$platform" == Darwin ]]; then
    printf 'Back up the %s signing key in your login Keychain securely.\n' "$account"
else
    printf 'Back up the private signing key at %s securely.\n' "$private_file"
fi
