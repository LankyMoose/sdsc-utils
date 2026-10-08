## 1.7.0
### Added
- Horizontal immersive layout mode for the Start screen
- Changelog viewer (this screen)
- Extra Steam library folder support
### Changed
- Toast position setting moved to the Notifications tab
### Fixed
- Position bar now briefly shows when changing sort in immersive mode

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
