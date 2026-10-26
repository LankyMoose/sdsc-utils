//! Host platform abstraction (additive, no behavior change).
//!
//! Safe wrappers that delegate verbatim to the existing platform
//! implementations. App ([`crate::app`]) and UI ([`crate::ui::layout`] /
//! [`crate::ui::start`]) call sites go through this seam instead of touching
//! Win32 directly.
//!
//! Window functions take a raw `isize` HWND, matching [`crate::platform::win32`].
//! An `HWND` is an opaque handle-table value, not a pointer, so a stale value
//! fails the underlying call instead of causing memory unsafety. Callers pass
//! HWNDs obtained from iced/winit `RawWindowHandle` inside a `window::run`
//! closure, where the window is live and owned by this process.
//!
//! Single implementation, plain inherent `pub fn` on [`Host`]: there is one
//! backend and zero mocks, so no traits are needed. Methods delegate to the
//! existing cross-platform functions, whose non-Windows stubs (`false` / `Err`
//! / `None`) are preserved verbatim.

use crate::AppResult;

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

/// Move `hwnd` from the primary to the first secondary monitor (debug quiet
/// mode: windows never activate on creation; see
/// [`crate::platform::debug_quiet`]).
///
/// No-op on single-monitor setups or when already off primary.
pub fn move_to_secondary_monitor(hwnd: isize) {
    #[cfg(windows)]
    {
        // SAFETY: same contract as [`show_window`]: opaque HWND from a
        // `window::run` closure on the UI thread; delegates verbatim to the
        // existing `win32::move_to_secondary_monitor` block.
        unsafe {
            crate::platform::win32::move_to_secondary_monitor(hwnd);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
    }
}

/// Force Win32 to repaint `hwnd` (helps iced pick up a new toast view).
pub fn invalidate_hwnd(hwnd: isize) {
    #[cfg(windows)]
    {
        // SAFETY: same contract as [`show_window`]: opaque HWND from a
        // `window::run` closure on the UI thread; `RedrawWindow` only marks
        // the frame dirty, it never dereferences caller memory.
        unsafe {
            crate::platform::win32::invalidate_hwnd(hwnd);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
    }
}

/// Default host: delegates verbatim to the existing platform implementations.
///
/// Non-Windows behavior is inherited from the existing per-platform stubs
/// (`false` / `Err` / `None`), unchanged.
pub struct Host;

/// Shell icon extraction for local files.
///
/// Single implementation; plain associated functions on [`Host`] (no trait:
/// there is only one backend and no mock callers).
///
/// Reads through the Start image layer's owned
/// [`crate::platform::file_icon::FileIconCache`]
/// (see [`crate::ui::start::icon_cache::IconCache`]); no process-global.
impl Host {
    /// Square RGBA icon for `path`, or `None` when extraction fails.
    pub fn icon_get(
        cache: &crate::platform::file_icon::FileIconCache,
        path: &std::path::Path,
        size: u32,
    ) -> Option<(u32, u32, Vec<u8>)> {
        cache.rgba_for_path(path, size)
    }

    /// Drop every cached shell extract (process Exit).
    pub fn icon_clear(cache: &crate::platform::file_icon::FileIconCache) {
        cache.clear_all();
    }
}

/// App data-directory locations.
///
/// Single implementation delegating verbatim to [`crate::persist::paths`];
/// plain associated functions on [`Host`] (no trait: one backend, no mocks).
impl Host {
    /// Directory for prefs, controllers, analytics (and the log file).
    pub fn data_dir() -> std::path::PathBuf {
        crate::persist::paths::data_dir()
    }

    /// Create the data directory and open it in the OS file manager.
    pub fn open_data_folder() -> AppResult<()> {
        crate::persist::paths::open_data_folder()
    }
}

/// OS device arrival/removal watcher (trigger, not truth) plus the
/// launch-at-login entry: single implementation, plain associated functions
/// on [`Host`] (no traits: one backend, no mocks).
impl Host {
    /// Spawn the watcher; `true` when one is running.
    pub fn spawn<F>(on_event: F) -> bool
    where
        F: Fn() + Send + Sync + 'static,
    {
        crate::platform::device_watch::spawn_arrival_watcher(on_event)
    }

    /// Create the autostart entry.
    pub fn install() -> AppResult<()> {
        crate::platform::autostart::install()
    }

    /// Remove the autostart entry (including legacy entries).
    pub fn uninstall() -> AppResult<()> {
        crate::platform::autostart::uninstall()
    }

    /// Whether the autostart entry is currently enabled.
    pub fn is_enabled() -> bool {
        crate::platform::autostart::is_enabled()
    }

    /// Enable or disable the autostart entry.
    pub fn set_enabled(enabled: bool) -> AppResult<()> {
        crate::platform::autostart::set_enabled(enabled)
    }

    /// Replace a leftover Startup `.cmd` with a silent `.lnk` (fixes console
    /// flash on login). Safe to call on every launch; no-op when packaged
    /// or off Windows.
    pub fn ensure_quiet_entry() {
        crate::platform::autostart::ensure_quiet_entry();
    }
}

/// System clock / locale formatting plus physical-pixel monitor queries:
/// single implementation, plain associated functions on [`Host`] (no traits:
/// one backend, no mocks).
///
/// Physical-pixel monitor rect as `(left, top, right, bottom)`.
pub type PxRect = (i32, i32, i32, i32);

impl Host {
    /// Seconds to add to UTC to get local time (`local = utc + offset`).
    pub fn local_utc_offset_secs() -> i32 {
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

    /// System locale short time (e.g. `7:06 pm` or `19:06`), no seconds.
    ///
    /// `None` when the locale API is unavailable or fails; callers fall back
    /// to 12h am/pm formatting.
    pub fn locale_time() -> Option<String> {
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

    /// Whether the system cursor is currently on the primary monitor.
    pub fn cursor_on_primary_monitor() -> bool {
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

    /// Whether the cursor is inside the primary monitor's work area
    /// (excludes the taskbar strip and other reserved chrome).
    ///
    /// The Start cover spans the full monitor including the taskbar, but
    /// taskbar input never reaches Start — hiding there strands the cursor
    /// with no self-unhide path (notably across a blocking tray-menu modal
    /// loop). `false` off Windows or when the query fails (fail visible).
    pub fn cursor_in_work_area() -> bool {
        #[cfg(windows)]
        {
            // SAFETY: `GetCursorPos` writes only to the caller-owned `point`.
            unsafe {
                let mut point = crate::platform::win32::Point::default();
                if crate::platform::win32::GetCursorPos(&mut point) == 0 {
                    return false;
                }
                let Some(((ml, mt, mr, mb), (wl, wt, wr, wb), _, _)) = Self::primary_metrics()
                else {
                    return false;
                };
                let on_primary = point.x >= ml && point.x < mr && point.y >= mt && point.y < mb;
                on_primary && point.x >= wl && point.x < wr && point.y >= wt && point.y < wb
            }
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// Primary monitor + work-area geometry for UI cover math.
    ///
    /// Returns `(monitor_rect, work_rect, dpi_scale, foreground_covers_monitor)`
    /// in physical pixels; the caller converts to logical [`crate::ui::layout`]
    /// cover rects so no UI type crosses this seam. `None` off Windows or when
    /// the monitor query fails (callers fall back to the iced-provided size).
    pub fn primary_metrics() -> Option<(PxRect, PxRect, f32, bool)> {
        #[cfg(windows)]
        {
            // SAFETY: `MonitorFromPoint` takes coordinates by value and returns
            // an opaque monitor handle compared by equality only; `GetMonitorInfoW`
            // and `GetDpiForMonitor` write only to the caller-owned `info` /
            // `dpi_*` out-params; `GetForegroundWindow` / `GetWindowRect` only
            // read the foreground geometry, never caller memory beyond `rect`.
            unsafe {
                let monitor = crate::platform::win32::MonitorFromPoint(
                    crate::platform::win32::Point { x: 0, y: 0 },
                    crate::platform::win32::MONITOR_DEFAULTTOPRIMARY,
                );
                if monitor == 0 {
                    return None;
                }

                let mut info = crate::platform::win32::MonitorInfo {
                    size: std::mem::size_of::<crate::platform::win32::MonitorInfo>() as u32,
                    monitor: crate::platform::win32::Rect::default(),
                    work: crate::platform::win32::Rect::default(),
                    flags: 0,
                };
                if crate::platform::win32::GetMonitorInfoW(monitor, &mut info) == 0 {
                    return None;
                }

                let mut dpi_x = 0u32;
                let mut dpi_y = 0u32;
                let dpi_ok = crate::platform::win32::GetDpiForMonitor(
                    monitor,
                    crate::platform::win32::MDT_EFFECTIVE_DPI,
                    &mut dpi_x,
                    &mut dpi_y,
                ) == 0
                    && dpi_x > 0;
                let scale = if dpi_ok { dpi_x as f32 / 96.0 } else { 1.0 };

                let to_tuple = |rect: crate::platform::win32::Rect| {
                    (rect.left, rect.top, rect.right, rect.bottom)
                };

                let foreground = crate::platform::win32::GetForegroundWindow();
                let mut foreground_rect = crate::platform::win32::Rect::default();
                let got_foreground = foreground != 0
                    && crate::platform::win32::GetWindowRect(foreground, &mut foreground_rect) != 0;
                let foreground_covers_monitor = got_foreground
                    && foreground_rect.left <= info.monitor.left + 2
                    && foreground_rect.top <= info.monitor.top + 2
                    && foreground_rect.right >= info.monitor.right - 2
                    && foreground_rect.bottom >= info.monitor.bottom - 2;

                Some((
                    to_tuple(info.monitor),
                    to_tuple(info.work),
                    scale,
                    foreground_covers_monitor,
                ))
            }
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// Foreground-window geometry for the exclusive-fullscreen predicate.
    ///
    /// Returns `(width, height, monitor_width, monitor_height, style)` in
    /// physical pixels plus Win32 `GWL_STYLE` bits; the caller maps this onto
    /// [`crate::domain::pad::ForegroundWindowFacts`] and applies the pure
    /// [`crate::domain::pad::is_exclusive_fullscreen`] predicate, so no Win32
    /// type crosses this seam. `None` off Windows or when the foreground
    /// query fails (callers treat as not-fullscreen).
    pub fn foreground_facts() -> Option<(i32, i32, i32, i32, isize)> {
        #[cfg(windows)]
        {
            // SAFETY: `GetForegroundWindow` returns an opaque handle compared
            // against 0 only; `GetWindowRect` / `MonitorFromPoint` /
            // `GetMonitorInfoW` / `GetWindowLongW` read foreground geometry
            // and write only to the caller-owned `rect` / `info` out-params.
            // Verbatim the former `domain::pad` block, now behind this seam.
            unsafe {
                let hwnd = crate::platform::win32::GetForegroundWindow();
                if hwnd == 0 {
                    return None;
                }
                let mut rect = crate::platform::win32::Rect::default();
                if crate::platform::win32::GetWindowRect(hwnd, &mut rect) == 0 {
                    return None;
                }
                let width = rect.right - rect.left;
                let height = rect.bottom - rect.top;
                let point = crate::platform::win32::Point {
                    x: rect.left,
                    y: rect.top,
                };
                let monitor = crate::platform::win32::MonitorFromPoint(
                    point,
                    crate::platform::win32::MONITOR_DEFAULTTOPRIMARY,
                );
                if monitor == 0 {
                    return None;
                }
                let mut info = crate::platform::win32::MonitorInfo {
                    size: std::mem::size_of::<crate::platform::win32::MonitorInfo>() as u32,
                    ..Default::default()
                };
                if crate::platform::win32::GetMonitorInfoW(monitor, &mut info) == 0 {
                    return None;
                }
                let mon_w = info.monitor.right - info.monitor.left;
                let mon_h = info.monitor.bottom - info.monitor.top;
                let style =
                    crate::platform::win32::GetWindowLongW(hwnd, crate::platform::win32::GWL_STYLE);
                Some((width, height, mon_w, mon_h, style))
            }
        }
        #[cfg(not(windows))]
        {
            None
        }
    }
}

/// Posts `WM_QUIT` to a hotkey pump when the awaiting future is dropped.
///
/// Mirrors [`crate::platform::win32::ThreadQuitGuard`] through the host seam
/// so [`crate::app`] never names the Win32 type directly.
pub struct QuitGuard(u32);

impl Drop for QuitGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            if self.0 != 0 {
                // SAFETY: posts to a thread id, never touches memory; `0`
                // (no worker yet) is ignored, matching `ThreadQuitGuard`.
                unsafe {
                    crate::platform::win32::PostThreadMessageW(
                        self.0,
                        crate::platform::win32::WM_QUIT,
                        0,
                        0,
                    );
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = self.0;
        }
    }
}

/// Global hotkeys: the toast Escape dismiss plus the debug-only hitch markers
/// (F7 lightbar / F8 input, compiled out of release builds).
///
/// `RegisterHotKey` binds to the calling thread's message queue, so the whole
/// lifecycle (register → pump → unregister) lives on one worker thread owned by
/// the caller in [`crate::app`]; these methods only wrap the Win32 verbs.
/// Non-Windows stubs (`false` / `None` / `0` / no-op) are preserved verbatim —
/// off Windows the callers keep their `pending()` / no-watcher fallbacks.
impl Host {
    /// Toast Escape-dismiss hotkey id.
    #[cfg(windows)]
    pub const ESCAPE_ID: i32 = crate::platform::win32::HOTKEY_ID;
    /// Toast Escape-dismiss hotkey id (`0` stub off Windows).
    #[cfg(not(windows))]
    pub const ESCAPE_ID: i32 = 0;
    /// Debug hitch-marker id (F7 lightbar).
    #[cfg(all(debug_assertions, windows))]
    pub const HITCH_LIGHTBAR_ID: i32 = crate::platform::win32::HOTKEY_ID_LIGHTBAR_HITCH;
    /// Debug hitch-marker id (F7 lightbar; `1` stub off Windows).
    #[cfg(all(debug_assertions, not(windows)))]
    pub const HITCH_LIGHTBAR_ID: i32 = 1;
    /// Debug hitch-marker id (F8 input).
    #[cfg(all(debug_assertions, windows))]
    pub const HITCH_INPUT_ID: i32 = crate::platform::win32::HOTKEY_ID_INPUT_HITCH;
    /// Debug hitch-marker id (F8 input; `2` stub off Windows).
    #[cfg(all(debug_assertions, not(windows)))]
    pub const HITCH_INPUT_ID: i32 = 2;

    /// Register the toast Escape hotkey on the calling thread; `true` on success.
    #[cfg(windows)]
    pub fn register_escape() -> bool {
        // SAFETY: `hwnd` 0 binds the hotkey to the calling thread's queue;
        // plain integer params, no memory access.
        unsafe {
            crate::platform::win32::RegisterHotKey(
                0,
                Self::ESCAPE_ID,
                crate::platform::win32::MOD_NOREPEAT,
                crate::platform::win32::VK_ESCAPE,
            ) != 0
        }
    }

    /// Register the toast Escape hotkey on the calling thread (`false` stub off Windows).
    #[cfg(not(windows))]
    pub fn register_escape() -> bool {
        false
    }

    /// Remove the toast Escape hotkey.
    #[cfg(windows)]
    pub fn unregister_escape() {
        // SAFETY: integer id only; failure is a return code.
        unsafe {
            crate::platform::win32::UnregisterHotKey(0, Self::ESCAPE_ID);
        }
    }

    /// Remove the toast Escape hotkey (no-op stub off Windows).
    #[cfg(not(windows))]
    pub fn unregister_escape() {}

    /// Register the debug lightbar-hitch hotkey (F7) on the calling thread.
    #[cfg(all(debug_assertions, windows))]
    pub fn register_hitch_lightbar() -> bool {
        // SAFETY: same contract as [`Host::register_escape`].
        unsafe {
            crate::platform::win32::RegisterHotKey(
                0,
                Self::HITCH_LIGHTBAR_ID,
                crate::platform::win32::MOD_NOREPEAT,
                crate::platform::win32::VK_F7,
            ) != 0
        }
    }

    /// Register the debug lightbar-hitch hotkey (F7) on the calling thread
    /// (`false` stub off Windows).
    #[cfg(all(debug_assertions, not(windows)))]
    pub fn register_hitch_lightbar() -> bool {
        false
    }

    /// Register the debug input-hitch hotkey (F8) on the calling thread.
    #[cfg(all(debug_assertions, windows))]
    pub fn register_hitch_input() -> bool {
        // SAFETY: same contract as [`Host::register_escape`].
        unsafe {
            crate::platform::win32::RegisterHotKey(
                0,
                Self::HITCH_INPUT_ID,
                crate::platform::win32::MOD_NOREPEAT,
                crate::platform::win32::VK_F8,
            ) != 0
        }
    }

    /// Register the debug input-hitch hotkey (F8) on the calling thread
    /// (`false` stub off Windows).
    #[cfg(all(debug_assertions, not(windows)))]
    pub fn register_hitch_input() -> bool {
        false
    }

    /// Remove the debug lightbar-hitch hotkey.
    #[cfg(all(debug_assertions, windows))]
    pub fn unregister_hitch_lightbar() {
        // SAFETY: integer id only; failure is a return code.
        unsafe {
            crate::platform::win32::UnregisterHotKey(0, Self::HITCH_LIGHTBAR_ID);
        }
    }

    /// Remove the debug lightbar-hitch hotkey (no-op stub off Windows).
    #[cfg(all(debug_assertions, not(windows)))]
    pub fn unregister_hitch_lightbar() {}

    /// Remove the debug input-hitch hotkey.
    #[cfg(all(debug_assertions, windows))]
    pub fn unregister_hitch_input() {
        // SAFETY: integer id only; failure is a return code.
        unsafe {
            crate::platform::win32::UnregisterHotKey(0, Self::HITCH_INPUT_ID);
        }
    }

    /// Remove the debug input-hitch hotkey (no-op stub off Windows).
    #[cfg(all(debug_assertions, not(windows)))]
    pub fn unregister_hitch_input() {}

    /// Block for the next `WM_HOTKEY` on the calling thread and return its id.
    ///
    /// Non-hotkey messages are skipped internally (the verbatim `continue`);
    /// `None` when the pump quits (`WM_QUIT`) or errors.
    #[cfg(windows)]
    pub fn next_hotkey() -> Option<i32> {
        // SAFETY: `message` is caller-owned; `GetMessageW` with null filters
        // pumps this thread's queue and writes only to `message`. Returns 0
        // on `WM_QUIT`, negative on error — both end the pump like the
        // verbatim `> 0` loop they replace.
        unsafe {
            let mut message = crate::platform::win32::Message::default();
            while crate::platform::win32::GetMessageW(&mut message, 0, 0, 0) > 0 {
                if message.message == crate::platform::win32::WM_HOTKEY {
                    return Some(message.w_param as i32);
                }
            }
            None
        }
    }

    /// Block for the next `WM_HOTKEY` on the calling thread (`None` stub off
    /// Windows: callers keep their `pending()` / no-watcher fallbacks).
    #[cfg(not(windows))]
    pub fn next_hotkey() -> Option<i32> {
        None
    }

    /// OS id of the calling thread (quit-guard target; `0` stub off Windows).
    #[cfg(windows)]
    pub fn current_thread_id() -> u32 {
        // SAFETY: returns the calling thread's OS id; no memory access.
        unsafe { crate::platform::win32::GetCurrentThreadId() }
    }

    /// OS id of the calling thread (`0` stub off Windows).
    #[cfg(not(windows))]
    pub fn current_thread_id() -> u32 {
        0
    }

    /// RAII poster: unblocks a [`Host::next_hotkey`] pump on `thread_id`
    /// when dropped (toast dismissed by some other means).
    pub fn quit_guard(thread_id: u32) -> QuitGuard {
        QuitGuard(thread_id)
    }
}
