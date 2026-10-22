//! Multi-controller driver seam: the only dispatch point for pad families.
//!
//! A fork adds a family by implementing [`ControllerDriver`] (kind, match,
//! product name, battery, input, capabilities, optional backends, power-off)
//! and registering it in [`DRIVERS`]; UI, session, persist, and the DualSense
//! implementation stay untouched. DualSense is the only driver today.
//!
//! The shared model takes plain data ([`BatteryReading`], [`PowerState`]);
//! wire encodings (report IDs, CRC seeds, nibbles, `LIGHT_OUT`) stay
//! DualSense-private behind the impl.

use crate::controller::dualsense::{battery, identity, input, lightbar, rumble};
use crate::controller::model::{Capabilities, ControllerKind};
use crate::domain::color::Rgb;
use crate::domain::pad::PadSample;
use crate::{AppError, AppResult};
use hidapi::{DeviceInfo, HidApi, HidDevice};

/// Wire write for a family's LED bar (`None` = the family lacks one).
///
/// The shared claim unit (`ClaimStore`) is threaded through like `HidIoLock`:
/// one owned instance shared by the worker and the app root. Only the byte
/// layout dispatched here is per-family.
pub trait LightbarBackend: Send + Sync {
    fn apply(
        &self,
        api: &HidApi,
        serial: &str,
        color: Rgb,
        claims: &lightbar::ClaimStore,
    ) -> (AppResult<()>, lightbar::HidPhaseTiming);
}

/// Wire write for a family's motors (`None` = the family lacks them).
pub trait RumbleBackend: Send + Sync {
    fn set_rumble(
        &self,
        device: &HidDevice,
        is_bluetooth: bool,
        serial: &str,
        right: u8,
        left: u8,
        claims: &lightbar::ClaimStore,
    ) -> Result<(), hidapi::HidError>;

    fn stop_rumble(
        &self,
        device: &HidDevice,
        is_bluetooth: bool,
        serial: &str,
        claims: &lightbar::ClaimStore,
    ) -> Result<(), hidapi::HidError> {
        self.set_rumble(device, is_bluetooth, serial, 0, 0, claims)
    }
}

pub trait ControllerDriver: Send + Sync {
    fn kind(&self) -> ControllerKind;
    /// True when this HID node is this family's gamepad interface (VID/PID + usage).
    fn matches(&self, info: &DeviceInfo) -> bool;
    fn product_name(&self, product_id: u16) -> &'static str;
    fn read_battery(&self, device: &HidDevice) -> AppResult<battery::BatteryReading>;
    /// Fail-fast variant for known pads on liveness ticks (fewer waits for a
    /// streaming input report). New/unknown pads get the full attempts.
    fn read_battery_fast(&self, device: &HidDevice) -> AppResult<battery::BatteryReading>;
    fn parse_input(&self, buf: &[u8], base: usize) -> PadSample;
    fn capabilities(&self) -> Capabilities;
    /// Optional backends; `None` = the family lacks the hardware. Callers
    /// branch `if let Some(backend)`, never on bool flags or family name.
    fn lightbar(&self) -> Option<&dyn LightbarBackend>;
    fn rumble(&self) -> Option<&dyn RumbleBackend>;
    /// Power off over `api` (Bluetooth-only for DualSense). Returns phase
    /// timing for hid-worker metrics alongside the result.
    fn power_off(&self, api: &HidApi, serial: &str) -> (AppResult<()>, lightbar::HidPhaseTiming);
}

/// DualSense driver: delegates every fn to the existing `dualsense::` free
/// fns (report bytes, CRC, `LIGHT_OUT` stay private to those files).
pub struct DualSenseDriver;

/// Shared driver instance; the sole entry of [`DRIVERS`].
pub static DUALSENSE: DualSenseDriver = DualSenseDriver;

/// Family registry. Poll and the HID worker dispatch through this slice
/// (tests inject fakes); [`is_gamepad`] is the any-match over it.
pub static DRIVERS: &[&dyn ControllerDriver] = &[&DUALSENSE];

impl ControllerDriver for DualSenseDriver {
    fn kind(&self) -> ControllerKind {
        ControllerKind::DualSense
    }

    fn matches(&self, info: &DeviceInfo) -> bool {
        identity::is_dualsense_gamepad(info)
    }

    fn product_name(&self, product_id: u16) -> &'static str {
        identity::product_name(product_id)
    }

    fn read_battery(&self, device: &HidDevice) -> AppResult<battery::BatteryReading> {
        battery::read_battery(device).map_err(AppError::from)
    }

    fn read_battery_fast(&self, device: &HidDevice) -> AppResult<battery::BatteryReading> {
        battery::read_battery_fast(device).map_err(AppError::from)
    }

    fn parse_input(&self, buf: &[u8], base: usize) -> PadSample {
        input::parse_report(buf, base)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::FULL
    }

    fn lightbar(&self) -> Option<&dyn LightbarBackend> {
        Some(&lightbar::DUALSENSE_LIGHTBAR)
    }

    fn rumble(&self) -> Option<&dyn RumbleBackend> {
        Some(&rumble::DUALSENSE_RUMBLE)
    }

    fn power_off(&self, api: &HidApi, serial: &str) -> (AppResult<()>, lightbar::HidPhaseTiming) {
        battery::power_off_bluetooth_timed(api, serial)
    }
}

