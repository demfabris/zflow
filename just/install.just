import 'common.just'

set working-directory := ".."

[default]
[private]
default:
    @just --list install

# Install/update the Linux service (requires sudo).
linux: (_platform "linux")
    ./scripts/install.sh
