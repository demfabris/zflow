import 'common.just'

set working-directory := ".."

[default]
[private]
default:
    @just --list debug

# Run native Mac app diagnostics and save output under target/logs.
mac *args:
    @just debug _app mac "$@"

# Run Linux desktop-agent diagnostics and save output under target/logs.
linux *args:
    @just debug _app linux "$@"

# Enable Linux daemon diagnostics until reboot; requires sudo and restarts the service.
daemon: (_platform "linux")
    #!/usr/bin/env bash
    set -euo pipefail
    sudo install -d -m 0755 /run/systemd/system/zflowd.service.d
    printf '[Service]\nEnvironment="RUST_LOG=warn,zflow=debug"\n' | sudo tee /run/systemd/system/zflowd.service.d/debug.conf >/dev/null
    sudo systemctl daemon-reload
    sudo systemctl restart zflowd.service
    printf 'Daemon debug logging enabled until reboot. Read with: journalctl -u zflowd -f -o short-iso-precise\n'

[private]
_app platform *args: (_platform platform)
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
