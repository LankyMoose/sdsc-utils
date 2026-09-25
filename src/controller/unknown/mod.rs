//! Fallback driver for HID gamepads that are not DualSense / DualSense Edge.
//!
//! Presence-only: no input reads, feature reports, or output reports.

use crate::controller::dualsense::identity::{
    hid_serial, is_dualsense_vid_pid, is_storable_serial, normalize_identity,
};
use crate::controller::model::{
    BatteryReading, Connection, ControllerKind, ControllerStatus, PowerState,
};
use crate::ui::start::input::PadSample;
use hidapi::{DeviceInfo, HidDevice};
use std::collections::BTreeSet;

const HID_USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
const HID_USAGE_GAMEPAD: u16 = 0x05;

/// True for Generic Desktop / Gamepad nodes that DualSense does not own.
pub fn is_unknown_gamepad(d: &DeviceInfo) -> bool {
    d.usage_page() == HID_USAGE_PAGE_GENERIC_DESKTOP
        && d.usage() == HID_USAGE_GAMEPAD
        && !is_dualsense_vid_pid(d.vendor_id(), d.product_id())
}

pub fn product_label(info: &DeviceInfo) -> String {
    info.product_string()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "Controller".to_string())
}

/// Stable identity without opening the device or issuing HID I/O.
pub fn identity_from_info(info: &DeviceInfo) -> String {
    let hid = hid_serial(info);
    if is_storable_serial(&hid) {
        return normalize_identity(&hid);
    }
    let path = info.path().to_string_lossy();
    let path_key: String = path
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .take(48)
        .collect();
    format!(
        "{:04x}{:04x}{path_key}",
        info.vendor_id(),
        info.product_id()
    )
}

pub fn connection_from_info(info: &DeviceInfo) -> Connection {
    Connection::from_bus(info.bus_type()).unwrap_or(Connection::Usb)
}

/// Presence-only status (no device open / no report reads).
pub fn status_from_info(index: usize, info: &DeviceInfo) -> ControllerStatus {
    let connection = connection_from_info(info);
    ControllerStatus {
        index,
        kind: ControllerKind::Unknown,
        product: product_label(info),
        connection,
        serial: identity_from_info(info),
        percent: 0,
        state: PowerState::Unknown,
        supports_lightbar: false,
        supports_power_off: false,
        supports_battery: false,
        supports_input: false,
    }
}

pub fn synthetic_battery(info: &DeviceInfo) -> BatteryReading {
    BatteryReading {
        percent: 0,
        state: PowerState::Unknown,
        connection: connection_from_info(info),
    }
}

pub fn resting_sample() -> PadSample {
    PadSample {
        held: BTreeSet::new(),
        stick_y: 0.0,
        dpad_up: false,
        dpad_down: false,
        cross: false,
        circle: false,
        square: false,
        triangle: false,
        options: false,
        l2: false,
        r2: false,
    }
}

/// Registry-backed Unknown driver (no HID protocol I/O).
pub struct UnknownDriver;

impl UnknownDriver {
    pub fn identity(&self, info: &DeviceInfo, _device: &HidDevice) -> String {
        identity_from_info(info)
    }
}
