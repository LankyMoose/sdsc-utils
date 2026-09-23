//! DualSense lightbar HID output.

use crate::app_log;
use crate::color::Rgb;
use crate::dualsense::{self, is_dualsense_gamepad, normalize_identity, resolve_device_identity};
use hidapi::{BusType, HidApi, HidDevice};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Phase timings for hid-worker metrics (enumerate / open / io).
#[derive(Debug, Default, Clone, Copy)]
pub struct HidPhaseTiming {
    pub enumerate_ms: u128,
    pub open_ms: u128,
    pub io_ms: u128,
}

impl HidPhaseTiming {
    pub fn add_assign(&mut self, other: Self) {
        self.enumerate_ms += other.enumerate_ms;
        self.open_ms += other.open_ms;
        self.io_ms += other.io_ms;
    }
}

const OUTPUT_REPORT_USB_ID: u8 = 0x02;
const OUTPUT_REPORT_USB_SIZE: usize = 63;
const OUTPUT_REPORT_BT_ID: u8 = 0x31;
const OUTPUT_REPORT_BT_SIZE: usize = 78;
const OUTPUT_REPORT_BT_TAG: u8 = 0x10;
const OUTPUT_CRC32_SEED: u8 = 0xA2;

/// Offsets into the DualSense common output payload (after report ID / BT header).
const OFF_VALID_FLAG1: usize = 1;
const OFF_VALID_FLAG2: usize = 38;
const OFF_LIGHTBAR_SETUP: usize = 41;
const OFF_LIGHTBAR_R: usize = 44;
const OFF_LIGHTBAR_G: usize = 45;
const OFF_LIGHTBAR_B: usize = 46;

const OUTPUT_VALID_FLAG1_LIGHTBAR: u8 = 1 << 2;
const OUTPUT_VALID_FLAG2_LIGHTBAR_SETUP: u8 = 1 << 1;
/// Reconfigure so RGB programming is accepted (Linux hid-playstation Bluetooth init).
/// Must be a **separate** report from the RGB write.
const OUTPUT_LIGHTBAR_SETUP_LIGHT_OUT: u8 = 1 << 1;

const WRITE_RETRY_DELAY: Duration = Duration::from_millis(75);

pub const IDENTIFY_FLASH_MS: u64 = 150;
pub const IDENTIFY_FLASH_COUNT: u32 = 5;

/// Whether the identify sequence should show white at `now`.
///
/// Matches Identify flash timing: white for [`IDENTIFY_FLASH_MS`], then battery
/// color for the same duration, repeated [`IDENTIFY_FLASH_COUNT`] times.
/// Returns `None` when the sequence is finished.
pub fn identify_flash_is_white(started: Instant, now: Instant) -> Option<bool> {
    let elapsed_ms = now.saturating_duration_since(started).as_millis() as u64;
    let step = elapsed_ms / IDENTIFY_FLASH_MS;
    if step >= u64::from(IDENTIFY_FLASH_COUNT) * 2 {
        None
    } else {
        Some(step.is_multiple_of(2))
    }
}
pub const LOW_BATTERY_PULSE_ON_MS: u64 = 400;
pub const LOW_BATTERY_PULSE_GAP_MS: u64 = 1600;
pub const LOW_BATTERY_ORANGE: Rgb = Rgb::ORANGE;

/// Serializes DualSense lightbar claim tracking (I/O lock lives in [`crate::dualsense`]).
static BT_OUTPUT_SEQ: AtomicU8 = AtomicU8::new(0);
/// When false, automatic battery/poll/pulse RGB writes are skipped (Identify / CLI still work).
static AUTOMATIC_ENABLED: AtomicBool = AtomicBool::new(true);
/// Serials that have already received a `LIGHT_OUT` claim this connection.
static CLAIMED_SERIALS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Enable or disable automatic lightbar RGB (poll + low-battery pulse).
pub fn set_enabled(enabled: bool) {
    AUTOMATIC_ENABLED.store(enabled, Ordering::SeqCst);
}

/// Whether automatic lightbar RGB writes should run.
pub fn is_enabled() -> bool {
    AUTOMATIC_ENABLED.load(Ordering::SeqCst)
}

fn next_bt_seq_tag() -> u8 {
    let seq = BT_OUTPUT_SEQ.fetch_add(1, Ordering::Relaxed) & 0x0F;
    seq << 4
}

/// Drop claims for pads that are no longer present.
pub fn sync_lightbar_claims(active_serials: impl IntoIterator<Item = impl AsRef<str>>) {
    let active: HashSet<String> = active_serials
        .into_iter()
        .map(|s| normalize_identity(s.as_ref()))
        .collect();
    if let Ok(mut claimed) = CLAIMED_SERIALS.lock() {
        claimed.retain(|s| active.contains(s));
    }
}

