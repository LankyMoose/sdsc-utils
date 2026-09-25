//! Persisted per-game macro library (`macros.json`).

use crate::games::GameEntry;
use crate::games::macro_text::{
    self, GameRef, MAX_MACROS_PER_GAME, MacroDocument, ParseError, ParsedMacro,
};
use crate::platform::app_log;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacroLibrary {
    /// Keyed by [`GameEntry::play_key`].
    #[serde(default)]
    pub games: BTreeMap<String, GameMacros>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameMacros {
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub macros: Vec<MacroRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacroRecord {
    pub id: String,
    pub name: String,
    /// Action chain text (`focus > wait 100 > …`).
    pub body: String,
    #[serde(default)]
    pub meta: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportBind {
    /// Merge into this play_key.
    Target(String),
    /// `game:` matched nothing; use the open game and surface a warning.
    Fallback {
        play_key: String,
        warning: String,
    },
    Ambiguous(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportResult {
    pub play_key: String,
    pub added: usize,
    pub updated: usize,
    pub warning: Option<String>,
}

impl MacroLibrary {
    pub fn load() -> Self {
        let path = store_path();
        let Ok(bytes) = fs::read(&path) else {
            return Self::default();
        };
        match serde_json::from_slice::<MacroLibrary>(&bytes) {
            Ok(lib) => lib,
            Err(err) => {
                app_log::warn(format!(
                    "failed to parse macros at {}: {err}; starting empty",
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
            app_log::warn(format!("failed to create macros dir: {err}"));
            return;
        }
        match serde_json::to_vec_pretty(self) {
            Ok(bytes) => {
                if let Err(err) = fs::write(&path, bytes) {
                    app_log::warn(format!("failed to write macros: {err}"));
                }
            }
            Err(err) => app_log::warn(format!("failed to serialize macros: {err}")),
        }
    }

    pub fn for_game(&self, play_key: &str) -> Option<&GameMacros> {
        self.games.get(play_key)
    }

    pub fn for_game_mut(&mut self, play_key: &str) -> &mut GameMacros {
        self.games.entry(play_key.to_string()).or_default()
    }

    pub fn remove_game(&mut self, play_key: &str) -> bool {
        self.games.remove(play_key).is_some()
    }

    pub fn has_macros(&self, play_key: &str) -> bool {
        self.games
            .get(play_key)
            .is_some_and(|g| !g.macros.is_empty())
    }

    pub fn find_macro(&self, play_key: &str, id: &str) -> Option<&MacroRecord> {
        self.games.get(play_key)?.macros.iter().find(|m| m.id == id)
    }

    /// Resolve which play_key an imported document should merge into.
    pub fn resolve_import_target(
        &self,
        doc: &MacroDocument,
        open_play_key: &str,
        catalog: &[GameEntry],
        display_title: impl Fn(&GameEntry) -> String,
    ) -> ImportBind {
        let Some(game_raw) = doc.game_header() else {
            return ImportBind::Fallback {
                play_key: open_play_key.to_string(),
                warning: "document has no game: header".into(),
            };
        };
        let game = GameRef::parse(game_raw);

        if let Some(appid) = game.appid
            && catalog
                .iter()
                .any(|e| matches!(e, GameEntry::Steam { appid: id } if *id == appid))
        {
            return ImportBind::Target(format!("steam:{appid}"));
        }

        if !game.name.is_empty() {
            let matches: Vec<_> = catalog
                .iter()
                .filter(|e| match e {
                    GameEntry::Manual { title, .. } => title.eq_ignore_ascii_case(&game.name),
                    GameEntry::Steam { .. } => false,
                })
                .collect();
            match matches.as_slice() {
                [one] => return ImportBind::Target(one.play_key()),
                [] => {}
                _ => {
                    return ImportBind::Ambiguous(format!(
                        "multiple manual shortcuts titled `{}`",
                        game.name
                    ));
                }
            }
        }

        let _ = display_title;
        ImportBind::Fallback {
            play_key: open_play_key.to_string(),
            warning: format!("game: `{game_raw}` did not match a catalog row"),
        }
    }

    /// Merge a parsed document into `play_key` by macro name.
    pub fn merge_document(
        &mut self,
        play_key: &str,
        doc: MacroDocument,
    ) -> Result<ImportResult, String> {
        let game = self.for_game_mut(play_key);
        for (k, v) in doc.headers {
            game.headers.insert(k, v);
        }

        let mut added = 0usize;
        let mut updated = 0usize;
        for parsed in doc.macros {
            match merge_one(game, parsed)? {
                MergeKind::Added => added += 1,
                MergeKind::Updated => updated += 1,
            }
        }
        Ok(ImportResult {
            play_key: play_key.to_string(),
            added,
            updated,
            warning: None,
        })
    }

    pub fn export_catalog(&self, play_key: &str, fallback_game: &str) -> Option<String> {
        let game = self.games.get(play_key)?;
        if game.macros.is_empty() {
            return None;
        }
        let mut headers = game.headers.clone();
        headers
            .entry("game".into())
            .or_insert_with(|| fallback_game.to_string());
        let macros: Vec<_> = game
            .macros
            .iter()
            .map(|m| (m.name.clone(), m.body.clone(), &m.meta))
            .collect();
        Some(macro_text::export(&headers, &macros))
    }

    pub fn export_macro(&self, play_key: &str, id: &str, fallback_game: &str) -> Option<String> {
        let game = self.games.get(play_key)?;
        let record = game.macros.iter().find(|m| m.id == id)?;
        let mut headers = game.headers.clone();
        headers
            .entry("game".into())
            .or_insert_with(|| fallback_game.to_string());
        Some(macro_text::export_one(
            &headers,
            &record.name,
            &record.body,
            &record.meta,
        ))
    }

    pub fn add_macro(
        &mut self,
        play_key: &str,
        name: String,
        body: String,
    ) -> Result<String, String> {
        let steps = macro_text::parse_action(&body).map_err(|e| e.to_string())?;
        if steps.is_empty() {
            return Err("action must contain at least one step".into());
        }
        let game = self.for_game_mut(play_key);
        if game.macros.len() >= MAX_MACROS_PER_GAME {
            return Err(format!("game already has {MAX_MACROS_PER_GAME} macros"));
        }
        if game
            .macros
            .iter()
            .any(|m| m.name.eq_ignore_ascii_case(&name))
        {
            return Err(format!("macro `{name}` already exists"));
        }
        let id = new_id();
        game.macros.push(MacroRecord {
            id: id.clone(),
            name,
            body,
            meta: BTreeMap::new(),
        });
        Ok(id)
    }

    pub fn update_macro(
        &mut self,
        play_key: &str,
        id: &str,
        name: String,
        body: String,
    ) -> Result<(), String> {
        let _ = macro_text::parse_action(&body).map_err(|e| e.to_string())?;
        let game = self.games.get_mut(play_key).ok_or("game has no macros")?;
        if game
            .macros
            .iter()
            .any(|m| m.id != id && m.name.eq_ignore_ascii_case(&name))
        {
            return Err(format!("macro `{name}` already exists"));
        }
        let record = game
            .macros
            .iter_mut()
            .find(|m| m.id == id)
            .ok_or("macro not found")?;
        record.name = name;
        record.body = body;
        Ok(())
    }

    pub fn remove_macro(&mut self, play_key: &str, id: &str) -> bool {
        let Some(game) = self.games.get_mut(play_key) else {
            return false;
        };
        let before = game.macros.len();
        game.macros.retain(|m| m.id != id);
        if game.macros.is_empty() && game.headers.is_empty() {
            self.games.remove(play_key);
            return before > 0;
        }
        before != game.macros.len()
    }

    /// Ensure `game:` header matches the catalog row when exporting.
    pub fn ensure_game_header(&mut self, play_key: &str, game_value: String) {
        let game = self.for_game_mut(play_key);
        game.headers.insert("game".into(), game_value);
    }
}

enum MergeKind {
    Added,
    Updated,
}

fn merge_one(game: &mut GameMacros, parsed: ParsedMacro) -> Result<MergeKind, String> {
    if let Some(existing) = game
        .macros
        .iter_mut()
        .find(|m| m.name.eq_ignore_ascii_case(&parsed.name))
    {
        existing.name = parsed.name;
        existing.body = parsed.action;
        existing.meta = parsed.meta;
        return Ok(MergeKind::Updated);
    }
    if game.macros.len() >= MAX_MACROS_PER_GAME {
        return Err(format!("game already has {MAX_MACROS_PER_GAME} macros"));
    }
    game.macros.push(MacroRecord {
        id: new_id(),
        name: parsed.name,
        body: parsed.action,
        meta: parsed.meta,
    });
    Ok(MergeKind::Added)
}

fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("m{nanos:x}")
}

fn store_path() -> PathBuf {
    crate::persist::paths::data_dir().join("macros.json")
}

/// Parse paste text; returns the document or a parse error.
pub fn parse_import(text: &str) -> Result<MacroDocument, ParseError> {
    macro_text::parse(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::games::GameEntry;

    fn steam(appid: u32) -> GameEntry {
        GameEntry::Steam { appid }
    }

    fn manual(title: &str) -> GameEntry {
        GameEntry::manual_with(title, "x.exe", "", None)
    }

    #[test]
    fn merge_single_keeps_others() {
        let mut lib = MacroLibrary::default();
        lib.add_macro(
            "steam:238960",
            "Remaining".into(),
            "focus > press enter".into(),
        )
        .unwrap();
        let doc = parse_import(
            r#"game: 238960|Path of Exile
name: Hideout
action: focus > type "/hideout"
"#,
        )
        .unwrap();
        let result = lib.merge_document("steam:238960", doc).unwrap();
        assert_eq!(result.added, 1);
        assert_eq!(lib.for_game("steam:238960").unwrap().macros.len(), 2);
    }

    #[test]
    fn merge_updates_same_name_keeps_id() {
        let mut lib = MacroLibrary::default();
        let id = lib
            .add_macro("steam:1", "Hideout".into(), "focus > wait 10".into())
            .unwrap();
        let doc = parse_import(
            r#"game: 1|Test
name: Hideout
action: focus > wait 20
"#,
        )
        .unwrap();
        let result = lib.merge_document("steam:1", doc).unwrap();
        assert_eq!(result.updated, 1);
        assert_eq!(result.added, 0);
        let rec = lib.find_macro("steam:1", &id).unwrap();
        assert_eq!(rec.id, id);
        assert_eq!(rec.body, "focus > wait 20");
    }

    #[test]
    fn resolve_prefers_steam_appid() {
        let lib = MacroLibrary::default();
        let catalog = vec![steam(238960), manual("Path of Exile")];
        let doc = parse_import(
            r#"game: 238960|Path of Exile
name: Hideout
action: focus
"#,
        )
        .unwrap();
        match lib.resolve_import_target(&doc, "manual:x", &catalog, |_| String::new()) {
            ImportBind::Target(key) => assert_eq!(key, "steam:238960"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn resolve_manual_by_name() {
        let lib = MacroLibrary::default();
        let catalog = vec![manual("Path of Exile")];
        let doc = parse_import(
            r#"game: 238960|Path of Exile
name: Hideout
action: focus
"#,
        )
        .unwrap();
        match lib.resolve_import_target(&doc, "steam:1", &catalog, |_| String::new()) {
            ImportBind::Target(key) => assert!(key.starts_with("manual:")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn resolve_ambiguous_manuals() {
        let lib = MacroLibrary::default();
        let catalog = vec![manual("Path of Exile"), manual("Path of Exile")];
        let doc = parse_import(
            r#"game: |Path of Exile
name: Hideout
action: focus
"#,
        )
        .unwrap();
        assert!(matches!(
            lib.resolve_import_target(&doc, "open", &catalog, |_| String::new()),
            ImportBind::Ambiguous(_)
        ));
    }
}
