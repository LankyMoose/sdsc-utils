//! Controller driver traits and the in-process registry.

use crate::controller::model::{BatteryReading, Connection, ControllerKind};
use crate::controller::unknown::{self, UnknownDriver};
use crate::ui::color::Rgb;
use crate::ui::start::input::PadSample;
use hidapi::{DeviceInfo, HidApi, HidDevice};
use std::sync::OnceLock;

/// Core HID family operations shared by every registered controller.
pub trait ControllerDriver: Send + Sync {
    fn kind(&self) -> ControllerKind;
    fn matches(&self, info: &DeviceInfo) -> bool;
    /// Gamepad usage interface used for battery / input (not vendor-only nodes).
    fn matches_gamepad(&self, info: &DeviceInfo) -> bool;
    /// Identity from enumerate info only (no open / no I/O).
    fn identity_from_info(&self, info: &DeviceInfo) -> String;
    fn identity(&self, info: &DeviceInfo, device: &HidDevice) -> String;
    fn product_name(&self, info: &DeviceInfo) -> String;
    fn supports_input(&self) -> bool;
    fn supports_battery(&self) -> bool;
    fn read_battery(&self, device: &HidDevice) -> Result<BatteryReading, String>;
    fn parse_input(&self, buf: &[u8], bus: Connection) -> PadSample;
}

pub trait LightbarControl: ControllerDriver {
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    fn apply(
        &self,
        device: &HidDevice,
        color: Rgb,
        bus: Connection,
        claim: bool,
        serial: &str,
        caller: &str,
        retry: bool,
    ) -> Result<(), String>;
}

pub trait PowerOff: ControllerDriver {
    #[allow(dead_code)]
    fn power_off_bluetooth(&self, api: &HidApi, serial: &str) -> Result<(), String>;
}

/// DualSense + Unknown registration and lookup.
pub struct DriverRegistry {
    dualsense: DualSenseDriver,
    unknown: UnknownDriver,
}

struct DualSenseDriver;

impl ControllerDriver for DualSenseDriver {
    fn kind(&self) -> ControllerKind {
        ControllerKind::DualSense
    }

    fn matches(&self, info: &DeviceInfo) -> bool {
        crate::controller::dualsense::identity::is_dualsense_device(info)
    }

    fn matches_gamepad(&self, info: &DeviceInfo) -> bool {
        crate::controller::dualsense::identity::is_dualsense_gamepad(info)
    }

    fn identity_from_info(&self, info: &DeviceInfo) -> String {
        let hid = crate::controller::dualsense::identity::hid_serial(info);
        if crate::controller::dualsense::identity::is_storable_serial(&hid) {
            crate::controller::dualsense::identity::normalize_identity(&hid)
        } else {
            "unknown".into()
        }
    }

    fn identity(&self, info: &DeviceInfo, device: &HidDevice) -> String {
        crate::controller::dualsense::identity::resolve_device_identity(info, device)
    }

    fn product_name(&self, info: &DeviceInfo) -> String {
        crate::controller::dualsense::identity::product_name(info.product_id()).to_string()
    }

    fn supports_input(&self) -> bool {
        true
    }

    fn supports_battery(&self) -> bool {
        true
    }

    fn read_battery(&self, device: &HidDevice) -> Result<BatteryReading, String> {
        crate::controller::dualsense::battery::read_battery(device).map_err(|e| e.to_string())
    }

    fn parse_input(&self, buf: &[u8], bus: Connection) -> PadSample {
        let base = match bus {
            Connection::Usb => 0,
            Connection::Bluetooth => 1,
        };
        crate::controller::dualsense::input::parse_report(buf, base)
    }
}