/// Drop all `LIGHT_OUT` claims (e.g. CLI force-reapply, hid-worker session start).
pub fn forget_all_claims() {
    if let Ok(mut claimed) = CLAIMED_SERIALS.lock() {
        claimed.clear();
    }
}

fn take_claim_if_needed(serial: &str) -> bool {
    let Ok(mut claimed) = CLAIMED_SERIALS.lock() else {
        return true;
    };
    claimed.insert(normalize_identity(serial))
}

fn forget_claim(serial: &str) {
    if let Ok(mut claimed) = CLAIMED_SERIALS.lock() {
        claimed.remove(&normalize_identity(serial));
    }
}

/// Drop the claim for `serial` so the next apply runs `LIGHT_OUT` then RGB.
///
/// Used when a pad newly appears in the live set: presence can clear the tray
/// without a HID poll, which would otherwise leave a stale claim and skip
/// reclaim on Bluetooth reconnect.
pub fn prepare_connect_apply(serial: &str) {
    forget_claim(serial);
}

fn is_retryable_write_error(err: &impl std::fmt::Display) -> bool {
    let text = err.to_string();
    text.contains("0x000003E5")
        || text.contains("WaitForSingleObject")
        || text.contains("Overlapped I/O")
}

/// Daemon worker entry: no outer lock (worker is exclusive).
pub fn apply_lightbar_rgb_timed(serial: &str, color: Rgb) -> (Result<(), String>, HidPhaseTiming) {
    let (first, mut timing) = try_apply_by_serial_timed(serial, color);
    match first {
        Ok(()) => (Ok(()), timing),
        Err(err) if is_retryable_write_error(&err) => {
            thread::sleep(WRITE_RETRY_DELAY);
            let (retry, t2) = try_apply_by_serial_timed(serial, color);
            timing.add_assign(t2);
            (retry, timing)
        }
        Err(err) => (Err(err), timing),
    }
}

fn try_apply_by_serial_timed(serial: &str, color: Rgb) -> (Result<(), String>, HidPhaseTiming) {
    let mut timing = HidPhaseTiming::default();
    let enum_started = Instant::now();
    let api = match HidApi::new() {
        Ok(api) => api,
        Err(e) => {
            timing.enumerate_ms = enum_started.elapsed().as_millis();
            return (Err(e.to_string()), timing);
        }
    };
    let target = normalize_identity(serial);

    // Prefer USB when the same pad appears on both buses.
    let mut best: Option<(HidDevice, bool, bool)> = None; // device, is_bluetooth, is_usb
    let mut open_ms = 0u128;
    for info in api.device_list().filter(|d| is_dualsense_gamepad(d)) {
        let open_started = Instant::now();
        let device = match info.open_device(&api) {
            Ok(d) => d,
            Err(_) => {
                open_ms += open_started.elapsed().as_millis();
                continue;
            }
        };
        open_ms += open_started.elapsed().as_millis();
        let identity = resolve_device_identity(info, &device);
        if identity != target {
            continue;
        }
        let is_bluetooth = matches!(info.bus_type(), BusType::Bluetooth);
        let is_usb = matches!(info.bus_type(), BusType::Usb);
        let replace = match &best {
            None => true,
            Some((_, _, prev_is_usb)) => is_usb && !*prev_is_usb,
        };
        if replace {
            best = Some((device, is_bluetooth, is_usb));
        }
    }
    timing.enumerate_ms = enum_started.elapsed().as_millis().saturating_sub(open_ms);
    timing.open_ms = open_ms;

    let Some((device, is_bluetooth, _)) = best else {
        return (Err(format!("controller {serial} not found")), timing);
    };
    let io_started = Instant::now();
    let result = apply_on_open_device(&device, &target, color, is_bluetooth);
    timing.io_ms = io_started.elapsed().as_millis();
    (result, timing)
}

/// Write lightbar on an already-open handle (caller must hold the HID I/O lock).
pub fn apply_on_open_device(
    device: &HidDevice,
    serial: &str,
    color: Rgb,
    is_bluetooth: bool,
) -> Result<(), String> {
    apply_on_open_device_timed(device, serial, color, is_bluetooth).0
}

/// Timed write on an already-open handle (hid-worker Identify cache — no enumerate/open).
pub fn apply_on_open_device_timed(
    device: &HidDevice,
    serial: &str,
    color: Rgb,
    is_bluetooth: bool,
) -> (Result<(), String>, HidPhaseTiming) {
    let mut timing = HidPhaseTiming::default();
    let target = normalize_identity(serial);
    let claim = take_claim_if_needed(&target);
    let io_started = Instant::now();
    let result = match set_lightbar_on_device(device, color, is_bluetooth, claim) {
        Ok(()) => Ok(()),
        Err(err) => {
            // Claim may have partially completed; forget so the next attempt reclaims.
            // Do not RGB-write after a failed claim/write on this handle — caller should
            // reopen (hid_close cancels stuck overlapped I/O on Windows).
            if claim {
                forget_claim(&target);
            }
            Err(err.to_string())
        }
    };
    timing.io_ms = io_started.elapsed().as_millis();
    (result, timing)
}

