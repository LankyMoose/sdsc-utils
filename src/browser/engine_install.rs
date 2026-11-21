//! Optional BrowserEngine package install (phase 5).
//!
//! Store/packaged builds use the platform installer path in `app::browser`;
//! this module is the unpackaged/dev fallback: download a zipped CEF runtime
//! from `SDSC_CEF_DLC_URL`, verify `SDSC_CEF_DLC_SHA256`, extract it under
//! `%LOCALAPPDATA%/sdsc-utils/cef-engine`, and report success only when a
//! complete runtime layout is present.
//!
//! No URL/hash is compiled in: the DLC feed is deployment configuration, not
//! source. Without both env vars the install UI reports that the package is
//! not configured instead of downloading something arbitrary.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Install from the dev DLC feed.
pub fn install_from_env() -> Result<PathBuf, String> {
    let url = std::env::var("SDSC_CEF_DLC_URL")
        .map_err(|_| "Browser engine download is not configured (SDSC_CEF_DLC_URL)".to_string())?;
    let expected = std::env::var("SDSC_CEF_DLC_SHA256")
        .map_err(|_| "Browser engine hash is not configured (SDSC_CEF_DLC_SHA256)".to_string())?;
    if url.trim().is_empty() || expected.trim().is_empty() {
        return Err("Browser engine download is not configured".to_string());
    }

    let target = crate::persist::paths::data_dir().join("cef-engine");
    let staging = target.with_extension("staging");
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&staging).map_err(|e| format!("create staging dir: {e}"))?;

    let archive = staging.join("cef-engine.zip");
    download(&url, &archive)?;
    let actual = sha256_file(&archive)?;
    if !actual.eq_ignore_ascii_case(expected.trim()) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!(
            "Browser engine hash mismatch (expected {expected}, got {actual})"
        ));
    }
    extract_zip(&archive, &staging)?;
    let _ = std::fs::remove_file(&archive);

    let Some(runtime) = find_runtime_root(&staging) else {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("Downloaded package does not contain a CEF runtime".to_string());
    };
    if runtime == staging {
        std::fs::rename(&staging, &target).map_err(|e| format!("install runtime: {e}"))?;
    } else {
        std::fs::create_dir_all(&target).map_err(|e| format!("create target dir: {e}"))?;
        copy_dir(&runtime, &target)?;
        let _ = std::fs::remove_dir_all(&staging);
    }
    if !crate::browser::cef_backend::resolve_engine_dir().is_some_and(|p| p == target) {
        // The resolver may find another install; re-check the directory itself.
        if !crate::browser::cef_backend::validate_engine_dir(&target) {
            return Err("Installed files do not form a valid CEF runtime".to_string());
        }
    }
    Ok(target)
}

fn download(url: &str, dest: &Path) -> Result<(), String> {
    let response = ureq::get(url)
        .header("User-Agent", "sdsc-utils")
        .config()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .build()
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let mut reader = response.into_body().into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| format!("create archive: {e}"))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("download read: {e}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_ARCHIVE_BYTES {
            return Err("Browser engine archive is unexpectedly large".to_string());
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("download write: {e}"))?;
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("open archive: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("hash read: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| format!("open archive: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("read archive: {e}"))?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| format!("archive entry {i}: {e}"))?;
        let Some(path) = entry.enclosed_name() else {
            return Err(format!("archive entry {i} escapes the install directory"));
        };
        let out = dest.join(path);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| format!("create dir: {e}"))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
        }
        let mut out_file = std::fs::File::create(&out).map_err(|e| format!("create file: {e}"))?;
        std::io::copy(&mut entry, &mut out_file).map_err(|e| format!("extract file: {e}"))?;
    }
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(src).map_err(|e| format!("read dir: {e}"))? {
        let entry = entry.map_err(|e| format!("read dir entry: {e}"))?;
        let path = entry.path();
        let out = dst.join(entry.file_name());
        if path.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| format!("create dir: {e}"))?;
            copy_dir(&path, &out)?;
        } else {
            std::fs::copy(&path, &out).map_err(|e| format!("copy file: {e}"))?;
        }
    }
    Ok(())
}

/// Find the directory containing the CEF runtime (handles archives that wrap
/// everything in `cef_windows_x86_64/`).
fn find_runtime_root(root: &Path) -> Option<PathBuf> {
    if crate::browser::cef_backend::validate_engine_dir(root) {
        return Some(root.to_path_buf());
    }
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir()
            && let Some(found) = find_runtime_root(&path)
        {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sdsc-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_wrapped_runtime_directory() {
        let root = temp_dir("cef-wrap");
        let inner = root.join("cef_windows_x86_64");
        std::fs::create_dir_all(inner.join("locales")).unwrap();
        for file in ["libcef.dll", "icudtl.dat", "resources.pak"] {
            std::fs::write(inner.join(file), b"x").unwrap();
        }
        std::fs::write(inner.join("locales").join("en-US.pak"), b"x").unwrap();
        assert_eq!(find_runtime_root(&root), Some(inner));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_partial_runtime() {
        let root = temp_dir("cef-partial");
        std::fs::write(root.join("libcef.dll"), b"x").unwrap();
        assert_eq!(find_runtime_root(&root), None);
        let _ = std::fs::remove_dir_all(root);
    }
}
