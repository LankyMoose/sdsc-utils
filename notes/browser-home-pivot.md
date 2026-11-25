# Browser Home pivot — managed apps + CEF engine (planned)

**Status:** in progress on `browser/main` (the pivot line merged in at
`d245777`).
Phases 0–4 are complete; phase 5 (DLC packaging/install + catalog) is
partially landed. The engine package, resolver, catalog/install UI, and dev
download path exist; the Store API wiring and optional-package submission
remain.
**When to read:** before touching Launcher/browser window ownership, the PS
overlay, app selection, the engine backend, or MSIX packaging.

> **Ownership update:** the target product model is now the **Browser as a
> separate optional app** (UI + engine), not only the CEF engine DLC. The
> CEF/Home/DLC history here remains useful, but the packaging/ownership plan
> is revised by [controller-os-handoff.md](controller-os-handoff.md).
>
> **v0 update (Phases A–D process split landed):** the Browser is chrome-less,
> single-tab, and runs in the `sdsc-browser` add-on (window + CEF + page
> surface + cursor + text session). The OSK is system owned;
> L1/R1/Options map to semantic back/forward/reload; popups are denied. The
> in-process Browser window path is removed. MSIX/Store packaging remains.

Current browser internals: [browser-embedding.md](browser-embedding.md).

## Goal

Split the controller browser out of the Launcher and make it a standalone
managed app. PS opens a small overlay **Home** listing installed apps (Launcher
first, Browser second when its engine is installed, ＋ tile for adding optional
packages). Future controller-first apps register the same way.
Browser runs its own fullscreen window with its own chrome. Home, Launcher, and
Browser are all managed by sdsc-utils (tray + HID + iced daemon, one process).

Engine target: **CEF OSR** as a Store/standalone DLC. Servo is deleted with no
reader fallback (viability already proven; no `A11yTree`-dependent
half-measures, no `fetch.rs` text fallback).

## Locked decisions

- **CEF, not WebView2-child, not headless-CDP.** Only CEF preserves the
  offscreen-texture design cross-platform (Windows/macOS/Linux,
  x86_64+ARM64) with one API, one `OnPaint` → `FrameSlot` path, one input map.
  WebView2 composition+capture is Windows-only and would force three
  backends.
- **Accelerated paint in two levels.** L1: GPU-composited Chromium + CPU
  `OnPaint` into the existing `queue.write_texture` pipeline (stable-texture
  primitive untouched). L2 later, gated on frame-time data: zero-copy
  `OnAcceleratedPaint` shared-texture import (D3D11/Metal/DMA-BUF; fights the
  Vulkan-preferred `WGPU_BACKEND` on Windows — explicit risk).
- **No single-`.exe`.** CEF ships as sidecar files next to the exe (`libcef`,
  `*.pak`, `locales/`, ANGLE/SwiftShader); helpers reuse our exe via `--type=`
  branching. The exe delay-loads `libcef.dll` (`build.rs`), so a bundle
  without the engine still starts and can install it from the catalog.
  "Self-contained" = one installer/bundle (one `.msix` + related set),
  offline-capable, no system-browser dependency.
- **No Servo fallback.** Delete `servo_backend.rs`, the `servo-engine` feature
  and `servo/surfman/dpi/euclid` deps, `declare_surfman!`, the rustls pin
  (keep what `ureq` needs for `platform/updater.rs`), Stylo stack env,
  Clang/Python build prerequisites, `SDSC_NO_ENGINE`, and `fetch.rs` in full —
  except `normalize_url` + `resolve_navigate_target`, which move to the
  Browser app as the address-bar funnel. `--no-default-features` becomes
  "UI-only iteration build" (browser tile shows install UI).
- **Home tile policy:** Browser tile always visible; engine missing → inline
  Install affordance. ＋ tile opens the catalog.
- **Window levels:** Launcher + Browser = `Level::Normal` + bring-to-front
  (`focus_window`), no `raise_window_topmost`, out of the raise-above-toast
  set. Toast/popup/Settings keep current levels. **Home is the exception:**
  `AlwaysOnTop`, raised last (above toast/Settings/popup). Topmost still loses
  to exclusive-fullscreen scanout — documented limitation, best-effort only.
- **PS:** toggles Home from anywhere, including over running games. Pad works
  unfocused (HID is direct); mouse/keyboard in Home over a focused game are
  best-effort. Auto-open behavior (toast-first latch, USB scope, `AfterToast`
  settle, PS-chord suppression, `WindowClosed` retry) moves from Launcher to Home;
  pref key migrates (`Never`/`Bluetooth`/`Any`, default `Any`).
- **Browser window:** fullscreen immersive, own tab strip + toolbar +
  bookmarks + page + status strip (current geometry, minus the Launcher
  wash/card surround).

