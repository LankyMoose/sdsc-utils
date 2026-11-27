//! Popup / toast placement and iced window platform settings.

use crate::persist::prefs::ToastPosition;
use crate::platform::host::{self, Host};
use crate::ui::toast::view as toast_view;

use iced::{Point, Size, Task, window};

/// Gap between the tray icon and the popup.
pub const POPUP_GAP: f32 = 8.0;
/// Minimum distance kept from any screen edge.
pub const SCREEN_MARGIN: f32 = 8.0;
/// Slide-in / slide-out duration for overlay toasts.
pub const TOAST_SLIDE_DURATION: std::time::Duration = std::time::Duration::from_millis(250);
/// Max wall time applied per toast animation frame (~2× the ~16ms UI tick).
/// Caps a stalled frame so slide-in cannot jump to the rest pose in one tick.
pub const TOAST_SLIDE_MAX_FRAME_DT: std::time::Duration = crate::ui::motion::MAX_FRAME_DT;

/// Screen rectangle of the tray icon, in physical pixels.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
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

/// Full monitor rectangle in logical pixels (covers the taskbar).
#[derive(Debug, Clone, Copy)]
pub struct MonitorCover {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl MonitorCover {
    pub fn size(self) -> Size {
        Size::new(self.width, self.height)
    }

    pub fn origin(self) -> Point {
        Point::new(self.x, self.y)
    }
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
/// Callers that have Home / Settings / popup open should raise those
/// windows (focus/refresh), then re-raise the toast with
/// [`raise_toast_topmost`] so it stays visible above Home's full-screen
/// scrim without stealing focus.
/// normal-level window and stays behind the toast by design.
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
            host::set_no_activate(hwnd, true);
            host::show_window(hwnd);
        })
        .discard()
    }
    #[cfg(not(windows))]
    {
        window::set_mode(id, window::Mode::Windowed)
            .chain(window::set_level(id, window::Level::AlwaysOnTop))
    }
}

/// Debug quiet mode: relocate to the secondary monitor and show without
/// activating (see [`crate::platform::debug_quiet`]).
#[cfg(windows)]
fn quiet_show<Message: Send + 'static>(id: window::Id, show: bool) -> Task<Message> {
    window::run(id, move |window| {
        use window::raw_window_handle::RawWindowHandle;

        let Ok(handle) = window.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(win32_handle) = handle.as_raw() else {
            return;
        };
        let hwnd = win32_handle.hwnd.get();
        if crate::platform::debug_quiet::offscreen() {
            host::move_offscreen(hwnd);
        } else {
            host::move_to_secondary_monitor(hwnd);
        }
        if show {
            host::set_no_activate(hwnd, true);
            host::show_window(hwnd);
            if crate::platform::debug_quiet::offscreen() && !host::window_is_offscreen(hwnd) {
                // Debug QA contract: quiet automated runs must never appear
                // on any monitor. The Python watcher is the hard gate; this
                // witness catches regressions in the app log too.
                crate::platform::app_log::error("ui-diag: offscreen violation after quiet show");
                debug_assert!(false, "quiet show placed a window on-screen");
            }
        }
    })
    .discard()
}

/// Give a UI window focus (`window::gain_focus`). In debug quiet mode it is
/// shown on the secondary monitor without activation instead.
pub fn focus_window<Message: Send + 'static>(id: window::Id) -> Task<Message> {
    #[cfg(windows)]
    if crate::platform::debug_quiet::enabled() {
        return quiet_show(id, true);
    }
    window::gain_focus(id)
}

