//! Persisted start-screen game catalog (`games.json`).

pub mod launch;
pub mod process_match;
pub mod steam;

use crate::persist::json;
use crate::persist::prefs::GamesSortMode;
use serde::{Deserialize, Serialize};
use serde_json::Map;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// New persisted fields need `#[serde(default)]` (or the oldest-shape fixture fails).
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
    /// Last-known display names for Steam appids, keyed by appid.
    ///
    /// Refreshed from every successful library scan/seed and consulted when
    /// the live scan misses a curated appid, so uninstalled rows keep their
    /// real title instead of the `Steam {appid}` fallback. Pruned to
    /// curated-or-scanned appids so the map cannot grow without bound.
    #[serde(default)]
    pub steam_names: BTreeMap<u32, String>,
}

impl GamesCatalog {
    pub fn load() -> Self {
        let path = store_path();
        let Some(root) = crate::persist::store::load_object(&path) else {
            return Self::default();
        };
        Self::from_root(&path, root)
    }

    fn from_root(path: &Path, mut root: Map<String, serde_json::Value>) -> Self {
        let items = json::take_array(&mut root, "entries");
        let source_len = items.len();
        let (entries, skipped) = json::decode_array::<GameEntry>(items);
        if json::total_loss(path, "game entries", source_len, entries.len()) {
            return Self::default();
        }
        json::warn_skipped(path, "game entries", skipped);
        let last_played_ms = json::take_default(&mut root, "last_played_ms");
        let steam_names = json::take_default(&mut root, "steam_names");
        Self {
            entries,
            last_played_ms,
            steam_names,
        }
    }

    pub fn save(&self) {
        let path = store_path();
        crate::persist::store::save_pretty("games", &path, self);
    }

    pub fn contains_steam(&self, appid: u32) -> bool {
        self.entries
            .iter()
            .any(|e| matches!(e, GameEntry::Steam { appid: id } if *id == appid))
    }

    /// Entries-only inequality against an edit draft: true when the draft
    /// would change the saved games list on commit. `last_played_ms` (live
    /// launch stamps, including background badge confirmations) and
    /// `steam_names` (scan refreshes) are deliberately excluded — neither is
    /// an edit, and counting them would false-dirty the Save prompt.
    pub fn entries_differ(&self, draft: &Self) -> bool {
        self.entries != draft.entries
    }

    pub fn toggle_steam(&mut self, appid: u32, enabled: bool) -> bool {
        let present = self.contains_steam(appid);
        if enabled && !present {
            self.entries.push(GameEntry::Steam { appid });
            true
        } else if !enabled && present {
            self.entries
                .retain(|e| !matches!(e, GameEntry::Steam { appid: id } if *id == appid));
            self.steam_names.remove(&appid);
            true
        } else {
            false
        }
    }

    /// Remember display names from a library scan/seed. Returns true when the
    /// map changed (caller can skip the disk write otherwise).
    ///
    /// Names for appids that are neither curated nor scanned are pruned, so
    /// never-curated installs do not accumulate here forever.
    pub fn remember_steam_names(&mut self, games: &[steam::SteamGame]) -> bool {
        let mut changed = false;
        for game in games {
            let name = game.name.trim();
            if name.is_empty() {
                continue;
            }
            match self.steam_names.get(&game.appid) {
                Some(existing) if existing == name => {}
                _ => {
                    self.steam_names.insert(game.appid, name.to_string());
                    changed = true;
                }
            }
        }
        let before = self.steam_names.len();
        let curated: Vec<u32> = self
            .entries
            .iter()
            .filter_map(|entry| match entry {
                GameEntry::Steam { appid } => Some(*appid),
                GameEntry::Manual { .. } => None,
            })
            .collect();
        self.steam_names.retain(|appid, _| {
            curated.contains(appid) || games.iter().any(|game| &game.appid == appid)
        });
        changed || self.steam_names.len() != before
    }

