Prebuilt zflow for Linux x86-64/ARM64 and macOS Intel/Apple Silicon.
This is a prototype release; the live qualification checklist is in TESTPLAN.md.

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/demfabris/zflow/main/install.sh | bash
```

The installer selects the platform, verifies SHA-256 checksums, and installs
the binaries. Ubuntu/Debian installations use the `.deb` when no archive/source
installation is present. Other supported Linux systems use the archive.
Existing archive/source installations continue using `/usr/local` on upgrades.

Linux binaries require glibc 2.39+ and systemd 254+. GNOME settings require
GJS, GTK 4.12+, and libadwaita 1.5+. macOS requires version 26 or newer.
No Rust, Swift, Xcode, or C compiler is needed on the installing computer.

Mac apps use ad hoc signatures and are not notarized. Ordinary input sharing
works; installing the optional AWDL helper requires an Apple-issued signing
identity and is unavailable in these artifacts.

Updates preserve configuration and pairing identities. Restarting the Linux
service interrupts active sharing. Log out and back in after updating the
GNOME extension. Debian packages include the application launcher and extension;
enable zflow in GNOME Extensions and use Start at Login in Settings if desired.
