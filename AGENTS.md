# AGENTS — SDSC Utils

## Quick commands

| Action | Command |
|---|---|
| Run fmt + clippy + test + build (full CI) | `bash scripts/ci.sh` |
| Format check only | `cargo fmt --all -- --check` |
| Clippy (all targets, fail on warnings) | `cargo clippy --all-targets -- -D warnings` |
| Run all tests | `cargo test --all-targets` |
| Debug build (every binary) | `cargo build` |
| Release build | `cargo build --release` |
| Run (tray + HID + UI, one process) | `cargo run` |
| UI-only check (no engine) | `cargo check --no-default-features` |
| List connected controllers | `sdsc-utils --list-controllers` |
| Latest session action timeline | `sdsc-utils --trace-last` |
| Follow one causal flow | `sdsc-utils --trace-follow <flow-id>` |
| Replay a browser flow in a sandbox | `sdsc-utils --trace-replay <session> --flow <flow-id>` |
| Install/update QA console | `python scripts/install_qa_web.py --open` (see `notes/install-qa.md`) |
| Browser input QA (synthetic, no OS input) | `python scripts/qa_browser_input.py` |
| Browser action QA (Back/Forward/Reload) | `python scripts/qa_browser_actions.py` |
| Browser popup-denial QA (synthetic, no OS input) | `python scripts/qa_browser_popup.py` |
| System OSK QA (Quadrant + Grid) | `python scripts/qa_browser_osk.py` |
| Launcher Settings OSK proof QA | `python scripts/qa_launcher_osk.py` |
| Install/remove local Browser add-on | `python scripts/install_browser_addon.py` (`--remove`) |
| Home Browser install/launch/update/uninstall QA | `python scripts/qa_browser_app.py` |
| Remote Browser add-on QA (protocol + host launch) | `python scripts/qa_browser_remote.py` |
| Browser pixel QA (render smoke test) | `python scripts/qa_browser_visual.py` |

## Project structure

