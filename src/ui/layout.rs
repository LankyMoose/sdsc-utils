//! Popup / toast placement and iced window platform settings.

use crate::persist::prefs::ToastPosition;
#[cfg(windows)]
use crate::platform::win32;
use crate::ui::toast::view as toast_view;

use iced::{Point, Size, Task, window};

/// Gap between the tray icon and the popup.
pub const POPUP_GAP: f32 = 8.0;
/// Minimum distance kept from any screen edge.
pub const SCREEN_MARGIN: f32 = 8.0;
/// Slide-in / slide-out duration for overlay toasts.
pub const TOAST_SLIDE_DURATION: std::time::Duration = std::time::Duration::from_millis(250);

/// Screen rectangle of the tray icon, in physical pixels.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrayAnchor {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct ToastArea {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct ToastPlacement {
    pub x: f32,
    pub target_y: f32,
    pub outside_y: f32,
}

/// Show the overlay toast without activating it (so a game keeps focus).
///
/// iced/`set_mode(Windowed)` maps to `ShowWindow(SW_SHOW)`, which steals the
/// foreground. On Windows we apply `WS_EX_NOACTIVATE`, pin `HWND_TOPMOST`, and
/// show with `SW_SHOWNOACTIVATE` instead.
///
/// Callers that have Settings / Start / popup open should then
/// [`raise_window_topmost`] those windows so they sit above the toast in the
/// topmost band and keep receiving presents (iced#3108 / #3320).
pub fn show_toast_without_activate<Message: Send + 'static>(id: window::Id) -> Task<Message> {
    #[cfg(windows)]
    {
        window::run(id, move |window| {
            use window::raw_window_handle::RawWindowHandle;

            let Ok(handle) = window.window_handle() else {
                return;
            };
            let RawWindowHandle::Win32(win32_handle) = handle.as_raw() else {
                return;
            };
            let hwnd = win32_handle.hwnd.get();
            unsafe {
                win32::apply_noactivate_exstyle(hwnd, true);
                win32::ShowWindow(hwnd, win32::SW_SHOWNOACTIVATE);
            }
        })
        .discard()
    }
    #[cfg(not(windows))]
    {
        window::set_mode(id, window::Mode::Windowed)
            .chain(window::set_level(id, window::Level::AlwaysOnTop))
    }
}

/// Re-assert toast topmost (e.g. after Settings / Start / popup open or close).
pub fn set_toast_topmost<Message: Send + 'static>(id: window::Id) -> Task<Message> {
    #[cfg(windows)]
    {
        window::run(id, move |window| {
            use window::raw_window_handle::RawWindowHandle;

            let Ok(handle) = window.window_handle() else {
                return;
            };
            let RawWindowHandle::Win32(win32_handle) = handle.as_raw() else {
                return;
            };
            let hwnd = win32_handle.hwnd.get();
            unsafe {
                win32::apply_noactivate_exstyle(hwnd, true);
            }
        })
        .discard()
    }
    #[cfg(not(windows))]
    {
        window::set_level(id, window::Level::AlwaysOnTop)
    }
}

/// Raise an interactive UI window to the top of the topmost band (above toast).
pub fn raise_window_topmost<Message: Send + 'static>(id: window::Id) -> Task<Message> {
    #[cfg(windows)]
    {
        window::run(id, move |window| {
            use window::raw_window_handle::RawWindowHandle;

            let Ok(handle) = window.window_handle() else {
                return;
            };
            let RawWindowHandle::Win32(win32_handle) = handle.as_raw() else {
                return;
            };
            let hwnd = win32_handle.hwnd.get();
            unsafe {
                win32::raise_topmost(hwnd);
            }
        })
        .discard()
    }
    #[cfg(not(windows))]
    {
        window::set_level(id, window::Level::AlwaysOnTop)
    }
}

/// Ask Win32 to invalidate the toast HWND so a new view can present.
pub fn invalidate_toast<Message: Send + 'static>(id: window::Id) -> Task<Message> {
    #[cfg(windows)]
    {
        window::run(id, move |window| {
            use window::raw_window_handle::RawWindowHandle;

            let Ok(handle) = window.window_handle() else {
                return;
            };
            let RawWindowHandle::Win32(win32_handle) = handle.as_raw() else {
                return;
            };
            let hwnd = win32_handle.hwnd.get();
            unsafe {
                win32::invalidate_hwnd(hwnd);
            }
        })
        .discard()
    }
    #[cfg(not(windows))]
    {
        let _ = id;
        Task::none()
    }
}

/// Nudge toast size by ±1px so wgpu rebuilds the surface for a new message.
pub fn remount_toast_surface<Message: Send + 'static>(
    id: window::Id,
    generation: u64,
) -> Task<Message> {
    let width = crate::ui::toast::view::WIDTH
        + if generation.is_multiple_of(2) {
            0.0
        } else {
            1.0
        };
    window::resize(id, Size::new(width, crate::ui::toast::view::HEIGHT))
}

