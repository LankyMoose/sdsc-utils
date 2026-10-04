//! Cached Steam library metadata for fast Start cold open.

use crate::games::steam::SteamGame;
use crate::persist::json;
use crate::persist::paths;
use crate::platform::app_log;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const CACHE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SteamLibraryCache {
    pub version: u32,
    #[serde(default)]
    pub scanned_at_ms: u64,
    #[serde(default)]
    pub steam_root: String,
    #[serde(default)]
    pub games: Vec<SteamGame>,
}

fn store_path() -> PathBuf {
    paths::data_dir().join("steam_library.json")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Drop art paths that no longer exist so cold load does not wait forever.
fn prune_missing_art_paths(entry: &mut SteamGame) {
    if entry.icon_path.as_ref().is_some_and(|p| !p.is_file()) {
        entry.icon_path = None;
    }
    if entry.backdrop_path.as_ref().is_some_and(|p| !p.is_file()) {
        entry.backdrop_path = None;
    }
}

/// Load cached library metadata. `None` on miss / bad version / empty.
pub fn load() -> Option<SteamLibraryCache> {
    let path = store_path();
    let Ok(bytes) = fs::read(&path) else {
        crate::controller::hid::diag::diag_info("ui-diag: steam library cache load hit=0 n=0");
        return None;
    };
    let Some(root) = json::parse_object(&path, &bytes) else {
        crate::controller::hid::diag::diag_info("ui-diag: steam library cache load hit=0 n=0");
        return None;
    };
    match serde_json::from_value::<SteamLibraryCache>(serde_json::Value::Object(root)) {
        Ok(mut cache) if cache.version == CACHE_VERSION && !cache.games.is_empty() => {
            for entry in &mut cache.games {
                prune_missing_art_paths(entry);
            }
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: steam library cache load hit=1 n={}",
                cache.games.len()
            ));
            Some(cache)
        }
        Ok(_) => {
            crate::controller::hid::diag::diag_info("ui-diag: steam library cache load hit=0 n=0");
            None
        }
        Err(err) => {
            app_log::warn(format!(
                "failed to decode steam library cache {}: {err}",
                path.display()
            ));
            json::backup_once(&path);
            crate::controller::hid::diag::diag_info("ui-diag: steam library cache load hit=0 n=0");
            None
        }
    }
}

/// Rewrite the on-disk cache after a successful live scan.
pub fn save(games: &[SteamGame], steam_root: Option<&Path>) {
    let path = store_path();
    if let Some(parent) = path.parent()
        && let Err(err) = fs::create_dir_all(parent)
    {
        app_log::warn(format!("failed to create steam library cache dir: {err}"));
        return;
    }
    let cache = SteamLibraryCache {
        version: CACHE_VERSION,
        scanned_at_ms: now_ms(),
        steam_root: steam_root
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        games: games.to_vec(),
    };
    match serde_json::to_vec_pretty(&cache) {
        Ok(bytes) => {
            if let Err(err) = fs::write(&path, bytes) {
                app_log::warn(format!("failed to write steam library cache: {err}"));
            } else {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: steam library cache save n={}",
                    games.len()
                ));
            }
        }
        Err(err) => app_log::warn(format!("failed to serialize steam library cache: {err}")),
    }
}
