//! Launch Steam URLs, executables, and shortcuts.

use crate::AppResult;
#[cfg(not(windows))]
use std::path::Path;
use std::path::PathBuf;
#[cfg(not(windows))]
use std::process::Command;

/// Open a catalog target (path, `.lnk`, or URL such as `steam://…`).
///
/// `args` is passed through to the process when non-empty (Windows: ShellExecute
/// parameters; Unix: whitespace-split argv for file targets).
pub fn launch_target(target: &str, args: &str) -> AppResult<()> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Err("empty launch target".into());
    }
    let args = args.trim();

    #[cfg(windows)]
    {
        launch_windows(trimmed, args)
    }
    #[cfg(not(windows))]
    {
        launch_unix(trimmed, args)
    }
}

#[cfg(windows)]
fn launch_windows(target: &str, args: &str) -> AppResult<()> {
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
    let params: Option<Vec<u16>> = if args.is_empty() {
        None
    } else {
        Some(
            std::ffi::OsStr::new(args)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect(),
        )
    };

    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(wide.as_ptr()),
            params
                .as_ref()
                .map(|p| PCWSTR(p.as_ptr()))
                .unwrap_or(PCWSTR::null()),
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
fn launch_unix(target: &str, args: &str) -> AppResult<()> {
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
    let mut cmd = Command::new(path);
    if !args.is_empty() {
        for arg in args.split_whitespace() {
            cmd.arg(arg);
        }
    }
    cmd.spawn()
        .map_err(|e| format!("failed to launch {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(not(windows))]
fn looks_like_url(target: &str) -> bool {
    target.contains("://")
}

/// Ask matching processes to close (WM_CLOSE), then terminate leftovers.
///
/// Moved next to [`launch_target`]: this is the catalog-launch teardown half,
/// kept in the launch module instead of the match-policy module.
pub fn close_matching(match_paths: &[PathBuf]) -> AppResult<()> {
    #[cfg(windows)]
    {
        close_matching_windows(match_paths)
    }
    #[cfg(not(windows))]
    {
        let _ = match_paths;
        Err("process close is Windows-only".into())
    }
}

#[cfg(windows)]
fn matching_pids(match_paths: &[PathBuf]) -> Vec<u32> {
    if match_paths.is_empty() {
        return Vec::new();
    }
    crate::games::process_match::list_process_images()
        .into_iter()
        .filter_map(|(pid, path)| {
            crate::games::process_match::path_matches(&path, match_paths).then_some(pid)
        })
        .collect()
}

#[cfg(windows)]
fn close_matching_windows(match_paths: &[PathBuf]) -> AppResult<()> {
    use std::collections::HashSet;
    use std::thread;
    use std::time::Duration;
    use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE,
    };

    let pids: HashSet<u32> = matching_pids(match_paths).into_iter().collect();
    if pids.is_empty() {
        return Ok(());
    }

    struct EnumState {
        pids: HashSet<u32>,
    }

    // SAFETY: `EnumWindows` invokes this on the calling thread with the `lparam`
    // we pass below; `state` outlives the enumeration and is only read, never
    // moved. The other `unsafe` calls take handles/pointers by value.
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let state = unsafe { &*(lparam.0 as *const EnumState) };
        if unsafe { IsWindowVisible(hwnd) }.as_bool() {
            let mut pid = 0u32;
            unsafe {
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
            }
            if state.pids.contains(&pid) {
                let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            }
        }
        true.into()
    }

    let state = EnumState { pids: pids.clone() };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&state as *const _ as isize));
    }

    thread::sleep(Duration::from_millis(800));

    for pid in matching_pids(match_paths) {
        if let Ok(handle) = unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
            let _ = unsafe { TerminateProcess(handle, 1) };
            let _ = unsafe { CloseHandle(handle) };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn empty_target_errors() {
        assert!(super::launch_target("", "").is_err());
        assert!(super::launch_target("   ", "").is_err());
    }
}
