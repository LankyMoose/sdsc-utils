//! Controller popup flyout: rows, window lifecycle, and update-arm handlers.
//!
//! Behavior-preserving extraction from `app::App`; no logic change.

use super::{App, Message};
use crate::controller::dualsense::identity as dualsense;
use crate::persist::analytics;
use crate::ui::layout::{
    focus_window, move_window, overlay_platform_specific, popup_position, show_window,
};
use crate::ui::popup::{self as popup_view, ControllerRow, PopupMessage};
use iced::widget::operation;
use iced::{Size, Task, window};
use std::time::Instant;

impl App {
    pub(crate) fn handle_popup_opened(&mut self, id: window::Id) -> Task<Message> {
        if self.popup_metrics_cached {
            self.reveal_popup(id)
        } else {
            super::place_popup(id)
        }
    }

    pub(crate) fn handle_place_popup(
        &mut self,
        id: window::Id,
        scale: f32,
        monitor: Option<Size>,
    ) -> Task<Message> {
        self.popup_scale = if scale > 0.0 { scale } else { 1.0 };
        self.popup_monitor = monitor;
        self.popup_metrics_cached = true;
        self.reveal_popup(id)
    }

    pub(crate) fn sync_popup_rows(&mut self) {
        if self
            .start_state
            .powering_off
            .as_ref()
            .is_some_and(|s| !self.session.controllers.iter().any(|c| c.serial == *s))
        {
            self.start_state.powering_off = None;
        }
        let threshold = self.session.prefs.low_battery_percent;
        let mut rows = Vec::new();
        for controller in &self.session.controllers {
            let eta = if self.session.prefs.analytics_enabled {
                self.session
                    .analytics
                    .eta_for(controller)
                    .map(analytics::format_eta_ring)
            } else {
                None
            };
            rows.push(ControllerRow::connected(
                controller,
                self.session.known.is_remembered(controller.serial.as_ref()),
                dualsense::is_storable_serial(controller.serial.as_ref())
                    && !super::is_emulated_serial(controller.serial.as_ref()),
                self.session
                    .known
                    .nickname(controller.serial.as_ref())
                    .map(str::to_string),
                threshold,
                eta,
            ));
        }
        for controller in self
            .session
            .known
            .remembered_disconnected(&self.session.controllers)
        {
            let nickname = self
                .session
                .known
                .nickname(controller.serial.as_ref())
                .map(str::to_string);
            let eta = if self.session.prefs.analytics_enabled {
                self.session
                    .analytics
                    .eta_play_at(controller.serial.as_ref(), controller.percent)
                    .map(analytics::format_eta_ring)
            } else {
                None
            };
            rows.push(ControllerRow::disconnected(controller, nickname, eta));
        }
        self.popup_rows = rows;
    }

    /// Rebuild popup rows and, when the flyout is open, resize/re-anchor if height changed.
    pub(crate) fn sync_popup_rows_and_fit(&mut self) -> Task<Message> {
        let before = self.popup_window_height();
        self.sync_popup_rows();
        let after = self.popup_window_height();
        if before == after {
            Task::none()
        } else {
            self.fit_popup_window()
        }
    }

    pub(crate) fn popup_window_height(&self) -> f32 {
        popup_view::window_height(self.popup_rows.len(), self.popup_monitor.map(|m| m.height))
    }

    pub(crate) fn fit_popup_window(&self) -> Task<Message> {
        let Some(id) = self.popup_window else {
            return Task::none();
        };
        let height = self.popup_window_height();
        let size = Size::new(popup_view::WIDTH, height);
        let position = popup_position(self.tray_anchor, self.popup_scale, self.popup_monitor, size);
        window::resize(id, size).chain(move_window(id, position))
    }

    pub(crate) fn toggle_popup(&mut self) -> Task<Message> {
        if self.popup_window.is_some() {
            self.close_popup()
        } else {
            self.open_popup()
        }
    }

    pub(crate) fn open_popup(&mut self) -> Task<Message> {
        if let Some(id) = self.popup_window {
            return focus_window(id);
        }

        self.popup_state.cancel();
        let height = self.popup_window_height();
        let (id, open) = window::open(window::Settings {
            size: Size::new(popup_view::WIDTH, height),
            position: window::Position::Default,
            visible: false,
            transparent: crate::ui::chrome::transparent_windows(),
            resizable: false,
            decorations: false,
            level: window::Level::AlwaysOnTop,
            exit_on_close_request: true,
            platform_specific: overlay_platform_specific(),
            ..window::Settings::default()
        });

        self.popup_window = Some(id);
        open.map(Message::PopupOpened)
            .chain(self.sync_toast_zorder())
    }

    pub(crate) fn reveal_popup(&mut self, id: window::Id) -> Task<Message> {
        self.popup_state.begin_entrance(Instant::now());
        let height = self.popup_window_height();
        let size = Size::new(popup_view::WIDTH, height);
        let position = popup_position(self.tray_anchor, self.popup_scale, self.popup_monitor, size);
        window::resize(id, size)
            .chain(move_window(id, position))
            .chain(show_window(id))
            .chain(focus_window(id))
    }

    pub(crate) fn close_popup(&mut self) -> Task<Message> {
        self.popup_state.cancel();
        // Hide first (same as Settings) so iced's surface teardown cannot paint a
        // brief empty root-colored frame while the HWND is still visible.
        // Keep `popup_window` until WindowClosed.
        match self.popup_window {
            Some(id) => window::set_mode(id, window::Mode::Hidden).chain(window::close(id)),
            None => Task::none(),
        }
    }

    pub(crate) fn on_popup_message(&mut self, message: PopupMessage) -> Task<Message> {
        match message {
            PopupMessage::OpenSettings => {
                let close = self.close_popup();
                close.chain(self.open_configure())
            }
            PopupMessage::Identify(serial) => {
                if self.identify(&serial) {
                    self.popup_state.begin_identify_flash(&serial);
                    self.start_state.begin_identify_flash(&serial);
                }
                Task::none()
            }
            PopupMessage::PowerOff(serial) => {
                self.power_off(&serial);
                Task::none()
            }
            PopupMessage::ToggleRemember(serial) => self.toggle_remember(&serial),
            PopupMessage::BeginEdit(serial) => {
                let current = self
                    .session
                    .known
                    .nickname(serial.as_ref())
                    .map(str::to_string);
                self.popup_state.begin_edit(&serial, current.as_deref());
                Task::batch([
                    operation::focus(popup_view::nickname_input_id()),
                    operation::select_all(popup_view::nickname_input_id()),
                ])
            }
            PopupMessage::DraftChanged(value) => {
                self.popup_state.draft = value;
                Task::none()
            }
            PopupMessage::CommitNickname => {
                if let Some((serial, nickname)) = self.popup_state.commit() {
                    self.set_nickname(&serial, nickname)
                } else {
                    Task::none()
                }
            }
            PopupMessage::CancelEdit => {
                self.popup_state.cancel();
                Task::none()
            }
        }
    }
}
