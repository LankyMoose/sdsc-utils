//! Windows autostart helpers.
//!
//! Portable builds use a Startup-folder `.lnk`. Packaged (MSIX / Store) builds use
//! the `desktop:StartupTask` declared in `packaging/AppxManifest.xml`.

use crate::platform::app_log;
use crate::platform::app_meta::PKG_NAME;
use crate::platform::packaged;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// TaskId must match `packaging/AppxManifest.xml` `desktop:StartupTask`.
#[cfg(windows)]
const STARTUP_TASK_ID: &str = "SdscUtilsStartup";

pub fn is_enabled() -> bool {
    #[cfg(windows)]
    {
        if packaged::is_packaged() {
            return packaged_is_enabled();
        }
        migrate_legacy_cmd();
        startup_lnk_path()
            .map(|path| path.exists())
            .unwrap_or(false)
    }

    #[cfg(not(windows))]
    {
        false
    }
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    if enabled { install() } else { uninstall() }
}

/// If an older Startup `.cmd` entry exists, replace it with a silent `.lnk`.
/// Safe to call on every launch. No-op when packaged.
pub fn ensure_quiet_entry() {
    #[cfg(windows)]
    {
        if packaged::is_packaged() {
            return;
        }
        migrate_legacy_cmd();
    }
}

pub fn install() -> Result<(), String> {
    #[cfg(not(windows))]
    {
        return Err("autostart install is only supported on Windows".into());
    }

    #[cfg(windows)]
    {
        if packaged::is_packaged() {
            return packaged_set_enabled(true);
        }

        let exe = env::current_exe().map_err(|e| e.to_string())?;
        let lnk_path = startup_lnk_path()?;
        if let Some(parent) = lnk_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }

        let work_dir = exe
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        create_shortcut(&lnk_path, &exe, &work_dir)?;
        remove_if_exists(&startup_cmd_path()?)?;

        app_log::info(format!("installed autostart at {}", lnk_path.display()));
        Ok(())
    }
}

pub fn uninstall() -> Result<(), String> {
    #[cfg(not(windows))]
    {
        return Err("autostart uninstall is only supported on Windows".into());
    }

    #[cfg(windows)]
    {
        if packaged::is_packaged() {
            return packaged_set_enabled(false);
        }

        let lnk_path = startup_lnk_path()?;
        let cmd_path = startup_cmd_path()?;
        let mut removed_any = false;

        if remove_if_exists(&lnk_path)? {
            app_log::info(format!("removed autostart at {}", lnk_path.display()));
            removed_any = true;
        }
        if remove_if_exists(&cmd_path)? {
            app_log::info(format!(
                "removed legacy autostart at {}",
                cmd_path.display()
            ));
            removed_any = true;
        }
        if !removed_any {
            app_log::info("autostart entry was not present");
        }
        Ok(())
    }
}

#[cfg(windows)]
fn packaged_is_enabled() -> bool {
    match packaged_task_state() {
        Ok(state) => state == PackagedStartupState::Enabled,
        Err(err) => {
            app_log::warn(format!("packaged autostart state: {err}"));
            false
        }
    }
}

#[cfg(windows)]
fn packaged_set_enabled(enabled: bool) -> Result<(), String> {
    use windows::ApplicationModel::{StartupTask, StartupTaskState};
    use windows::core::HSTRING;

    let task = StartupTask::GetAsync(&HSTRING::from(STARTUP_TASK_ID))
        .map_err(|e| format!("StartupTask::GetAsync: {e}"))?
        .get()
        .map_err(|e| format!("StartupTask::GetAsync wait: {e}"))?;

    if enabled {
        let state = task
            .RequestEnableAsync()
            .map_err(|e| format!("RequestEnableAsync: {e}"))?
            .get()
            .map_err(|e| format!("RequestEnableAsync wait: {e}"))?;
        match state {
            StartupTaskState::Enabled | StartupTaskState::EnabledByPolicy => {
                app_log::info("packaged StartupTask enabled");
                Ok(())
            }
            StartupTaskState::DisabledByUser => {
                Err("startup was disabled in Task Manager; re-enable it there first".into())
            }
            StartupTaskState::DisabledByPolicy => {
                Err("startup is disabled by system policy".into())
            }
            other => Err(format!("could not enable startup (state={other:?})")),
        }
    } else {
        task.Disable()
            .map_err(|e| format!("StartupTask::Disable: {e}"))?;
        app_log::info("packaged StartupTask disabled");
        Ok(())
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackagedStartupState {
    Enabled,
    Disabled,
}

#[cfg(windows)]
fn packaged_task_state() -> Result<PackagedStartupState, String> {
    use windows::ApplicationModel::{StartupTask, StartupTaskState};
    use windows::core::HSTRING;

    let task = StartupTask::GetAsync(&HSTRING::from(STARTUP_TASK_ID))
        .map_err(|e| format!("StartupTask::GetAsync: {e}"))?
        .get()
        .map_err(|e| format!("StartupTask::GetAsync wait: {e}"))?;
    let state = task
        .State()
        .map_err(|e| format!("StartupTask::State: {e}"))?;
    Ok(match state {
        StartupTaskState::Enabled | StartupTaskState::EnabledByPolicy => {
            PackagedStartupState::Enabled
        }
        _ => PackagedStartupState::Disabled,
    })
}

/// Replace a leftover Startup `.cmd` with a silent `.lnk` (fixes console flash on login).
#[cfg(windows)]
fn migrate_legacy_cmd() {
    let Ok(cmd_path) = startup_cmd_path() else {
        return;
    };
    let Ok(lnk_path) = startup_lnk_path() else {
        return;
    };
    if !cmd_path.exists() {
        return;
    }
    if let Err(err) = install() {
        app_log::warn(format!(
            "failed to migrate legacy autostart {} → {}: {err}",
            cmd_path.display(),
            lnk_path.display()
        ));
    }
}

#[cfg(windows)]
fn create_shortcut(lnk: &Path, target: &Path, work_dir: &Path) -> Result<(), String> {
    let script = format!(
        "$s = (New-Object -ComObject WScript.Shell).CreateShortcut('{}'); \
         $s.TargetPath = '{}'; \
         $s.WorkingDirectory = '{}'; \
         $s.Save()",
        ps_single_quoted(&lnk.to_string_lossy()),
        ps_single_quoted(&target.to_string_lossy()),
        ps_single_quoted(&work_dir.to_string_lossy()),
    );

    let status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| format!("failed to run powershell for shortcut: {e}"))?;

    if !status.success() {
        return Err(format!(
            "powershell failed creating shortcut (exit {status})"
        ));
    }
    if !lnk.exists() {
        return Err("shortcut was not created".into());
    }
    Ok(())
}

#[cfg(windows)]
fn ps_single_quoted(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(windows)]
fn remove_if_exists(path: &Path) -> Result<bool, String> {
    if path.exists() {
        fs::remove_file(path).map_err(|e| e.to_string())?;
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(windows)]
fn startup_lnk_path() -> Result<PathBuf, String> {
    Ok(startup_dir()?.join(format!("{PKG_NAME}.lnk")))
}

#[cfg(windows)]
fn startup_cmd_path() -> Result<PathBuf, String> {
    Ok(startup_dir()?.join(format!("{PKG_NAME}.cmd")))
}

#[cfg(windows)]
fn startup_dir() -> Result<PathBuf, String> {
    let appdata = env::var_os("APPDATA").ok_or("APPDATA not set")?;
    Ok(PathBuf::from(appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup"))
}
