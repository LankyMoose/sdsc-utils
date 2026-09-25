//! System tray icon construction and event bridging.

use crate::controller::model::ControllerStatus;
use crate::ui::icon;
use crate::ui::layout::TrayAnchor;

use iced::futures::Stream;
use iced::futures::channel::mpsc;
use iced::stream;

use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{
    MouseButton as TrayMouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

pub const QUIT_ID: &str = "quit";
pub const SETTINGS_ID: &str = "settings";

#[derive(Debug, Clone)]
pub enum TrayEvent {
    Menu(String),
    LeftClick(TrayAnchor),
}

/// Build the tray icon with Settings / Exit menu for the current controller set.
pub fn create_tray(controllers: &[ControllerStatus]) -> Option<TrayIcon> {
    let menu = Menu::new();
    let _ = menu.append(&MenuItem::with_id(SETTINGS_ID, "Settings", true, None));
    let _ = menu.append(&MenuItem::with_id(QUIT_ID, "Exit", true, None));

    match TrayIconBuilder::new()
        .with_tooltip(icon::tooltip_for_controllers(controllers))
        .with_icon(icon::icon_for_controllers(controllers))
        .with_menu(Box::new(menu))
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
pub fn apply_tray(tray: &mut TrayIcon, controllers: &[ControllerStatus]) {
    let icon = icon::icon_for_controllers(controllers);
    let tooltip = icon::tooltip_for_controllers(controllers);
    let _ = tray.set_icon(Some(icon));
    let _ = tray.set_tooltip(Some(tooltip));
}

/// Bridges the global `tray-icon` / `muda` event handlers into the iced runtime.
///
/// Both handlers are backed by a `OnceLock`, so this stream must be created
/// exactly once for the lifetime of the process.
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
