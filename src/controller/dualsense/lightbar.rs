//! DualSense lightbar HID output.

use crate::controller::dualsense::identity::{
    self as dualsense, HidIoLock, is_dualsense_device, is_dualsense_gamepad, normalize_identity,
    resolve_device_identity,
};
use crate::domain::color::Rgb;
use crate::domain::protocol::{CALIBRATION_FEATURE_REPORT, CALIBRATION_FEATURE_SIZE};
use crate::platform::app_log;
use hidapi::{BusType, HidApi, HidDevice};
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Steam claim verdict cache.
///
/// Intentional: 3s release claim cache. The debug-only hid-diag Steam scan in
/// `hid::diag` (`STEAM_REFRESH`, 1s) is a separate verbose-logging cache with a
/// shorter TTL — different purpose, different TTL; neither should be "unified".
const STEAM_CLAIM_CACHE_TTL: Duration = Duration::from_secs(3);

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

/// Half-step duration (white OR battery color). Full flash = 2 × this.
pub const IDENTIFY_FLASH_MS: u64 = 125;
/// Full white→color cycles. Total Identify ≈ `COUNT * 2 * FLASH_MS` = 1s.
pub const IDENTIFY_FLASH_COUNT: u32 = 4;
/// Soft off-white for Identify flashes (UI rings + lightbar) — less harsh than pure white.
pub const IDENTIFY_FLASH: Rgb = Rgb::new(210, 212, 220);

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

/// Slow reassert so foreign overwrites (game launchers, etc.) do not stick,
/// without rewriting every pad on every 5s liveness tick.
pub const LIGHTBAR_REASSERT_INTERVAL: Duration = Duration::from_secs(30);

struct SteamClaimCache {
    running: bool,
    refreshed_at: Option<Instant>,
}

/// Owned lightbar-claim unit: claim-once set, last-applied colors, Steam
/// transition tracking, and the BT output sequence.
///
/// Owned by the HID worker (`HidWorkerHandle::claims`) and cloned into the
/// app root for the presence-empty fast path. Single worker today, so
/// per-store sequence matches the old global behavior.
pub struct ClaimStore {
    claimed: Mutex<HashSet<String>>,
    applied: Mutex<HashMap<String, (Rgb, Instant)>>,
    last_steam: Mutex<Option<bool>>,
    steam_cache: Mutex<SteamClaimCache>,
    automatic: AtomicBool,
    bt_seq: AtomicU8,
    #[cfg(test)]
    test_override: Mutex<Option<bool>>,
}

impl ClaimStore {
    pub fn new() -> Self {
        Self {
            claimed: Mutex::new(HashSet::new()),
            applied: Mutex::new(HashMap::new()),
            last_steam: Mutex::new(None),
            steam_cache: Mutex::new(SteamClaimCache {
                running: false,
                refreshed_at: None,
            }),
            automatic: AtomicBool::new(true),
            bt_seq: AtomicU8::new(0),
            #[cfg(test)]
            test_override: Mutex::new(None),
        }
    }

    /// Enable or disable automatic lightbar RGB (poll + low-battery pulse).
    pub fn set_enabled(&self, enabled: bool) {
        self.automatic.store(enabled, Ordering::SeqCst);
    }

    /// Whether automatic lightbar RGB writes should run.
    pub fn is_enabled(&self) -> bool {
        self.automatic.load(Ordering::SeqCst)
    }

    fn next_seq_tag(&self) -> u8 {
        let seq = self.bt_seq.fetch_add(1, Ordering::Relaxed) & 0x0F;
        seq << 4
    }

    /// Whether `steam.exe` is running (cached). Used only for lightbar claim strategy —
    /// must work in release (unlike debug-only hid-diag steam tags).
    pub fn steam_running(&self) -> bool {
        #[cfg(test)]
        if let Ok(guard) = self.test_override.lock()
            && let Some(value) = *guard
        {
            return value;
        }

        let now = Instant::now();
        if let Ok(mut guard) = self.steam_cache.lock() {
            if guard
                .refreshed_at
                .is_some_and(|t| now.duration_since(t) < STEAM_CLAIM_CACHE_TTL)
            {
                return guard.running;
            }
            let running = detect_steam_running();
            guard.running = running;
            guard.refreshed_at = Some(now);
            return running;
        }
        detect_steam_running()
    }

