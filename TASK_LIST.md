# Task Organization - PS5 Battery Display (Detailed)

Based on codebase exploration, here is a detailed, well-ordered task list.

## 1. Immersive Layout Configurability ⬅️ HIGHEST PRIORITY
**Where to look:** `src/app/configure/`, `src/theme.rs`, `src/domain/color.rs`, `src/domain/pad.rs`

- [ ] **Add horizontal mode** for immersive layout
  - Add a setting to toggle between vertical and horizontal immersive layout
  - Horizontal mode should arrange games in a row rather than columns
  - May require reworking the game grid layout in the start screen

- [ ] **Anchor status chips to the top left** — *only when horizontal mode is enabled*
  - Status chips (battery %, connect/disconnect status) should be anchored to top-left corner
  - This styling applies specifically in horizontal immersive layout
  - Currently they may be centered or positioned differently in vertical mode
  - Look at `src/app/configure/` for status chip positioning

- [ ] **Reverse slide animations / content order** — *only when horizontal mode is enabled*
  - Reverse the slide direction/animation when navigating immersive mode in horizontal layout
  - Reverse content order (right-to-left vs left-to-right slide)
  - Likely in the navigation/transition code for immersive start screen horizontal mode

## 2. Center-Top Clock Widget
**Where to look:** `src/app/`, `src/theme.rs`, UI window setup

- [ ] **Add center-top 'clock' widget**
  - Display well-formatted system time (e.g., "3:45 PM", "14:45")
  - Position at center-top of the immersive start screen or main UI
  - Should use the system clock, possibly iced `time` widget or custom rendering
  - May need to add to the immersive start screen drawer/overlay

## 3. Presence/Toast Behavior — Bullet-Proof
**Where to look:** `src/service/`, `src/app/`, toast/connect logic

- [ ] **Verify current behavior:**
  - When 'presence' is newly detected and will open immersive mode → do NOT show 'connected' toast
  - Currently there's a difference between:
    - Cold-launch with controller already on (no connect toast)
    - Turning on a controller afterwards (shows connect toast)
  
- [ ] **Make bullet-proof:**
  - Unify the behavior so both scenarios are consistent
  - If presence detects a controller that will trigger immersive mode, skip the connect toast entirely
  - Check `src/service/mod.rs` and toast generation logic
  - The fix should handle both cold-start and hot-restart cases

## 4. Start Screen Enable — Two Independent Settings
**Where to look:** `src/persist/prefs.rs`, `src/app/configure/`, `src/bin/sdsc-utils.rs`

- [ ] **Separate 'connecting a controller opens the start screen' into two settings, both on by default:**
  - **Setting 1: Start Screen Enabled** (toggle) — whether the start screen feature is active at all
  - **Setting 2: Controller Connection Opens Start Screen** (toggle) — whether connecting a controller triggers the start screen
  - Both should default to `true`
  - Currently these are likely combined into one gate
  - This separation means start screen can be on without controller connection auto-opening it, and vice versa

- [ ] **Implementation approach:**
  - Add new pref key in `prefs.json` for `start_screen_enabled` (bool, default true)
  - Modify the connection logic to check both settings
  - Update the configure UI to have two separate toggles

## 5. Changelog Scrollable Modal in Configure UI
**Where to look:** `CHANGELOG.md` format, `src/app/configure/`, `src/persist/`

- [ ] **Add scrollable modal display in the main configure UI**
  - Should include all release notes, well separated
  - Follow the Keep a Changelog format from `CHANGELOG.md`
  - Add a "Changelog" or "Release Notes" section in the Configure window
  - Should be scrollable and possibly have sections collapsed/expanded
  - Modal should list all versions from newest to oldest
  - Each version section should have: Added, Changed, Fixed categories

- [ ] **Implementation:**
  - Add a new tab/panel in the Configure window
  - Read from `CHANGELOG.md` or embed release notes
  - Use iced scrollable container
  - Format text with proper heading hierarchy

## 6. Bring Immersive UI Style/Theming to Full UI
**Where to look:** `src/theme.rs`, `src/app/configure/`, `src/bin/sdsc-utils.rs`

- [ ] **Bring newer UI style/theming from immersive mode to the rest of the UI**
  - The immersive mode has a newer, sharper theme (from CHANGELOG 1.5.0: "Darker, sharper Settings / Start / chrome")
  - Apply consistent theming across:
    - Settings window
    - Controller popup
    - Tray menu
    - Overlay toasts
    - Main application window
  - Specific improvements from 1.5.0:
    - Theme hierarchy with framed outlines
    - List separators
    - Icon-only buttons with hover tooltips
    - Darker, sharper chrome

- [ ] **Implementation:**
  - Unify the theme constants in `src/theme.rs`
  - Apply shared design tokens across all UI components
  - Ensure consistency in colors, spacing, and chrome styling
  - May require refactoring of individual UI windows to share theme

## 7. Immersive Navigation — Section-by-Section for Massive Libraries
**Where to look:** `src/app/`, start screen navigation code, gesture handling