    /// Last-known display name for a Steam appid the live scan missed.
    pub fn steam_display_name(&self, appid: u32) -> Option<&str> {
        self.steam_names.get(&appid).map(String::as_str)
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
    ///
    /// `played_at_ms` supplies the effective last-played timestamp (milliseconds).
    /// Callers typically take the newer of catalog `last_played_ms` and Steam’s
    /// `LastPlayed` without writing Steam times back into the catalog.
    pub fn merge_sorted(
        &self,
        installed_steam: Option<&[u32]>,
        mode: GamesSortMode,
        title_of: impl Fn(&GameEntry) -> String,
        played_at_ms: impl Fn(&GameEntry) -> Option<u64>,
    ) -> Vec<GameEntry> {
        let mut entries = self.merge_for_display(installed_steam);
        match mode {
            GamesSortMode::Alphabetical => {
                entries.sort_by_key(|e| title_of(e).to_lowercase());
            }
            GamesSortMode::LastPlayed => {
                entries.sort_by(|a, b| {
                    let pa = played_at_ms(a).unwrap_or(0);
                    let pb = played_at_ms(b).unwrap_or(0);
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
    crate::persist::paths::data_dir().join("games.json")
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
    use serde_json::Value;

    fn catalog_from_str(json: &str) -> GamesCatalog {
        let Value::Object(root) = serde_json::from_str(json).unwrap() else {
            panic!("expected object");
        };
        GamesCatalog::from_root(Path::new("games.json"), root)
    }

    #[test]
    fn empty_catalog_deserializes() {
        let catalog: GamesCatalog = serde_json::from_str("{}").unwrap();
        assert!(catalog.entries.is_empty());
        assert!(catalog.last_played_ms.is_empty());
    }

    #[test]
    fn entries_differ_tracks_edits_not_background_stamps() {
        let mut saved = GamesCatalog::default();
        assert!(saved.toggle_steam(10, true));
        // Untouched draft: clean.
        let draft = saved.clone();
        assert!(!saved.entries_differ(&draft));
        // Membership toggle dirties.
        let mut toggled = saved.clone();
        assert!(toggled.toggle_steam(10, false));
        assert!(saved.entries_differ(&toggled));
        // Background stamps must NOT dirty the Save prompt: a launch stamp
        // or scan-name refresh alone keeps entries identical.
        let mut stamped = saved.clone();
        stamped.last_played_ms.insert("steam:10".to_string(), 99);
        stamped.steam_names.insert(10, "Name".to_string());
        assert!(!saved.entries_differ(&stamped));
    }

    #[test]
    fn older_games_shape_loads_and_skips_bad_entry() {
        let catalog = catalog_from_str(
            r#"{
                "entries": [
                    {"kind":"steam","appid":10},
                    {"kind":"manual","id":"m1","title":"Manual","target":"C:\\game.exe"},
                    {"kind":"steam"}
                ],
                "last_played_ms": {"steam:10": 99}
            }"#,
        );
        assert_eq!(catalog.entries.len(), 2);
        assert!(matches!(
            &catalog.entries[0],
            GameEntry::Steam { appid: 10 }
        ));
        match &catalog.entries[1] {
            GameEntry::Manual {
                id,
                title,
                target,
                args,
                icon,
            } => {
                assert_eq!(id, "m1");
                assert_eq!(title, "Manual");
                assert_eq!(target, r"C:\game.exe");
                assert!(args.is_empty());
                assert!(icon.is_none());
            }
            other => panic!("expected manual, got {other:?}"),
        }
        assert_eq!(catalog.last_played_ms.get("steam:10"), Some(&99));
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
        let played = |e: &GameEntry| catalog.last_played(e);
        let sorted = catalog.merge_sorted(None, GamesSortMode::LastPlayed, title, played);
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

        let alpha = catalog.merge_sorted(None, GamesSortMode::Alphabetical, title, played);
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

        // Steam LastPlayed (ms) wins when newer than catalog touch.
        let steam_played = |e: &GameEntry| match e {
            GameEntry::Steam { appid: 1 } => Some(500),
            _ => catalog.last_played(e),
        };
        let steam_sorted =
            catalog.merge_sorted(None, GamesSortMode::LastPlayed, title, steam_played);
        assert_eq!(
            steam_sorted
                .iter()
                .map(|e| match e {
                    GameEntry::Steam { appid } => format!("s{appid}"),
                    GameEntry::Manual { title, .. } => title.clone(),
                })
                .collect::<Vec<_>>(),
            vec!["s1", "s2", "Zebra"]
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

    fn steam_game(appid: u32, name: &str) -> steam::SteamGame {
        steam::SteamGame {
            appid,
            name: name.into(),
            icon_path: None,
            backdrop_path: None,
            playtime_minutes: None,
            last_played_unix: None,
            size_bytes: None,
            update_required: false,
        }
    }

    #[test]
    fn steam_names_roundtrip_through_json() {
        let mut catalog = GamesCatalog::default();
        catalog.toggle_steam(10, true);
        assert!(catalog.remember_steam_names(&[steam_game(10, "Counter-Strike")]));
        let json = serde_json::to_string(&catalog).unwrap();
        let back: GamesCatalog = serde_json::from_str(&json).unwrap();
        assert_eq!(back, catalog);
        assert_eq!(back.steam_display_name(10), Some("Counter-Strike"));
    }

    #[test]
    fn older_shape_without_steam_names_loads() {
        let catalog = catalog_from_str(
            r#"{
                "entries": [{"kind":"steam","appid":10}],
                "last_played_ms": {}
            }"#,
        );
        assert!(catalog.steam_names.is_empty());
        assert_eq!(catalog.steam_display_name(10), None);
    }

    #[test]
    fn remember_steam_names_upserts_and_prunes() {
        let mut catalog = GamesCatalog::default();
        catalog.toggle_steam(10, true);
        // First sight records; identical re-scan is a no-op.
        assert!(catalog.remember_steam_names(&[steam_game(10, "Old Name")]));
        assert!(!catalog.remember_steam_names(&[steam_game(10, "Old Name")]));
        // Rename updates.
        assert!(catalog.remember_steam_names(&[steam_game(10, "New Name")]));
        assert_eq!(catalog.steam_display_name(10), Some("New Name"));
        // Empty scan names are ignored, never blanking a known title.
        assert!(!catalog.remember_steam_names(&[steam_game(10, "   ")]));
        assert_eq!(catalog.steam_display_name(10), Some("New Name"));
        // Never-curated installs are remembered while scanned, pruned once
        // neither curated nor scanned.
        assert!(
            catalog
                .remember_steam_names(&[steam_game(10, "New Name"), steam_game(99, "Stranger"),])
        );
        assert_eq!(catalog.steam_display_name(99), Some("Stranger"));
        assert!(catalog.remember_steam_names(&[steam_game(10, "New Name")]));
        assert_eq!(catalog.steam_display_name(99), None);
        assert_eq!(catalog.steam_display_name(10), Some("New Name"));
        // Untoggling drops the name so uninstalled rows fall back cleanly.
        assert!(catalog.toggle_steam(10, false));
        assert_eq!(catalog.steam_display_name(10), None);
    }
}
