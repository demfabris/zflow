import 'common.just'

set working-directory := ".."

[default]
[private]
default:
    @just --list test

# Run Rust tests.
rust *args:
    cargo test --locked --all-targets "$@"

# Run Swift/Rust bridge tests with an isolated temporary configuration (macOS).
native: (_platform "mac")
    MACOSX_DEPLOYMENT_TARGET=26.0 cargo build --locked --lib
    swift test --package-path macos

# Exercise the shipped GNOME extension with a simulated compositor (requires Node.js).
desktop:
    node tests/gnome_desktop_test.mjs
    node tests/gnome_panel_test.mjs

# Exercise native GTK controls through D-Bus on the current display (Linux).
gtk: (_platform "linux")
    dbus-run-session -- cargo test --locked --lib app:: -- --ignored --skip native_desktop_geometry
    GTK_A11Y=none GIO_USE_VFS=local dbus-run-session --config-file=tests/gtk-session.conf -- gjs -m tests/gnome_settings_test.js
    GSETTINGS_BACKEND=memory GTK_A11Y=none GIO_USE_VFS=local dbus-run-session --config-file=tests/gtk-session.conf -- gjs -m tests/gnome_setup_test.js
    GTK_A11Y=none GIO_USE_VFS=local dbus-run-session --config-file=tests/gtk-session.conf -- gjs -m tests/gnome_updates_test.js

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
