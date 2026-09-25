//! Shared app data directory helpers.

use crate::platform::app_meta::PKG_NAME;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Directory for prefs, controllers, analytics (and on Windows, the log file).
pub fn data_dir() -> PathBuf {
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
pub fn open_data_folder() -> Result<(), String> {
    let dir = data_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;

    #[cfg(windows)]
    {
        // explorer.exe often returns a non-zero exit even when it opened the folder.
        Command::new("explorer")
            .arg(&dir)
            .spawn()
            .map_err(|e| format!("failed to open {}: {e}", dir.display()))?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        let status = Command::new("open")
            .arg(&dir)
            .status()
            .map_err(|e| format!("failed to open {}: {e}", dir.display()))?;
        if status.success() {
            return Ok(());
        }
        return Err(format!("open exited with {status}"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let status = Command::new("xdg-open")
            .arg(&dir)
            .status()
            .map_err(|e| format!("failed to open {}: {e}", dir.display()))?;
        if status.success() {
            return Ok(());
        }
        return Err(format!("xdg-open exited with {status}"));
    }
}
