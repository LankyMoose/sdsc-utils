//! Poll DualSense battery status and reassert lightbar under one HID lock.
//!
//! Also owns the presence / battery / liveness cadence used by the iced daemon.

use crate::app_log;
use crate::battery::{self, ControllerStatus};
use crate::color::color_for_battery_percent;
use crate::dualsense::{
    self, hid_serial, is_dualsense_gamepad, is_storable_serial, product_name,
    resolve_device_identity,
};
use crate::lightbar::{self, HidPhaseTiming};
use hidapi::{BusType, DeviceInfo, HidApi, HidDevice};
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

/// Opened DualSense after a successful battery read (kept for lightbar write).
struct PolledPad {
    status: ControllerStatus,
    device: HidDevice,
    is_bluetooth: bool,
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
            let pad_is_usb = pad.status.connection == "USB";
            let existing_is_usb = existing.status.connection == "USB";
            if pad_is_usb && !existing_is_usb {
                *existing = pad;
            }
        } else {
            unique.push(pad);
        }
    }

    unique
}

/// Poll DualSense battery and reassert lightbar RGB on each connected pad.
///
/// `previously_connected` is the serial set from the last successful UI snapshot.
/// Serials not in that set are treated as fresh connects: claim is cleared so
/// `LIGHT_OUT` + RGB always run (needed after a presence-only disconnect that
/// never synced claims).
pub fn poll_controllers(previously_connected: &[String]) -> Result<Vec<ControllerStatus>, String> {
    dualsense::with_hid_lock(|| poll_controllers_unlocked(previously_connected))
}

/// Daemon worker entry: no outer lock (worker is exclusive).
pub fn poll_controllers_timed(
    previously_connected: &[String],
) -> (Result<Vec<ControllerStatus>, String>, HidPhaseTiming) {
    let started = Instant::now();
    let mut timing = HidPhaseTiming::default();
    let result = poll_controllers_unlocked_timed(previously_connected, &mut timing);
    // If unlocked path did not fill enumerate (empty list early exit), still record total as io-ish.
    let _ = started;
    (result, timing)
}

fn poll_controllers_unlocked(
    previously_connected: &[String],
) -> Result<Vec<ControllerStatus>, String> {
    let mut timing = HidPhaseTiming::default();
    poll_controllers_unlocked_timed(previously_connected, &mut timing)
}

fn poll_controllers_unlocked_timed(
    previously_connected: &[String],
    timing: &mut HidPhaseTiming,
) -> Result<Vec<ControllerStatus>, String> {
    let enum_started = Instant::now();
    let api = HidApi::new().map_err(|e| e.to_string())?;
    let devices: Vec<&DeviceInfo> = api
        .device_list()
        .filter(|d| is_dualsense_gamepad(d))
        .collect();
    timing.enumerate_ms += enum_started.elapsed().as_millis();

    if devices.is_empty() {
        lightbar::sync_lightbar_claims(std::iter::empty::<&str>());
        return Ok(Vec::new());
    }

    let mut pads = Vec::with_capacity(devices.len());

    for info in devices {
        let product = product_name(info.product_id());
        let hid = hid_serial(info);
        let is_bluetooth = matches!(info.bus_type(), BusType::Bluetooth);

        let open_started = Instant::now();
        match info.open_device(&api).and_then(|device| {
            timing.open_ms += open_started.elapsed().as_millis();
            let io_started = Instant::now();
            let serial = resolve_device_identity(info, &device);
            let battery = battery::read_battery(&device)?;
            timing.io_ms += io_started.elapsed().as_millis();
            Ok((device, serial, battery))
        }) {
            Ok((device, serial, battery)) => pads.push(PolledPad {
                status: ControllerStatus {
                    index: 0,
                    product,
                    connection: battery.connection,
                    serial,
                    percent: battery.percent,
                    state: battery.state,
                },
                device,
                is_bluetooth,
            }),
            Err(err) => {
                timing.open_ms += open_started.elapsed().as_millis();
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

    lightbar::sync_lightbar_claims(pads.iter().map(|p| p.status.serial.as_str()));

    if lightbar::is_enabled() {
        let previous: std::collections::HashSet<String> = previously_connected
            .iter()
            .map(|s| dualsense::normalize_identity(s))
            .collect();

        for pad in &pads {
            let serial_key = dualsense::normalize_identity(&pad.status.serial);
            if !previous.contains(&serial_key) {
                lightbar::prepare_connect_apply(&pad.status.serial);
            }
            let color = color_for_battery_percent(pad.status.percent);
            let io_started = Instant::now();
            if let Err(err) = lightbar::apply_on_open_device(
                &pad.device,
                &pad.status.serial,
                color,
                pad.is_bluetooth,
            ) {
                timing.io_ms += io_started.elapsed().as_millis();
                // Reopen clears stuck Windows overlapped I/O after a write timeout.
                let (retry, t) = lightbar::apply_lightbar_rgb_timed(&pad.status.serial, color);
                timing.add_assign(t);
                if let Err(retry_err) = retry {
                    lightbar::warn_lightbar(
                        pad.status.product,
                        format!("{err}; retry: {retry_err}"),
                    );
                }
            } else {
                timing.io_ms += io_started.elapsed().as_millis();
            }
        }
    }

    Ok(pads.into_iter().map(|p| p.status).collect())
}
