//! DualSense discovery and battery reading.

use crate::app_log;
use crate::color::color_for_battery_percent;
use crate::lightbar::{self, with_lightbar_lock};
use hidapi::{BusType, DeviceInfo, HidApi, HidDevice};
use std::thread;
use std::time::Duration;

const SONY_VENDOR_ID: u16 = 0x054C;
const DUALSENSE_PRODUCT_ID: u16 = 0x0CE6;
const DUALSENSE_EDGE_PRODUCT_ID: u16 = 0x0DF2;

/// Generic Desktop / Game Pad — the DualSense HID interface that carries battery data.
const HID_USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
const HID_USAGE_GAMEPAD: u16 = 0x05;

const USB_REPORT_SIZE: usize = 64;
const BT_REPORT_SIZE: usize = 78;
const USB_POWER_OFFSET: usize = 53;
const BT_POWER_OFFSET: usize = 54;

const BT_REPORT_TRUNCATED: u8 = 0x01;
const BT_REPORT_FULL: u8 = 0x31;
const USB_REPORT_ID: u8 = 0x01;
const CALIBRATION_FEATURE_REPORT: u8 = 0x05;
const CALIBRATION_FEATURE_SIZE: usize = 41;
/// DualSense pairing-info feature report (Linux hid-playstation).
const PAIRING_INFO_FEATURE_REPORT: u8 = 0x09;
const PAIRING_INFO_FEATURE_SIZE: usize = 20;
/// DualSense Bluetooth control feature report (dualsensectl / HID descriptor).
/// Descriptor Report Count for ID 0x08 is 47 data bytes → 48 with report ID.
const BT_CONTROL_FEATURE_REPORT: u8 = 0x08;
const BT_CONTROL_FEATURE_SIZE: usize = 48;
/// dualsensectl historically used 47; keep as a Windows fallback size.
const BT_CONTROL_FEATURE_SIZE_ALT: usize = 47;
const BT_CONTROL_FEATURE_SIZE_PADDED: usize = 64;
const BT_CONTROL_OFF: u8 = 0x02;
/// Feature-report CRC seeds: Linux hid-playstation uses 0xA3; dualsensectl uses 0x53.
const FEATURE_CRC32_SEEDS: [u8; 2] = [0xA3, 0x53];

const POWER_LEVEL_MASK: u8 = 0x0F;
const POWER_STATE_SHIFT: u8 = 4;
const MAX_POWER_LEVEL: u8 = 0x0A;

/// Lowest DualSense reporting bucket (mid-point 5% ≈ 0–9%).
pub const LOW_BATTERY_PERCENT: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    Discharging,
    Charging,
    Complete,
    AbnormalVoltage,
    AbnormalTemperature,
    ChargingError,
    Unknown,
}

impl PowerState {
    pub fn from_nibble(value: u8) -> Self {
        match value {
            0x00 => Self::Discharging,
            0x01 => Self::Charging,
            0x02 => Self::Complete,
            0x0A => Self::AbnormalVoltage,
            0x0B => Self::AbnormalTemperature,
            0x0F => Self::ChargingError,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discharging => "discharging",
            Self::Charging => "charging",
            Self::Complete => "fully charged",
            Self::AbnormalVoltage => "abnormal voltage",
            Self::AbnormalTemperature => "abnormal temperature",
            Self::ChargingError => "charging error",
            Self::Unknown => "unknown",
        }
    }

    pub fn is_discharging(self) -> bool {
        matches!(self, Self::Discharging)
    }
}

#[derive(Debug, Clone)]
pub struct ControllerStatus {
    pub index: usize,
    pub product: &'static str,
    pub connection: &'static str,
    pub serial: String,
    pub percent: u8,
    pub state: PowerState,
}

impl ControllerStatus {
    /// Low battery while discharging (toast, orange pulse, popup label).
    pub fn is_low_battery(&self, threshold: u8) -> bool {
        self.percent <= threshold && self.state.is_discharging()
    }
}

#[derive(Debug, Clone)]
struct BatteryStatus {
    percent: u8,
    state: PowerState,
    connection: &'static str,
}

