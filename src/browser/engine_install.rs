//! Optional BrowserEngine package install.
//!
//! Unpackaged/dev builds use this module: download a zipped CEF runtime from
//! the configured DLC feed (`SDSC_CEF_DLC_MANIFEST_URL`, or the older
//! `SDSC_CEF_DLC_URL` + `SDSC_CEF_DLC_SHA256` pair), verify it, and install it
//! into a versioned directory under `%LOCALAPPDATA%/sdsc-utils`.
//!
//! A running CEF maps `libcef.dll` and cannot have its directory replaced.
//! Every install therefore lands in a fresh `cef-engine-<stamp>` folder and
//! publishes it through `cef-engine.current`. The previous directory is
//! scheduled for deletion after the next boot; reinstall/update works in the
//! same process even while the old runtime is still loaded.
//!
//! Installs carry an `engine.json` manifest copy (version, changelog, source
//! versions, size). Home compares that against the latest feed manifest to
//! offer updates and renders the changelog in Info.
//!
//! No URL/hash is compiled in: the DLC feed is deployment configuration, not
//! source. Without configuration the install UI reports that the package is
//! not configured instead of downloading something arbitrary.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering as AtomicOrdering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const REPORT_INTERVAL: Duration = Duration::from_millis(100);
const THROTTLE_SLICE: Duration = Duration::from_millis(25);

const LOCAL_ENGINE_PREFIX: &str = "cef-engine";
const LOCAL_LEGACY_DIR: &str = "cef-engine";
const LOCAL_POINTER_FILE: &str = "cef-engine.current";
const LOCAL_STAGING_DIR: &str = "cef-engine.staging";
const LOCAL_PENDING_FILE: &str = "cef-engine.uninstall";
const LOCAL_META_FILE: &str = "engine.json";

/// Download caps offered in Home Settings. `0` means unlimited.
pub const DOWNLOAD_LIMIT_OPTIONS_KBPS: [u32; 4] = crate::app::installs::DOWNLOAD_LIMIT_OPTIONS_KBPS;

/// Latest DLC feed metadata. Served by the QA console in development and by
/// the release feed in production.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineManifest {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub cef_version: String,
    #[serde(default)]
    pub chromium_version: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub sha256: String,
    /// Download size advertised by the feed (bytes).
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub changelog: String,
}

/// Metadata persisted inside an installed runtime (`engine.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineMeta {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub cef_version: String,
    #[serde(default)]
    pub chromium_version: String,
    #[serde(default)]
    pub installed_at: u64,
    /// Measured size on disk (bytes) at install time.
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub changelog: String,
}

/// Install phases reported to Home while the engine DLC is prepared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallProgress {
    Starting,
    Downloading { received: u64, total: Option<u64> },
    Verifying { done: u64, total: u64 },
    Extracting { done: u64, total: u64 },
    Finishing,
}

/// A cooperative install cancel is not a failure; the caller restores the
/// prior state and clears staging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Install cancelled"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

#[derive(Clone)]
pub struct InstallOptions {
    /// `0` = unlimited; otherwise cap the download at this many KB/s. The
    /// atomic is shared with Home Settings, so a change applies at the next
    /// chunk boundary of a live download.
    pub download_limit_kbps: Arc<AtomicU32>,
    pub cancel: Arc<AtomicBool>,
}

impl InstallOptions {
    pub fn new(download_limit_kbps: u32, cancel: Arc<AtomicBool>) -> Self {
        Self {
            download_limit_kbps: Arc::new(AtomicU32::new(download_limit_kbps)),
            cancel,
        }
    }
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self::new(0, Arc::new(AtomicBool::new(false)))
    }
}

/// Install from the dev DLC feed.
pub fn install_from_env() -> Result<PathBuf, InstallError> {
    install_from_env_with_progress(|_| {})
}

/// Install with progress but no cap/cancel (tests and simple callers).
pub fn install_from_env_with_progress(
    mut progress: impl FnMut(InstallProgress),
) -> Result<PathBuf, InstallError> {
    install_with_progress(&mut progress, InstallOptions::default())
}

