# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Unified "immersive" visual language across the app: floating glass cards, rounded corners, and the same ambient shader backdrop in the controllers popup, Settings, toasts, and compact Start.
- Presentation now prefers Vulkan with DX12 as fallback, so transparent rounded windows composite correctly; on DX12-only systems the settings window falls back to a rounded, opaque DWM frame.
- Toasts are rounded glass cards with a soft shadow and a rounded accent rail.
- Settings window is larger (720×520) with a floating sidebar, grouped cards, and consistent toggles and sliders.
- `SDSC_DEBUG_OPEN` (debug builds): open specific windows at boot; quiet mode moves them to the secondary monitor without focus and mutes sounds/haptics.

### Changed

- Lightbar spectrum editor bars and handles are rounded; the saturation square has rounded corners.
- Remember is now a pin toggle icon in the popup row.
- The developer emulator and other debug-only tooling ship in every non-release build (`cfg(debug_assertions)`) instead of behind the `dev-emulate` Cargo feature or a `--dev` flag — a plain `cargo run` now has the Configure → Controllers section. Release builds still exclude it.
- Configure's debug tools move under a **Developer mode** caption so they are clearly separated from the shipped settings.
- **Developer mode:** the Developer tab is now **Controllers**, with a compact row per emulated controller you can add, remove, and drive: a connection switch, and a chevron that expands that pad's controls inline — a battery slider that snaps to the levels a DualSense actually reports, a link picker (USB charges, Bluetooth runs down), and battery-analytics actions (seed estimates, walk the charge/drain cycle). Commands address pads by serial, so an action can never land on the wrong pad.
- **Developer mode:** a pad's **charge state is derived from its link**, the way the hardware reports it — USB charges (and reports full once it tops off), Bluetooth discharges. There is no separate charge-state picker, so the emulator can no longer describe states real hardware cannot be in; each link chip shows the state it implies. New emulated controllers are created remembered but **disconnected**, so adding one does not fire a connect toast or open Start. A per-pad switch plugs them in and out through the normal connect/disconnect path, so toasts and the Start-open preference behave as they would for real hardware. Battery changes on a **connected** pad are previewed and published after a one-second debounce, with a spinner while they wait, so a drag cannot spam preference-driven notifications; a disconnected pad is not published at all, so its slider applies immediately. Unplugging a pad mid-debounce cancels the pending change.
- Horizontal immersive Start centers every cover by its art (neighbors no longer ride above the selection) and fades neighbors to transparent instead of grey.

### Fixed

- Horizontal immersive Start draws the selected game's title and subtitle inside its cover, like its neighbors, so they scroll with the card instead of crossfading in a separate full-width layer.

## [1.6.1] - 2026-10-07

### Added

- Powering-off status for controllers, mirroring game Closing: holding Triangle on a Start controller row (or the popup power button) immediately swaps the row to a muted-white `Powering off` status with a static `Powering off…` hint until the pad actually disappears; repeat commands while pending are ignored. Powering-off rows render dimmed with no Identify/Power off actions (the worker drops the pad any moment).
- Center-top clock widget in immersive Start showing the system locale time (12h/24h per Windows settings; 12h am/pm fallback elsewhere), with a Display clock toggle in Start settings and Configure (on by default).
- Separate Start screen settings: `Enable start screen` (master) and a single `Auto open` selector (`Never` / `When a Bluetooth controller connects` / `When any controller connects`, default `When any controller connects`): expandable dropdown in Start settings (Cross opens, dpad picks, Left/Right quick-cycle) and native dropdown in Configure.

### Fixed

