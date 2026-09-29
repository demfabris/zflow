# Roadmap: two-way input, one GUI, Deskflow parity

Written 2026-09-28 against `e14a0c6`. Line numbers point at that commit.

Today the Mac sends input and Linux receives it. The goal is that any paired computer can control any other, like Synergy and Deskflow, with at least Deskflow's features and a simpler setup. Both apps show the same settings, natively: a shared top section, then a platform section.

Tags: **exists**, **partial**, **missing**. "Guess" marks anything not proven by code or by a command on the Ubuntu box.

## How to use this plan

- Phases ship one at a time, in the order in [Order](#order). Each phase lists its acceptance checks.
- **(Mac)** marks steps that need macOS to build or test: Swift, `src/macos/`, `just test-native`, the C harnesses. On the Ubuntu box, those files can be edited but not built. Leave them for the Mac, or note what is left.
- Run the checks from README's Develop section: `just check` and `just test-gtk` on Linux; `just test-native` and the C harnesses on the Mac.
- When a phase changes a rule written in SPEC.md, update SPEC.md in the same commit. Known ones:
  - `SPEC.md:161`: the capture set is watched while idle.
  - `SPEC.md:166`: grabs are all-or-none.
  - `SPEC.md:516`: portal limits.
  - `SPEC.md:548` and README line 7: receiving on macOS is future work.
- The decisions below are the defaults this plan follows. Change them here before starting the phase they affect.

## Decisions

| # | Question | Default |
|---|---|---|
| 1 | Roles or symmetric peers? | **Symmetric peers.** Ownership is per crossing; one sharing switch per machine; a per-peer "Can control this computer" toggle. |
| 2 | How does Linux send on GNOME Wayland? | **GNOME extension barriers plus the existing evdev grab.** No consent dialog on GNOME 50. The InputCapture portal comes later, for KDE and GNOME 51+. |
| 3 | Which devices does Linux grab when sending? | **All keyboards, mice and touchpads, through one static udev rule.** Skip devices that return `EBUSY`. The cost is that the `zflow` account can read every keyboard. |
| 4 | Monitor layout: shared or per machine? | **Shared.** Tiles keyed by key fingerprint, newest version wins. Needs `zflow/3`. |
| 5 | Order? | **Phase 0, then Phase 1**, with a Mac injection spike alongside Phase 1. |
| 6 | Clipboard? | **Text and images, synced when the pointer crosses, in Phase 4. Never files.** |
| 7 | Mac pointer acceleration? | **None by default: one count moves one point.** Spike I showed posted motion is not accelerated. A libinput-style curve in Rust felt accelerated in the first sitting, so it is opt-in through `ZFLOW_MAC_POINTER`. A virtual HID device needs Karabiner's driver or Apple's CoreHID entitlement (`SPEC.md:581-590`). |
| 8 | Protocol bumps? | **One batched `zflow/3` bump, no `zflow/2` fallback.** New desktop commands are strict JSON anyway. |

---

## 1. Parity matrix

Verdicts:
- **have**: zflow already matches or beats Deskflow.
- **build**: missing, and worth it.
- **improve**: exists but needs work.
- **skip**: not worth its complexity.

Deskflow issue numbers refer to [github.com/deskflow/deskflow](https://github.com/deskflow/deskflow/issues).

### Roles and topology

| Feature | Deskflow | zflow Mac | zflow Linux | Verdict | Why |
|---|---|---|---|---|---|
| Which machine controls | One server, N clients. Radio button, core restart. | Sends only (`src/macos/mod.rs:657-671`) | Receives; sends only by chord or CLI (`src/daemon.rs:758-777`) | improve | Drop roles. The session is already two-way. |
| Any machine controls any other | Not supported ([#5160](https://github.com/deskflow/deskflow/issues/5160)) | missing | partial | build | The main goal, and Deskflow's biggest gap. |
| Three or more computers, pointer moves from peer to peer | The server routes between clients | partial. Crossing only starts from the local tile (`src/app/handoff.rs:61`) | same | build (Phase 4) | Needs a Returned reply that says which edge it left through. |
| Wire compatibility with Barrier/Synergy | yes | no | no | skip | Different protocol by design. |
| Reconnect | Fixed or backoff | exists (`src/macos/link.rs`) | exists: a live link per peer on the Mac's schedule (`src/daemon/links.rs`, `src/link.rs`) | have | |
| Finds a peer whose IP changed | Several hostnames | exists: tries nearby addresses with the pinned key (`src/macos/link.rs`) | partial: a crossing tries nearby addresses with the pinned key (`ensure_session`); the live link dials only a computer advertised at the saved address | improve | Both sides advertise and browse. |
| Encryption and trust | TLS, fingerprint on first use | exists: QUIC, pinned key, 6-digit code plus Allow | exists | have | Stronger than Deskflow. |
| Discovery | none | browses only (`src/app/nearby.rs`) | the daemon advertises and browses (`start_discovery` in `src/daemon.rs`); the desktop agent browses for the pairing dialog (`src/app/nearby.rs`) | improve | Needed so Linux can find a Mac. |
| Computer names | Must match on both ends, plus aliases | Host name at pairing | same | have; rename: skip | No name agreement needed. |

### Layout

| Feature | Deskflow | Mac | Linux | Verdict | Why |
|---|---|---|---|---|---|
| Layout editor | 5x3 grid | exists (`ComputerLayout.swift`) | exists (`settings.js`, drag or arrow keys) | have | Both edit the shared layout. |
| Partial edge links | Text config only | exists (`src/app/layout_model.rs:98-146`) | shared code | have | |
| One layout for all machines | The server's | One per Mac | none | build | Decision 4. |
| Per-monitor tiles | none ([#6257](https://github.com/deskflow/deskflow/issues/6257)) | partial: the type allows several tiles per peer (`layout_model.rs:13-31`), but the app builds one bounding box per computer (`native.rs:436-477`) | none | skip | Entry already snaps to the nearest monitor (`extension.js:175-190`). Portal barriers only work on outer edges (`SPEC.md:518`). |

### Switching

| Feature | Deskflow | Mac | Linux | Verdict | Why |
|---|---|---|---|---|---|
| Edge switching as the sender | yes | exists (`src/app/sharing.rs:244-297`) | missing | build | Section 3. |
| Return through the edge as the receiver | yes | missing | exists (`extension.js:197-210`) | build | Section 4. |
| Hotkey: next or named computer | yes | missing | partial: only works with exactly one eligible peer (`src/daemon.rs:758-777`) | improve | One "switch to next computer" chord on both machines. |
| Emergency return | none | exists: Ctrl+Cmd+Backspace, hardcoded (`capture_bridge.c:593-601`) | exists: set in config (`src/config.rs:97-101`) | have | Same physical keys on both. |
| Lock the pointer to the current computer | Scroll Lock toggle | missing | missing | build (small) | A menu item on both. |
| Switch delay | Off, or 250 ms | missing | missing | build (small) | One "pause at edges" toggle, off by default. |
| Double tap | Off, or 250 ms | missing | missing | skip | The delay covers the same need. |
| Dead corners | Per-corner checkboxes | missing | missing | build (tiny, no setting) | A fixed few-pixel dead zone at corners. It protects the GNOME hot corner. |
| No crossing while a button is held | yes | exists (`sharing.rs:261`, `capture_bridge.c:132-141`) | n/a yet | have | |
| Require a modifier to cross | Mouse Without Borders | no | no | skip | Conflicts with the rule that modifiers must be up. |
| Relative moves for games | option | Always relative on the wire (`src/session.rs:97`) | same | have | |

### Keyboard

| Feature | Deskflow | Mac | Linux | Verdict | Why |
|---|---|---|---|---|---|
| Modifier remap per computer | Any to any | n/a | 3 modes per peer (`src/core/keymap.rs:15-28`) | improve | A Mac receiver needs a mode that turns PC shortcuts into Mac ones. |
| Keyboard layout | Keysym plus language sync | Physical keys; the receiver's layout applies (`SPEC.md:312`) | same | have | Avoids Deskflow's AltGr and dead-key bugs. |
| Left and right modifiers kept apart | no ([#8486](https://github.com/deskflow/deskflow/issues/8486)) | exists (`capture_bridge.c:462-501`) | exists | have | |
| Key repeat | DKRP message | The wire drops repeats (`src/session.rs:595-599`), so a Mac receiver must make its own | The compositor repeats | build (Mac) | |
| Lock key LED sync | Per-screen fixes | Caps sent as press and release per toggle (`capture_bridge.c:604-611`) | LED sync is optional in the spec (`SPEC.md:490`) | skip LED sync | Test Caps Lock on the Mac. |
| Secure Input notice | Names the app | Notice shown (`sharing.rs:266-270`) | n/a | have | |
| Media keys | yes | Swallowed while sending (`capture_bridge.c:19-28`) | Sent (`src/session.rs:78-83`) | build | |

### Pointer and scroll

| Feature | Deskflow | Mac | Linux | Verdict | Why |
|---|---|---|---|---|---|
| Reverse scroll per peer | per axis | missing | missing | build (small) | Natural scrolling on the Mac against Linux settings is the usual mismatch. Apply it on the receiver. |
| Scroll speed per peer | 0.1 to 10 | missing | missing | skip | Reversing is what people need. |
| Pointer speed | open request | The Mac receiver moves one point per count; a curve is opt-in | libinput accelerates the virtual pointer (guess) | build (Mac, no UI at first) | |
| Hi-res wheel | yes | Pixel deltas | exists (`src/linux/mapping.rs:368-381`) | have | |
| Trackpad gestures | no ([#2905](https://github.com/deskflow/deskflow/issues/2905)) | Raw contacts to Linux, experimental | receives them | have (Mac to Linux); skip (Linux to Mac) | No public macOS API. |
| Linux laptop touchpad as the sender | yes, through EIS | n/a | Raw contacts only (`src/linux/touch.rs`) | skip for now | Section 3. |

### Clipboard, power, misc

| Feature | Deskflow | Mac | Linux | Verdict | Why |
|---|---|---|---|---|---|
| Clipboard text | yes | missing (`SPEC.md:71`) | exists (Phase 4) | build (Phase 4) | Sync it only when the pointer crosses. |
| Clipboard images | yes, 3 MiB cap | missing | exists: PNG, 3 MiB cap (Phase 4) | build after text | Capped, no UI. |
| Primary selection | yes | n/a | missing | skip | |
| Files | Removed in v1.22 ([PR #8569](https://github.com/deskflow/deskflow/pull/8569)) | no | no | skip | |
| Screensaver sync | always on | Locking the Mac turns on Secure Input, which ends the crossing (`src/macos/mod.rs:46`) | The seat gate refuses input while locked (`src/daemon.rs:1033-1050`) | have (partial) | |
| Keep the sender awake | n/a | Guess: fine | missing: with devices grabbed, GNOME sees no input and may lock | build (Linux) | An idle inhibitor while sending. |
| Wake the target display | in master | missing | Guess: fine | build (Mac) | |
| Run a command on enter/exit | in master | no | no | skip | |
| Start at login, tray | declined / yes | exists | exists | have | |
| Logs in the GUI | panel | Logs go to stderr only (`src/ffi.rs:52`) | journald | build (tiny) | An "Open logs" action. |
| Pre-login input | Windows only | n/a | exists | have | |
| Mac permission checks | Fatal error | exists (`src/app/native.rs:221-237`) | n/a | have | |
| Wi-Fi lag (AWDL) | wiki workaround | exists (`src/macos/awdl.rs`) | n/a | improve | Hold the lease while receiving too. |
| Wayland sender | Portal, dialog every session on GNOME 50 | n/a | missing | build | |
| Wayland receiver | RemoteDesktop portal | n/a | exists: uinput, works at the greeter | have | |

---

## 2. Two-way input

### Why roles can go

Nothing below the GUI and the pairing defaults has a role:
- The session actor holds both a `sender` and an `inbound` receiver (`src/session.rs:372-381`).
- Either end can start sending (`src/session.rs:241-245`).
- The daemon reuses a session the peer dialed: `accept_connection` stores it under the peer's name (`src/daemon.rs:489-495`), and `ensure_session` returns it (`src/daemon.rs:400-402`).
- The ALPN names no role.

"Mac sends, Linux receives" comes from six local choices:
- pairing saves one direction (`src/pairing.rs:291-313`);
- the Mac only dials;
- the Mac refuses input (`src/macos/mod.rs:657-671`);
- only the Linux daemon advertises on mDNS;
- the handoff client is built only on the Mac (`src/app/mod.rs:9-21`);
- only the Mac has a layout.

### Model

- **Ownership is per crossing.** The machine whose own input crossed an edge, or whose user pressed the chord, is the sender. The peer it entered is the receiver.
- **One sharing switch per machine**, meaning "send and receive".
- **`send_normal` becomes a per-peer toggle.** It already means "this peer may control me" (`src/config.rs:222-227`). `receive_normal` is set to true at pairing, and the layout decides where this machine sends.

### Exclusion rules (none exist yet)

1. **A machine being controlled arms no outbound edges and refuses to start sending.** Today `activate` checks only the local grab state (`src/daemon.rs:354`).
2. **A machine that is sending, or about to send, refuses to be controlled.** It marks itself "outbound pending" before it sends Prepare. Desktop requests are already refused (`src/daemon/desktop.rs:215-224`). Input effects are not: `route_receiver_effects` never checks `active_outbound` (`src/daemon.rs:1430-1470`).
3. **Both machines cross at the same moment:** each refuses the other, and both stay local. The user pushes again. This fails safe, so no tie-break is needed.
4. **A receiver's own devices keep working.** On the Mac this needs a suppression interval of 0.

**Duplicate connections:** keep the connection dialed by the peer with the lower key fingerprint. The daemon does (`identity::wins_simultaneous_dial`); the Mac needs it once it accepts connections.

### Smallest code changes, by file (no wire change)

| # | File | Change | LOC (guess) |
|---|---|---|---|
| 1 | `src/pairing.rs:291-313` | Pairing saves both directions, as the CLI already does (`src/cli.rs:1506-1523`). Update the tests at `:662-712`. Pairing a known peer again still keeps its permissions (`:306-313`), so existing peers need #2. | 30 |
| 2 | `src/peer_view.rs:14-44`, `src/daemon/peer_view.rs:88-143` | `SetPeer {name, allow_control?, keyboard?}` replaces `SetKeyboard`. It never touches `inject_prelogin`. Extend the tests at `src/peer_view.rs:146-163`. | 60 |
| 3 | `src/app/native.rs:28-68` | The same `SetPeer` on the Mac. | 30 |
| 4 | `src/macos/link.rs:385-400`, `:111-135` **(Mac)** | One endpoint bound to `transport.listen` (`[::]:43119`, `src/config.rs:140`) with a server config, copied from `src/daemon.rs:93-100`. Add an accept loop. Link to every peer with `connect`, not only `receive_normal` (`link.rs:116`). Add the fingerprint rule. | 250 |
| 5 | `src/macos/link.rs:481`, `src/macos/mod.rs:637,657-671` **(Mac)** | Send input effects to the injector and desktop requests to a Mac desktop server, instead of refusing them. | 300 plus section 4 |
| 6 | `src/session/receive.rs:608-622` | A macOS `backend_supports`. | 20 |
| 7 | `src/app/sharing.rs:244-297` **(Mac)** | The observer skips crossings while the Mac is being controlled. Otherwise remote input that reaches the edge would start a crossing back. | 20 |
| 8 | `src/daemon.rs:353-356`, `:1430-1470` | Exclusion rules 1 and 2. | 40 |
| 9 | `src/daemon.rs:1223-1243`, `src/discovery.rs:204-212` | The Mac advertises on mDNS (`_zflow._udp` is already in `packaging/macOS/Info.plist:29-32`). The daemon browses and tries pinned connections to nearby addresses, like the Mac does (`native.rs:414-420`). | 120 |
| 10 | `src/app/mod.rs:9-21`, `src/macos/mod.rs:241-317,741-753` | Stop building `handoff` only on macOS. Move the generic handoff client (token, Prepare, Poll, Finish) into shared code, behind a small trait for the AWDL lease, capture and geometry. | 300 moved |

### Wire, ALPN and version

- **Two-way itself needs no wire change.** It uses the existing `DesktopRequest` (`src/desktop.rs:212-229`) and receiver messages.
- **Later features need new messages:** a shared layout, clipboard, and a Returned reply that says which edge the pointer left through.
  - There is no version field on the wire; the ALPN is the version (`src/wire/mod.rs:1-4`).
  - Desktop messages are strict JSON (`deny_unknown_fields`, `src/desktop.rs:212-213`), so an old peer cannot parse a new command.
  - So batch all three into one `zflow/2` to `zflow/3` bump (`src/transport/tls.rs:29`), with no fallback. Discovery already flags peers on a different ALPN.

### How both ends agree on the layout

- Tiles are keyed by key fingerprint, because local names differ: the Mac's own tile is literally labelled "This Mac" (`native.rs:462`).
- Each machine writes its own tile size.
- The whole document (at most 32 tiles) is sent on connect and after each change. The newest `(counter, fingerprint)` wins.
- Each machine computes its outbound edges from its own tile. `transitions()` already produces both directions of every touching edge (`layout_model.rs:98-146`).
- A stale copy never breaks a crossing, because the receiver follows the sender's Prepare.
- About 300 to 400 lines, and it needs `zflow/3`.

Fallback if decision 4 changes: each machine keeps its own layout, and the user arranges things twice.

---

## 3. Linux sender on GNOME Wayland

### What the Ubuntu box supports (checked 2026-09-28)

| Check | Result |
|---|---|
| OS and GNOME | Ubuntu 26.04.1 LTS, GNOME Shell 50.1 |
| InputCapture portal | version 1, capabilities 15 |
| RemoteDesktop portal | version 2 |
| GlobalShortcuts portal | version 1 |
| Cursor hiding API for shell code | `inhibit_cursor_visibility` (used in the shell's `magnifier.js:187,203`) |
| zflowd | runs as `User=zflow` (`packaging/systemd/zflowd.service:10`); it can only read event nodes that udev grants to group `zflow` |
| udev | one Logitech rule, plus a hand-written `/etc/udev/rules.d/71-zflow-openlogi-capture.rules` for an "OpenLogi virtual mouse" |

InputCapture version 1 has no restore token, so its permission dialog appears every session. An LTS usually does not move to a new GNOME major, so this box likely stays on GNOME 50 (guess). The OpenLogi rule shows that picking devices by hand already needs manual work.

### Recommended: extension barriers plus evdev grab

1. The daemon computes its outbound edges with the shared `handoff` code. It sends them over the daemon-to-agent link it already has (`src/daemon/desktop.rs`), as a new `edges` message.
2. The agent (`src/app/desktop.rs`) passes them to the extension. The extension API goes from 1 to 2 (`src/app/gnome.rs:18`, `client.js:8`).
3. The extension places outward-blocking `Meta.Barrier`s the same way it places return barriers (`extension.js:197-210`). On `hit`, it signals the agent, and the agent sends `EdgeHit` to the daemon over `peers.sock`.
4. The daemon checks the exclusion rules, marks itself "outbound pending", and sends Prepare. On `Prepared`, it sends `RuntimeCommand::Activate`. The existing Arming step waits for all keys to be released, then grabs (`src/runtime/linux.rs:985-1000`).
5. The daemon asks the agent to hide the cursor and hold an idle inhibitor.
6. The daemon polls. On Returned, it ungrabs, and the extension warps the pointer to the mapped point (`src/desktop.rs:99-179`) and shows it again.
7. The escape chord and the greeter path stay as they are (`src/runtime/linux.rs:943-968`).

Pros:
- No consent dialog.
- It reuses the extension, which receiving already requires, plus the existing grab, ownership states and barriers.
- Modifier state comes from the kernel. mutter's EIS sends no modifier events.
- Barriers can also sit on inner monitor edges.

Cons:
- GNOME only.
- A laptop touchpad gives raw contacts, not a pointer.
- Idle must be inhibited while devices are grabbed.
- The capture set must include every device the user touches.
- Motion is raw, so the receiver must accelerate it.

### Later: InputCapture portal plus libei

- Gains: it covers every device and hotplug, and touchpads arrive as pointer input.
- Costs on this box: a consent dialog every session, no cursor hiding before mutter 51, no modifier events, and a new event stream from the agent to the daemon.
- `SPEC.md:516` would need relaxing.
- Use it for KDE Plasma 6.1+ and GNOME 51+.

Rejected: mutter's private InputCapture D-Bus API, and capturing inside the shell with `pushModal`.

### Capture set

1. Ship one static udev rule: every keyboard, mouse and touchpad on seat0 without `ZFLOW_CAPTURE_EXCLUDE` gets group `zflow`, mode 0640.
2. Grab all of them at Arming, found by the scan in `src/linux/devices.rs:80-132`.
3. Skip a device that returns `EBUSY`, instead of failing the whole grab. A remapper such as OpenLogi already holds its source device, and zflow grabs its virtual output instead. This changes the all-or-none rule in `SPEC.md:166` for that one case.

Cost: the network-facing account can read every keyboard. To reduce that, open devices only at Arming and make the idle chord optional. Today the daemon watches the capture set while idle (`SPEC.md:161`). Whether the runtime can open devices only at Arming is a guess; nobody traced it yet.

### Check these first (guesses)

1. A barrier hit fires for local mouse motion. Injected motion is proven today; local motion is not.
2. The extension can safely call `inhibit_cursor_visibility`.
3. The delay from hit to grab feels fine.
4. A key held at the edge pins the pointer at the barrier until it is released.

---

## 4. Mac receiver (Mac)

The baseline is CGEventPost from the logged-in app, which is what Deskflow and lan-mouse do. No new process is needed. Spike I (2026-09-28, `spikes/i-mac-inject/RESULT.md`) passed on this Mac: the gotchas below are measured unless marked (guess).

**Reusable pieces:**
- The receiver state machine (`src/core/receiver.rs`) and playout (`src/session/receive.rs`).
- The keymap (`src/core/keymap.rs`).
- Display rectangles, cursor position and warp (`src/macos/mod.rs:129-160`, `capture_bridge.c:78-126,294-314`).
- The keycode table (`src/macos/mod.rs:867-988`) and ISO detection (`capture_bridge.c:389-399`).
- The existing Accessibility grant.

**New files:** `src/macos/inject.c` and `src/macos/inject.rs`. Everything in the table is **missing**.

| Area | Design | Gotcha |
|---|---|---|
| Event source | HID system-state source, posted at the HID tap, suppression interval 0, local events permitted. | The Mac's own trackpad kept working with every source tried, including the default one, while events were posted every 8 ms. The deprecated system-wide suppression calls are not needed. |
| Pointer | Current cursor plus the delta, one point per count, clamped to the displays and snapped to the nearest one. Also set the delta fields. A libinput-style curve is opt-in. | Linux sends raw deltas (`src/session.rs:97`). CGEventPost places the cursor at the event location 1:1, with no acceleration; delta fields alone do not move it. macOS does not clamp an off-screen location. |
| Drag and clicks | Drag event types while a button is down. Click state on every down and up; the event number is optional. | Without click state there is no double-click. A posted click does activate a background app on 27. |
| Keys | Inverted table, ISO swap keyed on the receiving Mac's keyboard type, modifier flags (including the left/right device bits) on every event. | The HID-state source merges held modifiers anyway. The keyboard-type field on a posted event does not change characters, so the ISO swap must be in the table. Untested on an ISO Mac. |
| Repeat | Made on the receiver at the System Settings delay and interval, with the autorepeat field set. | The wire drops repeats (`src/session.rs:595-599`), and macOS does not repeat a held posted key. Defaults read 225/30 ms, AppKit reports 250/33 ms; pick one. |
| Caps Lock | `IOHIDSetModifierLockState` on an `IOHIDSystem` connection. | Keycode 57 only sets the event flag, so letters and the real lock disagree. |
| Unmapped keys | PrintScreen becomes F13. Drop ScrollLock, Pause and F21 to F24. | F14 and F15 never reach apps. F16 to F19 are untested. |
| Media keys | NX_SYSDEFINED subtype 8. | Volume and brightness both work; brightness stepped on the external display with the lid closed. |
| Scroll | Pixel units only, 30 px per detent (Deskflow's 3 lines of 10 px), with the part under a pixel carried on each axis. | Playout hands over the wheel in batches of any size, so the distance must depend only on the total. No phase or momentum on the wire (`src/session.rs:103-104`). macOS accepts posted phases and momentum, so the wire could carry them later. |
| Wake | `IOPMAssertionDeclareUserActivity` on Prepare. | Not tested yet (spike I skipped it). |

**Handoff answers, in-process:**
- Snapshot: the display rectangles plus the cursor position.
- Prepare: warp 3 px inside the entry edge (as `extension.js:189-190` does) and start the 2 s lease.
- Poll: hold up to 200 ms. When a clamped move would leave through the armed edge range, answer Returned. Also read the real cursor, because the Mac's own trackpad can reach that edge.
- Finish: release everything.

**Other Mac work:**
- Copy (`OnboardingView.swift:112`, `SettingsView.swift:124`): the Accessibility wording mentions sending only.
- Firewall: listening may trigger an Application Firewall prompt (guess).
- AWDL: hold the lease while receiving too (`src/macos/mod.rs:260-263`).
- App activity: hold the latency activity whenever a session is active (`AppModel.swift:78-87`).
- Keyboard modes: make `mac` mean "translate to this machine's shortcuts" (the value gains the alias `shortcuts`). On a Mac receiver that means PC shortcuts become Mac ones. Terminals keep Ctrl; the app tells the engine which app is in front, like Linux's existing `focus` request (`src/peer_view.rs:30-32`).
- Skip gestures, the login window and fast user switching.

---

## 5. Unified GUI

Rust produces one `Snapshot` and accepts one `Request` set on both platforms. SwiftUI and libadwaita render them natively. The top section is always shared. The bottom section comes from `snapshot.platform`. Health is a list of `{id, level, title, detail, action}` built in Rust.

### Top section (shared, in this order)

| # | Label | Control | API (today's backing) | Mac | Linux |
|---|---|---|---|---|---|
| 1 | Status: Ready / Controlling X / Controlled by X / Paused / Needs attention | Title plus badge | `snapshot.status` (`native.rs:522-541`, `client.js:15-25`) | yes | yes |
| 2 | Input sharing | Switch | `set_sharing` | menu only | yes |
| 3 | Health | Rows with fix buttons | `snapshot.health[]`, `retry` | yes (popover, `SettingsView.swift:114-204`) | one error row (`settings.js:47-51`) |
| 4 | Computers: layout | Drag tiles | `move_tile` (`native.rs:151-177`) | yes | yes |
| 5 | Peer row: name and state | Subtitle | `snapshot.peers[].state` (`native.rs:505-518`, `daemon/peer_view.rs:103-117`) | names only (`Core.swift:35`) | yes (the `settings.js:106` label is backwards) |
| 6 | Can control this computer | Switch | `set_peer {allow_control}` (new) | no | root CLI only (`src/control.rs:32-35`) |
| 7 | Keys from this computer | Dropdown | `set_peer {keyboard}` | no | yes |
| 8 | Reverse scrolling | Switch | `set_peer {reverse_scroll}` (new) | no | no |
| 9 | Forget | Button plus confirm | `forget` | yes | yes |
| 10 | Pair Computer… | Sheet: "your code" and "their code" side by side, plus nearby computers | `pair`, `pair_respond`, `pair_cancel` | yes | yes |
| 11 | Pause at edges | Switch | `set_switching` (new) | no | no |
| 12 | Shortcuts | Read-only rows | `snapshot.shortcuts` | hint only (`SettingsView.swift:186`) | no |
| 13 | Start at login | Switch | Mac: Swift SMAppService (`Services.swift:106-126`); Linux: `set_autostart` (`gnome.rs:240-262`) | yes | yes |
| 14 | Share clipboard (Phase 4) | Switch | `set_clipboard` | no | yes |
| 15 | Advanced configuration | Mac: button; Linux: path text (the file is root-owned) | none | yes | text only (`settings.js:52`) |

### Bottom section, Mac

| Item | Backing | Status today |
|---|---|---|
| Accessibility | `native.rs:196-205` | exists |
| Local Network | `discover`, the local-network status | exists |
| Reduce Wi-Fi lag, plus helper install/repair | `set_awdl`; Swift `Services.swift:73-104` | exists |
| Move to Applications | Shown only when needed | exists |
| Secure Input row | | partial |
| Open logs | | new |

### Bottom section, Linux

| Item | Backing | Status today |
|---|---|---|
| GNOME integration: Install / Turn on / Log out | `install_extension`, `setup.js` | exists |
| Background service: state and version | | partial |
| Input devices | | new; CLI only today |
| Login screen input (read-only) | | new |
| Experimental touchpad (read-only) | | new |
| Open logs (journalctl) | | new |

### Menus (same on both)

1. Status line.
2. Input sharing.
3. Lock pointer to this computer (new).
4. Settings….
5. Mac only: Set Up… and Quit.

### API changes

- **New module:** `src/app/api.rs`, with no platform gates.
- **Mac:** `native.rs` uses it through the unchanged JSON FFI (`src/ffi.rs:95-113`).
- **Linux:** `gnome.rs` uses it over D-Bus and forwards privileged requests to `peer_view`.
- **`peer_view` stays the privilege boundary:**
  - add `SetPeer`, `MoveTile`, `EdgeHit`, and per-peer state in `Status`;
  - remove `SetKeyboard`;
  - keep refusing `permissions` and `inject_prelogin`.
- **`control` does not change.**
- **Also remove:**
  - the `pair_start` / `remote` names (use `pair` / `address`);
  - the flattened `receiver_error` string (`native.rs:511-514`);
  - the 43120 port rewrite, which is duplicated in `PairingView.swift:123-126` and `settings.js:278`. Rust returns `pair_address` instead.
- **Later:** merge `[daemon].sharing` and `[macos].sharing` (`src/config.rs:46-78`).

**Copy that encodes the old roles:**
- Mac: `OnboardingView.swift:112,121,186,207`, `PairingView.swift:58,62,71`, `SettingsView.swift:124`.
- Linux: `settings.js:36,176,253,265`.
- Rust: `src/macos/mod.rs:661`.
- Bug: `settings.js:106` labels `sending_to` as "Controlling this computer"; it should read "Controlled from here".

---

## 6. Phases

### Phase 0: two-way groundwork (do first)

- **Scope:** changes 1, 2 and 8 from section 2, the role wording fixes, and the `settings.js:106` bug. The Swift wording is **(Mac)**.
- **Acceptance:**
  - Pairing from either GUI saves both directions on both ends.
  - An old one-way peer becomes two-way through the Linux toggle, without re-pairing.
  - The daemon refuses input while its devices are grabbed (unit test next to `src/daemon.rs:1052`).
  - No GUI text names a role or an OS.
- **Size:** about 8 files, 150 to 250 LOC. No wire change.
- **Risk:** low.
- **Status (2026-09-28):** done on Linux. `set_peer` also sets `receive_normal`, which is how an old record becomes two-way. The GNOME agent still takes `set_keyboard` from extensions.gnome.org copies that lag the package. The Mac side is done too: the Mac build, clippy, Rust, Swift and C checks passed with no code changes. The Swift views no longer name Linux or a role, and the errors the Mac window shows say "the other computer" instead of "the receiver". The Mac still refuses input until Phase 2 (`src/macos/mod.rs:657-671`), so its copy does not claim it can be controlled.

### Phase 1: one API, one GUI

- **Scope:** `api.rs`; `native.rs` and `gnome.rs` moved onto it; the Swift **(Mac)** and GJS top/bottom split; health rows on Linux. The Linux layout slot stays empty until Phase 3.
- **Acceptance:**
  - Top-section items 1 to 13 appear in the same order on both platforms.
  - One `Snapshot` is decoded by both GUIs, tested in Rust plus the FFI tests at `native.rs:557-596`.
  - The `peer_view` rejection tests still pass.
- **Size:** about 14 files, 900 to 1300 lines changed.
- **Risk:** churn. No new features in this phase.
- **Status (2026-09-28):** the Linux half is done on branch `one-settings-api`.
  - `src/app/api.rs` holds `Snapshot<P>` (the platform section is a type parameter), `Status`, `Peer`, `Health` and one `Request` enum. Status titles and peer row text come from Rust.
  - The GNOME agent, panel and settings window use it. Health rows and shortcut rows show on Linux. Nearby records carry `pair_address`, so the 43120 rewrite in `settings.js` is gone.
  - The GNOME API level went from 1 to 2, because the snapshot changed shape. Phase 3 needs no second raise if it ships in the same release.
  - Items 8 and 11 wait for Phase 4, since they are new features.
  - The Mac half is done too. `native.rs` builds `api::Snapshot<MacPlatform>` and matches `api::Request`. Link errors are `unreachable` peers with the error as their text, plus a "Paired computers" health row with Retry. That row is also an error when no paired computer takes input from this Mac. Missing Accessibility, a failed crossing, layout and configuration errors are health rows too, so the status says "Needs attention" for them. The Allow button for Accessibility is only in the "This Mac" section. The layout check waits until a computer connects, since a new one has no tile yet. The Mac platform struct carries Accessibility, Local Network and Reduce Wi-Fi lag for the "This Mac" section; the app adds the helper and Move to Applications. The Mac hides "Can control this computer" and the keyboard picker until Phase 2. A missing Wi-Fi helper no longer makes the status "Needs attention", since sharing works without it.

### Phase 2: the Mac receives (Mac)

- **Scope:** changes 4 to 7 and 9 from section 2, and all of section 4.
- **Acceptance** (Mac16,5 on macOS 27, controlled by chord from the Ubuntu box):
  - ISO typing, modifier clicks, drag, double-click, wheel, media keys and system-rate repeat all work.
  - The Mac's own trackpad still works while it is being controlled.
  - Killing zflowd releases held keys within 1 s (`SPEC.md:45`).
  - Mac-to-Linux crossing still works.
  - A two-session unit test with a fake sender covers the desktop handoff.
- **Size:** about 10 files, 1600 to 2200 LOC.
- **Risks:** macOS 27 posting quirks; pointer feel; Caps Lock; a firewall prompt; duplicate connections.
- **Status (2026-09-28):** built on branch `mac-receiver`; the live sitting has not run.
  - Done:
    - The injector: `inject.c` and `media.m` post, and `inject.rs` turns receiver effects into Mac events, with the pointer clamp and an opt-in curve, click state, drags, flags, the ISO swap, receiver-made repeat, Caps Lock through IOKit, media keys, pixel scroll, and release on every exit path the app sees.
    - Receiving over the session the Mac dials and over connections peers open. The Mac listens on `transport.listen` and keeps one session per computer with the shared dial rule.
    - One ownership guard: input goes one way at a time, and the Mac starts no crossing while controlled.
    - The desktop handoff server, so a Linux edge crossing enters and returns at the mapped point. The Mac's own crossings use the shared handoff functions.
    - Keyboard modes, with Mac shortcuts trading Ctrl and Cmd outside terminals, and reverse scrolling per computer.
    - The shared layout on the Mac, the mDNS advertisement, the AWDL lease while controlled, and waking the display on Prepare and on taking control.
    - The Mac settings rows: "Can control this computer", "Keys from NAME" and "Reverse scrolling".
    - This also covers the Mac items left under Phases 3 and 4: merging layouts, the shared handoff functions, and reverse scroll.
  - The sitting still has to prove (TESTPLAN.md, "Two-way input sitting, Mac side"): pointer feel, click and drag behavior, typing with repeat at the system rate, Caps Lock, media keys, scroll distance per detent, the Mac's own devices while controlled, release within 1 s after killing zflowd, every exit path, wake, AWDL, duplicate connections, the firewall and Local Network behavior on the listener, and that Mac-to-Linux crossings still work.
  - Known limits:
    - Held input stays down after SIGKILL of the Mac app, until the local key is pressed.
    - Motion is one point per count by default (`flat:0`, chosen in the first sitting), whatever the sender's mouse DPI. `ZFLOW_MAC_POINTER=adaptive:<speed>` opts into the untuned curve; it is a sitting-only knob.
    - Scroll is 30 px per detent at any speed, with no wheel acceleration. The number is untuned.
    - The Mac reads every sender's scroll as 120 units per detent, so a Mac trackpad sender scrolls a Mac receiver slowly.
    - A connection that arrives during a crossing waits until the crossing ends.
    - With `experimental_touchpad` on both computers, a Linux sender may send contacts the Mac cannot post, and they are dropped.
    - Secure fields, full-screen apps and games are untested.
    - Clipboard sharing and pause at edges are on the Mac too (Phase 4 items). The Mac reads the clipboard only when macOS allows it without asking; otherwise Checks says to allow zflow under Paste from Other Apps. Whether macOS 27 still shows an alert at crossings is for the sitting.

### Phase 3: Linux sends by edge, plus the shared layout

- **Scope:** change 10 from section 2, section 3's recommended design, the capture set, the Linux layout canvas, and the shared layout with `zflow/3`.
- **Acceptance** (on the Ubuntu box):
  - Crossing to the Mac by edge shows no dialog and hides the Linux cursor.
  - Returning puts the pointer back at the mapped point.
  - The escape chord works.
  - A 15-minute remote session does not lock the Linux screen.
  - No crossing starts while Linux is being controlled.
  - A simultaneous crossing leaves both machines local.
  - A hotplugged mouse is grabbed at the next crossing.
  - A layout edit shows up on the other machine within 2 s.
- **Size:** about 15 files, 1500 to 2000 LOC.
- **Risks:** barrier hits from local motion; held keys at the edge; remappers; extension review; wider device access.
- **Status (2026-09-28):** in progress on branch `linux-edge-sending`.
  - Done:
    - Capture-all: an empty `capture_devices` list captures every keyboard and pointer, and a device another program grabbed (EBUSY) is skipped.
    - The daemon half of change 9, and the rule for two computers dialing each other at once (`identity::wins_simultaneous_dial`).
    - GNOME edge barriers: a push sends `edge_hit`. While sending, the pointer is hidden and an idle inhibitor is held. The barriers come back after unlocking, and they stop 8 px short of the desktop's corners (Phase 4's dead corners).
    - Change 10: the pure handoff parts are shared in `src/app/handoff.rs`, and `src/daemon/crossing.rs` drives a crossing: Prepare at the matching entry point, arm, Poll until the pointer leaves the other computer, warp back and Finish. A push made with a key or button held gives up after 400 ms instead of crossing later.
    - Each computer writes its own tile size into the shared layout. A resize keeps the sides that touch a neighbour.
    - The shared layout (decision 4): the daemon keeps the newest copy and passes it on. The protocol is now `zflow/3`.
    - The Linux layout editor: the settings window shows the shared layout, and a drag or an arrow key moves a computer through `move_tile`. With no layout yet, the daemon keeps a first one, with each paired computer to the right of this one, as soon as GNOME describes the desktop. Any layout a peer arranged replaces it.
    - A computer paired after the layout exists gets a tile beside this one (`SharedLayout::with_tiles_for`), and so does one missing from a layout a peer sent.
    - Live links (`src/daemon/links.rs`): the daemon keeps a session to each peer it may send to, with the Mac's retry schedule (`src/link.rs`), so the first crossing reuses it. A session the peer dialed counts. It dials only where mDNS shows a zflow computer at the peer's saved address, so it dials a Mac only while the Mac listens. GNOME shows each peer as Connected, Connecting or Unreachable with a reason, plus a "Paired computers" health row with Retry. The GNOME API level stays 2: the snapshot keeps its shape, and `connecting`, `unreachable` and health actions were already part of it.
  - Left:
    - The Mac sending and merging layouts, and switching its handoff to the shared functions (Phase 2 side).
    - The live checks in TESTPLAN.md, "Two-way input sitting".
  - Known limits:
    - A finger resting on a captured touchpad counts as held, so an edge push gives up. This matters on a Linux laptop.
    - In capture-all mode, any captured device going away (a sleeping Bluetooth mouse) ends a crossing.
    - With three or more computers where not every pair is paired, an edit on one computer drops the tiles of computers it has not paired. A computer that paired them puts them back beside itself, but not where they were. SPEC's rule against resurrecting forgotten computers causes this; peer-to-peer hops (Phase 4) need a rule that keeps third-party tiles.

### Phase 4: the Deskflow extras

- **Scope:**
  - Clipboard, synced when the pointer crosses: text first, then images.
    - It uses its own QUIC stream and a 3 MiB cap, and both sides opt in.
    - GNOME side via `St.Clipboard` (guess); Mac side via `NSPasteboard`.
  - Peer-to-peer hops.
  - Lock, pause at edges, dead corners, reverse scroll.
  - A Mac "switch to next computer" chord.
  - Mac media keys.
  - Open logs.
- **Status (2026-09-28):** started on Linux, on branch `linux-edge-sending`.
  - Done: dead corners (outbound barriers stop 8 px short of the desktop's corners), pause at edges (`[switching] pause_at_edges`, `set_switching`, 250 ms rest against a barrier on Linux), and reverse scrolling per computer. `reverse_scroll` is on the peer record, in `set_peer` and in the snapshot; the Linux daemon turns that peer's scroll around, and the settings window has the switch. The Mac injector and Mac UI pick it up in Phase 2.
  - Clipboard on Linux: done, merged into `linux-edge-sending`. The computer the pointer leaves sends its clipboard to the one it enters, as SPEC.md's Clipboard section says. `[clipboard] share` and the Share Clipboard switch (`set_clipboard`) turn it on for one computer. The GNOME extension reads and writes the clipboard through `St.Clipboard` in two D-Bus methods only the agent may call, and shows the over-limit notice. The agent carries clips to the service as base64 on its own stream, which alone allows messages that large. The Mac side (`NSPasteboard` and its switch) is pending.
- **Acceptance:**
  - Text and PNG copy/paste work both ways.
  - A 5 MiB clip is refused with a notice, without disconnecting.
  - The pointer walks A, B, C and back.
- **Size:** 1500 to 2200 LOC.
- **Risks:** Wayland clipboard access; clipboard loops.

### Phase 5 (later)

- Portal sender for KDE and GNOME 51+.
- Linux laptop touchpad as the sender.
- Mac login window.

### Order

1. Phase 0 first: it is tiny and removes the one-way defaults.
2. Then Phase 1: it is the first thing asked for, and it makes new features land once, in shared code.
3. Alongside Phase 1: a throwaway injection spike on macOS 27 **(Mac)**. It is the biggest unknown and touches none of Phase 1's files. Done 2026-09-28 as spike I: passed.
4. Then Phases 2, 3 and 4. Most of Phase 3 can be built on Linux before Phase 2 lands, but its acceptance needs a Mac that receives.

Move shared code a piece at a time: the endpoint in Phase 2, the handoff client in Phase 3. The Linux orchestration is `daemon.rs` at 1969 lines. The Mac side is `native.rs`, `link.rs`, `sharing.rs` and `macos/mod.rs`, 3437 lines together. How much they overlap is not measured, so no big-bang rewrite.