fn hid_serial(info: &DeviceInfo) -> String {
    info.serial_number()
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn is_known_serial(serial: &str) -> bool {
    !serial.is_empty() && serial != "unknown"
}

/// Normalize MAC-style identities so `AA:BB:…` and `aabb…` compare equal.
pub(crate) fn normalize_identity(serial: &str) -> String {
    serial
        .chars()
        .filter(|c| *c != ':' && *c != '-')
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Format a little-endian MAC (as in feature report 0x09) like Windows BT HID serials.
fn format_mac_from_le(mac_le: &[u8; 6]) -> String {
    mac_le.iter().rev().map(|b| format!("{b:02x}")).collect()
}

fn read_mac_address_le(device: &HidDevice) -> Result<[u8; 6], hidapi::HidError> {
    let mut buf = vec![0u8; PAIRING_INFO_FEATURE_SIZE];
    buf[0] = PAIRING_INFO_FEATURE_REPORT;
    let n = device.get_feature_report(&mut buf)?;
    if n < 7 || buf[0] != PAIRING_INFO_FEATURE_REPORT {
        return Err(hidapi::HidError::HidApiError {
            message: format!("pairing-info report too short or unexpected id ({n} bytes)"),
        });
    }
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&buf[1..7]);
    Ok(mac)
}

/// Stable pad identity: HID serial when present, otherwise MAC from pairing-info.
/// Windows often leaves the USB serial empty while Bluetooth exposes the MAC.
pub(crate) fn resolve_device_identity(info: &DeviceInfo, device: &HidDevice) -> String {
    let hid = hid_serial(info);
    if is_known_serial(&hid) {
        return normalize_identity(&hid);
    }
    match read_mac_address_le(device) {
        Ok(mac) => format_mac_from_le(&mac),
        Err(err) => {
            app_log::warn(format!(
                "could not read MAC for {} over {}: {err}",
                product_name(info.product_id()),
                match info.bus_type() {
                    BusType::Usb => "USB",
                    BusType::Bluetooth => "Bluetooth",
                    _ => "unknown bus",
                }
            ));
            "unknown".into()
        }
    }
}

/// Presence keys for connect/disconnect (HID paths — unique per USB/BT node).
pub fn list_controller_serials() -> Result<Vec<String>, String> {
    let api = HidApi::new().map_err(|e| e.to_string())?;
    let mut keys: Vec<String> = api
        .device_list()
        .filter(|d| is_dualsense_gamepad(d))
        .map(|d| d.path().to_string_lossy().into_owned())
        .collect();
    keys.sort();
    Ok(keys)
}

/// Collapse the same physical pad enumerated on USB and Bluetooth (prefer USB).
#[cfg(test)]
fn dedupe_statuses(mut statuses: Vec<ControllerStatus>) -> Vec<ControllerStatus> {
    let mut unique: Vec<ControllerStatus> = Vec::with_capacity(statuses.len());

    for status in statuses.drain(..) {
        if !is_known_serial(&status.serial) {
            unique.push(status);
            continue;
        }

        if let Some(existing) = unique
            .iter_mut()
            .find(|s| is_known_serial(&s.serial) && s.serial == status.serial)
        {
            let status_is_usb = status.connection == "USB";
            let existing_is_usb = existing.connection == "USB";
            if status_is_usb && !existing_is_usb {
                *existing = status;
            }
        } else {
            unique.push(status);
        }
    }

    unique
}

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
        if !is_known_serial(&pad.status.serial) {
            unique.push(pad);
            continue;
        }

        if let Some(existing) = unique
            .iter_mut()
            .find(|s| is_known_serial(&s.status.serial) && s.status.serial == pad.status.serial)
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

/// Poll DualSense battery (and optionally refresh lightbar).
///
/// `force_lightbar`: re-assert RGB even when unchanged (battery / membership polls).
/// Liveness ticks pass `false` so unchanged colors are skipped.
pub fn poll_controllers(force_lightbar: bool) -> Result<Vec<ControllerStatus>, String> {
    // Entire open/read/write under one lock so pulse/spectrum cannot race this pad.
    with_lightbar_lock(|| poll_controllers_unlocked(force_lightbar))
}

fn poll_controllers_unlocked(force_lightbar: bool) -> Result<Vec<ControllerStatus>, String> {
    let api = HidApi::new().map_err(|e| e.to_string())?;
    let devices: Vec<&DeviceInfo> = api
        .device_list()
        .filter(|d| is_dualsense_gamepad(d))
        .collect();

    if devices.is_empty() {
        lightbar::sync_lightbar_claims(std::iter::empty::<&str>());
        return Ok(Vec::new());
    }

    let mut pads = Vec::with_capacity(devices.len());

    for info in devices {
        let product = product_name(info.product_id());
        let hid = hid_serial(info);
        let is_bluetooth = matches!(info.bus_type(), BusType::Bluetooth);

        match info.open_device(&api).and_then(|device| {
            let serial = resolve_device_identity(info, &device);
            let battery = read_battery(&device)?;
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

    // Apply lightbar on the same handle used for the battery read.
    if lightbar::is_enabled() {
        for pad in &pads {
            let color = color_for_battery_percent(pad.status.percent);
            if let Err(err) = lightbar::apply_on_open_device(
                &pad.device,
                &pad.status.serial,
                color,
                pad.is_bluetooth,
                force_lightbar,
            ) {
                // Reopen clears stuck Windows overlapped I/O after a write timeout.
                if let Err(retry_err) =
                    lightbar::apply_lightbar_rgb_unlocked(&pad.status.serial, color, force_lightbar)
                {
                    lightbar::warn_lightbar(
                        pad.status.product,
                        format!("{err}; retry: {retry_err}"),
                    );
                }
            }
        }
    }

    Ok(pads.into_iter().map(|p| p.status).collect())
}

pub(crate) fn is_dualsense_gamepad(d: &DeviceInfo) -> bool {
    d.vendor_id() == SONY_VENDOR_ID
        && matches!(
            d.product_id(),
            DUALSENSE_PRODUCT_ID | DUALSENSE_EDGE_PRODUCT_ID
        )
        && d.usage_page() == HID_USAGE_PAGE_GENERIC_DESKTOP
        && d.usage() == HID_USAGE_GAMEPAD
}

fn product_name(product_id: u16) -> &'static str {
    match product_id {
        DUALSENSE_EDGE_PRODUCT_ID => "DualSense Edge",
        _ => "DualSense",
    }
}

fn read_battery(device: &HidDevice) -> Result<BatteryStatus, hidapi::HidError> {
    let bus_type = device.get_device_info()?.bus_type();
    let (connection, report_size, power_offset, is_bluetooth) = match bus_type {
        BusType::Usb => ("USB", USB_REPORT_SIZE, USB_POWER_OFFSET, false),
        BusType::Bluetooth => ("Bluetooth", BT_REPORT_SIZE, BT_POWER_OFFSET, true),
        other => {
            return Err(hidapi::HidError::HidApiError {
                message: format!("unsupported connection type: {other:?}"),
            });
        }
    };

    let mut requested_full_report = false;

    // DualSense streams input reports when awake; a dead/sleeping pad must fail fast
    // so disconnect is visible within a couple of liveness ticks.
    for _ in 0..6 {
        let mut buf = vec![0u8; report_size];
        let n = device.read_timeout(&mut buf, 150)?;
        if n == 0 {
            continue;
        }

        if is_bluetooth && buf[0] == BT_REPORT_TRUNCATED {
            if !requested_full_report {
                request_full_bt_report(device)?;
                requested_full_report = true;
                thread::sleep(Duration::from_millis(200));
            }
            continue;
        }

        let expected_id = if is_bluetooth {
            BT_REPORT_FULL
        } else {
            USB_REPORT_ID
        };

        if buf[0] != expected_id {
            return Err(hidapi::HidError::HidApiError {
                message: format!(
                    "unexpected report id {:#04x} (expected {:#04x})",
                    buf[0], expected_id
                ),
            });
        }

        if n <= power_offset {
            return Err(hidapi::HidError::HidApiError {
                message: format!("input report too short ({n} bytes)"),
            });
        }

        let power = buf[power_offset];
        let level = (power & POWER_LEVEL_MASK).min(MAX_POWER_LEVEL);
        let state = PowerState::from_nibble(power >> POWER_STATE_SHIFT);
        let percent = percent_from_level(level, state);

        return Ok(BatteryStatus {
            percent,
            state,
            connection,
        });
    }

    Err(hidapi::HidError::HidApiError {
        message: "timed out waiting for a DualSense input report with battery data".into(),
    })
}

/// DualSense reports battery in 11 coarse steps (0..=10).
/// Linux maps each step to the mid-point of its 10% bucket:
/// 0 → 0–9% (5%), 1 → 10–19% (15%), …, 9 → 90–99% (95%), 10/full → 100%.
pub fn percent_from_level(level: u8, state: PowerState) -> u8 {
    match state {
        PowerState::Complete => 100,
        PowerState::AbnormalVoltage
        | PowerState::AbnormalTemperature
        | PowerState::ChargingError => 0,
        PowerState::Discharging | PowerState::Charging | PowerState::Unknown => {
            if level >= MAX_POWER_LEVEL {
                100
            } else {
                (u16::from(level) * 10 + 5).min(100) as u8
            }
        }
    }
}

fn request_full_bt_report(device: &HidDevice) -> Result<(), hidapi::HidError> {
    let mut feature = vec![0u8; CALIBRATION_FEATURE_SIZE];
    feature[0] = CALIBRATION_FEATURE_REPORT;
    device.get_feature_report(&mut feature)?;
    Ok(())
}

fn build_bt_control_off_report(size: usize, seed: u8) -> Vec<u8> {
    let mut buf = vec![0u8; size];
    buf[0] = BT_CONTROL_FEATURE_REPORT;
    buf[1] = BT_CONTROL_OFF;
    // CRC covers everything except the final 4 bytes (same family as output reports).
    let payload_end = size.saturating_sub(4);
    let mut data = Vec::with_capacity(payload_end + 1);
    data.push(seed);
    data.extend_from_slice(&buf[..payload_end]);
    let crc = crc32fast::hash(&data);
    buf[payload_end..].copy_from_slice(&crc.to_le_bytes());
    buf
}

fn is_dualsense_device(d: &DeviceInfo) -> bool {
    d.vendor_id() == SONY_VENDOR_ID
        && matches!(
            d.product_id(),
            DUALSENSE_PRODUCT_ID | DUALSENSE_EDGE_PRODUCT_ID
        )
}

/// Power off a DualSense over Bluetooth (feature report 0x08). USB pads are rejected.
///
/// Windows exposes DualSense as multiple HID interfaces; feature report 0x08 may not be
/// on the Gamepad usage we use for battery. Try every Bluetooth DualSense interface and
/// several report sizes/CRC seeds until SetFeature succeeds.
pub fn power_off_bluetooth(serial: &str) -> Result<(), String> {
    // Hold the lightbar lock for the whole open/write so we do not race identify/RGB.
    with_lightbar_lock(|| power_off_bluetooth_unlocked(serial))
}

fn power_off_bluetooth_unlocked(serial: &str) -> Result<(), String> {
    let api = HidApi::new().map_err(|e| e.to_string())?;
    let target = normalize_identity(serial);

    let mut matched: Vec<(String, HidDevice)> = Vec::new();
    let mut unknown: Vec<(String, HidDevice)> = Vec::new();
    for info in api.device_list().filter(|d| is_dualsense_device(d)) {
        if !matches!(info.bus_type(), BusType::Bluetooth) {
            continue;
        }
        let path = info.path().to_string_lossy().into_owned();
        let device = match info.open_device(&api) {
            Ok(d) => d,
            Err(err) => {
                app_log::warn(format!("power-off: open failed for {path}: {err}"));
                continue;
            }
        };
        let identity = resolve_device_identity(info, &device);
        if identity == target {
            matched.push((path, device));
        } else if !is_known_serial(&identity) {
            // Vendor/audio collections often lack a serial; try after exact matches.
            unknown.push((path, device));
        }
    }

    let mut candidates = matched;
    if candidates.is_empty() {
        candidates = unknown;
    } else {
        candidates.extend(unknown);
    }

    if candidates.is_empty() {
        return Err(format!(
            "Bluetooth controller {serial} not found (power-off is Bluetooth-only)"
        ));
    }

    let sizes = [
        BT_CONTROL_FEATURE_SIZE,
        BT_CONTROL_FEATURE_SIZE_ALT,
        BT_CONTROL_FEATURE_SIZE_PADDED,
    ];
    let mut errors = Vec::new();

    for (path, device) in &candidates {
        for &seed in &FEATURE_CRC32_SEEDS {
            for &size in &sizes {
                let report = build_bt_control_off_report(size, seed);
                match device.send_feature_report(&report) {
                    Ok(()) => {
                        app_log::info(format!(
                            "power-off sent for {serial} via {path} (size={size}, seed={seed:#04x})"
                        ));
                        return Ok(());
                    }
                    Err(err) => {
                        errors.push(format!("{path} size={size} seed={seed:#04x}: {err}"));
                    }
                }
            }
        }
    }

    Err(format!(
        "power-off feature report failed after {} attempt(s): {}",
        errors.len(),
        errors
            .last()
            .cloned()
            .unwrap_or_else(|| "no attempts".into())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_buckets_match_linux_midpoints() {
        assert_eq!(percent_from_level(0, PowerState::Discharging), 5);
        assert_eq!(percent_from_level(1, PowerState::Discharging), 15);
        assert_eq!(percent_from_level(9, PowerState::Discharging), 95);
        assert_eq!(percent_from_level(10, PowerState::Discharging), 100);
        assert_eq!(percent_from_level(10, PowerState::Charging), 100);
        assert_eq!(percent_from_level(3, PowerState::Complete), 100);
        assert_eq!(percent_from_level(5, PowerState::AbnormalVoltage), 0);
        assert_eq!(percent_from_level(5, PowerState::ChargingError), 0);
    }

    #[test]
    fn low_battery_requires_discharging() {
        let discharging = ControllerStatus {
            index: 1,
            product: "DualSense",
            connection: "Bluetooth",
            serial: "abc".into(),
            percent: 5,
            state: PowerState::Discharging,
        };
        let charging = ControllerStatus {
            state: PowerState::Charging,
            ..discharging.clone()
        };
        assert!(discharging.is_low_battery(LOW_BATTERY_PERCENT));
        assert!(!charging.is_low_battery(LOW_BATTERY_PERCENT));
    }

    #[test]
    fn low_battery_respects_threshold() {
        let pad = ControllerStatus {
            index: 1,
            product: "DualSense",
            connection: "USB",
            serial: "abc".into(),
            percent: 25,
            state: PowerState::Discharging,
        };
        assert!(!pad.is_low_battery(15));
        assert!(pad.is_low_battery(25));
        assert!(pad.is_low_battery(35));
    }

    #[test]
    fn mac_from_le_matches_windows_bt_serial_style() {
        // Feature report stores MAC little-endian; Windows BT serial is big-endian hex.
        let mac_le = [0x26, 0x69, 0x15, 0x48, 0x46, 0x44];
        assert_eq!(format_mac_from_le(&mac_le), "444648156926");
    }

    #[test]
    fn normalize_identity_strips_separators_and_case() {
        assert_eq!(
            normalize_identity("AA:BB:CC:DD:EE:FF"),
            normalize_identity("aa-bb-cc-dd-ee-ff")
        );
    }

    #[test]
    fn dedupe_prefers_usb_for_same_identity() {
        let bt = ControllerStatus {
            index: 0,
            product: "DualSense",
            connection: "Bluetooth",
            serial: "444648156926".into(),
            percent: 35,
            state: PowerState::Charging,
        };
        let usb = ControllerStatus {
            connection: "USB",
            percent: 35,
            ..bt.clone()
        };
        let other = ControllerStatus {
            serial: "444648164f65".into(),
            percent: 75,
            state: PowerState::Discharging,
            connection: "Bluetooth",
            ..bt.clone()
        };
        let merged = dedupe_statuses(vec![bt, usb, other]);
        assert_eq!(merged.len(), 2);
        let charging = merged.iter().find(|s| s.serial == "444648156926").unwrap();
        assert_eq!(charging.connection, "USB");
    }

    #[test]
    fn bt_power_off_report_has_id_command_and_crc() {
        let report = build_bt_control_off_report(BT_CONTROL_FEATURE_SIZE, 0xA3);
        assert_eq!(report.len(), BT_CONTROL_FEATURE_SIZE);
        assert_eq!(report[0], BT_CONTROL_FEATURE_REPORT);
        assert_eq!(report[1], BT_CONTROL_OFF);
        assert!(
            report[2..BT_CONTROL_FEATURE_SIZE - 4]
                .iter()
                .all(|&b| b == 0)
        );
        assert!(
            report[BT_CONTROL_FEATURE_SIZE - 4..]
                .iter()
                .any(|&b| b != 0)
        );

        let mut expected = vec![0u8; BT_CONTROL_FEATURE_SIZE];
        expected[0] = BT_CONTROL_FEATURE_REPORT;
        expected[1] = BT_CONTROL_OFF;
        let mut data = Vec::with_capacity(BT_CONTROL_FEATURE_SIZE - 3);
        data.push(0xA3);
        data.extend_from_slice(&expected[..BT_CONTROL_FEATURE_SIZE - 4]);
        let crc = crc32fast::hash(&data);
        expected[BT_CONTROL_FEATURE_SIZE - 4..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(report, expected);

        let alt = build_bt_control_off_report(BT_CONTROL_FEATURE_SIZE_ALT, 0x53);
        assert_eq!(alt.len(), BT_CONTROL_FEATURE_SIZE_ALT);
        assert_eq!(alt[0], BT_CONTROL_FEATURE_REPORT);
        assert_eq!(alt[1], BT_CONTROL_OFF);
    }
}
