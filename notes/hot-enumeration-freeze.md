# Hot enumeration freeze hides second-pad connects (open)

**Status:** open (latent, pre-existing — NOT introduced by the incremental-poll work).
**When to read:** a pad connected while Start is open / input is hot never appears
(no toast, no tray, no sample opens) until input goes cold. Index: [README.md](README.md).

## Repro (2026-10-07 session, `app.log`/`hid-trace.log` 13:2x, epoch base 17913327xx)

- Pad 1 (low, `444648164f65`, 15%) connects → 0→1 presence + `Connected` at
  `:2715`, Start opens and stays visible until `:2786`.
- Pad 2 (`444648156926`, charged) is connected afterwards but never appears:
  no `Connected` toast, no tray change, no `open caller=sample|poll` for its
  path (`8&15b6e16c`) until `:2784`.
- `:2718`–`:2784`: **zero** successful `hidapi_refresh` — every 3s logs
  `hid-diag: hidapi_refresh deferred input_hot=1 open=1`, and no `cmd=Poll`
  runs (`:2715` → `:2788`, 73s gap).
- `:2784`: a 71ms refresh (slow = real hardware-list change) breaks the freeze;
  both paths are sample-opened within 50ms. But pad 2's reads never succeed
  (sample timeouts → `live silence`; poll full-probe open-ok + 962ms battery
  timeout), pad 1 goes silent at the same time, presence 1→0 at `:2786`, exit
  at `:2795`. `steam=1` the whole session (possible exclusive-grab confounder
  for pad 2's read starvation).

## Mechanism (both halves pre-date the incremental poll; verified in HEAD)

1. `worker.rs` `ensure_all_pads`: while `input_hot && !devices.is_empty()`,
   `refresh_devices()` is skipped and the cadence clock is bumped, so the
   refresh never goes due while sampling runs hot.
2. `service/mod.rs` cold path: `else if !nav_priority` — battery/liveness
   polls (the only other refresh callers) stop while Start owns input.
   The hot path only applies on live-list *changes*, so a stable pad
   produces no polls either.

Net: with Start open and one stable pad, the device list is frozen indefinitely.
New arrivals are undiscoverable until input goes cold. Chicken-and-egg: without
enumerating, nothing can notice the arrival.

## Audit of the uncommitted incremental-poll changes (this incident)

File-by-file, none of them gate discovery, and the log corroborates:

- `worker.rs` `poll_incremental`: pad 2 was never cached/live, so it always
  took the full-probe branch (open + resolve + 6-attempt read) — identical to
  the old full poll. The `:2788` probe behaved exactly like old code.
- Reuse path / `read_battery_fast`: only for cached + stale pads. Never
  touched pad 2. The `io_ms=0` polls prove the fast path, but no poll ran
  during the freeze anyway.
- Conditional lightbar (`lightbar_reassert_due`, 30s backstop): display-only;
  cannot affect enumeration, opens, or reads.
- 2s wall-time hold (committed in `7d69d7f`): only delays *drops*
  (`hold pad across missed read` spam after the `:1841` intentional power-off,
  2s-late Disconnect for pad 1 at `:2786`). Never suppresses arrivals.

Visible-but-benign fingerprints of the new code in this log: `io_ms=0` polls,
repeated hold lines, rare `caller=lightbar` writes.

## Open questions (need a re-repro with wall-clock notes)

- When was pad 2's PS button pressed relative to `:2784`? Arrival-at-`:2784`
  (71ms refresh) vs connected-much-earlier decides between "freeze hid it for
  ~60s" and "reads starved for ~11s".
- Was Steam's controller support holding pad 2 (exclusive open ok + read
  starvation fits `steam=1`)?
- Did pad 2's player LED ever light (BT link up) vs blink (pairing)?

## Grep tokens

- `hidapi_refresh deferred input_hot=1` — freeze active (runs of these with no
  `hidapi_refresh end` between = blind window; correlate with missing 0→1)
- `cmd=Poll` gaps — with Start open, gaps ≫5s are expected (hot path owns
  updates); do NOT read them as idleness
- `open caller=sample` for a path with no prior `hidapi_refresh end` within
  ~3s = list was stale when the pad arrived

## Lessons (test matrix)

- Manual matrix must include: Start OPEN + stable pad 1 + connect pad 2.
  Before this incident every two-pad test started from tray-idle (cold), where
  enumeration runs at 500ms and discovery works.
