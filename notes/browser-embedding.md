# Browser embedding: CEF OSR in the Browser app

**Status:** current (phase 4). CEF OSR is the default engine; Servo and the
reader fallback are deleted. The pivot plan and remaining phases live in
[browser-home-pivot.md](browser-home-pivot.md).

**When to read:** anything touching `src/browser/`, the standalone Browser
window, the engine worker, DOM targets, or the CEF packaging path. For a
capability/API lookup, start with [cef-api.md](cef-api.md).

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
  package. The exe delay-loads `libcef.dll`, so a bundle with no engine is
  runnable and the catalog can install it (and attach CEF) without a restart.

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
  until engine-created tab adoption lands. Canonical history comes from
  `CefBrowserHost::GetNavigationEntries` and is projected into the UI stack;
  reload uses `CefBrowser::Reload`. Favicons come from
  `DisplayHandler::on_favicon_urlchange` and
  CEF `BrowserHost::DownloadImage`; bookmarks with no page visit yet prefetch
  the site's conventional `/favicon.ico` through the same path.
- `src/browser/dom.rs` — DOM target query JS. CEF polls it from the page's
  V8 context through a renderer-process message bridge; the browser process
  parses the JSON. Stable per-document ids, viewport boxes, visibility,
  overflow clipping, scroll truth, document extent, and autofocus id.
- `src/browser/cursor.rs` — controller cursor: analog free point, magnet
  dock with hysteresis/break-free, D-pad flights, unified page+header space,
  row-locked horizontals, R2 turbo.
- `src/browser/frame_slot.rs` + `src/ui/launcher/browser_frame.rs` — per-tab
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

- Analog scroll is accumulated once per `BrowserFrame` and applied as an
  instant `scrollBy` on the innermost scroller under the cursor (falling back
  to the window). Wheel events feed Chromium's scroll animator, which trails
  the stick and keeps gliding after release; the instant path stops the frame
  the stick centers. `--disable-smooth-scrolling` still covers real wheel
  input. The custom iced scrollbar was removed — Chromium owns the page
  scrollbar.
- Docked links use trusted CEF mouse input at the element center. If no
  navigation starts within 700ms, the backend falls back to
  `window.__sdsc_click` so a miss never leaves the link unclickable. Docked
  buttons use trusted mouse input by default; the Diagnostics JS-click toggle
  keeps the old untrusted A/B path. Precise/free clicks use trusted input at
  the point.
- `--force-color-profile=srgb` and a white `BrowserSettings.background_color`
  keep the OSR buffer aligned with the browser canvas.
- The OSR frame texture is **`Rgba8Unorm`, not `Rgba8UnormSrgb`**. iced renders
  into a non-sRGB target (its own image atlas is plain Unorm too), so an sRGB
  texture view decoded every midtone and the shader wrote the linear values
  raw: a `#808080` page read back as `#373737`. Verified against Edge with a
  solid-color probe — all eight patches are now byte-identical.
- Cursor bounds come from the live page area and never from `Cursor::new()`'s
  800x600 defaults. Navigation calls `reset_for_page()` (recenter, keep the
  live viewport) instead of replacing the cursor, and the browser takes its
  window size from `window::resize_events()` rather than a creation-time
  `window::size()` query, which can return the pre-fullscreen default.

PS opens the topmost Home over Launcher, Browser, or a game; while Home is
open it owns pad input and the app underneath is gated. Selecting Browser
closes Home and opens the standalone fullscreen Browser window. Browser
close (Circle/Esc with no modal) suspends the engine and returns focus to
Launcher when it is visible.

Input priority is **Home → Browser → Launcher**. The Browser window has its own
pad bank (`BrowserAppState::nav`) and D-pad stepper; Launcher's bank no longer
sees browser presses. PS is checked in the Browser pad path too, so it opens
Home over Browser exactly like over Launcher.

Face/Options/Create/L3/R3 taps are normalized at the HID source by
`domain::pad::PadInterpreter`: raw drained reports are debounced, and ordered
`PadAction::Tap` events travel with the input edge. `PadActionGate` dedupes the
UI sequence and keeps actions suppressed until a pad reaches rest after an owner
change (Home/Browser/Launcher). L3 is a release tap; Browser maps its single
action to the cursor-magnet toggle. Bumper tap/hold is also a source action
(L1/R1 `Tap`/`Hold`); analog/D-pad repeat and the Cross/Triangle hold progress
meters still use the legacy per-surface bank.

