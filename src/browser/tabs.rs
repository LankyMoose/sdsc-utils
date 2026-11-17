//! Persisted tab strip (`tabs.json`).
//!
//! Second deliberate disk exception to session-scoped browser state (after
//! bookmarks): a tab strip that evaporates on restart is pointless. Plain
//! JSON via the shared `persist::store` helpers — no loss detection needed
//! (a corrupt file parses to empty and gets overwritten on the next change).
//!
//! Only the strip is persisted (URL + title per tab, active position).
//! Engine state never crosses a restart: tabs restore by re-navigating
//! fresh views, so back/forward stacks start over — the engine's canonical
//! history overwrites whatever we remembered on first load.

use std::path::{Path, PathBuf};

/// Cap on restored tabs (degenerate-file guard; WebViews are heavyweight).
pub const MAX_SAVED_TABS: usize = 25;

/// One persisted tab: URL plus the title at save time (paints instantly,
/// before the restored load's `TitleChanged` lands).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SavedTab {
    pub url: String,
    pub title: String,
}

/// The persisted strip: creation-order tabs plus the active position (an
/// index, not an id — tab ids are per-launch).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SavedTabs {
    pub tabs: Vec<SavedTab>,
    pub active: usize,
}

/// Storage location next to `prefs.json`.
pub fn default_path() -> PathBuf {
    crate::persist::paths::data_dir().join("tabs.json")
}

/// Load, defaulting to empty (missing or corrupt file).
pub fn load() -> SavedTabs {
    load_from(&default_path())
}

fn load_from(path: &Path) -> SavedTabs {
    crate::persist::store::load_or_default(path)
}

/// Overwrite the file with the current strip.
pub fn save(strip: &SavedTabs) {
    save_to(&default_path(), strip);
}

fn save_to(path: &Path, strip: &SavedTabs) {
    // `save_pretty` needs a Sized value: one small clone on the tab-action
    // path (open/close/switch/navigate), never on a hot loop.
    crate::persist::store::save_pretty("tabs", path, &strip.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_through_disk() {
        let dir = std::env::temp_dir().join("sdsc-tabs-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("tabs.json");
        let _ = std::fs::remove_file(&path);
        assert!(load_from(&path).tabs.is_empty());
        let strip = SavedTabs {
            tabs: vec![
                SavedTab {
                    url: "https://example.com/".to_string(),
                    title: "Example".to_string(),
                },
                SavedTab {
                    url: "https://x.test/".to_string(),
                    title: String::new(),
                },
            ],
            active: 1,
        };
        save_to(&path, &strip);
        assert_eq!(load_from(&path), strip);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_file_loads_empty() {
        let dir = std::env::temp_dir().join("sdsc-tabs-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("tabs-corrupt.json");
        let _ = std::fs::write(&path, "{not json");
        assert!(load_from(&path).tabs.is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
