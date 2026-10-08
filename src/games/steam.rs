//! Steam library discovery (Windows): registry path + VDF/ACF manifests.
//!
//! Library roots are resolved newest-format-first (nested `libraryfolders.vdf`
//! `"path"` objects → legacy flat numeric paths → `config.vdf`
//! `BaseInstallFolder_*`). App manifests, `loginusers.vdf`, and
//! `localconfig.vdf` stay on the current text reader until those layouts change;
//! bump [`LAST_KNOWN_COMPATIBLE_STEAM_VERSION`] after verifying a newer client.

use crate::platform::app_log;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Steam client package build last verified to scan successfully.
/// Bump only after a successful check against a newer client.
pub const LAST_KNOWN_COMPATIBLE_STEAM_VERSION: &str = "1788652215";

/// Steam `StateFlags` bit: update available / required.
const STATE_UPDATE_REQUIRED: u32 = 2;

/// SteamID64 base for converting to the numeric `userdata/<id>` folder.
const STEAM_ID64_BASE: u64 = 76_561_197_960_265_728;

/// Which reader contributed extra library roots beyond the Steam install.
#[cfg(debug_assertions)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibraryFolderSource {
    Nested,
    LegacyFlat,
    BaseInstall,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SteamGame {
    pub appid: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_path: Option<PathBuf>,
    /// Landscape hero/header art for immersive backdrops (when present).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backdrop_path: Option<PathBuf>,
    /// Lifetime playtime from `localconfig.vdf`, in minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playtime_minutes: Option<u32>,
    /// Newest of manifest / localconfig `LastPlayed` (unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played_unix: Option<u64>,
    /// Install size from the appmanifest (`SizeOnDisk`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// Manifest `StateFlags` has the update-required bit set.
    #[serde(default)]
    pub update_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManifestMeta {
    appid: u32,
    name: String,
    last_played_unix: Option<u64>,
    size_bytes: Option<u64>,
    update_required: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AppPlayStats {
    playtime_minutes: Option<u32>,
    last_played_unix: Option<u64>,
}

/// Returns true if `path` is a valid Steam library folder —
/// i.e. it contains a `steamapps/` subdirectory with at least one `appmanifest_*.acf` file.
pub fn validate_steam_library_path(path: &std::path::Path) -> bool {
    let steamapps = path.join("steamapps");
    let Ok(entries) = std::fs::read_dir(&steamapps) else {
        return false;
    };
    entries.flatten().any(|e| {
        let name = e.file_name();
        let s = name.to_string_lossy();
        s.starts_with("appmanifest_") && s.ends_with(".acf")
    })
}

/// Installed Steam games, sorted by name.
pub fn list_installed_games(extra_paths: &[PathBuf]) -> Result<Vec<SteamGame>, String> {
    let steam_root = steam_root()?.ok_or_else(|| "Steam install not found".to_string())?;
    let mut library_roots = library_folders(&steam_root)?;
    for path in extra_paths {
        if !validate_steam_library_path(path) {
            app_log::warn(format!(
                "steam: skipping invalid extra library path: {}",
                path.display()
            ));
            continue;
        }
        push_library_paths(
            &mut library_roots,
            std::iter::once(path.to_string_lossy().into_owned()),
        );
    }
    let play_by_id = load_play_stats(&steam_root);
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
            let Some(meta) = parse_appmanifest_meta(&text) else {
                continue;
            };
            let play = play_by_id.get(&meta.appid).cloned().unwrap_or_default();
            let last_played_unix = max_opt(meta.last_played_unix, play.last_played_unix);
            let icon_path = library_cache_icon(&steam_root, meta.appid);
            let backdrop_path = library_cache_backdrop(&steam_root, meta.appid);
            by_id.insert(
                meta.appid,
                SteamGame {
                    appid: meta.appid,
                    name: meta.name,
                    icon_path,
                    backdrop_path,
                    playtime_minutes: play.playtime_minutes.filter(|&m| m > 0),
                    last_played_unix: last_played_unix.filter(|&t| t > 0),
                    size_bytes: meta.size_bytes.filter(|&b| b > 0),
                    update_required: meta.update_required,
                },
            );
        }
    }

    let mut games: Vec<_> = by_id.into_values().collect();
    games.sort_by_key(|a| a.name.to_lowercase());

    #[cfg(debug_assertions)]
    {
        let with_play = games
            .iter()
            .filter(|g| g.playtime_minutes.is_some())
            .count();
        let with_size = games.iter().filter(|g| g.size_bytes.is_some()).count();
        let with_last = games
            .iter()
            .filter(|g| g.last_played_unix.is_some())
            .count();
        app_log::hid_trace(format!(
            "steam scan: games={} playtime={} size={} last_played={}",
            games.len(),
            with_play,
            with_size,
            with_last
        ));
    }

    Ok(games)
}

