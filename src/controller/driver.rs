//! DualSense device matching helpers (no multi-family registry).
//!
//! Registry stub direction: if more families arrive, grow this module into a
//! family registry; for now callers that need battery / input / lightbar go
//! through [`crate::controller::dualsense`] directly.

use crate::controller::dualsense::identity;
use hidapi::DeviceInfo;

/// True when this HID node is a DualSense gamepad interface.
pub fn is_gamepad(info: &DeviceInfo) -> bool {
    identity::is_dualsense_gamepad(info)
}
