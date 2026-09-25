# UI lock-up / HID fight investigation (2026-09-23)

Hand-off notes. **Fix landed (reuse HidApi):** worker keeps one long-lived `HidApi`, refreshes via `refresh_devices()` on a throttle (not every sample), shares that api into Poll/Identify/lightbar/power-off, **does not clear** the input snapshot on Poll, publishes presence paths for the UI tick (no UI-thread `HidApi::new`), runs process enum via `Task::perform`, and Identify is **4 flashes / 1s** (`IDENTIFY_FLASH_COUNT=4`, `IDENTIFY_FLASH_MS=125`).

**Diagnostics are debug-build only** (`cfg(debug_assertions)`): `hid-trace.log`, hitch F7/F8 + report buttons, worker phase watchdog / `enter_op` traces, and investigation-volume `hid-diag:` / `ui-diag:` lines no-op or omit in `--release`. Future agents: see [`.cursor/rules/debug-diagnostics.mdc`](../.cursor/rules/debug-diagnostics.mdc) — new work must include a similar level of non-release diagnostics.

Historical evidence of the ~5s freezes is preserved below (`last_op=hidapi_new`). See [Investigate next](#investigate-next) for BT write size.

## Symptoms (pre-fix)

1. Occasional start-screen pad nav lock-up (keyboard/mouse may still work).
2. Occasional full freeze (pad + UI) lasting multiple seconds.
3. Occasional lightbar write that appears to do nothing.
4. Early hypothesis: Steam Input holding the DualSense interface — **not supported by the captured input hitches** (no `err=` / access denied; all opens/reads `ok`, `steam=1`).

## Log locations

| File | Role |
|------|------|
| `%APPDATA%\sdsc-utils\app.log` | Edge summaries, second timestamps, 1 MB rotate → `app.log.1` |
| `%APPDATA%\sdsc-utils\hid-trace.log` | Every HID open / write / read, **ms** timestamps, 16 MB rotate → `hid-trace.log.1` |

Logger: [`src/app_log.rs`](../src/app_log.rs). Phase + watchdog: [`src/hid_diag.rs`](../src/hid_diag.rs).

## Known internal causes (historical)

### A. Poll snapshot wipe (pad-only, short) — **fixed**

Poll no longer publishes an empty `ClearedPoll` snapshot. It still `drop_all()`s input handles so Poll can open, then `refresh_devices` + poll on the shared api; samples reopen afterward.

### B. ~5s `HidApi::new()` hang (full freeze) — **fixed (mitigated)**

Was: `ensure_all_pads` / `refresh_api` called **`HidApi::new()` every sample** (16–50 ms), plus UI `list_presence_paths` and Poll/Identify each constructing their own api. Hang typically `slow op=hidapi_new ms=5010–5011`.

Now: one worker `HidApi`; `enter_op("hidapi_refresh")` + `refresh_devices()` only when empty / presence cadence / Poll / open miss. Verify: `slow op=hidapi_new` should be gone; `hidapi_refresh` rare.

### C. Identify duration — **shortened**

Was ~1.7s (5×150 ms×2). Now 4 flashes × 125 ms × 2 half-steps = **1.0s**.

## Grep tokens

### `hid-trace.log`

- `open caller=` / `write caller=` / `read caller=`
- `steam=0|1`; failures also `fg=` / `fs=`
- `hid-diag: slow op=hid_trace_io` — sync append to the trace file took ≥50ms
- `hid-diag: worker stall ...` — watchdog (also in `app.log`)
- `HITCH_MARK`
- `write … ok bytes=547 expected=78` — BT lightbar return-size quirk ([Investigate next](#investigate-next))

### `app.log`

- `hid-diag: cmd begin=` / `snapshot clear` / `snapshot restore` / `sample short` / `sample stalled`
- `hid-diag: slow op=` — completed op ≥50ms (`open_device`, `read_timeout`, `hid_write`, `hidapi_refresh`, …)
- `hid-diag: worker stall kind=op|idle last_op=... gap_ms=... tier=250|500|1000|2000|5000`
- `start-nav: no … snapshot` / `snapshot restored` / `snapshot stale`
- `ui-diag: pad-poll stall` / `pad-poll slow` / `process-enum`
- `HITCH_MARK kind=lightbar|input source=hotkey|button`
- `PANIC at file:line:col: …` — custom hook in [`app_log::init`](../src/app_log.rs); required because `windows_subsystem = "windows"` discards stderr panic text (exit 101 with an empty console)
- `PANIC_BACKTRACE …` — capped `Backtrace::force_capture()` right after `PANIC at` (all builds); use to tell iced atlas/main-thread from the image worker
- `crash-restart: scheduled` / `launching` / `giving up after N` — panic hook arms a delayed self-relaunch ([`crash_restart`](../src/crash_restart.rs)); budget file `crash-restart.json` caps 3 restarts / 10 min
- `crash-restart: notice toast` — post-relaunch (or `--test-crash-toast`) one-shot toast with bug icon; `notice` flag in `crash-restart.json`

## How to tell causes apart

| Signature | Meaning |
|-----------|---------|
| `worker stall … last_op=hidapi_new` / `slow op=hidapi_new ms≈5000` | Pre-fix primary freeze |
| `slow op=hidapi_refresh` rare / large | Post-fix: throttled enum still slow on Windows |
| `ClearedPoll` + restore | Pre-fix Poll wipe (should no longer appear) |
| `pad-poll stall gap_ms` thousands | Iced UI thread stuck |
| Identify `total_ms≈1000` | Expected Identify after timing change |
| Identify `total_ms≈1700` | Pre-change Identify |
| `open`/`write`/`read` `err=` + `steam=1` | Device fight (not seen on freeze marks) |

## Multi-session hitch hunting

- Session id: `session=` on start lines in both logs.
- **F7** / button → `kind=lightbar`; **F8** / button → `kind=input` (`RegisterHotKey`, stamps even if iced is stuck).
- Look **5–10 s before** each `HITCH_MARK`.

## Suggested verify after this fix

- Pad nav >30 s; Identify (~1s, 4 flashes); F7/F8 if anything still freezes.
- Grep `slow op=hidapi_new` / `last_op=hidapi_new` — should be **absent**.
- Grep `hidapi_refresh` — should not fire every sample.
- Confirm Identify / lightbar / power-off / running badge / connect-disconnect.

## Investigate next

**BT lightbar write return size: `ok bytes=547 expected=78`**

- Seen repeatedly in `hid-trace.log` on DualSense **Bluetooth** lightbar writes (`caller=lightbar` / `caller=poll`, phases `claim` and `rgb`).
- hidapi still reports success (`ok`); the bar sometimes still looks unchanged — may be separate from the ~5s enum freezes.
- Likely avenues: wrong report length / padding for BT output reports, truncating vs accepting oversized returns, Steam or another writer overwriting immediately after, claim/`LIGHT_OUT` not sticking on BT.
- Capture: pair a “lightbar no-op” F7 mark with matching `write … bytes=547 expected=78` lines; compare USB vs BT write sizes; check build/CRC helpers in [`src/lightbar.rs`](../src/lightbar.rs) against DualSense BT output report layout.

## Out of scope (still)

- hid-trace I/O buffering (only if stalls show `slow op=hid_trace_io`).

---

## Captured sessions (2026-09-23 evening)

Pre-watchdog. Machine: Windows, DualSense **BT** serial `444648156926`, `steam=1`, F8 marks. Quiet gaps were visible but `last_op` unknown at the time.

| | Hitch 1 | Hitch 2 |
|--|---------|---------|
| `session=` | `1790145725366` | `1790146291895` |
| Mark epoch ms | `1790145868924` | `1790146352649` |
| `up_ms` | `143558` | `60753` |
| Pattern | Identify + **5.0s quiet** mid-flash | **5.0s quiet** + UI stall 4230ms |
| At F8 | `age_ms=10` fresh | `age_ms=15` fresh |

### Hitch 1 — excerpts

```text
[1790145859] INFO: hid-diag: cmd begin=Identify …
[1790145860] WARN: start-nav: snapshot stale age_ms=105 … reason=Identify
[1790145866] INFO: hid-worker: cmd=Identify flashes=10 … total_ms=6791
[1790145868] INFO: hid-diag: snapshot clear reason=ClearedPoll …
[1790145868] WARN: HITCH_MARK session=1790145725366 up_ms=143558 kind=input … age_ms=10 …
```

```text
[1790145859912] write caller=lightbar phase=rgb … rgb=4403ff … ok
# ← ~5016ms NO hid-trace →
[1790145864928] open caller=sample …
[1790145868924] HITCH_MARK session=1790145725366 …
```

### Hitch 2 — excerpts

```text
[1790146343] WARN: start-nav: snapshot stale age_ms=104 …
[1790146348] INFO: ui-diag: pad-poll stall gap_ms=4230
[1790146348] INFO: hid-diag: snapshot clear reason=ClearedPoll …
[1790146352] WARN: HITCH_MARK session=1790146291895 up_ms=60753 kind=input … age_ms=15 …
```

```text
[1790146343147] read caller=sample … ok
# ← ~5034ms NO hid-trace →
[1790146348181] read caller=sample … ok
[1790146352649] HITCH_MARK session=1790146291895 …
```

---

## Watchdog sessions (2026-09-23 later)

Same machine/pad. Build with `enter_op` + hid-watchdog. **Root cause confirmed: `last_op=hidapi_new` ≈ 5.011s.**

| | LB1 (F7) | IN3 (F8) | IN4 (F8) |
|--|----------|----------|----------|
| `session=` | `1790147353650` | `1790147353650` | `1790147843512` |
| Mark epoch (app.log sec) | `1790147471` | `1790147569` | `1790147913` |
| `up_ms` | `117437` | `215633` | `69624` |
| Kind | lightbar | input | input |
| Pattern | F7 mid-**`hidapi_new`** hang (`age_ms=2752`); `slow op=hidapi_new ms=5011` | **`hidapi_new` 5011ms** + UI stall 4867ms + Poll | Two Identifies (~1732 / 1726ms) + Poll wipes; **no** 5s `hidapi_new` |
| At mark | stale snapshot | fresh after restore | fresh |

### LB1 — F7 during `hidapi_new`

```text
[1790147468] WARN: start-nav: snapshot stale age_ms=127 seq=3859 reason=Sample
[1790147468] WARN: hid-diag: worker stall kind=op last_op=hidapi_new gap_ms=321 tier=250
[1790147468] WARN: hid-diag: worker stall kind=op last_op=hidapi_new gap_ms=522 tier=500
[1790147469] WARN: hid-diag: worker stall kind=op last_op=hidapi_new gap_ms=1025 tier=1000
[1790147470] WARN: hid-diag: worker stall kind=op last_op=hidapi_new gap_ms=2029 tier=2000
[1790147471] WARN: HITCH_MARK session=1790147353650 up_ms=117437 kind=lightbar source=hotkey pads=1 reason=Sample age_ms=2752 seq=3859 steam=1 fg=sdsc-utils.exe fs=0
[1790147473] INFO: hid-diag: slow op=hidapi_new ms=5011
```

### IN3 — classic full freeze

```text
[1790147562] WARN: start-nav: snapshot stale age_ms=107 …
[1790147562] WARN: hid-diag: worker stall kind=op last_op=hidapi_new gap_ms=330 tier=250
[1790147562] WARN: hid-diag: worker stall kind=op last_op=hidapi_new gap_ms=531 tier=500
[1790147563] WARN: hid-diag: worker stall kind=op last_op=hidapi_new gap_ms=1034 tier=1000
[1790147564] WARN: hid-diag: worker stall kind=op last_op=hidapi_new gap_ms=2038 tier=2000
[1790147567] INFO: hid-diag: slow op=hidapi_new ms=5011
[1790147567] INFO: ui-diag: pad-poll stall gap_ms=4867
[1790147567] INFO: hid-diag: snapshot clear reason=ClearedPoll …
[1790147569] WARN: HITCH_MARK session=1790147353650 up_ms=215633 kind=input source=hotkey pads=1 reason=Sample age_ms=0 seq=7016 steam=1 fg=sdsc-utils.exe fs=0
```

### IN4 — Identify/Poll without 5s enum

```text
[1790147905] INFO: hid-diag: cmd begin=Identify …
[1790147906] INFO: hid-worker: cmd=Identify flashes=10 … total_ms=1732
[1790147907] INFO: hid-diag: snapshot clear reason=ClearedPoll …
[1790147908] INFO: hid-diag: cmd begin=Identify …
[1790147910] INFO: hid-worker: cmd=Identify flashes=10 … total_ms=1726
[1790147913] WARN: HITCH_MARK session=1790147843512 up_ms=69624 kind=input … age_ms=29 …
[1790147913] INFO: hid-diag: snapshot clear reason=ClearedPoll …
```

No `last_op=hidapi_new` / `slow op=hidapi_new ms=5xxx` in this window — felt hitch is Identify ownership + Poll wipe (causes A/C), not the enum hang.

### Additional unmarked blackout (same session as LB1/IN3)

```text
[1790147681]–[1790147686] worker stall last_op=hidapi_new … tier through 5000
[1790147686] INFO: hid-diag: slow op=hidapi_new ms=5010
```

### Side notes

- BT writes still log `ok bytes=547 expected=78`.
- Shorter `slow op=hidapi_new ms=50–166` also appear; user-visible multi-second freezes match the **~5010ms** completions.
- Pre-watchdog Hitch 1 `Identify total_ms=6791` is consistent with a ~5s `hidapi_new` buried inside Identify flash reopen/enumerate.

## Toast Z-order vs Settings / Start freeze (Windows)

iced multi-window present starvation: [iced#3108](https://github.com/iced-rs/iced/issues/3108) / [#3320](https://github.com/iced-rs/iced/issues/3320).

**Do not demote** the toast to `HWND_NOTOPMOST` when Settings/Start/popup are open — that puts the toast under Cursor.

**Do not** raise toast above Start every `ToastFrame` — that starves Start presents.

**Do:** keep toast `HWND_TOPMOST` (above Cursor); raise Start/Settings into the same topmost band *above* the toast (corner toast stays visible beside centered Start); `gain_focus` the interactive window.

Debug grep: `ui-diag: place toast gen=`, `ui-diag: sync toast z-order (toast + raise UI)`.

## Queued toast wrong content (Windows)

Symptom: two connect toasts both showed the first pad’s %, then recreate made only one toast appear.

`app.log` proved the **model is correct** (`toast show … percent=55` then `percent=100`). Stale swapchain on reuse; closing/recreating the HWND dropped the follow-up toast.

Fix: reuse one toast HWND; on handoff skip hide and **remount** (±1px resize + `RedrawWindow`); `PlaceToast` is generation-guarded.

Debug grep: `ui-diag: toast show`, `ui-diag: place toast gen=`, `ui-diag: toast handoff remount`.