/// Write lightbar while already holding the HID I/O lock (used during poll).
///
/// Bluetooth DualSense ignores RGB until the lightbar is reconfigured with a
/// dedicated `LIGHT_OUT` setup report (same as Linux `hid-playstation`). Color is
/// then applied in a second report with only the lightbar RGB flag.
///
/// If the claim write fails, RGB is **not** attempted on the same handle.
pub fn set_lightbar_on_device(
    device: &HidDevice,
    color: Rgb,
    is_bluetooth: bool,
    claim: bool,
) -> Result<(), hidapi::HidError> {
    if claim {
        write_lightbar_claim(device, is_bluetooth)?;
    }
    write_lightbar_rgb(device, is_bluetooth, color)
}

fn write_lightbar_claim(device: &HidDevice, is_bluetooth: bool) -> Result<(), hidapi::HidError> {
    write_output_report(device, is_bluetooth, |common| {
        common[OFF_VALID_FLAG2] = OUTPUT_VALID_FLAG2_LIGHTBAR_SETUP;
        common[OFF_LIGHTBAR_SETUP] = OUTPUT_LIGHTBAR_SETUP_LIGHT_OUT;
    })
}

fn write_lightbar_rgb(
    device: &HidDevice,
    is_bluetooth: bool,
    color: Rgb,
) -> Result<(), hidapi::HidError> {
    write_output_report(device, is_bluetooth, |common| {
        common[OFF_VALID_FLAG1] = OUTPUT_VALID_FLAG1_LIGHTBAR;
        common[OFF_LIGHTBAR_R] = color.r;
        common[OFF_LIGHTBAR_G] = color.g;
        common[OFF_LIGHTBAR_B] = color.b;
    })
}

fn write_output_report(
    device: &HidDevice,
    is_bluetooth: bool,
    fill_common: impl FnOnce(&mut [u8]),
) -> Result<(), hidapi::HidError> {
    if is_bluetooth {
        device.write(&build_bt_report(fill_common))?;
    } else {
        let mut report = [0u8; OUTPUT_REPORT_USB_SIZE];
        report[0] = OUTPUT_REPORT_USB_ID;
        fill_common(&mut report[1..]);
        device.write(&report)?;
    }
    Ok(())
}

fn build_bt_report(fill_common: impl FnOnce(&mut [u8])) -> [u8; OUTPUT_REPORT_BT_SIZE] {
    let mut report = [0u8; OUTPUT_REPORT_BT_SIZE];
    report[0] = OUTPUT_REPORT_BT_ID;
    report[1] = next_bt_seq_tag();
    report[2] = OUTPUT_REPORT_BT_TAG;
    fill_common(&mut report[3..]);

    let crc = {
        let mut data = Vec::with_capacity(OUTPUT_REPORT_BT_SIZE - 3);
        data.push(OUTPUT_CRC32_SEED);
        data.extend_from_slice(&report[..OUTPUT_REPORT_BT_SIZE - 4]);
        crc32fast::hash(&data)
    };
    report[OUTPUT_REPORT_BT_SIZE - 4..].copy_from_slice(&crc.to_le_bytes());
    report
}

/// Apply a color to every connected DualSense (CLI / debug).
/// Clears claims so each pad receives a fresh `LIGHT_OUT` then RGB.
pub fn apply_lightbar_all(color: Rgb) -> Result<usize, String> {
    let _guard = dualsense::lock_hid()?;
    apply_lightbar_all_timed(color).0
}

pub fn apply_lightbar_all_timed(color: Rgb) -> (Result<usize, String>, HidPhaseTiming) {
    let mut timing = HidPhaseTiming::default();
    let enum_started = Instant::now();
    let api = match HidApi::new() {
        Ok(api) => api,
        Err(e) => {
            timing.enumerate_ms = enum_started.elapsed().as_millis();
            return (Err(e.to_string()), timing);
        }
    };
    let mut applied = 0usize;

    forget_all_claims();

    let mut open_ms = 0u128;
    let mut io_ms = 0u128;
    for info in api.device_list().filter(|d| is_dualsense_gamepad(d)) {
        let open_started = Instant::now();
        let device = match info.open_device(&api) {
            Ok(d) => d,
            Err(err) => {
                open_ms += open_started.elapsed().as_millis();
                app_log::warn(format!("lightbar open failed: {err}"));
                continue;
            }
        };
        open_ms += open_started.elapsed().as_millis();
        let serial = resolve_device_identity(info, &device);
        let is_bluetooth = matches!(info.bus_type(), BusType::Bluetooth);
        let io_started = Instant::now();
        match apply_on_open_device(&device, &serial, color, is_bluetooth) {
            Ok(()) => applied += 1,
            Err(err) => {
                app_log::warn(format!("lightbar write failed: {err}"));
            }
        }
        io_ms += io_started.elapsed().as_millis();
    }
    timing.enumerate_ms = enum_started
        .elapsed()
        .as_millis()
        .saturating_sub(open_ms + io_ms);
    timing.open_ms = open_ms;
    timing.io_ms = io_ms;

    (Ok(applied), timing)
}