/// One labeled meta row under a Steam browse title (playtime / size / last played).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaLine {
    pub label: &'static str,
    pub value: String,
}

/// Labeled browse meta lines (no update clause). Falls back to Store / Steam when empty.
pub fn browse_meta_lines(game: &SteamGame) -> Vec<MetaLine> {
    let mut lines: Vec<MetaLine> = Vec::new();
    if let Some(unix) = game.last_played_unix.filter(|&t| t > 0)
        && let Some(date) = format_last_played_date(unix)
    {
        lines.push(MetaLine {
            label: "Last played",
            value: date,
        });
    }
    if let Some(mins) = game.playtime_minutes.filter(|&m| m > 0) {
        lines.push(MetaLine {
            label: "Played",
            value: format_playtime(mins),
        });
    }
    if lines.is_empty() {
        lines.push(MetaLine {
            label: "Store",
            value: "Steam".into(),
        });
    }
    lines
}

/// True when meta is only the empty-catalog Store / Steam fallback.
pub fn meta_lines_are_steam_fallback(lines: &[MetaLine]) -> bool {
    matches!(
        lines,
        [MetaLine {
            label: "Store",
            value
        }] if value == "Steam"
    )
}

/// Meta-only browse subtitle joined with middots (tests / legacy).
///
/// Example: `2 Oct 2025 · 82 h`. Falls back to `Steam` when empty.
pub fn browse_meta_subtitle(game: &SteamGame) -> String {
    let lines = browse_meta_lines(game);
    if meta_lines_are_steam_fallback(&lines) {
        "Steam".into()
    } else {
        lines
            .into_iter()
            .map(|line| line.value)
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// Compact browse subtitle: meta plus optional Update.
///
/// Example: `2 Oct 2025 · 82 h · Update required`.
pub fn browse_subtitle(game: &SteamGame) -> String {
    let meta = browse_meta_subtitle(game);
    if !game.update_required {
        return meta;
    }
    if meta == "Steam" {
        "Update required".into()
    } else {
        format!("{meta} · Update required")
    }
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
    #[cfg(debug_assertions)]
    let mut sources: Vec<LibraryFolderSource> = Vec::new();
    let vdf_path = steam_root.join("steamapps").join("libraryfolders.vdf");
    if let Ok(text) = fs::read_to_string(&vdf_path) {
        if push_library_paths(&mut roots, parse_libraryfolders(&text)) {
            #[cfg(debug_assertions)]
            sources.push(LibraryFolderSource::Nested);
        }
        if push_library_paths(&mut roots, parse_libraryfolders_legacy_flat(&text)) {
            #[cfg(debug_assertions)]
            sources.push(LibraryFolderSource::LegacyFlat);
        }
    }

    // Older Steam listed extra libraries only in config.vdf.
    if roots.len() == 1 {
        let config_path = steam_root.join("config").join("config.vdf");
        if let Ok(text) = fs::read_to_string(&config_path)
            && push_library_paths(&mut roots, parse_base_install_folders(&text))
        {
            #[cfg(debug_assertions)]
            sources.push(LibraryFolderSource::BaseInstall);
        }
    }

    #[cfg(debug_assertions)]
    {
        let source_labels: Vec<&str> = sources
            .iter()
            .map(|s| match s {
                LibraryFolderSource::Nested => "nested",
                LibraryFolderSource::LegacyFlat => "legacy_flat",
                LibraryFolderSource::BaseInstall => "base_install",
            })
            .collect();
        app_log::hid_trace(format!(
            "steam library folders: roots={} sources={}",
            roots.len(),
            if source_labels.is_empty() {
                "root_only".into()
            } else {
                source_labels.join(",")
            }
        ));
    }

    Ok(roots)
}

/// Append existing directory paths not already in `roots`. Returns true if any were added.
fn push_library_paths(roots: &mut Vec<PathBuf>, paths: impl IntoIterator<Item = String>) -> bool {
    let mut added = false;
    for path in paths {
        let normalized = PathBuf::from(path.replace('\\', "/"));
        if normalized.is_dir() && !roots.iter().any(|r| r == &normalized) {
            roots.push(normalized);
            added = true;
        }
    }
    added
}

fn load_play_stats(steam_root: &Path) -> BTreeMap<u32, AppPlayStats> {
    let Some(account) = resolve_account_id(steam_root) else {
        return BTreeMap::new();
    };
    let path = steam_root
        .join("userdata")
        .join(account.to_string())
        .join("config")
        .join("localconfig.vdf");
    let Ok(text) = fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    parse_localconfig_apps(&text)
}

fn resolve_account_id(steam_root: &Path) -> Option<u32> {
    let loginusers = steam_root.join("config").join("loginusers.vdf");
    if let Ok(text) = fs::read_to_string(&loginusers)
        && let Some(id) = most_recent_account_id(&text)
    {
        let path = steam_root
            .join("userdata")
            .join(id.to_string())
            .join("config")
            .join("localconfig.vdf");
        if path.is_file() {
            return Some(id);
        }
    }

    let userdata = steam_root.join("userdata");
    let Ok(entries) = fs::read_dir(&userdata) else {
        return None;
    };
    let mut ids: Vec<u32> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id: u32 = name.parse().ok()?;
            let lc = e.path().join("config").join("localconfig.vdf");
            lc.is_file().then_some(id)
        })
        .collect();
    ids.sort_unstable();
    ids.into_iter().next()
}

