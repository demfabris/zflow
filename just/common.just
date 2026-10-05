set shell := ["bash", "-euo", "pipefail", "-c"]
set positional-arguments := true

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
