//! Minimal Win32 FFI used by the iced daemon (hotkeys, DPI, no-activate toasts).

#![cfg(windows)]

pub const MOD_NOREPEAT: u32 = 0x4000;
pub const VK_ESCAPE: u32 = 0x1B;
#[cfg(debug_assertions)]
pub const VK_F7: u32 = 0x76;
#[cfg(debug_assertions)]
pub const VK_F8: u32 = 0x77;
pub const HOTKEY_ID: i32 = 0x4454;
#[cfg(debug_assertions)]
pub const HOTKEY_ID_LIGHTBAR_HITCH: i32 = 0x4455;
#[cfg(debug_assertions)]
pub const HOTKEY_ID_INPUT_HITCH: i32 = 0x4456;
pub const WM_HOTKEY: u32 = 0x0312;
pub const WM_QUIT: u32 = 0x0012;
pub const MONITOR_DEFAULTTOPRIMARY: u32 = 1;
pub const MDT_EFFECTIVE_DPI: u32 = 0;

pub const GWL_STYLE: i32 = -16;
pub const GWL_EXSTYLE: i32 = -20;
pub const WS_CAPTION: isize = 0x00C0_0000;
pub const WS_THICKFRAME: isize = 0x0004_0000;
pub const WS_EX_NOACTIVATE: isize = 0x0800_0000;
pub const SW_HIDE: i32 = 0;
pub const SW_SHOWNOACTIVATE: i32 = 4;
pub const HWND_TOPMOST: isize = -1;
pub const HWND_NOTOPMOST: isize = -2;
pub const SWP_NOSIZE: u32 = 0x0001;
pub const SWP_NOMOVE: u32 = 0x0002;
pub const SWP_NOACTIVATE: u32 = 0x0010;
pub const SWP_FRAMECHANGED: u32 = 0x0020;
pub const RDW_INVALIDATE: u32 = 0x0001;
pub const RDW_ERASE: u32 = 0x0004;
pub const RDW_FRAME: u32 = 0x0400;
pub const RDW_ALLCHILDREN: u32 = 0x0080;

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct MonitorInfo {
    pub size: u32,
    pub monitor: Rect,
    pub work: Rect,
    pub flags: u32,
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Message {
    pub hwnd: isize,
    pub message: u32,
    pub w_param: usize,
    pub l_param: isize,
    pub time: u32,
    pub pt: Point,
}

/// Posts `WM_QUIT` to the hotkey worker when the awaiting future is dropped.
pub struct ThreadQuitGuard(pub u32);

impl Drop for ThreadQuitGuard {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                PostThreadMessageW(self.0, WM_QUIT, 0, 0);
            }
        }
    }
}

/// Mark `hwnd` as non-activating and optionally pin it above other apps.
///
/// Toast stays topmost so it clears Cursor / games. When Settings / Start /
/// popup are open, raise those HWNDs into the same topmost band *after* the
/// toast (see [`raise_topmost`]) so they keep presents (iced#3108 / #3320).
pub unsafe fn apply_noactivate_exstyle(hwnd: isize, topmost: bool) {
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        let _ = SetWindowLongW(hwnd, GWL_EXSTYLE, style | WS_EX_NOACTIVATE);
        let after = if topmost {
            HWND_TOPMOST
        } else {
            HWND_NOTOPMOST
        };
        let _ = SetWindowPos(
            hwnd,
            after,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }
}

/// Pin `hwnd` to the top of the topmost band without activating it.
pub unsafe fn raise_topmost(hwnd: isize) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Force Win32 to repaint `hwnd` (helps iced pick up a new toast view).
pub unsafe fn invalidate_hwnd(hwnd: isize) {
    unsafe {
        let _ = RedrawWindow(
            hwnd,
            std::ptr::null(),
            0,
            RDW_INVALIDATE | RDW_ERASE | RDW_FRAME | RDW_ALLCHILDREN,
        );
    }
}

#[link(name = "user32")]
unsafe extern "system" {
    pub fn RegisterHotKey(hwnd: isize, id: i32, modifiers: u32, key: u32) -> i32;
    pub fn UnregisterHotKey(hwnd: isize, id: i32) -> i32;
    pub fn GetMessageW(message: *mut Message, hwnd: isize, min: u32, max: u32) -> i32;
    pub fn PostThreadMessageW(thread: u32, message: u32, w_param: usize, l_param: isize) -> i32;
    pub fn MonitorFromPoint(point: Point, flags: u32) -> isize;
    pub fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
    pub fn GetForegroundWindow() -> isize;
    pub fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
    pub fn GetWindowLongW(hwnd: isize, index: i32) -> isize;
    pub fn SetWindowLongW(hwnd: isize, index: i32, value: isize) -> isize;
    pub fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
    pub fn SetWindowPos(
        hwnd: isize,
        insert_after: isize,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> i32;
    pub fn RedrawWindow(hwnd: isize, rect: *const Rect, region: isize, flags: u32) -> i32;
}

#[link(name = "shcore")]
unsafe extern "system" {
    pub fn GetDpiForMonitor(monitor: isize, dpi_type: u32, dpi_x: *mut u32, dpi_y: *mut u32)
    -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn GetCurrentThreadId() -> u32;
}
