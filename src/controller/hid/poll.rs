//! Poll controller battery status and reassert lightbar under one HID lock.
//!
//! Also owns the presence / battery / liveness cadence used by the iced daemon.

use crate::controller::driver;
use crate::controller::dualsense::battery;
use crate::controller::dualsense::identity::{self as dualsense, hid_serial, is_storable_serial};
use crate::controller::dualsense::lightbar::{self, HidPhaseTiming};
use crate::controller::hid::launch_paths::LaunchPaths;
use crate::controller::model::ControllerStatus;
use crate::domain::color::color_for_battery_percent;
use crate::platform::app_log;
use hidapi::{DeviceInfo, HidApi};
use std::time::{Duration, Instant};

/// How often to scan for DualSense connect/disconnect when pads are already known.
pub const PRESENCE_INTERVAL: Duration = Duration::from_secs(3);
/// Faster presence scan while the tray is empty (and start screen can auto-open on 0→1).
pub const PRESENCE_INTERVAL_EMPTY: Duration = Duration::from_millis(500);
/// How often to re-read battery / lightbar when membership is stable and pads are readable.
pub const BATTERY_INTERVAL: Duration = Duration::from_secs(60);
/// While the tray shows connected pads, re-probe often so a powered-off BT pad
/// (still lingering in the HID list) is dropped quickly — and so lightbar RGB is
/// reasserted if another HID writer overwrote the bar.
pub const LIVENESS_INTERVAL: Duration = Duration::from_secs(5);
/// When HID lists pads but battery reads keep failing, retry sooner than BATTERY_INTERVAL.
pub const UNREAD_RETRY_INTERVAL: Duration = Duration::from_secs(15);

/// Opened pad after a successful battery read (device dropped before lightbar).
struct PolledPad {
    status: ControllerStatus,
}

/// Collapse USB+BT of the same pad, keeping the preferred open handle (USB wins).
fn dedupe_polled(mut pads: Vec<PolledPad>) -> Vec<PolledPad> {
    let mut unique: Vec<PolledPad> = Vec::with_capacity(pads.len());

    for pad in pads.drain(..) {
        if !is_storable_serial(&pad.status.serial) {
            unique.push(pad);
            continue;
        }

        if let Some(existing) = unique
            .iter_mut()
            .find(|s| is_storable_serial(&s.status.serial) && s.status.serial == pad.status.serial)
        {
            if pad.status.connection.is_usb() && !existing.status.connection.is_usb() {
                *existing = pad;
            }
        } else {
            unique.push(pad);
        }
    }

    unique
}

fn status_from_reading(
    product: &str,
    reading: &battery::BatteryReading,
    serial: String,
) -> ControllerStatus {
    battery::dualsense_status(
        0,
        product,
        reading.connection,
        serial,
        reading.percent,
        reading.state,
    )
}

/// Poll connected pads and reassert lightbar RGB where supported.
///
/// `previously_connected` is the serial set from the last successful UI snapshot.
/// Serials not in that set are treated as fresh connects: claim is cleared so
/// `LIGHT_OUT` + RGB always run (needed after a presence-only disconnect that
/// never synced claims).
pub fn poll_controllers(previously_connected: &[String]) -> Result<Vec<ControllerStatus>, String> {
    dualsense::with_hid_lock(|| {
        let api = HidApi::new().map_err(|e| e.to_string())?;
        let mut timing = HidPhaseTiming::default();
        poll_controllers_with_api(&api, previously_connected, &mut timing, None, None)
    })
}

/// Daemon worker entry: no outer lock (worker is exclusive). Uses caller's `HidApi`.
///
/// When `launch` / `launch_quiet` are set, serials for paths present at first enum
/// are appended to `launch_quiet` on a successful battery read (no Connected toast).
pub fn poll_controllers_timed(
    api: &HidApi,
    previously_connected: &[String],
    launch: Option<&mut LaunchPaths>,
    launch_quiet: Option<&mut Vec<String>>,
) -> (Result<Vec<ControllerStatus>, String>, HidPhaseTiming) {
    let mut timing = HidPhaseTiming::default();
    let result =
        poll_controllers_with_api(api, previously_connected, &mut timing, launch, launch_quiet);
    (result, timing)
}

