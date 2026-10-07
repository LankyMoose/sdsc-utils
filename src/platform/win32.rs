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
pub const MONITOR_DEFAULTTONULL: u32 = 0;
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
///
/// # Safety
/// `hwnd` must be a valid Win32 window handle owned by this process.
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
///
/// # Safety
/// `hwnd` must be a valid Win32 window handle owned by this process.
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
///
/// # Safety
/// `hwnd` must be a valid Win32 window handle owned by this process.
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
    pub fn GetCursorPos(point: *mut Point) -> i32;
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
    pub fn EnumDisplayMonitors(
        hdc: isize,
        clip: *const Rect,
        callback: unsafe extern "system" fn(isize, isize, *mut Rect, isize) -> i32,
        data: isize,
    ) -> i32;
}

pub const SWP_NOZORDER: u32 = 0x0004;
pub const MONITORINFOF_PRIMARY: u32 = 0x0001;

/// Full bounds of every monitor, primary flagged (physical pixels).
pub fn monitor_bounds() -> Vec<(Rect, bool)> {
    unsafe extern "system" fn collect(monitor: isize, _: isize, _: *mut Rect, data: isize) -> i32 {
        // SAFETY: `data` is the `&mut Vec` passed below, alive for the call.
        let out = unsafe { &mut *(data as *mut Vec<(Rect, bool)>) };
        let mut info = MonitorInfo {
            size: std::mem::size_of::<MonitorInfo>() as u32,
            ..MonitorInfo::default()
        };
        if unsafe { GetMonitorInfoW(monitor, &mut info) } != 0 {
            out.push((info.monitor, info.flags & MONITORINFOF_PRIMARY != 0));
        }
        1
    }
    let mut out: Vec<(Rect, bool)> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            0,
            std::ptr::null(),
            collect,
            &mut out as *mut Vec<(Rect, bool)> as isize,
        );
    }
    out
}

/// Target top-left for moving a window at `win` on `primary` to the same
/// relative spot on `secondary`, clamped so it stays fully on `secondary`
/// where it fits. `None` when the window is not on `primary`.
pub fn relocate_to_secondary(win: Rect, primary: Rect, secondary: Rect) -> Option<(i32, i32)> {
    let cx = (win.left + win.right) / 2;
    let cy = (win.top + win.bottom) / 2;
    let on_primary =
        cx >= primary.left && cx < primary.right && cy >= primary.top && cy < primary.bottom;
    if !on_primary {
        return None;
    }
    let w = win.right - win.left;
    let h = win.bottom - win.top;
    let clamp = |v: i32, lo: i32, hi: i32| if hi < lo { lo } else { v.clamp(lo, hi) };
    let x = clamp(
        secondary.left + (win.left - primary.left),
        secondary.left,
        secondary.right - w,
    );
    let y = clamp(
        secondary.top + (win.top - primary.top),
        secondary.top,
        secondary.bottom - h,
    );
    Some((x, y))
}

/// Move `hwnd` from the primary to the first secondary monitor (no activation,
/// no z-order change). No-op on single-monitor setups or when already off primary.
///
/// # Safety
/// `hwnd` must be a valid Win32 window handle owned by this process.
pub unsafe fn move_to_secondary_monitor(hwnd: isize) {
    let monitors = monitor_bounds();
    let Some(primary) = monitors.iter().find(|(_, p)| *p).map(|(r, _)| *r) else {
        return;
    };
    let Some(secondary) = monitors.iter().find(|(_, p)| !*p).map(|(r, _)| *r) else {
        return;
    };
    let mut win = Rect::default();
    if unsafe { GetWindowRect(hwnd, &mut win) } == 0 {
        return;
    }
    if let Some((x, y)) = relocate_to_secondary(win, primary, secondary) {
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                0,
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

#[cfg(test)]
mod relocate_tests {
    use super::*;

    fn r(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn keeps_relative_position_on_secondary() {
        let primary = r(0, 0, 1920, 1080);
        let secondary = r(1920, 0, 3840, 1080);
        let win = r(100, 200, 500, 600);
        assert_eq!(
            relocate_to_secondary(win, primary, secondary),
            Some((2020, 200))
        );
    }

    #[test]
    fn clamps_onto_smaller_secondary_and_ignores_off_primary() {
        let primary = r(0, 0, 2560, 1440);
        let secondary = r(-1920, 0, 0, 1080);
        // Bottom-right of a big primary lands clamped inside the smaller one.
        let win = r(2160, 1240, 2560, 1440);
        assert_eq!(
            relocate_to_secondary(win, primary, secondary),
            Some((-400, 880))
        );
        // Already on the secondary: leave it alone.
        assert_eq!(
            relocate_to_secondary(r(-1000, 100, -600, 500), primary, secondary),
            None
        );
    }
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
