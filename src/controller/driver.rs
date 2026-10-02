//! DualSense device matching helpers (no multi-family registry).
//!
//! Callers that need battery / input / lightbar go through
//! [`crate::controller::dualsense`] directly.

use crate::controller::dualsense::identity;
use hidapi::DeviceInfo;

/// True when this HID node is a DualSense gamepad interface.
pub fn is_gamepad(info: &DeviceInfo) -> bool {
    identity::is_dualsense_gamepad(info)
}

/// True when this HID node is any DualSense interface (including vendor-only).
#[allow(dead_code)] // used when scanning non-gamepad DualSense collections
pub fn is_device(info: &DeviceInfo) -> bool {
    identity::is_dualsense_device(info)
}