pub fn warn_lightbar(product: &str, err: impl std::fmt::Display) {
    app_log::warn(format!("failed to set lightbar on {product}: {err}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt_claim_report_has_setup_without_rgb() {
        let report = build_bt_report(|common| {
            common[OFF_VALID_FLAG2] = OUTPUT_VALID_FLAG2_LIGHTBAR_SETUP;
            common[OFF_LIGHTBAR_SETUP] = OUTPUT_LIGHTBAR_SETUP_LIGHT_OUT;
        });
        assert_eq!(report.len(), 78);
        assert_eq!(report[0], OUTPUT_REPORT_BT_ID);
        assert_eq!(report[2], OUTPUT_REPORT_BT_TAG);
        assert_eq!(report[3 + OFF_VALID_FLAG1], 0);
        assert_eq!(
            report[3 + OFF_VALID_FLAG2],
            OUTPUT_VALID_FLAG2_LIGHTBAR_SETUP
        );
        assert_eq!(
            report[3 + OFF_LIGHTBAR_SETUP],
            OUTPUT_LIGHTBAR_SETUP_LIGHT_OUT
        );
        assert_eq!(report[3 + OFF_LIGHTBAR_R], 0);
        assert!(report[74..78].iter().any(|&b| b != 0));
    }

    #[test]
    fn bt_rgb_report_sets_color_without_setup() {
        let report = build_bt_report(|common| {
            common[OFF_VALID_FLAG1] = OUTPUT_VALID_FLAG1_LIGHTBAR;
            common[OFF_LIGHTBAR_R] = 255;
            common[OFF_LIGHTBAR_G] = 100;
            common[OFF_LIGHTBAR_B] = 0;
        });
        assert_eq!(report[3 + OFF_VALID_FLAG1], OUTPUT_VALID_FLAG1_LIGHTBAR);
        assert_eq!(report[3 + OFF_VALID_FLAG2], 0);
        assert_eq!(report[3 + OFF_LIGHTBAR_SETUP], 0);
        assert_eq!(report[3 + OFF_LIGHTBAR_R], 255);
        assert_eq!(report[3 + OFF_LIGHTBAR_G], 100);
        assert_eq!(report[3 + OFF_LIGHTBAR_B], 0);
        assert!(report[74..78].iter().any(|&b| b != 0));
    }

    #[test]
    fn claim_once_per_connection() {
        forget_all_claims();

        sync_lightbar_claims(std::iter::empty::<&str>());
        assert!(take_claim_if_needed("aabbcc"));
        assert!(!take_claim_if_needed("aabbcc"));
        assert!(take_claim_if_needed("ddeeff"));
        sync_lightbar_claims(["ddeeff"]);
        assert!(!take_claim_if_needed("ddeeff"));
        assert!(take_claim_if_needed("aabbcc"));
        sync_lightbar_claims(std::iter::empty::<&str>());

        forget_all_claims();
        assert!(take_claim_if_needed("aabbcc"));
        assert!(!take_claim_if_needed("aabbcc"));
        prepare_connect_apply("aabbcc");
        assert!(take_claim_if_needed("aabbcc"));
        forget_all_claims();
    }

    #[test]
    fn identify_flash_matches_lightbar_timing() {
        let start = Instant::now();
        assert_eq!(identify_flash_is_white(start, start), Some(true));
        assert_eq!(
            identify_flash_is_white(start, start + Duration::from_millis(IDENTIFY_FLASH_MS)),
            Some(false)
        );
        assert_eq!(
            identify_flash_is_white(start, start + Duration::from_millis(IDENTIFY_FLASH_MS * 2)),
            Some(true)
        );
        let done_at =
            start + Duration::from_millis(IDENTIFY_FLASH_MS * u64::from(IDENTIFY_FLASH_COUNT) * 2);
        assert_eq!(identify_flash_is_white(start, done_at), None);
    }

    #[test]
    fn retryable_write_error_detects_io_pending() {
        assert!(is_retryable_write_error(
            &"hidapi error: hid_write/WaitForSingleObject: (0x000003E5) Overlapped I/O operation is in progress."
        ));
        assert!(!is_retryable_write_error(&"controller not found"));
    }
}