/// Prefer `MostRecent`, else highest `Timestamp`, else first listed user.
pub fn most_recent_account_id(loginusers_text: &str) -> Option<u32> {
    let users = parse_loginusers(loginusers_text);
    if users.is_empty() {
        return None;
    }
    if let Some((_, id)) = users
        .iter()
        .filter(|u| u.most_recent)
        .filter_map(|u| account_id_from_steam_id64(u.steam_id64).map(|id| (u, id)))
        .next()
    {
        return Some(id);
    }
    users
        .iter()
        .filter_map(|u| {
            let id = account_id_from_steam_id64(u.steam_id64)?;
            Some((u.timestamp, id))
        })
        .max_by_key(|(ts, _)| *ts)
        .map(|(_, id)| id)
}

pub fn account_id_from_steam_id64(steam_id64: u64) -> Option<u32> {
    steam_id64
        .checked_sub(STEAM_ID64_BASE)
        .and_then(|v| u32::try_from(v).ok())
}

#[derive(Debug, Clone)]
struct LoginUser {
    steam_id64: u64,
    most_recent: bool,
    timestamp: u64,
}

fn parse_loginusers(text: &str) -> Vec<LoginUser> {
    let mut users = Vec::new();
    let mut depth = 0i32;
    let mut pending_key: Option<String> = None;
    let mut current_id: Option<u64> = None;
    let mut current_depth: Option<i32> = None;
    let mut most_recent = false;
    let mut timestamp = 0u64;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "{" {
            depth += 1;
            if let Some(key) = pending_key.take()
                && let Ok(id) = key.parse::<u64>()
                && depth >= 2
            {
                current_id = Some(id);
                current_depth = Some(depth);
                most_recent = false;
                timestamp = 0;
            }
            continue;
        }
        if trimmed == "}" {
            if current_depth == Some(depth)
                && let Some(steam_id64) = current_id.take()
            {
                users.push(LoginUser {
                    steam_id64,
                    most_recent,
                    timestamp,
                });
                current_depth = None;
            }
            depth -= 1;
            pending_key = None;
            continue;
        }
        if let Some(value) = vdf_string_value(trimmed, "MostRecent") {
            if current_id.is_some() {
                most_recent = value == "1";
            }
            pending_key = None;
        } else if let Some(value) = vdf_string_value(trimmed, "Timestamp") {
            if current_id.is_some() {
                timestamp = value.parse().unwrap_or(0);
            }
            pending_key = None;
        } else if let Some(key) = vdf_key_only(trimmed) {
            pending_key = Some(key);
        } else {
            pending_key = None;
        }
    }
    users
}

