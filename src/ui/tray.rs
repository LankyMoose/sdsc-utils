//! System tray icon construction and event bridging.
//!
//! Windows runs the tray icon (window, menu, modal menu loop) on a dedicated
//! thread: `TrackPopupMenu` blocks its caller, and on the iced thread that
//! wedged the whole app when a menu outlived dismissal (right-click during
//! boot toast placement held the loop in `NtUserTrackPopupMenuEx` until the
//! process was killed; WinDbg: `APPLICATION_HANG`). A stuck menu now costs
//! one thread; HID, toasts, and Settings keep running. Other platforms keep
//! tray-icon on the main thread (their toolkits require it).

#[cfg(not(windows))]
use crate::controller::model::ControllerStatus;
use crate::ui::icon;
use crate::ui::layout::TrayAnchor;

use iced::futures::Stream;
use iced::futures::channel::mpsc;
use iced::stream;

#[cfg(not(windows))]
use tray_icon::TrayIcon;
#[cfg(not(windows))]
use tray_icon::TrayIconBuilder;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{MouseButton as TrayMouseButton, MouseButtonState, TrayIconEvent};

pub const QUIT_ID: &str = "quit";
pub const SETTINGS_ID: &str = "settings";

#[derive(Debug, Clone)]
pub enum TrayEvent {
    Menu(String),
    LeftClick(TrayAnchor),
}

/// Build the Settings / Exit menu (called on whichever thread owns the tray;
/// muda `Menu` is `Rc`-based and never crosses threads).
fn build_tray_menu() -> Menu {
    let menu = Menu::new();
    let _ = menu.append(&MenuItem::with_id(SETTINGS_ID, "Settings", true, None));
    let _ = menu.append(&MenuItem::with_id(QUIT_ID, "Exit", true, None));
    menu
}

/// Build the tray icon with Settings / Exit menu for the current controller set.
#[cfg(not(windows))]
pub fn create_tray(controllers: &[ControllerStatus]) -> Option<TrayIcon> {
    match TrayIconBuilder::new()
        .with_tooltip(icon::tooltip_for_controllers(controllers))
        .with_icon(icon::icon_for_controllers(controllers))
        .with_menu(Box::new(build_tray_menu()))
        .with_menu_on_left_click(false)
        .build()
    {
        Ok(tray) => Some(tray),
        Err(err) => {
            crate::platform::app_log::error(format!("failed to create tray icon: {err}"));
            None
        }
    }
}

/// Sync tray icon + tooltip with the latest controller snapshot.
#[cfg(not(windows))]
pub fn apply_tray(tray: &mut TrayIcon, controllers: &[ControllerStatus]) {
    let icon = icon::icon_for_controllers(controllers);
    let tooltip = icon::tooltip_for_controllers(controllers);
    let _ = tray.set_icon(Some(icon));
    let _ = tray.set_tooltip(Some(tooltip));
}

