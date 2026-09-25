# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.4.1] - 2026-09-25

### Added

- After a tray-mode panic, the app schedules a budgeted delayed relaunch and shows a one-shot toast asking for `app.log` from the data directory (bug icon). Undocumented `--test-crash-toast` exercises that path without crashing.

### Fixed

- Start-screen shell icons no longer rebuild a new iced `image::Handle` every frame (stable RGBA handles), reducing wgpu image-atlas churn that could panic after GPU device loss.
- Panic hook records a capped `PANIC_BACKTRACE` in `app.log` for renderer crashes.

## [1.4.0] - 2026-09-23

### Added

- **Start screen** launcher: when the first DualSense connects (0→1), an always-on-top quick-launch card can open with curated Steam games and manual shortcuts. Navigate with D-pad / combined analog sticks / keyboard; Cross or Enter launches; Circle or Escape dismisses. Opens with an empty catalog (empty copy + Square hint). Any connected DualSense can drive open / close / navigation (inputs are never merged across pads).
- Reopen anytime with a configurable controller chord (default **PS**); record or reset the gesture in Settings → **Start screen**. Catalog lives in `games.json`.
- Settings → **Start screen**: enable toggle, reopen-gesture recording, and optional UI sounds (nav / action / hold-complete) with volume.
- **Square** toggles edit mode: Steam checklist (Cross adds/removes) plus removable manuals; **Add shortcut…** (edit only) for title, optional args, and optional custom image; **Triangle** edits the selected manual. Edit is draft-based (**Square** Saves, **Circle** Cancels without closing).
- Browse mode DualSense **Options** (footer **START** hint) toggles sort between **Last played** and **A–Z** (preference in prefs). Successful launches record `last_played_ms`. Manual shortcuts support optional launch arguments and a custom icon path.
- Games ↔ Controllers header (L2/R2 to switch); item actions on the selected row; footer for Edit / Close / START. Steam icons use the modern `librarycache/{appid}/` layout (with legacy flat-name fallback); game art uses a shared 2:3 portrait cell.
- Discrete face / Options actions activate on **release** (pressed hint stays lit while held); L2/R2 and list nav stay on press. Hold actions (close game / power off / replace Proceed) complete on **release** only when the hold was full and not cancelled. Hold arcs turn muted when armed; face hints ease slightly larger while pressed (~100ms, in-bounds). Pending face releases cancel on slide/nav context changes. UI sounds play only when the action actually runs. Nav and sounds arm only after the window is shown.

### Changed

- DualSense HID I/O (lightbar / battery poll / Identify / power-off, and start-nav input when enabled) runs on a dedicated **hid-worker** thread. The UI clones a shared input snapshot and never opens HID on the iced thread, so Identify can interleave with short input reads on the same handle.
- While pad input is live (start screen, reopen gesture, Pad Input), HID sampling and UI snapshot apply run at about one wired DualSense report (~4 ms); presence / unread poll stays ~500 ms when no controllers are known.
- Darker, sharper Settings / Start / popup chrome (theme hierarchy, framed outlines, list separators).
- Overlay toasts stay topmost over other apps; when Start or Settings is open, those windows sit above the toast in the same topmost band so they keep presenting. Start opens concurrently with connect toasts (no longer waits on toast expiry).

### Fixed

- Multi-second UI freezes from constructing a new `HidApi` on every poll — the worker keeps one long-lived API and refreshes devices as needed.
- Windows multi-window toast present starvation and wrong battery % on queued connect toasts (remount toast surface on handoff; generation-guarded place).

## [1.3.3] - 2026-09-20

### Fixed

- Lightbar reasserts RGB on every ~5s poll with claim-once (`LIGHT_OUT` then RGB) per connection, without Steam-specific branching — works against any other HID writer that briefly owns the bar.
- Fresh connects always reclaim the lightbar (`LIGHT_OUT` + RGB), including after a presence-only disconnect that never ran a HID poll.

### Changed

- Default lightbar spectrum is `#0101FE` → `#8704FF` → `#FF0101` (full / mid / empty).
- Removed unused lightbar skip-unchanged / `force` poll knobs; polls always reassert after claim-once.
- HID layering: shared `dualsense` identity + lock; `battery` read-only; `poll` orchestrates read then lightbar apply under one lock.
- Thinned the iced daemon: tray, window placement, and Win32 FFI live in dedicated modules; Settings canvas programs split under `configure_view`.
- Presence scans use path keys (`list_presence_paths`); storable-serial checks go through `dualsense::is_storable_serial`.

## [1.3.2] - 2026-09-18

### Fixed

