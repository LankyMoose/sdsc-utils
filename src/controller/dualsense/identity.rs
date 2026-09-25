//! Shared DualSense HID identity, device filters, and I/O lock.

use crate::platform::app_log;
use hidapi::{BusType, DeviceInfo, HidDevice};
use std::sync::Mutex;

pub const SONY_VENDOR_ID: u16 = 0x054C;
pub const DUALSENSE_PRODUCT_ID: u16 = 0x0CE6;
pub const DUALSENSE_EDGE_PRODUCT_ID: u16 = 0x0DF2;

/// Generic Desktop / Game Pad — the DualSense HID interface that carries battery data.
const HID_USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
const HID_USAGE_GAMEPAD: u16 = 0x05;

/// DualSense pairing-info feature report (Linux hid-playstation).
const PAIRING_INFO_FEATURE_REPORT: u8 = 0x09;
const PAIRING_INFO_FEATURE_SIZE: usize = 20;

/// Serializes all DualSense HID open/read/write/close (battery, lightbar, power-off).
static HID_IO_LOCK: Mutex<()> = Mutex::new(());

/// Hold the DualSense HID I/O lock for a closure (CLI one-shots only).
pub fn with_hid_lock<T>(f: impl FnOnce() -> T) -> T {
    let _guard = HID_IO_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    f()
}

/// Try to take the HID lock; returns an error string if poisoned.
pub fn lock_hid() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    HID_IO_LOCK
        .lock()
        .map_err(|_| "HID lock poisoned".to_string())
}

pub fn product_name(product_id: u16) -> &'static str {
    match product_id {
        DUALSENSE_EDGE_PRODUCT_ID => "DualSense Edge",
        _ => "DualSense",
    }
}

pub fn hid_serial(info: &DeviceInfo) -> String {
    info.serial_number()
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

/// True when a serial is a real pad identity (not empty / `"unknown"`).
pub fn is_storable_serial(serial: &str) -> bool {
    !serial.is_empty() && serial != "unknown"
}

/// Normalize MAC-style identities so `AA:BB:…` and `aabb…` compare equal.
pub fn normalize_identity(serial: &str) -> String {
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
pub fn resolve_device_identity(info: &DeviceInfo, device: &HidDevice) -> String {
    let hid = hid_serial(info);
    if is_storable_serial(&hid) {
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

/// True for DualSense / DualSense Edge vendor+product (any interface).
pub fn is_dualsense_vid_pid(vendor_id: u16, product_id: u16) -> bool {
    vendor_id == SONY_VENDOR_ID
        && matches!(product_id, DUALSENSE_PRODUCT_ID | DUALSENSE_EDGE_PRODUCT_ID)
}

pub fn is_dualsense_gamepad(d: &DeviceInfo) -> bool {
    is_dualsense_vid_pid(d.vendor_id(), d.product_id())
        && d.usage_page() == HID_USAGE_PAGE_GENERIC_DESKTOP
        && d.usage() == HID_USAGE_GAMEPAD
}

pub fn is_dualsense_device(d: &DeviceInfo) -> bool {
    is_dualsense_vid_pid(d.vendor_id(), d.product_id())
}

/// Presence keys for connect/disconnect (HID paths — unique per USB/BT node).
/// One-shot helper (CLI/tests). The daemon uses [`crate::controller::hid::worker::HidWorkerHandle::presence_paths`].
#[allow(dead_code)]
pub fn list_presence_paths() -> Result<Vec<String>, String> {
    let api = hidapi::HidApi::new().map_err(|e| e.to_string())?;
    let mut keys: Vec<String> = api
        .device_list()
        .filter(|d| is_dualsense_gamepad(d))
        .map(|d| d.path().to_string_lossy().into_owned())
        .collect();
    keys.sort();
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_identity_strips_separators_and_case() {
        assert_eq!(
            normalize_identity("AA:BB:CC:DD:EE:FF"),
            normalize_identity("aa-bb-cc-dd-ee-ff")
        );
    }

    #[test]
    fn storable_serial_rejects_empty_and_unknown() {
        assert!(!is_storable_serial(""));
        assert!(!is_storable_serial("unknown"));
        assert!(is_storable_serial("aabbccddeeff"));
    }
}
