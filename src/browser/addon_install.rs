//! Browser add-on package install/update.
//!
//! The add-on is a standalone executable. The shell installs it into
//! `<data>/browser-addon/` so it survives a rebuilt sidecar and can be updated
//! independently of the main app. A sidecar next to the main executable is the
//! unpackaged source; `SDSC_BROWSER_ADDON_SOURCE` overrides it for QA and
//! portable deployments.
//!
//! Copies are staged and renamed into place. A running add-on maps its own
//! executable on Windows, so callers must close it before installing.

use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const ADDON_DIR: &str = "browser-addon";
pub const SOURCE_ENV: &str = "SDSC_BROWSER_ADDON_SOURCE";
pub const VERSION_ENV: &str = "SDSC_BROWSER_ADDON_VERSION";
const META_FILE: &str = "addon.json";
const STAGING_DIR: &str = "browser-addon.staging";
const COPY_BUFFER: usize = 256 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Metadata persisted with an installed add-on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddonMeta {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub installed_at: u64,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub source_modified: u64,
}

/// A package available for install/update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddonSource {
    pub path: PathBuf,
    pub version: String,
    pub size: u64,
    pub modified: u64,
}

/// Current install/update state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddonStatus {
    pub installed: Option<AddonMeta>,
    /// Newer/different package next to the app or at `SOURCE_ENV`.
    pub source: Option<AddonSource>,
    pub update_ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddonProgress {
    Copying { done: u64, total: u64 },
    Finishing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddonError {
    Cancelled,
    Failed(String),
}

/// Result carried back to the shell after an install/update worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddonInstallOutcome {
    Installed(AddonMeta),
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for AddonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Install cancelled"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

pub fn exe_name() -> &'static str {
    if cfg!(windows) {
        "sdsc-browser.exe"
    } else {
        "sdsc-browser"
    }
}

pub fn installed_exe(data_dir: &Path) -> PathBuf {
    data_dir.join(ADDON_DIR).join(exe_name())
}

/// Installed add-on executable, then the development sidecar.
pub fn runtime_path() -> Option<PathBuf> {
    let data = crate::persist::paths::data_dir();
    let installed = installed_exe(&data);
    if installed.is_file() {
        return Some(installed);
    }
    sidecar_path().filter(|path| path.is_file())
}

pub fn is_installed() -> bool {
    installed_exe(&crate::persist::paths::data_dir()).is_file()
}

pub fn status() -> AddonStatus {
    status_in(&crate::persist::paths::data_dir(), source())
}

pub fn status_in(data_dir: &Path, source: Option<AddonSource>) -> AddonStatus {
    let installed = read_meta(data_dir).filter(|_| installed_exe(data_dir).is_file());
    let update_ready = installed.as_ref().is_some_and(|meta| {
        source.as_ref().is_some_and(|source| {
            meta.version != source.version
                || meta.size != source.size
                || meta.source_modified != source.modified
        })
    });
    AddonStatus {
        installed,
        source,
        update_ready,
    }
}

pub fn source() -> Option<AddonSource> {
    if let Some(path) = std::env::var_os(SOURCE_ENV) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return source_at(&path, None);
        }
    }
    sidecar_path().and_then(|path| source_at(&path, None))
}

pub fn source_at(path: &Path, version: Option<String>) -> Option<AddonSource> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some(AddonSource {
        path: path.to_path_buf(),
        version: version.unwrap_or_else(source_version),
        size: meta.len(),
        modified: meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or(0),
    })
}

pub fn source_version() -> String {
    std::env::var(VERSION_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| crate::platform::app_meta::PKG_VERSION.to_string())
}

/// Install/replace the add-on from `source`. Staging is cleaned on failure.
pub fn install(
    source: &AddonSource,
    progress: &mut impl FnMut(AddonProgress),
    cancel: &AtomicBool,
) -> Result<AddonMeta, AddonError> {
    install_in(source, &crate::persist::paths::data_dir(), progress, cancel)
}

pub fn install_in(
    source: &AddonSource,
    data_dir: &Path,
    progress: &mut impl FnMut(AddonProgress),
    cancel: &AtomicBool,
) -> Result<AddonMeta, AddonError> {
    if source.size == 0 {
        return Err(AddonError::Failed("add-on package is empty".to_string()));
    }
    let staging = data_dir.join(STAGING_DIR);
    let target = data_dir.join(ADDON_DIR);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)
        .map_err(|err| AddonError::Failed(format!("create staging dir: {err}")))?;

    let result = install_into(source, &staging, progress, cancel);
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
        return result;
    }
    let meta = result?;
    check_cancel(cancel)?;
    progress(AddonProgress::Finishing);
    if target.exists() {
        fs::remove_dir_all(&target)
            .map_err(|err| AddonError::Failed(format!("replace add-on: {err}")))?;
    }
    fs::rename(&staging, &target)
        .map_err(|err| AddonError::Failed(format!("publish add-on: {err}")))?;
    Ok(meta)
}

