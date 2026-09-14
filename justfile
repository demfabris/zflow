set shell := ["bash", "-euo", "pipefail", "-c"]
set positional-arguments

# List development commands.
default:
    @just --list

# Run the GUI on this host (mac or linux); forward extra arguments to the app.
run platform *args: (_platform platform)
    @if [[ "$1" == linux ]]; then printf '%s\n' 'Ubuntu: run just install-linux after updating code, then enable desktop handoff in the app.'; fi
    shift; cargo run --locked --features gui --bin zflow-gui -- "$@"

# Install or update the Linux service and GUI, then restart the service (requires sudo).
install-linux:
    ./scripts/install.sh --gui

# Run with crossing diagnostics and save terminal output under target/logs.
debug platform *args: (_platform platform)
    #!/usr/bin/env bash
    set -euo pipefail
    umask 077
    platform="$1"
    shift
    mkdir -p target/logs
    logfile="target/logs/zflow-$platform-$(date -u +%Y%m%dT%H%M%SZ)-$$.log"
    printf 'Saving diagnostics to %s/%s\n' "$PWD" "$logfile"
    {
        printf 'zflow diagnostics: platform=%s revision=%s\n' "$platform" "$(git describe --always --dirty)"
        cargo run --locked --features gui --bin zflow-gui -- --debug "$@"
    } 2>&1 | tee "$logfile"

# Enable Linux daemon diagnostics until reboot; requires sudo and restarts the service.
debug-daemon: (_platform "linux")
    #!/usr/bin/env bash
    set -euo pipefail
    sudo install -d -m 0755 /run/systemd/system/zflowd.service.d
    printf '[Service]\nEnvironment="RUST_LOG=warn,zflow=debug"\n' | sudo tee /run/systemd/system/zflowd.service.d/debug.conf >/dev/null
    sudo systemctl daemon-reload
    sudo systemctl restart zflowd.service
    printf 'Daemon debug logging enabled until reboot. Read with: journalctl -u zflowd -f -o short-iso-precise\n'

# Build the GUI on this host (mac or linux); append --release for a release build.
build platform *args: (_platform platform)
    shift; cargo build --locked --features gui --bin zflow-gui "$@"

# Run tests, including the GUI; forward extra arguments to Cargo.
test *args:
    cargo test --locked --all-targets --all-features "$@"

# Exercise the shipped GNOME extension with a simulated compositor (requires Node.js).
test-desktop:
    node tests/gnome_desktop_test.mjs

# Format Rust code.
fmt:
    cargo fmt --all

# Check formatting without changing files.
fmt-check:
    cargo fmt --all --check

# Lint Rust code, including the GUI.
lint:
    cargo clippy --locked --all-targets --all-features -- -D warnings

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
