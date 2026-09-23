//! Match and close game processes by image path (Windows).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Keep an optimistic session this long after launch even if process match fails.
pub const LAUNCH_GRACE: Duration = Duration::from_secs(45);
/// After grace, clear only after this many consecutive failed matches.
pub const MISS_CLEAR: Duration = Duration::from_secs(4);

/// Session remembered after a catalog launch for running detection / close.
#[derive(Debug, Clone)]
pub struct RunningSession {
    pub title: String,
    pub target: String,
    /// Absolute exe paths and/or directory roots to match against process images.
    pub match_paths: Vec<PathBuf>,
    pub launched_at: Instant,
    /// First consecutive miss after grace; cleared when a match is seen again.
    pub miss_since: Option<Instant>,
}

impl RunningSession {
    pub fn matches_target(&self, target: &str) -> bool {
        self.target == target
    }

    pub fn new(title: String, target: String, match_paths: Vec<PathBuf>) -> Self {
        Self {
            title,
            target,
            match_paths,
            launched_at: Instant::now(),
            miss_since: None,
        }
    }

    /// Restored from a live process — grace already expired so sticky-miss applies immediately.
    pub fn restored(title: String, target: String, match_paths: Vec<PathBuf>) -> Self {
        Self {
            title,
            target,
            match_paths,
            launched_at: Instant::now()
                .checked_sub(LAUNCH_GRACE + Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
            miss_since: None,
        }
    }
}

/// Resolve match paths for a catalog launch target (best-effort).
pub fn match_paths_for_target(target: &str) -> Vec<PathBuf> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    if let Some(appid) = steam_appid(trimmed) {
        return match crate::steam::install_dir_for_appid(appid) {
            Some(dir) => vec![dir],
            None => {
                log_missing_steam_installdir(appid);
                Vec::new()
            }
        };
    }

    let path = PathBuf::from(trimmed);
    if path.is_file() {
        return vec![path];
    }
    // Basename fallback for shortcuts / relative paths.
    if let Some(name) = path.file_name() {
        return vec![PathBuf::from(name)];
    }
    Vec::new()
}

fn log_missing_steam_installdir(appid: u32) {
    static SEEN: Mutex<Option<HashSet<u32>>> = Mutex::new(None);
    let mut guard = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let seen = guard.get_or_insert_with(HashSet::new);
    if seen.insert(appid) {
        crate::app_log::warn(format!(
            "steam installdir missing for appid {appid}; running match paths empty"
        ));
    }
}

fn steam_appid(target: &str) -> Option<u32> {
    let rest = target.strip_prefix("steam://rungameid/")?;
    rest.split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|s| s.parse().ok())
}

/// True when any running process image matches `match_paths` (exact file or under a root).
pub fn any_matching_running(match_paths: &[PathBuf]) -> bool {
    any_matching_in_images(match_paths, &list_process_images())
}

/// Same as [`any_matching_running`] against a pre-taken process snapshot.
pub fn any_matching_in_images(match_paths: &[PathBuf], images: &[(u32, PathBuf)]) -> bool {
    if match_paths.is_empty() {
        return false;
    }
    images
        .iter()
        .any(|(_, path)| path_matches(path, match_paths))
}

/// Ask matching processes to close (WM_CLOSE), then terminate leftovers.
pub fn close_matching(match_paths: &[PathBuf]) -> Result<(), String> {
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

fn path_matches(image: &Path, match_paths: &[PathBuf]) -> bool {
    // Avoid canonicalize() here — it is far too expensive when scanning many processes.
    let image_s = normalize_path_key(image);
    let image_name = image
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase());

    match_paths.iter().any(|m| {
        let m_s = normalize_path_key(m);
        if m.components().count() == 1 {
            return image_name.as_deref() == Some(m_s.as_str());
        }
        let looks_like_file = m
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"));
        if looks_like_file {
            return image_s == m_s;
        }
        // Directory / install root: process image lives under this path.
        image_s == m_s
            || image_s.starts_with(&format!("{m_s}\\"))
            || image_s.starts_with(&format!("{m_s}/"))
    })
}

fn normalize_path_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase().replace('/', "\\")
}