- `Closing` (game) and `Powering off` (controller) status text is muted white instead of orange; low-battery warnings stay orange.
- Newly-detected presence that auto-opens immersive Start no longer shows a `Connected` toast (cold-launch and hot-connect now match); compact auto-open shows the toast first and opens Start on slide settle, so the card can never be swallowed by the window create.
- Closing a game flips status from `Playing`/`Running` to `Closing` immediately (async close; badge clears on process exit).
- Game rows disable (dim + `Not installed`, skipped in nav, launch blocked) when a scan proves them uninstalled; stale immediate launches fail gracefully with a rows refresh instead of an error popup.
- Brief input-report stalls (e.g. a weak low-battery Bluetooth link under two-pad load) no longer flap the session: missed polls hold the pad for 2s of wall time instead of a single miss, so short stalls can't trigger `Disconnected`/`Connected` toast loops, lightbar reclaim flashes, or input-handle drops.
- Incremental HID poll: stable pads with fresh sample data cost zero HID reads on liveness ticks (cached handles are reused, rumble/input handles and the live map are no longer dropped every 5s); blocking battery reads are reserved for new serials (full attempts) and stale pads (fail-fast), and the lightbar reasserts only on new pads, color changes, and a 30s backstop instead of every tick.
- Hot enumeration freeze bounded: `refresh_devices()` is forced at least every 20s even while input is hot, and a new OS arrival watcher (Windows `CM_Register_Notification` on the HID class, Linux `/dev` snapshot thread) fires list-only refresh hints so second-pad connects surface in ~1s instead of staying invisible until input goes cold.
- Stale miss-streak no longer drops instantly: the hot path clears streaks for seen pads on every loop (`mark_seen`), not just on change, so a recovered pad's next stall holds the full 2s instead of firing an immediate false `Disconnected`.
- Launch-quiet suppression only covers continuously-present pads: a serial absent from a snapshot loses its suppression, so a pad returning after a power cycle gets its `Connected` toast instead of inheriting a stale launch entry.
- Power-off no longer falls through to unknown-identity interfaces when exact matches exist: if every matched send fails it reports failure rather than risk powering off a stranger's pad.
- Ghost HID paths (enumerated but un-openable) back off after 3 consecutive open failures instead of burning an open per sample; records clear on success or when the path leaves enumeration.
- Arrival watcher debounce gains a trailing follow-up so a second pad connecting inside the 1s burst window is re-scanned ~1.5s later instead of waiting out the 20s heartbeat.

## [1.6.0] - 2026-10-05

### Added

- Immersive Start paints a soft rectangular edge vignette over atmosphere/splash only (games strip, dock, and footer stay undimmed).
- Fullscreen immersive Start, including an always-immersive setting (default on). A second **PS** press promotes the compact launcher; **Circle** demotes (always-immersive closes instead).
- Immersive cover-flow games strip, controllers dock, splash, and cold-load chrome.
- Immersive idle and sleep dimming, with Settings controls. The cover darkens after inactivity and wakes on input.
- In-window Start settings via the DualSense **Options** button (compact modal; immersive drawer) for Start screen prefs except Enabled.
- USB controllers setting for start-screen auto open/close. When off, only Bluetooth pads open or hold the launcher.
- Steam playtime, install size, and last played on game rows. Sort prefers Steam last played, and **Cross** reads **Update & launch** when an update is required.
- Steam library format fallbacks, plus a last-known compatible version footnote.
- Settings · System footer with GitHub and Microsoft Store links and the app version.
- Display-only immersive games-list position indicator. Sections follow last played (Today, 7 days, 1 month, 1 year, Older, Never) or A–Z, slide in from the left, and hide after 2s without scrolling.
- Empty Games list shows a ringed triangle hint on the browse prompt.

### Fixed

- A bad game, controller, or analytics record no longer discards the rest of that file. When nothing in the file can be loaded, the original is copied to `*.json.bak` before any later save replaces it.
- Pad input no longer stalls for ~5s when Windows `hidapi_refresh` blocks while Start is hot; open pads keep sampling and F8 hitch marks in shell-client mode report IPC pad context instead of misleading `NeverPublished`.

### Changed

