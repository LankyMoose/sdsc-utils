//! Persisted notification preferences.

use crate::controller::dualsense::battery::LOW_BATTERY_PERCENT;
use crate::domain::color::BatterySpectrum;
use crate::domain::gesture::{GestureControl, default_gesture};
use crate::platform::app_log;
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

/// Browse-mode ordering for the start-screen games list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GamesSortMode {
    #[default]
    LastPlayed,
    Alphabetical,
}

impl GamesSortMode {
    pub fn cycle(self) -> Self {
        match self {
            Self::LastPlayed => Self::Alphabetical,
            Self::Alphabetical => Self::LastPlayed,
        }
    }
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
    /// Always use immersive Start (never show compact; cancel closes) (default off).
    #[serde(default)]
    pub start_screen_always_immersive: bool,
    /// Immersive idle dim after this many seconds (default 30). Snapped to idle steps.
    #[serde(default = "default_start_screen_inactive_secs")]
    pub start_screen_inactive_secs: u32,
    /// Immersive sleep (full black) after this many seconds (default 120). Snapped to sleep steps.
    #[serde(default = "default_start_screen_sleep_secs")]
    pub start_screen_sleep_secs: u32,
    /// Idle-mode black overlay percent 0–100 (default 50). Sleep is always 100%.
    #[serde(default = "default_start_screen_inactive_dim_percent")]
    pub start_screen_inactive_dim_percent: u8,
    /// Count USB pads for start-screen auto-open / auto-close (default on).
    /// When off, only Bluetooth pads drive open and close.
    #[serde(default = "default_true")]
    pub start_screen_usb_controllers: bool,
    /// Reopen / promote chord while a pad is connected.
    /// Always forced to PS on load/save; custom chords are no longer honored.
    #[serde(default = "default_gesture")]
    pub start_screen_gesture: Vec<GestureControl>,
    /// Play UI cues while navigating the start screen (default on).
    #[serde(default = "default_true")]
    pub start_screen_sounds_enabled: bool,
    /// Master volume for start-screen UI sounds (0–100, default 60).
    #[serde(default = "default_start_screen_sound_volume")]
    pub start_screen_sound_volume: u8,
    /// Play DualSense rumble cues while navigating the start screen (default on).
    #[serde(default = "default_true")]
    pub start_screen_haptics_enabled: bool,
    /// Master strength for start-screen haptic cues (0–100, default 60).
    #[serde(default = "default_start_screen_haptics_strength")]
    pub start_screen_haptics_strength: u8,
    /// Start-screen games list sort (default last played).
    #[serde(default)]
    pub games_sort_mode: GamesSortMode,
    /// Controllers slide: include remembered disconnected pads (default off).
    #[serde(default)]
    pub show_all_controllers: bool,
}

fn default_true() -> bool {
    true
}

fn default_low_battery_percent() -> u8 {
    LOW_BATTERY_PERCENT
}

fn default_start_screen_sound_volume() -> u8 {
    60
}

fn default_start_screen_haptics_strength() -> u8 {
    60
}

fn default_start_screen_inactive_secs() -> u32 {
    30
}

fn default_start_screen_sleep_secs() -> u32 {
    120
}

fn default_start_screen_inactive_dim_percent() -> u8 {
    50
}

/// Allowed immersive idle timeouts (seconds): 5s, 15s, 30s, 1m, 2m, 3m, 5m, 10m.
pub const START_SCREEN_IDLE_SECS_STEPS: &[u32] = &[5, 15, 30, 60, 120, 180, 300, 600];
/// Allowed immersive sleep timeouts (seconds): 1m–5m like idle, then 5m gaps to 60m.
pub const START_SCREEN_SLEEP_SECS_STEPS: &[u32] = &[
    60, 120, 180, 300, 600, 900, 1_200, 1_500, 1_800, 2_100, 2_400, 2_700, 3_000, 3_300, 3_600,
];

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

pub fn clamp_start_screen_sound_volume(value: u8) -> u8 {
    value.min(100)
}

pub fn clamp_start_screen_haptics_strength(value: u8) -> u8 {
    value.min(100)
}

pub fn clamp_start_screen_inactive_dim_percent(value: u8) -> u8 {
    value.min(100)
}

/// Snap `value` to the nearest entry in a sorted ascending step table.
/// Ties prefer the shorter timeout.
pub fn snap_to_secs_steps(value: u32, steps: &[u32]) -> u32 {
    let Some(&first) = steps.first() else {
        return value;
    };
    let mut best = first;
    let mut best_dist = value.abs_diff(first);
    for &step in steps.iter().skip(1) {
        let dist = value.abs_diff(step);
        if dist < best_dist || (dist == best_dist && step < best) {
            best = step;
            best_dist = dist;
        }
    }
    best
}

