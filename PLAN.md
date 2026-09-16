# Plan: cursor transition latency and weirdness

Date: 2026-09-14. Source: review of crossing 9 in the Mac debug log plus the
code on `main` at `57fba3c`.

## What the log showed

| Phase | Measured |
|---|---|
| QUIC connect + session negotiate | 9 ms |
| Gap before "desktop preparation starting" | 15 ms |
| Prepare round trip to Ubuntu | 25 ms |
| Edge hit to native capture start | 51 ms (GUI notices at 66 ms) |
| Cursor drift on the Mac during that window | 42 px |
| Return polls while on Ubuntu | 1288 in 51 s, about 25/s, 21 ms each |
| Barrier hit to edge sharing rearmed | about 130 ms |

## Current status, September 16

The recorded latency fixes preceded the native interface migration. macOS now
uses SwiftUI with a dedicated Rust application worker; Linux uses the headless
desktop agent. Live two-computer qualification and Finding 2 Step B remain
outstanding. The measured table above and findings below refer to the reviewed
September 14 revision. Source paths under `src/gui/` in those notes have moved
to `src/app/`. Current build and test commands are in README.md and TESTPLAN.md.

## Historical September 14 execution procedure

The following staging, commit, and remote-checkout instructions record that
session's procedure and do not authorize those actions in a later task.

1. One finding at a time. Write a self-contained task (Codex cannot see the
   chat): files, contract, what must not change, gates as exact commands.
2. Hand it off to a scoped worker in this Codex task. Keep task files under
   `target/plan-tasks/` and review the worker changes before staging.
3. Review the full diff and rerun the gates. This Codex session has unrestricted
   filesystem and socket access; socket tests can run here.
4. Mac gates: `cargo fmt --all -- --check`, `cargo clippy --locked
   --all-targets --all-features -- -D warnings`, `cargo test --locked
   --all-targets --all-features`, `node tests/gnome_desktop_test.mjs`,
   `git diff --check`.
5. Linux gates (daemon and linux modules do not compile on the Mac): rsync
   the tree to the review checkout on the Ubuntu box, then clippy and test
   there. The review checkout is separate from `~/dev/zflow`, which has
   uncommitted work.
   ```
   rsync -a --delete --exclude target --exclude .git ./ ubuntu:~/dev/zflow-review/
   ssh ubuntu 'cd ~/dev/zflow-review && PATH=$HOME/.cargo/bin:$PATH cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked --all-targets --all-features'
   ```
6. Stage the finding source/docs once it passes, keeping PLAN.md untracked. Commit
   per finding at the end, or as asked.

Order: 3 (done), 1, 2, 4, 5, then live qualification. 3 went before 1 because
the long poll keeps a request in flight almost all the time, which would have
made the teardown bug fire on every escape.

## Finding 3: escape closed the transport before Leave and Finish (done)

`run_endpoint` dropped the in-flight poll when its async block ended; the
`CancelOnDrop` guard in `SessionHandle::desktop_request` then closed the
session before `end_outbound` and `Finish` ran. Now the poll carries a pending
flag, survives the block, and is awaited (bounded by the request timeout)
after Leave and before Finish. No unit test can reach `run_endpoint` without
the native bridge; verified by the Mac gates and to be confirmed live.

## Finding 1: return-edge poll storm (implemented and staged)

Goal: a few polls per second instead of 25, no logind process spawns per
poll, return detection in about one round trip.

Implemented: extension long poll, `POLL_HOLD_MS`, Mac poll floor, and the
following changes:

- `src/daemon/desktop.rs` `serve`: poll jobs stop calling
  `spawn_blocking(query_primary_seat)` before and after the compositor call.
  Query once before the loop, refresh on the existing 250 ms interval tick,
  and feed `authorize_peer` and the `current` recheck from that stored
  `SeatState`. Prepare, finish and snapshot keep the fresh before/after
  queries and update the stored state. Keep all tracing strings.
- Log thresholds: `src/session.rs` (`desktop_request`), `src/daemon/desktop.rs`
  (`request`, `serve`), `src/gui/desktop.rs` (`call`) demote a fast poll to
  trace with `elapsed_ms >= 150`. A held poll takes about 200 ms, so use
  `POLL_HOLD_MS + 150` in all four places or every poll lands in the debug log.
- `tests/gnome_desktop_test.mjs`: polls that expect `active` must start the
  poll, `advance(200)`, then await. Add: poll pending when the barrier is hit
  resolves at once with the position; poll pending at finish or lease expiry
  rejects promptly with the expired/ended error; the hold timer is removed
  after an early resolve (extend the timer stub minimally to observe it).
- Docs: `grep -n -i poll README.md TESTPLAN.md SPEC.md`; fix any sentence that
  describes the 16 ms cadence or "fast polling" semantics.
- Chain timeouts stay as they are (450 ms D-Bus, 600/700 ms broker, 1000 ms
  session); 200 ms plus a normal round trip fits under all of them.

Acceptance: Mac and Linux gates green; live, `request_id` grows by about
300 per minute instead of 1500, Ubuntu journal shows `seat_before_ms` 0 for
polls, and a barrier hit is reported within a few ms of the previous poll.

## Finding 2: Ubuntu cursor appears where the hand was 50 ms ago (Step A staged)

