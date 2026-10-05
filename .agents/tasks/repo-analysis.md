# Repository Analysis: ps5-battery-display (SDSC Utils)

## Summary

This is a mature, feature-rich **Windows-first Rust desktop application** (v1.5.2, with unreleased work in progress) that displays Sony DualSense / DualSense Edge controller battery levels in the Windows system tray. What started as a simple battery monitor has grown into a full gaming utility with a PS5-style start screen game launcher, lightbar control, battery analytics, overlay toasts, and an immersive fullscreen mode. It is built with the **iced** GUI framework, uses **hidapi** for HID I/O, and is structured as a single-crate library with multiple binaries. It targets the **Microsoft Store** (MSIX) as well as portable `.exe` distribution.

---

## 1. Project Structure and Architecture

### Crate Layout

This is a **single Cargo workspace** — one crate named `sdsc-utils` at the repo root. There are no sub-crates. The `Cargo.toml` declares:

- **Library:** `sdsc_utils` (path: `src/lib.rs`)
- **Binaries:**
  - `sdsc-utils` — the HID/tray service; the main executable (default-run)
  - `sdsc-shell` — the iced GUI shell; spawned by the service
  - `gen_msix_logos` — build-time helper to generate MSIX Store logo assets

### Architecture: Service + Shell Split (since v1.5.0)

The key architectural decision (landed in v1.5.0) is a **service/shell split**:

- **`sdsc-utils` (service)** — owns DualSense HID, the system tray, session state, and reconnect logic. Spawns `sdsc-shell` as a child process and communicates over a named pipe (Windows) or Unix domain socket (non-Windows) via length-prefixed JSON frames.
- **`sdsc-shell` (iced shell)** — the iced `daemon` window manager. Opens windows on demand (popup, Settings, Start screen, overlay toast). Can run in two modes:
  - **Client mode** (`SDSC_UTILS_PIPE` env set): attaches to the service over IPC; service owns HID/tray.
  - **Standalone mode** (env not set): runs the full in-process daemon (legacy/dev path).

This split means a shell crash (e.g., from iced/wgpu GPU issues) can be restarted without reopening Bluetooth or re-scanning HID.

### Source Module Tree

