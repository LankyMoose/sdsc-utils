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
| Run (service + matching shell) | `cargo run` |
| Run (developer UI included) | `cargo run` |
| List connected controllers | `sdsc-utils --list-controllers` |

## Project structure

- **Single package, no workspace.** Root `Cargo.toml` is one crate (`sdsc-utils`): a library (`src/lib.rs`, crate name `sdsc_utils`) plus four binaries in `src/bin/`.
- **`src/`** — everything lives here; see [Directory ownership](#directory-ownership) below.
- **`build.rs`** — rasterizes the DualSense SVG and embeds the `.ico` into the Windows .exe
- **`notes/`** — topic handoffs for agents (index: `notes/README.md`). Read the matching note before touching a do-not-regress area.
- **`packaging/`** — MSIX packaging; **`scripts/`** — CI checklist; **`assets/`** — icons, sounds, README screenshots.

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

- `src/bin/sdsc-utils.rs` — the service: HID, tray, session (the default binary); spawns `sdsc-shell` from next to its own exe
- `src/bin/sdsc-shell.rs` — the iced UI process (popup, Settings, Start, toasts)
- `src/bin/gen_msix_logos.rs` — MSIX logo generator
- `src/bin/bundle-portable.rs` — appends `sdsc-shell` onto `sdsc-utils` for the portable download
- Plain `cargo build` builds **all** binaries. `cargo run` builds only `sdsc-utils`, so when launched by `cargo run` the service rebuilds `sdsc-shell` with the same profile/features before spawning it (`shell_bundle::sync_shell_under_cargo_run`; grep `shell-sync:` in `app.log`). If that rebuild fails (usually an orphaned `sdsc-shell.exe` holding the file), the existing shell is used and a warning is logged.
- `src/lib.rs` — re-exports all internal modules (`app`, `controller`, `domain`, `games`, `ipc`, `persist`, `platform`, `service`, `session`, `ui`)

## Conventions & quirks

- **Battery steps**: DualSense firmware reports battery in 11 coarse steps (0–10). Percentages use the Linux mid-point mapping (step 0 → 5%, step 9 → 95%, step 10/full → 100%). See `src/controller/dualsense/battery.rs` or the README.
- **Lightbar color**: Blue → Purple → Red gradient as battery drops; reasserted every ~5 seconds. Low-battery pulse (orange) when ≤5% while discharging.
- **Single instance**: The app enforces single-instance via the `single-instance` crate. Second launch exits quietly.
- **Windows features**: `tray-icon` builds differ per OS — root `Cargo.toml` has `[target.'cfg(windows)'.dependencies]`, `[target.'cfg(target_os = "macos")']`, `[target.'cfg(target_os = "linux")']` sections.
- **No telemetry**: All data stays local (`prefs.json`, `controllers.json`, `analytics.json`, `app.log`).
- **Git hooks** (`git config core.hooksPath .githooks`): pre-commit runs `cargo fmt --all` and re-stages staged `.rs` files; pre-push runs `scripts/ci.sh`. Bypass with `--no-verify`.
- **CI**: GitHub Actions `ci.yml` runs `bash scripts/ci.sh` on push/PR. The script does fmt → clippy → test → build in order.

## Platform-specific notes

- **Windows**: `windows` / `winreg` crates under `[target.'cfg(windows)'.dependencies]`. HID access via `hidapi`. Tray via `tray-icon` (no default features).
- **macOS**: `tray-icon` (no default features). Input monitoring may be prompted by macOS.
- **Linux**: `tray-icon` with `gtk` feature. Requires `libhidapi` / udev rules for DualSense access.
- **Graphics backend**: on Windows boot sets `WGPU_BACKEND=vulkan,dx12` (Vulkan preferred, DX12 fallback) unless the env var is already set; `wgpu_diag::alpha_composite()` reports whether transparent windows will composite. See `notes/ui-refresh.md`.
- **Developer emulator**: Always compiled in non-release builds (`cfg(debug_assertions)`) — a plain `cargo run` already has it. Configure groups the debug tools under a **Developer mode** caption: **Controllers** (one row per emulated pad with a chevron that expands its controls inline; `EmulatorCommand` addresses pads by serial, never row index), **Pad input**, **Diagnostics**. The fleet lives in `App::emulated_pads` and is **rehydrated at boot** from remembered `emu-` serials (`emulate::rehydrate`, fed by `known.remembered_disconnected(&[])`) — pads persist to `controllers.json` and come back **disconnected**, so a restart never fires a connect toast. Battery edits preview and debounce by 1s *only while the pad is connected* — a disconnected pad is not published, so it applies immediately, and unplugging cancels an armed change. A pad's charge state is **derived from its link** (`emulate::state_for`: USB charges, `Complete` at 100%; Bluetooth discharges); there is no separate `SetState` command, and `set_percent`/`set_link` re-derive so the invariant holds however the pad is edited. Release builds (`cargo build --release`) compile them out; no Cargo feature or CLI flag exists.

## Directory ownership

| Directory | Typical changes |
|---|---|
| `src/app/` | iced daemon (`App`): window lifecycle for popup / Settings / Start / toast, message routing, shell side of IPC |
| `src/service/` | Service process: owns HID worker + tray, spawns `sdsc-shell`, talks to it over IPC |
| `src/session/` | Pure device session (no windows/HWND): presence, notify, analytics, Start-open policy; emits `SessionEffect`s the UI applies |
| `src/ipc/` | Service ↔ shell pipe transport (Windows named pipe / Unix socket) |
| `src/controller/` | HID polling + worker (`hid/`), DualSense protocol (`dualsense/`: battery, lightbar, rumble, input, identity), known controllers, debug-only `emulate` emulation |
| `src/domain/` | Pure logic: battery colors/spectrum math, pad gestures/chords, protocol helpers |
| `src/games/` | Steam library scan, game launch, process matching |
| `src/persist/` | `prefs.json`, `controllers.json`, `analytics.json`, `steam_library.json`, paths |
| `src/platform/` | Logging, app metadata, autostart, crash-restart, device-arrival watch, file icons, MSIX/portable (`packaged`, `shell_bundle`), UI sounds, wgpu diagnostics, Win32 helpers |
| `src/ui/` | All iced views + styling: `theme.rs`, `popup/`, `configure/` (Settings), `start/` (compact + immersive Start), `toast/`, `shader/`, `percent_ring.rs`, `layout.rs` (window placement) |
| Tests | Inline `#[cfg(test)] mod tests` next to the code (no `tests/` dir) |

## Common gotchas for agents

- **Intermittent test failure? Check shared globals before anything else.** `cargo test` runs tests on N threads in one process, so any `static`/`OnceLock`/`LazyLock` is shared. Reproduce with `--test-threads=16` in a loop rather than re-running (see `notes/flaky-tests.md`). Any test that mutates a process-global must take that global's test lock — `art_worker::shutdown()` wipes the shared `icon_cache` via `wipe_art_caches()`, and `domain::color::set_active_spectrum` swaps the process-wide `SPECTRUM`. Locks are `icon_cache::with_cache_lock` and `color::with_spectrum_lock`.
- **`cargo fmt --all -- --check` must run before `cargo clippy --all-targets -- -D warnings`**. Clippy can emit warnings that fmt would have reformatted; running fmt first avoids the "clippy fixes fmt" cycle.
- **Clippy `-D warnings` is active in CI**. Any new `#[allow(clippy::...)]` must be justified; otherwise the build will fail on PR.
- **The `single-instance` crate ensures only one run**. If you add GUI windows, make sure they're owned correctly or you'll get "single instance" exits.
- **Windows .exe icon is generated from SVG at build time** via `build.rs`. If the SVG changes, the icon rebuilds automatically (rerun-if-changed is set).
- **Debug-only code is gated by `cfg(debug_assertions)`, not a Cargo feature**. The Developer emulator, HID diagnostics, and verbose logging ship in every non-release build. `scripts/ci.sh` lints *both* profiles (`--release` included) because release compiles out whole items that debug compiles — a debug-only clean build does not prove a clean release.
- **Lightbar color reassertion every ~5 seconds** means if you overwrite the lightbar via another tool (e.g., Steam Input), the app will re-apply its color. This is intentional but worth noting when debugging "stuck" colors. (Incremental-poll work narrows this to change/new/30s-backstop; see `notes/hot-enumeration-freeze.md` for the Steam test watch.)
- **Hot enumeration freeze**: while input is hot (`input_hot=1`, e.g. Start open with a stable pad), `refresh_devices()` is deferred up to `HOT_ENUM_MAX_DEFER` (20s heartbeat cap) and service cold polls are gated off — plus the OS arrival watcher (Windows `CM_Register_Notification`, Linux `/dev` snapshot; see `notes/device-arrival-watch.md`) fires list-only refresh hints so newly-connected pads surface in ~1s. Pre-existing freeze documented in `notes/hot-enumeration-freeze.md`; any poll/sampling change must still be validated with a Start-open + second-connect test, not just tray-idle.
- **Battery step mapping is Linux-midpoint**. Windows may report different percentages; the app maps firmware steps → percentages via the fixed mid-point formula.

## Where to find things

- **HID driver / controller state** → `src/controller/hid/`, `src/controller/driver.rs`, `src/controller/model.rs`, `src/controller/dualsense/`
- **Battery step → percentage math** → `src/controller/dualsense/battery.rs`
- **Battery color spectrum** → `src/domain/color.rs`; lightbar writes / reassert → `src/controller/dualsense/lightbar.rs`
- **Lightbar spectrum editor (Settings)** → `src/ui/configure/spectrum.rs`, `src/ui/configure/mod.rs`
- **Theme / shared widget styles** → `src/ui/theme.rs`; restyle plan → `notes/ui-refresh.md`
- **Start screen / game launcher** → `src/ui/start/` (UI), `src/games/` (Steam scan, launch); immersive mode → `notes/start-immersive.md`
- **Toasts** → `src/ui/toast/` (state machine in `machine.rs`); z-order / remount → `notes/windows-toast.md`
- **Persisted state (prefs/controllers/analytics)** → `src/persist/`
- **Service / shell split, IPC, single instance** → `src/service/`, `src/ipc/`, `src/session/`, `src/bin/sdsc-utils.rs`
- **MSIX packaging** → `packaging/`, `src/platform/packaged.rs`, `src/platform/shell_bundle.rs`
- **CI / clippy fmt gates** → `scripts/ci.sh`, `.github/workflows/ci.yml`