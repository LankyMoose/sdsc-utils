# HID live session (hot-path battery, silence drop, rumble)

**Status:** do-not-regress. **When to read:** live pad sampling, disconnect while BT still listed, rumble vs lightbar handles. Index: [README.md](README.md). Detailed form of [`.cursor/rules/input-latency-bluetooth.mdc`](../.cursor/rules/input-latency-bluetooth.mdc).

## Start-screen rumble (haptics)

Start-menu nav/action cues may pulse DualSense motors via `HidCmd::Rumble`. **Do not** route rumble through `write_rgb_exclusive` (that drops the input-cache handle and reopens). Rumble keeps a **separate long-lived output handle**, opens ranked DualSense collections like lightbar (USB gamepad → USB other → BT gamepad → BT other), and calls `prepare_bt_output_mode` once on BT opens. Poll / Shutdown drop all rumble handles; **PowerOff drops only the target** pad’s rumble + input handle + live status (siblings stay in `live_pads` / the nav snapshot). Intentional skip-connect-cooldown arms only when that pad was the sole live controller. Debug: grep `hid-diag: rumble` / `hid-diag: power-off drop` / write traces with `caller=rumble`.

## Battery from input stream (hot path)

While Start / pad-input is hot, **do not** run exclusive battery `Poll` (`drop_all` + `read_timeout`). Battery is parsed from the same USB `0x01` / BT `0x31` reports the sample loop already drains (`parse_battery_from_report`). Service synthesizes `Controllers` from `HidWorkerHandle::live_controllers()` and applies lightbar via exclusive `SetRgb` only on membership / color-bucket change.

Cold (tray idle): classic timed `Poll` unchanged.

**Silence drop:** a powered-off DualSense often lingers in the Windows HID list while stopping input reports. Short sample timeouts still keep the open handle and the last reading (no reconnect / no extra BT traffic). After ~200ms of consecutive sample timeouts (~50 × 4ms), drop that pad from `live_pads` and stop republishing its stale nav reading. Silence streak lives on the open handle so Identify-only sampling does not age other pads. Service hot path reads `live_controllers()` every loop (~16ms), not only on the 500ms presence tick. “Wait for first sample” (`live` empty + presence nonempty) applies only when `session.controllers` is already empty — otherwise a silence drop must clear the session. Shell Start Controllers (compact list **and** immersive dock) refresh from `ServiceMessage::Controllers`; send the **reconciled** `session.controllers` (after the one-miss hold) on hot-path changes and on presence-empty clear so the UI and disconnect toast move together.

Debug grep: `service: hot path waiting for live battery sample`, `service: hot lightbar connect`, `service: hot lightbar color`, `hid-diag: sample short`, `hid-diag: live silence serial=`.
