Prebuilt zflow for Windows x64, Linux x86-64/ARM64, and one universal macOS app for Apple silicon and Intel.
On a Mac, open `zflow-VERSION-macos.dmg` and drag zflow to Applications, or use the command below.
This is a prototype release; the live qualification checklist is in TESTPLAN.md.

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/demfabris/zflow/releases/latest/download/install.sh | bash
```

## New in 0.6.0: the pointer walks across computers

- With three or more computers arranged, the pointer moves straight from one
  computer to the next one beside it, and back, while the computer whose
  keyboard and mouse you use routes it. The computers in between do not need
  to trust each other, and an edit to the arrangement on one of them keeps
  the tiles of computers it has not paired.
- A move on to the next computer waits until no key or mouse button is held;
  until then the pointer stays where it is, and what you type still reaches
  the computer it is on. A finger resting on a touchpad does not count.
- Update every computer to 0.6.0, including GNOME extension API 5. Pairing keys
  stay valid; the new input protocol does not connect to 0.5.0 or older peers.
- Windows takes part as a computer the pointer passes through. Started from
  Windows, the pointer still only goes out and back.
- Hops are not yet tried on real machines; the checklist is in TESTPLAN.md.
- A Linux laptop's touchpad now drives the other computer like a mouse: one
  finger moves the pointer, two fingers scroll the natural way, a tap clicks,
  and a two-finger tap or press right-clicks. Before, the pointer stopped
  until the escape chord. A finger resting on the touchpad no longer stops a
  crossing. Raw touchpad contacts still go to a Linux computer when both have
  touch forwarding on; a Mac gets pointer input instead of dropping them.
  zflow does not read GNOME's touchpad settings yet and uses its defaults.

## Previous release: individual monitors (0.5.0)

- Native update controls: signed Sparkle updates on macOS, package-aware GNOME
  updates with system authorization, and a per-user Windows installer with
  Velopack updates. Settings and paired computers survive updates. Install this
  version once through the installer or disk image to receive future updates
  from inside the app.
- Install commands now fetch the installer from the latest published release.
  Windows users can run `zflow-windows-x86_64-Setup.exe`; the ZIP stays available
  for portable use.
- Windows, macOS, and GNOME discover active logical monitors with persistent
  platform identities, display names, and physical dimensions where available.
- Arrange each monitor independently. Handoffs and returns address the selected
  monitor, including negative desktop coordinates and mixed scaling. Local
  monitor boundaries continue to follow system display settings.
- Existing computer tiles split automatically. Disabled or unplugged monitors
  leave the canvas but retain their positions. Mirrors are one cursor surface.
- Update every computer to 0.5.0, including GNOME extension API 4. Pairing keys
  stay valid; the new input protocol does not connect to 0.4.0 or older peers.
- Shared physical monitor input selection, automatic DDC switching, manual
  size calibration, and rearranging OS-internal crossings remain future work.

## Previous release: Windows preview and native icons (0.4.0)

- Cursor Flow artwork across all three desktops: Icon Composer appearances on
  macOS, full-color and symbolic GNOME icons, and Windows app and tray icons.

- Native WinUI 3 app with a draggable computer arrangement, explicit pairing,
  Tailscale address entry, per-computer keyboard and scroll settings, clipboard
  sharing, and a notification-area menu.
- A per-user Rust engine captures physical keys and relative pointer motion,
  injects input on the unlocked Windows desktop, and releases input on lock,
  disconnect, pause, or shutdown. Ctrl+Win+Backspace returns input locally.
- Download `zflow-v0.4.0-windows-x86_64.zip`, extract the entire folder, and open
  `Zflow.App.exe`. The included PowerShell installer adds a Start menu shortcut.
  Windows 10 version 2004+ and Windows 11 x64 are supported; runtimes are bundled.
- Update every computer to 0.4.0 before pairing. The Windows OS label requires
  a protocol revision; 0.3.0 peers are incompatible. Existing pairing keys stay
  valid after all peers update. Windows requires explicit trust on each side.
- This preview still needs the hardware matrix in `windows/README.md` and
  `TESTPLAN.md`. Windows secure desktops, elevated apps, and raw multitouch are
  outside its support boundary.

## Previous release: arrange to pair (0.3.0)

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
0.4.0 cannot connect to ones on 0.3.0, 0.2.0 or v0.1.0, so update every computer;
until then they show each other as a different zflow version. An updated
computer never adds another by itself: only a fresh install does.
Restarting the Linux service interrupts active sharing. Reboot Linux
hosts once after updating from v0.1.0: its udev rule let logind give the desktop user write access to
`/dev/uinput`, and that access lasts until the next boot. The installer loads
the GNOME extension and starts the desktop agent in your current session; if
GNOME cannot fetch the extension from extensions.gnome.org, it installs the
bundled copy and asks you to log out once. The desktop agent starts at every
GNOME login; turn off Start at Login in zflow's settings to stop that.