- [ ] **For users with massive libraries, immersive navigation should move section-by-section after holding for > 1sec**
  - Currently: navigation moves game-by-game on hold
  - New behavior: after holding D-pad/analog for > 1 second, navigation moves by "section" (Today, 7d, 1m, 1y, Older, Never, or A-Z groups)
  - May need to stop rendering non-first games per section for performance
  - This is for users with very large Steam libraries where game-by-game scrolling is slow

- [ ] **Implementation:**
  - Add hold timer ( > 1sec ) to navigation input
  - After timer fires, change navigation granularity from "game" to "section"
  - Possibly add a config option to disable/change this behavior
  - For performance: only render the first game in each section, or cache rendered sections
  - Look at `src/games/` and the start screen navigation code

## 8. Extra Sources for Steam Scan
**Where to look:** `src/games/steam.rs`, `src/persist/steam_library.rs`, prefs

- [ ] **Enable adding extra sources for Steam scan**
  - Currently the Steam scan likely uses default library paths
  - Allow users to add/customize library sources/paths
  - Add a setting in Configure → Developer (or new section) to add custom Steam library paths
  - Paths could be stored in `prefs.json` or `steam_library.rs`
  - Steam library scan should incorporate these extra paths

- [ ] **Implementation:**
  - Add extra library path configuration in prefs
  - Modify `src/games/steam.rs` scan logic to include custom paths
  - Possibly add a "Add Library..." button in the Configure window
  - Store and use extra paths on next scan

## 9. Game Close Status Text
**Where to look:** `src/games/launch.rs`, status tracking code

- [ ] **Change status text from 'playing' to 'closing' when closing a game**
  - When a game is in the process of closing, the status should show "closing" instead of "playing"
  - This is a status text update during the close hold action
  - Look at hold-to-power-off/close game logic
  - The status should transition from "playing" → "closing" → "closed" or back to tray

- [ ] **Implementation:**
  - Track game launch state in persist or domain models
  - Update the status display when close hold is initiated
  - Change the text shown in the start screen/game row, tray popup, or toast

## 10. Disable Uninstalled Games & Graceful Invalid Launch
**Where to look:** `src/games/steam.rs`, `src/games/launch.rs`, `src/persist/`

- [ ] **Disable game rows if scan determines they are no longer installed**
  - After a Steam library scan, check which games are actually installed
  - Gray out or disable rows for games that are no longer present
  - This prevents users from trying to launch games that don't exist

- [ ] **Handle immediate launch → invalid game launch gracefully**
  - If a user immediately tries to launch a game after scan, handle the case where the game is marked as not installed
  - Don't crash; show appropriate message ("Game no longer installed")
  - Possibly re-scan or offer to repair

- [ ] **Implementation:**
  - After Steam scan completes, compare game rows against installed check
  - Mark uninstalled games as disabled/grayed
  - In launch logic, check game validity before attempting to launch
  - Show friendly error if game is not installed

## 11. Battery/Connect Edge Case Replication
**Where to look:** `src/domain/`, `src/controller/`, battery/presence logic

- [ ] **First controller = low battery → connecting second controller causes 'blip' for the first → first periodically dc's for some time**
  - This is a battery/connectivity interaction issue
  - Scenario: 
    1. First controller has low battery
    2. Connect second controller
    3. First controller gets a 'blip' (status change)
    4. First controller periodically disconnects for some time
  - "If there is no obvious cause, replicate with local dev build and flag once hitch occurs"

- [ ] **Investigation steps:**
  - Reproduce with a local dev build with `--dev` flag
  - Monitor battery percentages and connection state
  - Check if the issue is related to HID polling interference between two controllers
  - Look at the battery read timing and lightbar claim interactions
  - Flag the specific hitch/pattern once observed

- [ ] **Possible causes:**
  - HID worker thread contention when two controllers are present
  - Battery read interleaving issues
  - Lightbar claim-once behavior with multiple pads
  - Presence scan timing conflicts

## 12. Close Game Status Transition (Duplicate/Refinement of #9)
- [ ] This overlaps with Task #9 — ensure status transitions properly:
  - playing → closing → closed
  - No crashing during transition
  - Status updates reflected in all UIs (tray, popup, start screen, toast)

---

## Already Addressed (from CHANGELOG)

These tasks are already implemented based on the changelog — no action needed:
- [x] Connected toasts skipped for controllers already connected at startup (1.6.0)
- [x] Low-battery toast fires only on threshold crossing (1.5.1)
- [x] Controller popup and Settings are ephemeral (1.2.1)
- [x] Darker, sharper Settings/Start/chrome (1.5.0)
- [x] Overlay toasts stay topmost over other apps (1.5.0)
- [x] Default lightbar spectrum `#0101FE` → `#8704FF` → `#FF0101` (1.3.2)
- [x] Battery analytics bucket-windows ignore first 30 min after connect (1.3.3)
- [x] Controller list percent ring flashes white in sync with Identify (1.3.2)
- [x] Various lightbar and HID fixes across versions
- [x] Start screen launch with empty catalog and Square hint (1.4.0)
- [x] Various toast and connect/disconnect behavior fixes