- Battery analytics ignores bucket-window starts for ~30 minutes after a controller connects, so the first (often wrong) DualSense readings cannot open or poison a step timer.
- DualSense HID write storms no longer surface as overlapped I/O errors (debounced lightbar applies, serialized battery reads/writes, skip unchanged RGB rewrites, single timeout retry).

## [1.3.1] - 2026-09-13

### Fixed

- MSIX package identity matches Partner Center (`LankyMoose.SDSCUtils` / Store publisher CN) so Store uploads validate.

## [1.3.0] - 2026-09-13

### Added

- Settings → System: **Open data folder** opens the app data directory in the file explorer.
- Settings → Lightbar: **Enable lightbar** toggle (on by default). When off, battery-driven colors and the low-battery pulse stop writing RGB; Identify still works. Existing prefs without the key stay enabled.

### Changed

- Renamed the product to **SDSC Utils** (`sdsc-utils`). DualSense is referenced only as compatible hardware.
- Windows Store / MSIX packaging (`packaging/`) with packaged Startup Task when installed from the Store; portable builds still use a Startup `.lnk`.
- Release workflow publishes `sdsc-utils.exe` and `sdsc-utils.msix`.
- Icon-only buttons show hover tooltips (controller popup actions and Settings close).

### Removed

- Settings → Analytics: **Clear recorded data** button (delete or edit files via **Open data folder** instead).

### Fixed

- Controller popup window height updates when controllers connect or disconnect while it is open.
- Controller list percent ring flashes white in sync with Identify (same timing as the lightbar).

## [1.2.1] - 2026-09-13

### Changed

- Controller popup and Settings are ephemeral again (create/destroy on each open). The overlay toast window is pre-created hidden at boot and kept as a GPU compositor sentinel so reopen stays fast without holding full UI surfaces idle.

### Fixed

- Settings no longer flashes white on close on Windows (DWM undecorated shadow removed; hide before destroy).
- Controllers popup no longer briefly shows an empty dark frame on dismiss (hide before destroy).

## [1.2.0] - 2026-09-13

### Changed

- Remaining-time estimates interpolate within the current DualSense percent bucket using accrued drain/charge time (clamped so ETA never drops below the next breakpoint).
- Remaining-time labels (tray/toast rings and Settings full charge/drain totals) always floor to duration tiers: 30-minute steps at ≥4h, 15-minute at ≥2h, 5-minute below 2h (e.g. `~3h 30m`).
- Controller popup and overlay toast rings are larger so longer ETA labels fit.
- Low-battery threshold is DualSense mid-points 5–35% (was 5–50% in 5% steps); non-observable values in saved prefs snap down to the next mid-point. The threshold slider is shown only when low-battery notifications are enabled.
- Lightbar hue picker is a rainbow spectrum strip instead of a plain slider.

## [1.1.0] - 2026-09-12

### Changed

- Battery analytics records per DualSense bucket-step windows instead of abortable charge/play sessions; mid-cycle interruptions clear only the in-progress timer.
- Remaining-time estimates unlock after one qualifying step (missing edges filled from median ms/%; the 100↔95 edge is excluded as a rate source).
- Settings labels use “Full charge …” / “Full drain …” for cycle totals; tray rings show compact `~7h` / `~30m`, including for remembered disconnected pads.

### Fixed

- Analytics Settings panel scrollbar no longer overlays content.

## [1.0.1] - 2026-09-12

### Fixed

- Overlay toasts on Windows no longer steal focus when they appear (games and other apps keep the foreground).

## [1.0.0] - 2026-09-11

### Added

- Opt-in **Battery analytics** (Settings → Analytics, off by default): records local charge and play cycles from DualSense power-state events (no extra polling) to estimate time-to-full and play time remaining. Needs one qualifying full charge and one drain from 100% (which may span several sittings). Stores a compact last-5 sample ring plus in-progress timeline waypoints in `analytics.json`; Clear recorded data wipes it. When enough data exists, the percent ring on the tray popup and overlay toasts shows a compact estimate (e.g. `est. 8h`).
- Developer mode analytics presets (seed estimates, charge/drain advances, pause/resume) for walking cycles without hardware.

### Changed

- Body text sizes and secondary colors are slightly larger/brighter across Settings, popup, and toasts for readability.

## [0.1.15] - 2026-09-11

### Added

- Configurable low-battery threshold (5–50%, default 5%) in Configure → Notifications; shared by the low-battery toast, orange lightbar pulse, and popup “low battery” label.
- Overlay toasts show a circular battery outline filled to the current percent, with the percentage in large type inside the ring.
- Controller popup rows use the same circular percent readout in place of the DualSense glyph.

### Changed

- Configure Settings uses a fixed left-hand tab list with content beside it instead of a single-open accordion.

## [0.1.14] - 2026-09-11

### Added

