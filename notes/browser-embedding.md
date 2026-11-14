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
- `src/browser/fetch.rs` + `BrowserSession` — reader backend (the
  `--no-default-features` fallback). `BrowserSession` also carries the engine
  `BrowserFrame` and all suspend/resume generation guards.
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

Servo's SpiderMonkey/ICU needs Clang + Python at build time, the MSVC C++
runtime import lib at link time, and a bumped Stylo thread stack:

- LLVM 23 (winget `LLVM.LLVM`) for `clang`/`libclang` (`LIBCLANG_PATH`).
- Python 3.13 (winget `Python.Python.3.13`). Stylo's codegen runs at build
  time and looks for `python.exe` on `PATH` (or the `PYTHON3` env var) —
  a missing interpreter fails the build with "Can't find python".
- BuildTools ≥ 17.14 (MSVC 14.44): older STLs lack the `__std_*` vectorized
  helpers mozjs/ICU reference → LNK2019. `build.rs` links `msvcprt` on
  Windows MSVC for the same reason (proven by the spike link).
- `.cargo/config.toml` sets `SERVO_STYLE_THREAD_STACK_SIZE_KB=1024`
  (Stylo reads it at *compile time*). Servo's default 512 KiB style threads
  overflow on real-world restyle storms — typing into Google's autocomplete
  box aborts the process (`StyleThread has overflowed its stack`), while
  passive loads never push that deep. 1024 matches the platform default
  thread stack; the extra is address-space reserve. Changing the value
  needs a `cargo clean -p stylo` (at least) to take effect, since Cargo
  does not track env changes in fingerprints.

## Feature flag

`servo-engine = ["dep:servo", "dep:dpi", "dep:euclid", "dep:url"]`,
**on by default** (single exe): plain `cargo run` / `cargo test` / CI builds
the engine. `servo` itself is `default-features = false` plus `bundled`,
`clipboard`, `js_jit`, `multiprocess` — i.e. upstream defaults minus
`devtools` (remote-debugging server, pure build cost; JIT and multiprocess
stay for page performance). UI-only iteration skips Servo entirely with
`cargo run --no-default-features` (reader backend) or `SDSC_NO_ENGINE=1`
at runtime; the MSRV job uses `--no-default-features` since the engine
needs the pinned toolchain.

## Upstream gaps (Servo 0.8 AX tree, 2026-11)

Verified against 14k+ nodes across main + iframe documents on
google.com: these controls never appear in any `TreeUpdate`, so no
client-side rule can target them (no role, no Click action, no tag —
`html_tag` is never populated either):

- Search box (`<textarea>`/`<input>`): zero text-input roles of any kind.
- People-also-ask accordions (`div[role=button]`): zero `Button` roles
  anywhere, including pages full of native `<button>`s — Servo's
  role mapping drops them (arrive as `GenericContainer` at best, usually
  absent entirely).
- Some header links arrive as `Link` without bounds and are correctly
  skipped (can't aim at geometry that isn't there).

Missing nodes do NOT corrupt other nodes' positions (each rect is
absolute); offsets come from layout-shift lag and scroll tracking, which
are handled separately. Workaround today: free-point click + typing works
(input bypasses AX); only magnetism needs the nodes. Next step: file
upstream with a minimal repro page + `SDSC_AX_DEBUG=1` dump.

## Open gaps (phase 2+)

- No mouse-wheel→engine scroll yet (image has no scrollable; needs
  `mouse_area` scroll events forwarded as `Scroll::Delta`).
- No link clicking / text input into the page (`notify_input_event`
  plumbing). Pad currently scrolls + re-goes only.
- Fixed 800×600 viewport (card scales it); resize on card-size change.
- No per-engine pref for JS/media toggles; no disk cache tuning.