- Connected toasts are skipped for controllers already connected when the app starts.
- Configure System footer: GitHub icon fill matches the Microsoft Store bag (`#F2F2F2`); both footer link icons use 0.8 opacity until hovered.
- Start reopen / promote gesture is fixed to **PS** (Guide); custom chord recording is removed from Settings.
- Immersive games strip does not wrap; pad navigation stops at the ends.
- Start footer hints use a split layout (secondary controls on the left, primary actions on the right). Sort and cycle hints are pad-only.
- Connected toast on 0→1 auto-open shows inside immersive Start, not as a separate desktop card.
- Toasts and file pickers stay above immersive Start.
- Compact reopen-chord hint is hidden in temporary immersive Start.
- Powering off one DualSense keeps sibling controllers live.
- A disconnected pad drops off the hot Start list within ~200ms.
- Opening Start does not promote to immersive on the same **PS** press.
- Empty Games list copy uses a hyphen in "No games yet - press" (the previous em dash is gone).

## [1.5.2] - 2026-10-03

### Fixed

- Idle start-screen reopen gesture (default PS) works again in the service/shell split and Store/MSIX builds. The service matches the chord while the UI shell is connected and tells the shell to open Start, instead of only latching the gesture across a shell restart.

## [1.5.1] - 2026-10-03

### Changed

