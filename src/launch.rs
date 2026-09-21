//! Launch Steam URLs, executables, and shortcuts.

#[cfg(not(windows))]
use std::path::Path;
#[cfg(not(windows))]
use std::process::Command;

/// Open a catalog target (path, `.lnk`, or URL such as `steam://…`).
pub fn launch_target(target: &str) -> Result<(), String> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Err("empty launch target".into());
    }

    #[cfg(windows)]
    {
        launch_windows(trimmed)
    }
    #[cfg(not(windows))]
    {
        launch_unix(trimmed)
    }
}

#[cfg(windows)]
fn launch_windows(target: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;

    let wide: Vec<u16> = std::ffi::OsStr::new(target)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let operation: Vec<u16> = std::ffi::OsStr::new("open")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };

    // ShellExecute returns > 32 on success (as HINSTANCE cast to isize).
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!(
            "ShellExecute failed for {target} (code {})",
            result.0 as isize
        ))
    }
}

#[cfg(not(windows))]
fn launch_unix(target: &str) -> Result<(), String> {
    if looks_like_url(target) {
        #[cfg(target_os = "macos")]
        {
            Command::new("open")
                .arg(target)
                .spawn()
                .map_err(|e| format!("failed to open {target}: {e}"))?;
            return Ok(());
        }
        #[cfg(not(target_os = "macos"))]
        {
            Command::new("xdg-open")
                .arg(target)
                .spawn()
                .map_err(|e| format!("failed to open {target}: {e}"))?;
            return Ok(());
        }
    }

    let path = Path::new(target);
    Command::new(path)
        .spawn()
        .map_err(|e| format!("failed to launch {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(not(windows))]
fn looks_like_url(target: &str) -> bool {
    target.contains("://")
}

#[cfg(test)]
mod tests {
    #[test]
    fn empty_target_errors() {
        assert!(super::launch_target("").is_err());
        assert!(super::launch_target("   ").is_err());
    }
}