```
src/
├── lib.rs                  — top-level module re-exports
├── bin/
│   ├── sdsc-utils.rs       — service entry point (CLI args, SingleInstance guard, service::run())
│   ├── sdsc-shell.rs       — iced shell entry point (standalone or client mode)
│   └── gen_msix_logos.rs   — MSIX logo generator
├── app/mod.rs              — iced daemon App struct + update/view/subscription + boot
├── service/mod.rs          — HID/tray service run loop (no iced windows)
├── controller/
│   ├── driver.rs           — HID device detection (is_gamepad etc.)
│   ├── dualsense/          — DualSense-specific HID: battery, identity, input, lightbar, rumble
│   ├── emulate.rs          — dev-emulate feature: fake controller presets
│   ├── hid/                — HID plumbing: worker thread, poll, diagnostics, launch paths
│   ├── known.rs            — remembered controllers + nicknames persistence
│   └── model.rs            — ControllerStatus, Connection, PowerState (shared types)
├── domain/
│   ├── color.rs            — BatterySpectrum, Rgb, gradient interpolation
│   ├── gesture.rs          — GestureDetector, ChordReleaseGate (PS button chord)
│   ├── pad.rs              — InputEdge, InputSnapshot, nav parsing from HID reports
│   └── protocol.rs         — DualSense HID report protocol structs
├── games/
│   ├── mod.rs              — GamesCatalog (games.json), GameEntry, Steam/manual entries
│   ├── launch.rs           — process launcher
│   ├── process_match.rs    — running game detection
│   └── steam.rs            — Steam library scan, appmanifest parsing, icon paths
├── ipc/
│   ├── mod.rs              — ShellCommand / ServiceMessage enums, frame codec
│   ├── win_pipe.rs         — Windows named-pipe server/client
│   └── unix_pipe.rs        — Unix socket server/client
├── persist/
│   ├── analytics.rs        — battery analytics store (analytics.json)
│   ├── json.rs             — safe JSON load/save with .bak on corruption
│   ├── notify.rs           — notification event evaluation
│   ├── paths.rs            — app data directory paths
│   ├── prefs.rs            — Prefs struct (prefs.json)
│   └── steam_library.rs    — Steam library metadata cache
├── platform/
│   ├── app_log.rs          — file logger (app.log, hid-trace.log)
│   ├── app_meta.rs         — DISPLAY_NAME, PKG_NAME, PKG_VERSION
│   ├── autostart.rs        — Windows startup .lnk / MSIX StartupTask
│   ├── crash_restart.rs    — panic-hook + budgeted relaunch
│   ├── file_icon.rs        — Windows shell icon extraction
│   ├── packaged.rs         — MSIX vs portable detection
│   ├── shell_bundle.rs     — portable self-extracting shell binary
│   ├── ui_sound.rs         — start-screen audio cues (rodio/mp3)
│   ├── wgpu_diag.rs        — wgpu adapter logging + panic context
│   └── win32/              — Win32 FFI (HWND manipulation, topmost, etc.)
├── session/
│   ├── mod.rs              — DeviceSession: presence, notify, analytics, open/close policy
│   └── gate.rs             — pure open/close policy logic + tests
└── ui/
    ├── mod.rs              — re-exports all UI modules
    ├── color.rs            — re-exports from domain::color
    ├── configure/          — Settings window (notifications, toast, lightbar, analytics, system)
    ├── icon.rs             — tray icon (embedded RGBA from OUT_DIR)
    ├── layout.rs           — window placement, toast slide, TrayAnchor, ToastPlacement
    ├── percent_ring.rs     — circular battery ring widget (iced canvas)
    ├── popup/              — controller popup window
    ├── shader/             — custom wgpu shaders: ambient, vignette
    ├── start/              — start screen: art_worker, carousel, gesture, idle, immersive,
    │                         input, mode, position, reveal, settings, view, vstrip
    ├── svg_icon.rs         — SVG rasterizer (resvg), icon cache
    ├── theme.rs            — iced theme + custom widget styles
    ├── toast/              — overlay toast: machine (state machine), view, mod
    └── tray.rs             — system tray icon + menu
```

---

## 2. What Each Module Does

| Module                   | Role                                                                                                                                                                                                                                                                                                                                              |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `app`                    | Central iced `App` struct. `boot()` initializes state; `update()` is the message dispatch; `view()` renders per-window; `subscription()` drives tray events, pad input, timers. ~750+ lines.                                                                                                                                                      |
| `service`                | Headless service run loop. Owns `HidWorkerHandle`, `PipeServer`, tray. Spawns and respawns sdsc-shell. Handles hot-path battery from the input stream, ghost-flap cooldown, and forwards `SessionEffect` to the shell over IPC.                                                                                                                   |
| `controller/hid/worker`  | Single-threaded DualSense HID owner. All HID I/O (lightbar, poll, identify, power-off, rumble, input sampling) runs here. Publishes an `InputSnapshot` that the UI clones. Never blocks the iced thread.                                                                                                                                          |
| `controller/dualsense/*` | DualSense-specific: `battery` (report parsing), `identity` (MAC, product name), `input` (HID report → nav reading), `lightbar` (RGB writes, BT claim sequence), `rumble` (motor output handle).                                                                                                                                                   |
| `domain/pad`             | `InputEdge`, `InputSnapshot`, `GestureDetectorBank`, `NavReading` — the abstraction between raw HID bytes and typed gamepad events.                                                                                                                                                                                                               |
| `session`                | Pure domain logic: `DeviceSession` tracks live controllers, fires `SessionEffect` values (open/close start, notify, set tray). `gate.rs` contains all open/close policy as pure functions — well tested.                                                                                                                                          |
| `ipc`                    | Named-pipe protocol. `ShellCommand` (shell→service) and `ServiceMessage` (service→shell) serialized as length-prefixed JSON.                                                                                                                                                                                                                      |
| `ui/start`               | The PS5-style start screen launcher. ~15 sub-modules covering: art_worker (background Steam art decode), carousel (Games/Controllers slide), gesture (chord), idle (dim/sleep overlay), immersive (full-monitor promote/demote ceremony), input (DualSense/keyboard nav), mode, settings (in-window Options panel), view (main iced widget tree). |
| `ui/toast`               | Overlay toast presentation. `machine.rs` is a pure state machine (Placing→SlidingIn→Resting→SlidingOut) that was added after several race conditions with the Start screen.                                                                                                                                                                       |
| `ui/configure`           | Settings window: notification toggles, toast position picker, lightbar spectrum editor, battery analytics panels, system tab (data folder, autostart).                                                                                                                                                                                            |
| `persist`                | `prefs.json`, `controllers.json`, `analytics.json`, `games.json`, `steam_library.json`. All load/save via `json.rs` which has partial-parse resilience (bad records skip, `.bak` on full corruption).                                                                                                                                             |
| `platform`               | OS-specific plumbing: logging, autostart, crash-restart, Win32 HWND manipulation, wgpu diagnostics, MSIX detection, self-extracting shell bundle, UI sounds.                                                                                                                                                                                      |