- Left stick moves the cursor with velocity + friction; magnet docks on
  arrival, centre-snaps on fresh acquisition, holds in place while scrolling.
- D-pad: vertical is one row per press with column preserved; horizontal is
  row-locked and stops at row ends. D-pad Up/Down is the explicit
  header/page crossing; the analog stick may cross freely.
- R2 turbo doubles cursor acceleration/cap and scroll speed while held and
  disables docking; release re-snaps in place.
- Right stick scrolls only while the cursor is over the page. Scroll
  commands go straight to the engine as instant JS deltas (no wheel
  animation).
- Cross clicks the docked target (chrome target first, then exact-element
  click, else point click); Circle closes/backs out; Triangle edits the URL;
  L1/R1 travel history (tap) or open the history selector (hold); L3 toggles
  magnet; R3 dumps an issue bundle.
- Real keyboard input goes to the page as `RAWKEYDOWN` + `CHAR` + `KEYUP`
  (text needs the separate `CHAR` leg). `Ctrl+L`/`Ctrl+R` and
  `Alt+Left`/`Alt+Right` stay app shortcuts; the URL field owns printable
  keys while editing.
- Real page mouse supports left click, double/triple-click word/paragraph
  selection, drag selection, and native CEF wheel (lines × 120); CEF cursor
  shapes drive the OS cursor over the page. Right/middle click and a custom
  context menu are still unwired.
- Page `window.open` / `target=_blank` is canceled as a native popup and
  opened as a normal foreground tab instead. The source tab is preserved.
- The on-screen keyboard (Square, or F6 on a hardware keyboard) types into
  the focused page field or the URL draft; page IME flows through
  `ime_bridge` and CEF composition calls. See
  [browser-osk.md](browser-osk.md).
- Automated sessions set `SDSC_DEBUG_INPUT=0`, which locks real pad,
  keyboard, mouse, and touch input (`ui::input_lock`). Quiet sessions also
  suppress the startup toast and controller auto-open Home. Replay drives the
  same handlers through synthetic `text`, `key`, `mouse.click`, `mouse.drag`,
  `mouse.wheel`, `ime.*`, and `osk.*` steps and closes the Browser window at
  `replay.done`; never use OS input injection for QA and do not leave test
  windows on screen.
- Tabs: favicon (CEF icon URLs, 64px cache keyed by origin) + minimum-width
  title + close, active highlighted; crowded strips switch to an overflow
  chip instead of collapsing titles. Background tabs suspend, strip
  persists/restores lazily.
- The saved strip is not written until the first Browser open has restored
  it. The engine preloads the saved active URL at boot and its early
  `TitleChanged`/`UrlChanged` events used to persist a one-tab strip before
  the restore read the file; `BrowserAppState::tabs_restored` gates that.
- On alpha-capable backends the Browser root dims the whole screen with a
  55% black scrim behind the 16:9 card (`theme::BACKDROP_ALPHA`); opaque
  backends keep the flat backdrop. The cursor is recentered in the live page
  area every time the window opens.
- The Home window always covers the monitor and draws the same scrim; the
  card is centered and scales with the stage (icon tiles, no subtitles). On
  an opaque backend the scrim blends over the opaque clear color.

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
- **History is canonical**: `GetNavigationEntries` snapshots replace the
  URL-change inference; the UI stack is a projection. Selector jumps resolve
  the CDP entry id and call `Page.navigateToHistoryEntry` (exact index,
  skippable entries addressable, forward tail preserved); L1/R1 remains one
  `GoBack`/`GoForward` step. Favicons use CEF `DownloadImage` for HTTP(S)
  (Chromium cache/network stack; `is_favicon` disables cookies), with the
  local `favicon.rs` decoder as the `data:`/failure fallback and
  `/favicon.ico` fallback.
- **Accelerated OSR not wired**: L1 CPU `OnPaint` only; `OnAcceleratedPaint`
  shared-texture import is the L2 performance step.
- **macOS/Linux untested** for the CEF backend.
- `notes/wgpu-image-atlas-crash.md` Crash D (display sleep → surface loss →
  PS button → atlas panic) remains a separate recovery gap.