    #[cfg(test)]
    fn set_test_steam(&self, running: Option<bool>) {
        if let Ok(mut guard) = self.test_override.lock() {
            *guard = running;
        }
        if let Ok(mut cache) = self.steam_cache.lock() {
            cache.refreshed_at = None;
        }
        if let Ok(mut last) = self.last_steam.lock() {
            *last = None;
        }
    }

    /// Drop claims for pads that are no longer present.
    pub fn sync(&self, active_serials: impl IntoIterator<Item = impl AsRef<str>>) {
        let active: HashSet<String> = active_serials
            .into_iter()
            .map(|s| normalize_identity(s.as_ref()))
            .collect();
        if let Ok(mut claimed) = self.claimed.lock() {
            claimed.retain(|s| active.contains(s));
        }
        if let Ok(mut applied) = self.applied.lock() {
            applied.retain(|s, _| active.contains(s));
        }
    }

    /// Record a successful canonical-color write (poll reassert / hot SetRgb).
    /// Pulse orange/restore writes must not record: orange is not canonical.
    pub fn note_applied(&self, serial: &str, color: Rgb) {
        if let Ok(mut applied) = self.applied.lock() {
            applied.insert(normalize_identity(serial), (color, Instant::now()));
        }
    }

    /// Whether `serial` needs a lightbar write for `color` right now.
    pub fn reassert_due(&self, serial: &str, color: Rgb) -> bool {
        let record = self
            .applied
            .lock()
            .map(|guard| guard.get(&normalize_identity(serial)).copied())
            .unwrap_or(None);
        reassert_due_at(record, color, Instant::now())
    }

    /// Drop all `LIGHT_OUT` claims (e.g. CLI force-reapply, hid-worker session start).
    pub fn forget_all(&self) {
        if let Ok(mut claimed) = self.claimed.lock() {
            claimed.clear();
        }
        if let Ok(mut applied) = self.applied.lock() {
            applied.clear();
        }
    }

    /// Forget claims when Steam starts or stops so the next write picks the right path.
    fn sync_steam_transition(&self) {
        let running = self.steam_running();
        let Ok(mut last) = self.last_steam.lock() else {
            return;
        };
        match *last {
            None => *last = Some(running),
            Some(prev) if prev != running => {
                self.forget_all();
                *last = Some(running);
            }
            Some(_) => {}
        }
    }

    pub(crate) fn forget_one(&self, serial: &str) {
        if let Ok(mut claimed) = self.claimed.lock() {
            claimed.remove(&normalize_identity(serial));
        }
    }

    pub fn take_claim(&self, serial: &str) -> bool {
        self.sync_steam_transition();
        // Steam Input already initialized the lightbar; LIGHT_OUT fights it and leaves
        // RGB writes as silent no-ops (restored from v0.1.7).
        if self.steam_running() {
            return false;
        }

        let Ok(mut claimed) = self.claimed.lock() else {
            return true;
        };
        claimed.insert(normalize_identity(serial))
    }

    /// Drop the claim for `serial` so the next apply runs `LIGHT_OUT` then RGB.
    ///
    /// Used when a pad newly appears in the live set: presence can clear the tray
    /// without a HID poll, which would otherwise leave a stale claim and skip
    /// reclaim on Bluetooth reconnect. No-op for claim strategy while Steam is
    /// running (`take_claim` skips `LIGHT_OUT`).
    pub fn prepare_connect(&self, serial: &str) {
        self.forget_one(serial);
    }
}

impl Default for ClaimStore {
    fn default() -> Self {
        Self::new()
    }
}