/// Index of `secs` in `steps`, or nearest step index.
pub fn secs_step_index(secs: u32, steps: &[u32]) -> usize {
    let snapped = snap_to_secs_steps(secs, steps);
    steps.iter().position(|&s| s == snapped).unwrap_or(0)
}

/// Format a timeout for Settings labels (`5 seconds`, `3 minutes`).
pub fn format_timeout_duration(secs: u32) -> String {
    if secs < 60 {
        if secs == 1 {
            "1 second".into()
        } else {
            format!("{secs} seconds")
        }
    } else {
        let mins = secs / 60;
        if mins == 1 {
            "1 minute".into()
        } else {
            format!("{mins} minutes")
        }
    }
}

/// Earliest allowed sleep step that is strictly after `idle_secs` (one breakpoint ahead).
pub fn min_sleep_secs_for_idle(idle_secs: u32) -> u32 {
    let idle = snap_to_secs_steps(idle_secs, START_SCREEN_IDLE_SECS_STEPS);
    START_SCREEN_SLEEP_SECS_STEPS
        .iter()
        .copied()
        .find(|&s| s > idle)
        .unwrap_or(*START_SCREEN_SLEEP_SECS_STEPS.last().unwrap_or(&idle))
}

/// Clamp idle/sleep to allowed steps; sleep is always at least one breakpoint ahead of idle.
pub fn clamp_start_screen_idle_timeouts(idle_secs: u32, sleep_secs: u32) -> (u32, u32) {
    let idle = snap_to_secs_steps(idle_secs, START_SCREEN_IDLE_SECS_STEPS);
    let min_sleep = min_sleep_secs_for_idle(idle);
    let sleep = snap_to_secs_steps(sleep_secs, START_SCREEN_SLEEP_SECS_STEPS).max(min_sleep);
    (idle, sleep)
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
            start_screen_always_immersive: false,
            start_screen_inactive_secs: default_start_screen_inactive_secs(),
            start_screen_sleep_secs: default_start_screen_sleep_secs(),
            start_screen_inactive_dim_percent: default_start_screen_inactive_dim_percent(),
            start_screen_usb_controllers: true,
            start_screen_gesture: default_gesture(),
            start_screen_sounds_enabled: true,
            start_screen_sound_volume: default_start_screen_sound_volume(),
            start_screen_haptics_enabled: true,
            start_screen_haptics_strength: default_start_screen_haptics_strength(),
            games_sort_mode: GamesSortMode::default(),
            show_all_controllers: false,
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
                prefs.normalize();
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
        // Rewrite stale custom / empty chords as PS on disk.
        let mut prefs = self.clone();
        prefs.start_screen_gesture = default_gesture();
        match serde_json::to_vec_pretty(&prefs) {
            Ok(bytes) => {
                if let Err(err) = fs::write(&path, bytes) {
                    app_log::warn(format!("failed to write prefs: {err}"));
                }
            }
            Err(err) => app_log::warn(format!("failed to serialize prefs: {err}")),
        }
    }

    /// Clamp numeric prefs and force the reopen chord to PS.
    fn normalize(&mut self) {
        self.low_battery_percent = clamp_low_battery_percent(self.low_battery_percent);
        self.start_screen_sound_volume =
            clamp_start_screen_sound_volume(self.start_screen_sound_volume);
        self.start_screen_haptics_strength =
            clamp_start_screen_haptics_strength(self.start_screen_haptics_strength);
        self.start_screen_inactive_dim_percent =
            clamp_start_screen_inactive_dim_percent(self.start_screen_inactive_dim_percent);
        let (inactive, sleep) = clamp_start_screen_idle_timeouts(
            self.start_screen_inactive_secs,
            self.start_screen_sleep_secs,
        );
        self.start_screen_inactive_secs = inactive;
        self.start_screen_sleep_secs = sleep;
        self.start_screen_gesture = default_gesture();
    }
}

