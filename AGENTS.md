# AGENTS — SDSC Utils

## Quick commands

| Action | Command |
|---|---|
| Run fmt + clippy + test (full CI) | `bash scripts/ci.sh` |
| Format check only | `cargo fmt --all -- --check` |
| Clippy (all targets, fail on warnings) | `cargo clippy --all-targets -- -D warnings` |
| Run all tests | `cargo test --all-targets` |
| Release build | `cargo build --release` |
| Run with dev emulator | `cargo run --features dev-emulate -- --dev` |
| List connected controllers | `sdsc-utils --list-controllers` |

## Workspace structure

- **Root `Cargo.toml`** — defines 4 crates: `sdsc-core`, `sdsc-platform`, `sdsc-shell`, `dualsense`
- **`src/`** — monolithic source tree (not a workspace split); the root package is the UI app
- **`crates/sdsc-core`** — core logic: HID, domain models, analytics, persist, UI logic, games/launch
- **`crates/sdsc-platform`** — platform-specific code (macOS/Linux/windows build config, auto-start, crash restart, UI sounds, wgpu diag) — *currently mostly empty/macro-conditional*
- **`crates/sdsc-shell`** — shell-related utilities (currently mostly empty)
- **`crates/dualsense`** — DualSense HID protocol, driver, known controller list, emulation
- **`src/bin/`** — three binaries: `sdsc-utils`, `sdsc-shell`, `gen_msix_logos`
- **`build.rs`** — rasterizes the DualSense SVG and embeds the `.ico` into the Windows .exe

## Build / test / lint ordering

**Always: fmt → clippy → test** (this is the CI order and the pre-commit hook order).

```bash
bash scripts/ci.sh
# or individually:
cargo fmt --all -- --check     # must pass first
cargo clippy --all-targets -- -D warnings   # must pass second
cargo test --all-targets       # must pass third
```

Missing this order can cause subtle issues (clippy warnings can fmt-reformat code, breaking the `--check` gate).

## Key binaries & entrypoints

- `src/bin/sdsc-utils.rs` — the system-tray app (the default binary)
- `src/bin/sdsc-shell.rs` — shell / secondary binary
- `src/bin/gen_msix_logos.rs` — MSIX logo generator
- `src/lib.rs` — re-exports all internal modules (`app`, `controller`, `domain`, `games`, `ipc`, `persist`, `platform`, `service`, `session`, `ui`)

## Conventions & quirks

- **Battery steps**: DualSense firmware reports battery in 11 coarse steps (0–10). Percentages use the Linux mid-point mapping (step 0 → 5%, step 9 → 95%, step 10/full → 100%). See `src/domain/battery/` or the README.
- **Lightbar color**: Blue → Purple → Red gradient as battery drops; reasserted every ~5 seconds. Low-battery pulse (orange) when ≤5% while discharging.
- **Single instance**: The app enforces single-instance via the `single-instance` crate. Second launch exits quietly.
- **Windows features**: `tray-icon` builds differ per OS — root `Cargo.toml` has `[target.'cfg(windows)'.dependencies]`, `[target.'cfg(target_os = "macos")']`, `[target.'cfg(target_os = "linux")']` sections.
- **No telemetry**: All data stays local (`prefs.json`, `controllers.json`, `analytics.json`, `app.log`).
- **Pre-commit hooks**: `.githooks/` runs `cargo fmt --all` and `cargo clippy -D warnings` on every commit. Bypass with `--no-verify`.
- **CI**: GitHub Actions `ci.yml` runs `bash scripts/ci.sh` on push/PR. The script does fmt → clippy → test in order.

## Platform-specific notes

- **Windows**: Requires the `windows` feature group (see root `Cargo.toml`). HID access via `hidapi`. Tray via `tray-icon` with `win32` backend.
- **macOS**: `tray-icon` with default backend. Input monitoring may be prompted by macOS.
- **Linux**: `tray-icon` with `gtk` feature. Requires `libhidapi` / udev rules for DualSense access.
- **Developer emulator**: Build with `dev-emulate` feature: `cargo run --features dev-emulate -- --dev`. This adds a Developer section in Configure with emulated controllers and battery analytics presets.

## Directory ownership (what changes in each crate)

| Directory | Typical changes |
|---|---|
| `crates/sdsc-core/src/games/` | Game launch matching, process spawning, Steam integration |
| `crates/sdsc-core/src/persist/` | `prefs.json`, `controllers.json`, `analytics.json` read/write |
| `crates/sdsc-core/src/ui_logic/` | Internal UI state logic (not the iced UI) |
| `crates/sdsc-core/src/tests/` | Unit/integration tests and fixtures |
| `crates/sdsc-platform/src/` | Platform config, auto-start, crash-restart, UI sounds, wgpu diagnostics |
| `crates/dualsense/src/` | HID protocol, controller ID mapping, emulation support |
| `src/app/` | iced app setup, tray icon, popup window |
| `src/controller/` | HID driver, known controller models, emulation |
| `src/service/` | IPC service between instances (if needed) |
| `src/persist/` | File-backed prefs/persisted state (companion to crate) |

## Common gotchas for agents

- **`cargo fmt --all -- --check` must run before `cargo clippy --all-targets -- -D warnings`**. Clippy can emit warnings that fmt would have reformatted; running fmt first avoids the "clippy fixes fmt" cycle.
- **Clippy `-D warnings` is active in CI**. Any new `#[allow(clippy::...)]` must be justified; otherwise the build will fail on PR.
- **The `single-instance` crate ensures only one run**. If you add GUI windows, make sure they're owned correctly or you'll get "single instance" exits.
- **Windows .exe icon is generated from SVG at build time** via `build.rs`. If the SVG changes, the icon rebuilds automatically (rerun-if-changed is set).
- **`--dev` flag only works when built with `dev-emulate` feature**. Without it, `--dev` is a no-op.
- **Lightbar color reassertion every ~5 seconds** means if you overwrite the lightbar via another tool (e.g., Steam Input), the app will re-apply its color. This is intentional but worth noting when debugging "stuck" colors.
- **Battery step mapping is Linux-midpoint**. Windows may report different percentages; the app maps firmware steps → percentages via the fixed mid-point formula.

## Where to find things

- **HID driver / controller state** → `src/controller/hyd.rs`, `src/controller/model.rs`, `crates/dualsense/src/`
- **Battery step → percentage math** → `src/domain/color.rs`, `src/domain/pad.rs`
- **Lightbar gradient / spectrum editor** → `src/app/configure/`, `src/theme.rs`
- **Start screen / game launcher** → `src/games/`, `src/bin/sdsc-utils.rs`
- **Persisted state (prefs/controllers/analytics)** → `crates/sdsc-core/src/persist/`, `src/persist/`
- **IPC / multi-instance** → `src/ipc/`, `src/session/`
- **MSIX packaging** → `packaging/`, `src/platform/packaged.rs`, `src/platform/shell_bundle.rs`
- **CI / clippy fmt gates** → `scripts/ci.sh`, `.github/workflows/ci.yml`