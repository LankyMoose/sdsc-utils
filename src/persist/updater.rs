//! Once-per-version update notification state (`updater.json`).
//!
//! `last_notified` records the newest release tag the native OS
//! notification already fired for, so a new version notifies exactly once
//! no matter how often the app boots or re-checks.

use crate::persist::paths;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdaterState {
    /// Newest release tag already notified for (`None` = never notified).
    #[serde(default)]
    pub last_notified: Option<String>,
}

impl UpdaterState {
    pub fn load() -> Self {
        crate::persist::store::load_or_default(&updater_path())
    }

    pub fn save(&self) {
        crate::persist::store::save_pretty("updater", &updater_path(), self);
    }

    /// True when `tag` is a real tag we have not notified for yet.
    /// Pure (no disk I/O) so the gate is unit-testable.
    pub fn needs_notify(&self, tag: &str) -> bool {
        !tag.is_empty() && self.last_notified.as_deref() != Some(tag)
    }

    /// Record `tag` as notified. Pure in-memory; call [`Self::save`] after.
    pub fn mark_notified(&mut self, tag: &str) {
        if tag.is_empty() {
            return;
        }
        if self.last_notified.as_deref() != Some(tag) {
            self.last_notified = Some(tag.to_string());
        }
    }
}

fn updater_path() -> PathBuf {
    paths::data_dir().join("updater.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_version_needs_notify_then_sticks() {
        let mut state = UpdaterState::default();
        assert!(state.needs_notify("v1.7.3"));
        state.mark_notified("v1.7.3");
        assert!(!state.needs_notify("v1.7.3"));
        assert_eq!(state.last_notified.as_deref(), Some("v1.7.3"));
    }

    #[test]
    fn newer_version_rearms_older_does_not() {
        let mut state = UpdaterState::default();
        state.mark_notified("v1.7.3");
        assert!(state.needs_notify("v1.7.4"));
        // Re-marking the same tag is a no-op, never clears.
        state.mark_notified("v1.7.3");
        assert_eq!(state.last_notified.as_deref(), Some("v1.7.3"));
    }

    #[test]
    fn empty_tag_never_notifies_or_marks() {
        let mut state = UpdaterState::default();
        assert!(!state.needs_notify(""));
        state.mark_notified("");
        assert_eq!(state.last_notified, None);
    }
}