fn prefs_path() -> PathBuf {
    crate::persist::paths::data_dir().join("prefs.json")
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
    fn empty_gesture_list_normalizes_to_ps() {
        let mut prefs: Prefs =
            serde_json::from_str(r#"{"start_screen_enabled":true,"start_screen_gesture":[]}"#)
                .unwrap();
        assert!(prefs.start_screen_gesture.is_empty());
        prefs.normalize();
        assert_eq!(prefs.start_screen_gesture, default_gesture());
    }

    #[test]
    fn custom_gesture_normalizes_to_ps() {
        let mut prefs: Prefs = serde_json::from_str(
            r#"{"start_screen_enabled":true,"start_screen_gesture":["l2","r2"]}"#,
        )
        .unwrap();
        assert_ne!(prefs.start_screen_gesture, default_gesture());
        prefs.normalize();
        assert_eq!(prefs.start_screen_gesture, default_gesture());
    }

    #[test]
    fn older_prefs_default_start_screen_sounds_on() {
        let prefs: Prefs =
            serde_json::from_str(r#"{"start_screen_enabled":true,"start_screen_gesture":["ps"]}"#)
                .unwrap();
        assert!(prefs.start_screen_sounds_enabled);
        assert_eq!(prefs.start_screen_sound_volume, 60);
    }

    #[test]
    fn older_prefs_default_start_screen_haptics_on() {
        let prefs: Prefs =
            serde_json::from_str(r#"{"start_screen_enabled":true,"start_screen_gesture":["ps"]}"#)
                .unwrap();
        assert!(prefs.start_screen_haptics_enabled);
        assert_eq!(prefs.start_screen_haptics_strength, 60);
    }

    #[test]
    fn clamp_start_screen_sound_volume_bounds() {
        assert_eq!(clamp_start_screen_sound_volume(0), 0);
        assert_eq!(clamp_start_screen_sound_volume(60), 60);
        assert_eq!(clamp_start_screen_sound_volume(100), 100);
        assert_eq!(clamp_start_screen_sound_volume(255), 100);
    }

    #[test]
    fn clamp_start_screen_haptics_strength_bounds() {
        assert_eq!(clamp_start_screen_haptics_strength(0), 0);
        assert_eq!(clamp_start_screen_haptics_strength(60), 60);
        assert_eq!(clamp_start_screen_haptics_strength(100), 100);
        assert_eq!(clamp_start_screen_haptics_strength(255), 100);
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

    #[test]
    fn games_sort_mode_cycles_and_defaults() {
        assert_eq!(GamesSortMode::default(), GamesSortMode::LastPlayed);
        assert_eq!(
            GamesSortMode::LastPlayed.cycle(),
            GamesSortMode::Alphabetical
        );
        assert_eq!(
            GamesSortMode::Alphabetical.cycle(),
            GamesSortMode::LastPlayed
        );
        let prefs: Prefs = serde_json::from_str("{}").unwrap();
        assert_eq!(prefs.games_sort_mode, GamesSortMode::LastPlayed);
    }

    #[test]
    fn older_prefs_default_show_all_controllers_off() {
        let prefs: Prefs = serde_json::from_str("{}").unwrap();
        assert!(!prefs.show_all_controllers);
    }

    #[test]
    fn older_prefs_default_start_screen_always_immersive_off() {
        let prefs: Prefs =
            serde_json::from_str(r#"{"start_screen_enabled":true,"start_screen_gesture":["ps"]}"#)
                .unwrap();
        assert!(!prefs.start_screen_always_immersive);
    }

    #[test]
    fn older_prefs_default_immersive_idle_timeouts() {
        let prefs: Prefs =
            serde_json::from_str(r#"{"start_screen_enabled":true,"start_screen_gesture":["ps"]}"#)
                .unwrap();
        assert_eq!(prefs.start_screen_inactive_secs, 30);
        assert_eq!(prefs.start_screen_sleep_secs, 120);
        assert_eq!(prefs.start_screen_inactive_dim_percent, 50);
    }

    #[test]
    fn clamp_start_screen_idle_timeouts_orders_and_bounds() {
        assert_eq!(clamp_start_screen_idle_timeouts(30, 180), (30, 180));
        // Sleep must be ≥ one sleep breakpoint ahead of idle (2m → 3m).
        assert_eq!(clamp_start_screen_idle_timeouts(120, 60), (120, 180));
        assert_eq!(clamp_start_screen_idle_timeouts(120, 120), (120, 180));
        assert_eq!(clamp_start_screen_idle_timeouts(0, 10), (5, 60));
        assert_eq!(min_sleep_secs_for_idle(600), 900);
        assert_eq!(clamp_start_screen_idle_timeouts(9_999, 9_999), (600, 3_600));
        assert_eq!(snap_to_secs_steps(20, START_SCREEN_IDLE_SECS_STEPS), 15);
        assert_eq!(snap_to_secs_steps(45, START_SCREEN_IDLE_SECS_STEPS), 30);
        assert_eq!(snap_to_secs_steps(90, START_SCREEN_SLEEP_SECS_STEPS), 60);
        assert_eq!(clamp_start_screen_inactive_dim_percent(30), 30);
        assert_eq!(clamp_start_screen_inactive_dim_percent(255), 100);
    }

    #[test]
    fn format_timeout_duration_units() {
        assert_eq!(format_timeout_duration(5), "5 seconds");
        assert_eq!(format_timeout_duration(1), "1 second");
        assert_eq!(format_timeout_duration(60), "1 minute");
        assert_eq!(format_timeout_duration(180), "3 minutes");
    }

    #[test]
    fn older_prefs_default_start_screen_usb_controllers_on() {
        let prefs: Prefs =
            serde_json::from_str(r#"{"start_screen_enabled":true,"start_screen_gesture":["ps"]}"#)
                .unwrap();
        assert!(prefs.start_screen_usb_controllers);
    }
}