/// Hide the overlay toast without going through iced `set_mode(Hidden)`.
///
/// On Windows the toast is shown via raw `ShowWindow`, so winit's `VISIBLE`
/// flag stays false; `set_mode(Hidden)` would then be a no-op and leave the
/// toast on screen.
pub fn hide_toast<Message: Send + 'static>(id: window::Id) -> Task<Message> {
    #[cfg(windows)]
    {
        window::run(id, |window| {
            use window::raw_window_handle::RawWindowHandle;

            let Ok(handle) = window.window_handle() else {
                return;
            };
            let RawWindowHandle::Win32(win32_handle) = handle.as_raw() else {
                return;
            };
            let hwnd = win32_handle.hwnd.get();
            unsafe {
                win32::ShowWindow(hwnd, win32::SW_HIDE);
            }
        })
        .discard()
    }
    #[cfg(not(windows))]
    {
        window::set_mode(id, window::Mode::Hidden)
    }
}

pub fn popup_position(anchor: TrayAnchor, scale: f32, monitor: Option<Size>, size: Size) -> Point {
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let anchor_x = anchor.x / scale;
    let anchor_y = anchor.y / scale;
    let anchor_w = anchor.width / scale;
    let anchor_h = anchor.height / scale;

    let mut x = anchor_x + anchor_w / 2.0 - size.width / 2.0;
    let mut y = anchor_y - size.height - POPUP_GAP;

    if let Some(monitor) = monitor {
        // A tray at the top of the screen leaves no room above it.
        if y < SCREEN_MARGIN {
            y = anchor_y + anchor_h + POPUP_GAP;
        }
        x = clamp_axis(x, size.width, monitor.width);
        y = clamp_axis(y, size.height, monitor.height);
    }

    Point::new(x.max(0.0), y.max(0.0))
}

pub fn toast_placement(position: ToastPosition, monitor: Option<Size>) -> ToastPlacement {
    toast_placement_in(position, resolve_toast_area(monitor))
}

fn resolve_toast_area(monitor: Option<Size>) -> ToastArea {
    #[cfg(windows)]
    if let Some(area) = primary_toast_area() {
        return area;
    }

    match monitor {
        Some(size) => ToastArea {
            x: 0.0,
            y: 0.0,
            width: size.width,
            height: size.height,
        },
        None => ToastArea {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        },
    }
}

fn toast_placement_in(position: ToastPosition, area: ToastArea) -> ToastPlacement {
    let target = toast_position_in(position, area);
    let outside_y = match position {
        ToastPosition::TopLeft | ToastPosition::TopCenter | ToastPosition::TopRight => {
            area.y - toast_view::HEIGHT
        }
        ToastPosition::BottomLeft | ToastPosition::BottomCenter | ToastPosition::BottomRight => {
            area.y + area.height
        }
    };
    ToastPlacement {
        x: target.x,
        target_y: target.y,
        outside_y,
    }
}

pub fn toast_position_in(position: ToastPosition, area: ToastArea) -> Point {
    let width = toast_view::WIDTH;
    let height = toast_view::HEIGHT;
    let margin = toast_view::MARGIN;

    let x = match position {
        ToastPosition::TopLeft | ToastPosition::BottomLeft => area.x + margin,
        ToastPosition::TopCenter | ToastPosition::BottomCenter => {
            area.x + (area.width - width) / 2.0
        }
        ToastPosition::TopRight | ToastPosition::BottomRight => {
            area.x + area.width - width - margin
        }
    };

    let y = match position {
        ToastPosition::TopLeft | ToastPosition::TopCenter | ToastPosition::TopRight => {
            area.y + margin
        }
        ToastPosition::BottomLeft | ToastPosition::BottomCenter | ToastPosition::BottomRight => {
            area.y + area.height - height - margin
        }
    };

    Point::new(x.max(0.0), y.max(0.0))
}

pub fn slide_y(placement: ToastPlacement, progress: f32, dismissing: bool) -> f32 {
    let eased = ease_out_cubic(progress.clamp(0.0, 1.0));
    let t = if dismissing { 1.0 - eased } else { eased };
    placement.outside_y + (placement.target_y - placement.outside_y) * t
}

pub fn ease_out_cubic(progress: f32) -> f32 {
    1.0 - (1.0 - progress).powi(3)
}