/// Index of the first path-set that matches a live process (one process snapshot).
#[allow(dead_code)]
pub fn first_matching_index(path_sets: &[Vec<PathBuf>]) -> Option<usize> {
    first_matching_index_in_images(path_sets, &list_process_images())
}

/// Same as [`first_matching_index`] against a pre-taken process snapshot.
pub fn first_matching_index_in_images(
    path_sets: &[Vec<PathBuf>],
    images: &[(u32, PathBuf)],
) -> Option<usize> {
    if path_sets.is_empty() || path_sets.iter().all(|p| p.is_empty()) {
        return None;
    }
    for (_pid, path) in images {
        for (i, paths) in path_sets.iter().enumerate() {
            if !paths.is_empty() && path_matches(path, paths) {
                return Some(i);
            }
        }
    }
    None
}

const SLOW_PROCESS_ENUM_MS: u128 = 20;

/// Full process image snapshot (Toolhelp + module path). Safe to call off the UI thread.
pub fn list_process_images() -> Vec<(u32, PathBuf)> {
    #[cfg(windows)]
    {
        process_images()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

#[cfg(windows)]
fn process_images() -> Vec<(u32, PathBuf)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::ProcessStatus::K32GetModuleFileNameExW;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    let started = Instant::now();
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
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
                    && let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                {
                    let mut buf = [0u16; 512];
                    let n = K32GetModuleFileNameExW(Some(handle), None, &mut buf);
                    let _ = CloseHandle(handle);
                    if n > 0 {
                        let path = PathBuf::from(String::from_utf16_lossy(&buf[..n as usize]));
                        out.push((pid, path));
                    }
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    let ms = started.elapsed().as_millis();
    if ms >= SLOW_PROCESS_ENUM_MS {
        crate::hid_diag::diag_info(format!(
            "ui-diag: process-enum ms={ms} images={}",
            out.len()
        ));
    }
    out
}

/// True when any running process basename equals `name` (case-insensitive). Names only — no full path.
#[cfg(windows)]
#[allow(dead_code)] // hid_diag steam scan (debug builds)
pub fn any_process_name_eq_ignore_case(name: &str) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let needle = name.to_lowercase();
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return false;
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut found = false;
    unsafe {
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let exe = String::from_utf16_lossy(
                    &entry.szExeFile[..entry
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(entry.szExeFile.len())],
                )
                .to_lowercase();
                if exe == needle {
                    found = true;
                    break;
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    found
}

#[cfg(not(windows))]
pub fn any_process_name_eq_ignore_case(_name: &str) -> bool {
    false
}

/// Basename of the foreground window's process image, if available.
#[cfg(windows)]
#[allow(dead_code)] // hid_diag failure_context (debug builds)
pub fn foreground_process_name() -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::ProcessStatus::K32GetModuleFileNameExW;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return None;
        };
        let mut buf = [0u16; 512];
        let n = K32GetModuleFileNameExW(Some(handle), None, &mut buf);
        let _ = CloseHandle(handle);
        if n == 0 {
            return None;
        }
        let path = PathBuf::from(String::from_utf16_lossy(&buf[..n as usize]));
        path.file_name().map(|n| n.to_string_lossy().into_owned())
    }
}

#[cfg(not(windows))]
pub fn foreground_process_name() -> Option<String> {
    None
}

#[cfg(windows)]
fn matching_pids(match_paths: &[PathBuf]) -> Vec<u32> {
    if match_paths.is_empty() {
        return Vec::new();
    }
    list_process_images()
        .into_iter()
        .filter_map(|(pid, path)| path_matches(&path, match_paths).then_some(pid))
        .collect()
}

#[cfg(windows)]
fn close_matching_windows(match_paths: &[PathBuf]) -> Result<(), String> {
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
    use super::*;

    #[test]
    fn steam_appid_parses() {
        assert_eq!(steam_appid("steam://rungameid/570"), Some(570));
        assert_eq!(steam_appid("C:\\games\\foo.exe"), None);
    }

    #[test]
    fn match_paths_manual_file() {
        // Non-existent path still yields basename-style entry via PathBuf.
        let paths = match_paths_for_target("C:\\Games\\Demo\\game.exe");
        assert!(!paths.is_empty());
    }
}