---

## 3. Key Dependencies

From `Cargo.toml`:

| Dependency                      | Version | Role                                                                         |
| ------------------------------- | ------- | ---------------------------------------------------------------------------- |
| `iced`                          | 0.14.0  | GUI framework (daemon mode, multi-window, canvas, SVG, image, tokio runtime) |
| `wgpu`                          | 27      | GPU rendering backend for iced                                               |
| `hidapi`                        | 2.6.6   | Cross-platform DualSense HID I/O                                             |
| `tray-icon`                     | 0.24.2  | System tray icon and menu                                                    |
| `windows`                       | 0.61    | Win32 FFI (HID, registry, threading, shell, gaming input)                    |
| `serde` + `serde_json`          | 1       | Preferences, IPC, game catalog, analytics                                    |
| `resvg`                         | 0.45    | SVG rasterization (icons, tray)                                              |
| `rodio`                         | 0.20    | Start-screen audio (mp3)                                                     |
| `image`                         | 0.25    | JPEG/PNG/WebP/ICO decode for game art                                        |
| `bytemuck`                      | 1       | Zero-copy HID report casting                                                 |
| `crc32fast`                     | 1.5.0   | DualSense BT CRC32 in output reports                                         |
| `sha2`                          | 0.10    | Controller identity hashing                                                  |
| `rfd`                           | 0.15    | Native file dialogs (Add shortcut, custom icon)                              |
| `single-instance`               | 0.3.3   | Prevent multiple service instances                                           |
| `winreg`                        | 0.55    | Windows Registry (autostart)                                                 |
| Build: `ico`, `resvg`, `winres` | —       | Embed .ico and .exe version info at build time                               |

---

## 4. Build System and CI

### Build Script (`build.rs`)

- Rasterizes the DualSense SVG into RGBA bytes at 32px and embeds them as a compiled `&[u8]` constant (`OUT_DIR/icon_embedded.rs`) for the tray icon.
- On Windows: generates a multi-size `.ico` (16/32/48/256px) and links it into the `.exe` via `winres`, along with `ProductName` and `FileDescription` version info.
- Invalidation triggered by SVG asset changes and `build.rs` itself.

### CI (`scripts/ci.sh`)

No GitHub Actions workflow files exist in `.github/`. The CI script `scripts/ci.sh` is a bash script that runs:

1. `cargo fmt --all -- --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test --all-targets`

### Git Hooks (`.githooks/` — referenced in README but directory not found)

The README documents:

- **pre-commit**: `cargo fmt --all`, re-stage `.rs` files
- **pre-push**: runs `scripts/ci.sh`

The `.githooks/` directory was not found by file search, which likely means it has not been created yet (or the search is limited). The README instructs: `git config core.hooksPath .githooks`.

### Packaging (`packaging/`)

- `pack-msix.ps1` — PowerShell script to build an MSIX package. Stamps version from `Cargo.toml`, generates Store logos via `gen_msix_logos`, calls `makeappx.exe`.
- `AppxManifest.xml` — MSIX manifest with Store identity `LankyMoose.SDSCUtils`.
- Tagged release publishes `sdsc-utils.exe` (portable) and `sdsc-utils.msix` to GitHub Releases.
- Portable builds use `platform/shell_bundle.rs` — the shell binary is embedded in the service exe and unpacked on first launch.

