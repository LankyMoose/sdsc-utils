//! DualSense compatible-vibration (rumble) HID output for start-screen cues.
//!
//! Uses a dedicated long-lived output handle (see hid-worker). Does **not** go
//! through lightbar's exclusive open-write-close path, and never requests the
//! Bluetooth calibration feature report.

use crate::controller::dualsense::identity::normalize_identity;
use crate::controller::dualsense::lightbar;
use hidapi::HidDevice;
use std::time::Duration;

/// Offsets into the DualSense common output payload (after report ID / BT header).
const OFF_VALID_FLAG0: usize = 0;
const OFF_MOTOR_RIGHT: usize = 2;
const OFF_MOTOR_LEFT: usize = 3;
const OFF_VALID_FLAG2: usize = 38;

const OUTPUT_VALID_FLAG0_COMPATIBLE_VIBRATION: u8 = 1 << 0;
/// Select classic/emulated rumble over audio haptics (Linux/SDL `HAPTICS_SELECT`).
const OUTPUT_VALID_FLAG0_HAPTICS_SELECT: u8 = 1 << 1;
const OUTPUT_VALID_FLAG2_COMPATIBLE_VIBRATION2: u8 = 1 << 2;

/// Pulse lengths for each cue kind (motors on, then a stop report).
pub const NAV_MS: u64 = 50;
pub const ACTION_MS: u64 = 55;
pub const HOLD_MS: u64 = 90;
pub const SLIDE_MS: u64 = 50;

/// Full-scale motor amplitudes (strength 100%). Scaled by the prefs slider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotorPulse {
    pub right: u8,
    pub left: u8,
    pub duration: Duration,
}

impl MotorPulse {
    pub const NAV: Self = Self {
        right: 80,
        left: 48,
        duration: Duration::from_millis(NAV_MS),
    };
    pub const ACTION: Self = Self {
        right: 140,
        left: 96,
        duration: Duration::from_millis(ACTION_MS),
    };
    pub const HOLD: Self = Self {
        right: 110,
        left: 110,
        duration: Duration::from_millis(HOLD_MS),
    };
    pub const SLIDE: Self = Self {
        right: 72,
        left: 110,
        duration: Duration::from_millis(SLIDE_MS),
    };

    /// Scale motors by `strength` in 0..=100. Strength 0 yields zeros.
    pub fn scaled(self, strength: u8) -> (u8, u8, Duration) {
        let strength = strength.min(100);
        let scale = |base: u8| -> u8 { ((u16::from(base) * u16::from(strength)) / 100) as u8 };
        (scale(self.right), scale(self.left), self.duration)
    }
}

/// Write compatible-vibration motors on an already-open device.
pub fn set_rumble_on_device(
    device: &HidDevice,
    is_bluetooth: bool,
    serial: &str,
    right: u8,
    left: u8,
) -> Result<(), hidapi::HidError> {
    let target = normalize_identity(serial);
    lightbar::write_output_report(
        device,
        is_bluetooth,
        &target,
        "rumble",
        "motors",
        None,
        false,
        false,
        |common| {
            // Linux hid-playstation / SDL: always set HAPTICS_SELECT when driving
            // motors; use both v1 (flag0 bit0) and v2 (flag2 bit2) so FW ≥2.21 and
            // older DualSense firmware accept the pulse.
            common[OFF_VALID_FLAG0] =
                OUTPUT_VALID_FLAG0_COMPATIBLE_VIBRATION | OUTPUT_VALID_FLAG0_HAPTICS_SELECT;
            common[OFF_VALID_FLAG2] = OUTPUT_VALID_FLAG2_COMPATIBLE_VIBRATION2;
            common[OFF_MOTOR_RIGHT] = right;
            common[OFF_MOTOR_LEFT] = left;
        },
    )
}

/// Stop motors (zero amplitude with vibration valid flags).
pub fn stop_rumble_on_device(
    device: &HidDevice,
    is_bluetooth: bool,
    serial: &str,
) -> Result<(), hidapi::HidError> {
    set_rumble_on_device(device, is_bluetooth, serial, 0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_strength_scales_motors() {
        let (r, l, d) = MotorPulse::NAV.scaled(100);
        assert_eq!((r, l), (80, 48));
        assert_eq!(d, Duration::from_millis(NAV_MS));

        let (r, l, _) = MotorPulse::NAV.scaled(50);
        assert_eq!((r, l), (40, 24));

        let (r, l, _) = MotorPulse::ACTION.scaled(0);
        assert_eq!((r, l), (0, 0));

        let (r, l, _) = MotorPulse::HOLD.scaled(60);
        assert_eq!((r, l), (66, 66));
    }

    #[test]
    fn rumble_common_payload_flags_and_motors() {
        let mut common = [0u8; 63];
        common[OFF_VALID_FLAG0] =
            OUTPUT_VALID_FLAG0_COMPATIBLE_VIBRATION | OUTPUT_VALID_FLAG0_HAPTICS_SELECT;
        common[OFF_VALID_FLAG2] = OUTPUT_VALID_FLAG2_COMPATIBLE_VIBRATION2;
        common[OFF_MOTOR_RIGHT] = 80;
        common[OFF_MOTOR_LEFT] = 48;
        assert_eq!(common[0], 0x03); // vibration + haptics select
        assert_eq!(common[1], 0);
        assert_eq!(common[2], 80);
        assert_eq!(common[3], 48);
        assert_eq!(common[38], 0x04);
        // Lightbar flags/bytes untouched.
        assert_eq!(common[44], 0);
        assert_eq!(common[45], 0);
        assert_eq!(common[46], 0);
    }
}
