Prebuilt zflow for Linux x86-64/ARM64 and one universal macOS app for Apple silicon and Intel.
On a Mac, open `zflow-VERSION-macos.dmg` and drag zflow to Applications, or use the command below.
This is a prototype release; the live qualification checklist is in TESTPLAN.md.

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/demfabris/zflow/main/install.sh | bash
```

## New in 0.3.0: arrange to pair

- Setup codes are gone. zflow computers on the same network show up in the
  zflow window under Found on your network. Drag one into place next to this
  computer to add it.
- Once someone is at its desktop, a fresh install adds one computer by
  itself during the next 10 minutes, when that computer is the only new one
  around. If two new computers show up, or two share a name, nothing is added
  until you drag one in. An install over ssh alone never does this.
- Every computer has a mark of four colored squares, the same on every
  screen, so two computers with the same name can be told apart.
- A computer zflow cannot find, such as one on Tailscale, can be added by its
  address. On Linux without a desktop, `sudo zflow nearby` lists what was
  found and `sudo zflow trust NAME` adds one.
- Only UDP port 43119 needs to be open. Nothing listens on 43120 any more.
- Computers you paired before stay paired.

## Installing and updating

The installer selects the platform, verifies SHA-256 checksums, and installs
the binaries. Ubuntu/Debian installations use the `.deb` when no archive/source
installation is present. Other supported Linux systems use the archive.
Existing archive/source installations continue using `/usr/local` on upgrades.

Linux binaries require glibc 2.39+ and systemd 254+. GNOME settings require
GJS, GTK 4.12+, and libadwaita 1.5+. macOS requires version 26 or newer.
No Rust, Swift, Xcode, or C compiler is needed on the installing computer.

Mac apps include Developer ID signatures and Apple's notarization ticket.
You can install the optional AWDL helper from the app with administrator approval.

Updates preserve configuration and the computers you added. Computers on
0.3.0 cannot connect to ones on 0.2.0 or v0.1.0, so update every computer;
until then they show each other as a different zflow version. An updated
computer never adds another by itself: only a fresh install does.
Restarting the Linux service interrupts active sharing. Reboot Linux
hosts once after updating from v0.1.0: its udev rule let logind give the desktop user write access to
`/dev/uinput`, and that access lasts until the next boot. The installer loads
the GNOME extension and starts the desktop agent in your current session; if
GNOME cannot fetch the extension from extensions.gnome.org, it installs the
bundled copy and asks you to log out once. The desktop agent starts at every
GNOME login; turn off Start at Login in zflow's settings to stop that.