- Any poll/sampling change must be validated against a frozen-enumeration
  session, not just the cold path: check `hidapi_refresh deferred` runs and
  second-pad discovery latency, not just `cmd=Poll` timings.
- When a hitch involves discovery, note the wall-clock time of the physical
  button press — logs alone cannot distinguish "invisible to enumeration"
  from "enumerated but unreadable".

## Fix directions (NOT implemented)

- Slow heartbeat refresh while hot (e.g. force `refresh_devices()` every
  15–30s even when deferred) — bounds the blind window without putting a
  multi-second enum on the 4ms sample path.
- Proper arrival trigger: `WM_DEVICECHANGE` → refresh (Windows notifies HID
  arrival; no polling needed).
- Surface staleness: trace `presence_paths` age so logs show the list is
  frozen instead of implying idleness.

## Follow-up: power-off "wrong pad" verdict (2026-10-07, nicknamed run)

Two consecutive runs accused power-off of hitting the wrong pad. Log forensics
clears the pipeline at every observable layer, twice:

- Commands carried the intended serials (`…56926` then `…64f65`), sent via
  each pad's own live HID path, followed by correctly-nicknamed toasts
  (`high-charge Disconnected 85%`, then `low-charge Disconnected 15%`).
- The allegedly-wrongly-killed low pad kept streaming sample reports *after*
  the first command (reads at :5359 post-`:5352`); each pad went dark exactly
  at its own command. Rows, `KnownControllers`, nicknames, toasts, and the
  power-off candidate selection are all serial-keyed — no swap point exists.
- Confounders that explain the perception: identical hardware, and (first
  run) no nicknames to disambiguate.

Remaining REAL (minor) findings from the same logs, not yet fixed:

- **Power-off `unknown` fallback can cross-kill (latent, code inspection):**
  `power_off_bluetooth_unlocked_timed` appends `unknown`-identity devices to
  the candidates even when exact matches exist; if every matched send fails it
  fires at a stranger's interface. Never observed (both runs matched first).
  Fix (implemented 1.6.1): fall back to `unknown` only when `matched` is empty.
- **Ghost-path open churn (implemented 1.6.1):** after pad 2's power-off, its
  HID path lingered in enumeration while un-openable (`open_device: file not
  found`, ~9.4k failed `open caller=sample` across both trace files). Paths
  now back off after 3 consecutive open failures (5s window) in the sampling
  open loop; records clear on success or when the path leaves enumeration.
- **Pad-1 input silences persist with haptics off** (4× this run, all held, no
  toasts): link-level on a ~15% pad, not our output contention. The 200ms
  last-reading reuse stays as is (longer = stuck-button risk).

## Follow-up incidents (2026-10-07, arrival-watch build running)

### A. Stale miss-streak drops instantly (OUR BUG, in the 2s wall-time hold)

`miss_since` is only maintained inside `on_poll_result`, but the service hot
path skips `on_poll_result` when the live snapshot is *equivalent* to the
session — and a held pad's clone is byte-identical to the live original, so a
recovered pad never clears its streak. Next stall reuses the old timestamp and
drops instantly with no hold lines. Log proof (`app.log` 13:3x): holds for
`…64f65` at :3838–40 (episode 1, recovered, no toast — hold working as
designed), then silence at :3874 → immediate `Disconnected` with zero hold
lines, `Connected` again at :3879. The drop tick itself logs nothing (held is
empty by construction), which made this look like a skipped hold rather than
a stale one.
Fix (implemented 1.6.1): clear streaks for seen pads on *every* ingestion,
not just on change — `DeviceSession::mark_seen(live_serials)` runs each hot
loop (and cold poll) before the equivalence gate.

### B. Launch-quiet suppression fires hours late (PRE-EXISTING hole)

`LaunchPaths` seeds once and only shrinks; a pad present-but-unreadable at
worker start keeps its path `pending` forever (powered-off BT pads linger in
Windows enumeration, so `retain` never drops it). The first sampling take
queues its serial into `NotifyTracker.launch_quiet`, where it sits until the
pad's first *session* appearance — power cycles later — wrongly suppressing
that `Connected` toast. Log proof: `connect suppressed (present at launch)
serial=444648156926` at :3806 for a pad powered off at :1841 and reconnected
~:3777. The incremental poll's extra `take_serial_for_path` calls only
duplicate the long-standing sampling take; the hole predates them.
Fix (implemented 1.6.1): `launch_quiet` retains only serials present in the
current snapshot — absence purges the suppression, so a much-later return
toasts normally while genuine launch presence still suppresses.
