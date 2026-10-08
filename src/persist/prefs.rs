//! Persisted notification preferences.

use crate::controller::dualsense::battery::LOW_BATTERY_PERCENT;
use crate::domain::color::BatterySpectrum;
use crate::domain::gesture::{GestureControl, default_gesture};
use crate::platform::app_log;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
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

/// When a controller connect auto-opens the start screen.
/// Single control subsuming the former auto-open bool + USB-scope bool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartAutoOpen {
    /// Never auto-open (manual / gesture open only).
    Never,
    /// Only Bluetooth connects open; USB pads are ignored for open and close.
    Bluetooth,
    /// Any controller connect opens (default).
    #[default]
    Any,
}

impl StartAutoOpen {
    /// Whether connects can auto-open at all.
    pub fn auto_opens(self) -> bool {
        !matches!(self, Self::Never)
    }

    /// Whether USB pads count toward open/close presence.
    pub fn includes_usb(self) -> bool {
        matches!(self, Self::Any)
    }

    /// Next option, wrapping around (Cross cycles infinitely).
    pub fn next(self) -> Self {
        match self {
            Self::Never => Self::Bluetooth,
            Self::Bluetooth => Self::Any,
            Self::Any => Self::Never,
        }
    }

    /// Previous option, wrapping around.
    pub fn prev(self) -> Self {
        match self {
            Self::Never => Self::Any,
            Self::Bluetooth => Self::Never,
            Self::Any => Self::Bluetooth,
        }
    }

    /// Short display label for the single-option viewport.
    pub fn label(self) -> &'static str {
        match self {
            Self::Never => "Never",
            Self::Bluetooth => "When a Bluetooth controller connects",
            Self::Any => "When any controller connects",
        }
    }
}

impl std::fmt::Display for StartAutoOpen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// All modes in cycle/menu order.
pub const AUTO_OPEN_MODES: [StartAutoOpen; 3] = [
    StartAutoOpen::Never,
    StartAutoOpen::Bluetooth,
    StartAutoOpen::Any,
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImmersiveLayout {
    #[default]
    Vertical,
    Horizontal,
}

impl ImmersiveLayout {
    pub fn cycle(self) -> Self {
        match self {
            Self::Vertical => Self::Horizontal,
            Self::Horizontal => Self::Vertical,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Vertical => "Vertical",
            Self::Horizontal => "Horizontal",
        }
    }
}

pub const IMMERSIVE_LAYOUT_MODES: [ImmersiveLayout; 2] =
    [ImmersiveLayout::Vertical, ImmersiveLayout::Horizontal];

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
    /// Master switch: when off, the start screen never opens.
    #[serde(default = "default_true")]
    pub start_screen_enabled: bool,
    /// Automatically open the start screen when a controller connects.
    /// `never` disables it (gesture / manual open still work);
    /// `bluetooth` ignores USB pads for open and close; `any` counts every
    /// controller (default).
    #[serde(default)]
    pub start_screen_auto_open: StartAutoOpen,
    /// Always use immersive Start (never show compact; cancel closes) (default on).
    #[serde(default = "default_true")]
    pub start_screen_always_immersive: bool,
    /// Show the clock widget at the top while immersive Start is visible (default on).
    #[serde(default = "default_true")]
    pub start_screen_clock_enabled: bool,
    /// Immersive idle dim after this many seconds (default 30). Snapped to idle steps.
    #[serde(default = "default_start_screen_inactive_secs")]
    pub start_screen_inactive_secs: u32,
    /// Immersive sleep (full black) after this many seconds (default 120). Snapped to sleep steps.
    #[serde(default = "default_start_screen_sleep_secs")]
    pub start_screen_sleep_secs: u32,
    /// Idle-mode black overlay percent 0–100 (default 50). Sleep is always 100%.
    #[serde(default = "default_start_screen_inactive_dim_percent")]
    pub start_screen_inactive_dim_percent: u8,
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
    #[serde(default)]
    pub start_screen_immersive_layout: ImmersiveLayout,
    #[serde(default)]
    pub extra_steam_paths: Vec<std::path::PathBuf>,
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
            start_screen_auto_open: StartAutoOpen::Any,
            start_screen_always_immersive: true,
            start_screen_clock_enabled: true,
            start_screen_inactive_secs: default_start_screen_inactive_secs(),
            start_screen_sleep_secs: default_start_screen_sleep_secs(),
            start_screen_inactive_dim_percent: default_start_screen_inactive_dim_percent(),
            start_screen_gesture: default_gesture(),
            start_screen_sounds_enabled: true,
            start_screen_sound_volume: default_start_screen_sound_volume(),
            start_screen_haptics_enabled: true,
            start_screen_haptics_strength: default_start_screen_haptics_strength(),
            games_sort_mode: GamesSortMode::default(),
            show_all_controllers: false,
            start_screen_immersive_layout: ImmersiveLayout::default(),
            extra_steam_paths: Vec::new(),
        }
    }
}

