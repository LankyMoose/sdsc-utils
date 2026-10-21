//! Host platform abstraction (additive, no behavior change).
//!
//! Safe wrappers and traits that delegate verbatim to the existing platform
//! implementations. UI call sites in [`crate::ui::layout`] / [`crate::ui::start`]
//! go through this seam instead of touching Win32 directly.
//!
//! Window functions take a raw `isize` HWND, matching [`crate::platform::win32`].
//! An `HWND` is an opaque handle-table value, not a pointer, so a stale value
//! fails the underlying call instead of causing memory unsafety. Callers pass
//! HWNDs obtained from iced/winit `RawWindowHandle` inside a `window::run`
//! closure, where the window is live and owned by this process.
//!
//! Trait impls delegate to the existing cross-platform functions, whose
//! non-Windows stubs (`false` / `Err` / `None`) are preserved verbatim.

/// Show a window without activating it (so a game keeps focus).
pub fn show_window(hwnd: isize) {
    #[cfg(windows)]
    {
        // SAFETY: `hwnd` is an opaque handle-table value from iced/winit
        // `RawWindowHandle`, live inside a `window::run` closure on the UI
        // thread; invalid values fail `ShowWindow` instead of aliasing memory.
        unsafe {
            crate::platform::win32::ShowWindow(hwnd, crate::platform::win32::SW_SHOWNOACTIVATE);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
    }
}

/// Hide a window previously shown with [`show_window`].
pub fn hide(hwnd: isize) {
    #[cfg(windows)]
    {
        // SAFETY: same contract as [`show_window`]: opaque HWND from a
        // `window::run` closure on the UI thread; failure is a return code.
        unsafe {
            crate::platform::win32::ShowWindow(hwnd, crate::platform::win32::SW_HIDE);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
    }
}

/// No-activate "focus": raise the window in z-order without stealing input.
///
/// There is no `SetForegroundWindow` here by design; in the no-activate model
/// raising to the top of the topmost band is the focus equivalent.
pub fn focus(hwnd: isize) {
    #[cfg(windows)]
    {
        // SAFETY: same contract as [`show_window`]: opaque HWND from a
        // `window::run` closure on the UI thread; `raise_topmost` only
        // reorders the window, it never dereferences caller memory.
        unsafe {
            crate::platform::win32::raise_topmost(hwnd);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
    }
}

/// Pin (`topmost = true`) or unpin (`false`) a window in the topmost band
/// without activating it and without moving or resizing it.
pub fn set_topmost(hwnd: isize, topmost: bool) {
    #[cfg(windows)]
    {
        // SAFETY: same contract as [`show_window`]: opaque HWND from a
        // `window::run` closure on the UI thread; flags mirror
        // `win32::raise_topmost` (`SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE`).
        unsafe {
            let after = if topmost {
                crate::platform::win32::HWND_TOPMOST
            } else {
                crate::platform::win32::HWND_NOTOPMOST
            };
            crate::platform::win32::SetWindowPos(
                hwnd,
                after,
                0,
                0,
                0,
                0,
                crate::platform::win32::SWP_NOMOVE
                    | crate::platform::win32::SWP_NOSIZE
                    | crate::platform::win32::SWP_NOACTIVATE,
            );
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (hwnd, topmost);
    }
}

/// Mark a window non-activating, optionally pinning it topmost.
pub fn set_no_activate(hwnd: isize, topmost: bool) {
    #[cfg(windows)]
    {
        // SAFETY: same contract as [`show_window`]: opaque HWND from a
        // `window::run` closure on the UI thread; delegates verbatim to the
        // existing `apply_noactivate_exstyle` block.
        unsafe {
            crate::platform::win32::apply_noactivate_exstyle(hwnd, topmost);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (hwnd, topmost);
    }
}

/// OS device arrival/removal watcher (trigger, not truth).
pub trait Watcher {
    /// Spawn the watcher; `true` when one is running.
    fn spawn<F>(on_event: F) -> bool
    where
        F: Fn() + Send + Sync + 'static;
}

/// Launch-at-login entry.
pub trait Autostart {
    /// Create the autostart entry.
    fn install() -> Result<(), String>;
    /// Remove the autostart entry (including legacy entries).
    fn uninstall() -> Result<(), String>;
    /// Whether the autostart entry is currently enabled.
    fn is_enabled() -> bool;
}

/// Shell icon extraction for local files.
pub trait Icons {
    /// Square RGBA icon for `path`, or `None` when extraction fails.
    fn get(path: &std::path::Path, size: u32) -> Option<(u32, u32, Vec<u8>)>;
}

/// App data-directory locations.
pub trait Paths {
    /// Directory for prefs, controllers, analytics (and the log file).
    fn data_dir() -> std::path::PathBuf;
    /// Create the data directory and open it in the OS file manager.
    fn open_data_folder() -> Result<(), String>;
}

/// Default host: delegates verbatim to the existing platform implementations.
///
/// Non-Windows behavior is inherited from the existing per-platform stubs
/// (`false` / `Err` / `None`), unchanged.
pub struct Host;

impl Watcher for Host {
    fn spawn<F>(on_event: F) -> bool
    where
        F: Fn() + Send + Sync + 'static,
    {
        crate::platform::device_watch::spawn_arrival_watcher(on_event)
    }
}

impl Autostart for Host {
    fn install() -> Result<(), String> {
        crate::platform::autostart::install()
    }

    fn uninstall() -> Result<(), String> {
        crate::platform::autostart::uninstall()
    }

    fn is_enabled() -> bool {
        crate::platform::autostart::is_enabled()
    }
}

impl Icons for Host {
    fn get(path: &std::path::Path, size: u32) -> Option<(u32, u32, Vec<u8>)> {
        crate::platform::file_icon::rgba_for_path(path, size)
    }
}

impl Paths for Host {
    fn data_dir() -> std::path::PathBuf {
        crate::persist::paths::data_dir()
    }

    fn open_data_folder() -> Result<(), String> {
        crate::persist::paths::open_data_folder()
    }
}

/// System clock / locale formatting.
pub trait Clock {
    /// Seconds to add to UTC to get local time (`local = utc + offset`).
    fn local_utc_offset_secs() -> i32;
    /// System locale short time (e.g. `7:06 pm` or `19:06`), no seconds.
    ///
    /// `None` when the locale API is unavailable or fails; callers fall back
    /// to 12h am/pm formatting.
    fn locale_time() -> Option<String>;
}

/// Monitor / cursor placement queries.
pub trait Monitors {
    /// Whether the system cursor is currently on the primary monitor.
    fn cursor_on_primary_monitor() -> bool;
}

impl Clock for Host {
    fn local_utc_offset_secs() -> i32 {
        #[cfg(windows)]
        {
            use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
            const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
            const TIME_ZONE_ID_INVALID: u32 = 0xFFFF_FFFF;
            // SAFETY: `GetTimeZoneInformation` writes only to the caller-owned
            // `info` out-param; an invalid zone returns
            // `TIME_ZONE_ID_INVALID` instead of touching memory.
            unsafe {
                let mut info = TIME_ZONE_INFORMATION::default();
                let id = GetTimeZoneInformation(&mut info);
                if id == TIME_ZONE_ID_INVALID {
                    return 0;
                }
                // Windows Bias: UTC = local + bias(minutes). DaylightBias is negative.
                let mut bias = info.Bias;
                if id == TIME_ZONE_ID_DAYLIGHT {
                    bias += info.DaylightBias;
                } else {
                    bias += info.StandardBias;
                }
                bias.saturating_mul(-60)
            }
        }
        #[cfg(not(windows))]
        {
            0
        }
    }

    fn locale_time() -> Option<String> {
        #[cfg(windows)]
        {
            use windows::Win32::Globalization::{
                GetTimeFormatW, LOCALE_USER_DEFAULT, TIME_NOSECONDS,
            };
            use windows::core::PCWSTR;

            // Passing no time uses the current local time; a null format uses the
            // user's short-time locale format. First call queries the required length
            // (including the null terminator).
            // SAFETY: both calls pass either null or a caller-owned buffer with
            // its exact length; the API only reads the user locale, never caller
            // memory beyond `buf`.
            unsafe {
                let needed = GetTimeFormatW(
                    LOCALE_USER_DEFAULT,
                    TIME_NOSECONDS.0,
                    None,
                    PCWSTR::null(),
                    None,
                );
                if needed <= 0 {
                    return None;
                }
                let mut buf = vec![0u16; needed as usize];
                let written = GetTimeFormatW(
                    LOCALE_USER_DEFAULT,
                    TIME_NOSECONDS.0,
                    None,
                    PCWSTR::null(),
                    Some(&mut buf[..]),
                );
                if written <= 0 {
                    return None;
                }
                // `written` includes the null terminator; strip it before decoding.
                let len = (written as usize).saturating_sub(1);
                String::from_utf16(&buf[..len])
                    .ok()
                    .filter(|s| !s.is_empty())
            }
        }
        #[cfg(not(windows))]
        {
            None
        }
    }
}

impl Monitors for Host {
    fn cursor_on_primary_monitor() -> bool {
        #[cfg(windows)]
        {
            // SAFETY: `GetCursorPos` writes only to the caller-owned `point`;
            // `MonitorFromPoint` takes coordinates by value and returns an
            // opaque monitor handle compared by equality only.
            unsafe {
                let mut point = crate::platform::win32::Point::default();
                if crate::platform::win32::GetCursorPos(&mut point) == 0 {
                    return false;
                }
                let primary = crate::platform::win32::MonitorFromPoint(
                    crate::platform::win32::Point { x: 0, y: 0 },
                    crate::platform::win32::MONITOR_DEFAULTTOPRIMARY,
                );
                if primary == 0 {
                    return false;
                }
                let under_cursor = crate::platform::win32::MonitorFromPoint(
                    point,
                    crate::platform::win32::MONITOR_DEFAULTTONULL,
                );
                under_cursor != 0 && under_cursor == primary
            }
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}
