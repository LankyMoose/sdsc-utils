//! Steam library discovery (Windows): registry path + VDF/ACF manifests.

use crate::platform::app_log;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamGame {
    pub appid: u32,
    pub name: String,
    pub icon_path: Option<PathBuf>,
}

/// Installed Steam games, sorted by name.
pub fn list_installed_games() -> Result<Vec<SteamGame>, String> {
    let steam_root = steam_root()?.ok_or_else(|| "Steam install not found".to_string())?;
    let library_roots = library_folders(&steam_root)?;
    let mut by_id: BTreeMap<u32, SteamGame> = BTreeMap::new();

    for root in library_roots {
        let steamapps = root.join("steamapps");
        let Ok(entries) = fs::read_dir(&steamapps) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with("appmanifest_") || !name.ends_with(".acf") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Some((appid, title)) = parse_appmanifest(&text) else {
                continue;
            };
            let icon_path = library_cache_icon(&steam_root, appid);
            by_id.insert(
                appid,
                SteamGame {
                    appid,
                    name: title,
                    icon_path,
                },
            );
        }
    }

    let mut games: Vec<_> = by_id.into_values().collect();
    games.sort_by_key(|a| a.name.to_lowercase());
    Ok(games)
}

pub fn steam_root() -> Result<Option<PathBuf>, String> {
    #[cfg(windows)]
    {
        use winreg::RegKey;
        use winreg::enums::HKEY_CURRENT_USER;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = match hkcu.open_subkey(r"Software\Valve\Steam") {
            Ok(key) => key,
            Err(_) => return Ok(None),
        };
        let path: String = match key.get_value("SteamPath") {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };
        let root = PathBuf::from(path.replace('/', "\\"));
        if root.is_dir() {
            Ok(Some(root))
        } else {
            Ok(None)
        }
    }
    #[cfg(not(windows))]
    {
        Ok(None)
    }
}

fn library_folders(steam_root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut roots = vec![steam_root.to_path_buf()];
    let vdf_path = steam_root.join("steamapps").join("libraryfolders.vdf");
    let Ok(text) = fs::read_to_string(&vdf_path) else {
        return Ok(roots);
    };
    for path in parse_libraryfolders(&text) {
        let normalized = PathBuf::from(path.replace('\\', "/"));
        if normalized.is_dir() && !roots.iter().any(|r| r == &normalized) {
            roots.push(normalized);
        }
    }
    Ok(roots)
}

fn library_cache_icon(steam_root: &Path, appid: u32) -> Option<PathBuf> {
    let cache = steam_root.join("appcache").join("librarycache");
    library_cache_icon_in(&cache, appid)
}

/// Resolve artwork under `appcache/librarycache` for an appid.
///
/// Prefers Steam library capsules (2:3 portrait). Modern Steam nests these under
/// `{appid}/` or `{appid}/{hash}/`; older installs used flat `{appid}_*.jpg` names.
/// Tiny 32×32 client icons are never used — they look poor at list size.
fn library_cache_icon_in(cache: &Path, appid: u32) -> Option<PathBuf> {
    let app_dir = cache.join(appid.to_string());

    let named = [
        app_dir.join("library_600x900.jpg"),
        app_dir.join("library_capsule.jpg"),
        cache.join(format!("{appid}_library_600x900.jpg")),
        cache.join(format!("{appid}_icon.jpg")),
        app_dir.join("header.jpg"),
        cache.join(format!("{appid}.jpg")),
    ];
    if let Some(path) = named.into_iter().find(|p| p.is_file()) {
        return Some(path);
    }

    // Newer clients nest capsule/header under content-hash directories.
    if let Ok(entries) = fs::read_dir(&app_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            for name in ["library_capsule.jpg", "library_600x900.jpg"] {
                let candidate = path.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    None
}

/// Parse `libraryfolders.vdf` and collect `"path"` values.
pub fn parse_libraryfolders(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(value) = vdf_string_value(trimmed, "path") {
            paths.push(value);
        }
    }
    paths
}

/// Parse an `appmanifest_*.acf` for appid + name.
pub fn parse_appmanifest(text: &str) -> Option<(u32, String)> {
    let mut appid = None;
    let mut name = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(value) = vdf_string_value(trimmed, "appid") {
            appid = value.parse().ok();
        } else if let Some(value) = vdf_string_value(trimmed, "name") {
            name = Some(value);
        }
        if appid.is_some() && name.is_some() {
            break;
        }
    }
    Some((appid?, name?))
}

