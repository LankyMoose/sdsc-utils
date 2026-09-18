//! DualSense lightbar HID output (serialized DualSense HID I/O).

use crate::app_log;
use crate::battery::{is_dualsense_gamepad, normalize_identity, resolve_device_identity};
use crate::color::{Rgb, color_for_battery_percent};
use crate::steam;
use hidapi::{BusType, HidApi, HidDevice};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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
/// Matches [`identify_controller`]: white for [`IDENTIFY_FLASH_MS`], then battery
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

/// Serializes all DualSense HID open/read/write/close (battery poll + lightbar).
static HID_IO_LOCK: Mutex<()> = Mutex::new(());
static BT_OUTPUT_SEQ: AtomicU8 = AtomicU8::new(0);
/// When false, automatic battery/poll/pulse RGB writes are skipped (Identify / CLI still work).
static AUTOMATIC_ENABLED: AtomicBool = AtomicBool::new(true);
/// Serials that have already received a `LIGHT_OUT` claim this connection.
static CLAIMED_SERIALS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
/// Last observed Steam running state; forget claims when it changes.
static LAST_STEAM_RUNNING: LazyLock<Mutex<Option<bool>>> = LazyLock::new(|| Mutex::new(None));
/// Last successfully written RGB per pad (skip redundant poll rewrites).
static LAST_APPLIED: LazyLock<Mutex<HashMap<String, Rgb>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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

/// Drop claims (and last-applied colors) for pads that are no longer present.
pub fn sync_lightbar_claims(active_serials: impl IntoIterator<Item = impl AsRef<str>>) {
    let active: HashSet<String> = active_serials
        .into_iter()
        .map(|s| normalize_identity(s.as_ref()))
        .collect();
    if let Ok(mut claimed) = CLAIMED_SERIALS.lock() {
        claimed.retain(|s| active.contains(s));
    }
    if let Ok(mut last) = LAST_APPLIED.lock() {
        last.retain(|s, _| active.contains(s));
    }
}

/// Drop all `LIGHT_OUT` claims (e.g. Steam started/stopped).
fn forget_all_claims() {
    if let Ok(mut claimed) = CLAIMED_SERIALS.lock() {
        claimed.clear();
    }
    if let Ok(mut last) = LAST_APPLIED.lock() {
        last.clear();
    }
}

/// Forget claims when Steam starts or stops so the next write picks the right path.
fn sync_steam_transition() {
    let running = steam::is_running();
    let mut last = LAST_STEAM_RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    match *last {
        None => *last = Some(running),
        Some(prev) if prev != running => {
            forget_all_claims();
            *last = Some(running);
        }
        Some(_) => {}
    }
}

