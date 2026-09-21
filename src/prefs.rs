//! Persisted notification preferences.

use crate::app_log;
use crate::battery::LOW_BATTERY_PERCENT;
use crate::color::BatterySpectrum;
use crate::gesture::{GestureControl, default_gesture};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Inclusive bounds for the low-battery threshold slider (DualSense mid-points).
pub const LOW_BATTERY_PERCENT_MIN: u8 = 5;
pub const LOW_BATTERY_PERCENT_MAX: u8 = 35;

/// DualSense-observable mid-points within the low-battery threshold range.
const LOW_BATTERY_OBSERVABLE: [u8; 4] = [5, 15, 25, 35];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToastPosition {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    #[default]
    BottomCenter,
    BottomRight,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prefs {
    #[serde(default = "default_true")]
    pub notify_low: bool,
    #[serde(default = "default_true")]
    pub notify_charged: bool,
    #[serde(default = "default_true")]
    pub notify_connect: bool,
    #[serde(default = "default_true")]
    pub notify_disconnect: bool,
    #[serde(default = "default_low_battery_percent")]
    pub low_battery_percent: u8,
    #[serde(default)]
    pub toast_position: ToastPosition,
    #[serde(default)]
    pub spectrum: BatterySpectrum,
    /// Opt-in local charge/play duration analytics (default off).
    #[serde(default)]
    pub analytics_enabled: bool,
    /// Drive DualSense lightbar from battery spectrum (default on).
    #[serde(default = "default_true")]
    pub lightbar_enabled: bool,
    /// Open the start-screen launcher on 0→1 connect / reopen gesture (default on).
    #[serde(default = "default_true")]
    pub start_screen_enabled: bool,
    /// Chord that reopens the start screen while a pad is connected.
    /// Empty = no gesture reopen (0→1 auto-open still works). Missing key → default.
    #[serde(default = "default_gesture")]
    pub start_screen_gesture: Vec<GestureControl>,
}

fn default_true() -> bool {
    true
}

fn default_low_battery_percent() -> u8 {
    LOW_BATTERY_PERCENT
}

pub fn clamp_low_battery_percent(value: u8) -> u8 {
    let clamped = value.clamp(LOW_BATTERY_PERCENT_MIN, LOW_BATTERY_PERCENT_MAX);
    // Floor to the greatest observable mid-point ≤ clamped (preserves fire points
    // for legacy prefs like 10/20/30/40 that DualSense never reports).
    LOW_BATTERY_OBSERVABLE
        .iter()
        .copied()
        .rev()
        .find(|&p| p <= clamped)
        .unwrap_or(LOW_BATTERY_PERCENT_MIN)
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            notify_low: true,
            notify_charged: true,
            notify_connect: true,
            notify_disconnect: true,
            low_battery_percent: LOW_BATTERY_PERCENT,
            toast_position: ToastPosition::default(),
            spectrum: BatterySpectrum::default_spectrum(),
            analytics_enabled: false,
            lightbar_enabled: true,
            start_screen_enabled: true,
            start_screen_gesture: default_gesture(),
        }
    }
}

