# Browser app split — Phase D (Option A)

**Decision:** the Browser becomes a separate optional process/executable
(`sdsc-browser`), not a DLL/module. Main app remains the OS/shell.
**Status:** D1 protocol crate (`6e1a201`), D2 add-on process + host supervisor
(`6e8fbd3`..`cb5b2d4`), D2e remote replay (`fc1a3e3`), and D2f in-process
removal are landed. `sdsc-browser` owns its window/CEF/cursor/text session;
the main app owns Home/PS, HID, system OSK, lifecycle, and the engine package.
The add-on is resolved next to the main exe, then
`<data>/browser-addon/` (install via `scripts/install_browser_addon.py`).
There is no in-process Browser fallback; `SDSC_BROWSER_REMOTE=0` disables the
Browser for automation. MSIX/Store packaging remains D3.

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
   and data migration remain.

## What was removed

The main app no longer creates a Browser window, spawns CEF, owns a
`BrowserPanel`, subscribes to engine events, or routes page keyboard/mouse/IME.
Those paths now live only in `src/browser/server.rs` and the `sdsc-browser`
binary. Shell keyboard routing is `Message::ShellKey`; the Browser window
receives its own physical input. Diagnostics screenshots are replay-driven in
the add-on (`HostMessage::Replay` action `screenshot`).
