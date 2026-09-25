//! Hardware-neutral controller status types shared by drivers and UI.

use serde::{Deserialize, Serialize};

/// Family of gamepad this status describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ControllerKind {
    #[default]
    DualSense,
}

impl ControllerKind {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DualSense => "DualSense",
        }
    }
}

/// How the pad is attached to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Connection {
    Usb,
    Bluetooth,
}

impl Connection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Usb => "USB",
            Self::Bluetooth => "Bluetooth",
        }
    }

    pub fn is_usb(self) -> bool {
        matches!(self, Self::Usb)
    }

    pub fn is_bluetooth(self) -> bool {
        matches!(self, Self::Bluetooth)
    }

    #[allow(dead_code)]
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "USB" | "usb" => Some(Self::Usb),
            "Bluetooth" | "bluetooth" => Some(Self::Bluetooth),
            _ => None,
        }
    }
}

impl std::fmt::Display for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// DualSense-style coarse battery state (also used as the shared UI model).
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
    pub kind: ControllerKind,
    pub product: &'static str,
    pub connection: Connection,
    pub serial: String,
    pub percent: u8,
    pub state: PowerState,
    pub supports_lightbar: bool,
    pub supports_power_off: bool,
}

impl ControllerStatus {
    /// Low battery while discharging (toast, orange pulse, popup label).
    pub fn is_low_battery(&self, threshold: u8) -> bool {
        self.percent <= threshold && self.state.is_discharging()
    }
}
