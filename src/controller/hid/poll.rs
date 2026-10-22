//! Poll controller battery status and reassert lightbar under one HID lock.
//!
//! Also owns the presence / battery / liveness cadence used by the iced daemon.

use crate::AppResult;
use crate::controller::driver;
use crate::controller::dualsense::battery;
use crate::controller::dualsense::identity::{self as dualsense, HidIoLock, hid_serial};
use crate::controller::dualsense::lightbar::{self, ClaimStore, HidPhaseTiming};
use crate::controller::hid::launch_paths::LaunchPaths;
use crate::controller::hid::policy::Policy;
use crate::controller::model::ControllerStatus;
use crate::domain::color::SpectrumService;
use crate::platform::app_log;
use hidapi::{DeviceInfo, HidApi};
use std::time::{Duration, Instant};

/// How often to scan for DualSense connect/disconnect when pads are already known.
pub const PRESENCE_INTERVAL: Duration = Policy::DEFAULT.presence;
/// Slower presence backstop while the tray is empty. Arrivals wake the app via
/// the OS device watcher (`PresenceHint`), so this only bounds the blind
/// window on platforms without a watcher (macOS) — 2s keeps first-connect
/// snappy without the old 500ms wakeups.
pub const PRESENCE_INTERVAL_EMPTY: Duration = Policy::DEFAULT.presence_empty;
/// How often to re-read battery / lightbar when membership is stable and pads are readable.
pub const BATTERY_INTERVAL: Duration = Policy::DEFAULT.battery;
/// While the tray shows connected pads, re-probe often so a powered-off BT pad
/// (still lingering in the HID list) is dropped quickly — and so lightbar RGB is
/// reasserted if another HID writer overwrote the bar.
pub const LIVENESS_INTERVAL: Duration = Policy::DEFAULT.liveness;
/// When HID lists pads but battery reads keep failing, retry sooner than BATTERY_INTERVAL.
pub const UNREAD_RETRY_INTERVAL: Duration = Policy::DEFAULT.unread_retry;
/// Hot-sample battery newer than this skips the blocking read on liveness ticks.
/// Sampling runs while input is live (hot); when cold-idle the worker blocks
/// and stable pads cost zero HID reads until the next poll. Blocking reads are
/// reserved for new serials and stale pads.
pub const POLL_LIVE_BATTERY_STALE: Duration = Policy::DEFAULT.poll_live_battery_stale;

/// Opened pad after a successful battery read (device dropped before lightbar).
struct PolledPad {
    status: ControllerStatus,
}

