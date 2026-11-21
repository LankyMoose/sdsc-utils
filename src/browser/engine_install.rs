//! Optional BrowserEngine package install (phase 5).
//!
//! Store/packaged builds use the platform installer path in `app::browser`;
//! this module is the unpackaged/dev fallback: download a zipped CEF runtime
//! from `SDSC_CEF_DLC_URL`, verify `SDSC_CEF_DLC_SHA256`, and install it into
//! a versioned directory under `%LOCALAPPDATA%/sdsc-utils`.
//!
//! A running CEF maps `libcef.dll` and cannot have its directory replaced.
//! Every install therefore lands in a fresh `cef-engine-<stamp>` folder and
//! publishes it through `cef-engine.current`. The previous directory is
//! scheduled for deletion after the next boot; reinstall works in the same
//! process even while the old runtime is still loaded.
//!
//! No URL/hash is compiled in: the DLC feed is deployment configuration, not
//! source. Without both env vars the install UI reports that the package is
//! not configured instead of downloading something arbitrary.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

const LOCAL_ENGINE_PREFIX: &str = "cef-engine";
const LOCAL_LEGACY_DIR: &str = "cef-engine";
const LOCAL_POINTER_FILE: &str = "cef-engine.current";
const LOCAL_STAGING_DIR: &str = "cef-engine.staging";
const LOCAL_PENDING_FILE: &str = "cef-engine.uninstall";

/// Install phases reported to the hub while the engine DLC is prepared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallProgress {
    Starting,
    Downloading { received: u64, total: Option<u64> },
    Verifying { done: u64, total: u64 },
    Extracting { done: usize, total: usize },
    Finishing,
}

/// Install from the dev DLC feed.
pub fn install_from_env() -> Result<PathBuf, String> {
    install_from_env_with_progress(|_| {})
}

/// Install from the dev DLC feed, reporting download/verify/extract progress
/// to the caller (the hub renders it live).
pub fn install_from_env_with_progress(
    mut progress: impl FnMut(InstallProgress),
) -> Result<PathBuf, String> {
    progress(InstallProgress::Starting);
    let url = std::env::var("SDSC_CEF_DLC_URL")
        .map_err(|_| "Browser engine download is not configured (SDSC_CEF_DLC_URL)".to_string())?;
    let expected = std::env::var("SDSC_CEF_DLC_SHA256")
        .map_err(|_| "Browser engine hash is not configured (SDSC_CEF_DLC_SHA256)".to_string())?;
    if url.trim().is_empty() || expected.trim().is_empty() {
        return Err("Browser engine download is not configured".to_string());
    }

    let data = crate::persist::paths::data_dir();
    let staging = data.join(LOCAL_STAGING_DIR);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("create staging dir: {e}"))?;

    let archive = staging.join("cef-engine.zip");
    download(&url, &archive, &mut progress)?;
    let actual = sha256_file_with_progress(&archive, &mut progress)?;
    if !actual.eq_ignore_ascii_case(expected.trim()) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!(
            "Browser engine hash mismatch (expected {expected}, got {actual})"
        ));
    }
    extract_zip(&archive, &staging, &mut progress)?;
    let _ = std::fs::remove_file(&archive);
    progress(InstallProgress::Finishing);

    let Some(runtime) = find_runtime_root(&staging) else {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("Downloaded package does not contain a CEF runtime".to_string());
    };
    let previous = active_local_dir(&data);
    let target = unique_local_target(&data);
    if runtime == staging {
        std::fs::rename(&staging, &target).map_err(|e| format!("install runtime: {e}"))?;
    } else {
        std::fs::create_dir_all(&target).map_err(|e| format!("create target dir: {e}"))?;
        copy_dir(&runtime, &target)?;
        let _ = std::fs::remove_dir_all(&staging);
    }
    if !crate::browser::cef_backend::validate_engine_dir(&target) {
        let _ = std::fs::remove_dir_all(&target);
        return Err("Installed files do not form a valid CEF runtime".to_string());
    }
    if let Err(err) = write_pointer(&data, &target) {
        let _ = std::fs::remove_dir_all(&target);
        return Err(err);
    }
    schedule_stale_dirs(&data, &target, previous.as_deref());
    Ok(target)
}