## Target shape

```
sdsc-utils (tray + HID + manager)
├── Home overlay (small, topmost) — PS toggle, auto-open target
│   ├── Launcher (always)
│   └── Browser (or install tile) — Triangle opens actions/Info
├── Launcher app (Normal window — games, scan, art; browser fields removed)
└── Browser app (Normal fullscreen window — own engine + chrome)
```

Home is modal: owns the pad while open; the underlying app keeps its last frame
but takes no input. Close (PS/Circle/launch) returns focus to the previous app.

## Phases (each lands green; `bash scripts/ci.sh`)

0. **Extract state, no behavior change (Servo still working). — landed.**
   `BrowserPanel`/tabs/store/chrome/cursor moved to
   `src/ui/browser/state.rs`; panel + engine handle + capability flag +
   browser D-pad stepper now live in `BrowserAppState`
   (`src/app/browser.rs`). `LauncherState` keeps only a `browser_open` mirror
   for pure view helpers. `AppManager` was replaced by `HomeApps`
   (`src/app/home.rs`, `AppId { Launcher, Browser }`, install/active/focus,
   open/close/suspend/resume); Launcher still renders the panel until phase 2.
   `tabs.json`/`bookmarks.json` paths unchanged.
1. **Home window + PS retarget + auto-open moved. — landed.** New
   `src/ui/home/` (tiles, selection, pad/key edges) + `src/app/home.rs`
   (window lifecycle, topmost raise, PS toggle). PS opens Home from
   Launcher, desktop, or a running game; while open Home owns pad input and
   Launcher messages are gated. Auto-open/session latch, toast settle, retry,
   and cooldown all target Home (`SessionEffect::OpenHome`,
   `AfterToast::OpenHome`); `ApplyContext.home_visible` replaces
   `launcher_visible` for the gate. Launcher moved from `AlwaysOnTop` to
   `Normal`; Home is raised above Settings/popup, with the toast raised
   last above all shell surfaces. Home window always covers the monitor
   and dims the screen behind a centered card. The card is compact and
   icon-first (large glyph + title, no subtitles, no header/close chrome):
   Launcher / Browser / Add apps tiles. Add apps opens a tiles grid (Launcher /
   Browser); each tile's X affordance opens a per-app popup with
   Install/Uninstall/Info, and uninstalled entries are dimmed. **Later
   superseded:** the Add apps tile/screen was removed; Triangle on the
   Browser tile now opens the same actions/Info popup directly. Launcher and
   Browser are mutually exclusive; opening either dismisses the other. Over
   exclusive-fullscreen games the topmost Home can still be covered
   (documented limitation).
2. **Browser standalone window. — landed.** New fullscreen `Level::Normal`
   Browser window (`app::browser::open_browser_window`); Launcher no longer
   renders or owns the panel. Rendering moved to `src/ui/browser/view.rs`
   (`root_view`, `fullscreen_layout`) and all browser messages are
   `BrowserMessage` (no `LauncherMessage::Browser*` left). Browser owns pad
   routing (`on_browser_pad`: cursor, D-pad jumps, history selector, R3
   flag), keyboard (`on_browser_key`), its own 60Hz `BrowserFrame` tick, and
   window close/suspend/focus return. Home is routed before Browser; Browser
   before Launcher. The live window renders a **16:9 card**, height-driven at
   `window height - 100px` (50px breather per side), centered on the monitor
   with the tab/toolbar/bookmark header inside it; `browser_layout` is the
   single geometry source for both the card and the engine viewport.
3. **CEF backend behind the existing `EngineCommand`/`EngineEvent` seam. —
   landed (dev).** `src/browser/cef_backend.rs` owns CEF init + a worker;
   `FrameSlot` and the custom shader pipeline are reused (the frame texture
   is plain `Rgba8Unorm` now). L1 CPU `OnPaint` swizzles BGRA → RGBA into the
   tab slot. Reuses `dom_query_js` through a
   renderer-process message bridge (no A11y forcing/clip walks; `dom.rs`
   keeps the JS, CEF parses JSON in the browser process). Commands covered:
   navigate, tabs (open/close/activate), resize, visibility/suspend, back/
   forward, reload, mouse move/click/wheel, keyboard, click-at, DOM reseed.
   Navigation recenters the existing cursor so the live viewport bounds
   survive. `--type=` subprocesses re-exec the exe and run `CefExecuteProcess`
   before argument parsing. During phase 3 CEF was behind `cef-engine`, mutually
   exclusive with `servo-engine`; it needs the static MSVC CRT because
   `libcef_dll_wrapper` is built `/MT`. Sandbox is disabled in this phase;
   popups (`window.open`/`target=_blank`) are denied; canonical
   `HistoryChanged` is not emitted yet. Favicons are fetched from CEF's icon
   URLs (and `/favicon.ico` for bookmarks). Engine-presence/DLC install
   remains phase 5.
