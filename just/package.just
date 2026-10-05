import 'common.just'

set working-directory := ".."

[default]
[private]
default:
    @just --list package

# Pack the Mac app into target/release/zflow.dmg, signed like the app but not notarized.
dmg *args: (_platform "mac")
    ./scripts/build-macos-app.sh --dmg "$@"

# Pack the GNOME extension for extensions.gnome.org under target/dist.
extension: (_platform "linux")
    ./scripts/pack-extension.sh
