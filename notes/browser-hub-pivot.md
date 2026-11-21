# Browser hub pivot — managed apps + CEF engine (planned)

**Status:** in progress (branch `browser/hub-pivot`, based on `browser/main`).
Phases 0–3 landed: state extraction, PS hub + auto-open, the standalone
Browser window, and the CEF OSR backend. Phase 4 (delete Servo, flip the
default engine) is next.
**When to read:** before touching Start/browser window ownership, the PS
overlay, app selection, the engine backend, or MSIX packaging.

Current browser internals: [browser-embedding.md](browser-embedding.md).

## Goal

Split the controller browser out of the Start screen and make it a standalone
managed app. PS opens a small overlay **hub** listing installed apps (Start
Screen Launcher first, Browser second when its engine is installed, ＋ tile for
adding optional packages). Future controller-first apps register the same way.
Browser runs its own fullscreen window with its own chrome. Hub, Start, and
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
  branching. "Self-contained" = one installer/bundle (one `.msix` + related
  set), offline-capable, no system-browser dependency.
- **No Servo fallback.** Delete `servo_backend.rs`, the `servo-engine` feature
  and `servo/surfman/dpi/euclid` deps, `declare_surfman!`, the rustls pin
  (keep what `ureq` needs for `platform/updater.rs`), Stylo stack env,
  Clang/Python build prerequisites, `SDSC_NO_ENGINE`, and `fetch.rs` in full —
  except `normalize_url` + `resolve_navigate_target`, which move to the
  Browser app as the address-bar funnel. `--no-default-features` becomes
  "UI-only iteration build" (browser tile shows install UI).
- **Hub tile policy:** Browser tile always visible; engine missing → inline
  Install affordance. ＋ tile opens the catalog.
- **Window levels:** Start + Browser = `Level::Normal` + bring-to-front
  (`focus_window`), no `raise_window_topmost`, out of the raise-above-toast
  set. Toast/popup/Settings keep current levels. **Hub is the exception:**
  `AlwaysOnTop`, raised last (above toast/Settings/popup). Topmost still loses
  to exclusive-fullscreen scanout — documented limitation, best-effort only.
- **PS:** toggles hub from anywhere, including over running games. Pad works
  unfocused (HID is direct); mouse/keyboard in hub over a focused game are
  best-effort. Auto-open behavior (toast-first latch, USB scope, `AfterToast`
  settle, PS-chord suppression, `WindowClosed` retry) moves from Start to hub;
  pref key migrates (`Never`/`Bluetooth`/`Any`, default `Any`).
- **Browser window:** fullscreen immersive, own tab strip + toolbar +
  bookmarks + page + status strip (current geometry, minus the Start
  wash/card surround).

## Target shape

```
sdsc-utils (tray + HID + manager)
├── Hub overlay (small, topmost) — PS toggle, auto-open target
│   ├── Start Screen Launcher (always)
│   ├── Browser (or install tile)
│   └── ＋ Add apps → catalog (Store DLCs + future apps)
├── Start app (Normal window — games, scan, art; browser fields removed)
└── Browser app (Normal fullscreen window — own engine + chrome)
```

Hub is modal: owns the pad while open; the underlying app keeps its last frame
but takes no input. Close (PS/Circle/launch) returns focus to the previous app.

## Phases (each lands green; `bash scripts/ci.sh`)

0. **Extract state, no behavior change (Servo still working). — landed.**
   `BrowserPanel`/tabs/store/chrome/cursor moved to
   `src/ui/browser/state.rs`; panel + engine handle + capability flag +
   browser D-pad stepper now live in `BrowserAppState`
   (`src/app/browser.rs`). `StartState` keeps only a `browser_open` mirror
   for pure view helpers. `AppManager` was replaced by `AppHub`
   (`src/app/hub.rs`, `AppId { Start, Browser }`, install/active/focus,
   open/close/suspend/resume); Start still renders the panel until phase 2.
   `tabs.json`/`bookmarks.json` paths unchanged.
1. **Hub window + PS retarget + auto-open moved. — landed.** New
   `src/ui/hub/` (tiles, selection, pad/key edges) + `src/app/hub.rs`
   (window lifecycle, topmost raise, PS toggle). PS opens the hub from
   Start, desktop, or a running game; while open the hub owns pad input and
   Start messages are gated. Auto-open/session latch, toast settle, retry,
   and cooldown all target the hub (`SessionEffect::OpenHub`,
   `AfterToast::OpenHub`); `ApplyContext.hub_visible` replaces
   `start_visible` for the gate. Start moved from `AlwaysOnTop` to
   `Normal`; the hub is raised last above toast/Settings/popup. Over
   exclusive-fullscreen games the topmost hub can still be covered
   (documented limitation).