/// The active local engine: the pointer target when present, otherwise the
/// legacy single `cef-engine` directory.
pub(crate) fn active_local_dir(data: &Path) -> Option<PathBuf> {
    let pointer = data.join(LOCAL_POINTER_FILE);
    if pointer.is_file()
        && let Ok(raw) = std::fs::read_to_string(&pointer)
        && !raw.trim().is_empty()
    {
        let path = local_engine_path(data, raw.trim())?;
        return crate::browser::cef_backend::validate_engine_dir(&path).then_some(path);
    }
    let legacy = data.join(LOCAL_LEGACY_DIR);
    crate::browser::cef_backend::validate_engine_dir(&legacy).then_some(legacy)
}

fn local_engine_path(data: &Path, raw: &str) -> Option<PathBuf> {
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        data.join(path)
    };
    (path.starts_with(data) && path != data).then_some(path)
}

fn unique_local_target(data: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    let mut target = data.join(format!("{LOCAL_ENGINE_PREFIX}-{stamp}"));
    let mut suffix = 1;
    while target.exists() {
        target = data.join(format!("{LOCAL_ENGINE_PREFIX}-{stamp}-{suffix}"));
        suffix += 1;
    }
    target
}

fn write_pointer(data: &Path, target: &Path) -> Result<(), String> {
    std::fs::write(
        data.join(LOCAL_POINTER_FILE),
        target.to_string_lossy().as_bytes(),
    )
    .map_err(|e| format!("write engine pointer: {e}"))
}

fn is_engine_dir(data: &Path, path: &Path) -> bool {
    path.starts_with(data)
        && path != data
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(LOCAL_ENGINE_PREFIX))
}

/// Remove every old local runtime except `keep`. The runtime currently mapped
/// into this process is only scheduled (deleting its files could break the
/// live browser).
fn schedule_stale_dirs(data: &Path, keep: &Path, previous: Option<&Path>) {
    let loaded = crate::browser::cef_backend::loaded_engine_dir();
    let Ok(entries) = std::fs::read_dir(data) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || path == keep || !is_engine_dir(data, &path) {
            continue;
        }
        let name = entry.file_name();
        if name.to_string_lossy() == LOCAL_STAGING_DIR {
            continue;
        }
        let is_live = previous.is_some_and(|old| old == path.as_path())
            || loaded.as_deref() == Some(path.as_path());
        let removed = !is_live && std::fs::remove_dir_all(&path).is_ok();
        if !removed {
            schedule_pending(data, &path);
        }
    }
}

fn schedule_pending(data: &Path, path: &Path) {
    if !is_engine_dir(data, path) {
        return;
    }
    let marker = data.join(LOCAL_PENDING_FILE);
    let mut lines: Vec<String> = std::fs::read_to_string(&marker)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    let line = path.to_string_lossy().to_string();
    if !lines.iter().any(|existing| existing == &line) {
        lines.push(line);
    }
    let _ = std::fs::write(marker, lines.join("\n"));
}

fn download(
    url: &str,
    dest: &Path,
    progress: &mut impl FnMut(InstallProgress),
) -> Result<(), String> {
    let response = ureq::get(url)
        .header("User-Agent", "sdsc-utils")
        .config()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .build()
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let expected = response.body().content_length();
    progress(InstallProgress::Downloading {
        received: 0,
        total: expected,
    });
    let mut reader = response.into_body().into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| format!("create archive: {e}"))?;
    let mut buf = vec![0u8; 1 << 20];
    let step = expected
        .map(|total| (total / 100).max(64 * 1024))
        .unwrap_or(4 << 20);
    let mut received = 0u64;
    let mut last_report = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("download read: {e}"))?;
        if n == 0 {
            break;
        }
        received += n as u64;
        if received > MAX_ARCHIVE_BYTES {
            return Err("Browser engine archive is unexpectedly large".to_string());
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("download write: {e}"))?;
        if received.saturating_sub(last_report) >= step {
            progress(InstallProgress::Downloading {
                received,
                total: expected,
            });
            last_report = received;
        }
    }
    progress(InstallProgress::Downloading {
        received,
        total: expected,
    });
    Ok(())
}