/// Parse `"installdir"` from an appmanifest.
pub fn parse_appmanifest_installdir(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(value) = vdf_string_value(line.trim(), "installdir") {
            return Some(value);
        }
    }
    None
}

/// Absolute `steamapps/common/{installdir}` for an installed appid, if found.
pub fn install_dir_for_appid(appid: u32) -> Option<PathBuf> {
    let steam_root = steam_root().ok().flatten()?;
    let library_roots = library_folders(&steam_root).ok()?;
    let manifest_name = format!("appmanifest_{appid}.acf");
    for root in library_roots {
        let steamapps = root.join("steamapps");
        let manifest = steamapps.join(&manifest_name);
        let Ok(text) = fs::read_to_string(&manifest) else {
            continue;
        };
        let Some(installdir) = parse_appmanifest_installdir(&text) else {
            continue;
        };
        let path = steamapps.join("common").join(installdir);
        if path.is_dir() {
            return Some(path);
        }
    }
    None
}

fn vdf_string_value(line: &str, key: &str) -> Option<String> {
    // "key"		"value"
    let mut parts = line.split('"').filter(|s| !s.trim().is_empty());
    let found_key = parts.next()?;
    if found_key != key {
        return None;
    }
    let value = parts.next()?;
    Some(unescape_vdf(value))
}

fn unescape_vdf(value: &str) -> String {
    value
        .replace("\\\\", "\\")
        .replace("\\n", "\n")
        .replace("\\\"", "\"")
}

pub fn launch_uri(appid: u32) -> String {
    format!("steam://rungameid/{appid}")
}

/// Best-effort log helper when a scan fails mid-settings refresh.
pub fn warn_scan_error(err: &str) {
    app_log::warn(format!("steam library scan: {err}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_libraryfolders_collects_paths() {
        let text = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
		"label"		""
	}
	"1"
	{
		"path"		"D:\\SteamLibrary"
	}
}
"#;
        let paths = parse_libraryfolders(text);
        assert_eq!(paths.len(), 2);
        assert!(paths[0].contains("Steam"));
        assert_eq!(paths[1], r"D:\SteamLibrary");
    }

    #[test]
    fn parse_appmanifest_reads_id_and_name() {
        let text = r#"
"AppState"
{
	"appid"		"1245620"
	"Universe"		"1"
	"name"		"ELDEN RING"
	"StateFlags"		"4"
}
"#;
        let (id, name) = parse_appmanifest(text).unwrap();
        assert_eq!(id, 1245620);
        assert_eq!(name, "ELDEN RING");
    }

    #[test]
    fn parse_appmanifest_installdir_reads() {
        let text = r#"
"AppState"
{
	"appid"		"570"
	"installdir"		"dota 2 beta"
	"name"		"Dota 2"
}
"#;
        assert_eq!(
            parse_appmanifest_installdir(text).as_deref(),
            Some("dota 2 beta")
        );
    }

    #[test]
    fn launch_uri_format() {
        assert_eq!(launch_uri(570), "steam://rungameid/570");
    }

    #[test]
    fn library_cache_prefers_modern_app_subdir() {
        let root = std::env::temp_dir().join(format!(
            "sdsc-steam-icon-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cache = root.join("librarycache");
        let app = cache.join("1245620");
        fs::create_dir_all(&app).unwrap();
        let modern = app.join("library_600x900.jpg");
        fs::write(&modern, b"fake").unwrap();
        // Legacy flat names would have been preferred by the old resolver —
        // ensure we find the modern path when only the subdir exists.
        assert_eq!(library_cache_icon_in(&cache, 1245620), Some(modern));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn library_cache_falls_back_to_legacy_flat_icon() {
        let root = std::env::temp_dir().join(format!(
            "sdsc-steam-icon-legacy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cache = root.join("librarycache");
        fs::create_dir_all(&cache).unwrap();
        let legacy = cache.join("570_icon.jpg");
        fs::write(&legacy, b"fake").unwrap();
        assert_eq!(library_cache_icon_in(&cache, 570), Some(legacy));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn library_cache_finds_nested_capsule() {
        let root = std::env::temp_dir().join(format!(
            "sdsc-steam-icon-nested-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cache = root.join("librarycache");
        let nested = cache
            .join("570")
            .join("6843027380c3bfd0952449fd9174f492ef2e7b40");
        fs::create_dir_all(&nested).unwrap();
        let capsule = nested.join("library_capsule.jpg");
        fs::write(&capsule, b"fake").unwrap();
        assert_eq!(library_cache_icon_in(&cache, 570), Some(capsule));
        let _ = fs::remove_dir_all(&root);
    }
}
