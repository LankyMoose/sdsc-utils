# Browser app split — Phase D (Option A)

**Decision:** the Browser becomes a separate optional process/executable
(`sdsc-browser`), not a DLL/module. Main app remains the OS/shell.
**Status:** D1 protocol crate (`6e1a201`), D2 add-on process + host supervisor
(`6e8fbd3`..`cb5b2d4`), D2e remote replay (`fc1a3e3`), D2f in-process
removal, and the Home-managed local lifecycle (`47b1c2f`..`159cea8`) are
landed. `sdsc-browser` owns its window/CEF/cursor/text session; the main app
owns Home/PS, HID, system OSK, lifecycle, and the engine package. Resolution
order is package-owned sidecar → `<data>/browser-addon/` → development sidecar.
There is no in-process Browser fallback; `SDSC_BROWSER_REMOTE=0` disables the
Browser for automation. MSIX/Store submission remains D3.

## Transport

Use the length-prefixed protocol in `crates/browser-protocol/` over the
child process's **stdin/stdout**:

- host writes `HostMessage` frames to child stdin;
- child writes `BrowserEvent` frames to stdout;
- logs/diagnostics stay on stderr;
- no frame/pixel IPC — the Browser renders its own window with CEF.

Pad/control traffic is small, so this avoids Windows named-pipe/shared-memory
plumbing while keeping one versioned framing/codec. A future pipe transport can
implement the same message directions.

## Ownership

- **Main app:** HID, pad action mapping, Home/PS, system OSK, toasts, app
  lifecycle, launch/close supervision.
- **Browser process:** its own window, CEF engine, page DOM/focusables/cursor,
  magnet/docking/D-pad jumps, click/scroll/hover, page IME/keyboard, text
  session client, action handlers (`back`/`forward`/`reload`).
- **No Browser chrome** in either process.

## Input / text / overlays

- Main sends pad snapshots + source-side actions; Browser applies surface input
  and actions. Browser never sees button→action bindings.
- Browser reports `BrowserEvent::RequestTextInput` when Cross lands on a page
  text field; main opens the system OSK. Focus/autofocus never opens it.
- OSK text actions go back to Browser as `TextAction`.
- Browser reports real keyboard/IME input so main can close the OSK. While the
  OSK is open, Browser must not process the dismissing key locally; main
  returns the hand-off key (Escape consumed). Mouse close need not reach the
  Browser.
- Home/PS opens above the Browser and pauses/suspends its input; closing Home
  resumes it. Browser close remains OS-owned.

## D2 implementation checklist

1. `sdsc-browser` binary + Browser-only iced app (no tray/HID/Home/Launcher).
2. Host supervisor: spawn/connect, hello handshake, stdin/stdout reader/writer
   threads, exit/crash handling, pause/resume/focus messages.
3. Main integration: Home Browser tile launches the process; track visibility;
   pause/resume it around Home.
4. Stream HID edges/actions; route OSK text/hand-off; apply action state.
5. Replay is forwarded to the add-on (`HostMessage::Replay`) and answered with
   `BrowserEvent::ReplayState`; all browser QA scripts run against the child.
6. Packaging (D3): local add-on install landed; MSIX/Store related-set policy
   remains.

## Data directory

`%APPDATA%\sdsc-utils` (or the platform equivalent from
`persist::paths::data_dir`) stays canonical. The host sends that path in
`Hello.data_dir`; `sdsc-browser` pins it before the window, CEF profile, tabs,
or bookmarks are touched, so a packaged add-on with a different package
identity still shares the shell's data. The supervisor also exports
`SDSC_DATA_DIR` to the child because CEF re-executes the binary for
renderer/GPU subprocesses before any `Hello` frame exists.

`addon.json` carries `protocol`; Home treats an install built for a different
host protocol as update-required, and the launch preflight refuses to spawn it
instead of taking a protocol close. There is no data migration step because the
Browser add-on and engine are unreleased: the shared path is the contract from
the first shipped build.

Runtime precedence is package-owned sidecar → Home-installed copy →
development sidecar (`SDSC_BROWSER_ADDON_PACKAGE_DIR` mocks the first for QA).
A package-owned sidecar always wins; a leftover local copy is simply ignored.

## App lifecycle

`src/app/addons.rs` owns the install/update/uninstall orchestration. Install
copies the add-on (sidecar or `SDSC_BROWSER_ADDON_SOURCE`) into
`<data>/browser-addon/`, writes `addon.json`, and chains the CEF engine package
when it is missing or updatable. Update compares the installed protocol plus
metadata (version, size, source mtime) against the source. Home's Browser menu
acts on this app package, not just the engine.

Traces:

- `actions.jsonl`: `home.app.install`, `home.app.update`,
  `home.app.uninstall`, `home.app.launch` with `pre`/`post`, `repro`, and checks.
- `home.jsonl`: `browser.addon.install.phase`, `browser.addon.install.done`,
  `browser.addon.ready`, `browser.engine.install.*`, uninstall phases.
- `manifest.jsonl`: `browser_addon` state snapshots and `browser_addon_launch`.

Replay steps for the app flow are `home.open`, `home.app.install`,
`home.app.update`, `home.app.uninstall`, `home.app.launch`, and
`home.app.close`; source/version can be supplied in step params.

## What was removed

The main app no longer creates a Browser window, spawns CEF, owns a
`BrowserPanel`, subscribes to engine events, or routes page keyboard/mouse/IME.
Those paths now live only in `src/browser/server.rs` and the `sdsc-browser`
binary. Shell keyboard routing is `Message::ShellKey`; the Browser window
receives its own physical input. Diagnostics screenshots are replay-driven in
the add-on (`HostMessage::Replay` action `screenshot`).
