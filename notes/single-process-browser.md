# Single-process Browser (add-on split reverted)

**Status:** decision (2026-11-26). **When to read:** before proposing a Browser
process split, an app protocol, or Home-managed app packages; when input
latency or Browser launch time regresses.

## Decision

The Browser runs **in-process** with the shell (`sdsc-utils`). The optional CEF
engine DLC remains the only optional payload.

The `sdsc-browser` add-on process experiment (D1–D3 on `browser/main`) was
implemented and then reverted in full:

- removed `src/bin/sdsc-browser.rs`, `src/browser/server.rs`,
  `src/app/browser_remote.rs`, `src/browser/addon_install.rs`,
  `src/app/addons.rs`, and `crates/browser-protocol/` (transport + framing);
- removed Home add-on install/update/uninstall and the remote protocol/install
  QA scripts;
- restored the in-process Browser window, CEF engine ownership, pad routing,
  cursor, text session, and replay from the pre-split tree;
- kept the single-tab Browser (now with a one-row Back/URL/Forward header),
  system OSK, hosted contract/action bindings, Home overlay, CEF engine DLC,
  and in-process replay/tracing.

## Why it was reverted

- **Cold launch:** 1.2 s to `Ready` in a debug trace vs effectively instant
  pre-split (CEF was spawned at boot). Launcher↔Browser swaps destroyed the
  child and CEF, so every swap paid it again.
- **Input path:** the host forwarded raw HID edges at ~650/s. The child had an
  unbounded, non-conflating queue and ran CEF mouse moves per edge (pre-split:
  once per 60 Hz frame). Cursor physics used arrival time, so a backlog
  distorted `dt`.
- **Lifecycle:** blocking up-to-2 s shutdown on the UI thread; no broken-pipe
  flag (warning per pad edge); no launch timeout; no parent-death/job-object
  handling (takeover/crash could orphan the child); the child had no logger and
  `stderr` was `Stdio::null()`.
- **Packaging/versioning:** add-on copy + `addon.json` protocol gate + engine
  chain + Home install/update UI, all before the MSIX/Store optional-package
  path had landed.
- **Two UI/GPU stacks:** a second iced/wgpu device and window for no product
  benefit while the Browser is a first-party app.

The input stutter that prompted the review was **not** IPC: pre- and post-split
traces both show a ~70 ms `hidapi.refresh_devices()` stall every ~6 s causing
~80–125 ms pad-edge gaps. `HidCmd::Poll` refreshes the device list even while
input is hot. Fix that independently.

## Follow-ups (still open)

- `HidCmd::Poll` should use the cached list (or defer `refresh_devices` while
  hot); the arrival watcher plus `HOT_ENUM_MAX_DEFER` already cover discovery.
- Debug perf is dominated by instrumentation: debug defaults to `Full` tracing
  (~60 MB per 10 min with the Browser open) and `hid_trace` opens/appends/closes
  `hid-trace.log` for every HID read. Measure release, and gate `hid_trace`
  behind an explicit diagnostic switch.

## Do not regress

- Browser window/CEF/cursor/text session stay in the shell process.
- OSK stays a system service; semantic actions stay OS-owned.
- Engine DLC install/update/uninstall and the Home Browser tile stay as they are.
- Do not reintroduce a Browser process/IPC boundary without a committed
  packaging requirement and a plan for the input, launch, and lifecycle costs
  above.