/// Parse `apps` entries from `localconfig.vdf` (Playtime minutes + LastPlayed).
fn parse_localconfig_apps(text: &str) -> BTreeMap<u32, AppPlayStats> {
    let mut out: BTreeMap<u32, AppPlayStats> = BTreeMap::new();
    let mut depth = 0i32;
    let mut pending_key: Option<String> = None;
    let mut apps_depth: Option<i32> = None;
    let mut current_app: Option<u32> = None;
    let mut current_app_depth: Option<i32> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "{" {
            depth += 1;
            if let Some(key) = pending_key.take() {
                if key == "apps" && apps_depth.is_none() {
                    apps_depth = Some(depth);
                } else if let Some(ad) = apps_depth
                    && depth == ad + 1
                    && let Ok(appid) = key.parse::<u32>()
                {
                    current_app = Some(appid);
                    current_app_depth = Some(depth);
                    out.entry(appid).or_default();
                }
            }
            continue;
        }
        if trimmed == "}" {
            if current_app_depth == Some(depth) {
                current_app = None;
                current_app_depth = None;
            }
            if apps_depth == Some(depth) {
                apps_depth = None;
            }
            depth -= 1;
            pending_key = None;
            continue;
        }

        if let Some(appid) = current_app {
            if let Some(value) = vdf_string_value(trimmed, "Playtime") {
                if let Ok(mins) = value.parse::<u32>() {
                    out.entry(appid).or_default().playtime_minutes = Some(mins);
                }
                pending_key = None;
                continue;
            }
            if let Some(value) = vdf_string_value(trimmed, "LastPlayed") {
                if let Ok(ts) = value.parse::<u64>() {
                    out.entry(appid).or_default().last_played_unix = Some(ts);
                }
                pending_key = None;
                continue;
            }
        }

        if let Some(key) = vdf_key_only(trimmed) {
            pending_key = Some(key);
        } else {
            pending_key = None;
        }
    }
    out
}

fn library_cache_icon(steam_root: &Path, appid: u32) -> Option<PathBuf> {
    let cache = steam_root.join("appcache").join("librarycache");
    library_cache_icon_in(&cache, appid)
}

fn library_cache_backdrop(steam_root: &Path, appid: u32) -> Option<PathBuf> {
    let cache = steam_root.join("appcache").join("librarycache");
    library_cache_backdrop_in(&cache, appid)
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

/// Landscape art for immersive full-bleed backdrops (`library_hero` / `header`).
fn library_cache_backdrop_in(cache: &Path, appid: u32) -> Option<PathBuf> {
    let app_dir = cache.join(appid.to_string());

    let named = [
        app_dir.join("library_hero.jpg"),
        app_dir.join("library_hero.png"),
        app_dir.join("header.jpg"),
        cache.join(format!("{appid}_library_hero.jpg")),
        cache.join(format!("{appid}_header.jpg")),
    ];
    if let Some(path) = named.into_iter().find(|p| p.is_file()) {
        return Some(path);
    }

    if let Ok(entries) = fs::read_dir(&app_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            for name in ["library_hero.jpg", "library_hero.png", "header.jpg"] {
                let candidate = path.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    None
}

/// Parse modern `libraryfolders.vdf` and collect nested `"path"` values.
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

/// Parse legacy flat `libraryfolders.vdf` entries (`"1" "D:\\SteamLibrary"`).
///
/// Only numeric keys whose value looks like a directory are accepted.
fn parse_libraryfolders_legacy_flat(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = vdf_key_value(line.trim()) else {
            continue;
        };
        if !key.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if looks_like_directory_path(&value) {
            paths.push(value);
        }
    }
    paths
}

/// Parse `BaseInstallFolder_*` path values from `config/config.vdf`.
fn parse_base_install_folders(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = vdf_key_value(line.trim()) else {
            continue;
        };
        if key.starts_with("BaseInstallFolder_") && looks_like_directory_path(&value) {
            paths.push(value);
        }
    }
    paths
}

fn looks_like_directory_path(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    value.contains('\\') || value.contains('/') || (value.len() >= 2 && value.as_bytes()[1] == b':')
}

/// Parse an `appmanifest_*.acf` for appid + name.
pub fn parse_appmanifest(text: &str) -> Option<(u32, String)> {
    let meta = parse_appmanifest_meta(text)?;
    Some((meta.appid, meta.name))
}

/// Parse appid, name, size, last played, and update flag from an appmanifest.
fn parse_appmanifest_meta(text: &str) -> Option<ManifestMeta> {
    let mut appid = None;
    let mut name = None;
    let mut last_played_unix = None;
    let mut size_bytes = None;
    let mut state_flags = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(value) = vdf_string_value(trimmed, "appid") {
            appid = value.parse().ok();
        } else if let Some(value) = vdf_string_value(trimmed, "name") {
            name = Some(value);
        } else if let Some(value) = vdf_string_value(trimmed, "LastPlayed") {
            last_played_unix = value.parse().ok();
        } else if let Some(value) = vdf_string_value(trimmed, "SizeOnDisk") {
            size_bytes = value.parse().ok();
        } else if let Some(value) = vdf_string_value(trimmed, "StateFlags") {
            state_flags = value.parse::<u32>().ok();
        }
    }
    Some(ManifestMeta {
        appid: appid?,
        name: name?,
        last_played_unix,
        size_bytes,
        update_required: state_flags
            .map(|f| f & STATE_UPDATE_REQUIRED != 0)
            .unwrap_or(false),
    })
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

fn format_playtime(minutes: u32) -> String {
    if minutes < 60 {
        format!("{minutes} min")
    } else {
        format!("{} h", minutes / 60)
    }
}

fn format_last_played_date(unix_secs: u64) -> Option<String> {
    let local_secs = unix_secs_to_local(unix_secs)?;
    let days = (local_secs as i64).div_euclid(86_400);
    let (year, month, day) = ymd_from_unix_days(days)?;
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month_name = MONTHS.get((month - 1) as usize)?;
    Some(format!("{day} {month_name} {year}"))
}

fn unix_secs_to_local(unix_secs: u64) -> Option<u64> {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::Storage::FileSystem::FileTimeToLocalFileTime;

        // FILETIME is 100ns ticks since 1601-01-01 UTC.
        const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
        let ticks = unix_secs
            .checked_add(EPOCH_DIFF_SECS)?
            .checked_mul(10_000_000)?;
        let utc = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        let mut local = FILETIME::default();
        unsafe { FileTimeToLocalFileTime(&utc, &mut local) }.ok()?;
        let local_ticks = ((local.dwHighDateTime as u64) << 32) | (local.dwLowDateTime as u64);
        local_ticks
            .checked_div(10_000_000)?
            .checked_sub(EPOCH_DIFF_SECS)
    }
    #[cfg(not(windows))]
    {
        Some(unix_secs)
    }
}