/// Primary-monitor toast target in logical pixels.
///
/// Uses the Windows work area so a visible taskbar is cleared, while an
/// auto-hide taskbar (which does not reserve work-area space) lets bottom
/// toasts sit near the screen edge. Fullscreen apps get the full monitor.
#[cfg(windows)]
fn primary_toast_area() -> Option<ToastArea> {
    unsafe {
        let monitor =
            win32::MonitorFromPoint(win32::Point { x: 0, y: 0 }, win32::MONITOR_DEFAULTTOPRIMARY);
        if monitor == 0 {
            return None;
        }

        let mut info = win32::MonitorInfo {
            size: std::mem::size_of::<win32::MonitorInfo>() as u32,
            monitor: win32::Rect::default(),
            work: win32::Rect::default(),
            flags: 0,
        };
        if win32::GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }

        let foreground = win32::GetForegroundWindow();
        let mut foreground_rect = win32::Rect::default();
        let got_foreground =
            foreground != 0 && win32::GetWindowRect(foreground, &mut foreground_rect) != 0;
        let fullscreen = got_foreground
            && foreground_rect.left <= info.monitor.left + 2
            && foreground_rect.top <= info.monitor.top + 2
            && foreground_rect.right >= info.monitor.right - 2
            && foreground_rect.bottom >= info.monitor.bottom - 2;

        let area = if fullscreen { info.monitor } else { info.work };

        let mut dpi_x = 0u32;
        let mut dpi_y = 0u32;
        let dpi_ok =
            win32::GetDpiForMonitor(monitor, win32::MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) == 0
                && dpi_x > 0;
        let scale = if dpi_ok { dpi_x as f32 / 96.0 } else { 1.0 };

        Some(ToastArea {
            x: area.left as f32 / scale,
            y: area.top as f32 / scale,
            width: (area.right - area.left).max(1) as f32 / scale,
            height: (area.bottom - area.top).max(1) as f32 / scale,
        })
    }
}

fn clamp_axis(value: f32, extent: f32, available: f32) -> f32 {
    let max = (available - extent - SCREEN_MARGIN).max(SCREEN_MARGIN);
    value.clamp(SCREEN_MARGIN, max)
}

#[cfg(target_os = "windows")]
pub fn overlay_platform_specific() -> window::settings::PlatformSpecific {
    window::settings::PlatformSpecific {
        drag_and_drop: false,
        skip_taskbar: true,
        undecorated_shadow: false,
        corner_preference: window::settings::platform::CornerPreference::DoNotRound,
    }
}

#[cfg(not(target_os = "windows"))]
pub fn overlay_platform_specific() -> window::settings::PlatformSpecific {
    window::settings::PlatformSpecific::default()
}

#[cfg(target_os = "windows")]
pub fn window_platform_specific() -> window::settings::PlatformSpecific {
    window::settings::PlatformSpecific {
        drag_and_drop: false,
        skip_taskbar: false,
        // Match the popup: DWM undecorated shadows flash white on close when the
        // wgpu surface is destroyed before the HWND is gone.
        undecorated_shadow: false,
        corner_preference: window::settings::platform::CornerPreference::DoNotRound,
    }
}

#[cfg(not(target_os = "windows"))]
pub fn window_platform_specific() -> window::settings::PlatformSpecific {
    window::settings::PlatformSpecific::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::prefs::ToastPosition;
    use iced::Size;

    #[test]
    fn popup_is_anchored_above_a_bottom_right_tray() {
        let anchor = TrayAnchor {
            x: 1800.0,
            y: 1040.0,
            width: 24.0,
            height: 24.0,
        };
        let position = popup_position(
            anchor,
            1.0,
            Some(Size::new(1920.0, 1080.0)),
            Size::new(360.0, 200.0),
        );
        assert!(position.y < 1040.0);
        assert!(position.x + 360.0 <= 1920.0);
    }

    #[test]
    fn popup_flips_below_a_top_anchored_tray() {
        let anchor = TrayAnchor {
            x: 100.0,
            y: 0.0,
            width: 24.0,
            height: 24.0,
        };
        let position = popup_position(
            anchor,
            1.0,
            Some(Size::new(1920.0, 1080.0)),
            Size::new(360.0, 200.0),
        );
        assert!(position.y >= 24.0);
    }

    #[test]
    fn toast_positions_respect_the_requested_corner() {
        let area = ToastArea {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let top_right = toast_position_in(ToastPosition::TopRight, area);
        assert!(top_right.x > 960.0);
        assert!(top_right.y < 100.0);

        let bottom_left = toast_position_in(ToastPosition::BottomLeft, area);
        assert!(bottom_left.x < 100.0);
        assert!(bottom_left.y > 900.0);
    }

    #[test]
    fn slide_progress_moves_from_outside_to_target() {
        let placement = ToastPlacement {
            x: 10.0,
            target_y: 100.0,
            outside_y: 200.0,
        };
        assert!((slide_y(placement, 0.0, false) - 200.0).abs() < f32::EPSILON);
        assert!((slide_y(placement, 1.0, false) - 100.0).abs() < f32::EPSILON);
        assert!((slide_y(placement, 0.0, true) - 100.0).abs() < f32::EPSILON);
        assert!((slide_y(placement, 1.0, true) - 200.0).abs() < f32::EPSILON);
        let mid = slide_y(placement, 0.5, false);
        assert!(mid > 100.0 && mid < 200.0);
    }
}