impl Prefs {
    pub fn load() -> Self {
        let path = prefs_path();
        let Ok(bytes) = fs::read(&path) else {
            return Self::default();
        };
        match serde_json::from_slice::<Prefs>(&bytes) {
            Ok(mut prefs) => {
                prefs.low_battery_percent = clamp_low_battery_percent(prefs.low_battery_percent);
                prefs
            }
            Err(err) => {
                app_log::warn(format!(
                    "failed to parse prefs at {}: {err}; using defaults",
                    path.display()
                ));
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        let path = prefs_path();
        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            app_log::warn(format!("failed to create prefs dir: {err}"));
            return;
        }
        match serde_json::to_vec_pretty(self) {
            Ok(bytes) => {
                if let Err(err) = fs::write(&path, bytes) {
                    app_log::warn(format!("failed to write prefs: {err}"));
                }
            }
            Err(err) => app_log::warn(format!("failed to serialize prefs: {err}")),
        }
    }
}

fn prefs_path() -> PathBuf {
    crate::paths::data_dir().join("prefs.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_prefs_default_to_bottom_center() {
        let prefs: Prefs = serde_json::from_str(
            r#"{"notify_low":true,"notify_charged":true,"notify_connect":true}"#,
        )
        .unwrap();
        assert_eq!(prefs.toast_position, ToastPosition::BottomCenter);
        assert!(prefs.notify_disconnect);
        assert_eq!(prefs.low_battery_percent, LOW_BATTERY_PERCENT);
        assert!(!prefs.analytics_enabled);
        assert!(prefs.lightbar_enabled);
        assert!(prefs.start_screen_enabled);
        assert_eq!(prefs.start_screen_gesture, default_gesture());
    }

    #[test]
    fn older_prefs_default_analytics_off() {
        let prefs: Prefs = serde_json::from_str(
            r#"{"notify_low":true,"notify_charged":true,"notify_connect":true,"notify_disconnect":true}"#,
        )
        .unwrap();
        assert!(!prefs.analytics_enabled);
        assert!(prefs.lightbar_enabled);
    }

    #[test]
    fn older_prefs_default_start_screen_on_with_default_gesture() {
        let prefs: Prefs = serde_json::from_str(
            r#"{"notify_low":true,"notify_charged":true,"notify_connect":true,"notify_disconnect":true}"#,
        )
        .unwrap();
        assert!(prefs.start_screen_enabled);
        assert_eq!(prefs.start_screen_gesture, default_gesture());
    }

    #[test]
    fn empty_gesture_list_deserializes() {
        let prefs: Prefs =
            serde_json::from_str(r#"{"start_screen_enabled":true,"start_screen_gesture":[]}"#)
                .unwrap();
        assert!(prefs.start_screen_gesture.is_empty());
    }

    #[test]
    fn clamp_low_battery_percent_bounds() {
        assert_eq!(clamp_low_battery_percent(0), LOW_BATTERY_PERCENT_MIN);
        assert_eq!(clamp_low_battery_percent(5), 5);
        assert_eq!(clamp_low_battery_percent(10), 5);
        assert_eq!(clamp_low_battery_percent(15), 15);
        assert_eq!(clamp_low_battery_percent(20), 15);
        assert_eq!(clamp_low_battery_percent(25), 25);
        assert_eq!(clamp_low_battery_percent(35), 35);
        assert_eq!(clamp_low_battery_percent(45), LOW_BATTERY_PERCENT_MAX);
        assert_eq!(clamp_low_battery_percent(50), LOW_BATTERY_PERCENT_MAX);
        assert_eq!(clamp_low_battery_percent(99), LOW_BATTERY_PERCENT_MAX);
    }

    #[test]
    fn legacy_spectrum_fields_migrate_on_load() {
        let prefs: Prefs = serde_json::from_str(
            r#"{"notify_low":true,"notify_charged":true,"notify_connect":true,"spectrum":{"full":{"r":1,"g":2,"b":3},"mid":{"r":4,"g":5,"b":6},"empty":{"r":7,"g":8,"b":9}}}"#,
        )
        .unwrap();
        assert_eq!(prefs.spectrum.stops.len(), 3);
        assert_eq!(prefs.spectrum.stops[0].percent, 100);
        let encoded = serde_json::to_value(&prefs.spectrum).unwrap();
        assert!(encoded.get("stops").is_some());
        assert!(encoded.get("full").is_none());
    }

    #[test]
    fn toast_position_uses_stable_snake_case_names() {
        assert_eq!(
            serde_json::to_string(&ToastPosition::BottomLeft).unwrap(),
            r#""bottom_left""#
        );
        assert_eq!(
            serde_json::to_string(&ToastPosition::TopCenter).unwrap(),
            r#""top_center""#
        );
        assert_eq!(
            serde_json::to_string(&ToastPosition::BottomCenter).unwrap(),
            r#""bottom_center""#
        );
    }
}
