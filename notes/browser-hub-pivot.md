# Browser hub pivot — managed apps + CEF engine (planned)

**Status:** planned (branch `browser/hub-pivot`, based on `browser/main`).
**When to read:** before touching Start/browser window ownership, the PS
overlay, app selection, the engine backend, or MSIX packaging.

This note is the agreed direction, not a description of current code. Current
browser internals: [browser-embedding.md](browser-embedding.md).

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

0. **Extract state, no behavior change (Servo still working).** Move
   `BrowserPanel`/tabs/store/chrome/cursor/engine out of `StartState` into a
   top-level `BrowserAppState` (same file paths for `tabs.json`/
   `bookmarks.json`). Promote `AppManager` to a real top-level hub
   (`AppId { Start, Browser }`, install/active/focus, open/close/suspend/
   resume). Repoint the ~100 `start_state.browser.*` sites.
1. **Hub window + PS retarget + auto-open moved.** New `src/ui/hub/` +
   `src/app/hub.rs`, same open/close/focus pattern as popup/configure. Hub is
   the topmost exception. Pref migration + toast-settle latch + over-game
   focus behavior as explicit acceptance.
2. **Browser standalone window.** Move panel rendering, cursor/scroll/D-pad
   routing (`tick_browser_cursor`, jumps, clicks, `sync_browser_mouse`) and
   all `StartMessage::Browser*` into `BrowserMessage`. Settings/history/
   bookmark-bubble/URL-edit move with it.
3. **CEF backend behind the existing `EngineCommand`/`EngineEvent` seam.**
   New `src/browser/cef_backend.rs`; `FrameSlot` + shader pipeline unchanged
   (L1 CPU `OnPaint`). DOM contract: fragments from `getClientRects()` +
   `checkVisibility()` + `elementsFromPoint`/`findNext` + exact `click()`;
   delete forcing, verdict cache, `clipRect` walks, cursor-cull/`ccull`,
   rebase/settle/starvation logic. Engine-presence check (optional package
   path valid + `libcef`/resources present, else Store/`LocalAppData`/dev
   fallback).
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

## Open verifications (before committing to Phase 4)

- Dummy native DLL in a related set loads via `LoadPackagedLibrary`
  (dependency-graph search) — proves plumbing without CEF.
- Real CEF paths: `CefSettings` roots into optional install, `user_data_dir`
  writable outside the package, GPU + sandbox + `--type=` subprocesses healthy
  under `runFullTrust`/`mediumIL`.
- Frame latency vs Servo (expect +0–1 frames L1), 60Hz scroll, dock/jump
  traces unchanged, no per-frame handle churn.
- Cloudflare-blocked sites pass; `cf_clearance` persists per profile.
- Manual matrix: taskbar/Alt-Tab, toast-over-app, auto-open borderless vs
  exclusive-fullscreen, hub→app focus transfer, game-over-hub input
  coexistence.

## Viability (DLC route)

Store 8/10 (integrity/delta/uninstall managed; costs: approval + lockstep
resubmissions + ~300MB download UX). Standalone/dev fallback 6/10 but
mandatory regardless. Overall **7.5/10** — seamless if one installer/bundle
counts as self-contained; literal single-`.exe` is impossible for any real
Chromium and is out of scope by decision.
