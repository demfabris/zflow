import 'just/common.just'

# Capture app or daemon diagnostics.
mod debug 'just/debug.just'
# Apply or check Rust formatting.
mod fmt 'just/fmt.just'
# Install a native service.
mod install 'just/install.just'
# Package distributable artifacts.
mod package 'just/package.just'
# Check or publish a GitHub release.
mod release 'just/release.just'
# Run a test suite.
mod test 'just/test.just'

# List development commands.
[default]
[private]
default:
    @just --list

# Build and open the native app.
run platform *args: (_platform platform)
    #!/usr/bin/env bash
    set -euo pipefail
    platform="$1"
    shift
    if [[ "$platform" == mac ]]; then
        ./scripts/build-macos-app.sh --debug
        open target/debug/zflow.app --args "$@"
    else
        cargo run --locked --bin zflow -- settings "$@"
    fi

# Package the Mac app or build the Linux service and native app launcher.
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

# Lint Rust code.
lint:
    cargo clippy --locked --all-targets -- -D warnings

# Check formatting, lint, and run tests.
check: fmt::check lint test::rust test::desktop test::install test::release
