//! Browser security decisions (`browser_prefs.json`).
//!
//! Remembered navigation decisions are keyed by the destination's registrable
//! domain so subdomains and changing query strings hit the same entry.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const MAX_NAVIGATION_DECISIONS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NavDecisionAction {
    Block,
    Allow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavDecision {
    #[serde(default)]
    pub destination: String,
    pub action: NavDecisionAction,
    #[serde(default)]
    pub created_unix: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserPrefs {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub navigation_decisions: Vec<NavDecision>,
}

pub fn path() -> PathBuf {
    crate::persist::paths::data_dir().join("browser_prefs.json")
}

impl BrowserPrefs {
    /// Load, defaulting to empty on a missing/corrupt file.
    pub fn load() -> Self {
        Self::load_from(&path())
    }

    pub fn load_from(path: &std::path::Path) -> Self {
        let mut prefs: Self = crate::persist::store::load_or_default(path);
        prefs.normalize();
        prefs
    }

    /// Drop malformed or duplicate entries and enforce the cap (oldest first).
    pub fn normalize(&mut self) {
        self.version = 1;
        let mut seen = std::collections::HashSet::new();
        self.navigation_decisions.retain(|entry| {
            let destination = entry.destination.trim().to_ascii_lowercase();
            !destination.is_empty() && seen.insert(destination)
        });
        for entry in &mut self.navigation_decisions {
            entry.destination = entry.destination.trim().to_ascii_lowercase();
        }
        let len = self.navigation_decisions.len();
        if len > MAX_NAVIGATION_DECISIONS {
            self.navigation_decisions
                .drain(..len - MAX_NAVIGATION_DECISIONS);
        }
    }

    pub fn remember(&mut self, destination: &str, action: NavDecisionAction) {
        let destination = destination.trim().to_ascii_lowercase();
        if destination.is_empty() {
            return;
        }
        self.navigation_decisions
            .retain(|entry| entry.destination != destination);
        self.navigation_decisions.push(NavDecision {
            destination,
            action,
            created_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        });
        let len = self.navigation_decisions.len();
        if len > MAX_NAVIGATION_DECISIONS {
            self.navigation_decisions
                .drain(..len - MAX_NAVIGATION_DECISIONS);
        }
    }

    pub fn save(&self) {
        self.save_to(&path());
    }

    pub fn save_to(&self, path: &std::path::Path) {
        let mut prefs = self.clone();
        prefs.version = 1;
        crate::persist::store::save_pretty("browser_prefs", path, &prefs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_cap() {
        let dir = std::env::temp_dir().join("sdsc-browser-prefs-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("browser_prefs.json");
        let _ = std::fs::remove_file(&path);

        let mut prefs = BrowserPrefs::default();
        prefs.remember("giftdash.rest", NavDecisionAction::Block);
        prefs.remember("example.com", NavDecisionAction::Allow);
        prefs.save_to(&path);
        let loaded = BrowserPrefs::load_from(&path);
        assert_eq!(loaded.navigation_decisions.len(), 2);
        assert_eq!(loaded.navigation_decisions[0].destination, "giftdash.rest");
        assert_eq!(
            loaded.navigation_decisions[1].action,
            NavDecisionAction::Allow
        );

        for index in 0..MAX_NAVIGATION_DECISIONS + 8 {
            prefs.remember(&format!("host{index}.test"), NavDecisionAction::Block);
        }
        assert_eq!(prefs.navigation_decisions.len(), MAX_NAVIGATION_DECISIONS);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_file_is_empty() {
        let dir = std::env::temp_dir().join("sdsc-browser-prefs-corrupt");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("browser_prefs.json");
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(
            BrowserPrefs::load_from(&path)
                .navigation_decisions
                .is_empty()
        );
        let _ = std::fs::remove_file(&path);
    }
}
