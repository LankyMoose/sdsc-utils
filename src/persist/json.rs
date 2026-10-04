//! Lenient JSON store loading: keep sibling records when one fails to decode.

use crate::platform::app_log;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Copy `path` to `{path}.bak` once. Leaves an existing bak alone.
pub fn backup_once(path: &Path) {
    let bak = backup_path(path);
    if bak.exists() {
        return;
    }
    match fs::copy(path, &bak) {
        Ok(_) => app_log::warn(format!(
            "backed up unreadable store {} to {}",
            path.display(),
            bak.display()
        )),
        Err(err) => app_log::warn(format!(
            "failed to back up {} to {}: {err}",
            path.display(),
            bak.display()
        )),
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(".bak");
    PathBuf::from(os)
}

/// Parse bytes as a JSON object. On failure, warn, back up once, and return `None`.
pub fn parse_object(path: &Path, bytes: &[u8]) -> Option<Map<String, Value>> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(map)) => Some(map),
        Ok(_) => {
            app_log::warn(format!(
                "store at {} is not a JSON object; starting empty",
                path.display()
            ));
            backup_once(path);
            None
        }
        Err(err) => {
            app_log::warn(format!(
                "failed to parse {} as JSON: {err}; starting empty",
                path.display()
            ));
            backup_once(path);
            None
        }
    }
}

/// Decode array elements one by one. Returns kept values and the skip count.
pub fn decode_array<T: DeserializeOwned>(items: Vec<Value>) -> (Vec<T>, usize) {
    let mut kept = Vec::with_capacity(items.len());
    let mut skipped = 0usize;
    for item in items {
        match serde_json::from_value::<T>(item) {
            Ok(value) => kept.push(value),
            Err(_) => skipped += 1,
        }
    }
    (kept, skipped)
}

/// Decode object values one by one (keys preserved). Returns kept map and skip count.
pub fn decode_map_values<T: DeserializeOwned>(
    map: Map<String, Value>,
) -> (HashMap<String, T>, usize) {
    let mut kept = HashMap::with_capacity(map.len());
    let mut skipped = 0usize;
    for (key, value) in map {
        match serde_json::from_value::<T>(value) {
            Ok(decoded) => {
                kept.insert(key, decoded);
            }
            Err(_) => skipped += 1,
        }
    }
    (kept, skipped)
}

/// Take a JSON array field, or empty when absent / wrong type.
pub fn take_array(root: &mut Map<String, Value>, key: &str) -> Vec<Value> {
    match root.remove(key) {
        Some(Value::Array(items)) => items,
        Some(_) | None => Vec::new(),
    }
}

/// Take a JSON object field, or empty when absent / wrong type.
pub fn take_object(root: &mut Map<String, Value>, key: &str) -> Map<String, Value> {
    match root.remove(key) {
        Some(Value::Object(map)) => map,
        Some(_) | None => Map::new(),
    }
}

/// Deserialize a field, or `T::default()` when absent / wrong shape.
pub fn take_default<T: DeserializeOwned + Default>(root: &mut Map<String, Value>, key: &str) -> T {
    match root.remove(key) {
        Some(value) => serde_json::from_value(value).unwrap_or_default(),
        None => T::default(),
    }
}

/// When the source collection was non-empty and nothing decoded, back up and signal empty.
///
/// Returns `true` when the caller should return an empty store.
pub fn total_loss(path: &Path, label: &str, source_len: usize, kept_len: usize) -> bool {
    if source_len > 0 && kept_len == 0 {
        app_log::warn(format!(
            "no {label} survived loading {}; backed up and starting empty",
            path.display()
        ));
        backup_once(path);
        true
    } else {
        false
    }
}

/// Warn when some records were skipped but siblings remain.
pub fn warn_skipped(path: &Path, label: &str, skipped: usize) {
    if skipped == 0 {
        return;
    }
    app_log::warn(format!(
        "skipped {skipped} unreadable {label} in {}",
        path.display()
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Debug, PartialEq, Eq, Deserialize)]
    struct Item {
        id: u32,
    }

    fn scratch_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("sdsc-utils-json-{name}-{nanos}.json"))
    }

    #[test]
    fn decode_array_keeps_siblings() {
        let items = vec![
            serde_json::json!({"id": 1}),
            serde_json::json!("bad"),
            serde_json::json!({"id": 3}),
        ];
        let (kept, skipped) = decode_array::<Item>(items);
        assert_eq!(kept, vec![Item { id: 1 }, Item { id: 3 }]);
        assert_eq!(skipped, 1);
    }

    #[test]
    fn decode_map_values_keeps_siblings() {
        let mut map = Map::new();
        map.insert("a".into(), serde_json::json!({"id": 1}));
        map.insert("b".into(), serde_json::json!(null));
        map.insert("c".into(), serde_json::json!({"id": 3}));
        let (kept, skipped) = decode_map_values::<Item>(map);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept["a"], Item { id: 1 });
        assert_eq!(kept["c"], Item { id: 3 });
        assert_eq!(skipped, 1);
    }

    #[test]
    fn backup_once_writes_bak_and_leaves_existing() {
        let path = scratch_path("backup");
        let bak = backup_path(&path);
        let _ = fs::remove_file(&bak);
        fs::write(&path, br#"{"broken""#).unwrap();
        assert!(total_loss(&path, "items", 2, 0));
        assert!(bak.exists());
        let first = fs::read(&bak).unwrap();

        fs::write(&path, b"changed").unwrap();
        backup_once(&path);
        assert_eq!(fs::read(&bak).unwrap(), first);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak);
    }

    #[test]
    fn parse_object_backs_up_invalid_json() {
        let path = scratch_path("parse");
        let bak = backup_path(&path);
        let _ = fs::remove_file(&bak);
        fs::write(&path, b"not-json").unwrap();
        assert!(parse_object(&path, b"not-json").is_none());
        assert!(bak.exists());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak);
    }
}
