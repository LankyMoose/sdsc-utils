# Shell stale remembered `%` after full-charge + unplug (2026-10-14)

**Status:** fixed in 1.7.0 (service-owned session + snapshot sync). **When to read:** history of the popup `%` vs toast / `controllers.json` disagreement; service↔shell remembered-state sync. Index: [README.md](README.md).

## Incident

- Released build: `app.log` opens with `SDSC Utils (sdsc-utils) 1.6.1 starting`.
- User charged a pad to full, saw the `Finished charging` toast, unplugged USB, and later the popup `%` had not updated.
- Unplug marker in `app.log`: `[1791957137] WARN: could not read MAC for DualSense over USB ... (0x000001B1) A device which does not exist was specified` — logged only from `resolve_device_identity` (`src/controller/dualsense/identity.rs:92`), i.e. the USB node vanished mid-probe. Poll lines change at the same boundary (`open_ms=3` → `open_ms=0`).
- Service side was correct: `controllers.json` has the serial as `USB 100%`; `analytics.json` shows `charge_steps 95→100` committed, `in_progress: null`, `last_seen_ms` in the same second as the MAC warning.
- Release `app.log` cannot confirm the toast or the stale `%` directly: `diag_info` (`src/controller/hid/diag.rs:18`) and notify diag (`src/persist/notify.rs:171`) are `debug_assertions`-only no-ops, and the Charged toast hardcodes `percent: Some(100)` (`src/persist/notify.rs:209`).
- Re-running 1.6.1 (fresh process) shows the correct `100%` — consistent with a stale-shell / correct-disk split, since the shell reloads `controllers.json` at boot.

## Root cause

The service owns remembered state; the shell's copy freezes at boot:

- Service: `sync_from_live` on every apply (`src/session/mod.rs:176`), `SaveKnown` (`src/service/mod.rs:601`), reload-from-disk every 2s (`src/service/mod.rs:747`).
- Shell: loads `KnownControllers` once at boot (`src/app/mod.rs:526`); `apply_service_controllers` (`src/app/mod.rs:5258`) replaces only `session.controllers`, never `session.known` and never reloads disk. Popup remembered-disconnected rows render from that stale `known` (`src/app/mod.rs:1995`).

So `95%→100% Complete` updates disk to `100%` but the running shell keeps the boot-time `%`; after unplug the remembered row shows the old value. Restart fixes it by re-reading disk.

## Why 1.7 does not fix it

`v1.6.1..HEAD` adds `SyncPopup` row-count/resize (`src/session/mod.rs`), Start-selection IPC (`src/ipc/mod.rs`, `src/service/mod.rs`), and debug-emulator work. None of it pushes `known` deltas to the shell or makes the shell reload `controllers.json`; `apply_service_controllers` still only syncs live rows. Verified via `git diff v1.6.1..HEAD -- src/app/mod.rs src/service/mod.rs src/ipc/mod.rs src/session/mod.rs`.

## Fix (landed in 1.7.0)

Chose the third option, generalized: the service publishes one rev-gated retained snapshot (`SessionSnapshot`: live + remembered + nicknames + prefs + analytics + stored selection) on change, newcomer attach, and `RequestSnapshot`, and the shell renders popup / Start dock / disconnected ETAs purely from it. See [session-snapshot.md](session-snapshot.md) for the protocol. The old `Controllers` push, `ControllerList` boot pull, and `StartSelection` message are gone (the pull pair stays for the CLI); shell mutations travel as absolute intents; the service owns all data files with atomic writes; IPC is versioned with quit-on-mismatch.

Regression test: `session::tests::full_charge_then_unplug_snapshot_carries_100` — service applies `95% Charging → 100% Complete → disconnected` and asserts the snapshot carries `100`, not the boot-time `95`.
