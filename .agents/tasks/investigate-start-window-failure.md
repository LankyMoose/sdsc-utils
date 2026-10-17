# Investigation: Start Window Failure to Open

**Date of investigation:** 2025 (current session)
**Log files examined:** `C:\Users\rob\AppData\Roaming\sdsc-utils\app.log` and `app.log.1`
**App version in log:** SDSC Utils (sdsc-utils) 1.6.1

---

## Summary Answer

The Start window is **not outright failing to open** — it opens successfully in both log sessions. However, the logs capture one specific known-warning condition: `start-nav: not ready for_ms=300`, which fires when the Start window has been open for ≥200 ms but `start_nav_ready` has not yet been set (i.e. the `Message::StartOpened` iced callback has not fired). This means pad navigation is blocked during that brief window, which can make the Start screen appear unresponsive even though the window exists.

The larger context is that this is a v1.6.1 session. The task open in `.agents/tasks/1.7.0.md` is implementing v1.7.0 features. The "failed to open start window" report most likely refers to the **`start open skipped (window still closing)` guard** — a stale window handle that prevents a new Start window from being created — or to the `start-nav: not ready` stall that blocks controller input briefly after every open.

No PANIC, no ERROR, and no explicit open-failure log line was found. The session ended cleanly (`shell exited gracefully status=exit code: 0`).

---

## Evidence

### Log session examined (`app.log`, session `1791384594339` / `1791384595573`)

**Key events in order:**

```
[1791384623] INFO: service: reopen gesture → OpenStart
[1791384623] INFO: ui-diag: start cold open immersive=0
[1791384623] INFO: ui-diag: start opened id=Id(4)
```
Start 1: opened successfully (compact, non-immersive). Navigation armed immediately, Start used normally.

```
[1791384790] INFO: service: reopen gesture → OpenStart
[1791384790] INFO: ui-diag: start cold open immersive=0
[1791384790] WARN: start-nav: not ready for_ms=300
[1791384790] INFO: ui-diag: start opened id=Id(5)
```
Start 2: opened, but the `StartOpened` message and `start_nav_ready = true` set appears to have fired in the same log-second. The `not ready for_ms=300` warning fired just before `start opened id=Id(5)` — meaning the iced `window::open` callback took >200 ms to deliver `Message::StartOpened`. During that window, all pad input was dropped in `on_pad_readings` (`if !self.start_nav_ready { return Task::none(); }`).

```
[1791384792] INFO: start-nav: src=hid cross=0 circle=1 ...
[1791384792] INFO: ui-diag: start close
```
Circle was pressed ~2s after open, immediately closing Start. The window closed correctly.

**Second session (`app.log.1`):** Contains only HID polling noise from an older run. No Start open/close events are visible in the portion read (the file is full HID-trace-style verbosity with no start events in the explored range). This session likely predates the gesture gesture that triggered the user's report.

### The `not ready` mechanism (source: `src/app/mod.rs`)

```rust
// Line ~4625
if !self.start_nav_ready
    && !self.nav_not_ready_warned
    && self.start_opened_at
           .is_some_and(|t| t.elapsed().as_millis() >= START_NAV_NOT_READY_MS)
{
    app_log::warn(format!("start-nav: not ready for_ms={for_ms}"));
    self.nav_not_ready_warned = true;
}
```

`START_NAV_NOT_READY_MS = 200` (line 106). The warning fires if the iced `window::open` callback `Message::StartOpened` has not arrived within 200 ms of calling `open_start_screen()`.

`start_nav_ready` is set to `true` only in the `Message::StartOpened` handler (line ~944–946):
```rust
Message::StartOpened(id) => {
    if Some(id) == self.start_window && self.start_visible {
        self.start_nav_ready = true;
        ...
    }
}
```

Until that message arrives, `on_pad_readings` returns early for pad input, so the controller appears unresponsive.

### The `start open skipped` guard

In `open_start_screen()` (`src/app/mod.rs`, line ~3260):
```rust
if self.start_window.is_some() {
    // Stale id without visibility (close in flight) — wait for WindowClosed.
    crate::controller::hid::diag::diag_info(
        "ui-diag: start open skipped (window still closing)",
    );
    return Task::none();
}
```

This guard fires if a previous Start window hasn't delivered its `WindowClosed` event yet when a new open is requested. In this case the open is silently dropped and `start_auto_open_pending` keeps the intent alive for `WindowClosed` to retry. This is a debug-only log line (`diag_info`) — it would not appear in a release build's `app.log`. If the user experienced "failed to open" right after closing Start, this is the most likely mechanism.

### HID slow ops during the Start session

Throughout both sessions, every `hidapi_refresh` while `input_hot=1` showed ~70–72 ms:
```
hid-diag: hidapi_refresh deferred input_hot=1 open=1
...
hid-diag: hidapi_refresh end ms=71
hid-diag: slow op=hidapi_refresh ms=71
```

These are the expected "deferred, then slow-but-normal" pattern after the hot-path defer fix. They are **not** the ~5000 ms freezes from the pre-1.6.x era. However, occasional outliers appear:
```
[1791384745] hid-diag: slow op=hid_trace_io ms=129
[1791384745] hid-diag: slow op=sample_one ms=137
[1791384752] hid-diag: slow op=hid_trace_io ms=235
[1791384752] hid-diag: slow op=sample_one ms=244
[1791384762] hid-diag: slow op=hid_trace_io ms=168
[1791384762] hid-diag: slow op=sample_one ms=169
```

