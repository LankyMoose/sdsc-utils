//! Generic JSON store helpers: one pretty-print + atomic-write path for every
//! persisted file (`prefs.json`, `controllers.json`, `analytics.json`,
//! `games.json`, `steam_library.json`, `restart-ui.json`).
//!
//! Behavior-preserving by construction:
//! - reads return `Default` on missing/corrupt input (callers keep their own
//!   lenient per-record decoding via [`crate::persist::json`]);
//! - writes serialize with `serde_json::to_vec_pretty` and commit through
//!   [`crate::persist::atomic::write_atomic_logged`] (tmp file + rename), so
//!   on-disk bytes and file paths are unchanged;
//! - no debounce, no timing, no schema migration lives here.

use crate::persist::{atomic, json};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::fs;
use std::path::Path;

/// Load a whole JSON file, or `Default` on any failure (missing, I/O error,
/// corrupt JSON, shape mismatch). Warns through `app_log`, takes no backup.
pub fn load_or_default<T: DeserializeOwned + Default>(path: &Path) -> T {
    let Ok(bytes) = fs::read(path) else {
        return T::default();
    };
    match serde_json::from_slice::<T>(&bytes) {
        Ok(value) => value,
        Err(err) => {
            crate::platform::app_log::warn(format!(
                "failed to parse {}: {err}; using defaults",
                path.display()
            ));
            T::default()
        }
    }
}

/// Read `path` as a JSON object. `None` on missing/corrupt input.
/// Corrupt bodies are backed up once via [`json::backup_once`].
pub fn load_object(path: &Path) -> Option<Map<String, Value>> {
    let Ok(bytes) = fs::read(path) else {
        return None;
    };
    json::parse_object(path, &bytes)
}

/// Serialize `value` with pretty JSON and commit via tmp-file + rename.
/// Returns true on success; warns and returns false otherwise.
pub fn save_pretty<T: Serialize>(label: &str, path: &Path, value: &T) -> bool {
    let bytes = match serde_json::to_vec_pretty(value) {
        Ok(bytes) => bytes,
        Err(err) => {
            crate::platform::app_log::warn(format!("failed to serialize {label}: {err}"));
            return false;
        }
    };
    atomic::write_atomic_logged(label, path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::path::PathBuf;

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Sample {
        #[serde(default)]
        name: String,
        #[serde(default)]
        count: u32,
    }

    fn scratch(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("sdsc-store-{name}-{nanos}.json"))
    }

    #[test]
    fn free_functions_default_and_save() {
        let path = scratch("free");
        let _ = fs::remove_file(&path);
        let loaded: Sample = load_or_default(&path);
        assert_eq!(loaded, Sample::default());
        fs::write(&path, b"not-json").unwrap();
        let loaded: Sample = load_or_default(&path);
        assert_eq!(loaded, Sample::default());
        assert!(save_pretty(
            "sample",
            &path,
            &Sample {
                name: "y".into(),
                count: 1,
            }
        ));
        let object = load_object(&path).expect("object");
        assert_eq!(object["name"], Value::String("y".into()));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn load_object_missing_is_none() {
        let path = scratch("missing-object");
        let _ = fs::remove_file(&path);
        assert!(load_object(&path).is_none());
    }
}