/// Show or hide the one shell HWND through the same raw Win32 path.
///
/// iced's [`window::set_mode`] effect and the raw quiet-mode show path can
/// complete out of order across a rapid PS hide/resume. Every request carries
/// a monotonic token; a task that is no longer the latest visibility request
/// does nothing, so the last call always wins.
pub fn set_shell_visible<Message: Send + 'static>(
    id: window::Id,
    visible: bool,
    epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
    token: u64,
) -> Task<Message> {
    #[cfg(windows)]
    {
        let quiet = crate::platform::debug_quiet::enabled();
        let offscreen = crate::platform::debug_quiet::offscreen();
        window::run(id, move |window| {
            use std::sync::atomic::Ordering;
            use window::raw_window_handle::RawWindowHandle;

            if epoch.load(Ordering::SeqCst) != token {
                return;
            }
            let Ok(handle) = window.window_handle() else {
                return;
            };
            let RawWindowHandle::Win32(win32_handle) = handle.as_raw() else {
                return;
            };
            let hwnd = win32_handle.hwnd.get();
            let style = unsafe { crate::platform::win32::GetWindowLongW(hwnd, -16) };
            let currently_visible = style & 0x1000_0000 != 0;
            // A mode switch re-asserts visibility so a stale raw hide cannot
            // win, but ShowWindow on an already-visible window forces an extra
            // DWM repaint (the one-frame backdrop flicker). No-op when the
            // requested state is already true.
            if currently_visible == visible {
                return;
            }
            if visible {
                if quiet {
                    if offscreen {
                        host::move_offscreen(hwnd);
                    } else {
                        host::move_to_secondary_monitor(hwnd);
                    }
                    host::set_no_activate(hwnd, true);
                }
                host::show_window(hwnd);
            } else {
                host::hide(hwnd);
            }
        })
        .discard()
    }
    #[cfg(not(windows))]
    {
        use std::sync::atomic::Ordering;
        if epoch.load(Ordering::SeqCst) != token {
            return Task::none();
        }
        if visible {
            window::set_mode(id, window::Mode::Windowed)
        } else {
            window::set_mode(id, window::Mode::Hidden)
        }
    }
}

/// Set the shared shell window's z-order band without touching winit's
/// show/hide path (its level change unconditionally calls `ShowWindow`, which
/// can flash a transparent frame during a mode switch).
pub fn set_shell_level<Message: Send + 'static>(id: window::Id, topmost: bool) -> Task<Message> {
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
            let insert_after = if topmost {
                crate::platform::win32::HWND_TOPMOST
            } else {
                crate::platform::win32::HWND_NOTOPMOST
            };
            unsafe {
                crate::platform::win32::SetWindowPos(
                    hwnd,
                    insert_after,
                    0,
                    0,
                    0,
                    0,
                    crate::platform::win32::SWP_NOMOVE
                        | crate::platform::win32::SWP_NOSIZE
                        | crate::platform::win32::SWP_NOACTIVATE,
                );
            }
        })
        .discard()
    }
    #[cfg(not(windows))]
    {
        window::set_level(
            id,
            if topmost {
                window::Level::AlwaysOnTop
            } else {
                window::Level::Normal
            },
        )
    }
}

/// Show a hidden UI window (`Mode::Windowed`). Quiet mode: no activation,
/// secondary monitor.
pub fn show_window<Message: Send + 'static>(id: window::Id) -> Task<Message> {
    #[cfg(windows)]
    if crate::platform::debug_quiet::enabled() {
        return quiet_show(id, true);
    }
    window::set_mode(id, window::Mode::Windowed)
}

/// Move a window (`window::move_to`, primary-monitor logical coords). Quiet
/// mode re-applies the secondary-monitor offset after the move.
pub fn move_window<Message: Send + 'static>(id: window::Id, position: Point) -> Task<Message> {
    #[cfg(windows)]
    if crate::platform::debug_quiet::enabled() {
        return window::move_to(id, position).chain(quiet_show(id, false));
    }
    window::move_to(id, position)
}

/// Whether newly opened UI windows should start visible (false in quiet mode
/// so they never activate on creation; [`focus_window`] shows them).
pub fn open_visible() -> bool {
    !crate::platform::debug_quiet::enabled()
}

/// Show a system overlay without activating it. Debug quiet mode relocates it
/// to the secondary monitor (same as the Browser/app it overlays) and shows it
/// without activation; normal sessions keep the computed primary placement.
pub fn show_overlay_without_activate<Message: Send + 'static>(id: window::Id) -> Task<Message> {
    #[cfg(windows)]
    if crate::platform::debug_quiet::enabled() {
        return quiet_show(id, true);
    }
    show_toast_without_activate(id)
}