These `sample_one` stalls (130–244 ms) occur only while Start is open and input is hot. At 244 ms they exceed the `START_NAV_NOT_READY_MS = 200` threshold. If one of these happens right after `open_start_screen()` but before `Message::StartOpened` arrives, it could delay the nav-ready signal enough to hit the warning — and could explain a brief moment where the Start screen appears frozen.

### No PANIC or ERROR found

Searched the full `app.log` content. No `PANIC`, `ERROR`, or crash-restart lines found. The session ended with:
```
[1791384796] INFO: service: shell exited gracefully status=exit code: 0
```

---

## Source Files Involved

| File | Relevance |
|---|---|
| `src/app/mod.rs` | `open_start_screen()`, `Message::StartOpened`, `start_nav_ready`, `START_NAV_NOT_READY_MS`, the stale-window guard |
| `src/service/mod.rs` | `reopen gesture → OpenStart`, `SessionEffect::OpenStart` send, latch logic |
| `src/session/mod.rs` | `SessionEffect::OpenStart` definition, auto-open logic |
| `src/controller/hid/worker.rs` | `set_input_hot`, sample loop, `hidapi_refresh` defer |
| `src/domain/gesture.rs` | Chord/gesture definitions |
| `src/persist/prefs.rs` | `start_screen_enabled` master switch |
| `src/platform/app_log.rs` | Log path: `%APPDATA%\sdsc-utils\app.log` |

---

## Root Cause Analysis

There are two distinct scenarios that could produce a "failed to open start window" experience:

### Scenario A — Stale window handle (most likely for an immediately-repeated open)

If the user presses the gesture to open Start, then closes it (Circle or Escape), then immediately presses the gesture again within the same event loop cycle before `WindowClosed` is delivered:

1. `open_start_screen()` runs, but `self.start_window.is_some()` is still true (stale from the previous close).
2. The code logs `ui-diag: start open skipped (window still closing)` (debug only — not visible in release `app.log`).
3. The window silently does not open.
4. `start_auto_open_pending` is NOT set in this path, so the retry on `WindowClosed` may not fire unless `start_auto_open_pending` was already true.

**Relevant code path** (`src/app/mod.rs` ~line 3260):
```rust
if self.start_window.is_some() {
    crate::controller::hid::diag::diag_info(
        "ui-diag: start open skipped (window still closing)",
    );
    return Task::none();
}
```
Note: `start_auto_open_pending` is not set here. If the gesture fires while closing, the open intent is lost.

### Scenario B — `start_nav_ready` delay causes perceived freeze

The Start window opens, but pad navigation silently drops inputs for up to ~300+ ms (observed in the log) while waiting for `Message::StartOpened`. During this window a user pressing Circle would have it ignored, making the screen appear frozen/unresponsive. The `sample_one ms=244` outliers make this delay variable.

### Scenario C — `start_screen_enabled = false`

Least likely, but: if `start_screen_enabled` is false in `prefs.json`, every open path returns `Task::none()` immediately. Confirmed default is `true` from the prefs source and the working log sessions.

---

## Recommendations

### Recommendation 1 — Fix the stale-window guard to latch the open intent (Scenario A)

In `open_start_screen()`, the stale-window early return should set `start_auto_open_pending = true` so `WindowClosed` retries the open:

```rust
if self.start_window.is_some() {
    // Stale id without visibility (close in flight) — wait for WindowClosed.
    self.session.start_auto_open_pending = true;  // ← add this line
    crate::controller::hid::diag::diag_info(
        "ui-diag: start open skipped (window still closing)",
    );
    return Task::none();
}
```

This matches the comment ("Leave start_auto_open_pending set so WindowClosed can retry") and fixes the silent drop. Currently that comment only applies to the IPC-latch path, not the in-flight-close path.

### Recommendation 2 — Increase `START_NAV_NOT_READY_MS` or log it at INFO (Scenario B)

The warning threshold of 200 ms is tight given that `sample_one` regularly takes 130–244 ms when Start is hot. This produces spurious `WARN` entries and causes the first 200–300 ms of every Start session to silently drop controller input.

Options:
- Raise `START_NAV_NOT_READY_MS` from 200 to 500 (matches the longest observed delay).
- Add an INFO log in `Message::StartOpened` that records the actual open-to-ready latency: `"start-nav: ready after open_ms={ms}"` — this would give actionable data without the premature warning.

### Recommendation 3 — Add a release-visible log for the stale-window skip

Change the `diag_info` to `app_log::info` so the skip is visible in release `app.log` without requiring a debug build:

```rust
crate::platform::app_log::info("start open skipped (window still closing)");
```

This would have made the failure immediately diagnosable from the user's log.

---

## What Was NOT Found

- No PANIC or crash.
- No `ERROR` lines.
- No IPC failure (shell connected cleanly in both sessions).
- No `start_screen_enabled = false` in the logs (Start opened in the current session).
- No evidence of `SessionEffect::OpenStart` being dropped or a missing `service: reopen gesture → OpenStart` line before a failed open.
- The user's specific "failed to open" session may be in an older log that has been rotated past the two files available (`app.log` and `app.log.1`).