- Controller nicknames: pencil edit icon after each name in the controllers popup; names persist in `controllers.json` independently of Remember and appear in toast headings.

### Changed

- Configure window and controller popup are now built with **iced** (`iced::daemon` + multi-window) instead of the hand-rolled softbuffer/Taffy UI.
- Overlay toasts are iced always-on-top windows sharing the same event loop (dark card styling preserved).
- Minimum Rust version is **1.88** (required by iced 0.14).

## [0.1.13] - 2026-09-11

### Added

- Bluetooth **Turn off** action in the controller popup (HID feature report `0x08`).
- SVG icons under `assets/icons/` for DualSense, Settings, Identify, Power, Edit, Close, Minimize, and Check; rasterized with `resvg`.

### Changed

- Controller popup rows are status cards (battery rail + DualSense glyph) instead of clickable hover cards; Identify and Turn off are icon buttons.
- Tray / `.exe` / header DualSense mark comes from SVG instead of hand-drawn pixel art.
- Configure title bar uses SVG minimize/close icons; Remember checkboxes use the check SVG.
- Bluetooth power-off tries every DualSense HID interface and multiple feature-report sizes/CRC seeds (Windows `HidD_SetFeature` is picky about collection and length).

## [0.1.12] - 2026-09-10

### Added

- Toast position options for **top center** and **bottom center** (default is now bottom center).

### Changed

- Overlay toasts always appear on the primary monitor, sized to their content with uniform padding, and use sentence-cased status text.

## [0.1.11] - 2026-09-10

### Added

- Steam-style, always-on-top overlay toasts replace OS notifications for controller connect, disconnect, low-battery, and charged events.
- Configure’s Toast position controls select any screen corner and immediately preview the placement.
- Left-clicking the tray icon opens an anchored controller popup with live and remembered pads, battery state, click-to-identify rows, per-controller Remember controls, and a Settings shortcut (Windows and macOS).

### Changed

- Toasts slide vertically in and out from their configured screen edge, dismiss on click, Escape, or automatically after five seconds, and queue when multiple controller events occur together.
- Configure now uses the same dark visual language as overlay toasts, with custom window chrome and notification, position, autostart, lightbar, and developer controls directly in the window.
- Configure’s custom title bar is more compact.
- The native right-click tray menu now contains only **Settings** and **Exit**; controller status and actions moved to the reusable dark popup.
- Configure, controller popup, and toast spacing now comes from shared Taffy Flexbox layouts, design tokens, and measured text bounds instead of independent absolute coordinates; long labels are ellipsized and toast margins scale with monitor DPI.
- Configure is now a compact single-column window with concise section headings, a 16:9 toast-position stage with toast-shaped corner selectors, and an animated single-open accordion layout with a fixed height sized to its tallest panel. Configure and the controller popup now share the same compact header height and title treatment.
- Lightbar colors now use an interactive 2–5 stop gradient editor that is always fully shown when the Lightbar accordion panel is active: select and drag stops along the spectrum, click empty bar areas to add stops, drag a stop away to remove it, and apply edits immediately (drag commits on mouse up). Legacy three-field spectrum prefs migrate automatically.

## [0.1.10] - 2026-08-30

### Added

- Opt-in **Remember** toggle per controller in the tray menu. Remembered pads stay listed after disconnect with their last-known battery % (`controllers.json` beside `prefs.json`). Each controller is a submenu with **Identify** and **Remember**.

### Changed

- Notification toggles in Configure → Settings now sit under a **Notifications** submenu (**On connect**, **When low**, **When charged**). **Start with Windows** remains a top-level Settings item.

## [0.1.9] - 2026-08-25

### Fixed

- Windows autostart no longer flashes a Command Prompt window at login. Startup now uses a `.lnk` shortcut instead of a `.cmd` script; an existing `.cmd` entry is migrated automatically on launch.

## [0.1.8] - 2026-08-25

### Added

- Desktop notification when a DualSense connects, including current battery percent. Toggle via Configure → Settings → **Notify on connect** (on by default; stored in `prefs.json`).

## [0.1.7] - 2026-08-25

### Fixed

- Lightbar color and identify work again while Steam is running: skip the Bluetooth `LIGHT_OUT` claim when `steam.exe` is present (Steam Input already initialized the bar). The 0.1.6 claim sequence is still used when Steam is not running.

## [0.1.6] - 2026-08-05

### Fixed

- Lightbar color and identify now work on Bluetooth without Steam: claim control with a dedicated `LIGHT_OUT` setup report, then set RGB in a **second** report (Linux `hid-playstation` sequence). Combining setup+RGB in one packet left the bar dark or stuck on the default blue.

### Added

- `--set-lightbar R G B` CLI flag to push a color to all connected pads (useful for debugging).

## [0.1.5] - 2026-08-03