/// Re-assert toast topmost (e.g. after Settings / Home / popup open or close).
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
            host::set_no_activate(hwnd, true);
        })
        .discard()
    }
    #[cfg(not(windows))]
    {
        window::set_level(id, window::Level::AlwaysOnTop)
    }
}

/// Disable DWM open/close transitions for a window (the Windows 11 scale
/// effect) while it is still hidden, before its first show.
pub fn disable_window_transitions<Message: Send + 'static>(id: window::Id) -> Task<Message> {
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
            host::disable_transitions(win32_handle.hwnd.get());
        })
        .discard()
    }
    #[cfg(not(windows))]
    {
        let _ = id;
        Task::none()
    }
}

/// Raise the non-activating toast to the top of the z-order without stealing
/// focus from Home or Settings underneath it.
pub fn raise_toast_topmost<Message: Send + 'static>(id: window::Id) -> Task<Message> {
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
            host::set_no_activate(win32_handle.hwnd.get(), true);
            host::raise_no_activate(win32_handle.hwnd.get());
        })
        .discard()
    }
    #[cfg(not(windows))]
    {
        window::set_level(id, window::Level::AlwaysOnTop)
    }
}

/// Raise an interactive UI window to the top of the topmost band.
/// The toast is raised after these windows so it stays on top.
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
            host::focus(hwnd);
        })
        .discard()
    }
    #[cfg(not(windows))]
    {
        window::set_level(id, window::Level::AlwaysOnTop)
    }
}

/// Win32 HWND for an iced window (`None` when unavailable / non-Win32).
pub fn window_hwnd(id: window::Id) -> Task<Option<isize>> {
    #[cfg(windows)]
    {
        window::run(id, |window| {
            use window::raw_window_handle::RawWindowHandle;

            let Ok(handle) = window.window_handle() else {
                return None;
            };
            let RawWindowHandle::Win32(win32_handle) = handle.as_raw() else {
                return None;
            };
            Some(win32_handle.hwnd.get())
        })
    }
    #[cfg(not(windows))]
    {
        let _ = id;
        Task::done(None)
    }
}

/// Ask Win32 to invalidate a window HWND so its next view can present.
pub fn invalidate_window<Message: Send + 'static>(id: window::Id) -> Task<Message> {
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
            host::invalidate_hwnd(hwnd);
        })
        .discard()
    }
    #[cfg(not(windows))]
    {
        let _ = id;
        Task::none()
    }
}

/// Ask Win32 to invalidate the toast HWND so a new view can present.
pub fn invalidate_toast<Message: Send + 'static>(id: window::Id) -> Task<Message> {
    invalidate_window(id)
}

/// Fixed toast HWND size. One source for creation and remount: the union
/// box must survive the generation remount or the card is clipped.
pub fn toast_window_size() -> Size {
    Size::new(toast_view::WIDTH, toast_view::WINDOW_HEIGHT)
}

/// Nudge toast size by ±1px so wgpu rebuilds the surface for a new message.
/// Height stays at the union size (create and remount must agree).
pub fn remount_toast_surface<Message: Send + 'static>(
    id: window::Id,
    generation: u64,
) -> Task<Message> {
    let mut size = toast_window_size();
    if !generation.is_multiple_of(2) {
        size.width += 1.0;
    }
    window::resize(id, size)
}

/// Hide an overlay shown through [`show_overlay_without_activate`].
///
/// On Windows these overlays are shown via raw `ShowWindow`, so winit's
/// `VISIBLE` flag stays false; `set_mode(Hidden)` would then be a no-op and
/// leave the window on screen.
pub fn hide_overlay_without_activate<Message: Send + 'static>(id: window::Id) -> Task<Message> {
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
            host::hide(hwnd);
        })
        .discard()
    }
    #[cfg(not(windows))]
    {
        window::set_mode(id, window::Mode::Hidden)
    }
}

/// Hide the overlay toast; see [`hide_overlay_without_activate`].
pub fn hide_toast<Message: Send + 'static>(id: window::Id) -> Task<Message> {
    hide_overlay_without_activate(id)
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

