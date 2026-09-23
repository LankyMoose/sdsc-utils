//! DualSense discovery and battery reading.

use crate::app_log;
use crate::dualsense::{
    is_dualsense_device, is_storable_serial, normalize_identity, resolve_device_identity,
};
use hidapi::{BusType, HidApi, HidDevice};
use std::thread;
use std::time::Duration;

const USB_REPORT_SIZE: usize = 64;
const BT_REPORT_SIZE: usize = 78;
const USB_POWER_OFFSET: usize = 53;
const BT_POWER_OFFSET: usize = 54;

const BT_REPORT_TRUNCATED: u8 = 0x01;
const BT_REPORT_FULL: u8 = 0x31;
const USB_REPORT_ID: u8 = 0x01;
const CALIBRATION_FEATURE_REPORT: u8 = 0x05;
const CALIBRATION_FEATURE_SIZE: usize = 41;
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
pub(crate) struct BatteryReading {
    pub percent: u8,
    pub state: PowerState,
    pub connection: &'static str,
}

fn is_known_serial(serial: &str) -> bool {
    is_storable_serial(serial)
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

pub(crate) fn read_battery(device: &HidDevice) -> Result<BatteryReading, hidapi::HidError> {
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

        return Ok(BatteryReading {
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

/// Power off a DualSense over Bluetooth (feature report 0x08). USB pads are rejected.
///
/// Windows exposes DualSense as multiple HID interfaces; feature report 0x08 may not be
/// on the Gamepad usage we use for battery. Try every Bluetooth DualSense interface and
/// several report sizes/CRC seeds until SetFeature succeeds.
/// Power-off using caller's `HidApi` (hid-worker). CLI can `HidApi::new()` once then call this.
pub fn power_off_bluetooth_timed(
    api: &HidApi,
    serial: &str,
) -> (Result<(), String>, crate::lightbar::HidPhaseTiming) {
    let mut timing = crate::lightbar::HidPhaseTiming::default();
    let enum_started = std::time::Instant::now();
    let result = power_off_bluetooth_unlocked_timed(api, serial, &mut timing);
    if timing.enumerate_ms == 0 && timing.open_ms == 0 && timing.io_ms == 0 {
        timing.enumerate_ms = enum_started.elapsed().as_millis();
    }
    (result, timing)
}

fn power_off_bluetooth_unlocked_timed(
    api: &HidApi,
    serial: &str,
    timing: &mut crate::lightbar::HidPhaseTiming,
) -> Result<(), String> {
    let enum_started = std::time::Instant::now();
    let target = normalize_identity(serial);

    let mut matched: Vec<(String, HidDevice)> = Vec::new();
    let mut unknown: Vec<(String, HidDevice)> = Vec::new();
    let mut open_ms = 0u128;
    for info in api.device_list().filter(|d| is_dualsense_device(d)) {
        if !matches!(info.bus_type(), BusType::Bluetooth) {
            continue;
        }
        let path = info.path().to_string_lossy().into_owned();
        let open_started = std::time::Instant::now();
        let device = match info.open_device(api) {
            Ok(d) => d,
            Err(err) => {
                open_ms += open_started.elapsed().as_millis();
                app_log::warn(format!("power-off: open failed for {path}: {err}"));
                continue;
            }
        };
        open_ms += open_started.elapsed().as_millis();
        let identity = resolve_device_identity(info, &device);
        if identity == target {
            matched.push((path, device));
        } else if !is_known_serial(&identity) {
            // Vendor/audio collections often lack a serial; try after exact matches.
            unknown.push((path, device));
        }
    }
    timing.enumerate_ms += enum_started.elapsed().as_millis().saturating_sub(open_ms);
    timing.open_ms += open_ms;

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

    let io_started = std::time::Instant::now();
    for (path, device) in &candidates {
        for &seed in &FEATURE_CRC32_SEEDS {
            for &size in &sizes {
                let report = build_bt_control_off_report(size, seed);
                match device.send_feature_report(&report) {
                    Ok(()) => {
                        timing.io_ms += io_started.elapsed().as_millis();
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
    timing.io_ms += io_started.elapsed().as_millis();

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