---

## 5. Cursor Rules

Two Cursor rules apply to all code in this repo (`alwaysApply: true`):

### `.cursor/rules/debug-diagnostics.mdc`

**Require debug-only diagnostic logging for all new work.** Key constraints:

- Use `cfg(debug_assertions)` or the `hid_diag::diag_info`/`diag_warn` helpers that no-op in `--release`.
- Never leave high-volume HID traces or hitch-report UI on in `--release`.
- When chasing hangs/races: add phase markers, slow-op timing (`hid_diag::enter_op`), correlatable log tokens.
- If the UI can freeze, provide a debug-only F7/F8 mark path (`HITCH_MARK` via `app_log::mark_hitch`).
- Add a sibling under `notes/` when landing new investigation tooling.
- Keep normal `app.log` warn/error for real user-facing failures in all builds.

### `.cursor/rules/input-latency-bluetooth.mdc`

**Prefer immediate DualSense input reads; keep Bluetooth traffic minimal.** Key constraints:

- Prefer immediate reads over poll-sleep delays.
- Block in HID read (report rate ~4ms wired); don't busy-spin.
- While pad input is live, keep UI snapshot apply cadence matching pad report rate (~4ms wired).
- Reading existing reports does not count as extra Bluetooth traffic.
- Do NOT add output reports, feature reports, or handle churn to read faster.
- Lightbar/battery poll/Identify stay infrequent.
- Keep open handles on short timeouts; only drop on real I/O errors.

Both rules also reference the `notes/` investigation docs as mandatory context for related work.

---

## 6. Developer Notes (`notes/`)

| Note                         | Status         | Summary                                                                                                                                                                                           |
| ---------------------------- | -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ui-lockup-investigation.md` | playbook       | History of ~5s UI/HID freezes. Root causes: `HidApi::new()` on every sample, hot-path `hidapi_refresh` while `input_hot`. All fixed. Grep tokens, F7/F8 hitch marks, log paths documented.        |
| `hid-live-session.md`        | do-not-regress | Hot-path battery from input stream (no exclusive poll while hot), rumble handles, silence drop logic (~200ms → drop from live_pads). Must not regress.                                            |
| `windows-toast.md`           | do-not-regress | Toast z-order vs Settings/Start (iced#3108 present starvation), wrong toast content on reuse, connect toast slide vs Start open. Pure state machine in `toast/machine.rs` is the fix.             |
| `start-immersive.md`         | do-not-regress | Immersive Start promote/demote ceremony, chord latch, controllers dock, idle/sleep overlay, Options settings panel, art worker, ken-burns splash, cold-load status rail. Very detailed.           |
| `windows-bt-lightbar.md`     | historical     | BT lightbar silent no-ops on Windows (interrupt vs control path, Steam HID claim). Fixed in v1.4.3.                                                                                               |
| `wgpu-image-atlas-crash.md`  | do-not-regress | iced/wgpu `create_renderer` panic in `Atlas::new`. Not atlas growth — triggered by cold window open under GPU pressure. Mitigations: warm toast sentinel, no same-turn raise+open, deferred hide. |

---

## 7. Current Completeness / Work-in-Progress

### Version: 1.5.2 (released 2026-10-03), with unreleased changes

The **[Unreleased]** section in `CHANGELOG.md` describes three in-progress items:

**Added (unreleased):**

- Immersive Start soft rectangular edge **vignette** over atmosphere/splash (games strip, dock, footer stay undimmed). Evidence: `src/ui/shader/mod.rs` exposes both `ambient` and `vignette` shader modules; `start/mod.rs` calls `reset_pipeline_diag()` for the vignette.

**Fixed (unreleased):**

- Bad JSON records no longer discard the rest of the file — partial parse resilience with `.bak`. This is already implemented in `persist/json.rs` (the `decode_array_keeps_siblings` and `decode_map_values_keeps_siblings` tests exist).
- Pad input no longer stalls for ~5s on hot-path `hidapi_refresh`; shell-client F8 hitch marks now report IPC context. This is the content of the `ui-lockup-investigation.md` playbook — already implemented, pending release.

**Changed (unreleased):**

- Connected toasts skipped for already-connected controllers at app start.
- Configure System footer: icon styling tweaks.
- Start reopen gesture fixed to PS; custom chord recording removed.

### Development Status: Mature, Active

The project has had 30+ releases from v0.1.0 (2026-07-30) to v1.5.2 (2026-10-03), indicating very active development over ~2 months. The architecture is well-settled (the service/shell split landed in v1.5.0), test coverage exists for the key domain modules, and the notes directory shows systematic investigation documentation.

### Known Open Areas

From the notes and code:

- The `dev-emulate` feature provides developer-mode controller presets and analytics testing — this is a supported but non-default build path.
- macOS and Linux support is "expected to work" but not actively tested (`platform/win32` is Windows-only; tray click events don't work on Linux per README).
- The `gen_msix_logos` binary produces Store logo assets — this runs at pack time, not build time.

---

## 8. UI Framework

**[iced](https://github.com/iced-rs/iced) 0.14.0** is the UI framework. Specific features used:

- `daemon` mode (windowless boot, windows created on demand)
- Multi-window support (popup, settings, start screen, toast — all separate windows)
- `canvas` widget (battery percent rings, lightbar spectrum editor coverage charts)
- `svg` widget (DualSense and action icons)
- `image` widget (Steam game art)
- `tokio` runtime
- `advanced` features (custom widget rendering)
- Custom **wgpu shaders** for the immersive Start atmosphere/vignette (`ui/shader/ambient`, `ui/shader/vignette`)

The app uses iced's `Theme` system with custom styles via `ui/theme.rs`. Windows use `HWND_TOPMOST` manipulation via Win32 FFI for toast z-ordering (documented in `notes/windows-toast.md` with specific iced issue references: iced#3108, #3320).

---

## 9. Existing Tests

Tests are **all inline** (`#[cfg(test)] mod tests` inside source files). No separate `tests/` directory. Coverage by module:

