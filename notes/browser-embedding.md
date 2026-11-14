# Browser embedding: Servo offscreen inside Start

**Status:** in progress (branch `browser/multi-app-suspend-spike`). **When to read:** anything
touching `src/browser/`, the Start browser surface, or the `servo-engine` feature.

## Decision

- Inside-Start + open web + no-DRM → Servo offscreen composited as an iced
  texture. Rejected: `wry`/WebView2 child HWND (native child draws above the
  iced/wgpu canvas — toasts, veils, and drawer chrome cannot composite over
  it; Linux child is X11-only anyway). `retsurf` is a standalone GPL app, not
  a library — only its controller UX patterns carry over (virtual cursor,
  link hints, grid/wheel OSK, input maps). Incompatible pages fall back to
  the system browser via `games::launch`.
- Proven by spike (Nov 2026, temp `servo-spike` crate, Servo 0.8.0):
  `SoftwareRenderingContext` 800×600 → `read_to_image` → `example.com`
  rendered correctly (icon, Latin/Arabic-RTL/Chinese/French/Russian). Software
  path needs no GL sharing and no GStreamer (media feature off by default).

## Layout

- `src/browser/engine.rs` — engine seam: `EngineCommand` down, `EngineEvent`
  up, `EngineHandle` channel pair (mirrors `InputHub` bind/clear/emit).
- `src/browser/servo_backend.rs` (`servo-engine` feature only) — worker
  thread owning all Servo types (`!Send` by design). Suspend = skip spin +
  long sleep; generation-guarded loads; 30s load timeout.
- `src/browser/fetch.rs` + `BrowserSession` — reader backend (default
  build). `BrowserSession` also carries the engine `BrowserFrame` and all
  suspend/resume generation guards.
- `src/browser/mod.rs` — `AppManager` (one foreground app, rest
  hidden/unfocused/suspended) + `open_browser` helper.
- Start surface (`src/ui/start/view.rs::browser_view`): wash + wide card over
  the live stage, above modals, below the settings drawer. Engine image
  (Contain) when a frame exists, reader text otherwise. Pad Up/Down scrolls
  (engine `Scroll::Delta` vs text `scroll_to`); Cross re-goes; Circle/Esc
  closes through the standard unwind chain.
- `Message::BrowserEngine` is system-class (like ticks, not input): frames
  must not reset idle-dim/cursor-hide.

## Startup ordering contract (Windows, NVIDIA)

Worker-thread WGL context creation segfaults inside `nvoglv64.dll` once
any Vulkan is initialized in the process — but is fine before it (proven by
scratch probes). So `App::boot` blocks on the engine's context-ready signal
(up to 15s, then falls back to reader-only) and `wgpu_diag` adapter logging
(which creates a wgpu Instance) moved after it. `WGPU_BACKEND` stays
Vulkan-first (transparency needs it); DX12-only also dodges the crash but
loses alpha compositing. `SDSC_NO_ENGINE=1` forces the reader path without
spawning the worker.

## Build prerequisites (Windows)

Servo's SpiderMonkey/ICU needs Clang + Python at build time and the MSVC C++
runtime import lib at link time:

- LLVM 23 (winget `LLVM.LLVM`) for `clang`/`libclang` (`LIBCLANG_PATH`).
- Python 3.13 (winget `Python.Python.3.13`).
- BuildTools ≥ 17.14 (MSVC 14.44): older STLs lack the `__std_*` vectorized
  helpers mozjs/ICU reference → LNK2019. `build.rs` links `msvcprt` on
  Windows MSVC for the same reason (proven by the spike link).

## Feature flag

`servo-engine = ["dep:servo", "dep:dpi", "dep:euclid", "dep:url"]`,
default off while the backend matures: Servo dominates clean-build/CI time.
Default `cargo test`/CI stays on the reader backend; test the engine with
`cargo run --features servo-engine`. Flip default when the backend is the
product path.

## Open gaps (phase 2+)

- No mouse-wheel→engine scroll yet (image has no scrollable; needs
  `mouse_area` scroll events forwarded as `Scroll::Delta`).
- No link clicking / text input into the page (`notify_input_event`
  plumbing). Pad currently scrolls + re-goes only.
- Fixed 800×600 viewport (card scales it); resize on card-size change.
- No per-engine pref for JS/media toggles; no disk cache tuning.
