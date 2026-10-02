# UI lock-up / HID fight investigation (2026-09-23)

Hand-off notes. **Fix landed (reuse HidApi):** worker keeps one long-lived `HidApi`, refreshes via `refresh_devices()` on a throttle (not every sample), shares that api into Poll/Identify/lightbar/power-off, **does not clear** the input snapshot on Poll, publishes presence paths for the UI tick (no UI-thread `HidApi::new`), runs process enum via `Task::perform`, and Identify is **4 flashes / 1s** (`IDENTIFY_FLASH_COUNT=4`, `IDENTIFY_FLASH_MS=125`).

**Diagnostics are debug-build only** (`cfg(debug_assertions)`): `hid-trace.log`, hitch F7/F8 + report buttons, worker phase watchdog / `enter_op` traces, and investigation-volume `hid-diag:` / `ui-diag:` lines no-op or omit in `--release`. Future agents: see [`.cursor/rules/debug-diagnostics.mdc`](../.cursor/rules/debug-diagnostics.mdc) — new work must include a similar level of non-release diagnostics.

Historical evidence of the ~5s freezes is preserved below (`last_op=hidapi_new`). See [Investigate next](#investigate-next) for BT lightbar write path.

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
- `write … ok bytes=547 expected=78` — historical BT interrupt padding quirk (pre-control-path)
- `transport=control` — Windows BT lightbar via hidapi `send_output_report` (side CreateFileW path was a silent no-op)
- `transport=control|interrupt` — lightbar write path (Windows BT should be `control`)

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
- **Sibling:** wgpu `iced_wgpu::image texture atlas` / `Texture::create_view` panics (seen on **v1.4.1** after the stable-Rgba mitigation) → [`wgpu-image-atlas-crash.md`](wgpu-image-atlas-crash.md). Not fixed by toast/Start slide latch work.

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

**BT lightbar silent no-op on Windows — interrupt path (1.4.1)**

- Captured with F7: session `1790507921026` (below). Interrupt `write` returned `ok bytes=547 expected=78` while the bar stayed unchanged.
- Root cause: Windows DualSense BT advertises `OutputReportByteLength=547`; interrupt `WriteFile` pads and “succeeds” without updating RGB. Poll also wrote on the battery-read handle, which can accept `Ok` without changing the bar.

**BT lightbar still no-op after control `CreateFileW` path (1.4.2) — fixed**

- Session `1790586627711` (debug) / `1790596459165` (release): every write was `transport=control` `ok bytes=78 expected=78` (`rgb=6b16e4` / `7f12e2` on `444648156926`, `steam=1`) and the bar stayed unchanged. No `lightbar write failed` on the live pad.
- Side-handle `CreateFileW` + `HidD_SetOutputReport` (pad to 547) returned success without updating RGB. Claim (`LIGHT_OUT`) was also forced before every SetRgb while start-nav held an input handle, fading the bar before each color.
- Session `1790683895217` (after send_output_report on open handle + claim-once): still `claim=true` then RGB `ok` with `steam=1`; Identify flashed in hid-trace (`d2d4dc`/`ac0c9e`) but the bar did not change.
- Session `1790684159072` (Steam skip landed): all writes `claim=false` `transport=control` `ok`, Identify still traced, bar still unchanged — control Set_Report is a silent no-op on this machine even for RGB-only.
- Fix: skip `LIGHT_OUT` while `steam.exe` is running; Windows BT RGB uses interrupt `write` again (control only as hard-error fallback); try every DualSense HID collection for the serial; request calibration feature before BT writes. Poll still drops the battery-read handle before lightbar.

## Out of scope (still)

- hid-trace I/O buffering (only if stalls show `slow op=hid_trace_io`).

---

## Captured session — lightbar no-op F7 (2026-09-28)

SDSC Utils **1.4.1** debug. DualSense **BT** serial `444648156926`, 85%, `steam=1`. F7 ~3s after the first claim+rgb pair; bar never took `rgb=431fe8` (correct spectrum color for 85%). Every lightbar transfer is `ok` — no `lightbar write failed` line. Power-off ~15s after the mark succeeded on the control endpoint (`size=48, seed=0x53`).

| | LB-noop (F7) |
|--|--|
| `session=` | `1790507921026` |
| Mark epoch ms | `1790507971682` |
| `up_ms` | `50655` |
| Kind | lightbar |
| Pattern | claim+rgb `ok bytes=547 expected=78`; reassert same RGB; bar unchanged; `age_ms=0` |
| At mark | fresh Sample snapshot, `fg=sdsc-utils.exe` |

### `app.log` (verbatim; start-nav omitted)

```text
[1790507921] INFO: SDSC Utils (sdsc-utils) 1.4.1 starting session=1790507921026
[1790507921] INFO: hitch hotkeys armed: F7=lightbar F8=input
[1790507962] INFO: hid-diag: sample short cached=1 published=0
[1790507962] INFO: hid-diag: cmd begin=Poll since_publish_ms=32
[1790507962] INFO: hid-worker: cmd=Poll enumerate_ms=0 open_ms=0 io_ms=10 total_ms=19
[1790507962] INFO: ui-diag: toast show heading="DualSense (Bluetooth)" percent=85 queue_left=0
[1790507968] INFO: hid-diag: cmd begin=Poll since_publish_ms=0
[1790507968] INFO: hid-diag: slow op=hidapi_refresh ms=71
[1790507968] INFO: hid-worker: cmd=Poll enumerate_ms=0 open_ms=0 io_ms=0 total_ms=74
[1790507971] WARN: HITCH_MARK session=1790507921026 up_ms=50655 kind=lightbar source=hotkey pads=1 reason=Sample age_ms=0 seq=6320 steam=1 fg=sdsc-utils.exe fs=0
[1790507974] INFO: hid-diag: cmd begin=Poll since_publish_ms=0
[1790507974] INFO: hid-worker: cmd=Poll enumerate_ms=0 open_ms=0 io_ms=0 total_ms=72
[1790507980] INFO: hid-diag: cmd begin=Poll since_publish_ms=0
[1790507980] INFO: hid-worker: cmd=Poll enumerate_ms=0 open_ms=0 io_ms=0 total_ms=73
[1790507986] INFO: hid-diag: cmd begin=Poll since_publish_ms=0
[1790507986] INFO: hid-worker: cmd=Poll enumerate_ms=0 open_ms=0 io_ms=0 total_ms=73
[1790507986] INFO: power-off sent for 444648156926 via \\?\HID#{00001124-0000-1000-8000-00805f9b34fb}_VID&0002054c_PID&0ce6#8&15b6e16c&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030} (size=48, seed=0x53)
[1790507986] INFO: power-off sent for 444648156926
[1790507987] INFO: hid-diag: sample timeout keep_handle serial=444648156926 bus=bt
[1790507989] INFO: ui-diag: toast show heading="DualSense (Bluetooth)" percent=85 queue_left=0
```

### `hid-trace.log.1` / `hid-trace.log` (verbatim)

```text
[1790507962452] read caller=sample serial=444648156926 bus=bt ms=6 fail=truncated steam=1 fg=Cursor.exe fs=0
[1790507962566] open caller=poll serial=444648156926 path=\\?\HID#{00001124-0000-1000-8000-00805f9b34fb}_VID&0002054c_PID&0ce6#8&15b6e16c&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030} iface=-1 usage_page=0x0001 usage=0x0005 bus=bt ms=0 ok steam=1
[1790507962576] write caller=poll phase=claim serial=444648156926 bus=bt claim=true retry=false ms=0 ok bytes=547 expected=78 steam=1
[1790507962577] write caller=poll phase=rgb serial=444648156926 bus=bt rgb=431fe8 claim=true retry=false ms=0 ok bytes=547 expected=78 steam=1
[1790507968675] write caller=poll phase=rgb serial=444648156926 bus=bt rgb=431fe8 claim=false retry=false ms=0 ok bytes=547 expected=78 steam=1
[1790507971682] HITCH_MARK session=1790507921026 up_ms=50655 kind=lightbar source=hotkey pads=1 reason=Sample age_ms=0 seq=6320 steam=1 fg=sdsc-utils.exe fs=0
[1790507974677] write caller=poll phase=rgb serial=444648156926 bus=bt rgb=431fe8 claim=false retry=false ms=0 ok bytes=547 expected=78 steam=1
[1790507980669] write caller=poll phase=rgb serial=444648156926 bus=bt rgb=431fe8 claim=false retry=false ms=0 ok bytes=547 expected=78 steam=1
[1790507986670] write caller=poll phase=rgb serial=444648156926 bus=bt rgb=431fe8 claim=false retry=false ms=0 ok bytes=547 expected=78 steam=1
[1790507987303] read caller=sample serial=444648156926 bus=bt ms=0 fail=io err=hidapi error: ReadFile: (0x0000048F) The device is not connected. steam=1 fg=sdsc-utils.exe fs=0
```

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

## Connect toast slide vs Start open (Windows)

Symptom: on 0→1 connect (toast + Start together), the Connected toast sometimes never appears, freezes mid-slide, or pops into the rest pose.

Cause (historical): slide-in used a 250ms wall clock that stopped issuing `move_to` once elapsed ≥ 250ms, and the clock started in `PlaceToast` before show. Opening Start in the same batch stalled the UI.

Cause (session `1790509148457`): a 500ms Start deadline fired **before** `place toast`, so Start’s window create starved the toast present (iced#3108 / #3320). Log order: `toast show` → `defer start` → `start open after toast deadline` → `place toast gen=1`.

Cause (session `1790510886493`): after the state machine landed, a **2s Placing wall-clock failsafe** treated a stale `ToastFrame` `raw_dt` (time since app boot / last toast) as “placement gave up.” Log: `Idle->Placing` → `Placing->Idle` + `start open` in the same second — toast never reached `SlidingIn`. Reconnect after intentional power-off then showed toast with `after=Nothing` because `START_CONNECT_COOLDOWN` armed on the power-off 1→0.

Fix: pure presentation state machine in [`src/ui/toast/machine.rs`](../src/ui/toast/machine.rs). Phases `Placing → SlidingIn → Resting → SlidingOut`. **Placing is event-only** (`Frame` ignored for phase change; leave only on `Shown` or `Dismiss`) — **no placing wall-clock failsafe** (stays removed). Start opens only as an `OpenStart` effect from slide rest or dismiss finish. `RaiseInteractive` only after Resting. Placement uses `primary_toast_area()` immediately (no `monitor_size` hop). Intentional power-off sets `skip_next_connect_cooldown` so the next reconnect can latch `OpenStart` again (unexpected disconnects still get the 5s ghost-flap cooldown). Do not put the toast on a second thread — iced presents on one UI thread.

**Reopen gesture during latch:** `suppresses_reopen_gesture()` while `after == OpenStart` and phase is Placing/SlidingIn. During Placing, suppress lifts after ~60 Frame ticks (~1s) so a lost `Shown` cannot block the gesture forever; phase stays Placing and no `OpenStart` is emitted from that budget.

### 0→1 Start survive toast (robustness)

Toast and Start are separate gates. To stop Connected-without-Start on turn-on:

- **One-miss hold:** a failed battery/open poll keeps the last status for one tick; the second consecutive miss accepts the drop. Presence-empty (HID list gone) still clears immediately.
- **Short arrival:** nonempty stretch under 2s does not arm the 5s ghost cooldown (enumerate blip). Stable disconnects still arm it; intentional Power Off still skips.
- **Pending retry:** `start_auto_open_pending` is set when 0→1 auto-open gates pass; cleared only once Start is visible. Close-in-flight leaves it set; `WindowClosed` retries. Confirmed empty clears pending and `clear_after()` even when Start is not visible.

Debug grep: `ui-diag: defer start until toast slide settles`, `ui-diag: toast Placing->SlidingIn`, `ui-diag: toast SlidingIn->Resting`, `ui-diag: toast show … body=Connected after=OpenStart`, `ui-diag: place toast gen=`, `ui-diag: start open after toast settle`, `ui-diag: skip connect cooldown (intentional power-off)`, `ui-diag: skip connect cooldown (short arrival)`, `ui-diag: hold pad across missed read serial=`, `ui-diag: retry start open after close`, `ui-diag: reopen gesture suppressed (toast OpenStart pending)`.