/// Install with a live progress sink, download cap, and cooperative cancel.
pub fn install_with_progress(
    progress: &mut impl FnMut(InstallProgress),
    options: InstallOptions,
) -> Result<PathBuf, InstallError> {
    progress(InstallProgress::Starting);
    check_cancel(&options.cancel)?;
    let manifest = resolve_manifest()?;
    let url = manifest.url.trim().to_string();
    let expected = manifest.sha256.trim().to_string();
    if url.is_empty() || expected.is_empty() {
        return Err(InstallError::Failed(
            "Browser engine download is not configured".to_string(),
        ));
    }

    let data = crate::persist::paths::data_dir();
    let staging = data.join(LOCAL_STAGING_DIR);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .map_err(|e| InstallError::Failed(format!("create staging dir: {e}")))?;

    let result = install_into_staging(
        &data, &staging, &manifest, &url, &expected, progress, &options,
    );
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

fn install_into_staging(
    data: &Path,
    staging: &Path,
    manifest: &EngineManifest,
    url: &str,
    expected: &str,
    progress: &mut impl FnMut(InstallProgress),
    options: &InstallOptions,
) -> Result<PathBuf, InstallError> {
    let archive = staging.join("cef-engine.zip");
    download(
        url,
        &archive,
        Arc::clone(&options.download_limit_kbps),
        &options.cancel,
        progress,
    )?;
    let actual = sha256_file_with_progress(&archive, &options.cancel, progress)?;
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(InstallError::Failed(format!(
            "Browser engine hash mismatch (expected {expected}, got {actual})"
        )));
    }
    extract_zip(&archive, staging, &options.cancel, progress)?;
    let _ = std::fs::remove_file(&archive);
    check_cancel(&options.cancel)?;
    progress(InstallProgress::Finishing);

    let Some(runtime) = find_runtime_root(staging) else {
        return Err(InstallError::Failed(
            "Downloaded package does not contain a CEF runtime".to_string(),
        ));
    };
    let previous = active_local_dir(data);
    let target = unique_local_target(data);
    if runtime == staging {
        std::fs::rename(staging, &target)
            .map_err(|e| InstallError::Failed(format!("install runtime: {e}")))?;
    } else {
        std::fs::create_dir_all(&target)
            .map_err(|e| InstallError::Failed(format!("create target dir: {e}")))?;
        if let Err(err) = copy_dir(&runtime, &target, &options.cancel) {
            let _ = std::fs::remove_dir_all(&target);
            return Err(err);
        }
        let _ = std::fs::remove_dir_all(staging);
    }
    if let Err(err) = check_cancel(&options.cancel) {
        let _ = std::fs::remove_dir_all(&target);
        return Err(err);
    }
    if !crate::browser::cef_backend::validate_engine_dir(&target) {
        let _ = std::fs::remove_dir_all(&target);
        return Err(InstallError::Failed(
            "Installed files do not form a valid CEF runtime".to_string(),
        ));
    }

    let meta = EngineMeta {
        version: manifest.version.trim().to_string(),
        cef_version: manifest.cef_version.trim().to_string(),
        chromium_version: manifest.chromium_version.trim().to_string(),
        installed_at: now_millis(),
        size: dir_size(&target),
        changelog: manifest.changelog.clone(),
    };
    if let Err(err) = write_meta(&target, &meta) {
        let _ = std::fs::remove_dir_all(&target);
        return Err(err);
    }
    if let Err(err) = write_pointer(data, &target) {
        let _ = std::fs::remove_dir_all(&target);
        return Err(err);
    }
    schedule_stale_dirs(data, &target, previous.as_deref());
    Ok(target)
}

/// The latest manifest, fetching `SDSC_CEF_DLC_MANIFEST_URL` when configured.
/// Without a manifest URL an env-only feed (`URL` + `SHA256`) still installs,
/// but carries no version/changelog.
pub fn fetch_manifest() -> Result<Option<EngineManifest>, String> {
    if let Some(raw) = std::env::var_os("SDSC_CEF_DLC_MANIFEST_URL") {
        let url = raw.to_string_lossy().trim().to_string();
        if !url.is_empty() {
            let response = ureq::get(&url)
                .header("User-Agent", "sdsc-utils")
                .config()
                .timeout_global(Some(MANIFEST_TIMEOUT))
                .build()
                .call()
                .map_err(|e| format!("engine manifest request failed: {e}"))?;
            let body = response
                .into_body()
                .read_to_string()
                .map_err(|e| format!("engine manifest read failed: {e}"))?;
            let manifest: EngineManifest = serde_json::from_str(&body)
                .map_err(|e| format!("engine manifest parse failed: {e}"))?;
            if manifest.url.trim().is_empty() || manifest.sha256.trim().is_empty() {
                return Err("engine manifest is missing url/sha256".to_string());
            }
            return Ok(Some(manifest));
        }
    }
    Ok(env_manifest())
}

