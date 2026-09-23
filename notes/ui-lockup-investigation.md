# UI lock-up investigation (2026-09-23)

Hand-off notes for a follow-up fix. Investigation only — no code changes for this issue yet.

## Symptom

Occasional / “random” start-screen UI lock-up while using DualSense nav. Feels like pad-driven UI freezes briefly.

## Log location

- Active: `%APPDATA%\sdsc-utils\app.log` (rotated: `app.log.1`)
- Legacy / unused: `%APPDATA%\ps5-battery-display\app.log`, `%APPDATA%\dualsense-battery-indicators\app.log`
- Logger: [`src/app_log.rs`](../src/app_log.rs) (unix epoch seconds timestamps)

No `ERROR` lines observed in current logs. This is not a panic / hard crash.

## Smoking gun

Every hid-worker **battery/liveness `Poll`** clears the start-nav input snapshot, then blocks the worker on HID I/O. Start UI then sees an empty snapshot until the next input sample.

### Code

[`src/hid_worker.rs`](../src/hid_worker.rs) — `HidCmd::Poll`:

```rust
cache.drop_all();
publish_snapshot(snapshot, Vec::new()); // wipes pad samples
let (result, timing) = poll::poll_controllers_timed(&previously);
```

`PowerOff` also clears the snapshot the same way.

Worker is single-threaded: while Poll/Identify/etc. runs, `sample_all_inputs` does not run.

### UI side

[`src/app.rs`](../src/app.rs) `on_pad_poll` → `start_input::read_nav_readings()`:

- Empty snapshot → `NavReadingsOutcome::Missing`
- With start window open: logs warn and returns without handling pad input
- Warn text: `start-nav: no hid-worker input snapshot yet; keyboard/mouse still work`

So keyboard/mouse should still work; pad nav goes dead for the Poll window.

## Evidence from logs

- **~303 / 313** “no hid-worker input snapshot yet” warns sit next to a `hid-worker: cmd=Poll` (same second / adjacent lines).
- Liveness refresh cadence: [`LIVENESS_INTERVAL`](../src/poll.rs) = **5s** when controllers are present → hitch is periodic, feels “random” during use.
- Typical Poll: `total_ms≈8–15`. Spikes stretch the dead window:
  - `open_ms` up to **~450–550**
  - `io_ms` often **~200–230**
- Identify usually `total_ms≈1700` (expected flash sequence). One outlier: `enumerate_ms=5084`, `total_ms=6719`.
- Latest session example: Poll at `T` → missing warn at `T` → nav resumes ~1s later after next sample.

## Cadence context

- Presence / refresh triggers in [`App`](../src/app.rs) call `request_refresh` → `worker.poll(...)`.
- Battery interval 60s; liveness **5s**; empty-presence retry faster.
- Comment in app already notes controller-row rebuilds can hitch the UI thread mid-slide (separate from snapshot wipe).

## Recommended fix (for next agent)

1. **Do not clear the input snapshot on Poll** — keep last good `NavReading`s until fresh samples are published after poll (or after cache rebuild).
2. Prefer not calling `cache.drop_all()` on every Poll if input handles can stay warm; if drop is required for correct battery membership, still leave snapshot intact during the gap.
3. Same for PowerOff if a brief empty snapshot is undesirable (or republish quickly after).
4. Optional: stop treating transient empty-after-Poll as a warn (or rate-limit) once (1) is fixed.
5. Out of scope unless still broken after (1): full mouse/window freeze (process enum on UI thread in `refresh_running_badge` / `process_match`) — logs do not show that path clearly; warn text implies mouse kept working.

## Suggested verify

- Open start screen, use pad continuously for >30s.
- Confirm no `no hid-worker input snapshot yet` lined up with each `cmd=Poll`.
- Confirm no multi-hundred-ms pad dead zones on slow Polls.
- Identify / lightbar / power-off still work.
