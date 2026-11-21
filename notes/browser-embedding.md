# Browser embedding: CEF OSR in the Browser app

**Status:** current (phase 4). CEF OSR is the default engine; Servo and the
reader fallback are deleted. The pivot plan and remaining phases live in
[browser-hub-pivot.md](browser-hub-pivot.md).

**When to read:** anything touching `src/browser/`, the standalone Browser
window, the engine worker, DOM targets, or the CEF packaging path.

## Decision

- Standalone Browser window + open web + no DRM → CEF offscreen rendered as
  an iced texture. CEF was chosen over a `wry`/WebView2 child HWND because
  the native child draws above the iced/wgpu canvas (toasts, idle/sleep
  veils, and overlay surfaces cannot composite over it) and WebView2 is
  Windows-only. One CEF API gives one `OnPaint` → `FrameSlot` path.
- Servo was the phase-0/1 backend. Chromium's real TLS/HTTP2 fingerprint and
  V8 environment are required for managed bot challenges; Servo could not
  pass them. Servo, its build prerequisites, `fetch.rs`, and the reader
  fallback are gone. `--no-default-features` is now a UI-only iteration
  build; a missing engine shows the install state, never a text page.
- CEF ships as sidecar files next to the exe (`libcef.dll`, `*.pak`,
  `locales/`, ANGLE/SwiftShader). A literal single `.exe` is impossible; the
  packaging phase makes this one installer/bundle with an optional engine
  package.

## Layout

- `src/browser/engine.rs` — engine seam: `EngineCommand` down,
  `EngineEvent` up, `EngineHandle` channel pair. The UI depends only on this
  seam.
- `src/browser/cef_backend.rs` — CEF worker. `CefInitialize` runs on the
  main thread; Windows uses CEF's multi-threaded message loop. One async OSR
  `Browser` per tab; `on_paint` swizzles BGRA → RGBA into the tab's
  `FrameSlot`. Covers navigate/tabs/resize/visibility/suspend/back/forward/
  reload/mouse/key/wheel/click-at/DOM reseed. `--type=` helpers re-exec the
  exe and run `CefExecuteProcess` before argument parsing. Popups are denied
  until engine-created tab adoption lands; favicons and canonical
  `HistoryChanged` are not emitted yet.
- `src/browser/dom.rs` — DOM target query JS. CEF polls it from the page's
  V8 context through a renderer-process message bridge; the browser process
  parses the JSON. Stable per-document ids, viewport boxes, visibility,
  overflow clipping, scroll truth, document extent, and autofocus id.
- `src/browser/cursor.rs` — controller cursor: analog free point, magnet
  dock with hysteresis/break-free, D-pad flights, unified page+header space,
  row-locked horizontals, R2 turbo.
- `src/browser/frame_slot.rs` + `src/ui/start/browser_frame.rs` — per-tab
  latest RGBA; a custom `iced_wgpu` primitive uploads unseen generations into
  one stable texture (`queue.write_texture`). No per-frame image handles.
- `src/browser/tabs.rs`, `src/browser/bookmarks.rs` — the two persisted
  browser stores (`tabs.json`, `bookmarks.json`). Restored tabs load lazily;
  engine state never crosses a restart.
- `src/browser/nav.rs` — address-bar funnel (`normalize_url`,
  `resolve_navigate_target`).
- `src/browser/keys.rs` — iced → engine keyboard mapping.
- `src/ui/browser/state.rs` / `view.rs` — panel state + standalone-window
  rendering/chrome and `BrowserMessage`.
- `src/app/browser.rs` — top-level `BrowserAppState`, window lifecycle,
  pad/key routing, `BrowserMessage` reducer.

## Current input/UX contract (pad)

- The analog scroll is accumulated and flushed once per `BrowserFrame`
  instead of one wheel event per pad poll; Chromium's smooth-scroll easing is
  disabled via `--disable-smooth-scrolling`. The custom iced scrollbar was
  removed — Chromium owns the page scrollbar.
- Docked clicks use `window.__sdsc_click`, defined by the DOM query itself
  (the observer install script is gone). Precise/free clicks use trusted
  synthetic events at the point.
- `--force-color-profile=srgb` and a white `BrowserSettings.background_color`
  keep the OSR buffer aligned with the sRGB texture and the browser canvas.

PS opens the topmost hub over Start, Browser, or a game; while the hub is
open it owns pad input and the app underneath is gated. Selecting Browser
closes the hub and opens the standalone fullscreen Browser window. Browser
close (Circle/Esc with no modal) suspends the engine and returns focus to
Start when it is visible.

Input priority is **hub → Browser → Start**. The Browser window has its own
pad bank (`BrowserAppState::nav`) and D-pad stepper; Start's bank no longer
sees browser presses. PS is checked in the Browser pad path too, so it opens
the hub over Browser exactly like over Start.

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
- Tabs: favicon (not emitted by CEF yet) + ellipsized title + close, active
  highlighted; background tabs suspend, strip persists/restores lazily.

## Build prerequisites (Windows)

CEF's `cef-dll-sys` downloads its binary distribution (~427 MB) into
`OUT_DIR` on first build; set `CEF_PATH` to share it. It builds
`libcef_dll_wrapper` with CMake + Ninja, so both must be on `PATH`
(winget `Kitware.CMake`, `Ninja-build.Ninja`).

`libcef_dll_wrapper` is compiled with the static MSVC CRT (`/MT`) — CEF's
sandbox also requires it — so `.cargo/config.toml` sets
`target-feature=+crt-static` for Windows targets. Do not flip Windows back to
the dynamic CRT without changing the CEF wrapper build too.

## Feature flag

`cef-engine = ["dep:cef"]`, **on by default**. Plain `cargo run` / `cargo
test` / CI builds CEF. UI-only iteration uses `cargo check` /
`cargo run --no-default-features` (no CEF compile; Browser tile shows the
install state). The MSRV job also uses `--no-default-features` because the
CEF toolchain needs the pinned compiler.

## Runtime paths

- `CefSettings.root_cache_path` / `log_file`: `<data dir>/cef/`. Profile
  cookies (including Cloudflare clearance) persist there.
- CEF resources/locales: the distribution directory resolved by
  `cef::sys::get_cef_dir()` (`CEF_PATH` or build `OUT_DIR` for dev).
- Packaging (phase 5): the optional `BrowserEngine` MSIX stores CEF under
  `cef\`; the resolver checks that package path, portable `cef\`, and the
  dev `LocalAppData` install before falling back to the build directory.

## Known gaps

- **Sandbox off**: `CefSettings.no_sandbox = 1` in the dev backend; enabling
  it needs the Windows bootstrap/helper path and the static CRT (already in
  place).
- **Popups denied**: `window.open`/`target=_blank` are canceled until the UI
  adopts engine-created tabs.
- **No favicons / canonical history events**: tab titles and URLs work;
  favicon cache and exact back/forward stacks come later.
- **Accelerated OSR not wired**: L1 CPU `OnPaint` only; `OnAcceleratedPaint`
  shared-texture import is the L2 performance step.
- **macOS/Linux untested** for the CEF backend.
- Page-field IME/OSK still unbuilt ([browser-osk.md](browser-osk.md)).
- `notes/wgpu-image-atlas-crash.md` Crash D (display sleep → surface loss →
  PS button → atlas panic) remains a separate recovery gap.
