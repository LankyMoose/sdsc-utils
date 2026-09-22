//! Persisted start-screen game catalog (`games.json`).

use crate::app_log;
use crate::prefs::GamesSortMode;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
        #[serde(default)]
        args: String,
        #[serde(default)]
        icon: Option<String>,
    },
}

impl GameEntry {
    pub fn manual_with(
        title: impl Into<String>,
        target: impl Into<String>,
        args: impl Into<String>,
        icon: Option<String>,
    ) -> Self {
        Self::Manual {
            id: new_manual_id(),
            title: title.into(),
            target: target.into(),
            args: args.into(),
            icon,
        }
    }

    pub fn play_key(&self) -> String {
        match self {
            Self::Steam { appid } => format!("steam:{appid}"),
            Self::Manual { id, .. } => format!("manual:{id}"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GamesCatalog {
    #[serde(default)]
    pub entries: Vec<GameEntry>,
    /// Unix millis of last successful launch, keyed by [`GameEntry::play_key`].
    #[serde(default)]
    pub last_played_ms: BTreeMap<String, u64>,
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

    pub fn add_manual(
        &mut self,
        title: String,
        target: String,
        args: String,
        icon: Option<String>,
    ) {
        self.entries
            .push(GameEntry::manual_with(title, target, args, icon));
    }

    pub fn update_manual(
        &mut self,
        id: &str,
        title: String,
        args: String,
        icon: Option<String>,
    ) -> bool {
        for entry in &mut self.entries {
            if let GameEntry::Manual {
                id: mid,
                title: t,
                args: a,
                icon: ic,
                ..
            } = entry
                && mid == id
            {
                *t = title;
                *a = args;
                *ic = icon;
                return true;
            }
        }
        false
    }

    pub fn remove_manual_id(&mut self, id: &str) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|e| !matches!(e, GameEntry::Manual { id: mid, .. } if mid == id));
        if self.entries.len() != before {
            self.last_played_ms.remove(&format!("manual:{id}"));
            true
        } else {
            false
        }
    }

    pub fn touch_played_key(&mut self, key: impl Into<String>) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.last_played_ms.insert(key.into(), now_ms);
    }

    pub fn last_played(&self, entry: &GameEntry) -> Option<u64> {
        self.last_played_ms.get(&entry.play_key()).copied()
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

    /// Curated catalog sorted for browse mode.
    pub fn merge_sorted(
        &self,
        installed_steam: Option<&[u32]>,
        mode: GamesSortMode,
        title_of: impl Fn(&GameEntry) -> String,
    ) -> Vec<GameEntry> {
        let mut entries = self.merge_for_display(installed_steam);
        match mode {
            GamesSortMode::Alphabetical => {
                entries.sort_by_key(|e| title_of(e).to_lowercase());
            }
            GamesSortMode::LastPlayed => {
                entries.sort_by(|a, b| {
                    let pa = self.last_played(a).unwrap_or(0);
                    let pb = self.last_played(b).unwrap_or(0);
                    pb.cmp(&pa)
                        .then_with(|| title_of(a).to_lowercase().cmp(&title_of(b).to_lowercase()))
                });
            }
        }
        entries
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
        assert!(catalog.last_played_ms.is_empty());
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
    fn update_manual_edits_fields() {
        let mut catalog = GamesCatalog::default();
        catalog.add_manual("A".into(), "a.exe".into(), String::new(), None);
        let id = match &catalog.entries[0] {
            GameEntry::Manual { id, .. } => id.clone(),
            _ => panic!("manual"),
        };
        assert!(catalog.update_manual(
            &id,
            "Bee".into(),
            "--flag".into(),
            Some(r"C:\icon.png".into()),
        ));
        match &catalog.entries[0] {
            GameEntry::Manual {
                title,
                args,
                icon,
                target,
                ..
            } => {
                assert_eq!(title, "Bee");
                assert_eq!(args, "--flag");
                assert_eq!(icon.as_deref(), Some(r"C:\icon.png"));
                assert_eq!(target, "a.exe");
            }
            _ => panic!("manual"),
        }
        assert!(!catalog.update_manual("missing", "X".into(), String::new(), None));
    }

    #[test]
    fn title_from_target_uses_stem() {
        assert_eq!(title_from_target(r"C:\Games\Cool Game.exe"), "Cool Game");
        assert!(title_from_target("steam://rungameid/1").starts_with("Steam:"));
    }

    #[test]
    fn last_played_sort_puts_recent_first() {
        let mut catalog = GamesCatalog::default();
        catalog.toggle_steam(1, true);
        catalog.toggle_steam(2, true);
        catalog.add_manual("Zebra".into(), "z.exe".into(), String::new(), None);
        let z_key = catalog.entries[2].play_key();
        catalog.last_played_ms.insert("steam:2".into(), 200);
        catalog.last_played_ms.insert(z_key, 100);

        let title = |e: &GameEntry| match e {
            GameEntry::Steam { appid } => format!("Steam {appid}"),
            GameEntry::Manual { title, .. } => title.clone(),
        };
        let sorted = catalog.merge_sorted(None, GamesSortMode::LastPlayed, title);
        assert_eq!(
            sorted
                .iter()
                .map(|e| match e {
                    GameEntry::Steam { appid } => format!("s{appid}"),
                    GameEntry::Manual { title, .. } => title.clone(),
                })
                .collect::<Vec<_>>(),
            vec!["s2", "Zebra", "s1"]
        );

        let alpha = catalog.merge_sorted(None, GamesSortMode::Alphabetical, title);
        assert_eq!(
            alpha
                .iter()
                .map(|e| match e {
                    GameEntry::Steam { appid } => format!("Steam {appid}"),
                    GameEntry::Manual { title, .. } => title.clone(),
                })
                .collect::<Vec<_>>(),
            vec!["Steam 1", "Steam 2", "Zebra"]
        );
    }

    #[test]
    fn touch_played_and_remove_manual_clears_timestamp() {
        let mut catalog = GamesCatalog::default();
        catalog.toggle_steam(7, true);
        catalog.add_manual("Solo".into(), "solo.exe".into(), String::new(), None);
        let manual = catalog.entries[1].clone();
        catalog.touch_played_key(GameEntry::Steam { appid: 7 }.play_key());
        catalog.touch_played_key(manual.play_key());
        assert!(
            catalog
                .last_played(&GameEntry::Steam { appid: 7 })
                .is_some()
        );
        assert!(catalog.last_played(&manual).is_some());
        let id = match &manual {
            GameEntry::Manual { id, .. } => id.clone(),
            _ => panic!("manual"),
        };
        assert!(catalog.remove_manual_id(&id));
        assert!(!catalog.last_played_ms.contains_key(&format!("manual:{id}")));
    }
}
