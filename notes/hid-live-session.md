# HID live session (hot-path battery, silence drop, rumble)

**Status:** do-not-regress. **When to read:** live pad sampling, disconnect while BT still listed, rumble vs lightbar handles. Index: [README.md](README.md). Detailed form of [`.cursor/rules/input-latency-bluetooth.mdc`](../.cursor/rules/input-latency-bluetooth.mdc).

## UI rumble (haptics)

Start, Browser, hub, and Configure nav/action cues may pulse DualSense motors via `HidCmd::Rumble`. **Do not** route rumble through `write_rgb_exclusive` (that drops the input-cache handle and reopens). Rumble keeps a **separate long-lived output handle**, opens ranked DualSense collections like lightbar (USB gamepad → USB other → BT gamepad → BT other), and calls `prepare_bt_output_mode` once on BT opens. Poll / Shutdown drop all rumble handles; **PowerOff drops only the target** pad’s rumble + input handle + live status (siblings stay in `live_pads` / the nav snapshot). Intentional skip-connect-cooldown arms only when that pad was the sole live controller. Debug: grep `hid-diag: rumble` / `hid-diag: power-off drop` / write traces with `caller=rumble`.

## Battery from input stream (hot path)

While a UI surface is hot (Start, Browser, hub, pad-input), **do not** run exclusive battery `Poll` (`drop_all` + `read_timeout`). Battery is parsed from the same USB `0x01` / BT `0x31` reports the sample loop already drains (`parse_battery_from_report`). The app synthesizes `Controllers` from `HidWorkerHandle::live_controllers()` and applies lightbar only on membership / color-bucket change (`reassert_lightbar_for_pads`).

**Never probe an actively used pad.** `LivePadStatus.last_input_at` updates on every non-resting sample (buttons / sticks / D-pad / triggers / bumpers). For ~3s after the last such sample, `poll_incremental` skips both the cached-handle battery read and any full probe: battery comes from the live stream instead. Idle, new, and ghost paths remain probeable (failed paths still use the exponential backoff).

Cold (tray idle): classic timed `Poll` unchanged.

**Silence drop:** a powered-off DualSense often lingers in the Windows HID list while stopping input reports. Short sample timeouts still keep the open handle and the last reading (no reconnect / no extra BT traffic). After ~200ms of consecutive sample timeouts (~50 × 4ms), drop that pad from `live_pads` and stop republishing its stale nav reading. Silence streak lives on the open handle so Identify-only sampling does not age other pads. The hot path reads `live_controllers()` every loop (~16ms), not only on the presence tick. “Wait for first sample” (`live` empty + presence nonempty) applies only when `session.controllers` is already empty — otherwise a silence drop must clear the session. The hub controllers strip and popup rows refresh from the reconciled `session.controllers` (after the one-miss hold) on hot-path changes and on presence-empty clear so the UI and disconnect toast move together.

Debug grep: `hid-diag: live silence serial=`, `hid-diag: sample short`, `hid-diag: rumble`, `hid-diag: power-off drop`.