4. **Delete Servo + reader, flip to CEF. — landed.** `servo_backend.rs`,
   the `servo-engine` feature/deps, `declare_surfman!`, Stylo stack env,
   Clang/Python build steps, `SDSC_NO_ENGINE`, and `fetch.rs` are gone;
   `normalize_url`/`resolve_navigate_target` live in `browser/nav.rs`. CEF is
   the default feature; `.cargo/config.toml` sets the static MSVC CRT for
   Windows; `--no-default-features` is UI-only. The Browser tile/install
   placeholder handles a missing engine (no reader page). CI now installs
   CMake/Ninja instead of LLVM.
5. **DLC install + catalog. — partially landed.** Main MSIX stays lean;
   `BrowserEngine.AppxManifest.xml` + `pack-msix.ps1 -CefDir` emit the
   optional package (runtime files under `cef\`). The engine resolver searches
   portable sidecars, the optional package, `LocalAppData`, then the debug
   build dir, and validates the required runtime files. Home ＋ opens a catalog
   card with Browser engine status/install/uninstall and controller focus
   (D-pad primary/Back, Cross select); Browser on a missing engine opens the
   catalog instead of a dead page. Dev install uses a DLC manifest
   (`SDSC_CEF_DLC_MANIFEST_URL`, or env-only
   `SDSC_CEF_DLC_URL`/`SDSC_CEF_DLC_SHA256`), SHA-256 verification, and
   extraction to `LocalAppData`. Installs are versioned
   (`cef-engine-<stamp>` + `cef-engine.current`) and carry `engine.json`
   (version/changelog/size). Home checks the manifest on open, shows
   **Update ready** when the feed is newer, and Info shows the engine version,
   size on disk, pending cleanup, and changelog. Install/update is cancellable
   (cooperative token; staging removed; previous state restored), and a running
   CEF keeps the old runtime until restart (`Restart to finish`). Uninstall
   drops the pointer and defers directory cleanup to the next boot. Home
   Settings owns Auto open, UI sound/haptic prefs, and the standalone-only
   app-install download cap (`Unlimited / 2 / 5 / 10 MB/s`, live during a download and
   shared by every future managed-app installer); the tile shows the live
   `current / target` size and MB/s. `SDSC_MSIX_ENGINE_DIR` mocks the
   optional package for resolver/managed-state testing. Because the app binary delay-loads CEF, a
   truly engine-less portable starts; the install path loads `libcef.dll` and
   attaches the engine in the same process. Remaining: Store
   `RequestDownloadAndInstallStorePackagesAsync` wiring/product IDs,
   AppInstaller/`PackageManager` sideload packaging, Partner Center optional
   package submission, and the generic `AppManifest` catalog for future apps.

## CEF build (default)

Prerequisites (Windows): CMake + Ninja on `PATH` (winget
`Kitware.CMake`, `Ninja-build.Ninja`). CEF's binary distribution (~427 MB)
downloads into `OUT_DIR` on first build; set `CEF_PATH` to share it.

```bash
cargo run                         # CEF OSR, static CRT from .cargo/config.toml
cargo check --no-default-features # UI-only; Browser tile shows install state
```

## Open verifications (before committing to Phase 4)

- Dummy native DLL in a related set loads via `LoadPackagedLibrary`
  (dependency-graph search) — proves plumbing without CEF.
- Real CEF paths: `CefSettings` roots into optional install, `user_data_dir`
  writable outside the package, GPU + sandbox + `--type=` subprocesses healthy
  under `runFullTrust`/`mediumIL`.
- Frame latency vs Servo (expect +0–1 frames L1), 60Hz scroll, dock/jump
  traces unchanged, no per-frame handle churn.
- Cloudflare-blocked sites pass; `cf_clearance` persists per profile.
  **Status:** CEF renders the interactive challenge (`nowsecure.nl`) where
  Servo never did; solving it with a pad click is the remaining manual check.
- Optional-package install: pack both MSIXs and confirm the engine resolver
  finds `<package>/cef`; Store API install still needs assigned Store IDs.
- Manual matrix: taskbar/Alt-Tab, toast-over-app, auto-open borderless vs
  exclusive-fullscreen, Home→app focus transfer, game-over-Home input
  coexistence.

## Viability (DLC route)

Store 8/10 (integrity/delta/uninstall managed; costs: approval + lockstep
resubmissions + ~300MB download UX). Standalone/dev fallback 6/10 but
mandatory regardless. Overall **7.5/10** — seamless if one installer/bundle
counts as self-contained; literal single-`.exe` is impossible for any real
Chromium and is out of scope by decision.