fn detect_steam_running() -> bool {
    #[cfg(windows)]
    {
        crate::games::process_match::any_process_name_eq_ignore_case("steam.exe")
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Pure decision: reassert when no record, the color changed, or the slow
/// backstop elapsed (foreign overwrites must not stick forever).
fn reassert_due_at(record: Option<(Rgb, Instant)>, color: Rgb, now: Instant) -> bool {
    match record {
        None => true,
        Some((last_color, at)) => {
            last_color != color || now.saturating_duration_since(at) >= LIGHTBAR_REASSERT_INTERVAL
        }
    }
}

fn is_retryable_write_error(err: &impl std::fmt::Display) -> bool {
    let text = err.to_string();
    text.contains("0x000003E5")
        || text.contains("WaitForSingleObject")
        || text.contains("Overlapped I/O")
}

/// Request calibration so the pad switches to full BT reports / accepts effects (SDL).
/// Best-effort: ignore failures (Steam may already own the feature pipe).
pub(crate) fn prepare_bt_output_mode(device: &HidDevice) {
    let mut feature = vec![0u8; CALIBRATION_FEATURE_SIZE];
    feature[0] = CALIBRATION_FEATURE_REPORT;
    let _ = device.get_feature_report(&mut feature);
}

/// Daemon worker entry: no outer lock (worker is exclusive). Uses caller's `HidApi`.
pub fn apply_lightbar_rgb_timed(
    api: &HidApi,
    serial: &str,
    color: Rgb,
    claims: &ClaimStore,
) -> (Result<(), String>, HidPhaseTiming) {
    let (first, mut timing) = try_apply_by_serial_timed(api, serial, color, false, claims);
    match first {
        Ok(()) => (Ok(()), timing),
        Err(err) if is_retryable_write_error(&err) => {
            thread::sleep(WRITE_RETRY_DELAY);
            let (retry, t2) = try_apply_by_serial_timed(api, serial, color, true, claims);
            timing.add_assign(t2);
            (retry, timing)
        }
        Err(err) => (Err(err), timing),
    }
}

fn try_apply_by_serial_timed(
    api: &HidApi,
    serial: &str,
    color: Rgb,
    retry: bool,
    claims: &ClaimStore,
) -> (Result<(), String>, HidPhaseTiming) {
    let mut timing = HidPhaseTiming::default();
    let enum_started = Instant::now();
    let target = normalize_identity(serial);

    // Prefer USB gamepad, then BT gamepad, then any other DualSense collection for
    // the same serial (output report 0x31 is not always accepted on the Gamepad
    // usage alone under Windows Bluetooth).
    let mut candidates: Vec<(HidDevice, bool, i32)> = Vec::new(); // device, is_bt, rank
    let mut open_ms = 0u128;
    for info in api.device_list().filter(|d| is_dualsense_device(d)) {
        let open_started = Instant::now();
        let hint = dualsense::hid_serial(info);
        let device = {
            let _op = crate::controller::hid::diag::enter_op("open_device");
            match info.open_device(api) {
                Ok(d) => {
                    let ms = open_started.elapsed().as_millis();
                    open_ms += ms;
                    crate::controller::hid::diag::trace_open("lightbar", info, &hint, ms, Ok(()));
                    d
                }
                Err(err) => {
                    let ms = open_started.elapsed().as_millis();
                    open_ms += ms;
                    crate::controller::hid::diag::trace_open(
                        "lightbar",
                        info,
                        &hint,
                        ms,
                        Err(&err.to_string()),
                    );
                    continue;
                }
            }
        };
        let identity = resolve_device_identity(info, &device);
        if identity != target {
            continue;
        }
        let is_bluetooth = matches!(info.bus_type(), BusType::Bluetooth);
        let is_usb = matches!(info.bus_type(), BusType::Usb);
        let is_gamepad = is_dualsense_gamepad(info);
        let rank = match (is_usb, is_gamepad) {
            (true, true) => 0,
            (true, false) => 1,
            (false, true) => 2,
            (false, false) => 3,
        };
        candidates.push((device, is_bluetooth, rank));
    }
    timing.enumerate_ms = enum_started.elapsed().as_millis().saturating_sub(open_ms);
    timing.open_ms = open_ms;

    if candidates.is_empty() {
        return (Err(format!("controller {serial} not found")), timing);
    }
    candidates.sort_by_key(|(_, _, rank)| *rank);

    let io_started = Instant::now();
    let mut last_err: Option<String> = None;
    let mut any_ok = false;
    // One claim decision for the serial; later interfaces only send RGB.
    let mut claim = claims.take_claim(&target);
    for (device, is_bluetooth, _) in candidates {
        if is_bluetooth {
            prepare_bt_output_mode(&device);
        }
        match apply_on_open_device_inner(
            &device,
            &target,
            color,
            is_bluetooth,
            "lightbar",
            retry,
            claim,
            claims,
        ) {
            Ok(()) => {
                any_ok = true;
                claim = false;
            }
            Err(err) => {
                if claim {
                    claims.forget_one(&target);
                    claim = claims.take_claim(&target);
                }
                last_err = Some(err);
            }
        }
    }
    timing.io_ms = io_started.elapsed().as_millis();

    if any_ok {
        (Ok(()), timing)
    } else {
        (
            Err(last_err.unwrap_or_else(|| format!("controller {serial} not found"))),
            timing,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_on_open_device_inner(
    device: &HidDevice,
    serial: &str,
    color: Rgb,
    is_bluetooth: bool,
    caller: &str,
    retry: bool,
    claim: bool,
    claims: &ClaimStore,
) -> Result<(), String> {
    let target = normalize_identity(serial);
    match set_lightbar_on_device(
        device,
        color,
        is_bluetooth,
        claim,
        &target,
        caller,
        retry,
        claims,
    ) {
        Ok(()) => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

/// Write lightbar while already holding the HID I/O lock (used during poll).
///
/// Bluetooth DualSense ignores RGB until the lightbar is reconfigured with a
/// dedicated `LIGHT_OUT` setup report (same as Linux `hid-playstation`) when Steam
/// is **not** running. Color is then applied in a second report with only the
/// lightbar RGB flag. While Steam Input is running, skip `LIGHT_OUT` — Steam already
/// initialized the bar and the claim fights it (silent no-op).
///
/// If the claim write fails, RGB is **not** attempted on the same handle.
#[allow(clippy::too_many_arguments)]
pub fn set_lightbar_on_device(
    device: &HidDevice,
    color: Rgb,
    is_bluetooth: bool,
    claim: bool,
    serial: &str,
    caller: &str,
    retry: bool,
    claims: &ClaimStore,
) -> Result<(), hidapi::HidError> {
    if claim {
        write_lightbar_claim(device, is_bluetooth, serial, caller, retry, claims)?;
    }
    write_lightbar_rgb(
        device,
        is_bluetooth,
        color,
        serial,
        caller,
        claim,
        retry,
        claims,
    )
}

fn write_lightbar_claim(
    device: &HidDevice,
    is_bluetooth: bool,
    serial: &str,
    caller: &str,
    retry: bool,
    claims: &ClaimStore,
) -> Result<(), hidapi::HidError> {
    write_output_report(
        device,
        is_bluetooth,
        serial,
        caller,
        "claim",
        None,
        true,
        retry,
        claims,
        |common| {
            common[OFF_VALID_FLAG2] = OUTPUT_VALID_FLAG2_LIGHTBAR_SETUP;
            common[OFF_LIGHTBAR_SETUP] = OUTPUT_LIGHTBAR_SETUP_LIGHT_OUT;
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn write_lightbar_rgb(
    device: &HidDevice,
    is_bluetooth: bool,
    color: Rgb,
    serial: &str,
    caller: &str,
    claim_needed: bool,
    retry: bool,
    claims: &ClaimStore,
) -> Result<(), hidapi::HidError> {
    write_output_report(
        device,
        is_bluetooth,
        serial,
        caller,
        "rgb",
        Some((color.r, color.g, color.b)),
        claim_needed,
        retry,
        claims,
        |common| {
            common[OFF_VALID_FLAG1] = OUTPUT_VALID_FLAG1_LIGHTBAR;
            common[OFF_LIGHTBAR_R] = color.r;
            common[OFF_LIGHTBAR_G] = color.g;
            common[OFF_LIGHTBAR_B] = color.b;
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_output_report(
    device: &HidDevice,
    is_bluetooth: bool,
    serial: &str,
    caller: &str,
    phase: &str,
    rgb: Option<(u8, u8, u8)>,
    claim_needed: bool,
    retry: bool,
    claims: &ClaimStore,
    fill_common: impl FnOnce(&mut [u8]),
) -> Result<(), hidapi::HidError> {
    let bus = crate::controller::hid::diag::bus_tag(if is_bluetooth {
        BusType::Bluetooth
    } else {
        BusType::Usb
    });
    let started = Instant::now();
    // (bytes_reported, expected_len, transport)
    let result: Result<(usize, usize, &'static str), hidapi::HidError> = if is_bluetooth {
        let report = build_bt_report(claims, fill_common);
        let _op = crate::controller::hid::diag::enter_op("hid_write");
        // Windows BT + Steam: interrupt `write` with RGB-only (no LIGHT_OUT) is the
        // path that historically updated the bar (v0.1.7). Control `send_output_report`
        // returns Ok here but the bar stays unchanged (session 1790684159072).
        // Fall back to control only when interrupt errors hard.
        #[cfg(windows)]
        {
            match device.write(&report) {
                Ok(n) => Ok((n, report.len(), "interrupt")),
                Err(write_err) => match device.send_output_report(&report) {
                    Ok(()) => Ok((report.len(), report.len(), "control-fallback")),
                    Err(_) => Err(write_err),
                },
            }
        }
        #[cfg(not(windows))]
        {
            match device.write(&report) {
                Ok(n) => Ok((n, report.len(), "interrupt")),
                Err(err) => Err(err),
            }
        }
    } else {
        let mut report = [0u8; OUTPUT_REPORT_USB_SIZE];
        report[0] = OUTPUT_REPORT_USB_ID;
        fill_common(&mut report[1..]);
        let _op = crate::controller::hid::diag::enter_op("hid_write");
        match device.write(&report) {
            Ok(n) => Ok((n, report.len(), "interrupt")),
            Err(err) => Err(err),
        }
    };
    let ms = started.elapsed().as_millis();
    match &result {
        Ok((n, expected, transport)) => crate::controller::hid::diag::trace_write(
            caller,
            phase,
            serial,
            bus,
            transport,
            rgb,
            claim_needed,
            retry,
            ms,
            Ok((*n, *expected)),
        ),
        Err(err) => crate::controller::hid::diag::trace_write(
            caller,
            phase,
            serial,
            bus,
            "interrupt",
            rgb,
            claim_needed,
            retry,
            ms,
            Err(err.to_string()),
        ),
    }
    result.map(|_| ())
}

fn build_bt_report(
    claims: &ClaimStore,
    fill_common: impl FnOnce(&mut [u8]),
) -> [u8; OUTPUT_REPORT_BT_SIZE] {
    let mut report = [0u8; OUTPUT_REPORT_BT_SIZE];
    report[0] = OUTPUT_REPORT_BT_ID;
    report[1] = claims.next_seq_tag();
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
///
/// Takes the caller's `&HidIoLock` (CLI one-shots use a local
/// `HidIoLock::new()`); the timed variant below assumes the lock is held.
pub fn apply_lightbar_all(
    lock: &HidIoLock,
    color: Rgb,
    claims: &ClaimStore,
) -> Result<usize, String> {
    let _guard = lock.lock()?;
    apply_lightbar_all_timed(color, claims).0
}

pub fn apply_lightbar_all_timed(
    color: Rgb,
    claims: &ClaimStore,
) -> (Result<usize, String>, HidPhaseTiming) {
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

    claims.forget_all();

    let mut open_ms = 0u128;
    let mut io_ms = 0u128;
    for info in api.device_list().filter(|d| is_dualsense_gamepad(d)) {
        let open_started = Instant::now();
        let hint = dualsense::hid_serial(info);
        let device = {
            let _op = crate::controller::hid::diag::enter_op("open_device");
            match info.open_device(&api) {
                Ok(d) => {
                    let ms = open_started.elapsed().as_millis();
                    open_ms += ms;
                    crate::controller::hid::diag::trace_open(
                        "lightbar_all",
                        info,
                        &hint,
                        ms,
                        Ok(()),
                    );
                    d
                }
                Err(err) => {
                    let ms = open_started.elapsed().as_millis();
                    open_ms += ms;
                    crate::controller::hid::diag::trace_open(
                        "lightbar_all",
                        info,
                        &hint,
                        ms,
                        Err(&err.to_string()),
                    );
                    app_log::warn(format!("lightbar open failed: {err}"));
                    continue;
                }
            }
        };
        let serial = resolve_device_identity(info, &device);
        let is_bluetooth = matches!(info.bus_type(), BusType::Bluetooth);
        if is_bluetooth {
            prepare_bt_output_mode(&device);
        }
        let claim = claims.take_claim(&serial);
        let io_started = Instant::now();
        match apply_on_open_device_inner(
            &device,
            &serial,
            color,
            is_bluetooth,
            "lightbar_all",
            false,
            claim,
            claims,
        ) {
            Ok(()) => applied += 1,
            Err(err) => {
                if claim {
                    claims.forget_one(&serial);
                }
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
        let claims = ClaimStore::new();
        let report = build_bt_report(&claims, |common| {
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
        let claims = ClaimStore::new();
        let report = build_bt_report(&claims, |common| {
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
        let claims = ClaimStore::new();
        claims.set_test_steam(Some(false));
        claims.forget_all();

        claims.sync(std::iter::empty::<&str>());
        assert!(claims.take_claim("aabbcc"));
        assert!(!claims.take_claim("aabbcc"));
        assert!(claims.take_claim("ddeeff"));
        claims.sync(["ddeeff"]);
        assert!(!claims.take_claim("ddeeff"));
        assert!(claims.take_claim("aabbcc"));
        claims.sync(std::iter::empty::<&str>());

        claims.forget_all();
        assert!(claims.take_claim("aabbcc"));
        assert!(!claims.take_claim("aabbcc"));
        claims.prepare_connect("aabbcc");
        assert!(claims.take_claim("aabbcc"));
    }

    #[test]
    fn steam_skips_light_out_claim() {
        let claims = ClaimStore::new();
        claims.set_test_steam(Some(true));
        claims.forget_all();
        assert!(!claims.take_claim("aabbcc"));
        assert!(!claims.take_claim("aabbcc"));

        claims.set_test_steam(Some(false));
        // Transition clears claims; without Steam we claim once.
        assert!(claims.take_claim("aabbcc"));
        assert!(!claims.take_claim("aabbcc"));

        claims.set_test_steam(Some(true));
        assert!(!claims.take_claim("aabbcc"));
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

    #[test]
    fn reassert_due_without_record_or_on_change_or_backstop() {
        use crate::domain::color::Rgb;
        let now = Instant::now();
        let red = Rgb::new(255, 0, 0);
        let blue = Rgb::new(0, 0, 255);
        assert!(reassert_due_at(None, red, now));
        assert!(!reassert_due_at(Some((red, now)), red, now));
        assert!(reassert_due_at(Some((blue, now)), red, now));
        assert!(reassert_due_at(
            Some((red, now - LIGHTBAR_REASSERT_INTERVAL)),
            red,
            now
        ));
        assert!(!reassert_due_at(
            Some((
                red,
                now - LIGHTBAR_REASSERT_INTERVAL + Duration::from_secs(1)
            )),
            red,
            now
        ));
    }
}