/// True when any registered driver claims this HID node as a gamepad.
pub fn is_gamepad(info: &DeviceInfo) -> bool {
    DRIVERS.iter().any(|d| d.matches(info))
}

/// Driver claiming this HID node, if any (registry order wins).
pub fn find_for_info(info: &DeviceInfo) -> Option<&'static dyn ControllerDriver> {
    DRIVERS.iter().copied().find(|d| d.matches(info))
}

/// Driver for a known pad kind, if registered.
pub fn find_for_kind(kind: ControllerKind) -> Option<&'static dyn ControllerDriver> {
    DRIVERS.iter().copied().find(|d| d.kind() == kind)
}

#[cfg(test)]
pub(crate) struct FakeDriver {
    pub percent: u8,
    pub state: crate::controller::model::PowerState,
    pub connection: crate::controller::model::Connection,
    pub cross: bool,
}

#[cfg(test)]
impl FakeDriver {
    pub(crate) fn canned_reading(&self) -> battery::BatteryReading {
        battery::BatteryReading {
            percent: self.percent,
            state: self.state,
            connection: self.connection,
        }
    }
}

/// Test double: canned battery/input, [`Capabilities::NONE`], no backends.
/// Never touches HID (its `HidDevice`/`HidApi` args are ignored), so
/// `hid::` tests can prove dispatch + capability gating with no hardware.
#[cfg(test)]
impl ControllerDriver for FakeDriver {
    fn kind(&self) -> ControllerKind {
        ControllerKind::DualSense
    }

    fn matches(&self, _info: &DeviceInfo) -> bool {
        false
    }

    fn product_name(&self, _product_id: u16) -> &'static str {
        "FakePad"
    }

    fn read_battery(&self, _device: &HidDevice) -> AppResult<battery::BatteryReading> {
        Ok(self.canned_reading())
    }

    fn read_battery_fast(&self, _device: &HidDevice) -> AppResult<battery::BatteryReading> {
        Ok(self.canned_reading())
    }

    fn parse_input(&self, _buf: &[u8], _base: usize) -> PadSample {
        PadSample {
            cross: self.cross,
            ..PadSample::default()
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::NONE
    }

    fn lightbar(&self) -> Option<&dyn LightbarBackend> {
        None
    }

    fn rumble(&self) -> Option<&dyn RumbleBackend> {
        None
    }

    fn power_off(&self, _api: &HidApi, _serial: &str) -> (AppResult<()>, lightbar::HidPhaseTiming) {
        (
            Err(AppError::Hid("fake: power-off unsupported".into())),
            lightbar::HidPhaseTiming::default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::model::PowerState;

    #[test]
    fn dualsense_driver_reports_full_capabilities_and_backends() {
        assert_eq!(DUALSENSE.kind(), ControllerKind::DualSense);
        assert_eq!(DUALSENSE.capabilities(), Capabilities::FULL);
        assert!(DUALSENSE.lightbar().is_some());
        assert!(DUALSENSE.rumble().is_some());
        assert_eq!(DUALSENSE.product_name(0x0CE6), "DualSense");
        assert_eq!(DUALSENSE.product_name(0x0DF2), "DualSense Edge");
    }

    #[test]
    fn dualsense_parse_input_delegates_to_report_parser() {
        let mut buf = vec![0u8; 64];
        buf[0] = 0x01;
        // D-pad neutral (0x08) + Cross (0x20).
        buf[8] = 0x28;
        let via_driver = DUALSENSE.parse_input(&buf, 0);
        let via_fn = input::parse_report(&buf, 0);
        assert_eq!(via_driver.cross, via_fn.cross);
        assert!(via_driver.cross);
        assert!(!via_driver.circle);
    }

    #[test]
    fn registry_holds_dualsense_only() {
        assert_eq!(DRIVERS.len(), 1);
        assert_eq!(DRIVERS[0].kind(), ControllerKind::DualSense);
        assert_eq!(
            find_for_kind(ControllerKind::DualSense).unwrap().kind(),
            ControllerKind::DualSense
        );
    }

    #[test]
    fn fake_driver_reports_no_capabilities_or_backends() {
        let fake = FakeDriver {
            percent: 55,
            state: PowerState::Discharging,
            connection: crate::controller::model::Connection::Bluetooth,
            cross: true,
        };
        assert_eq!(fake.capabilities(), Capabilities::NONE);
        assert!(fake.lightbar().is_none());
        assert!(fake.rumble().is_none());
        let reading = fake.canned_reading();
        assert_eq!(reading.percent, 55);
        assert!(reading.connection.is_bluetooth());
        assert!(fake.parse_input(&[0u8; 8], 0).cross);
    }
}
