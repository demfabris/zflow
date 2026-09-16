set shell := ["bash", "-euo", "pipefail", "-c"]
set positional-arguments

# List development commands.
default:
    @just --list

# Build and open the Mac app, or run the Linux desktop agent.
run platform *args: (_platform platform)
    #!/usr/bin/env bash
    set -euo pipefail
    platform="$1"
    shift
    if [[ "$platform" == mac ]]; then
        ./scripts/build-macos-app.sh --debug
        open target/debug/zflow.app --args "$@"
    else
        cargo run --locked --bin zflow -- desktop-agent "$@"
    fi

# Install/update the Linux service (requires sudo).
install-linux:
    ./scripts/install.sh

# Run with diagnostics and save terminal output under target/logs.
debug platform *args: (_platform platform)
    #!/usr/bin/env bash
    set -euo pipefail
    umask 077
    platform="$1"
    shift
    mkdir -p target/logs
    logfile="target/logs/zflow-$platform-$(date -u +%Y%m%dT%H%M%SZ)-$$.log"
    printf 'Saving diagnostics to %s/%s\n' "$PWD" "$logfile"
    if [[ "$platform" == mac ]]; then
        ./scripts/build-macos-app.sh --debug
        RUST_LOG=warn,zflow=debug target/debug/zflow.app/Contents/MacOS/zflow-app "$@" 2>&1 | tee "$logfile"
    else
        RUST_LOG=warn,zflow=debug cargo run --locked --bin zflow -- desktop-agent "$@" 2>&1 | tee "$logfile"
    fi

# Enable Linux daemon diagnostics until reboot; requires sudo and restarts the service.
debug-daemon: (_platform "linux")
    #!/usr/bin/env bash
    set -euo pipefail
    sudo install -d -m 0755 /run/systemd/system/zflowd.service.d
    printf '[Service]\nEnvironment="RUST_LOG=warn,zflow=debug"\n' | sudo tee /run/systemd/system/zflowd.service.d/debug.conf >/dev/null
    sudo systemctl daemon-reload
    sudo systemctl restart zflowd.service
    printf 'Daemon debug logging enabled until reboot. Read with: journalctl -u zflowd -f -o short-iso-precise\n'

# Package the Mac app or build the Linux service and agent.
build platform *args: (_platform platform)
    #!/usr/bin/env bash
    set -euo pipefail
    platform="$1"
    shift
    if [[ "$platform" == mac ]]; then
        ./scripts/build-macos-app.sh "$@"
    else
        cargo build --locked --bin zflow --bin zflowd "$@"
    fi

# Run Rust tests.
test *args:
    cargo test --locked --all-targets "$@"

# Run Swift/Rust bridge tests with an isolated temporary configuration.
test-native: (_platform "mac")
    MACOSX_DEPLOYMENT_TARGET=26.0 cargo build --locked --lib
    swift test --package-path macos

# Exercise the shipped GNOME extension with a simulated compositor (requires Node.js).
test-desktop:
    node tests/gnome_desktop_test.mjs

# Format Rust code.
fmt:
    cargo fmt --all

# Check formatting without changing files.
fmt-check:
    cargo fmt --all --check

# Lint Rust code.
lint:
    cargo clippy --locked --all-targets -- -D warnings

# Check formatting, lint, and run tests.
check: fmt-check lint test test-desktop

[private]
_platform platform:
    #!/usr/bin/env bash
    set -euo pipefail
    host=$(uname -s)
    case "$1:$host" in
        mac:Darwin|linux:Linux) ;;
        mac:*|linux:*)
            printf 'Platform %s does not match this host (%s). Run on the matching machine; these commands do not cross-compile or SSH.\n' "$1" "$host" >&2
            exit 1
            ;;
        *)
            printf 'Unknown platform: %s. Use mac or linux.\n' "$1" >&2
            exit 1
            ;;
    esac
