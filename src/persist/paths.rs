//! Shared app data directory helpers.

use crate::platform::app_meta::PKG_NAME;
use crate::{AppError, AppResult};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// Host-provided data directory (the `Hello.data_dir` contract for hosted
/// apps). Set once, before any path-dependent work.
static DATA_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Pin the process to the host's data directory.
///
/// The add-on receives this in `Hello`; honoring it keeps prefs, tabs,
/// bookmarks, and the CEF profile in the same place as the main app even when
/// the two processes have different package identities. Returns false when an
/// override was already set (first writer wins, so a process cannot be moved
/// after it starts reading files).
pub fn set_data_dir_override(dir: PathBuf) -> bool {
    DATA_DIR_OVERRIDE.set(dir).is_ok()
}

/// The host-provided directory, when one was pinned.
pub fn data_dir_override() -> Option<PathBuf> {
    DATA_DIR_OVERRIDE.get().cloned()
}

/// Directory for prefs, controllers, analytics, logs, and the Browser add-on.
pub fn data_dir() -> PathBuf {
    // Replay/QA sandbox override. Must be checked before platform defaults.
    if let Some(dir) = std::env::var_os("SDSC_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = DATA_DIR_OVERRIDE.get() {
        return dir.clone();
    }

    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join(PKG_NAME);
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join(PKG_NAME);
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Ok(config) = std::env::var("XDG_CONFIG_HOME") {
            return PathBuf::from(config).join(PKG_NAME);
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".config").join(PKG_NAME);
        }
    }

    PathBuf::from(PKG_NAME)
}

/// Create the data directory if needed and open it in the OS file manager.
pub fn open_data_folder() -> AppResult<()> {
    let dir = data_dir();
    fs::create_dir_all(&dir)
        .map_err(|e| AppError::Persist(format!("failed to create {}: {e}", dir.display())))?;

    #[cfg(windows)]
    {
        // explorer.exe often returns a non-zero exit even when it opened the folder.
        Command::new("explorer")
            .arg(&dir)
            .spawn()
            .map_err(|e| AppError::Persist(format!("failed to open {}: {e}", dir.display())))?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        let status = Command::new("open")
            .arg(&dir)
            .status()
            .map_err(|e| AppError::Persist(format!("failed to open {}: {e}", dir.display())))?;
        if status.success() {
            return Ok(());
        }
        return Err(AppError::Persist(format!("open exited with {status}")));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let status = Command::new("xdg-open")
            .arg(&dir)
            .status()
            .map_err(|e| AppError::Persist(format!("failed to open {}: {e}", dir.display())))?;
        if status.success() {
            return Ok(());
        }
        return Err(AppError::Persist(format!("xdg-open exited with {status}")));
    }
}
