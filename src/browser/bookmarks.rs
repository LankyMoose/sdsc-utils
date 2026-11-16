//! Chrome-style bookmarks, persisted to disk (`bookmarks.json`).
//!
//! The one deliberate disk exception to session-scoped browser state: a
//! bookmark that evaporates on restart is pointless. Plain JSON array via
//! the shared `persist::store` helpers — no loss detection needed (a corrupt
//! file parses to empty and gets overwritten on the next change).

use std::path::{Path, PathBuf};

/// One bookmark: URL plus the display name (defaults to the page title at
/// add time, editable through the star bubble).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Bookmark {
    pub url: String,
    pub title: String,
}

impl Bookmark {
    /// Display name, falling back to the URL when untitled.
    pub fn name(&self) -> &str {
        if self.title.is_empty() {
            &self.url
        } else {
            &self.title
        }
    }
}

/// Storage location next to `prefs.json`.
pub fn default_path() -> PathBuf {
    crate::persist::paths::data_dir().join("bookmarks.json")
}

/// Load, defaulting to empty (missing or corrupt file).
pub fn load() -> Vec<Bookmark> {
    load_from(&default_path())
}

fn load_from(path: &Path) -> Vec<Bookmark> {
    crate::persist::store::load_or_default(path)
}

/// Overwrite the file with the current list.
pub fn save(bookmarks: &[Bookmark]) {
    save_to(&default_path(), bookmarks);
}

fn save_to(path: &Path, bookmarks: &[Bookmark]) {
    // `save_pretty` needs a Sized value: one small clone on the user-action
    // path (add/rename/remove), never on a hot loop.
    crate::persist::store::save_pretty("bookmarks", path, &bookmarks.to_vec());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_through_disk() {
        let dir = std::env::temp_dir().join("sdsc-bookmark-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bookmarks.json");
        let _ = std::fs::remove_file(&path);
        assert!(load_from(&path).is_empty());
        let list = vec![
            Bookmark {
                url: "https://example.com/".to_string(),
                title: "Example".to_string(),
            },
            Bookmark {
                url: "https://x.test/".to_string(),
                title: String::new(),
            },
        ];
        save_to(&path, &list);
        assert_eq!(load_from(&path), list);
        // Name falls back to the URL when untitled.
        assert_eq!(list[1].name(), "https://x.test/");
        let _ = std::fs::remove_file(&path);
    }
}