fn resolve_manifest() -> Result<EngineManifest, InstallError> {
    match fetch_manifest() {
        Ok(Some(manifest)) => Ok(manifest),
        Ok(None) => Err(InstallError::Failed(
            "Browser engine download is not configured".to_string(),
        )),
        Err(err) => Err(InstallError::Failed(err)),
    }
}

fn env_manifest() -> Option<EngineManifest> {
    let url = std::env::var("SDSC_CEF_DLC_URL").ok()?;
    let sha256 = std::env::var("SDSC_CEF_DLC_SHA256").ok()?;
    if url.trim().is_empty() || sha256.trim().is_empty() {
        return None;
    }
    Some(EngineManifest {
        version: std::env::var("SDSC_CEF_DLC_VERSION").unwrap_or_default(),
        cef_version: String::new(),
        chromium_version: String::new(),
        url,
        sha256,
        size: 0,
        changelog: std::env::var("SDSC_CEF_DLC_CHANGELOG").unwrap_or_default(),
    })
}

/// True when `candidate` is a strictly newer dotted version than `current`.
/// Non-numeric versions (including empty/"dev") never claim an update.
pub fn version_is_newer(candidate: &str, current: &str) -> bool {
    matches!(
        compare_versions(candidate, current),
        Some(Ordering::Greater)
    )
}

fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    let a = version_numbers(a)?;
    let b = version_numbers(b)?;
    let len = a.len().max(b.len());
    for i in 0..len {
        let av = a.get(i).copied().unwrap_or(0);
        let bv = b.get(i).copied().unwrap_or(0);
        match av.cmp(&bv) {
            Ordering::Equal => {}
            other => return Some(other),
        }
    }
    Some(Ordering::Equal)
}

fn version_numbers(raw: &str) -> Option<Vec<u64>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let numbers: Vec<u64> = raw
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect();
    (!numbers.is_empty()).then_some(numbers)
}

