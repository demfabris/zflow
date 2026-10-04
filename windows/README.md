# zflow for Windows

The Windows preview uses a native WinUI 3 app and a Rust input engine. It runs
in your signed-in session on Windows 10 version 2004 or later and Windows 11,
x64. The portable folder includes .NET and the Windows App SDK runtimes.

## Run or install

Open `Zflow.App.exe` in the extracted Windows archive. Keep the whole folder
together. To add a Start menu shortcut and install into your account, run
`./install-windows.ps1` from PowerShell in that folder. No administrator access
is needed. Quit zflow from the notification-area menu before an update.

Windows may ask to allow `zflow.exe` through its firewall. Allow it on the
networks you use for sharing. Input and introductions use UDP port 43119;
local discovery uses mDNS. Tailscale names and addresses work through
**Add computer**, even when multicast discovery does not cross the VPN.

Version 0.5.0 adds individual monitor discovery and uses `zflow/6` for input
and `zflow-hello/5` for introductions. **Update the Mac and Linux computers
to the same source version, including the GNOME extension.** Older input
protocols cannot connect; pairing keys are preserved.

1. Open zflow on each computer.
2. Use **Add computer** with a hostname (such as `macbook` or `xps`), an IP,
   or `host:port`. Bracket IPv6 when including a port.
3. Compare the six-character marks on both computers. Add each computer on
   the other one too; discovery alone never grants input access.
4. Drag individual monitor tiles, including this PC's, until their edges touch.
   The arrangement propagates to connected peers. A remote monitor at an edge
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
controls the first connected computer after you release the chord. Each
computer also has a **Control** button. Closing the window keeps the app in
the notification area; **Quit zflow** releases input and stops the engine.

Settings include start at login, clipboard sharing, edge dwell, keyboard modes
and per-computer reverse scrolling. Expand a computer's card for its keyboard,
scrolling, and removal options. **About zflow** has the installed version,
license, documentation, and feedback links. Clipboard sharing supports Unicode text
and the registered PNG clipboard format, up to 3 MB. Files, DIB-only images,
and content marked `ExcludeClipboardContentFromMonitorProcessing` stay local.

Configuration is `%LOCALAPPDATA%\zflow\zflow.toml`; keys and arrangement are
in its `state` subdirectory. App edits preserve TOML comments and refuse to
overwrite external edits. After editing the file manually, use **Restart** in
**Settings → Connection and troubleshooting**. The engine's local named pipe permits only this Windows account
and rejects remote clients.

## Build

Install Rust (MSVC, 1.88 or later), the Visual Studio C++ Build Tools and a
Windows SDK, and the .NET 10 SDK. Then, from the repository root:

```powershell
./scripts/build-windows.ps1 -Launch
```

The result is `windows/dist/Zflow.App.exe`. `scripts/package-windows.ps1`
builds a portable archive and SHA-256 sidecar under `target/windows-release`.
`scripts/install-windows.ps1` installs an already-built app for the current user.

The build also detects a .NET SDK at `%LOCALAPPDATA%\zflow-dev\dotnet`.
No SDK or compiler is needed to run a packaged app.

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
choose **Settings → Restart as administrator** and approve the Windows prompt
locally. This restarts both the settings app and input engine; cancelling the
prompt keeps the existing app running. Elevation lasts for this run only;
**Run at startup** still starts normally. Windows sign-in and UAC prompts always
require local input. It does not install a service, driver, or UIAccess bypass.
If Windows rejects an input release after a focus change, zflow retains it and
retries until Windows permits it, before accepting another activation.
Remote keys and buttons release on disconnect, lease expiry, pause, and
shutdown. A native watchdog returns physical input if the engine stalls.

Automated checks:

```powershell
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
./scripts/build-windows.ps1
```

Before treating the preview as qualified, exercise both directions with a
Mac and GNOME desktop: typing, held keys, drag, wheel, crossing and return,
emergency pause, disconnect while a key is held, lock/unlock, sleep/wake,
multi-monitor DPI changes, and clipboard transfer. Real hardware capture and
cross-platform desktop behavior need this live check in addition to the
loopback and adapter tests.
