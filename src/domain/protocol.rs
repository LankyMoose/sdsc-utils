//! DualSense HID report constants shared by battery, lightbar, and input sampling.

/// USB input / gamepad report size.
pub const USB_REPORT_SIZE: usize = 64;
/// Bluetooth full input report size.
pub const BT_REPORT_SIZE: usize = 78;

pub const USB_REPORT_ID: u8 = 0x01;
pub const BT_REPORT_TRUNCATED: u8 = 0x01;
pub const BT_REPORT_FULL: u8 = 0x31;

/// Calibration feature report — requesting it switches BT pads to full reports / effects
/// (same as Linux hid-playstation / SDL enhanced mode).
pub const CALIBRATION_FEATURE_REPORT: u8 = 0x05;
pub const CALIBRATION_FEATURE_SIZE: usize = 41;

/// DualSense Bluetooth control feature report (dualsensectl / HID descriptor).
/// Descriptor Report Count for ID 0x08 is 47 data bytes → 48 with report ID.
pub const BT_CONTROL_FEATURE_REPORT: u8 = 0x08;
pub const BT_CONTROL_FEATURE_SIZE: usize = 48;
/// dualsensectl historically used 47; keep as a Windows fallback size.
pub const BT_CONTROL_FEATURE_SIZE_ALT: usize = 47;
pub const BT_CONTROL_FEATURE_SIZE_PADDED: usize = 64;