2. **Browser standalone window. — landed.** New fullscreen `Level::Normal`
   Browser window (`app::browser::open_browser_window`); Start no longer
   renders or owns the panel. Rendering moved to `src/ui/browser/view.rs`
   (`root_view`, `fullscreen_layout`) and all browser messages are
   `BrowserMessage` (no `StartMessage::Browser*` left). Browser owns pad
   routing (`on_browser_pad`: cursor, D-pad jumps, history selector, R3
   flag), keyboard (`on_browser_key`), its own 60Hz `BrowserFrame` tick, and
   window close/suspend/focus return. Hub is routed before Browser; Browser
   before Start. `browser_layout`/16:9 card remains only for the old Start
   card tests — the live window uses fullscreen geometry.
3. **CEF backend behind the existing `EngineCommand`/`EngineEvent` seam. —
   landed (dev).** `src/browser/cef_backend.rs` owns CEF init + a worker;
   `FrameSlot` and the shader pipeline are unchanged. L1 CPU `OnPaint`
   swizzles BGRA → RGBA into the tab slot. Reuses `dom_query_js` through a
   renderer-process message bridge (no A11y forcing/clip walks; `dom.rs`
   keeps the JS, CEF parses JSON in the browser process). Commands covered:
   navigate, tabs (open/close/activate), resize, visibility/suspend, back/
   forward, reload, mouse move/click/wheel, keyboard, click-at, DOM reseed.
   `--type=` subprocesses re-exec the exe and run `CefExecuteProcess` before
   argument parsing. CEF is behind `cef-engine`, mutually exclusive with
   `servo-engine` for now; it needs the static MSVC CRT because
   `libcef_dll_wrapper` is built `/MT`. Sandbox is disabled in this phase;
   popups (`window.open`/`target=_blank`) are denied; favicons and
   `HistoryChanged` are not emitted yet. Engine-presence/DLC install remains
   phase 5.
4. **Delete Servo + reader**, flip to CEF in the same landing (never ship a
   dead-browser main). Docs, `AGENTS.md`, CI/MSRV/toolchain updates included;
   keep the toolchain pin for one release, then unpin.
5. **DLC install + catalog.** Main MSIX stays lean; `BrowserEngine` optional
   package (all CEF files, one package) in a **related set** (mandatory for
   executable code). `pack-msix.ps1` grows bundle/mapping support. Store:
   `RequestDownloadAndInstallStorePackagesAsync(storeIds)` for first install;
   `GetAppAndOptional…` + `RequestDownload…UpdatesAsync` only for updates
   (UI thread + `InitializeWithWindow`). Sideload: AppInstaller
   `<OptionalPackages>` + `PackageManager`. Dev: `LocalAppData`
   download+hash-verify fallback. Missing/uninstalled engine → install UI.
   Future apps register as `AppManifest { id, title, icon, storeIds?,
   engineCheck? }` with per-app pad bindings. Needs Partner Center approval
   for optional packages; every Chromium security patch forces a
   main+optional resubmission (related-set lockstep) — the core
   Store-vs-self-managed tradeoff.

## CEF dev build (phase 3)

Prerequisites (Windows): CMake + Ninja on `PATH` (winget
`Kitware.CMake`, `Ninja-build.Ninja`); LLVM/Clang is only needed by Servo.
CEF's binary distribution (~427 MB) is downloaded by `cef-dll-sys` into
`OUT_DIR` on first build; set `CEF_PATH` to share it across build dirs.

```bash
# CEF backend (Servo off). Static CRT matches libcef_dll_wrapper's /MT.
RUSTFLAGS="-C target-feature=+crt-static"   cargo run --no-default-features --features cef-engine

# UI-only (no engine; Browser tile shows install state)
cargo check --no-default-features
```

`cef-engine` + `servo-engine` are mutually exclusive (clear compile error):
the two wrappers disagree on the MSVC runtime. Phase 4 removes Servo and
makes static CRT the Windows default.

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
  Servo never did; solving it with a pad click is the remaining check.
- Manual matrix: taskbar/Alt-Tab, toast-over-app, auto-open borderless vs
  exclusive-fullscreen, hub→app focus transfer, game-over-hub input
  coexistence.

## Viability (DLC route)

Store 8/10 (integrity/delta/uninstall managed; costs: approval + lockstep
resubmissions + ~300MB download UX). Standalone/dev fallback 6/10 but
mandatory regardless. Overall **7.5/10** — seamless if one installer/bundle
counts as self-contained; literal single-`.exe` is impossible for any real
Chromium and is out of scope by decision.