fn take_claim_if_needed(serial: &str) -> bool {
    sync_steam_transition();
    if steam::is_running() {
        // Steam Input already initialized the lightbar; RGB-only writes work.
        return false;
    }

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

fn record_last_applied(serial: &str, color: Rgb) {
    if let Ok(mut last) = LAST_APPLIED.lock() {
        last.insert(normalize_identity(serial), color);
    }
}

fn last_applied_matches(serial: &str, color: Rgb) -> bool {
    LAST_APPLIED
        .lock()
        .ok()
        .and_then(|last| last.get(&normalize_identity(serial)).copied())
        .is_some_and(|prev| prev == color)
}

/// Whether a poll/spectrum write can be skipped (same RGB, no pending claim).
pub(crate) fn should_skip_rgb_write(serial: &str, color: Rgb, claim: bool, force: bool) -> bool {
    !force && !claim && last_applied_matches(serial, color)
}

fn is_retryable_write_error(err: &impl std::fmt::Display) -> bool {
    let text = err.to_string();
    text.contains("0x000003E5")
        || text.contains("WaitForSingleObject")
        || text.contains("Overlapped I/O")
}

/// Whether a debounced spectrum commit generation is still current.
pub fn spectrum_commit_is_current(current: u64, commit: u64) -> bool {
    current == commit
}

/// Apply a lightbar color to the controller with the given serial.
pub fn apply_lightbar_rgb(serial: &str, color: Rgb) -> Result<(), String> {
    let _guard = HID_IO_LOCK
        .lock()
        .map_err(|_| "lightbar lock poisoned".to_string())?;
    apply_lightbar_rgb_unlocked(serial, color, true)
}

/// Apply lightbar while already holding [`HID_IO_LOCK`] (poll / identify).
pub(crate) fn apply_lightbar_rgb_unlocked(
    serial: &str,
    color: Rgb,
    force: bool,
) -> Result<(), String> {
    match try_apply_by_serial(serial, color, force) {
        Ok(()) => Ok(()),
        Err(err) if is_retryable_write_error(&err) => {
            thread::sleep(WRITE_RETRY_DELAY);
            try_apply_by_serial(serial, color, force)
        }
        Err(err) => Err(err),
    }
}

fn try_apply_by_serial(serial: &str, color: Rgb, force: bool) -> Result<(), String> {
    let api = HidApi::new().map_err(|e| e.to_string())?;
    let target = normalize_identity(serial);

    // Prefer USB when the same pad appears on both buses.
    let mut best: Option<(HidDevice, bool, bool)> = None; // device, is_bluetooth, is_usb
    for info in api.device_list().filter(|d| is_dualsense_gamepad(d)) {
        let device = match info.open_device(&api) {
            Ok(d) => d,
            Err(_) => continue,
        };
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

    let (device, is_bluetooth, _) = best.ok_or_else(|| format!("controller {serial} not found"))?;
    apply_on_open_device(&device, &target, color, is_bluetooth, force)
}

/// Write lightbar on an already-open handle (caller must hold [`HID_IO_LOCK`]).
pub fn apply_on_open_device(
    device: &HidDevice,
    serial: &str,
    color: Rgb,
    is_bluetooth: bool,
    force: bool,
) -> Result<(), String> {
    let target = normalize_identity(serial);
    let claim = take_claim_if_needed(&target);
    if should_skip_rgb_write(&target, color, claim, force) {
        return Ok(());
    }

    match set_lightbar_on_device(device, color, is_bluetooth, claim) {
        Ok(()) => {
            record_last_applied(&target, color);
            Ok(())
        }
        Err(err) => {
            // Claim may have partially completed; forget so the next attempt reclaims.
            // Do not RGB-write after a failed claim/write on this handle — caller should
            // reopen (hid_close cancels stuck overlapped I/O on Windows).
            if claim {
                forget_claim(&target);
            }
            Err(err.to_string())
        }
    }
}

/// Write lightbar while already holding [`HID_IO_LOCK`] (used during poll).
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

/// Hold the DualSense HID I/O lock for a closure (poll / power-off path).
pub fn with_lightbar_lock<T>(f: impl FnOnce() -> T) -> T {
    let _guard = HID_IO_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    f()
}

/// Flash white, then restore battery color, five times (lock held for the sequence).
pub fn identify_controller(serial: &str, percent: u8) -> Result<(), String> {
    let normal = color_for_battery_percent(percent);
    let flash_for = Duration::from_millis(IDENTIFY_FLASH_MS);
    let _guard = HID_IO_LOCK
        .lock()
        .map_err(|_| "lightbar lock poisoned".to_string())?;

    for _ in 0..IDENTIFY_FLASH_COUNT {
        apply_lightbar_rgb_unlocked(serial, Rgb::WHITE, true)?;
        thread::sleep(flash_for);
        apply_lightbar_rgb_unlocked(serial, normal, true)?;
        thread::sleep(flash_for);
    }

    Ok(())
}

/// Apply a color to every connected DualSense (CLI / debug).
/// Re-claims with `LIGHT_OUT` when Steam is not running.
pub fn apply_lightbar_all(color: Rgb) -> Result<usize, String> {
    let api = HidApi::new().map_err(|e| e.to_string())?;
    let mut applied = 0usize;
    let _guard = HID_IO_LOCK
        .lock()
        .map_err(|_| "lightbar lock poisoned".to_string())?;

    if let Ok(mut claimed) = CLAIMED_SERIALS.lock() {
        claimed.clear();
    }
    if let Ok(mut last) = LAST_APPLIED.lock() {
        last.clear();
    }

    for info in api.device_list().filter(|d| is_dualsense_gamepad(d)) {
        let device = match info.open_device(&api) {
            Ok(d) => d,
            Err(err) => {
                app_log::warn(format!("lightbar open failed: {err}"));
                continue;
            }
        };
        let serial = resolve_device_identity(info, &device);
        let is_bluetooth = matches!(info.bus_type(), BusType::Bluetooth);
        match apply_on_open_device(&device, &serial, color, is_bluetooth, true) {
            Ok(()) => applied += 1,
            Err(err) => {
                app_log::warn(format!("lightbar write failed: {err}"));
            }
        }
    }

    Ok(applied)
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

    fn reset_claim_test_state(steam_running: bool) {
        steam::set_running_for_test(Some(steam_running));
        forget_all_claims();
        *LAST_STEAM_RUNNING.lock().unwrap() = None;
    }

    fn clear_claim_test_state() {
        steam::set_running_for_test(None);
        forget_all_claims();
        *LAST_STEAM_RUNNING.lock().unwrap() = None;
    }

    #[test]
    fn claim_tracking_and_steam_behavior() {
        // Single test so the shared Steam override is not raced by parallel runners.
        reset_claim_test_state(false);

        sync_lightbar_claims(std::iter::empty::<&str>());
        assert!(take_claim_if_needed("aabbcc"));
        assert!(!take_claim_if_needed("aabbcc"));
        assert!(take_claim_if_needed("ddeeff"));
        sync_lightbar_claims(["ddeeff"]);
        assert!(!take_claim_if_needed("ddeeff"));
        assert!(take_claim_if_needed("aabbcc"));
        sync_lightbar_claims(std::iter::empty::<&str>());

        reset_claim_test_state(false);
        assert!(take_claim_if_needed("aabbcc"));
        assert!(!take_claim_if_needed("aabbcc"));

        reset_claim_test_state(true);
        assert!(!take_claim_if_needed("aabbcc"));
        assert!(!take_claim_if_needed("ddeeff"));

        reset_claim_test_state(false);
        assert!(take_claim_if_needed("aabbcc"));

        reset_claim_test_state(false);
        assert!(take_claim_if_needed("aabbcc"));
        assert!(!take_claim_if_needed("aabbcc"));

        steam::set_running_for_test(Some(true));
        *LAST_STEAM_RUNNING.lock().unwrap() = Some(false);
        assert!(!take_claim_if_needed("aabbcc"));

        steam::set_running_for_test(Some(false));
        *LAST_STEAM_RUNNING.lock().unwrap() = Some(true);
        assert!(take_claim_if_needed("aabbcc"));

        // Last-applied skip (same shared claim/last maps — keep in this serial test).
        forget_all_claims();
        let serial = "aabbccddeeff";
        let color = Rgb::new(10, 20, 30);
        let other = Rgb::new(40, 50, 60);
        assert!(!should_skip_rgb_write(serial, color, false, false));
        record_last_applied(serial, color);
        assert!(should_skip_rgb_write(serial, color, false, false));
        assert!(!should_skip_rgb_write(serial, other, false, false));
        assert!(!should_skip_rgb_write(serial, color, true, false));
        assert!(!should_skip_rgb_write(serial, color, false, true));
        sync_lightbar_claims(std::iter::empty::<&str>());
        assert!(!should_skip_rgb_write(serial, color, false, false));

        clear_claim_test_state();
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
    fn spectrum_commit_generation_drops_stale() {
        assert!(spectrum_commit_is_current(3, 3));
        assert!(!spectrum_commit_is_current(4, 3));
        assert!(!spectrum_commit_is_current(2, 3));
    }

    #[test]
    fn retryable_write_error_detects_io_pending() {
        assert!(is_retryable_write_error(
            &"hidapi error: hid_write/WaitForSingleObject: (0x000003E5) Overlapped I/O operation is in progress."
        ));
        assert!(!is_retryable_write_error(&"controller not found"));
    }
}
