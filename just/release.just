import 'common.just'

set working-directory := ".."

[default]
[private]
default:
    @just --list release

# Configure Sparkle update signing in the Mac Keychain and GitHub repository.
setup: (_platform "mac")
    ./scripts/setup-macos-updates.sh

# Check the committed version and GitHub state without pushing or tagging.
check:
    ./scripts/release.sh --dry-run

# Push main, wait for CI, tag the Cargo version, and wait for GitHub publication.
publish *args:
    ./scripts/release.sh "$@"
