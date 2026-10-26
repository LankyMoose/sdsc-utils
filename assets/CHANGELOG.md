## 1.7.1
### Added
- Uninstalled Steam games stay listed with their real titles (Show uninstalled games toggle, default on)
- View in Steam action for Steam rows
- Discard-changes guard when leaving edit mode with unsaved changes
### Changed
- Steam library auto-resyncs on install changes and whenever Start opens
- Controllers panel and settings drawer open over dirty edits without discarding
### Fixed
- Start/Options and Create no longer bypass the discard-confirm modal

## 1.7.0
### Added
- Horizontal immersive layout mode for the Start screen (Vertical/Horizontal selector)
- Changelog viewer (this screen)
- Extra Steam library folder support
- Performance settings: idle throttle, antialiasing/vsync toggles, reload action
- Position-rail visibility selector (When navigating / Always on)
- Reduced motion selector (System / On / Off)
- Unified glass theme across popup, Settings, toasts, and Start (Vulkan preferred, DX12 fallback)
### Changed
- Start screen is always the fullscreen console stage (compact window removed; PS opens, Circle/Escape closes, Create toggles controllers)
- Single process: tray, HID, session, and UI in one sdsc-utils.exe
- Toast position setting moved to the Notifications tab
- Relaunching the app now replaces the running instance instead of exiting quietly
- Windows Task Manager description now reads `SDSC Utils {version}`
- Controllers surface returns to a rows panel; footer hints reshuffled
- Battery analytics charts gain captions, legends, and elapsed recording time
- Steam rows list Last played before Played
### Fixed
- Position bar now shows on open, sort change, and reorder in immersive mode
- Selected game titles draw in-slot and wrap with shared breaks instead of crossfading
- Update badge no longer clips at the strip edge; outer peeks show titles
- Stuck Running badges on failed Steam launches fast-fail instead of parking
- Stuck Powering off rows clear eagerly and only show while connected
- Controllers popup refits while open; Auto open hides unless Start is enabled
- Toasts expire on wall-clock time; Start close restores the cursor

## 1.6.1
### Added
- Powering-off status for controllers, mirroring game Closing
- Center-top clock widget in immersive Start (Display clock toggle, on by default)
- Separate Start screen settings: Enable master toggle plus Auto open selector
### Fixed
- Newly-detected presence that auto-opens immersive Start no longer shows a Connected toast
- Brief input-report stalls no longer flap the session with Disconnected/Connected toast loops
- Stable pads cost zero HID reads on liveness ticks; lightbar reasserts only on change plus a 30s backstop
- Hot enumeration freeze bounded: OS arrival watcher surfaces second-pad connects in ~1s
- Power-off no longer falls through to unknown-identity interfaces when exact matches exist
- Ghost HID paths back off after consecutive open failures

## 1.6.0
### Added
- Fullscreen immersive Start with cover-flow games strip, controllers dock, and splash
- Second PS press promotes to immersive; Circle demotes (always-immersive closes instead)
- In-window Start settings via the DualSense Options button
- Immersive idle and sleep dimming, with Settings controls
- Steam playtime, install size, and last played on game rows
- Display-only games-list position indicator with last-played sections
### Changed
- Connected toasts are skipped for controllers already connected at launch
- Start reopen gesture is fixed to PS; custom chord recording is removed
- Immersive games strip does not wrap; pad navigation stops at the ends
### Fixed
- A bad game, controller, or analytics record no longer discards the rest of that file
- Pad input no longer stalls while Windows hidapi_refresh blocks with Start hot

## 1.5.2
### Fixed
- Idle start-screen reopen gesture works again in the service/shell split and Store/MSIX builds

## 1.5.1
### Changed
- Portable builds ship as one sdsc-utils.exe; first launch unpacks the UI shell into the app data folder

## 1.5.0
### Added
- Start screen controller haptics: nav, action, hold, and L2/R2 slide cues pulse DualSense motors
### Changed
- DualSense HID and the tray stay in the sdsc-utils service; the iced UI runs as a separate shell over local IPC
- Battery percent and lightbar color come from live input reports while pad input is active
### Fixed
- Low-battery toast fires only when a watched pad crosses down through the threshold
- Hold-to-power-off waits until an in-flight rumble pulse finishes

## 1.4.3
### Fixed
- Bluetooth lightbar on Windows works again while Steam is running: RGB via interrupt write with control fallback
