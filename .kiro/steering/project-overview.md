# ps5-battery-display — Project Overview

SDSC Utils (package: `sdsc-utils`, v1.6.0) is a Windows-first Rust desktop app that displays Sony DualSense/DualSense Edge controller battery levels in the Windows system tray. It has grown into a full gaming utility with:

- Battery display + analytics
- Lightbar RGB control
- PS5-style start screen game launcher with Steam library integration — compact and fullscreen immersive modes (always-immersive is the default since v1.6.0)
- Cover-flow games strip with Steam playtime/last-played metadata, section grouping (Today / 7 days / 1 month / etc.), and a position indicator
- In-window Start settings via the DualSense Options button (compact modal in normal mode; drawer in immersive)
- Controller haptics for nav/action/hold/slide cues over Bluetooth
- Overlay toasts (shown inside immersive Start; file pickers stay above it)
- Settings window, battery analytics
- Custom wgpu shaders: ambient atmosphere + vignette (over splash/atmosphere only; games strip, dock, footer stay undimmed)

## Stack

- **Language:** Rust (edition 2024, MSRV 1.88)
- **GUI:** iced 0.14 (daemon mode, multi-window, canvas, tokio runtime)
- **GPU:** wgpu 27 (custom ambient + vignette shaders in `src/ui/shader/`)
- **HID:** hidapi 2.6.6
- **Platform target:** Windows (Win32 FFI via the `windows` 0.61 crate); Linux/macOS compile but are not actively tested

## Architecture: Service + Shell Split

Since v1.5.0 the app runs as two processes:

- **`sdsc-utils` (service)** — owns DualSense HID, the Windows tray, session state, and reconnect logic. Spawns `sdsc-shell` as a child and communicates over a Windows named pipe (or Unix socket) using length-prefixed JSON frames defined in `src/ipc/`.
- **`sdsc-shell` (iced shell)** — the iced `daemon` window manager. Opens windows on demand (popup, Settings, Start screen, overlay toast). Can also run in standalone mode (no `SDSC_UTILS_PIPE` env) for dev/testing.

This split means a shell crash (GPU/wgpu panic) can be restarted without dropping the Bluetooth HID handle.

## Key Source Directories

| Path              | Role                                                          |
| ----------------- | ------------------------------------------------------------- |
| `src/app/`        | iced App struct: boot, update, view, subscription             |
| `src/service/`    | Headless service run loop (HID, tray, IPC, spawn shell)       |
| `src/controller/` | DualSense HID (battery, identity, lightbar, rumble, input)    |
| `src/domain/`     | Typed pad events, gesture detection, color/gradient           |
| `src/ipc/`        | Named-pipe protocol: ShellCommand / ServiceMessage            |
| `src/session/`    | DeviceSession, open/close policy, SessionEffect               |
| `src/games/`      | GamesCatalog, Steam library scan, process matching, launcher  |
| `src/persist/`    | prefs.json, controllers.json, analytics.json, games.json      |
| `src/platform/`   | Logging, autostart, crash-restart, Win32 HWND, MSIX detection |
| `src/ui/`         | All iced widgets: popup, configure, start, toast, tray, theme |

## Build & Verify

```bash
cargo build                   # debug build
cargo build --release         # release build
cargo test --all-targets      # run all inline tests
bash scripts/ci.sh            # full CI: fmt + clippy + test
```

Dev mode (no hardware needed):

```bash
cargo run --features dev-emulate -- --dev
```

## Distribution

- **Microsoft Store** (MSIX): `packaging/pack-msix.ps1` → `sdsc-utils.msix`
- **Portable**: single `sdsc-utils.exe` self-extracts `sdsc-shell.exe` to `%APPDATA%\sdsc-utils\shell\` on first launch
- Release: tag `v{version}` → GitHub Actions builds and attaches both artifacts

## Do Not Commit

- `.agents/` directory (workflow agent artifacts)
- Build artifacts in `target/`