The entry fraction is computed at detection (`src/gui/handoff.rs`
`crossing`, `src/gui/sharing.rs` `entry_position`) and sent unchanged in
Prepare 25 to 50 ms later.

Step A (do now):

- `src/desktop.rs` `ReturnMapping`: add the inverse of `position`, a method
  that maps a local point on the edge to the remote fraction, using the same
  arithmetic `crossing` uses today; make `crossing` call it so there is one
  copy of the formula.
- `src/macos/mod.rs` `run_endpoint`: right before building Prepare, read
  `cursor_position()`; if it is inside `entry_region`, replace
  `handoff.position` with the fraction of that point; otherwise keep the
  detection value (the entry check that follows will cancel anyway). Log
  the refreshed fraction and the displacement at debug.
- Unit tests in `src/desktop.rs`: inverse round-trips `position`, clamps at
  the segment ends, and matches the `crossing` test expectations.

Step B (decide after measuring A live): send the remaining along-edge
displacement between the Prepare point and the cursor at capture start as
one motion frame, scaled by remote span over local span. Only do this if the
residual jump is still visible; a single large relative delta goes through
libinput acceleration on the receiver, and SPEC line 307 requires a session
not to mix motion units.

Bigger option for later, not now: keep a warm QUIC session to the peer while
sharing is armed so a crossing costs only Prepare, not connect plus negotiate
plus Prepare. Touches session lifetime and receiver ownership rules.

## Finding 4: a cancelled crossing turns sharing off (implemented and staged)

Any failure of the worker, including "cursor left the configured edge" and
"release held keys", sets `enabled = false` in `src/gui/sharing.rs` `tick`
(lines around 194, 217). An overshoot or a click during the 50 ms connect
window then needs a trip to the window.

- Classify worker outcomes: a crossing cancelled by the admission checks
  (`validate_entry_position`, `capture_entry_allowed`, `input_is_neutral`,
  the GUI region check) is `Cancelled(reason)`, everything else stays
  `Failed`. Simplest shape: a typed error in `src/macos/mod.rs` that
  `run_controlled` downcasts into a new `SourceStatus::Cancelled` variant.
- `Sharing::tick`: on `Cancelled`, keep `enabled = true`, set the notice, and
  let the existing "must move away from the edge first" rule in `crossing`
  (previous.x must be more than 1 px inside) prevent an immediate retrigger.
  Geometry changes, worker panics and real failures still disable sharing.
- Keep the GUI-side region check (it cancels before Ubuntu warps its cursor)
  but it must not disable sharing.
- Unit test the classification function; no GUI test exists and none is
  needed for this.

Acceptance: overshoot during connect logs "crossing cancelled", the notice
says so, and the next clean crossing works without touching the window.

## Finding 5: rearm waits on QUIC drain and serial helper calls (AWDL staged)

After a barrier hit the Mac cursor is back in 1 ms but the watcher rearms
after Finish (25 ms), a 25 ms gap, `wait_idle` (66 ms) and a GUI tick.

- First verify in `src/daemon.rs` what happens when the same peer opens a
  new session while the previous one has not yet reported `Closed`
  (`remove_session_if_current`, `allows_session`, "Another computer owns
  input"). If a new session replaces the old one cleanly, rearm the watcher
  right after Finish is acknowledged and let the QUIC drain finish on the
  worker thread. If not, keep the drain on the critical path and only do
  the next two items.
- AWDL (`src/macos/awdl.rs`), only relevant with "Reduce Wi-Fi latency" on:
  acquire concurrently with the QUIC connect (`tokio::join!`), release
  concurrently with the endpoint shutdown. Measure the 15 ms and 25 ms gaps
  with `just debug mac` before and after; if the switch was off and the gaps
  are the display query instead, leave AWDL alone.
- Keep the 2 s bound on `wait_idle` and the "sharing remains off" error if
  the drain times out.

Acceptance: barrier hit to "edge sharing rearmed" under 60 ms in the log; a
quick back-and-forth across the edge within 150 ms starts a new crossing.

## Finding 6: verify the edge watcher survives a hidden window (live check pending)

Crossing detection now runs every 12 ms on the Rust application worker,
independently of SwiftUI rendering. Verify with Settings closed, minimized, and
on another Space. Input capture, return, and cleanup must work in every case.

## Live qualification after 1 to 5

Run the native app with `just debug mac`, the Linux desktop agent with
`just debug linux`, and daemon diagnostics with `just debug-daemon`. Cross five
times each way, escape twice, click once during a connect, and overshoot
once. Check:

- `connection_to_capture_ms` and the expected vs current cursor at
  "checking cursor before capture" (drift should be under 25 ms worth).
- Poll `request_id` growth per minute (about 300, not 1500).
- On escape: "waiting for desktop poll before cleanup" then a clean
  "remote input release completed success=true" and no BackendUnavailable.
- Ubuntu journal: no per-poll seat checks, `seat_before_ms` 0 for polls,
  `loginctl` no longer in `top`.
- "crossing cancelled" keeps `enabled=true` in "crossing worker finished".
- Time from "receiver reported return edge" to "edge sharing rearmed".

Then update TESTPLAN.md with the measured numbers and delete this file.