| Module                             | Test count | What's covered                                                                        |
| ---------------------------------- | ---------- | ------------------------------------------------------------------------------------- |
| `games/steam.rs`                   | ~12        | Library folder parsing, appmanifest, playtime, account detection, library cache paths |
| `games/mod.rs`                     | ~7         | Catalog deserialization, Steam toggle, merge, sort, last-played                       |
| `controller/known.rs`              | ~8         | Remember/forget, nickname CRUD, serde compat                                          |
| `domain/color.rs`                  | ~9         | Spectrum interpolation, migration from legacy format, stop CRUD                       |
| `domain/gesture.rs`                | ~7         | GestureDetector edge detection, ChordReleaseGate debounce                             |
| `session/gate.rs`                  | has tests  | Open/close policy (test count not fully counted)                                      |
| `controller/dualsense/battery.rs`  | has tests  | Battery percent parsing from HID reports                                              |
| `controller/dualsense/lightbar.rs` | has tests  | Lightbar HID packet construction                                                      |
| `controller/dualsense/identity.rs` | has tests  | Device identity resolution                                                            |
| `controller/dualsense/rumble.rs`   | has tests  | Rumble packet                                                                         |
| `ui/layout.rs`                     | ~9         | Popup placement, toast placement, slide animation                                     |
| `ui/toast/machine.rs`              | has tests  | State machine transitions                                                             |
| `ui/toast/mod.rs`                  | has tests  | Toast message construction                                                            |
| `ui/percent_ring.rs`               | has tests  | Ring geometry                                                                         |
| `ui/svg_icon.rs`                   | has tests  | SVG icon rendering                                                                    |
| `persist/json.rs`                  | ~4         | Partial parse resilience, .bak on corruption                                          |
| `persist/notify.rs`                | has tests  | Notification event evaluation                                                         |
| `persist/prefs.rs`                 | has tests  | Prefs migration/defaults                                                              |
| `platform/packaged.rs`             | has tests  | MSIX detection                                                                        |
| `games/launch.rs`                  | 1          | Empty target error                                                                    |
| `games/process_match.rs`           | ~2         | Steam appid parsing, manual path match                                                |
| `controller/hid/launch_paths.rs`   | has tests  | HID path parsing                                                                      |
| `controller/emulate.rs`            | ~3         | Emulator preset transitions                                                           |
| `start/art_worker.rs`              | has tests  | Art worker job/cancel                                                                 |
| `domain/pad.rs`                    | has tests  | Input parsing                                                                         |