/// Fixed toast HWND geometry `(y, height)` covering both slide extremes.
/// The window itself never moves per frame; the card is drawn translated
/// inside this union box (which may extend off-screen).
pub fn toast_window_geometry(placement: ToastPlacement) -> (f32, f32) {
    (
        placement.target_y.min(placement.outside_y),
        toast_view::WINDOW_HEIGHT,
    )
}

/// Toast top-left in cover-local coordinates (screen placement minus cover origin).
pub fn toast_local_in_cover(
    placement: ToastPlacement,
    progress: f32,
    dismissing: bool,
    cover: MonitorCover,
) -> Point {
    let screen_y = slide_y(placement, progress, dismissing);
    Point::new(placement.x - cover.x, screen_y - cover.y)
}

pub use crate::ui::motion::ease_out_cubic;

/// Advance toast slide elapsed with a per-frame cap so a UI stall cannot skip
/// the rest of the animation in one tick.
///
/// Returns `(new_elapsed, progress 0..=1, raw_dt_was_capped)`.
pub fn advance_toast_slide(
    elapsed: std::time::Duration,
    raw_dt: std::time::Duration,
    duration: std::time::Duration,
) -> (std::time::Duration, f32, bool) {
    crate::ui::motion::advance_capped(elapsed, raw_dt, duration)
}