/// Read the manifest copy from an installed runtime.
pub fn read_meta(dir: &Path) -> Option<EngineMeta> {
    let raw = std::fs::read(dir.join(LOCAL_META_FILE)).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn write_meta(dir: &Path, meta: &EngineMeta) -> Result<(), InstallError> {
    let raw = serde_json::to_vec_pretty(meta)
        .map_err(|e| InstallError::Failed(format!("serialize engine metadata: {e}")))?;
    std::fs::write(dir.join(LOCAL_META_FILE), raw)
        .map_err(|e| InstallError::Failed(format!("write engine metadata: {e}")))
}

/// Metadata for the active local engine, with a measured size fallback for
/// legacy installs that predate `engine.json`.
pub fn active_meta() -> Option<(PathBuf, EngineMeta)> {
    let path = active_local_dir(&crate::persist::paths::data_dir())?;
    let meta = read_meta(&path).unwrap_or_else(|| EngineMeta {
        version: "Unknown".to_string(),
        size: dir_size(&path),
        ..EngineMeta::default()
    });
    Some((path, meta))
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
    let stamp = now_millis();
    let mut target = data.join(format!("{LOCAL_ENGINE_PREFIX}-{stamp}"));
    let mut suffix = 1;
    while target.exists() {
        target = data.join(format!("{LOCAL_ENGINE_PREFIX}-{stamp}-{suffix}"));
        suffix += 1;
    }
    target
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn write_pointer(data: &Path, target: &Path) -> Result<(), InstallError> {
    std::fs::write(
        data.join(LOCAL_POINTER_FILE),
        target.to_string_lossy().as_bytes(),
    )
    .map_err(|e| InstallError::Failed(format!("write engine pointer: {e}")))
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

fn check_cancel(cancel: &AtomicBool) -> Result<(), InstallError> {
    if cancel.load(AtomicOrdering::Relaxed) {
        Err(InstallError::Cancelled)
    } else {
        Ok(())
    }
}

/// Token-bucket limiter over the download. The cap is read from a shared
/// atomic on every chunk, so Home Settings changes apply to a live download.
/// Sleeping stays in short slices so Cancel is immediate even at a low cap.
struct RateLimiter {
    limit_kbps: Arc<AtomicU32>,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    fn new(limit_kbps: Arc<AtomicU32>) -> Self {
        Self {
            limit_kbps,
            tokens: 0.0,
            last: Instant::now(),
        }
    }

    fn throttle(&mut self, bytes: u64, cancel: &AtomicBool) -> Result<(), InstallError> {
        loop {
            check_cancel(cancel)?;
            let kbps = self.limit_kbps.load(AtomicOrdering::Relaxed);
            if kbps == 0 {
                self.tokens = 0.0;
                self.last = Instant::now();
                return Ok(());
            }
            let rate = f64::from(kbps) * 1024.0;
            let now = Instant::now();
            let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
            self.last = now;
            // One second of burst absorbs scheduler jitter without letting a
            // long idle period bank unlimited tokens.
            self.tokens = (self.tokens + elapsed * rate).min(rate);
            let need = bytes as f64;
            if self.tokens >= need {
                self.tokens -= need;
                return Ok(());
            }
            let remaining = Duration::from_secs_f64((need - self.tokens) / rate);
            std::thread::sleep(remaining.min(THROTTLE_SLICE));
        }
    }
}

fn download(
    url: &str,
    dest: &Path,
    limit_kbps: Arc<AtomicU32>,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(InstallProgress),
) -> Result<(), InstallError> {
    let response = ureq::get(url)
        .header("User-Agent", "sdsc-utils")
        .config()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .build()
        .call()
        .map_err(|e| InstallError::Failed(format!("download failed: {e}")))?;
    let expected = response.body().content_length();
    progress(InstallProgress::Downloading {
        received: 0,
        total: expected,
    });
    let mut reader = response.into_body().into_reader();
    let mut file = std::fs::File::create(dest)
        .map_err(|e| InstallError::Failed(format!("create archive: {e}")))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut limiter = RateLimiter::new(limit_kbps);
    let mut received = 0u64;
    let mut last_report = Instant::now();
    loop {
        check_cancel(cancel)?;
        let n = reader
            .read(&mut buf)
            .map_err(|e| InstallError::Failed(format!("download read: {e}")))?;
        if n == 0 {
            break;
        }
        received += n as u64;
        if received > MAX_ARCHIVE_BYTES {
            return Err(InstallError::Failed(
                "Browser engine archive is unexpectedly large".to_string(),
            ));
        }
        file.write_all(&buf[..n])
            .map_err(|e| InstallError::Failed(format!("download write: {e}")))?;
        limiter.throttle(n as u64, cancel)?;
        if last_report.elapsed() >= REPORT_INTERVAL {
            progress(InstallProgress::Downloading {
                received,
                total: expected,
            });
            last_report = Instant::now();
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
    cancel: &AtomicBool,
    progress: &mut impl FnMut(InstallProgress),
) -> Result<String, InstallError> {
    let total = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    progress(InstallProgress::Verifying { done: 0, total });
    let mut file = std::fs::File::open(path)
        .map_err(|e| InstallError::Failed(format!("open archive: {e}")))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    let mut last_report = Instant::now();
    loop {
        check_cancel(cancel)?;
        let n = file
            .read(&mut buf)
            .map_err(|e| InstallError::Failed(format!("hash read: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        if last_report.elapsed() >= REPORT_INTERVAL {
            progress(InstallProgress::Verifying { done, total });
            last_report = Instant::now();
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
    cancel: &AtomicBool,
    progress: &mut impl FnMut(InstallProgress),
) -> Result<(), InstallError> {
    let file = std::fs::File::open(archive)
        .map_err(|e| InstallError::Failed(format!("open archive: {e}")))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| InstallError::Failed(format!("read archive: {e}")))?;
    let total_bytes: u64 = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|entry| entry.size()))
        .sum();
    progress(InstallProgress::Extracting {
        done: 0,
        total: total_bytes,
    });
    let mut done = 0u64;
    let mut last_report = Instant::now();
    for i in 0..zip.len() {
        check_cancel(cancel)?;
        let mut entry = zip
            .by_index(i)
            .map_err(|e| InstallError::Failed(format!("archive entry {i}: {e}")))?;
        let Some(path) = entry.enclosed_name() else {
            return Err(InstallError::Failed(format!(
                "archive entry {i} escapes the install directory"
            )));
        };
        let out = dest.join(path);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)
                .map_err(|e| InstallError::Failed(format!("create dir: {e}")))?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| InstallError::Failed(format!("create dir: {e}")))?;
            }
            let mut out_file = std::fs::File::create(&out)
                .map_err(|e| InstallError::Failed(format!("create file: {e}")))?;
            std::io::copy(&mut entry, &mut out_file)
                .map_err(|e| InstallError::Failed(format!("extract file: {e}")))?;
        }
        done += entry.size();
        if last_report.elapsed() >= REPORT_INTERVAL {
            progress(InstallProgress::Extracting {
                done,
                total: total_bytes,
            });
            last_report = Instant::now();
        }
    }
    progress(InstallProgress::Extracting {
        done: total_bytes.max(done),
        total: total_bytes.max(done),
    });
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path, cancel: &AtomicBool) -> Result<(), InstallError> {
    for entry in
        std::fs::read_dir(src).map_err(|e| InstallError::Failed(format!("read dir: {e}")))?
    {
        check_cancel(cancel)?;
        let entry = entry.map_err(|e| InstallError::Failed(format!("read dir entry: {e}")))?;
        let path = entry.path();
        let out = dst.join(entry.file_name());
        if path.is_dir() {
            std::fs::create_dir_all(&out)
                .map_err(|e| InstallError::Failed(format!("create dir: {e}")))?;
            copy_dir(&path, &out, cancel)?;
        } else {
            std::fs::copy(&path, &out)
                .map_err(|e| InstallError::Failed(format!("copy file: {e}")))?;
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

/// Total bytes under `path` (symlinks are skipped, not followed).
pub fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => total = total.saturating_add(dir_size(&path)),
            Ok(kind) if kind.is_file() => {
                total = total.saturating_add(entry.metadata().map(|meta| meta.len()).unwrap_or(0));
            }
            _ => {}
        }
    }
    total
}

/// Directories queued for deletion on the next boot.
pub fn pending_cleanup_dirs() -> Vec<PathBuf> {
    let data = crate::persist::paths::data_dir();
    let marker = data.join(LOCAL_PENDING_FILE);
    std::fs::read_to_string(&marker)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .filter(|path| is_engine_dir(&data, path) && path.is_dir())
        .collect()
}

/// Bytes still occupied by engines awaiting next-boot cleanup.
pub fn pending_cleanup_size() -> u64 {
    pending_cleanup_dirs()
        .iter()
        .map(|path| dir_size(path))
        .sum()
}

/// Remove a locally installed engine. The active pointer is dropped
/// immediately (Home reports Missing); the directory itself is deferred
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
    fn newer_versions_compare_numerically() {
        assert!(version_is_newer("1.0.1", "1.0.0"));
        assert!(version_is_newer("1.1", "1.0.9"));
        assert!(version_is_newer("v2.0.0", "1.99.99"));
        assert!(version_is_newer("2026.11.22.2", "2026.11.22.1"));
        assert!(!version_is_newer("1.0.0", "1.0.0"));
        assert!(!version_is_newer("1.0.0", "1.0.1"));
        assert!(!version_is_newer("dev", "1.0.0"));
        assert!(!version_is_newer("1.0.0", "dev"));
    }

    #[test]
    fn manifest_round_trips_and_reads_legacy_as_unknown() {
        let root = temp_dir("cef-meta");
        fake_runtime(&root);
        assert!(read_meta(&root).is_none());
        let meta = EngineMeta {
            version: "1.2.3".to_string(),
            cef_version: "156.0".to_string(),
            chromium_version: "156.0.1".to_string(),
            installed_at: 7,
            size: 42,
            changelog: "- Fixed tabs".to_string(),
        };
        write_meta(&root, &meta).unwrap();
        assert_eq!(read_meta(&root), Some(meta));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cancelled_install_returns_before_downloading() {
        let cancel = Arc::new(AtomicBool::new(true));
        let result = install_with_progress(&mut |_| {}, InstallOptions::new(0, cancel));
        assert_eq!(result, Err(InstallError::Cancelled));
    }

    #[test]
    fn dir_size_sums_nested_files() {
        let root = temp_dir("cef-size");
        std::fs::write(root.join("a"), vec![0u8; 10]).unwrap();
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested").join("b"), vec![0u8; 32]).unwrap();
        assert_eq!(dir_size(&root), 42);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rate_limiter_options_are_stable() {
        assert_eq!(DOWNLOAD_LIMIT_OPTIONS_KBPS[0], 0);
        assert_eq!(DOWNLOAD_LIMIT_OPTIONS_KBPS[3], 10 * 1024);
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
