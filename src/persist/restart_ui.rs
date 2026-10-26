//! UI restore record for the renderer-pref auto-restart (AA/vsync).
//!
//! AA/vsync are boot-only iced settings, so flipping them restarts the shell.
//! To avoid losing the user's place, the restarting shell snapshots which
//! windows were open (Start / Configure / popup), the Start selection keys,
//! list scroll offsets, drawer state, and the Configure section into
//! `restart-ui.json`, then exits. The next boot consumes the file (only when
//! fresh — see [`RESTART_UI_MAX_AGE_SECS`]) and reopens to the same state.
//!
//! Written only on the renderer-restart path, never on plain Quit. A stale
//! file (crash between schedule and exit, or an old quit) is ignored by age
//! and deleted on read.

use crate::persist::paths;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Maximum age of a restore record before it is ignored (seconds).
pub const RESTART_UI_MAX_AGE_SECS: u64 = 120;

/// Which Start slide was showing (`true` = Controllers).
fn default_false() -> bool {
    false
}

/// Full shell UI state captured before a renderer-pref restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartUiState {
    /// Unix millis when captured (freshness gate).
    pub saved_at_ms: u64,
    /// Start window was visible.
    #[serde(default = "default_false")]
    pub start_visible: bool,
    /// Controllers surface was open.
    #[serde(default = "default_false")]
    pub start_on_controllers: bool,
    /// Selected game / controller keys (re-resolved after rows rebuild).
    #[serde(default)]
    pub game_key: Option<String>,
    #[serde(default)]
    pub controller_key: Option<String>,
    /// Selected game row index at capture (by-position restore only; there is
    /// no controller index — controller persistence is out of scope).
    #[serde(default)]
    pub game_index: Option<usize>,
    /// Tracked settings-drawer offset.
    #[serde(default)]
    pub settings_scroll_y: f32,
    /// Start settings drawer was open + focused row.
    #[serde(default = "default_false")]
    pub settings_open: bool,
    #[serde(default)]
    pub settings_focus: usize,
    /// Configure window was open + section.
    #[serde(default)]
    pub configure_open: bool,
    #[serde(default)]
    pub configure_section: Option<crate::ui::configure::Section>,
    /// Configure content scroll offset.
    #[serde(default)]
    pub configure_scroll_y: f32,
    /// Tray popup was open + anchor to reopen at.
    #[serde(default)]
    pub popup_open: bool,
    #[serde(default)]
    pub popup_anchor: Option<crate::ui::layout::TrayAnchor>,
}

impl RestartUiState {
    pub fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// True when the record is fresh enough to honor.
    pub fn is_fresh(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.saved_at_ms) <= RESTART_UI_MAX_AGE_SECS.saturating_mul(1000)
    }
}

fn restart_ui_path() -> PathBuf {
    paths::data_dir().join("restart-ui.json")
}

/// Persist a restore record (renderer-restart path only).
pub fn save(state: &RestartUiState) {
    crate::persist::store::save_pretty("restart-ui", &restart_ui_path(), state);
}

/// Read + delete the restore record. Returns `None` when missing, corrupt,
/// or older than [`RESTART_UI_MAX_AGE_SECS`].
pub fn take() -> Option<RestartUiState> {
    let path = restart_ui_path();
    let bytes = fs::read(&path).ok()?;
    // Consume unconditionally: a restart record must never apply twice.
    let _ = fs::remove_file(&path);
    let state: RestartUiState = serde_json::from_slice(&bytes).ok()?;
    if state.is_fresh(RestartUiState::now_ms()) {
        Some(state)
    } else {
        None
    }
}

/// Drop any pending restore record (plain Quit must not restore on relaunch).
pub fn clear() {
    let _ = fs::remove_file(restart_ui_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> RestartUiState {
        RestartUiState {
            saved_at_ms: 1_000_000,
            start_visible: true,
            start_on_controllers: true,
            game_key: Some("steam:440".into()),
            controller_key: None,
            game_index: Some(2),
            settings_scroll_y: 42.0,
            settings_open: true,
            settings_focus: 3,
            configure_open: true,
            configure_section: None,
            configure_scroll_y: 120.0,
            popup_open: false,
            popup_anchor: None,
        }
    }

    #[test]
    fn restart_ui_round_trips() {
        let encoded = serde_json::to_string(&state()).unwrap();
        let decoded: RestartUiState = serde_json::from_str(&encoded).unwrap();
        assert!(decoded.start_visible);
        assert!(decoded.start_on_controllers);
        assert_eq!(decoded.game_key.as_deref(), Some("steam:440"));
        assert_eq!(decoded.game_index, Some(2));
        assert_eq!(decoded.settings_focus, 3);
        assert!((decoded.configure_scroll_y - 120.0).abs() < f32::EPSILON);
    }

    #[test]
    fn older_records_default_new_fields() {
        let decoded: RestartUiState = serde_json::from_str(r#"{"saved_at_ms":1000}"#).unwrap();
        assert!(!decoded.start_visible);
        assert!(decoded.game_key.is_none());
        assert!(decoded.game_index.is_none());
        assert!(!decoded.settings_open);
        assert!(!decoded.configure_open);
        assert!(!decoded.popup_open);
    }

    #[test]
    fn freshness_gates_stale_records() {
        let fresh = RestartUiState {
            saved_at_ms: 1_000_000,
            ..state()
        };
        assert!(fresh.is_fresh(1_000_000));
        assert!(fresh.is_fresh(1_000_000 + RESTART_UI_MAX_AGE_SECS * 1000));
        assert!(!fresh.is_fresh(1_000_000 + RESTART_UI_MAX_AGE_SECS * 1000 + 1));
        // Clock skew (saved in the future) still restores.
        assert!(fresh.is_fresh(999_000));
    }
}
