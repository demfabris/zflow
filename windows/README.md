# zflow for Windows

The Windows preview is two Rust programs: the zflow app, drawn with zz-gpui
(a fork of Zed's GPUI kept in this repository), and the input engine it starts
beside it. It runs in your signed-in session on Windows 10 version 2004 or
later and Windows 11, x64. Both link the Visual C++ runtime statically, so
nothing else needs to be installed.

## Run or install

Download and run the [Windows installer](https://github.com/demfabris/zflow/releases/latest/download/zflow-windows-x86_64-Setup.exe).
It installs into `%LOCALAPPDATA%\Zflow.App` and adds a Start menu shortcut.
No administrator access is needed. Configuration and paired computers remain
in `%LOCALAPPDATA%\zflow`, outside the installation directory.

The installed app checks for stable releases when it starts and every six
hours. **Settings → Updates** lets you check, download, and restart to
install an update. Restarting pauses input sharing; the app asks first.
**Check for updates automatically** in the same group controls automatic
checks. A failed check appears there, and you can retry with
**Check for updates**.

A WinUI release (0.6.x) updates to this app the same way as to any other
release. The package ID and program name are unchanged, so the installation
folder, Start menu shortcut, startup setting, configuration and paired
computers stay.

For a PowerShell installation, download and run the
[installation script](https://github.com/demfabris/zflow/releases/latest/download/install-windows.ps1):

```powershell
Invoke-WebRequest -UseBasicParsing https://github.com/demfabris/zflow/releases/latest/download/install-windows.ps1 -OutFile "$env:TEMP\zflow-install.ps1"
powershell -NoProfile -ExecutionPolicy Bypass -File "$env:TEMP\zflow-install.ps1"
```

It downloads the latest stable installer, checks its SHA-256 against that
release, and installs it. Add `-NoLaunch` to leave the app closed. No checkout,
SDK, or compiler is needed. The script waits for the previous app and engine
to close. If an old copy runs as administrator, quit it locally
before installing. The new app preserves your startup setting and uses the
new launcher's stable path. You can remove the old
`%LOCALAPPDATA%\Programs\zflow` folder after confirming the new app works.

The ZIP remains available for portable use. Open `Zflow.App.exe` and keep the
whole extracted folder together. Portable and development copies show a link
to the installer; they do not install updates in place.

Windows may ask to allow `zflow.exe` through its firewall. Allow it on the
networks you use for sharing. Input and introductions use UDP port 43119;
local discovery uses mDNS. Tailscale names and addresses work through
**+** (**Add a computer by address**), even when multicast discovery does not
cross the VPN.

Version 0.7.0 uses `zflow/8` for input and `zflow-hello/5` for introductions.
**Update the Mac and Linux computers to the same release, including the
GNOME extension.** Older input protocols cannot connect; pairing keys are
preserved.

1. Open zflow on each computer.
2. On **Computers**, other computers running zflow on this network show up
   under **Found on your network**. Drag one next to this PC's screens to add
   it. For one zflow cannot find, use **+** with a hostname (such as `macbook`
   or `xps`), an IP, or `host:port`. Bracket IPv6 when including a port.
3. Compare the marks on both computers. Add each computer on the other one
   too; discovery alone never grants input access.
4. Drag the other computer until its screens touch this PC's; the lit edges are
   where the pointer crosses. A computer's screens move together; double-click
   it to move them one by one. The arrangement propagates to connected peers. A remote monitor at an edge
   takes priority over a Windows monitor across that boundary. Disabled monitors
   leave the canvas and retain their saved positions for reconnection.
   If one physical panel is connected to two computers, it can appear twice:
   each tile belongs to that computer's active output. Place the Mac tile
   directly against the Dell tile to switch from the Dell to the Mac, and move
   the Windows copy elsewhere on the canvas. zflow does not switch the panel's
   hardware input or disable the other computer's output.
5. Leave all keys and buttons released, then move across a touching edge.
   **Pause at edges** adds a 250 ms dwell before crossing.

**Ctrl+Win+Backspace** returns input and pauses sharing. **Ctrl+Win+F12**
controls the first connected computer after you release the chord. Selecting
a connected computer also shows a **Control** button. Closing the window keeps
the app in the notification area; **Quit zflow** releases input and stops the
engine.

**Settings** has **Run at startup**, **Share clipboard**, **Pause at edges**,
administrator restart, updates, the keyboard shortcuts, and this PC's
addresses and configuration. Select a computer on **Computers** for its
crossings, screens, keyboard mode, reverse scrolling and **Forget**. The
version shows at the bottom of the sidebar. Clipboard sharing supports Unicode
text and the registered PNG clipboard format, up to 3 MB. Files, DIB-only
images, and content marked `ExcludeClipboardContentFromMonitorProcessing`
stay local.

Configuration is `%LOCALAPPDATA%\zflow\zflow.toml`; keys and arrangement are
in its `state` subdirectory. App edits preserve TOML comments and refuse to
overwrite external edits. **Settings → This computer → Configuration** opens
the file. After editing it manually, use **Restart** beside **Input engine**
in the same group. The engine's local named pipe permits only this Windows
account and rejects remote clients.

## Build

Install Rust (MSVC, 1.97 or later), the Visual Studio C++ Build Tools and a
Windows SDK. The build compiles the window's shaders with the SDK's
`fxc.exe`; set `GPUI_FXC_PATH` to its full path if the build cannot find it.
Then, from the repository root:

```powershell
./scripts/build-windows.ps1 -Launch
```

The result is `windows/dist`: `Zflow.App.exe`, built from `crates/zflow-app`,
and the `zflow.exe` engine side by side, plus this README, the license and the
installation script. The app keeps the name `Zflow.App.exe` so existing
installations, updates and the startup entry keep working. The build targets
`x86_64-pc-windows-msvc`, links both programs with the static CRT, and checks
their architecture and runtime imports with Visual Studio's `dumpbin`.

`scripts/package-windows.ps1` builds the installer, update feed and full
update package, plus a portable archive and SHA-256 sidecar under
`target/windows-release`. The package and app versions come from
`Cargo.toml`. Packaging uses Velopack's `vpk`, a .NET tool, so it needs a
.NET SDK (8 or later). It installs the `vpk` that matches the `velopack`
crate version in `Cargo.lock`.

To install a local release build, run
`./scripts/install-windows.ps1 -SourceDirectory target/windows-release`.

Publish `zflow-windows-x86_64-Setup.exe`, `releases.win-x64.json`, and the
`.nupkg` referenced by that feed together in each stable GitHub release.
The Windows updater ignores draft releases and prereleases. The release
workflow also publishes the portable ZIP and `install-windows.ps1` and includes
their checksums in `SHA256SUMS`.

Windows artifacts are unsigned unless `scripts/package-windows.ps1
-SignParameters '…'` supplies trusted `signtool` parameters. Velopack signs
the application, updater and installer during packaging. Windows may show
publisher or SmartScreen prompts for unsigned downloads.

Packaging also finds a .NET SDK at `%LOCALAPPDATA%\zflow-dev\dotnet`.
No SDK, compiler or runtime is needed to run a packaged app.

## Command line and diagnostics

```powershell
./zflow.exe setup
./zflow.exe run
# In another PowerShell window:
./zflow.exe doctor
./zflow.exe status
./zflow.exe nearby macbook
./zflow.exe trust <fingerprint-or-mark>
./zflow.exe switch macbook
./zflow.exe local
./zflow.exe stop
```

`--config PATH` selects another configuration. The GUI uses the default path.
The `request` command accepts one JSON request for scripted diagnostics.
Status exposes public identity and connection state; it never exposes private
keys or captured keystrokes. Input payloads are not logged.

## Support boundary and checks

The adapter uses low-level hooks to suppress local events while sending,
Raw Input for relative mouse motion, physical set-1 scan codes for keys, and
`SendInput` for injection. Media keys, five pointer buttons and horizontal
and vertical wheel input are supported. Raw multitouch forwarding is not.

The engine refuses input outside an active, unlocked desktop. Windows sign-in,
UAC secure desktops and elevated applications are outside a normal user's
injection rights. To control an administrator PowerShell or Terminal window,
choose **Restart as administrator** in **Settings → Administrator** and approve
the Windows prompt locally. This restarts both the app and the input engine;
cancelling the prompt keeps the existing app running. Elevation lasts for this
run only; **Run at startup** still starts normally. Windows sign-in and UAC prompts always
require local input. It does not install a service, driver, or UIAccess bypass.
If Windows rejects an input release after a focus change, zflow retains it and
retries until Windows permits it, before accepting another activation.
While an administrator window has focus, a normal zflow refuses crossings in
both directions and says why, rather than leaving the pointer at the edge.
Remote keys and buttons release on disconnect, lease expiry, pause, and
shutdown. A native watchdog returns physical input if the engine stalls,
without pausing sharing. A crossing the other computer refuses keeps the
connection, and Windows waits half a second before trying that computer again.

Automated checks:

```powershell
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
./scripts/build-windows.ps1
pwsh -NoProfile -File windows/Zflow.UpdateTests/Install.Tests.ps1
pwsh -NoProfile -File windows/Zflow.UpdateTests/Build.Tests.ps1
```

The two script checks replace Cargo, `dumpbin`, downloads and processes with
stand-ins, so they run with PowerShell 7 on any system. They cover the build's
flags and output folder, and the installer's checksum, shutdown and setup
order. Installation, the app's window and notification icon, updates,
elevation, file replacement and restart need native Windows qualification.
Test an update between two packaged versions before publishing it, including
one from the last WinUI release.

With sharing paused, `cargo test --lib native_return_guard_stops_injected_motion_at_an_internal_edge -- --ignored --nocapture`
checks the real Windows hook at the current display scaling. It briefly moves
and restores the pointer, without clicking or typing. Run it at scaling above
100% to cover the return-edge coordinate regression.

Before treating the preview as qualified, exercise both directions with a
Mac and GNOME desktop: typing, held keys, drag, wheel, crossing and return,
emergency pause, disconnect while a key is held, lock/unlock, sleep/wake,
multi-monitor DPI changes, and clipboard transfer. Real hardware capture and
cross-platform desktop behavior need this live check in addition to the
loopback and adapter tests.
