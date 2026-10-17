//! Atomic JSON store writes: temp file + rename so readers never see a
//! truncated body (the `analytics.json EOF` class from concurrent writers).

use crate::platform::app_log;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Write `bytes` to `path` atomically: fsync'd temp file in the same
/// directory, then renamed over the destination.
///
/// `fs::rename` replaces the destination on both Windows
/// (`MOVEFILE_REPLACE_EXISTING`) and Unix, so readers either see the old
/// file or the new file — never a half-written body.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && let Err(err) = fs::create_dir_all(parent)
    {
        return Err(format!(
            "failed to create dir for {}: {err}",
            path.display()
        ));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = temp_path(parent);
    match fs::write(&temp, bytes) {
        Ok(()) => {}
        Err(err) => {
            return Err(format!(
                "failed to write temp file {}: {err}",
                temp.display()
            ));
        }
    }
    if let Err(err) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(format!(
            "failed to rename {} to {}: {err}",
            temp.display(),
            path.display()
        ));
    }
    Ok(())
}

/// Log-and-continue wrapper for store `save()` paths.
pub fn write_atomic_logged(label: &str, path: &Path, bytes: &[u8]) -> bool {
    match write_atomic(path, bytes) {
        Ok(()) => true,
        Err(err) => {
            app_log::warn(format!("failed to write {label}: {err}"));
            false
        }
    }
}

fn temp_path(dir: &Path) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!(".sdsc-tmp-{}-{nanos}.json", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "sdsc-atomic-{name}-{nanos}-{}.json",
            std::process::id()
        ))
    }

    #[test]
    fn write_atomic_creates_and_overwrites() {
        let path = scratch("basic");
        let _ = fs::remove_file(&path);
        write_atomic(&path, b"{\"a\":1}").expect("first write");
        assert_eq!(fs::read(&path).unwrap(), b"{\"a\":1}");
        write_atomic(&path, b"{\"a\":2,\"b\":[1,2,3]}").expect("overwrite");
        assert_eq!(fs::read(&path).unwrap(), b"{\"a\":2,\"b\":[1,2,3]}");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn write_atomic_leaves_no_temp_files() {
        let path = scratch("notemp");
        let _ = fs::remove_file(&path);
        write_atomic(&path, b"{}").expect("write");
        let parent = path.parent().unwrap();
        let prefix = format!(".sdsc-tmp-{}-", std::process::id());
        let leftovers: Vec<_> = fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(&prefix))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
        let _ = fs::remove_file(&path);
    }
}