fn install_into(
    source: &AddonSource,
    staging: &Path,
    progress: &mut impl FnMut(AddonProgress),
    cancel: &AtomicBool,
) -> Result<AddonMeta, AddonError> {
    let target_exe = staging.join(exe_name());
    let mut input = File::open(&source.path)
        .map_err(|err| AddonError::Failed(format!("open add-on source: {err}")))?;
    let mut output = File::create(&target_exe)
        .map_err(|err| AddonError::Failed(format!("create add-on: {err}")))?;
    let mut buffer = vec![0u8; COPY_BUFFER];
    let mut copied = 0u64;
    let mut last_report = Instant::now();
    loop {
        check_cancel(cancel)?;
        let read = input
            .read(&mut buffer)
            .map_err(|err| AddonError::Failed(format!("read add-on source: {err}")))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|err| AddonError::Failed(format!("write add-on: {err}")))?;
        copied += read as u64;
        if last_report.elapsed() >= PROGRESS_INTERVAL {
            progress(AddonProgress::Copying {
                done: copied,
                total: source.size,
            });
            last_report = Instant::now();
        }
    }
    output
        .flush()
        .map_err(|err| AddonError::Failed(format!("flush add-on: {err}")))?;
    progress(AddonProgress::Copying {
        done: copied,
        total: source.size.max(copied),
    });

    let meta = AddonMeta {
        version: source.version.clone(),
        source: source.path.display().to_string(),
        installed_at: epoch_secs(),
        size: copied,
        source_modified: source.modified,
    };
    let meta_json = serde_json::to_vec_pretty(&meta)
        .map_err(|err| AddonError::Failed(format!("encode add-on metadata: {err}")))?;
    fs::write(staging.join(META_FILE), meta_json)
        .map_err(|err| AddonError::Failed(format!("write add-on metadata: {err}")))?;
    Ok(meta)
}

pub fn uninstall() -> Result<(), AddonError> {
    uninstall_in(&crate::persist::paths::data_dir())
}

pub fn uninstall_in(data_dir: &Path) -> Result<(), AddonError> {
    let target = data_dir.join(ADDON_DIR);
    let _ = fs::remove_dir_all(data_dir.join(STAGING_DIR));
    if target.exists() {
        fs::remove_dir_all(&target)
            .map_err(|err| AddonError::Failed(format!("remove add-on: {err}")))?;
    }
    Ok(())
}

pub fn read_meta(data_dir: &Path) -> Option<AddonMeta> {
    let text = fs::read_to_string(data_dir.join(ADDON_DIR).join(META_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

fn sidecar_path() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    Some(current.with_file_name(exe_name()))
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), AddonError> {
    if cancel.load(Ordering::Relaxed) {
        Err(AddonError::Cancelled)
    } else {
        Ok(())
    }
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("sdsc-addon-{name}-{}-{stamp}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn source(dir: &Path, name: &str, bytes: &[u8], version: &str) -> AddonSource {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        source_at(&path, Some(version.to_string())).unwrap()
    }

    #[test]
    fn install_status_update_uninstall_round_trip() {
        let dir = fixture("roundtrip");
        let data = dir.join("data");
        let source_v1 = source(&dir, "one.exe", b"version one payload", "1.0");
        let status = status_in(&data, Some(source_v1.clone()));
        assert!(status.installed.is_none());
        assert!(!status.update_ready);

        let mut progress = Vec::new();
        let cancel = AtomicBool::new(false);
        let meta = install_in(&source_v1, &data, &mut |p| progress.push(p), &cancel).unwrap();
        assert_eq!(meta.version, "1.0");
        assert!(installed_exe(&data).is_file());
        assert!(
            progress
                .iter()
                .any(|p| matches!(p, AddonProgress::Finishing))
        );

        let status = status_in(&data, Some(source_v1.clone()));
        assert_eq!(status.installed.as_ref().unwrap().version, "1.0");
        assert!(!status.update_ready);

        let source_v2 = source(
            &dir,
            "two.exe",
            b"version two payload with more bytes",
            "2.0",
        );
        let status = status_in(&data, Some(source_v2.clone()));
        assert!(status.update_ready);

        let status = status_in(&data, None);
        assert_eq!(status.installed.as_ref().unwrap().version, "1.0");
        assert!(!status.update_ready);

        let mut progress = Vec::new();
        let meta = install_in(&source_v2, &data, &mut |p| progress.push(p), &cancel).unwrap();
        assert_eq!(meta.version, "2.0");
        assert_eq!(
            status_in(&data, Some(source_v2)).installed.unwrap().version,
            "2.0"
        );

        uninstall_in(&data).unwrap();
        assert!(!installed_exe(&data).exists());
        assert!(status_in(&data, None).installed.is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cancelled_install_leaves_no_staging_or_installed_copy() {
        let dir = fixture("cancel");
        let data = dir.join("data");
        let source = source(&dir, "source.exe", &vec![7u8; 4096], "1.0");
        let cancel = AtomicBool::new(true);
        let err = install_in(&source, &data, &mut |_| {}, &cancel).unwrap_err();
        assert_eq!(err, AddonError::Cancelled);
        assert!(!data.join(ADDON_DIR).exists());
        assert!(!data.join(STAGING_DIR).exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn update_detection_sees_version_and_timestamp_changes() {
        let dir = fixture("update");
        let data = dir.join("data");
        let source = source(&dir, "source.exe", b"payload", "1.0");
        let cancel = AtomicBool::new(false);
        install_in(&source, &data, &mut |_| {}, &cancel).unwrap();
        assert!(!status_in(&data, Some(source.clone())).update_ready);

        let mut changed = source.clone();
        changed.version = "2.0".to_string();
        assert!(status_in(&data, Some(changed)).update_ready);
        let _ = fs::remove_dir_all(dir);
    }
}