### Added

- Tray **Configure** opens a settings window with a native menu bar (**Settings** for notifications/autostart, **Developer** presets when `--dev`) and a three-stop lightbar spectrum editor (full / mid / empty). Spectrum saved in `prefs.json`.

### Changed

- Default lightbar spectrum stops are now `#4141FB` → `#6605BE` → `#BE0000` (full / mid / empty).
- Notification and autostart toggles live in Configure → **Settings** instead of the tray menu.

## [0.1.4] - 2026-08-02

### Added

- Desktop notifications when a controller enters low battery (≤5% discharging) or finishes charging, with tray toggles persisted in `prefs.json`.
- Optional `dev-emulate` Cargo feature and `--dev` flag for emulated controller presets (not included in release builds).

## [0.1.3] - 2026-08-02

### Fixed

- A DualSense connected over both USB and Bluetooth no longer appears twice in the tray. Identity is resolved via the controller MAC (pairing-info feature report) when Windows leaves the USB HID serial empty, and the USB path is preferred.

### Added

- `--list-controllers` CLI flag to print connected pads (connection, identity, battery) and exit.

## [0.1.2] - 2026-07-30

### Fixed

- Tray no longer freezes or thrash-refreshes when a DualSense is visible to HID but cannot be read (powered off, sleeping, or exclusively held).
- Disconnect detection is much faster: clear immediately when HID drops the pad, and re-probe connected pads every few seconds so a Bluetooth power-off is reflected promptly.
- Battery HID reads fail faster so a dead pad cannot block polling for tens of seconds.

## [0.1.1] - 2026-07-30

### Fixed

- Lightbar color and identify now work when Steam is not running, by sending DualSense lightbar setup flags (`valid_flag2` / `LIGHT_OUT`) with every RGB output report.

## [0.1.0] - 2026-07-30

### Added

- System tray app for DualSense / DualSense Edge battery status (USB and Bluetooth).
- Battery-driven lightbar colors (blue → purple → red) and identify flash from the tray menu.
- Low-battery orange pulse while discharging at ≤5%.
- Connect/disconnect detection via periodic presence scan.
- Windows autostart (tray toggle and CLI flags).
- File logging, single-instance guard, `--help` / `--version`.
- Embedded DualSense silhouette for the tray and `.exe` icon.
- Windows CI and tagged release workflow.

[1.4.1]: https://github.com/LankyMoose/sdsc-utils/compare/v1.4.0...v1.4.1
[1.4.0]: https://github.com/LankyMoose/sdsc-utils/compare/v1.3.3...v1.4.0
[1.3.3]: https://github.com/LankyMoose/sdsc-utils/compare/v1.3.2...v1.3.3
[1.3.2]: https://github.com/LankyMoose/sdsc-utils/compare/v1.3.1...v1.3.2
[1.3.1]: https://github.com/LankyMoose/sdsc-utils/compare/v1.3.0...v1.3.1
[1.3.0]: https://github.com/LankyMoose/sdsc-utils/compare/v1.2.1...v1.3.0
[1.2.1]: https://github.com/LankyMoose/sdsc-utils/compare/v1.2.0...v1.2.1
[1.2.0]: https://github.com/LankyMoose/sdsc-utils/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/LankyMoose/sdsc-utils/compare/v1.0.1...v1.1.0
[1.0.1]: https://github.com/LankyMoose/sdsc-utils/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/LankyMoose/sdsc-utils/compare/v0.1.15...v1.0.0
[0.1.15]: https://github.com/LankyMoose/sdsc-utils/compare/v0.1.14...v0.1.15
[0.1.14]: https://github.com/LankyMoose/sdsc-utils/compare/v0.1.13...v0.1.14
[0.1.13]: https://github.com/LankyMoose/sdsc-utils/compare/v0.1.12...v0.1.13
[0.1.12]: https://github.com/LankyMoose/sdsc-utils/compare/v0.1.11...v0.1.12
[0.1.11]: https://github.com/LankyMoose/sdsc-utils/compare/v0.1.10...v0.1.11
[0.1.10]: https://github.com/LankyMoose/sdsc-utils/compare/v0.1.9...v0.1.10
[0.1.9]: https://github.com/LankyMoose/sdsc-utils/compare/v0.1.8...v0.1.9
[0.1.8]: https://github.com/LankyMoose/sdsc-utils/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/LankyMoose/sdsc-utils/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/LankyMoose/sdsc-utils/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/LankyMoose/sdsc-utils/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/LankyMoose/sdsc-utils/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/LankyMoose/sdsc-utils/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/LankyMoose/sdsc-utils/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/LankyMoose/sdsc-utils/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/LankyMoose/sdsc-utils/releases/tag/v0.1.0