/// Civil date from days since Unix epoch (proleptic Gregorian).
fn ymd_from_unix_days(days: i64) -> Option<(i32, u32, u32)> {
    // Howard Hinnant civil_from_days
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let year = i32::try_from(y).ok()?;
    let month = u32::try_from(m).ok()?;
    let day = u32::try_from(d).ok()?;
    Some((year, month, day))
}

fn max_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

fn vdf_string_value(line: &str, key: &str) -> Option<String> {
    let (found_key, value) = vdf_key_value(line)?;
    if found_key != key {
        return None;
    }
    Some(value)
}

/// `"key" "value"` on one line.
fn vdf_key_value(line: &str) -> Option<(String, String)> {
    let mut parts = line.split('"').filter(|s| !s.trim().is_empty());
    let key = parts.next()?;
    let value = parts.next()?;
    Some((unescape_vdf(key), unescape_vdf(value)))
}

fn vdf_key_only(line: &str) -> Option<String> {
    let mut parts = line.split('"').filter(|s| !s.trim().is_empty());
    let key = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some(unescape_vdf(key))
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
        // Modern nested objects must not be read as legacy flat paths.
        assert!(parse_libraryfolders_legacy_flat(text).is_empty());
    }

    #[test]
    fn parse_libraryfolders_legacy_flat_collects_directories() {
        let text = r#"
"LibraryFolders"
{
	"TimeNextStatsReport"		"1234567890"
	"ContentStatsID"		"9876543210"
	"1"		"D:\\SteamLibrary"
	"2"		"E:\\Games\\Steam"
}
"#;
        let paths = parse_libraryfolders_legacy_flat(text);
        assert_eq!(paths, vec![r"D:\SteamLibrary", r"E:\Games\Steam"]);
        assert!(parse_libraryfolders(text).is_empty());
    }

    #[test]
    fn parse_base_install_folders_collects_paths() {
        let text = r#"
"InstallConfigStore"
{
	"Software"
	{
		"Valve"
		{
			"Steam"
			{
				"BaseInstallFolder_1"		"D:\\SteamLibrary"
				"BaseInstallFolder_2"		"F:/Extra Library"
			}
		}
	}
}
"#;
        let paths = parse_base_install_folders(text);
        assert_eq!(paths, vec![r"D:\SteamLibrary", "F:/Extra Library"]);
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
    fn parse_appmanifest_meta_reads_size_played_update() {
        let text = r#"
"AppState"
{
	"appid"		"1145350"
	"name"		"Hades II"
	"StateFlags"		"6"
	"LastPlayed"		"1759385954"
	"SizeOnDisk"		"10497069117"
}
"#;
        let meta = parse_appmanifest_meta(text).unwrap();
        assert_eq!(meta.appid, 1145350);
        assert_eq!(meta.name, "Hades II");
        assert_eq!(meta.last_played_unix, Some(1_759_385_954));
        assert_eq!(meta.size_bytes, Some(10_497_069_117));
        assert!(meta.update_required);
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
    fn parse_localconfig_apps_reads_playtime() {
        let text = r#"
"UserLocalConfigStore"
{
	"Software"
	{
		"Valve"
		{
			"Steam"
			{
				"apps"
				{
					"730"
					{
						"LastPlayed"		"1714533943"
						"Playtime"		"4902"
					}
					"570"
					{
						"LastPlayed"		"1630653656"
						"Playtime"		"1149"
					}
				}
			}
		}
	}
}
"#;
        let apps = parse_localconfig_apps(text);
        assert_eq!(apps.get(&730).unwrap().playtime_minutes, Some(4902));
        assert_eq!(
            apps.get(&730).unwrap().last_played_unix,
            Some(1_714_533_943)
        );
        assert_eq!(apps.get(&570).unwrap().playtime_minutes, Some(1149));
    }

    #[test]
    fn account_id_from_steam_id64_math() {
        assert_eq!(
            account_id_from_steam_id64(76_561_198_095_830_057),
            Some(135_564_329)
        );
    }

    #[test]
    fn most_recent_account_prefers_flag_then_timestamp() {
        let text = r#"
"users"
{
	"76561198000000001"
	{
		"Timestamp"		"100"
		"MostRecent"		"0"
	}
	"76561198095830057"
	{
		"Timestamp"		"50"
		"MostRecent"		"1"
	}
}
"#;
        assert_eq!(most_recent_account_id(text), Some(135_564_329));

        let by_ts = r#"
"users"
{
	"76561198000000001"
	{
		"Timestamp"		"100"
	}
	"76561198095830057"
	{
		"Timestamp"		"200"
	}
}
"#;
        assert_eq!(most_recent_account_id(by_ts), Some(135_564_329));
    }

    #[test]
    fn browse_subtitle_formats_parts() {
        let game = SteamGame {
            appid: 730,
            name: "Counter-Strike 2".into(),
            icon_path: None,
            backdrop_path: None,
            playtime_minutes: Some(4902),
            last_played_unix: Some(1_759_385_954),
            size_bytes: Some(10_497_069_117),
            update_required: true,
        };
        let lines = browse_meta_lines(&game);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].label, "Last played");
        assert!(lines[0].value.contains("202"), "{}", lines[0].value);
        assert_eq!(lines[1].label, "Played");
        assert_eq!(lines[1].value, "81 h");
        let meta = browse_meta_subtitle(&game);
        assert!(meta.ends_with(" · 81 h"), "{meta}");
        assert!(!meta.contains("GB"), "{meta}");
        assert!(!meta.contains("Update"), "{meta}");
        let sub = browse_subtitle(&game);
        assert!(sub.starts_with(&meta), "{sub}");
        assert!(sub.ends_with(" · Update required"), "{sub}");

        let empty = SteamGame {
            appid: 1,
            name: "X".into(),
            icon_path: None,
            backdrop_path: None,
            playtime_minutes: None,
            last_played_unix: None,
            size_bytes: None,
            update_required: false,
        };
        assert!(meta_lines_are_steam_fallback(&browse_meta_lines(&empty)));
        assert_eq!(browse_meta_subtitle(&empty), "Steam");
        assert_eq!(browse_subtitle(&empty), "Steam");
        assert_eq!(
            browse_subtitle(&SteamGame {
                update_required: true,
                ..empty.clone()
            }),
            "Update required"
        );

        let mins = SteamGame {
            playtime_minutes: Some(13),
            size_bytes: Some(512_000),
            ..empty
        };
        assert_eq!(browse_subtitle(&mins), "13 min");
        let mins_lines = browse_meta_lines(&mins);
        assert_eq!(mins_lines.len(), 1);
        assert_eq!(mins_lines[0].label, "Played");
        assert_eq!(mins_lines[0].value, "13 min");
    }

    #[test]
    fn ymd_from_unix_days_known_dates() {
        // 2025-10-02 UTC
        assert_eq!(ymd_from_unix_days(20_363), Some((2025, 10, 2)));
        // 1970-01-01
        assert_eq!(ymd_from_unix_days(0), Some((1970, 1, 1)));
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
    fn library_cache_backdrop_prefers_hero_over_header() {
        let root = std::env::temp_dir().join(format!(
            "sdsc-steam-backdrop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cache = root.join("librarycache");
        let app = cache.join("570");
        fs::create_dir_all(&app).unwrap();
        let header = app.join("header.jpg");
        let hero = app.join("library_hero.jpg");
        fs::write(&header, b"header").unwrap();
        fs::write(&hero, b"hero").unwrap();
        assert_eq!(library_cache_backdrop_in(&cache, 570), Some(hero));
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

    #[test]
    fn validate_steam_library_path_cases() {
        let base = std::env::temp_dir().join(format!(
            "sdsc-steam-validate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        // (c) no steamapps/ dir.
        let no_steamapps = base.join("no-steamapps");
        fs::create_dir_all(&no_steamapps).unwrap();
        assert!(!validate_steam_library_path(&no_steamapps));

        // (b) steamapps/ exists but holds no manifest.
        let empty = base.join("empty");
        fs::create_dir_all(empty.join("steamapps")).unwrap();
        assert!(!validate_steam_library_path(&empty));

        // (a) valid library with one manifest.
        let valid = base.join("valid");
        fs::create_dir_all(valid.join("steamapps")).unwrap();
        fs::write(
            valid.join("steamapps").join("appmanifest_123.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"123\"\n\t\"name\"\t\t\"Valid Game\"\n}\n",
        )
        .unwrap();
        assert!(validate_steam_library_path(&valid));

        // (d) nonexistent path.
        assert!(!validate_steam_library_path(&base.join("does-not-exist")));

        let _ = fs::remove_dir_all(&base);
    }

    /// Extra-path scan through the real `list_installed_games`, with
    /// `HKCU\Software\Valve\Steam\SteamPath` pointed at a temp root and
    /// restored on drop.
    #[cfg(windows)]
    #[test]
    fn list_installed_games_scans_extra_path() {
        struct SteamPathGuard {
            prev: Option<String>,
        }

        impl Drop for SteamPathGuard {
            fn drop(&mut self) {
                let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
                if let Ok((key, _)) = hkcu.create_subkey(r"Software\Valve\Steam") {
                    match &self.prev {
                        Some(prev) => {
                            let _ = key.set_value("SteamPath", prev);
                        }
                        None => {
                            let _ = key.delete_value("SteamPath");
                        }
                    }
                }
            }
        }

        let base = std::env::temp_dir().join(format!(
            "sdsc-steam-extra-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let fake_root = base.join("root");
        fs::create_dir_all(fake_root.join("steamapps")).unwrap();
        let extra = base.join("extra");
        fs::create_dir_all(extra.join("steamapps")).unwrap();
        fs::write(
            extra.join("steamapps").join("appmanifest_999001.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"999001\"\n\t\"name\"\t\t\"Extra Lib Game\"\n}\n",
        )
        .unwrap();
        assert!(validate_steam_library_path(&extra));

        let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
        let prev: Option<String> = hkcu
            .open_subkey(r"Software\Valve\Steam")
            .ok()
            .and_then(|key| key.get_value("SteamPath").ok());
        {
            let (key, _) = hkcu.create_subkey(r"Software\Valve\Steam").unwrap();
            key.set_value("SteamPath", &fake_root.to_string_lossy().into_owned())
                .unwrap();
            let _guard = SteamPathGuard { prev };

            // The bogus path exercises the skip-with-warning branch.
            let bogus = base.join("bogus");
            let games = list_installed_games(&[bogus, extra]).unwrap();
            let game = games
                .iter()
                .find(|g| g.appid == 999001)
                .expect("extra-lib game scanned");
            assert_eq!(game.name, "Extra Lib Game");
        }

        let _ = fs::remove_dir_all(&base);
    }
}