- Portable GitHub builds ship as one `sdsc-utils.exe`. The first launch unpacks the UI shell into the app data folder (`shell\`). A `sdsc-shell` sitting next to the service (dev builds and the Store package) is still used directly.

## [1.5.0] - 2026-10-03

### Added

- Start screen **Controller haptics** (Settings → Start screen, on by default, strength slider default 60%): nav, action, hold, and L2/R2 slide cues pulse DualSense motors, including over Windows Bluetooth. Slide changes also play a slide clip when UI sounds are on.

### Changed

- DualSense HID and the tray stay in the `sdsc-utils` service; the iced UI runs as a separate shell over local IPC, so a shell crash can restart without reopening Bluetooth.
- While pad input is live, battery percent and lightbar color come from the reports the controller is already sending. Navigation stays responsive, another pad can join the list, and connect colors apply without a UI freeze. The slower exclusive poll remains when the app is idle.

### Fixed

- Low-battery toast fires only when a watched pad's percent crosses down through the threshold. A controller that powers on already inside that window shows Connected only.
- In-ring remaining-time labels like `~10h 30m` shrink to fit the battery ring; shorter labels stay the same size.
- 0→1 Start still opens if the Connected toast flaps: one missed poll keeps the last status, a presence blip under 2s does not arm the reconnect cooldown, and Start retries if the window was still closing.
- Close game hold decays on early release the same way Power off does. A short Cross on the running-game row no longer cancels the shared hold charge.
- Triangle hold is ignored on Controllers rows that do not show **Power off** (wired pads and remembered disconnected pads).
- Hold-to-power-off waits until an in-flight start-screen rumble pulse finishes, so the hold haptic is not cut off.

## [1.4.3] - 2026-09-30

### Fixed

- Bluetooth lightbar on Windows works again while Steam is running: skip `LIGHT_OUT` when Steam Input already owns the bar, send RGB via interrupt `write` (control only as a hard-error fallback), try every DualSense HID collection for the serial, and request the BT calibration feature before writing.

## [1.4.2] - 2026-09-28

### Changed

- Start Games list shows skeleton rows (empty icon well + muted bars) for Steam entries while the library scan is still pending, instead of temporary `Steam {appid}` + controller-SVG placeholders.

### Added

- Start Controllers slide: Square cycles **Connected** / **All** (prefs `show_all_controllers`, default Connected). **All** includes remembered disconnected pads (muted; no Identify / Power off), using the same face-cycle footer pattern as Games sort.

### Fixed

- Concurrent connect toast + cold Start open no longer pairs `window::open` with same-turn toast z-order sync; toast hide is deferred briefly while Start is visible, and Resting no longer raises every frame — reducing iced `create_renderer` / image-atlas panics under multi-window GPU stress.
- Debug builds log wgpu adapter/backend at boot and include toast/Start lifecycle ages in panic context (`wgpu-diag`).
- Bluetooth lightbar on Windows no longer silently no-ops: RGB goes out via control `HidD_SetOutputReport` (not interrupt `WriteFile` padded to 547 bytes), and poll reopens a fresh handle after the battery read so a prior input handle cannot stick a false `LIGHT_OUT` claim.
- Connect toast slide-in no longer freezes, pops, or stays off-screen when the start screen opens on the same 0→1 connect (slide clock starts after show, frames are dt-capped, and Start waits only for slide settle).
- 0→1 Start open is latched through toast slide-in with a ~500ms deadline so Start cannot be dropped if settle stalls or fullscreen/cooldown flips after connect.
- Reopen gesture (default PS) no longer opens Start during a deferred 0→1 connect toast, and a held chord is consumed on Start open/close so focus-close cannot immediately recreate the window.
- Start-screen game art no longer uploads full-size Steam capsules (or fresh shell `from_rgba` handles on every row refresh) into iced’s image atlas — icons are downscaled once and reused, reducing atlas `grow` / `create_view` panics after GPU pressure.

## [1.4.1] - 2026-09-25

### Added

- After a tray-mode panic, the app schedules a budgeted delayed relaunch and shows a one-shot toast asking for `app.log` from the data directory (bug icon). Undocumented `--test-crash-toast` exercises that path without crashing.

### Fixed

- Start-screen shell icons no longer rebuild a new iced `image::Handle` every frame (stable RGBA handles), reducing wgpu image-atlas churn that could panic after GPU device loss.
- Panic hook records a capped `PANIC_BACKTRACE` in `app.log` for renderer crashes.

## [1.4.0] - 2026-09-23

### Added

- **Start screen** launcher: when the first DualSense connects (0→1), an always-on-top quick-launch card can open with curated Steam games and manual shortcuts. Navigate with D-pad / combined analog sticks / keyboard; Cross or Enter launches; Circle or Escape dismisses. Opens with an empty catalog (empty copy + Square hint). Any connected DualSense can drive open / close / navigation (inputs are never merged across pads).
- Reopen anytime with the **PS** (Guide) button. Catalog lives in `games.json`.
- Settings → **Start screen**: enable toggle and optional UI sounds (nav / action / hold-complete) with volume.
- **Triangle** toggles edit mode: Steam checklist (Cross adds/removes) plus removable manuals; **Add shortcut…** (edit only) for title, optional args, and optional custom image; **Square** edits the selected manual while editing. Edit is draft-based (**Triangle** Saves, **Circle** Cancels without closing).
- Browse mode DualSense **Square** (footer **Last played · A–Z**) toggles sort between last played and alphabetical (preference in prefs). Successful launches record `last_played_ms`. Manual shortcuts support optional launch arguments and a custom icon path.
- Games ↔ Controllers header (L2/R2 to switch); item actions on the selected row; footer for sort / Edit / Close. Steam icons use the modern `librarycache/{appid}/` layout (with legacy flat-name fallback); game art uses a shared 2:3 portrait cell.
- Discrete face actions activate on **release** (pressed hint stays lit while held); L2/R2 and list nav stay on press. Hold actions (close game / power off / replace Proceed) complete on **release** only when the hold was full and not cancelled. Hold arcs turn muted when armed; face hints ease slightly larger while pressed (~100ms, in-bounds). Pending face releases cancel on slide/nav context changes. UI sounds play only when the action actually runs. Nav and sounds arm only after the window is shown.

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

[1.6.1]: https://github.com/LankyMoose/sdsc-utils/compare/v1.6.0...v1.6.1
[1.6.0]: https://github.com/LankyMoose/sdsc-utils/compare/v1.5.2...v1.6.0
[1.5.2]: https://github.com/LankyMoose/sdsc-utils/compare/v1.5.1...v1.5.2
[1.5.1]: https://github.com/LankyMoose/sdsc-utils/compare/v1.5.0...v1.5.1
[1.5.0]: https://github.com/LankyMoose/sdsc-utils/compare/v1.4.3...v1.5.0
[1.4.3]: https://github.com/LankyMoose/sdsc-utils/compare/v1.4.2...v1.4.3
[1.4.2]: https://github.com/LankyMoose/sdsc-utils/compare/v1.4.1...v1.4.2
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