- **Workspace with one root package.** Root `Cargo.toml` is the `sdsc-utils` crate (library + two binaries in `src/bin/`); `crates/browser-protocol/` is the shared versioned host ↔ Browser contract (serde + framing, no UI deps).
- **`src/`** — everything lives here; see [Directory ownership](#directory-ownership) below.
- **`build.rs`** — rasterizes the DualSense SVG and embeds the `.ico` into the Windows .exe
- **`notes/`** — topic handoffs for agents (index: `notes/README.md`). Read the matching note before touching a do-not-regress area.
- **`packaging/`** — MSIX packaging; **`scripts/`** — CI checklist; **`assets/`** — icons, sounds, README screenshots.

## Branch layout

- `main` — released line.
- `browser/main` — integration line for the browser + Home work: managed-app
  Home, standalone Browser window, CEF OSR engine. Absorbed the pivot at
  `d245777` (also contains the former `browser/multi-app-suspend-spike` /
  `browser/tabs` work). Plan/handoff: `notes/browser-home-pivot.md`.
- Land changes on `browser/main`; the pivot line and the pre-split snapshot
  branch were removed once the pivot merged.


## Terminology

- **Launcher** — the fullscreen game console (`src/ui/launcher/`, `src/app/launcher.rs`). Prefs and helpers use the `launcher_*` prefix; `Start` is not a screen name.
- **Home** — the topmost PS-button overlay app switcher (`src/ui/home/`, `src/app/home.rs`). Registry type is `HomeApps`; managed-app UI identity is `ManagedApp`.
- **Default page** — the Browser's start URL (`BROWSER_DEFAULT_URL`). "Home" never refers to a browser page.

## Build / test / lint ordering

**Always: fmt → clippy → test → build** (this is the CI order and the pre-push hook order).

```bash
bash scripts/ci.sh
# or individually:
cargo fmt --all -- --check     # must pass first
cargo clippy --all-targets -- -D warnings   # must pass second
cargo test --all-targets       # must pass third
cargo build                    # refreshes target/debug/*.exe (test builds don't)
```

Missing this order can cause subtle issues (clippy warnings can fmt-reformat code, breaking the `--check` gate).

## Key binaries & entrypoints

- `src/bin/sdsc-utils.rs` — the app: tray, HID, session, and iced UI in one process (the default binary). Internal `--replace` argv lets a renderer-pref reload successor retry the single-instance guard while the parent exits; a fresh launch instead terminates the predecessor and takes over (`src/platform/prev_instance.rs`).
- `src/bin/gen_msix_logos.rs` — MSIX logo generator
- `src/bin/sdsc-browser.rs` — the Browser add-on process (protocol over stdin/stdout, own iced window + CEF). The main app launches it when installed; there is no in-process Browser window.
- Plain `cargo build` builds **all** binaries. `cargo run` runs the main app; `cargo run --bin sdsc-browser` waits for a protocol `Hello` on stdin.
- `src/lib.rs` — re-exports all internal modules (`app`, `controller`, `domain`, `games`, `persist`, `platform`, `session`, `ui`)

## Conventions & quirks

- **Battery steps**: DualSense firmware reports battery in 11 coarse steps (0–10). Percentages use the Linux mid-point mapping (step 0 → 5%, step 9 → 95%, step 10/full → 100%). See `src/controller/dualsense/battery.rs` or the README.
- **Lightbar color**: Blue → Purple → Red gradient as battery drops; reapplied on change (new pad / color-bucket flip) plus a slow ~30s backstop. Low-battery pulse (orange) when ≤5% while discharging.
- **Single instance with takeover**: the app enforces single-instance via the `single-instance` crate. A fresh tray launch terminates the running predecessor and takes over; only the internal `--replace` child waits for its gracefully-exiting parent.
- **No async runtime**: iced already runs its executor on Tokio (see the `tokio` feature in `Cargo.toml`); app code programs against iced `Task`/`Subscription` plus the dedicated HID worker thread. Do not add an explicit Tokio dependency — blocking `hidapi` I/O must stay off the UI executor (see the cutover plan discussion).
- **Windows features**: `tray-icon` builds differ per OS — root `Cargo.toml` has `[target.'cfg(windows)'.dependencies]`, `[target.'cfg(target_os = "macos")']`, `[target.'cfg(target_os = "linux")']` sections.
- **No telemetry**: All data stays local (`prefs.json`, `controllers.json`, `analytics.json`, `app.log`).
- **Git hooks** (`git config core.hooksPath .githooks`): pre-commit runs `cargo fmt --all` and re-stages staged `.rs` files; pre-push runs `scripts/ci.sh`. Bypass with `--no-verify`.
- **CI**: GitHub Actions `ci.yml` runs `bash scripts/ci.sh` on push/PR (fmt → clippy → test → build, debug + release), plus an MSRV-floor job (`cargo +1.89 check --locked --all-targets --no-default-features`) enforcing the `rust-version` claim for UI-only app code (CEF needs the pinned toolchain). CI wires `sccache` + `rust-cache` and installs CMake/Ninja for the CEF wrapper.
- **Toolchain is pinned**: `rust-toolchain.toml` names the exact compiler for local dev (rustup auto-installs it); `ci.yml` pins the same version explicitly via the dtolnay `toolchain:` input (the action defaults to floating `stable`, it does not follow the file). `rust-version` in `Cargo.toml` (1.89) is only the MSRV floor — never float or "sync" the pins to it. Bump deliberately (both pins → run `scripts/ci.sh` → commit) so new-compiler lints surface in the bump commit, not as surprise CI failures.
- **Engines**: `cef-engine` is the default feature (CEF OSR). Windows builds use the static MSVC CRT via `.cargo/config.toml` because `libcef_dll_wrapper` is `/MT`. CEF requires CMake + Ninja on `PATH`; its binary distribution downloads to `OUT_DIR` (~427 MB; set `CEF_PATH` to share). `--no-default-features` is the UI-only iteration build (Browser tile shows the engine install state; no reader fallback). The app binary delay-loads `libcef.dll` via `build.rs`, so an install with no engine still starts; the Home catalog can then install CEF for the `sdsc-browser` add-on (see `notes/install-qa.md`). Local DLC installs are versioned (`cef-engine-<stamp>` + `cef-engine.current`); uninstall drops the pointer and defers cleanup of the still-mapped directory to the next boot, so reinstall never renames onto a locked `cef-engine\`. Each install carries `engine.json`; Home fetches `SDSC_CEF_DLC_MANIFEST_URL` on open for version/changelog/update state, install/update is cancellable, and an in-place update takes effect after restart. Home Settings owns Auto open (not the Launcher drawer), the standalone app-install download cap (live during a download and shared by every managed-app installer), and UI sound/haptic prefs; `SDSC_MSIX_ENGINE_DIR` mocks the optional package for resolver/managed tests.
- **Build speed (single exe, engine default-on)**: `cef-engine` is a default feature — plain `cargo run` builds Chromium OSR. Dev profile keeps debuginfo minimal (`line-tables-only` for our code, none for deps) because link time dominates; don't raise it. For UI-only iteration use `cargo check --no-default-features` (no CEF compile). Recommended locally: `sccache` (`cargo install sccache`, `RUSTC_WRAPPER=sccache`) — first build populates, later ones hit. Batch `Cargo.toml`/profile edits: each set invalidates fingerprints once.

## Platform-specific notes

- **Windows**: `windows` / `winreg` crates under `[target.'cfg(windows)'.dependencies]`. HID access via `hidapi`. Tray via `tray-icon` (no default features).
- **macOS**: `tray-icon` (no default features). Input monitoring may be prompted by macOS.
- **Linux**: `tray-icon` with `gtk` feature. Requires `libhidapi` / udev rules for DualSense access.
- **Graphics backend**: on Windows boot sets `WGPU_BACKEND=vulkan,dx12` (Vulkan preferred, DX12 fallback) unless the env var is already set; `wgpu_diag::alpha_composite()` reports whether transparent windows will composite. See `notes/ui-refresh.md`.
- **Developer emulator**: Always compiled in non-release builds (`cfg(debug_assertions)`) — a plain `cargo run` already has it. The fleet lives in `App::emulated_pads` and publishes through the local session. Configure groups the debug tools under a **Developer mode** caption: **Controllers** (one row per emulated pad with a chevron that expands its controls inline; `EmulatorCommand` addresses pads by serial, never row index), **Pad input**, **Diagnostics**. The fleet is **rehydrated at boot** from remembered `emu-` serials (`emulate::rehydrate`, fed by `known.remembered_disconnected(&[])`) — pads persist to `controllers.json` and come back **disconnected**, so a restart never fires a connect toast. An emulated pad carries the same **remember** pin a real controller has (`EmulatedPad.remembered`); `emulate::prune` drops a pad that is *both* unplugged and forgotten, so forgetting-then-unplugging and unplugging-then-forgetting converge on the same result. `sync_remembered_emulated` (`emulate.rs`, unit-testable without an `App`) reconciles the store: remembered pads are written, forgotten ones are actively forgotten — including pads `prune` already dropped, which is why it takes `previous` as well as `current`. **Forgetting must delete the record, not merely stop writing it**, or the pad resurrects on the next boot; that was the bug in `ce85c17`. Battery edits preview and debounce by 1s *only while the pad is connected* — a disconnected pad is not published, so it applies immediately, and unplugging cancels an armed change. A pad's charge state is **derived from its link** (`emulate::state_for`: USB charges, `Complete` at 100%; Bluetooth discharges); there is no separate `SetState` command, and `set_percent`/`set_link` re-derive so the invariant holds however the pad is edited. Release builds (`cargo build --release`) compile them out; no Cargo feature or CLI flag exists. Diagnostics also has a non-persisted "Display layout bounding boxes" toggle (F9) that outlines Launcher/drawer containers (magenta) and fixed/clamped-width elements (cyan) for layout debugging. Browser cursor/screenshot diagnostics now live in the add-on process and are driven by replay (`screenshot`), not a host F10 toggle.

## Directory ownership

| Directory | Typical changes |
|---|---|
| `src/app/` | iced daemon (`App`): tray, HID worker ownership, `DeviceSession`, window lifecycle for popup / Settings / Launcher / toast / system OSK, message routing; `browser.rs` (add-on launch gate/runtime state/replay), `addons.rs` (install/update/uninstall orchestration + traces), `browser_remote.rs` (child supervisor/input bridge), `osk.rs` (system OSK session/overlay/input/text routing), `actions.rs` (semantic action bindings), `home.rs` (`HomeApps` installed/active apps + Home window lifecycle/input) |
| `src/browser/` | Browser internals: engine seam + CEF OSR worker, DOM target query, controller cursor, dormant tabs/bookmarks/history backends, stable frame texture, `server.rs` add-on process, `addon_install.rs` package copy/update metadata; no reader fallback |
| `src/hosted/` | Re-export of the shared hosted-app contract v0 (manifest/native surface/actions/text session/lifecycle); canonical types live in `crates/browser-protocol/` |
| `crates/browser-protocol/` | Versioned wire protocol + contract crate: messages, framing, in-process channel, no UI/CEF deps |
| `src/ui/osk/` | System controller keyboard: Quadrant core/layouts + Grid fallback; no Browser imports |
| `src/app/actions.rs` | OS-owned semantic action bindings (Browser L1/R1/Options) + `ActionInvoked` path |
| `src/session/` | Pure device session (no windows/HWND): presence, notify, analytics, Launcher-open policy; emits `SessionEffect`s the UI applies |
| `src/controller/` | HID polling + worker (`hid/`), DualSense protocol (`dualsense/`: battery, lightbar, rumble, input, identity), known controllers, debug-only `emulate` emulation |
| `src/domain/` | Pure logic: battery colors/spectrum math, pad gestures/chords, protocol helpers |
| `src/games/` | Steam library scan, game launch, process matching |
| `src/persist/` | `prefs.json`, `controllers.json`, `analytics.json`, `steam_library.json`, paths |
| `src/platform/` | Logging, app metadata, autostart, crash-restart, device-arrival watch, file icons, MSIX (`packaged`), UI sounds, wgpu diagnostics, Win32 helpers |
| `src/ui/` | All iced views + styling: `theme.rs`, `popup/`, `configure/` (Settings), `launcher/` (fullscreen game console), `browser/` (add-on panel state + window view/`BrowserMessage`), `home/` (PS overlay tiles/selection), `toast/`, `shader/`, `percent_ring.rs`, `layout.rs` (window placement) |
| Tests | Inline `#[cfg(test)] mod tests` next to the code (no `tests/` dir) |

## Common gotchas for agents

- **Automated UI runs:** use the install QA app on the secondary monitor with quiet placement (`SDSC_DEBUG_OPEN` + `SDSC_DEBUG_QUIET=1`, QA console `"automated": true` or `"controller_input": false`) and real input off (`SDSC_DEBUG_INPUT=0`, which now locks pad, keyboard, and mouse). Manual QA launches leave **Automated** unchecked, so they open focused on the primary (`SDSC_DEBUG_QUIET=0`). Never let physical pad/keyboard/mouse drive the app during automation, never inject OS input (`SendInput`/`SendKeys`), and never focus a QA window over a game on the primary monitor. Browser input tests use synthetic replay steps (`text`, `key`, `mouse.click`, `mouse.wheel`), which call the same handlers without touching the OS. Keep automated UI minimal: quiet sessions suppress the startup toast and controller auto-open Home, and replay closes the Browser window as soon as the plan reaches `replay.done`; kill the QA process promptly after collecting traces instead of leaving windows up.
- **Never HID-probe a controller that is actively being used.** "Active" means a non-resting input sample (button/stick/D-pad/trigger/bumper) within ~3s; its battery must come from the live input stream. `poll_incremental` skips both the cached-handle read and any full probe for active pads. Probes are for new/idle/ghost paths only, and failed paths use exponential backoff (first retry 1s, doubling to a 5-minute cap, so boot-time presence/lightbar/auto-open do not wait out a ghost hold). Preserve this in any poll/sampling change.
- **Intermittent test failure? Check shared globals before anything else.** `cargo test` runs tests on N threads in one process, so any `static`/`OnceLock`/`LazyLock` is shared. Reproduce with `--test-threads=16` in a loop rather than re-running (see `notes/flaky-tests.md`). Any test that mutates a process-global must take that global's test lock — `art_worker::shutdown()` wipes the shared `icon_cache` via `wipe_art_caches()`, and `domain::color::set_active_spectrum` swaps the process-wide `SPECTRUM`. Locks are `icon_cache::with_cache_lock` and `color::with_spectrum_lock`.
- **`cargo fmt --all -- --check` must run before `cargo clippy --all-targets -- -D warnings`**. Clippy can emit warnings that fmt would have reformatted; running fmt first avoids the "clippy fixes fmt" cycle.
- **Clippy `-D warnings` is active in CI**. Any new `#[allow(clippy::...)]` must be justified; otherwise the build will fail on PR.
- **The `single-instance` crate ensures only one run, with takeover**. A fresh launch kills the predecessor (`platform::prev_instance`) — if you add GUI windows, make sure they're owned correctly or you'll get "single instance" exits.
- **Windows .exe icon is generated from SVG at build time** via `build.rs`. If the SVG changes, the icon rebuilds automatically (rerun-if-changed is set).
- **Debug-only code is gated by `cfg(debug_assertions)`, not a Cargo feature**. The Developer emulator, HID diagnostics, and verbose logging ship in every non-release build. `scripts/ci.sh` lints *both* profiles (`--release` included) because release compiles out whole items that debug compiles — a debug-only clean build does not prove a clean release.
- **Lightbar color reassertion (change/new-pad plus a slow ~30s backstop)** means if you overwrite the lightbar via another tool (e.g., Steam Input), the app will re-apply its color. This is intentional but worth noting when debugging "stuck" colors.
- **Hot enumeration freeze**: while input is hot (`input_hot=1`, e.g. Launcher open with a stable pad), `refresh_devices()` is deferred up to `HOT_ENUM_MAX_DEFER` (20s heartbeat cap) and cold polls are gated off — plus the OS arrival watcher (Windows `CM_Register_Notification`, Linux `/dev` snapshot; see `notes/device-arrival-watch.md`) fires list-only refresh hints so newly-connected pads surface in ~1s. Pre-existing freeze documented in `notes/hot-enumeration-freeze.md`; any poll/sampling change must still be validated with a Launcher-open + second-connect test, not just tray-idle.
- **Button taps are source-side events, not reconstructed state**. `PadInterpreter` consumes ordered raw HID reports (not the merged snapshot), debounces press/release, and emits `PadAction::Tap`; `PadActionGate` dedupes UI sequence and arms pads only after rest following an owner change. Home/Browser/Launcher route these actions centrally. Do not add per-surface edge detection for face/Options/Create/L1/R1/L3/R3 taps. Analog/D-pad repeat and the Cross/Triangle hold progress meters still use the legacy per-surface bank.
- **Battery step mapping is Linux-midpoint**. Windows may report different percentages; the app maps firmware steps → percentages via the fixed mid-point formula.
- **Browser v0 is chrome-less, single-tab, and a separate process**: `sdsc-browser` owns its window/CEF/page cursor/text session; the main app owns Home/PS, HID, system OSK, and lifecycle. No address/tabs/bookmarks/history/status UI, popups denied, Home/PS is the only close path. L1/R1/Options are semantic back/forward/reload via `src/app/actions.rs`; tap only. Keep page cursor/D-pad/docking/IME intact.
- **The OSK is a system service**: `src/ui/osk/` + `src/app/osk.rs`, topmost no-activate overlay; the Browser ships no OSK UI/layout/gesture/mode code. Cross is the only opener (first Cross on a page field); real mouse/keyboard/IME/touch closes it, keyboard keys hand off (Escape consumed). Quadrant is default; Grid fallback; `OskMode` is a system pref (`SDSC_OSK_MODE=quadrant|grid` debug override).
- **Browser add-on process (default)**: the main app launches `sdsc-browser(.exe)` over stdin/stdout. Runtime lookup prefers the Home-installed `<data>/browser-addon/` copy, then a development sidecar (or `SDSC_BROWSER_ADDON_SOURCE`). Home owns install/update/uninstall: the add-on is copied from the configured source, its `addon.json` records version/size/source mtime, and updates are detected by version or file change. The engine component is chained when missing/updatable. It streams HID edges/actions, OSK text, and lifecycle; Home/OSK remain in the main app. There is no in-process fallback: `SDSC_BROWSER_REMOTE=0` disables the Browser for automation. Replay is forwarded to the add-on; `scripts/qa_browser_app.py` covers install → launch → update → launch → uninstall, and `scripts/qa_browser_remote.py` covers protocol, crash recovery, and Home pause.

## Where to find things

- **HID driver / controller state** → `src/controller/hid/` (cadence `policy.rs`, error taxonomy `retry.rs`), `src/controller/driver.rs` (driver registry), `src/controller/model.rs` (`Serial`, `Capabilities`), `src/controller/dualsense/`
- **Battery step → percentage math** → `src/controller/dualsense/battery.rs`
- **Battery color spectrum** → `src/domain/color.rs`; lightbar writes / reassert → `src/controller/dualsense/lightbar.rs`
- **Error type** → `src/app_error.rs` (`AppError` + `AppResult`)
- **Lightbar spectrum editor (Settings)** → `src/ui/configure/spectrum.rs`, `src/ui/configure/mod.rs`
- **Theme / shared widget styles** → `src/ui/theme.rs`; restyle plan → `notes/ui-refresh.md`
- **Shared face-button hints / rings** → `src/ui/face/` (`mod.rs` widgets, `ring.rs` SVG rings); used by Launcher, Launcher Settings, and Home controller dock rows
- **Shared UI sounds / haptics** → `src/app/feedback.rs` (global cue + pref application), `src/platform/ui_sound.rs` (audio worker); Home Settings owns the controls
- **Launcher / fullscreen game console** → `src/ui/launcher/` (UI), `src/games/` (Steam scan, launch); behavior → `notes/launcher-immersive.md`
- **Browser add-on (CEF)** → `src/bin/sdsc-browser.rs` + `src/browser/server.rs` (protocol/window/input), `src/browser/` (engine seam, `cef_backend.rs`, DOM/cursor, nav helpers) + `src/ui/browser/state.rs` (page surface state; dormant tab data) + `src/ui/launcher/browser_frame.rs` (texture) + `src/app/browser_remote.rs` (host supervisor) + `src/app/browser.rs` (launch gate/install/replay); system OSK → `src/ui/osk/` + `src/app/osk.rs`; semantic actions → `src/app/actions.rs`; hosted contract → `src/hosted/` + `crates/browser-protocol/`; Home → `src/ui/home/` + `src/app/home.rs`; contracts → `notes/browser-embedding.md`; CEF API map → `notes/cef-api.md`; Home/CEF pivot plan → `notes/browser-home-pivot.md`
- **Toasts** → `src/ui/toast/` (state machine in `machine.rs`); z-order / remount → `notes/windows-toast.md`
- **Persisted state (prefs/controllers/analytics)** → `src/persist/`
- **Single instance, tray lifecycle, crash-restart** → `src/bin/sdsc-utils.rs`, `src/platform/crash_restart.rs`
- **MSIX packaging / engine DLC** → `packaging/` (`AppxManifest.xml`, `BrowserEngine.AppxManifest.xml`, `pack-msix.ps1 -CefDir`), `src/platform/packaged.rs`, `src/browser/cef_backend.rs` (path resolver), `src/browser/engine_install.rs` (dev feed)
- **CI / clippy fmt gates** → `scripts/ci.sh`, `.github/workflows/ci.yml`
