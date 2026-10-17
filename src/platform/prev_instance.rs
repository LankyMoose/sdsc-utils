//! Takeover: a fresh tray launch terminates the running instance instead of
//! exiting quietly, then takes over the single-instance guard.
//!
//! Only tray mode takes over — CLI flags (`--help`, `--list-controllers`,
//! …) return before this runs. The internal `--replace` child (renderer-pref
//! reload) does not kill either; its parent is already exiting gracefully,
//! so it only waits for the guard to free up.

use crate::platform::app_log;
use std::path::{Path, PathBuf};

/// Terminate other processes running this same exe. Best-effort: a
/// predecessor that already exited, or one we lack rights to kill, just
/// means the guard retry in the caller decides the outcome.
pub fn terminate_previous_instances() {
    let current_exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            app_log::warn(format!("takeover: current_exe failed: {err}"));
            return;
        }
    };
    let current_pid = std::process::id();
    let victims = previous_pids(&current_exe, current_pid);
    if victims.is_empty() {
        return;
    }
    app_log::info(format!(
        "takeover: replacing {} previous instance(s)",
        victims.len()
    ));
    for pid in victims {
        kill_pid(pid);
    }
}

fn previous_pids(current_exe: &Path, current_pid: u32) -> Vec<u32> {
    #[cfg(windows)]
    {
        previous_pids_windows(current_exe, current_pid)
    }
    #[cfg(target_os = "linux")]
    {
        previous_pids_linux(current_exe, current_pid)
    }
    #[cfg(target_os = "macos")]
    {
        previous_pids_macos(current_exe, current_pid)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = (current_exe, current_pid);
        Vec::new()
    }
}

/// Lowercased image path with uniform separators for comparison.
fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase().replace('/', "\\")
}

/// Lowercased exe file name (`sdsc-utils.exe`).
fn exe_file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// True when `candidate` is the same running image: exact path match, or the
/// same exe file name as a fallback so a dev build replaces an installed one
/// (and vice versa). The single-instance mutex is keyed by package name, not
/// path, so any same-named holder is a predecessor.
fn same_image(candidate: &Path, current_exe: &Path) -> bool {
    if path_key(candidate) == path_key(current_exe) {
        return true;
    }
    let (candidate_name, current_name) = (exe_file_name(candidate), exe_file_name(current_exe));
    !candidate_name.is_empty() && candidate_name == current_name
}

#[cfg(target_os = "linux")]
fn previous_pids_linux(current_exe: &Path, current_pid: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(pid): Result<u32, _> = entry.file_name().to_string_lossy().parse() else {
            continue;
        };
        if pid == current_pid {
            continue;
        }
        // Rebuild-while-running leaves `… (deleted)` on the old image link.
        let target = match std::fs::read_link(format!("/proc/{pid}/exe")) {
            Ok(target) => target,
            Err(_) => continue,
        };
        let target_str = target.to_string_lossy();
        let trimmed = PathBuf::from(strip_deleted_suffix(&target_str));
        if same_image(&trimmed, current_exe) {
            out.push(pid);
        }
    }
    out
}

/// Strip the ` (deleted)` marker Linux appends to `/proc/<pid>/exe` when the
/// binary was replaced while the process runs.
#[cfg(any(target_os = "linux", test))]
fn strip_deleted_suffix(target: &str) -> &str {
    target.strip_suffix(" (deleted)").unwrap_or(target)
}

#[cfg(target_os = "macos")]
fn previous_pids_macos(current_exe: &Path, current_pid: u32) -> Vec<u32> {
    let want = exe_file_name(current_exe);
    if want.is_empty() {
        return Vec::new();
    }
    let output = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,comm="])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_ps_line)
        .filter(|(pid, comm)| *pid != current_pid && exe_file_name(Path::new(comm)) == want)
        .map(|(pid, _)| pid)
        .collect()
}

/// Parse one `ps -o pid=,comm=` line (`  123 sdsc-utils`).
#[cfg(any(target_os = "macos", test))]
fn parse_ps_line(line: &str) -> Option<(u32, String)> {
    let mut parts = line.split_whitespace();
    let pid: u32 = parts.next()?.parse().ok()?;
    let comm = parts.next()?.to_string();
    Some((pid, comm))
}

#[cfg(windows)]
fn previous_pids_windows(current_exe: &Path, current_pid: u32) -> Vec<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::ProcessStatus::K32GetModuleFileNameExW;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        app_log::warn("takeover: process snapshot failed");
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    unsafe {
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let pid = entry.th32ProcessID;
                if pid != 0
                    && pid != current_pid
                    && let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                {
                    let mut buf = [0u16; 512];
                    let n = K32GetModuleFileNameExW(Some(handle), None, &mut buf);
                    let _ = CloseHandle(handle);
                    if n > 0 {
                        let path = PathBuf::from(String::from_utf16_lossy(&buf[..n as usize]));
                        if same_image(&path, current_exe) {
                            out.push(pid);
                        }
                    }
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

fn kill_pid(pid: u32) {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

        let Ok(handle) = (unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) }) else {
            app_log::warn(format!("takeover: cannot open pid={pid} for termination"));
            return;
        };
        let terminated = unsafe { TerminateProcess(handle, 1) }.is_ok();
        let _ = unsafe { CloseHandle(handle) };
        if terminated {
            app_log::info(format!("takeover: terminated previous instance pid={pid}"));
        } else {
            app_log::warn(format!("takeover: failed to terminate pid={pid}"));
        }
    }
    #[cfg(unix)]
    {
        match std::process::Command::new("kill")
            .arg("-KILL")
            .arg(pid.to_string())
            .status()
        {
            Ok(status) if status.success() => {
                app_log::info(format!("takeover: terminated previous instance pid={pid}"));
            }
            _ => app_log::warn(format!("takeover: failed to terminate pid={pid}")),
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_key_normalizes_case_and_separators() {
        assert_eq!(
            path_key(Path::new("C:/Program Files/App/SDSC-Utils.EXE")),
            "c:\\program files\\app\\sdsc-utils.exe"
        );
    }

    #[test]
    fn same_image_matches_exact_path_and_exe_name() {
        let current = Path::new("C:\\apps\\sdsc-utils.exe");
        assert!(same_image(Path::new("c:/apps/SDSC-UTILS.exe"), current));
        // Dev build replaces an installed one (and vice versa).
        assert!(same_image(
            Path::new("C:\\repos\\target\\debug\\sdsc-utils.exe"),
            current
        ));
        assert!(!same_image(Path::new("C:\\apps\\other.exe"), current));
        assert!(!same_image(
            Path::new("C:\\apps\\sdsc-utils-helper.exe"),
            current
        ));
    }

    #[test]
    fn strip_deleted_suffix_handles_proc_markers() {
        assert_eq!(
            strip_deleted_suffix("/tmp/target/debug/sdsc-utils (deleted)"),
            "/tmp/target/debug/sdsc-utils"
        );
        assert_eq!(
            strip_deleted_suffix("/tmp/target/debug/sdsc-utils"),
            "/tmp/target/debug/sdsc-utils"
        );
    }

    #[test]
    fn parse_ps_line_splits_pid_and_comm() {
        assert_eq!(
            parse_ps_line("  123 sdsc-utils"),
            Some((123, "sdsc-utils".to_string()))
        );
        assert_eq!(parse_ps_line("not-a-line"), None);
        assert_eq!(parse_ps_line(""), None);
    }
}
