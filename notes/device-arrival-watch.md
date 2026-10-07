# Device-arrival triggered refresh

**Status:** implemented (1.6.1: heartbeat + watcher seam + Windows/Linux watchers;
macOS heartbeat-only).
**Problem:** [hot-enumeration-freeze](hot-enumeration-freeze.md) —
while input is hot, `refresh_devices()` is deferred forever and service cold
polls are gated off, so a newly-connected pad is undiscoverable until input
goes cold (66s blind window observed 2026-10-07).
**Rule:** the watcher only *triggers* a refresh; enumeration + session gates
still decide truth. False positives cost one refresh (~8–10ms); misses fall
back to existing behavior. Nothing about battery/lightbar/toast logic changes.

## Shape (cross-platform seam)

```rust
// src/platform/device_watch.rs (new, cfg-gated inside)
/// Spawn the OS arrival/removal watcher. `on_event` fires on device
/// arrival *and* removal; implementations debounce bursts internally.
/// Platforms without a watcher compile to a no-op returning `false`.
pub fn spawn_arrival_watcher(on_event: impl Fn() + Send + 'static) -> bool;
```

- Worker owns integration: `HidWorkerHandle::start` spawns the watcher with
  `move || handle.notify_device_list_changed()`.
- New `HidCmd::RefreshPresence`: handler runs `cache.refresh_device_list()`
  only (no battery reads, no handle drops), throttled to ≥1s since the last
  refresh. Next `sample_all_inputs` opens newcomers from the fresh list via
  the existing `ensure_all_pads` best-loop; `presence_paths` update flows to
  the app/service ticks → `membership_changed` → incremental Poll.
- Thread lifetime: detached, `OnceLock`-guarded (precedent: hitch hotkey
  worker in `app/mod.rs`). Process-lifetime thread is acceptable for a tray
  daemon; document it.

## Per-OS watcher

- **Windows: `CM_Register_Notification`** (not `RegisterDeviceNotificationW` —
  the latter needs a message-only window + pump; CM_ takes a plain callback,
  no window). Filter `CM_NOTIFY_FILTER_DeviceInterface` with
  `GUID_DEVINTERFACE_HID` (`4D1E55B2-F16F-11CF-88CB-001111000030` — the GUID
  already visible in our HID paths). Fire on `CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL`
  and `...REMOVAL`. Via the existing `windows` 0.61 dependency (add
  `Win32_Devices_DeviceAndDriverInstallation` feature), following the
  `windows`-crate style used by the clock (`GetTimeFormatW`), not new hand FFI.
- **Linux: std-only `/dev` snapshot watcher** (`device_watch::linux`, no libudev
  dependency — keeps the install requirements unchanged). A thread snapshots
  `hidraw*`/`event*` nodes every 1s and fires on change: ~1s precision with
  zero HID I/O. The pure pieces (`node_relevant`, debounce) are unit-tested on
  every platform; the Linux thread glue is compile-unverified on this
  Windows-only checkout/CI — keep it to std-only calls.
- **macOS: heartbeat only.** IOKit matching notifications need a CFRunLoop +

## Universal heartbeat (the cross-platform core)

Implemented as `HOT_ENUM_MAX_DEFER` (20s) in `worker.rs` `ensure_all_pads`:
the hot deferral is skipped once the last *actual* refresh (`last_refresh_at`,
which deferrals do not bump) is older than the cap. Pure Rust, all three OSes,
no new dependencies, cadence logic unit-tested (`refresh_overdue_at`). This
alone bounds the observed 66s freeze to ≤20s everywhere; OS watchers shrink
detection to ~instant on Windows/Linux. `HidCmd::RefreshPresence` (list-only,
≥1s throttle) is the watcher entry point; sampling opens newcomers and session
ticks pick up membership — no other logic changed.

## Edges

- **Arrival storms:** BT pairing/USB+BT dual interfaces produce bursts.
  Debounce in the watcher (leading edge + 1s quiet) AND the worker ≥1s
  refresh throttle; refresh itself is idempotent.
- **Removal:** watcher fires on removal too; the existing presence-empty fast
  path (`sync_lightbar_claims(empty)` + immediate tray clear) is unchanged.
- **Suspend/resume:** resume bursts collapse via the same debounce.
- **Steam exclusive:** unchanged semantics — a Steam-grabbed pad still fails
  reads and stays undiscovered; the watcher only makes us *look* sooner.
- **Idle cost:** nothing. No watcher thread wakes unless hardware changes;
  heartbeat only shortens an already-scheduled check.

## Validation

- Logs: `hidapi_refresh end` must appear within ~1s of a physical connect
  while hot (Start open); previously the marker was `deferred` runs with no
  `end`. Watch `open caller=sample` latency after arrival.
- Matrix addition (extends [hot-enumeration-freeze](hot-enumeration-freeze.md)):
  Start open + stable pad 1 + connect pad 2, per OS, with Steam running and
  without. Time from PS-button press to `Connected` toast.
- Unit tests: debounce pure fn, `RefreshPresence` throttle, command plumbing.
  Arrival itself stays manual-test-only (no fake-HID harness in the tree).

## Staging

1. ~~Heartbeat cap (worker-only, all platforms, no deps).~~ Done (`HOT_ENUM_MAX_DEFER`).
2. ~~`HidCmd::RefreshPresence` + `spawn_arrival_watcher` seam (no-op everywhere).~~ Done.
3. ~~Windows `CM_Register_Notification` watcher.~~ Done (needs runtime check: `device watch: OS arrival watcher armed` in app.log + arrival latency).
4. ~~Linux std-only `/dev` watcher.~~ Done, compile-unverified here — needs a Linux run.
5. Re-measure blind window per OS; macOS IOKit only if needed.
