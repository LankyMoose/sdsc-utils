# Browser embedding: Servo offscreen in the Browser app (current), CEF pivot planned

**Status:** current implementation (Servo era) on `browser/hub-pivot` / `browser/main`.
**When to read:** anything touching `src/browser/`, the standalone Browser
window, or the `servo-engine` feature. **Planned replacement:**
[browser-hub-pivot.md](browser-hub-pivot.md) (replace Servo with CEF OSR).

## Decision (current)

- Standalone Browser window + open web + no DRM → Servo offscreen
  composited as an iced texture. Rejected: `wry`/WebView2 child HWND (native child draws above the
  iced/wgpu canvas — toasts, veils, drawer chrome cannot composite over it;
  Linux child is X11-only anyway). `retsurf` is a standalone GPL app, not a
  library — only its controller UX patterns carry over.
- Proven by spike (Nov 2026, temp `servo-spike` crate, Servo 0.8.0):
  `SoftwareRenderingContext` 800×600 → `read_to_image` → `example.com`
  rendered correctly (icon, Latin/Arabic-RTL/Chinese/French/Russian). Software
  path needs no GL sharing and no GStreamer (media feature off by default).
- **Cloudflare bot protection is the known ceiling** and the reason a CEF
  replacement is planned: Servo's TLS/HTTP2 fingerprint and JS environment do
  not pass managed challenges, so challenged sites are dead in the embedded
  browser. See the pivot note for the target architecture (CEF OSR, Store/DLC
  engine packaging).

## Layout

- `src/browser/engine.rs` — engine seam: `EngineCommand` down, `EngineEvent`
  up, `EngineHandle` channel pair (mirrors `InputHub` bind/clear/emit). The UI
  only depends on this seam, so the backend is replaceable without touching
  the Browser window.
- `src/browser/servo_backend.rs` (`servo-engine` feature only) — worker
  thread owning all Servo types (`!Send` by design). Suspend = skip spin +
  long sleep; generation-guarded loads; 30s load timeout. The `A11yTree`
  event name is legacy: it carries the DOM-derived focusable set, not Servo's
  accessibility tree.
- `src/browser/dom.rs` — DOM target source: injected `MutationObserver` /
  `ResizeObserver` / scroll listeners ping a marker URL (intercepted locally,
  no traffic); `dom_query_js` returns viewport-space candidates with stable
  per-document ids, visibility verdicts, overflow clipping, scroll truth and
  document dims. Culling/retention is deliberate (pull cost + Servo's slow
  `getComputedStyle`).
- `src/ui/browser/state.rs` — `BrowserPanel`, per-tab `BrowserTabPage`,
  `StoredTab`, `HistorySelector`, chrome targets/layout, cursor integration,
  bookmarks/favicon cache, tab persistence helpers.
- `src/ui/browser/view.rs` — standalone Browser rendering (`root_view`,
  `fullscreen_layout`) and the `BrowserMessage` vocabulary. The window is
  opaque fullscreen with the tab strip/toolbar/bookmarks header and the page
  taking the remaining height.
- `src/app/browser.rs` — `BrowserAppState`: panel, engine handle, capability
  flag, browser-only D-pad stepper and pad bank; window lifecycle
  (`open_browser_window`, `close_browser_window`), `BrowserMessage` router,
  `on_browser_pad`/`on_browser_key`, and the 60Hz `BrowserFrame` tick.
- `src/app/hub.rs` — `AppHub`: installed/active apps (`AppId::Start`,
  `AppId::Browser`), show/hide/focus/suspend/resume/close effects, plus the
  PS hub window lifecycle (`open_hub`/`close_hub`, pad/key routing). The hub
  is topmost; Start is `Normal`.
- `src/ui/hub/` — hub card: Start / Browser / Add-apps tiles, wrapping
  selection, Cross/Circle edges, install-state subtitle. Modal for pad input
  while open.
- `src/browser/cursor.rs` — controller cursor: analog free point, magnet dock
  with acquire/hold hysteresis and break-free, D-pad flights (fixed 150ms
  tween; wrap-to-wrap morph), unified page+header space, row-locked
  horizontals, R2 turbo.
- `src/browser/frame_slot.rs` + `src/ui/start/browser_frame.rs` — per-tab
  latest RGBA; custom `iced_wgpu` primitive uploads unseen generations into
  one stable texture (`queue.write_texture`). No per-frame image handles.
