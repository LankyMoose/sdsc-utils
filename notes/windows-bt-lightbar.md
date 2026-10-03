# Windows DualSense BT lightbar (silent no-ops)

**Status:** historical (fixed). **When to read:** lightbar writes look `ok` but RGB does not change; BT output transport / Steam claim. Index: [README.md](README.md).

## Investigate next (closed)

**BT lightbar silent no-op on Windows — interrupt path (1.4.1)**

- Captured with F7: session `1790507921026` (below). Interrupt `write` returned `ok bytes=547 expected=78` while the bar stayed unchanged.
- Root cause: Windows DualSense BT advertises `OutputReportByteLength=547`; interrupt `WriteFile` pads and “succeeds” without updating RGB. Poll also wrote on the battery-read handle, which can accept `Ok` without changing the bar.

**BT lightbar still no-op after control `CreateFileW` path (1.4.2) — fixed**

- Session `1790586627711` (debug) / `1790596459165` (release): every write was `transport=control` `ok bytes=78 expected=78` (`rgb=6b16e4` / `7f12e2` on `444648156926`, `steam=1`) and the bar stayed unchanged. No `lightbar write failed` on the live pad.
- Side-handle `CreateFileW` + `HidD_SetOutputReport` (pad to 547) returned success without updating RGB. Claim (`LIGHT_OUT`) was also forced before every SetRgb while start-nav held an input handle, fading the bar before each color.
- Session `1790683895217` (after send_output_report on open handle + claim-once): still `claim=true` then RGB `ok` with `steam=1`; Identify flashed in hid-trace (`d2d4dc`/`ac0c9e`) but the bar did not change.
- Session `1790684159072` (Steam skip landed): all writes `claim=false` `transport=control` `ok`, Identify still traced, bar still unchanged — control Set_Report is a silent no-op on this machine even for RGB-only.
- Fix: skip `LIGHT_OUT` while `steam.exe` is running; Windows BT RGB uses interrupt `write` again (control only as hard-error fallback); try every DualSense HID collection for the serial; request calibration feature before BT writes. Poll still drops the battery-read handle before lightbar.

## Grep tokens

- `write … ok bytes=547 expected=78` — historical BT interrupt padding quirk (pre-control-path)
- `transport=control` — Windows BT lightbar via hidapi `send_output_report` (side CreateFileW path was a silent no-op)
- `transport=control|interrupt` — lightbar write path
- `HITCH_MARK kind=lightbar`
- `lightbar write failed`

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