Test command from `scripts/ci.sh`: `cargo test --all-targets`

---

## 10. Packaging and Distribution

Two distribution channels:

### Microsoft Store (MSIX)

- Package identity: `LankyMoose.SDSCUtils` / Publisher `CN=F2379117-7506-444F-AA08-EC697BF7DE9D`
- `packaging/pack-msix.ps1` builds the `.msix` from `target/release/`
- Packaged startup task: `desktop:StartupTask` TaskId `SdscUtilsStartup` (must stay in sync with `src/autostart.rs`)
- Microsoft re-signs on Store publish

### Portable `.exe`

- Single `sdsc-utils.exe` that self-extracts `sdsc-shell.exe` to `%APPDATA%\sdsc-utils\shell\` on first launch
- Implemented in `platform/shell_bundle.rs`
- Not Authenticode-signed (SmartScreen warns)
- No auto-update; manual download from GitHub Releases

### Release Process

- Tag `v{version}` → GitHub Actions CI workflow builds and attaches `sdsc-utils.exe` + `sdsc-utils.msix`
- `.github/` workflows directory: **not found** in the workspace (may be git-ignored, removed, or searched outside file-search scope). The README references `git tag v1.5.2 && git push origin v1.5.2` as the release trigger.

---

## Conclusions and Recommendations

### For Kiro Agent Work

1. **Always read the two Cursor rules** before touching any DualSense HID, pad input, or UI code. They are `alwaysApply: true` and contain non-negotiable constraints on release build behavior and Bluetooth traffic.

2. **Check the relevant `notes/` file** before modifying any of the areas covered by the do-not-regress notes (toast, immersive Start, HID live session, wgpu atlas). These are the most fragile areas with detailed rollback context.

3. **Build and test command:**

   ```bash
   cargo build --release           # build check
   cargo test --all-targets        # run tests
   bash scripts/ci.sh              # full CI: fmt + clippy + test
   ```

   Minimum Rust: **1.88** (edition 2024).

4. **Dev emulation** for controller testing without hardware:

   ```bash
   cargo run --features dev-emulate -- --dev
   ```

5. **The `.githooks/` directory is missing** — the README instructs developers to `git config core.hooksPath .githooks` but the directory was not found. This should be created with the `pre-commit` and `pre-push` hooks described in the README before any commits.

### Suggested Kiro-Specific Rules

The Cursor rules translate directly for Kiro use. The following Kiro steering file additions would codify the mandatory reading:

- Enforce the same `debug_assertions`-gated logging pattern for all new code
- Point to `notes/README.md` as the investigation catalog
- Specify `bash scripts/ci.sh` as the verification command for all code changes
- Note that `.agents/` output artifacts should never be committed

### Potential Issues Observed

- **`.githooks/` directory absent**: If the pre-commit/pre-push hooks were relied on for formatting enforcement, they are currently inactive. Mitigate by running `bash scripts/ci.sh` manually.
- **No `.github/workflows/` found**: CI trigger for releases is not visible in the workspace. Either the directory is excluded from file search, the workflow was removed, or it lives outside the workspace. The README says CI builds on Windows and the release workflow uses `workflow_dispatch`; verify on GitHub.
- **Unreleased changes are present**: Three items in `[Unreleased]` are already implemented in code but not tagged. The next release would be v1.5.3 or higher.
- **Vignette shader is complete**: `src/ui/shader/mod.rs` declares both `pub mod ambient` and `pub mod vignette`; `src/ui/shader/vignette.rs` and `vignette.wgsl` exist. The unreleased vignette feature is fully implemented.