- `src/browser/tabs.rs`, `src/browser/bookmarks.rs` — the two persisted
  browser stores (`tabs.json`, `bookmarks.json`). Restored tabs load lazily;
  engine state never crosses a restart.
- `src/browser/fetch.rs` — reader fallback (blocking `ureq` + readability)
  used when the engine is unavailable. Scheduled for deletion with Servo.
- `src/browser/keys.rs` — iced → Servo keyboard mapping.

## Current input/UX contract (pad, engine mode)

PS opens the topmost hub over Start, Browser, or a game; while the hub is open
it owns pad input and the app underneath is gated. Selecting Browser closes
the hub and opens the standalone fullscreen Browser window. Browser close
(Circle/Esc with no modal) suspends the engine and returns focus to Start when
it is visible.

Input priority is **hub → Browser → Start**. The Browser window has its own
pad bank (`BrowserAppState::nav`) and D-pad stepper; Start's bank no longer
sees browser presses.

- Left stick moves the cursor with velocity + friction; magnet docks on
  arrival, centre-snaps on fresh acquisition, holds in place while scrolling.
- D-pad: vertical is one row per press with column preserved; horizontal is
  row-locked and stops at row ends. D-pad Up/Down is the explicit
  header/page crossing; the analog stick may cross freely.
- R2 turbo doubles cursor acceleration/cap and scroll speed while held and
  disables docking; release re-snaps in place.
- Right stick scrolls only while the cursor is over the page. Scroll
  commands go straight to the engine (no wheel-event round-trip).
- Cross clicks the docked target (chrome target first, then exact-element
  click, else point click); Circle closes/backs out; Triangle edits the URL;
  L1/R1 travel history (tap) or open the history selector (hold); L3 toggles
  magnet; R3 dumps an issue bundle.
- Tabs: favicon + ellipsized title + close, active highlighted,
  `_blank`/`window.open` foreground, background tabs suspend, strip
  persists/restores lazily.
- Browser popup: no Browser/title row, 4px card padding, 16:9 card sized from
  viewport height, dynamic engine viewport.

## Startup ordering contract (Windows, NVIDIA)

Worker-thread WGL context creation segfaults inside `nvoglv64.dll` once any
Vulkan is initialized in the process — but is fine before it (proven by
scratch probes). So `App::boot` blocks on the engine's context-ready signal
(up to 15s, then falls back to reader-only) and `wgpu_diag` adapter logging
(which creates a wgpu Instance) runs after it. `WGPU_BACKEND` stays
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
  box aborts the process. Changing the value needs `cargo clean -p stylo`
  (at least) since Cargo does not track env changes in fingerprints.

## Feature flag

`servo-engine = ["dep:servo", "dep:dpi", "dep:euclid", "dep:url", "dep:surfman", "dep:embedder_traits"]`,
**on by default** (single exe): plain `cargo run` / `cargo test` / CI builds
the engine. `servo` itself is `default-features = false` plus `bundled`,
`clipboard`, `js_jit`, `multiprocess` — upstream defaults minus `devtools`.
UI-only iteration skips Servo with `cargo run --no-default-features` (reader
backend) or `SDSC_NO_ENGINE=1` at runtime; the MSRV job uses
`--no-default-features` since the engine needs the pinned toolchain.

## Early Servo AX gaps (historical)

The first target source was Servo's accesskit tree; it withheld real controls
(textareas, `div[role=button]`, native buttons arrived as containers or not at
all). That is why `dom.rs` queries the live DOM instead. These gaps are no
longer blocking: targets come from `getClientRects`/DOM state, and
`notify_accessibility_tree_update` is a no-op in the backend.

## Known gaps

- **Cloudflare bot protection**: challenged sites cannot be used. Requires the
  CEF pivot (see [browser-hub-pivot.md](browser-hub-pivot.md)).
- Debug-build Servo performance dominates scroll/render timing; release is the
  reference for frame/scroll validation.
- Page-field IME/OSK still unbuilt ([browser-osk.md](browser-osk.md)).
- `notes/wgpu-image-atlas-crash.md` Crash D (display sleep → surface loss →
  PS button → atlas panic) remains a separate recovery gap.
- Reader fallback only covers text pages and is being removed with Servo.
