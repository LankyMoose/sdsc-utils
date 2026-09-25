//! Controller driver traits and the in-process registry.

use crate::controller::dualsense::battery::BatteryReading;
use crate::controller::model::{Connection, ControllerKind};
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
    fn identity(&self, info: &DeviceInfo, device: &HidDevice) -> String;
    fn product_name(&self, product_id: u16) -> &'static str;
    fn read_battery(&self, device: &HidDevice) -> Result<BatteryReading, String>;
    fn parse_input(&self, buf: &[u8], bus: Connection) -> PadSample;
}

pub trait LightbarControl: ControllerDriver {
    #[allow(dead_code)]
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

/// DualSense registration + lookup helpers.
pub struct DriverRegistry {
    dualsense: DualSenseDriver,
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

    fn identity(&self, info: &DeviceInfo, device: &HidDevice) -> String {
        crate::controller::dualsense::identity::resolve_device_identity(info, device)
    }

    fn product_name(&self, product_id: u16) -> &'static str {
        crate::controller::dualsense::identity::product_name(product_id)
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

impl DriverRegistry {
    fn new() -> Self {
        Self {
            dualsense: DualSenseDriver,
        }
    }

    /// First registered driver that matches this HID node (any interface).
    #[allow(dead_code)] // used when adding non-gamepad DualSense interfaces / future pads
    pub fn for_device(&self, info: &DeviceInfo) -> Option<&dyn ControllerDriver> {
        if self.dualsense.matches(info) {
            Some(&self.dualsense)
        } else {
            None
        }
    }

    /// First registered driver that matches the gamepad interface.
    pub fn for_gamepad(&self, info: &DeviceInfo) -> Option<&dyn ControllerDriver> {
        if self.dualsense.matches_gamepad(info) {
            Some(&self.dualsense)
        } else {
            None
        }
    }

    #[allow(dead_code)] // lightbar CLI / exclusive writes will route here as more drivers land
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
        }
    }

    pub fn driver_for_kind(&self, kind: ControllerKind) -> Option<&dyn ControllerDriver> {
        match kind {
            ControllerKind::DualSense => Some(&self.dualsense),
        }
    }
}

static REGISTRY: OnceLock<DriverRegistry> = OnceLock::new();

pub fn registry() -> &'static DriverRegistry {
    REGISTRY.get_or_init(DriverRegistry::new)
}