fn poll_controllers_with_api(
    api: &HidApi,
    previously_connected: &[String],
    timing: &mut HidPhaseTiming,
    mut launch: Option<&mut LaunchPaths>,
    mut launch_quiet: Option<&mut Vec<String>>,
) -> Result<Vec<ControllerStatus>, String> {
    let enum_started = Instant::now();
    let devices: Vec<&DeviceInfo> = api
        .device_list()
        .filter(|d| driver::is_gamepad(d))
        .collect();
    timing.enumerate_ms += enum_started.elapsed().as_millis();

    if devices.is_empty() {
        lightbar::sync_lightbar_claims(std::iter::empty::<&str>());
        return Ok(Vec::new());
    }

    let mut pads = Vec::with_capacity(devices.len());

    for info in devices {
        if !driver::is_gamepad(info) {
            continue;
        }
        let product = dualsense::product_name(info.product_id());
        let hid = hid_serial(info);

        let open_started = Instant::now();
        let hint = hid_serial(info);
        let device = match info.open_device(api) {
            Ok(d) => {
                let ms = open_started.elapsed().as_millis();
                timing.open_ms += ms;
                crate::controller::hid::diag::trace_open("poll", info, &hint, ms, Ok(()));
                d
            }
            Err(err) => {
                let ms = open_started.elapsed().as_millis();
                timing.open_ms += ms;
                crate::controller::hid::diag::trace_open(
                    "poll",
                    info,
                    &hint,
                    ms,
                    Err(&err.to_string()),
                );
                app_log::warn(format!(
                    "failed to open {product} (hid serial {hid}): {err}"
                ));
                continue;
            }
        };

        let io_started = Instant::now();
        let path = info.path().to_string_lossy().into_owned();
        let serial = dualsense::resolve_device_identity(info, &device);
        match battery::read_battery(&device) {
            Ok(reading) => {
                timing.io_ms += io_started.elapsed().as_millis();
                // Drop the read handle before lightbar: DualSense Windows handles that
                // have been used for input often accept write()/send_output_report with Ok
                // without updating the bar (see write_rgb_exclusive).
                drop(device);
                if let Some(quiet) = launch
                    .as_mut()
                    .and_then(|l| l.take_serial_for_path(&path, &serial))
                {
                    if let Some(out) = launch_quiet.as_mut() {
                        out.push(quiet);
                    }
                }
                pads.push(PolledPad {
                    status: status_from_reading(product, &reading, serial),
                });
            }
            Err(err) => {
                timing.io_ms += io_started.elapsed().as_millis();
                crate::controller::hid::diag::trace_read(
                    "poll",
                    &serial,
                    crate::controller::hid::diag::bus_tag(info.bus_type()),
                    io_started.elapsed().as_millis(),
                    &format!("fail=battery err={err}"),
                    false,
                );
                app_log::warn(format!(
                    "failed to read {product} (hid serial {hid}): {err}"
                ));
            }
        }
    }

    let mut pads = dedupe_polled(pads);
    pads.sort_by(|a, b| a.status.serial.cmp(&b.status.serial));
    for (i, pad) in pads.iter_mut().enumerate() {
        pad.status.index = i + 1;
    }

    lightbar::sync_lightbar_claims(
        pads.iter()
            .filter(|p| p.status.supports_lightbar)
            .map(|p| p.status.serial.as_str()),
    );

    if lightbar::is_enabled() {
        let previous: std::collections::HashSet<String> = previously_connected
            .iter()
            .map(|s| dualsense::normalize_identity(s))
            .collect();

        for pad in &pads {
            if !pad.status.supports_lightbar {
                continue;
            }
            let serial_key = dualsense::normalize_identity(&pad.status.serial);
            if !previous.contains(&serial_key) {
                lightbar::prepare_connect_apply(&pad.status.serial);
            }
            let color = color_for_battery_percent(pad.status.percent);
            // Fresh open for lightbar (do not reuse the battery-read handle).
            let (result, t) = lightbar::apply_lightbar_rgb_timed(api, &pad.status.serial, color);
            timing.add_assign(t);
            if let Err(err) = result {
                lightbar::warn_lightbar(&pad.status.product, err);
            }
        }
    }

    Ok(pads.into_iter().map(|p| p.status).collect())
}