fn sha256_file_with_progress(
    path: &Path,
    progress: &mut impl FnMut(InstallProgress),
) -> Result<String, String> {
    let total = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    progress(InstallProgress::Verifying { done: 0, total });
    let mut file = std::fs::File::open(path).map_err(|e| format!("open archive: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let step = (total / 100).max(1 << 20);
    let mut done = 0u64;
    let mut last_report = 0u64;
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("hash read: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        if done.saturating_sub(last_report) >= step {
            progress(InstallProgress::Verifying { done, total });
            last_report = done;
        }
    }
    progress(InstallProgress::Verifying { done, total });
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn extract_zip(
    archive: &Path,
    dest: &Path,
    progress: &mut impl FnMut(InstallProgress),
) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| format!("open archive: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("read archive: {e}"))?;
    let total = zip.len();
    progress(InstallProgress::Extracting { done: 0, total });
    let mut last_percent = usize::MAX;
    for i in 0..total {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| format!("archive entry {i}: {e}"))?;
        let Some(path) = entry.enclosed_name() else {
            return Err(format!("archive entry {i} escapes the install directory"));
        };
        let out = dest.join(path);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| format!("create dir: {e}"))?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
            }
            let mut out_file =
                std::fs::File::create(&out).map_err(|e| format!("create file: {e}"))?;
            std::io::copy(&mut entry, &mut out_file).map_err(|e| format!("extract file: {e}"))?;
        }
        let done = i + 1;
        let percent = (done * 100).checked_div(total).unwrap_or(100);
        if percent != last_percent {
            progress(InstallProgress::Extracting { done, total });
            last_percent = percent;
        }
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

/// Remove a locally installed engine. The active pointer is dropped
/// immediately (the hub reports Missing); the directory itself is deferred
/// until the next boot because the running process has `libcef.dll` mapped.
/// Reinstall works now because a new versioned directory is used.
pub fn uninstall_local(path: &Path) -> Result<(), String> {
    let data = crate::persist::paths::data_dir();
    if !is_engine_dir(&data, path) {
        return Err(
            "This engine lives outside the app data folder; remove its cef folder manually"
                .to_string(),
        );
    }
    let pointer = data.join(LOCAL_POINTER_FILE);
    if let Ok(raw) = std::fs::read_to_string(&pointer)
        && local_engine_path(&data, raw.trim()).as_deref() == Some(path)
    {
        let _ = std::fs::remove_file(&pointer);
    }
    schedule_pending(&data, path);
    Ok(())
}

/// Run before CEF initializes. Deletes scheduled engine directories after the
/// previous process released its DLLs.
pub fn cleanup_pending_uninstall() {
    let data = crate::persist::paths::data_dir();
    let marker = data.join(LOCAL_PENDING_FILE);
    if !marker.is_file() {
        return;
    }
    let raw = std::fs::read_to_string(&marker).unwrap_or_default();
    let mut remaining = Vec::new();
    for line in raw.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let path = PathBuf::from(line);
        if !is_engine_dir(&data, &path) {
            continue;
        }
        if std::fs::remove_dir_all(&path).is_err() {
            remaining.push(line.to_string());
        }
    }
    if remaining.is_empty() {
        let _ = std::fs::remove_file(marker);
    } else {
        let _ = std::fs::write(marker, remaining.join("\n"));
    }
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

    fn fake_runtime(root: &Path) {
        std::fs::create_dir_all(root.join("locales")).unwrap();
        for file in ["libcef.dll", "icudtl.dat", "resources.pak"] {
            std::fs::write(root.join(file), b"x").unwrap();
        }
        std::fs::write(root.join("locales").join("en-US.pak"), b"x").unwrap();
    }

    #[test]
    fn finds_wrapped_runtime_directory() {
        let root = temp_dir("cef-wrap");
        let inner = root.join("cef_windows_x86_64");
        fake_runtime(&inner);
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

    #[test]
    fn pointer_selects_versioned_install_and_legacy_falls_back() {
        let root = temp_dir("cef-pointer");
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let legacy = data.join(LOCAL_LEGACY_DIR);
        fake_runtime(&legacy);
        assert_eq!(active_local_dir(&data), Some(legacy));
        let versioned = data.join("cef-engine-1");
        fake_runtime(&versioned);
        write_pointer(&data, &versioned).unwrap();
        assert_eq!(active_local_dir(&data), Some(versioned));
        std::fs::write(data.join(LOCAL_POINTER_FILE), b"not an engine").unwrap();
        assert_eq!(active_local_dir(&data), None);
        let _ = std::fs::remove_dir_all(root);
    }
}
