//! Hardware-neutral controller status types shared by drivers and UI.

use serde::{Deserialize, Serialize};

/// Family of gamepad this status describes.
///
/// Single variant today; kept as an enum (not a unit struct) so future
/// families slot in without churning every constructor and match site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ControllerKind {
    #[default]
    DualSense,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerStatus {
    pub index: usize,
    pub kind: ControllerKind,
    pub product: String,
    pub connection: Connection,
    pub serial: Serial,
    pub percent: u8,
    pub state: PowerState,
    pub supports_lightbar: bool,
    pub supports_power_off: bool,
}

/// Normalized DualSense identity (MAC-style serial, lowercase, no separators).
///
/// `#[serde(transparent)]` keeps the on-disk JSON shape byte-compatible:
/// `"serial": "abc123"` serializes exactly like the old `String` field, and
/// map keys stay plain strings (`{"abc123": {...}}`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct Serial(String);

impl Serial {
    /// Normalize a raw HID serial so `AA:BB:…` and `aabb…` map to one pad.
    ///
    /// Single implementation of the strip-separators + lowercase rule;
    /// [`crate::controller::dualsense::identity::normalize_identity`] delegates here.
    pub fn normalize(serial: &str) -> Self {
        Self(
            serial
                .chars()
                .filter(|c| *c != ':' && *c != '-')
                .flat_map(|c| c.to_lowercase())
                .collect(),
        )
    }

    /// True when the serial names a real pad (not empty / `"unknown"`).
    ///
    /// Single implementation of the storability rule; the `identity` helper delegates here.
    pub fn is_storable(&self) -> bool {
        !self.0.is_empty() && self.0 != "unknown"
    }

    /// Unwrap back to the raw string (for the `identity` delegation).
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Display for Serial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Serial {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for Serial {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Serial {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for Serial {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl ControllerStatus {
    /// Low battery while discharging (toast, orange pulse, popup label).
    pub fn is_low_battery(&self, threshold: u8) -> bool {
        self.percent <= threshold && self.state.is_discharging()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn serial_normalize_strips_separators_and_lowercase() {
        assert_eq!(
            Serial::normalize("AA:BB:CC:DD:EE:FF"),
            Serial::normalize("aa-bb-cc-dd-ee-ff")
        );
        assert_eq!(Serial::normalize("AA:BB").as_ref(), "aabb");
        // Already-normal values are untouched (idempotent).
        assert_eq!(Serial::normalize("aabbccddeeff").as_ref(), "aabbccddeeff");
    }

    #[test]
    fn serial_is_storable_rejects_empty_and_unknown() {
        assert!(!Serial::from("").is_storable());
        assert!(!Serial::from("unknown").is_storable());
        assert!(Serial::from("aabbccddeeff").is_storable());
    }

    #[test]
    fn serial_string_conversions_round_trip() {
        let serial = Serial::from("abc123");
        assert_eq!(serial.as_ref(), "abc123");
        assert_eq!(serial.to_string(), "abc123");
        assert_eq!(String::from("abc123"), "abc123");
        let owned = String::from("abc123");
        assert_eq!(Serial::from(owned.clone()), Serial::from("abc123"));
        assert_eq!(Serial::from(owned).to_string(), "abc123");
        assert_eq!(format!("{serial}"), "abc123");
    }

    #[test]
    fn serial_map_lookup_accepts_plain_str() {
        let mut map = HashMap::new();
        map.insert(Serial::from("abc"), 1u8);
        // `Borrow<str>` keeps `&str` lookups working after the migration.
        assert_eq!(map.get("abc"), Some(&1));
        assert!(map.contains_key("abc"));
        assert_eq!(map["abc"], 1);
    }

    #[test]
    fn serial_json_shape_stays_a_string() {
        let serial = Serial::from("abc123");
        let value = serde_json::to_value(&serial).unwrap();
        assert_eq!(value, serde_json::Value::String("abc123".to_string()));
        let back: Serial = serde_json::from_value(value).unwrap();
        assert_eq!(back, serial);
    }

    #[test]
    fn controller_status_json_serial_stays_a_string() {
        let status = ControllerStatus {
            index: 1,
            kind: ControllerKind::DualSense,
            product: "DualSense".to_string(),
            connection: Connection::Bluetooth,
            serial: Serial::from("abc123"),
            percent: 50,
            state: PowerState::Discharging,
            supports_lightbar: true,
            supports_power_off: true,
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(
            value.get("serial"),
            Some(&serde_json::Value::String("abc123".to_string()))
        );
        // Full round trip: the shape still deserializes.
        let back: ControllerStatus = serde_json::from_value(value).unwrap();
        assert_eq!(back.serial, status.serial);
    }
}