/// Collapse USB+BT of the same pad, keeping the preferred open handle (USB wins).
///
/// Delegates to [`battery::dedupe_statuses`] (single USB-wins implementation);
/// this is just the `PolledPad` newtype adapter.
fn dedupe_polled(pads: Vec<PolledPad>) -> Vec<PolledPad> {
    let statuses: Vec<ControllerStatus> = pads.into_iter().map(|p| p.status).collect();
    battery::dedupe_statuses(statuses)
        .into_iter()
        .map(|status| PolledPad { status })
        .collect()
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
///
/// Takes the caller's `&HidIoLock` (CLI one-shots use a local
/// `HidIoLock::new()`) and `&Policy` (`&Policy::default()` reproduces the
/// legacy const cadence exactly).
#[allow(clippy::too_many_arguments)]
pub fn poll_controllers(
    lock: &HidIoLock,
    policy: &Policy,
    previously_connected: &[String],
    spectrum: &SpectrumService,
    claims: &ClaimStore,
) -> AppResult<Vec<ControllerStatus>> {
    lock.with_lock(|| {
        let api = HidApi::new().map_err(|e| e.to_string())?;
        let mut timing = HidPhaseTiming::default();
        poll_controllers_with_api(
            &api,
            lock,
            policy,
            spectrum,
            claims,
            previously_connected,
            &mut timing,
            None,
            None,
        )
    })
}

/// Daemon worker entry: uses caller's `HidApi`. The worker thread is
/// exclusive, so `lock` is uncontended here; it is still threaded through so
/// CLI and daemon share one `poll_controllers_with_api` seam with no global.
///
/// When `launch` / `launch_quiet` are set, serials for paths present at first enum
/// are appended to `launch_quiet` on a successful battery read (no Connected toast).
#[allow(clippy::too_many_arguments)]
pub fn poll_controllers_timed(
    api: &HidApi,
    lock: &HidIoLock,
    policy: &Policy,
    spectrum: &SpectrumService,
    claims: &ClaimStore,
    previously_connected: &[String],
    launch: Option<&mut LaunchPaths>,
    launch_quiet: Option<&mut Vec<String>>,
) -> (AppResult<Vec<ControllerStatus>>, HidPhaseTiming) {
    let mut timing = HidPhaseTiming::default();
    let result = lock.with_lock(|| {
        poll_controllers_with_api(
            api,
            lock,
            policy,
            spectrum,
            claims,
            previously_connected,
            &mut timing,
            launch,
            launch_quiet,
        )
    });
    (result, timing)
}

#[allow(clippy::too_many_arguments)]
fn poll_controllers_with_api(
    api: &HidApi,
    _lock: &HidIoLock,
    policy: &Policy,
    spectrum: &SpectrumService,
    claims: &ClaimStore,
    previously_connected: &[String],
    timing: &mut HidPhaseTiming,
    mut launch: Option<&mut LaunchPaths>,
    mut launch_quiet: Option<&mut Vec<String>>,
) -> AppResult<Vec<ControllerStatus>> {
    // Policy seam: `Policy::default()` reproduces the legacy const cadence
    // exactly; this reference keeps the seam without changing timing.
    let _ = policy;
    let enum_started = Instant::now();
    let devices: Vec<&DeviceInfo> = api
        .device_list()
        .filter(|d| driver::is_gamepad(d))
        .collect();
    timing.enumerate_ms += enum_started.elapsed().as_millis();

    if devices.is_empty() {
        claims.sync(std::iter::empty::<&str>());
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
                    && let Some(out) = launch_quiet.as_mut()
                {
                    out.push(quiet);
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

    reassert_lightbar_for_pads(
        api,
        spectrum,
        claims,
        previously_connected,
        pads.iter().map(|p| &p.status),
        timing,
    );

    Ok(pads.into_iter().map(|p| p.status).collect())
}

/// Reassert lightbar only where due (new pad, color change, slow backstop).
/// Shared by full and incremental polls so CLI and daemon behave alike.
pub(crate) fn reassert_lightbar_for_pads<'a>(
    api: &HidApi,
    spectrum: &SpectrumService,
    claims: &ClaimStore,
    previously_connected: &[String],
    statuses: impl Iterator<Item = &'a ControllerStatus>,
    timing: &mut HidPhaseTiming,
) {
    let statuses: Vec<&'a ControllerStatus> = statuses.collect();
    claims.sync(
        statuses
            .iter()
            .filter(|p| p.supports_lightbar)
            .map(|p| p.serial.as_ref()),
    );
    if !claims.is_enabled() {
        return;
    }
    let previous: std::collections::HashSet<String> = previously_connected
        .iter()
        .map(|s| dualsense::normalize_identity(s))
        .collect();

    for pad in statuses {
        if !pad.supports_lightbar {
            continue;
        }
        let serial_key = dualsense::normalize_identity(pad.serial.as_ref());
        let color = spectrum.color_at(pad.percent);
        if !previous.contains(&serial_key) {
            claims.prepare_connect(pad.serial.as_ref());
        } else if !claims.reassert_due(pad.serial.as_ref(), color) {
            // Stable pad, recently applied: skip the open-write-close so the
            // 5s liveness tick stops churning every bar (color changes, new
            // pads, and the 30s backstop still write).
            continue;
        }
        // Fresh open for lightbar (do not reuse the battery-read handle).
        let (result, t) =
            lightbar::apply_lightbar_rgb_timed(api, pad.serial.as_ref(), color, claims);
        timing.add_assign(t);
        if let Err(err) = result {
            lightbar::warn_lightbar(&pad.product, err);
        } else {
            claims.note_applied(pad.serial.as_ref(), color);
        }
    }
}