/// Legacy keys consumed into [`StartAutoOpen`] (no surviving struct fields).
const LEGACY_AUTO_OPEN_KEY: &str = "start_screen_auto_open";
const LEGACY_USB_KEY: &str = "start_screen_usb_controllers";

/// One-time migration from pre-1.6.1 bool prefs into the [`StartAutoOpen`]
/// enum. Runs on the JSON object before struct deserialization; new string
/// values pass through untouched:
/// - unreleased bool `start_screen_auto_open: false` → `never`;
/// - `true` (or a 1.6.0 file without the key) + legacy USB bool →
///   `any` / `bluetooth` (USB defaulted on, matching the old defaults).
fn migrate_legacy_auto_open(root: &mut Map<String, Value>) {
    if matches!(root.get(LEGACY_AUTO_OPEN_KEY), Some(Value::String(_))) {
        return;
    }
    let legacy_bool = root.get(LEGACY_AUTO_OPEN_KEY).and_then(Value::as_bool);
    let usb = root
        .get(LEGACY_USB_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mode = match legacy_bool {
        Some(false) => "never",
        _ => {
            if usb {
                "any"
            } else {
                "bluetooth"
            }
        }
    };
    root.insert(
        LEGACY_AUTO_OPEN_KEY.to_string(),
        Value::String(mode.to_string()),
    );
}

impl Prefs {
    pub fn load() -> Self {
        let path = prefs_path();
        let Ok(bytes) = fs::read(&path) else {
            return Self::default();
        };
        let Ok(Value::Object(mut root)) = serde_json::from_slice::<Value>(&bytes) else {
            app_log::warn(format!(
                "failed to parse prefs at {}: not a JSON object; using defaults",
                path.display()
            ));
            return Self::default();
        };
        migrate_legacy_auto_open(&mut root);
        match serde_json::from_value::<Prefs>(Value::Object(root)) {
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
    fn missing_start_screen_always_immersive_defaults_on() {
        let prefs: Prefs =
            serde_json::from_str(r#"{"start_screen_enabled":true,"start_screen_gesture":["ps"]}"#)
                .unwrap();
        assert!(prefs.start_screen_always_immersive);
        assert!(Prefs::default().start_screen_always_immersive);
    }

    #[test]
    fn explicit_start_screen_always_immersive_false_stays_off() {
        let prefs: Prefs = serde_json::from_str(
            r#"{"start_screen_enabled":true,"start_screen_always_immersive":false}"#,
        )
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

    /// Parse helper mirroring [`Prefs::load`] (minus the filesystem read).
    fn prefs_from_json(json: &str) -> Prefs {
        let Value::Object(mut root) = serde_json::from_str(json).unwrap() else {
            panic!("expected object");
        };
        migrate_legacy_auto_open(&mut root);
        let mut prefs: Prefs = serde_json::from_value(Value::Object(root)).unwrap();
        prefs.normalize();
        prefs
    }

    #[test]
    fn start_auto_open_defaults_to_any() {
        let prefs = prefs_from_json("{}");
        assert!(prefs.start_screen_enabled);
        assert_eq!(prefs.start_screen_auto_open, StartAutoOpen::Any);
        assert_eq!(Prefs::default().start_screen_auto_open, StartAutoOpen::Any);
        // Serializes as a plain string (single key, no legacy bool cruft).
        let value = serde_json::to_value(Prefs::default()).unwrap();
        assert_eq!(value["start_screen_auto_open"], "any");
        assert!(value.get("start_screen_usb_controllers").is_none());
    }

    #[test]
    fn start_auto_open_modes_parse_and_persist() {
        for (raw, mode) in [
            ("never", StartAutoOpen::Never),
            ("bluetooth", StartAutoOpen::Bluetooth),
            ("any", StartAutoOpen::Any),
        ] {
            let prefs = prefs_from_json(&format!(r#"{{"start_screen_auto_open":{raw:?}}}"#));
            assert_eq!(prefs.start_screen_auto_open, mode);
        }
    }

    #[test]
    fn legacy_1_6_0_usb_bool_migrates_to_scoped_mode() {
        // 1.6.0 files have no auto-open key: USB choice maps onto the enum.
        let prefs = prefs_from_json(
            r#"{"start_screen_enabled":true,"start_screen_usb_controllers":false}"#,
        );
        assert_eq!(prefs.start_screen_auto_open, StartAutoOpen::Bluetooth);
        let prefs =
            prefs_from_json(r#"{"start_screen_enabled":true,"start_screen_usb_controllers":true}"#);
        assert_eq!(prefs.start_screen_auto_open, StartAutoOpen::Any);
    }

    #[test]
    fn legacy_unreleased_auto_open_bool_migrates() {
        // `false` always meant never; `true` defers to the USB scope.
        let prefs = prefs_from_json(
            r#"{"start_screen_auto_open":false,"start_screen_usb_controllers":true}"#,
        );
        assert_eq!(prefs.start_screen_auto_open, StartAutoOpen::Never);
        let prefs = prefs_from_json(
            r#"{"start_screen_auto_open":true,"start_screen_usb_controllers":false}"#,
        );
        assert_eq!(prefs.start_screen_auto_open, StartAutoOpen::Bluetooth);
        let prefs = prefs_from_json(r#"{"start_screen_auto_open":true}"#);
        assert_eq!(prefs.start_screen_auto_open, StartAutoOpen::Any);
    }

    #[test]
    fn start_auto_open_cycles_with_wrap_and_labels() {
        assert_eq!(StartAutoOpen::Never.next(), StartAutoOpen::Bluetooth);
        assert_eq!(StartAutoOpen::Bluetooth.next(), StartAutoOpen::Any);
        assert_eq!(StartAutoOpen::Any.next(), StartAutoOpen::Never);
        assert_eq!(StartAutoOpen::Never.prev(), StartAutoOpen::Any);
        assert_eq!(StartAutoOpen::Any.prev(), StartAutoOpen::Bluetooth);
        assert_eq!(StartAutoOpen::Never.label(), "Never");
        assert_eq!(
            StartAutoOpen::Bluetooth.label(),
            "When a Bluetooth controller connects"
        );
        assert_eq!(StartAutoOpen::Any.label(), "When any controller connects");
        assert_eq!(
            StartAutoOpen::Bluetooth.to_string(),
            "When a Bluetooth controller connects"
        );
        assert!(StartAutoOpen::Any.auto_opens());
        assert!(StartAutoOpen::Bluetooth.auto_opens());
        assert!(!StartAutoOpen::Never.auto_opens());
        assert!(StartAutoOpen::Any.includes_usb());
        assert!(!StartAutoOpen::Bluetooth.includes_usb());
        assert!(!StartAutoOpen::Never.includes_usb());
    }

    #[test]
    fn start_screen_clock_defaults_on() {
        let prefs: Prefs = serde_json::from_str("{}").unwrap();
        assert!(prefs.start_screen_clock_enabled);
        assert!(Prefs::default().start_screen_clock_enabled);
        let prefs: Prefs = serde_json::from_str(r#"{"start_screen_clock_enabled":false}"#).unwrap();
        assert!(!prefs.start_screen_clock_enabled);
    }

    #[test]
    fn immersive_layout_round_trips_with_extra_steam_paths() {
        let prefs = Prefs {
            start_screen_immersive_layout: ImmersiveLayout::Horizontal,
            extra_steam_paths: vec![PathBuf::from("/steam/extra")],
            ..Prefs::default()
        };
        let encoded = serde_json::to_string(&prefs).unwrap();
        let decoded: Prefs = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            decoded.start_screen_immersive_layout,
            ImmersiveLayout::Horizontal
        );
        assert_eq!(
            decoded.extra_steam_paths,
            vec![PathBuf::from("/steam/extra")]
        );
    }

    #[test]
    fn older_prefs_default_to_vertical_layout_and_no_extra_paths() {
        let prefs: Prefs = serde_json::from_str("{}").unwrap();
        assert_eq!(
            prefs.start_screen_immersive_layout,
            ImmersiveLayout::Vertical
        );
        assert!(prefs.extra_steam_paths.is_empty());
        assert_eq!(
            Prefs::default().start_screen_immersive_layout,
            ImmersiveLayout::Vertical
        );
        assert!(Prefs::default().extra_steam_paths.is_empty());
    }
}
