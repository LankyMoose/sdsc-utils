//! Persisted start-screen game catalog (`games.json`).

use crate::app_log;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GameEntry {
    Steam {
        appid: u32,
    },
    Manual {
        id: String,
        title: String,
        target: String,
    },
}

impl GameEntry {
    pub fn manual(title: impl Into<String>, target: impl Into<String>) -> Self {
        Self::Manual {
            id: new_manual_id(),
            title: title.into(),
            target: target.into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GamesCatalog {
    #[serde(default)]
    pub entries: Vec<GameEntry>,
}

impl GamesCatalog {
    pub fn load() -> Self {
        let path = store_path();
        let Ok(bytes) = fs::read(&path) else {
            return Self::default();
        };
        match serde_json::from_slice::<GamesCatalog>(&bytes) {
            Ok(catalog) => catalog,
            Err(err) => {
                app_log::warn(format!(
                    "failed to parse games at {}: {err}; starting empty",
                    path.display()
                ));
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        let path = store_path();
        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            app_log::warn(format!("failed to create games dir: {err}"));
            return;
        }
        match serde_json::to_vec_pretty(self) {
            Ok(bytes) => {
                if let Err(err) = fs::write(&path, bytes) {
                    app_log::warn(format!("failed to write games: {err}"));
                }
            }
            Err(err) => app_log::warn(format!("failed to serialize games: {err}")),
        }
    }

    pub fn contains_steam(&self, appid: u32) -> bool {
        self.entries
            .iter()
            .any(|e| matches!(e, GameEntry::Steam { appid: id } if *id == appid))
    }

    pub fn toggle_steam(&mut self, appid: u32, enabled: bool) -> bool {
        let present = self.contains_steam(appid);
        if enabled && !present {
            self.entries.push(GameEntry::Steam { appid });
            true
        } else if !enabled && present {
            self.entries
                .retain(|e| !matches!(e, GameEntry::Steam { appid: id } if *id == appid));
            true
        } else {
            false
        }
    }

    pub fn add_manual(&mut self, title: String, target: String) {
        self.entries.push(GameEntry::manual(title, target));
    }

    pub fn remove_at(&mut self, index: usize) -> bool {
        if index < self.entries.len() {
            self.entries.remove(index);
            true
        } else {
            false
        }
    }

    pub fn move_entry(&mut self, index: usize, up: bool) -> bool {
        if index >= self.entries.len() {
            return false;
        }
        let swap = if up {
            index.checked_sub(1)
        } else if index + 1 < self.entries.len() {
            Some(index + 1)
        } else {
            None
        };
        let Some(other) = swap else {
            return false;
        };
        self.entries.swap(index, other);
        true
    }

    pub fn set_manual_title(&mut self, index: usize, title: String) -> bool {
        match self.entries.get_mut(index) {
            Some(GameEntry::Manual { title: current, .. }) => {
                *current = title;
                true
            }
            _ => false,
        }
    }

    /// Entries for the start screen.
    ///
    /// `installed_steam`: `None` = library scan not finished/failed — keep all Steam
    /// appids. `Some(ids)` = successful scan — drop Steam appids that are not installed.
    pub fn merge_for_display(&self, installed_steam: Option<&[u32]>) -> Vec<GameEntry> {
        self.entries
            .iter()
            .filter(|entry| match entry {
                GameEntry::Steam { appid } => match installed_steam {
                    None => true,
                    Some(ids) => ids.contains(appid),
                },
                GameEntry::Manual { .. } => true,
            })
            .cloned()
            .collect()
    }
}

fn new_manual_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}

fn store_path() -> PathBuf {
    crate::paths::data_dir().join("games.json")
}

/// Default display title for a manual target path or URL.
pub fn title_from_target(target: &str) -> String {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return "Untitled".to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("steam://") {
        return format!("Steam: {rest}");
    }
    Path::new(trimmed)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_catalog_deserializes() {
        let catalog: GamesCatalog = serde_json::from_str("{}").unwrap();
        assert!(catalog.entries.is_empty());
    }

    #[test]
    fn toggle_steam_and_merge() {
        let mut catalog = GamesCatalog::default();
        assert!(catalog.toggle_steam(10, true));
        assert!(catalog.contains_steam(10));
        assert!(!catalog.toggle_steam(10, true));
        // Before a successful scan, keep curated Steam rows.
        assert_eq!(catalog.merge_for_display(None).len(), 1);
        assert_eq!(catalog.merge_for_display(Some(&[10])).len(), 1);
        assert!(catalog.merge_for_display(Some(&[])).is_empty());
        assert!(catalog.toggle_steam(10, false));
        assert!(!catalog.contains_steam(10));
    }

    #[test]
    fn merge_keeps_steam_when_scan_unknown() {
        let mut catalog = GamesCatalog::default();
        catalog.toggle_steam(42, true);
        assert_eq!(catalog.merge_for_display(None).len(), 1);
        assert!(catalog.merge_for_display(Some(&[99])).is_empty());
    }

    #[test]
    fn move_and_rename_manual() {
        let mut catalog = GamesCatalog::default();
        catalog.add_manual("A".into(), "a.exe".into());
        catalog.add_manual("B".into(), "b.exe".into());
        assert!(catalog.move_entry(1, true));
        assert_eq!(
            match &catalog.entries[0] {
                GameEntry::Manual { title, .. } => title.as_str(),
                _ => "",
            },
            "B"
        );
        assert!(catalog.set_manual_title(0, "Bee".into()));
    }

    #[test]
    fn title_from_target_uses_stem() {
        assert_eq!(title_from_target(r"C:\Games\Cool Game.exe"), "Cool Game");
        assert!(title_from_target("steam://rungameid/1").starts_with("Steam:"));
    }
}
