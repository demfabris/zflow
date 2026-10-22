import 'common.just'

set working-directory := ".."

[default]
[private]
default:
    @just --list test

# Run Rust tests.
rust *args:
    cargo test --locked --all-targets "$@"

# Run the app's tests: its pages, arrangement math, tray menu and platform rules.
app:
    cargo test --locked -p zflow-app -p zz-gpui-tray

# Exercise the shipped GNOME extension with a simulated compositor (requires Node.js).
desktop:
    node tests/gnome_desktop_test.mjs
    node tests/gnome_panel_test.mjs
    node tests/gnome_loader_test.mjs

# Exercise the desktop agent's D-Bus service on a private bus (Linux).
bus: (_platform "linux")
    dbus-run-session -- cargo test --locked --lib app:: -- --ignored --skip native_desktop_geometry

# Exercise installation and recovery without changing the host (requires Python 3).
install:
    python3 tests/install_test.py

# Exercise a built Debian package in a disposable Ubuntu container.
package:
    ./tests/deb_lifecycle.sh

# Exercise release tagging and CI gates against a disposable local remote.
release:
    python3 tests/release_test.py
    python3 tests/macos_updates_test.py
