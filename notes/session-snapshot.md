# Service-owned session snapshot protocol (1.7.0)

**Status:** implemented. **When to read:** touching service↔shell state sync, adding shared state, changing IPC, debugging stale shell UI. Index: [README.md](README.md).

## Rule

The service is the sole owner of `DeviceSession` truth (live pads, remembered pads, prefs, analytics records, start selection, debug emulator fleet). The shell holds view-state only and performs zero data-file I/O in client mode. All shell mutations are absolute idempotent intents; all service state flows to the shell as one versioned snapshot.

## Topics

- **Retained, service→shell:** `ServiceMessage::Snapshot(Box<SessionSnapshot>)` with monotonic `rev`. Pushed on change (loop-end rev check), on newcomer attach, and on `ShellCommand::RequestSnapshot`. The shell applies wholesale iff `rev` is newer, then re-renders popup / Start / analytics panel. Stored selection rides inside (keys meet rows in one message — no cross-topic ordering).
- **Transient, service→shell:** `Effects` / `Notifications` / `PadInput` unchanged, OpenStart latch retained. Never merged into the snapshot.
- **Intents, shell→service:** `SetPrefs` (service validates via `Prefs::normalized`), `SetRemembered`, `SetNickname`, `SetStartSelection`, existing `Identify`/`PowerOff`/`Rumble`/…, debug-only `Emu(EmulatorCommand)`. The service persists, `echo_intent()`s (rev forward so even no-ops converge the shell), and the loop-end push broadcasts.
- **Still separate:** `ListControllers`/`ControllerList` (CLI only), `GetVersion`/`Version` handshake (answered on the accepting socket, not broadcast), tray messages, `SetLightbarAll`.

## IPC discipline (no cross-version shims)

- `IPC_PROTOCOL_VERSION` (`src/ipc/mod.rs`): bump on any breaking change. First frame on every connection must be `GetVersion`; anything else is dropped unregistered. Shell handshakes on boot (seeding the command client) and every reconnect, quitting (exit 3) on mismatch or dead service. Service sweeps pre-spawn stragglers with `Shutdown` at startup (kills ≤1.6.1 shells), never respawns code-3 shells, backs off on rapid exits.
- v2 (game-selection persistence): `SetStartSelection` carries `game_index` (by-position restore; no controller index — the controller axis is out of scope) and prefs carry `start_selection_persist` (default on) + `start_selection_mode` (`by_game` default). Game-axis resolve honors both; disabled skips report + scan re-pin.
- Newcomer watermark advances only past reached transports (a dropped probe can't steal a latched `OpenStart`).
- User-data compat (`#[serde(default)]` prefs/known/analytics JSON) is fully supported — that is the compat that matters.

## Single writer

Service owns `prefs.json` / `controllers.json` / `analytics.json` (all atomic temp+rename writes). Shell client-mode `save`/`load` of those files is gone except boot-time initial reads (converged by the first snapshot). `games.json` / `steam_library.json` stay shell-owned (service never touches them).

## Debug emulator

Fleet lives in `session.emu_fleet` (service, rehydrated at boot, disconnected). HID poll sites merge `emulate::live` into every ingestion; `Emu` intents run the same transition/publish/persist path as hardware. The shell tab fleet is a pure derivation (`emulate::derive_view_fleet`) from the snapshot plus its local drag-preview overlay; debounce commits cross as `PreviewPercent` intents. Release builds compile all of this out.

## Adding state later

New shared state = new `#[serde(default)]` field on `SessionSnapshot` (+ rev bump at its mutation points). New action = new intent variant (+ service arm + shell sender). Never add a second sync loop.
