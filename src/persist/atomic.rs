//! Atomic JSON store writes: temp file + rename so readers never see a
//! truncated body (the `analytics.json EOF` class from concurrent writers).

use crate::AppResult;
use crate::platform::app_log;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Write `bytes` to `path` atomically: temp file in the same
/// directory, then renamed over the destination.
///
/// No `fsync` is performed: the guarantee is that readers never see a
/// half-written body, not durability across a crash (single desktop process,
/// same volume).
///
/// `fs::rename` replaces the destination on both Windows
/// (`MOVEFILE_REPLACE_EXISTING`) and Unix, so readers either see the old
/// file or the new file — never a half-written body.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> AppResult<()> {
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
        // Unique subdir per test: parallel tests share the process (and its
        // temp-file prefix), so scanning the shared temp root would see a
        // sibling test's in-flight temp file.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("sdsc-atomic-{name}-{nanos}-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        dir.join("store.json")
    }

    fn cleanup(path: &std::path::Path) {
        let _ = fs::remove_file(path);
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }

    #[test]
    fn write_atomic_creates_and_overwrites() {
        let path = scratch("basic");
        cleanup(&path);
        write_atomic(&path, b"{\"a\":1}").expect("first write");
        assert_eq!(fs::read(&path).unwrap(), b"{\"a\":1}");
        write_atomic(&path, b"{\"a\":2,\"b\":[1,2,3]}").expect("overwrite");
        assert_eq!(fs::read(&path).unwrap(), b"{\"a\":2,\"b\":[1,2,3]}");
        cleanup(&path);
    }

    #[test]
    fn write_atomic_leaves_no_temp_files() {
        let path = scratch("notemp");
        cleanup(&path);
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
        cleanup(&path);
    }
}