/// Windows tray thread: owns the icon, menu, and message pump.
#[cfg(windows)]
mod threaded {
    use super::icon;
    use crate::controller::model::ControllerStatus;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    use tray_icon::TrayIconBuilder;
    use windows::Win32::Foundation::{HANDLE, WAIT_FAILED};
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
        PeekMessageW, QS_ALLINPUT, TranslateMessage,
    };
    use windows::core::PCWSTR;

    /// Commands main → tray thread. Plain data only: tray types (`TrayIcon`,
    /// muda `Menu`) stay on the tray thread (`Rc`-based, `!Send`).
    enum TrayCmd {
        Sync { connected: bool, tooltip: String },
        Shutdown,
    }

    /// Owned handle to the tray thread. Drop requests icon removal; never
    /// joins (a wedged menu must not block exit — the OS reclaims the thread).
    pub struct TrayHandle {
        tx: mpsc::Sender<TrayCmd>,
        /// `CreateEventW` handle as raw (`HANDLE` is `!Send`).
        wake: isize,
    }

    impl Drop for TrayHandle {
        fn drop(&mut self) {
            let _ = self.tx.send(TrayCmd::Shutdown);
            unsafe {
                let _ = SetEvent(HANDLE(self.wake as *mut _));
            }
        }
    }

    /// Build the tray icon on its own thread; block briefly for the build
    /// result (fast local work, same as the old synchronous build). Bounded:
    /// a panicking tray thread must degrade to trayless, never hang boot.
    pub fn create_tray(controllers: &[ControllerStatus]) -> Option<TrayHandle> {
        let (tx, rx) = mpsc::channel::<TrayCmd>();
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        // Auto-reset, initially non-signaled.
        let wake = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.ok()?;
        let wake_raw = wake.0 as isize;
        let initial = (
            !controllers.is_empty(),
            icon::tooltip_for_controllers(controllers),
        );
        thread::spawn(move || tray_thread(rx, wake_raw, ready_tx, initial));
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(true) => Some(TrayHandle { tx, wake: wake_raw }),
            Ok(false) => None,
            Err(_) => {
                // Tray thread died or stalled before reporting: run trayless
                // (existing `None` tolerance) rather than hang boot. If it
                // recovers, it runs ownerless until process exit.
                crate::platform::app_log::error(
                    "tray thread failed to report readiness; running trayless",
                );
                None
            }
        }
    }

    /// Sync tray icon + tooltip (non-blocking: queues the command and wakes
    /// the thread; applied on its next pump).
    pub fn apply_tray(handle: &TrayHandle, controllers: &[ControllerStatus]) {
        let cmd = TrayCmd::Sync {
            connected: !controllers.is_empty(),
            tooltip: icon::tooltip_for_controllers(controllers),
        };
        if handle.tx.send(cmd).is_ok() {
            unsafe {
                let _ = SetEvent(HANDLE(handle.wake as *mut _));
            }
        }
    }

    fn tray_thread(
        rx: mpsc::Receiver<TrayCmd>,
        wake_raw: isize,
        ready: mpsc::Sender<bool>,
        initial: (bool, String),
    ) {
        let wake = HANDLE(wake_raw as *mut _);
        let (connected, tooltip) = initial;
        let tray = match TrayIconBuilder::new()
            .with_tooltip(tooltip)
            .with_icon(icon::icon_for_connected(connected))
            .with_menu(Box::new(super::build_tray_menu()))
            .with_menu_on_left_click(false)
            .build()
        {
            Ok(tray) => {
                let _ = ready.send(true);
                tray
            }
            Err(err) => {
                crate::platform::app_log::error(format!("failed to create tray icon: {err}"));
                let _ = ready.send(false);
                return;
            }
        };
        loop {
            // Wait for commands or window messages. The tray WndProc (incl. a
            // modal menu loop) runs on this thread — never the iced thread.
            let wait = unsafe {
                MsgWaitForMultipleObjectsEx(
                    Some(&[wake]),
                    INFINITE,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                )
            };
            if wait == WAIT_FAILED {
                // Should not happen (the event lives as long as the thread);
                // back off instead of hot-spinning.
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            let mut shutdown = false;
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    TrayCmd::Sync { connected, tooltip } => {
                        let _ = tray.set_icon(Some(icon::icon_for_connected(connected)));
                        let _ = tray.set_tooltip(Some(tooltip));
                    }
                    TrayCmd::Shutdown => shutdown = true,
                }
            }
            if shutdown {
                // Best-effort icon removal on the owning thread; Drop runs here.
                let _ = tray.set_visible(false);
                return;
            }
            unsafe {
                let mut msg = std::mem::zeroed::<MSG>();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
    }
}

#[cfg(windows)]
pub use threaded::{TrayHandle, apply_tray, create_tray};

/// Bridges the global `tray-icon` / `muda` event handlers into the iced runtime.
///
/// Both handlers are backed by a `OnceLock`, so this stream must be created
/// exactly once for the lifetime of the process. Thread-agnostic: handlers
/// fire on whichever thread owns the tray window.
pub fn tray_events() -> impl Stream<Item = TrayEvent> {
    stream::channel(64, async move |output: mpsc::Sender<TrayEvent>| {
        let menu_output = output.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            // Every clone owns a guaranteed slot, so `try_send` cannot be starved.
            let _ = menu_output.clone().try_send(TrayEvent::Menu(event.id.0));
        }));

        let icon_output = output.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let TrayIconEvent::Click {
                rect,
                button: TrayMouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let anchor = TrayAnchor {
                    x: rect.position.x as f32,
                    y: rect.position.y as f32,
                    width: rect.size.width as f32,
                    height: rect.size.height as f32,
                };
                let _ = icon_output.clone().try_send(TrayEvent::LeftClick(anchor));
            }
        }));

        std::future::pending::<()>().await;
    })
}