#[allow(dead_code)] // seam for exclusive lightbar writes once more families register
impl LightbarControl for DualSenseDriver {
    fn apply(
        &self,
        device: &HidDevice,
        color: Rgb,
        bus: Connection,
        claim: bool,
        serial: &str,
        caller: &str,
        retry: bool,
    ) -> Result<(), String> {
        crate::controller::dualsense::lightbar::set_lightbar_on_device(
            device,
            color,
            bus.is_bluetooth(),
            claim,
            serial,
            caller,
            retry,
        )
        .map_err(|e| e.to_string())
    }
}

#[allow(dead_code)] // seam for worker/CLI power-off via registry
impl PowerOff for DualSenseDriver {
    fn power_off_bluetooth(&self, api: &HidApi, serial: &str) -> Result<(), String> {
        let (result, _timing) =
            crate::controller::dualsense::battery::power_off_bluetooth_timed(api, serial);
        result
    }
}

impl ControllerDriver for UnknownDriver {
    fn kind(&self) -> ControllerKind {
        ControllerKind::Unknown
    }

    fn matches(&self, info: &DeviceInfo) -> bool {
        unknown::is_unknown_gamepad(info)
    }

    fn matches_gamepad(&self, info: &DeviceInfo) -> bool {
        unknown::is_unknown_gamepad(info)
    }

    fn identity_from_info(&self, info: &DeviceInfo) -> String {
        unknown::identity_from_info(info)
    }

    fn identity(&self, info: &DeviceInfo, device: &HidDevice) -> String {
        UnknownDriver::identity(self, info, device)
    }

    fn product_name(&self, info: &DeviceInfo) -> String {
        unknown::product_label(info)
    }

    fn supports_input(&self) -> bool {
        false
    }

    fn supports_battery(&self) -> bool {
        false
    }

    fn read_battery(&self, device: &HidDevice) -> Result<BatteryReading, String> {
        // Never issued by poll/worker for Unknown; synthetic from enumerate info only.
        let info = device.get_device_info().map_err(|e| e.to_string())?;
        Ok(unknown::synthetic_battery(&info))
    }

    fn parse_input(&self, _buf: &[u8], _bus: Connection) -> PadSample {
        unknown::resting_sample()
    }
}

impl DriverRegistry {
    fn new() -> Self {
        Self {
            dualsense: DualSenseDriver,
            unknown: UnknownDriver,
        }
    }

    /// First registered driver that matches this HID node (any interface).
    #[allow(dead_code)]
    pub fn for_device(&self, info: &DeviceInfo) -> Option<&dyn ControllerDriver> {
        if self.dualsense.matches(info) {
            Some(&self.dualsense)
        } else if self.unknown.matches(info) {
            Some(&self.unknown)
        } else {
            None
        }
    }

    /// First registered driver that matches the gamepad interface (DualSense wins).
    pub fn for_gamepad(&self, info: &DeviceInfo) -> Option<&dyn ControllerDriver> {
        if self.dualsense.matches_gamepad(info) {
            Some(&self.dualsense)
        } else if self.unknown.matches_gamepad(info) {
            Some(&self.unknown)
        } else {
            None
        }
    }

    #[allow(dead_code)]
    pub fn lightbar_for(&self, info: &DeviceInfo) -> Option<&dyn LightbarControl> {
        if self.dualsense.matches_gamepad(info) || self.dualsense.matches(info) {
            Some(&self.dualsense)
        } else {
            None
        }
    }

    #[allow(dead_code)]
    pub fn power_off_for_kind(&self, kind: ControllerKind) -> Option<&dyn PowerOff> {
        match kind {
            ControllerKind::DualSense => Some(&self.dualsense),
            ControllerKind::Unknown => None,
        }
    }

    pub fn driver_for_kind(&self, kind: ControllerKind) -> Option<&dyn ControllerDriver> {
        match kind {
            ControllerKind::DualSense => Some(&self.dualsense),
            ControllerKind::Unknown => Some(&self.unknown),
        }
    }
}

static REGISTRY: OnceLock<DriverRegistry> = OnceLock::new();

pub fn registry() -> &'static DriverRegistry {
    REGISTRY.get_or_init(DriverRegistry::new)
}
