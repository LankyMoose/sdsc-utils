# Multi-controller driver seam (spec, not yet implemented)

Goal: a fork can add a second controller family (Xbox, DualShock, Switch Pro)
without touching UI, session, persist, or the DualSense implementation.
Lightbar / rumble / power-off are per-family capabilities, not shared bools.

Non-goals: no UX change, no new supported pad in this pass. DualSense stays
the only driver. No async runtime, no second process.

## Current coupling (why a fork hurts today)

- `controller/driver.rs` is a 1-family forwarder: `is_gamepad` →
  `is_dualsense_gamepad`. Comment admits "no multi-family registry".
- `controller/model.rs`: `ControllerKind` has one variant; `PowerState::from_nibble`
  is DualSense nibble encoding in the *shared* model; `ControllerStatus` carries
  `supports_lightbar: bool` / `supports_power_off: bool`, always `true` at the
  only producer (`dualsense/battery.rs:dualsense_status`). Consumers branch ad-hoc
  (`app/mod.rs` power-off + lightbar gates, `hid/poll.rs:reassert_lightbar_for_pads`,
  `app/start.rs` row badges, `hid/worker.rs` identify/rumble paths).
- `dualsense/lightbar.rs` (~900 lines) is all DualSense wire format: report IDs
  `0x02`/`0x31`, offsets 41/44–46, CRC seed `0xA2`, `LIGHT_OUT` claim setup, BT
  calibration sniff, Steam-claim fight. `ClaimStore` is DualSense+Steam logic
  threaded through generic poll/worker signatures.
- Same shape for rumble (`dualsense/rumble.rs` compatible-vibration +
  `worker.rs:RumbleCache`), power-off (BT feature `0x08` in `battery.rs`), input
  (`dualsense/input.rs` → `domain/pad.rs` nav types), percent mapping (11 firmware
  steps → midpoints in `battery.rs:percent_from_level`).
- Dispatch is hardcoded: `hid/poll.rs` filters by `driver::is_gamepad` then calls
  `dualsense::product_name` / `read_battery` directly. `dualsense`/`lightbar`
  strings appear in ~40 files including `ui/`.
- What already helps: UI mostly reads `ControllerStatus`; `Connection` is generic;
  `Policy` + `HidIoLock` are now threaded (not global) and `HidTransport`
  covers the sample drain in tests (production poll/lightbar still use `hidapi`
  directly), so a second family reuses the cadence/lock plumbing.

## Proposed shape

```rust
// controller/driver.rs — the only dispatch point.
pub struct Capabilities { pub lightbar: bool, pub rumble: bool, pub power_off: bool }

pub trait ControllerDriver: Send + Sync {
    fn kind(&self) -> ControllerKind;
    fn matches(&self, info: &DeviceInfo) -> bool;           // VID/PID + usage page
    fn product_name(&self, product_id: u16) -> &'static str;
    fn read_battery(&self, dev: &HidDevice) -> Result<BatteryReading, String>;
    fn parse_input(&self, buf: &[u8], base: usize) -> PadSample;
    fn capabilities(&self) -> Capabilities;
    // Optional backends; None = family lacks it. UI never names the wire type.
    fn lightbar(&self) -> Option<&dyn LightbarBackend>;
    fn rumble(&self) -> Option<&dyn RumbleBackend>;
    fn power_off(&self, api: &HidApi, serial: &str) -> Result<(), String>;
}

pub static DRIVERS: &[&dyn ControllerDriver] = &[&DUALSENSE]; // registry
```

- `BatteryReading` (`percent: u8`, `state: PowerState`, `connection: Connection`)
  stays shared, but `from_nibble` moves into `dualsense::` — the shared model
  takes a plain `PowerState`, never a nibble. Percent stays `u8`; the 11-step
  midpoint map stays DualSense-private.
- `ControllerStatus` keeps `kind/product/connection/serial/percent/state` plus a
  single `capabilities: Capabilities` (replaces the two bools; add `rumble` now
  so workers stop assuming it). UI branches only on `capabilities`.
- `LightbarBackend { apply(api, serial, color, claims-ish) }` — DualSense impl owns
  report bytes, CRC, `LIGHT_OUT`, `ClaimStore`. Xbox impl is simply absent
  (`None`), so `poll::reassert_lightbar_for_pads` becomes "if let Some backend".
- `RumbleBackend` / power-off likewise. `worker.rs:RumbleCache` keys off
  `capabilities.rumble`, not family name.

## Migration (additive, one family at a time)

1. Add `Capabilities` + `ControllerDriver` + `DRIVERS` with the single
   `DUALSENSE` entry. `driver::is_gamepad` becomes `DRIVERS.iter().any(matches)`.
2. Move `from_nibble` + step map behind the DualSense impl; `model.rs` keeps only
   the generic enum + `is_discharging`/`is_low_battery`.
3. Replace `supports_lightbar/power_off` reads with `capabilities.*` (mechanical,
   same call sites listed above). Keep serialized `controllers.json` shape
   (`kind` already persisted; capabilities are runtime, not stored).
4. Hide `ClaimStore`/report consts from `poll`/`worker` signatures behind
   `&dyn LightbarBackend`. `poll` takes `&[&dyn ControllerDriver]` (default:
   `DRIVERS`) so tests inject a fake with canned battery/input and no HID.
5. Fork cookbook lands in this note: implement 7 fns + register in `DRIVERS`;
   UI/session/persist untouched. DualSense file stays the reference impl.

## Test plan

- Fake driver in `hid::` tests: canned `read_battery`/`parse_input`, `capabilities`
  all-false → poll returns status, lightbar/rumble/power-off paths skipped.
- DualSense conformance: existing `battery`/`lightbar`/`input` tests unchanged
  (they now exercise the impl, not free fns).
- `cargo fmt`, both clippys, `cargo test --all-targets` green; no UX change.

## Fork cookbook (after this lands)

1. Copy `dualsense/` to `myfamily/` (battery/input/identity only; skip lightbar
   unless the pad has LEDs).
2. Implement `ControllerDriver` (VID/PID in `matches`, report parsing in
   `parse_input`/`read_battery`, `capabilities` honest — `None` backends allowed).
3. Add to `DRIVERS`, add `ControllerKind` variant, add `product_name` string.
4. Done — Start/toast/popup/known/emulate work via `ControllerStatus`.
