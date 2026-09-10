set shell := ["bash", "-euo", "pipefail", "-c"]
set positional-arguments

# List development commands.
default:
    @just --list

# Run the GUI on this host (mac or linux); forward extra arguments to the app.
run platform *args: (_platform platform)
    shift; cargo run --locked --features gui --bin zflow-gui -- "$@"

# Build the GUI on this host (mac or linux); append --release for a release build.
build platform *args: (_platform platform)
    shift; cargo build --locked --features gui --bin zflow-gui "$@"

# Run tests, including the GUI; forward extra arguments to Cargo.
test *args:
    cargo test --locked --all-targets --all-features "$@"

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
check: fmt-check lint test

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