/// Primary-monitor full cover in logical pixels (taskbar included).
pub fn primary_monitor_cover() -> Option<MonitorCover> {
    #[cfg(windows)]
    {
        primary_monitor_metrics().map(|m| m.cover)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Whether the system cursor is currently on the primary monitor.
///
/// Used for immersive Launcher idle cursor hide — secondary monitors keep a visible
/// cursor even while Launcher covers the primary.
///
/// Delegates to the [`Host`](crate::platform::host::Host) monitors seam; the
/// Windows branch compares the cursor's monitor against the primary verbatim,
/// other platforms return `false`.
pub fn cursor_on_primary_monitor() -> bool {
    crate::platform::host::Host::cursor_on_primary_monitor()
}

/// Primary-monitor toast target in logical pixels.
///
/// Uses the Windows work area so a visible taskbar is cleared, while an
/// auto-hide taskbar (which does not reserve work-area space) lets bottom
/// toasts sit near the screen edge. Fullscreen apps get the full monitor.
#[cfg(windows)]
fn primary_toast_area() -> Option<ToastArea> {
    let metrics = primary_monitor_metrics()?;
    let area = if metrics.foreground_covers_monitor {
        metrics.cover
    } else {
        metrics.work
    };
    Some(ToastArea {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height,
    })
}

#[cfg(windows)]
struct PrimaryMonitorMetrics {
    cover: MonitorCover,
    work: MonitorCover,
    foreground_covers_monitor: bool,
}

#[cfg(windows)]
fn primary_monitor_metrics() -> Option<PrimaryMonitorMetrics> {
    // Geometry comes from the host seam as physical pixels + DPI scale (no UI
    // type crosses into `platform`); the logical cover conversion stays here.
    let (monitor, work, scale, foreground_covers_monitor) = Host::primary_metrics()?;
    let to_cover = |rect: (i32, i32, i32, i32)| MonitorCover {
        x: rect.0 as f32 / scale,
        y: rect.1 as f32 / scale,
        width: (rect.2 - rect.0).max(1) as f32 / scale,
        height: (rect.3 - rect.1).max(1) as f32 / scale,
    };

    Some(PrimaryMonitorMetrics {
        cover: to_cover(monitor),
        work: to_cover(work),
        foreground_covers_monitor,
    })
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
        corner_preference: crate::ui::chrome::corner_preference(),
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
        corner_preference: crate::ui::chrome::corner_preference(),
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

    #[test]
    fn toast_window_geometry_contains_both_slide_extremes() {
        let bottom = ToastPlacement {
            x: 10.0,
            target_y: 1080.0 - toast_view::HEIGHT - toast_view::MARGIN,
            outside_y: 1080.0,
        };
        let (y, h) = toast_window_geometry(bottom);
        assert_eq!(y, bottom.target_y);
        assert!(y + h >= bottom.outside_y + toast_view::HEIGHT);
        assert!(y <= bottom.target_y);
        let top = ToastPlacement {
            x: 10.0,
            target_y: toast_view::MARGIN,
            outside_y: -toast_view::HEIGHT,
        };
        let (y, h) = toast_window_geometry(top);
        assert_eq!(y, top.outside_y);
        assert!(y + h >= top.target_y + toast_view::HEIGHT);
        for placement in [bottom, top] {
            let (y, h) = toast_window_geometry(placement);
            assert_eq!(
                h,
                toast_window_size().height,
                "remount must preserve the union height"
            );
            for step in 0..=10 {
                let progress = step as f32 / 10.0;
                let card_y = slide_y(placement, progress, false);
                assert!(
                    card_y >= y - 0.001 && card_y + toast_view::HEIGHT <= y + h + 0.001,
                    "slide pose {card_y} clipped by window {y}..{}",
                    y + h
                );
            }
        }
    }

    #[test]
    fn toast_local_in_cover_subtracts_origin() {
        let placement = ToastPlacement {
            x: 100.0,
            target_y: 50.0,
            outside_y: -100.0,
        };
        let cover = MonitorCover {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let resting = toast_local_in_cover(placement, 1.0, false, cover);
        assert!((resting.x - 100.0).abs() < f32::EPSILON);
        assert!((resting.y - 50.0).abs() < f32::EPSILON);

        let cover_offset = MonitorCover {
            x: 1920.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let placement2 = ToastPlacement {
            x: 1936.0,
            target_y: 16.0,
            outside_y: -84.0,
        };
        let local = toast_local_in_cover(placement2, 1.0, false, cover_offset);
        assert!((local.x - 16.0).abs() < f32::EPSILON);
        assert!((local.y - 16.0).abs() < f32::EPSILON);

        let placing = toast_local_in_cover(placement, 0.0, false, cover);
        assert!((placing.y - (-100.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn capped_slide_does_not_jump_on_large_dt() {
        use std::time::Duration;
        let (elapsed, progress, capped) = advance_toast_slide(
            Duration::ZERO,
            Duration::from_millis(400),
            TOAST_SLIDE_DURATION,
        );
        assert!(capped);
        assert_eq!(elapsed, TOAST_SLIDE_MAX_FRAME_DT);
        assert!(progress < 1.0);
        assert!((progress - 32.0 / 250.0).abs() < 0.001);
    }

    #[test]
    fn capped_slide_reaches_rest_pose_after_enough_steps() {
        use std::time::Duration;
        let placement = ToastPlacement {
            x: 10.0,
            target_y: 100.0,
            outside_y: 200.0,
        };
        let mut elapsed = Duration::ZERO;
        let mut progress = 0.0;
        // One huge gap still needs several capped steps to finish.
        while progress < 1.0 {
            let (next, p, _) =
                advance_toast_slide(elapsed, Duration::from_millis(400), TOAST_SLIDE_DURATION);
            elapsed = next;
            progress = p;
            assert!(
                elapsed <= TOAST_SLIDE_DURATION + TOAST_SLIDE_MAX_FRAME_DT,
                "should reach progress 1 without unbounded looping"
            );
        }
        assert!((slide_y(placement, progress, false) - placement.target_y).abs() < f32::EPSILON);
    }

    #[test]
    fn slide_in_must_apply_end_keyframe_even_if_elapsed_past_duration() {
        use std::time::Duration;
        let placement = ToastPlacement {
            x: 10.0,
            target_y: 100.0,
            outside_y: 200.0,
        };
        // Simulate many capped frames until progress hits 1 — the caller must
        // still move to target_y (dismiss-style end keyframe).
        let mut elapsed = Duration::ZERO;
        let mut progress = 0.0;
        while progress < 1.0 {
            let (next, p, _) =
                advance_toast_slide(elapsed, Duration::from_millis(50), TOAST_SLIDE_DURATION);
            elapsed = next;
            progress = p;
            assert!(
                elapsed <= TOAST_SLIDE_DURATION + TOAST_SLIDE_MAX_FRAME_DT,
                "should reach progress 1 without unbounded looping"
            );
        }
        assert!((slide_y(placement, progress, false) - placement.target_y).abs() < f32::EPSILON);
    }
}
