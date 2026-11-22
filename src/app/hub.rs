//! Managed-app hub: which controller-first apps exist, which one is active,
//! and the shell effects a transition requires.
//!
//! Phase 0 keeps this pure bookkeeping. The topmost PS hub window, app catalog,
//! and Start registration land in phase 1; the standalone Browser window in
//! phase 2. Start never appears in `active` yet because it is the shell that
//! hosts the hub, not a managed sibling — phase 1 makes that explicit.

use std::collections::HashSet;

use crate::platform::ui_sound::UiSoundKind;

/// Stable app identity. Never index rows by order; these ids survive UI
/// reflows and persist into future catalog entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppId {
    Start,
    Browser,
}

/// Shell work required after a hub transition. Order matters: unfocus/hide
/// and suspend the outgoing app before showing/focusing/resuming the
/// incoming one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEffect {
    Show(AppId),
    Hide(AppId),
    SetFocused {
        id: AppId,
        focused: bool,
    },
    /// Stop the app's engine loop: no event spin, no paint/present, media
    /// paused, caches droppable.
    Suspend(AppId),
    /// Reverse of suspend: show, refocus, resume timers and frames.
    Resume(AppId),
    /// Drop all engine resources for a closed app.
    Close(AppId),
}

/// Installed apps plus foreground/suspend bookkeeping.
#[derive(Debug)]
pub struct AppHub {
    installed: HashSet<AppId>,
    active: Option<AppId>,
    suspended: HashSet<AppId>,
}

impl Default for AppHub {
    fn default() -> Self {
        Self::new()
    }
}

// Phase 1 wires install/catalog/suspend-background through the hub window;
// the pure API lands here first so phase 0 stays behavior-preserving.
#[allow(dead_code)]
impl AppHub {
    /// Start is always present (it hosts the hub). Browser is always present
    /// in phase 0 because the reader backend still works without an engine;
    /// the tile-level install affordance arrives with the engine DLC.
    pub fn new() -> Self {
        let installed = HashSet::from([AppId::Start, AppId::Browser]);
        Self {
            installed,
            active: None,
            suspended: HashSet::new(),
        }
    }

    pub fn is_installed(&self, id: AppId) -> bool {
        self.installed.contains(&id)
    }

    pub fn active(&self) -> Option<AppId> {
        self.active
    }

    pub fn is_suspended(&self, id: AppId) -> bool {
        self.suspended.contains(&id)
    }

    /// Register an app (idempotent). Used by the catalog/DLC path later.
    pub fn install(&mut self, id: AppId) {
        self.installed.insert(id);
    }

    /// Foreground an installed app. Reopening the active app is a no-op;
    /// reopening a suspended app resumes it. Unknown/uninstalled ids are
    /// ignored (no effect).
    pub fn open(&mut self, id: AppId) -> Vec<AppEffect> {
        if !self.installed.contains(&id) {
            return Vec::new();
        }
        if self.active == Some(id) {
            return Vec::new();
        }
        let mut fx = Vec::new();
        if let Some(prev) = self.active {
            fx.extend(self.suspend_one(prev));
        }
        self.active = Some(id);
        self.suspended.remove(&id);
        fx.push(AppEffect::Show(id));
        fx.push(AppEffect::SetFocused { id, focused: true });
        fx.push(AppEffect::Resume(id));
        fx
    }

    /// Close an app. Closing the active app leaves no foreground app; the
    /// caller decides whether to return to Start or the hub.
    pub fn close(&mut self, id: AppId) -> Vec<AppEffect> {
        // Closing tears down runtime state; the app stays installed and can
        // be reopened (browser suspend/resume uses this path). Unknown or
        // not-running apps are a no-op.
        let was_open = self.active == Some(id) || self.suspended.contains(&id);
        if !self.installed.contains(&id) || !was_open {
            return Vec::new();
        }
        self.suspended.remove(&id);
        let fx = vec![
            AppEffect::SetFocused { id, focused: false },
            AppEffect::Hide(id),
            AppEffect::Close(id),
        ];
        if self.active == Some(id) {
            self.active = None;
        }
        fx
    }

    /// Suspend every installed app except the active one. Idempotent backstop
    /// for paths that hide the shell without routing through `open`.
    pub fn suspend_background(&mut self) -> Vec<AppEffect> {
        let ids: Vec<AppId> = self
            .installed
            .iter()
            .copied()
            .filter(|id| self.active != Some(*id))
            .collect();
        ids.into_iter()
            .flat_map(|id| self.suspend_one(id))
            .collect()
    }

    fn suspend_one(&mut self, id: AppId) -> Vec<AppEffect> {
        if !self.suspended.insert(id) {
            return Vec::new();
        }
        vec![
            AppEffect::SetFocused { id, focused: false },
            AppEffect::Hide(id),
            AppEffect::Suspend(id),
        ]
    }
}

// ---------------------------------------------------------------------------
// Hub window lifecycle + input (phase 1)
// ---------------------------------------------------------------------------

use super::{App, Message};
use crate::browser::keys::RawKey;
use crate::ui::hub as hub_view;
use crate::ui::layout::{
    MonitorCover, focus_window, overlay_platform_specific, primary_monitor_cover,
};
use crate::ui::start::gesture::ChordReleaseTick;
use crate::ui::start::input as start_input;
use iced::{Point, Size, Task, window};
use std::time::Instant;

impl App {
    /// Toggle the PS hub from anywhere. PS is the only global shell gesture:
    /// while the hub is open it closes; otherwise it opens over whatever is
    /// running (Start, a game, or the desktop).
    pub(crate) fn toggle_hub(&mut self) -> Task<Message> {
        if self.hub_visible {
            self.close_hub()
        } else {
            self.open_hub()
        }
    }

    /// Show the topmost hub overlay. Idempotent when already visible.
    pub(crate) fn open_hub(&mut self) -> Task<Message> {
        if self.hub_visible
            && let Some(id) = self.hub_window
        {
            return focus_window(id).chain(self.sync_toast_zorder());
        }
        if self.hub_window.is_some() {
            // Close in flight; the next attempt can open once WindowClosed lands.
            crate::controller::hid::diag::diag_info("ui-diag: hub open skipped (window closing)");
            return Task::none();
        }
        let cover = primary_monitor_cover().unwrap_or(MonitorCover {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        });
        // The backdrop always covers the monitor; the card scales/centers
        // inside it. On an opaque backend the scrim blends over the opaque
        // clear color instead of the desktop.
        self.hub_stage = (cover.width, cover.height);
        self.hub_monitor_cover = Some(cover);
        self.hub_pending_show = true;
        let window_size = Size::new(cover.width, cover.height);
        let window_position = window::Position::Specific(Point::new(cover.x, cover.y));
        self.refresh_engine_status();
        self.sync_hub_engine_info();
        self.sync_hub_settings();
        self.hub_state.on_open(
            self.session.prefs.start_screen_enabled,
            self.browser.engine_status.is_ready(),
        );
        // `on_open` resets transient screens; engine info and settings are
        // persistent shell state, so re-apply them after the reset.
        self.sync_hub_engine_info();
        self.sync_hub_settings();
        self.sync_hub_controllers();
        // The opening PS press must not toggle the hub straight back closed.
        self.arm_chord_release_latch();
        let (id, open) = window::open(window::Settings {
            size: window_size,
            position: window_position,
            // Hidden until the final cover size is configured and DWM
            // transitions are disabled; showing earlier lets Windows animate
            // the create-time default size into the full-screen rect.
            visible: false,
            transparent: crate::ui::chrome::transparent_windows(),
            resizable: false,
            decorations: false,
            // Above Settings/popup; the toast is raised/composited above.
            level: window::Level::AlwaysOnTop,
            exit_on_close_request: true,
            platform_specific: overlay_platform_specific(),
            ..window::Settings::default()
        });
        self.hub_window = Some(id);
        self.hub_visible = true;
        self.session.auto_open_pending = false;
        crate::controller::hid::diag::diag_info("ui-diag: hub open");
        open.map(Message::HubOpened)
            .chain(crate::ui::layout::disable_window_transitions(id))
            .chain(Task::done(Message::WindowShowNow(id)))
            .chain(self.check_engine_manifest())
    }

    /// Hide the hub and return focus to Start when it is the app underneath.
    pub(crate) fn close_hub(&mut self) -> Task<Message> {
        self.hub_visible = false;
        self.hub_pending_show = false;
        self.hub_backdrop_fade = None;
        let Some(id) = self.hub_window.take() else {
            return self.rehost_toast_after_composite();
        };
        crate::controller::hid::diag::diag_info("ui-diag: hub close");
        let task = window::close(id).chain(self.rehost_toast_after_composite());
        // Return focus to the app underneath: Browser first (it launches on
        // top of Start or the desktop), else Start.
        if self.browser_visible
            && let Some(browser) = self.browser_window
        {
            return task.chain(focus_window(browser));
        }
        if self.start_visible
            && let Some(start) = self.start_window
        {
            return task.chain(focus_window(start));
        }
        task
    }

    pub(crate) fn on_hub_opened(&mut self, id: window::Id) -> Task<Message> {
        // The hub may have been dismissed before the renderer came up; accept
        // the open only while it is still the tracked window.
        if self.hub_window != Some(id) {
            return Task::none();
        }
        self.hub_visible = true;
        crate::controller::hid::diag::diag_info(format!("ui-diag: hub opened id={id:?}"));
        Task::none()
    }

    pub(crate) fn on_hub_window_sized(&mut self, size: iced::Size) -> Task<Message> {
        let Some(cover) = self.hub_monitor_cover else {
            return Task::none();
        };
        if (size.width - cover.width).abs() > 1.0 || (size.height - cover.height).abs() > 1.0 {
            return Task::none();
        }
        self.hub_stage = (size.width, size.height);
        Task::none()
    }

    pub(crate) fn on_hub_show_now(&mut self) -> Task<Message> {
        self.show_hub_now()
    }

    fn show_hub_now(&mut self) -> Task<Message> {
        if !self.hub_pending_show {
            return Task::none();
        }
        let Some(id) = self.hub_window else {
            return Task::none();
        };
        self.hub_pending_show = false;
        self.hub_backdrop_fade = Some(Instant::now());
        // The hub composites the toast card; keep the HWND hidden so it
        // cannot sit above the scrim as a blank transparent window.
        let hide_toast = if self.toast_message.is_some() {
            self.toast_window
                .map(crate::ui::layout::hide_toast)
                .unwrap_or_else(Task::none)
        } else {
            Task::none()
        };
        hide_toast
            .chain(crate::ui::layout::show_window(id))
            .chain(focus_window(id))
            .chain(self.sync_toast_zorder())
    }

    pub(crate) fn on_hub_message(&mut self, message: hub_view::HubMessage) -> Task<Message> {
        use hub_view::HubMessage;
        match message {
            HubMessage::MoveX(delta) => {
                if delta != 0 {
                    self.play_ui_cue(UiSoundKind::Nav);
                }
                if self.hub_state.apps_info.is_some() {
                    // Info owns the screen; movement scrolls the changelog.
                    return iced::widget::operation::scroll_by(
                        hub_view::info_scroll_id(),
                        iced::widget::operation::AbsoluteOffset {
                            x: 0.0,
                            y: 56.0 * delta as f32,
                        },
                    );
                } else if self.hub_state.settings_open {
                    self.hub_state.settings_move(delta);
                } else if self.hub_state.apps_menu.is_some() {
                    self.hub_state.apps_menu_move(delta);
                } else {
                    self.hub_state.move_across(delta);
                    return self.scroll_to_hub_controller();
                }
                Task::none()
            }
            HubMessage::MoveY(delta) => {
                if delta != 0 {
                    self.play_ui_cue(UiSoundKind::Nav);
                }
                if self.hub_state.apps_info.is_some() {
                    return iced::widget::operation::scroll_by(
                        hub_view::info_scroll_id(),
                        iced::widget::operation::AbsoluteOffset {
                            x: 0.0,
                            y: 56.0 * delta as f32,
                        },
                    );
                } else if self.hub_state.settings_open {
                    self.hub_state.settings_move(delta);
                } else if self.hub_state.apps_menu.is_some() {
                    self.hub_state.apps_menu_move(delta);
                } else {
                    self.hub_state.move_row(delta);
                    return self.scroll_to_hub_controller();
                }
                Task::none()
            }
            HubMessage::SettingsToggle => {
                self.play_ui_cue(UiSoundKind::Action);
                if self.hub_state.settings_open {
                    self.hub_state.close_settings();
                } else {
                    self.hub_state.open_settings();
                    self.sync_hub_settings();
                }
                Task::none()
            }
            HubMessage::SettingsOpen => {
                self.play_ui_cue(UiSoundKind::Action);
                self.hub_state.open_settings();
                self.sync_hub_settings();
                Task::none()
            }
            HubMessage::SettingsClose => {
                self.hub_state.close_settings();
                Task::none()
            }
            HubMessage::SettingsMove(delta) => {
                self.hub_state.settings_move(delta);
                Task::none()
            }
            HubMessage::SettingsConfirm => self.hub_settings_confirm(),
            HubMessage::SettingsRow(row) => {
                if row >= hub_view::SETTINGS_ROWS.len() || !self.hub_state.settings_visible(row) {
                    return Task::none();
                }
                self.hub_state.settings_focus = row;
                self.hub_settings_confirm()
            }
            HubMessage::Activate(index) => {
                self.play_ui_cue(UiSoundKind::Action);
                self.hub_state.select(index);
                self.hub_activate()
            }
            HubMessage::IdentifyController(index) => {
                self.hub_state.select_controller(index);
                self.hub_identify_selected()
            }
            HubMessage::IdentifySelected => self.hub_identify_selected(),
            HubMessage::PowerOff => self.hub_power_off_selected(),
            HubMessage::AppsMenuOpen(index) => {
                self.play_ui_cue(UiSoundKind::Action);
                self.hub_state.open_apps_menu(index);
                Task::none()
            }
            HubMessage::AppsMenuMove(delta) => {
                self.hub_state.apps_menu_move(delta);
                Task::none()
            }
            HubMessage::AppsMenuConfirm => self.apps_menu_confirm(),
            HubMessage::AppsMenuAction(action) => self.apps_menu_action(action),
            HubMessage::AppsMenuClose => {
                self.hub_state.close_apps_menu();
                Task::none()
            }
            HubMessage::AppInfoClose => {
                self.hub_state.close_app_info();
                Task::none()
            }
            HubMessage::Confirm => {
                if self.hub_state.apps_info.is_some() {
                    self.hub_state.close_app_info();
                    Task::none()
                } else if self.hub_state.settings_open {
                    self.hub_settings_confirm()
                } else if self.hub_state.apps_menu.is_some() {
                    self.apps_menu_confirm()
                } else if self.hub_state.focus == hub_view::HubFocus::Controllers {
                    self.hub_identify_selected()
                } else {
                    self.hub_activate()
                }
            }
            HubMessage::Close => {
                if self.hub_state.apps_info.is_some() {
                    self.hub_state.close_app_info();
                    Task::none()
                } else if self.hub_state.settings_open {
                    self.hub_state.close_settings();
                    Task::none()
                } else if self.hub_state.apps_menu.is_some() {
                    self.hub_state.close_apps_menu();
                    Task::none()
                } else if self.hub_state.installing && !self.hub_state.cancelling {
                    self.cancel_engine_install()
                } else {
                    self.close_hub()
                }
            }
        }
    }

    fn apps_menu_confirm(&mut self) -> Task<Message> {
        let Some(action) = self.hub_state.apps_menu_action() else {
            return Task::none();
        };
        self.apps_menu_action(action)
    }

    fn apps_menu_action(&mut self, action: hub_view::AppAction) -> Task<Message> {
        let Some(index) = self.hub_state.apps_menu else {
            return Task::none();
        };
        self.play_ui_cue(UiSoundKind::Action);
        match action {
            hub_view::AppAction::Install | hub_view::AppAction::Update => {
                self.hub_state.close_apps_menu();
                self.start_engine_install()
            }
            hub_view::AppAction::Cancel => {
                self.hub_state.close_apps_menu();
                self.cancel_engine_install()
            }
            hub_view::AppAction::Uninstall => {
                self.hub_state.close_apps_menu();
                self.start_engine_uninstall()
            }
            hub_view::AppAction::Info => {
                self.hub_state.show_app_info(index);
                #[cfg(feature = "cef-engine")]
                {
                    self.load_engine_info_sizes()
                }
                #[cfg(not(feature = "cef-engine"))]
                {
                    Task::none()
                }
            }
        }
    }

    /// Copy the live prefs into the hub settings screen.
    fn sync_hub_settings(&mut self) {
        self.hub_state.set_settings(
            self.session.prefs.start_screen_sounds_enabled,
            self.session.prefs.start_screen_sound_volume,
            self.session.prefs.start_screen_haptics_enabled,
            self.session.prefs.start_screen_haptics_strength,
            self.session.prefs.engine_download_limit_kbps,
            self.session.prefs.show_all_controllers,
        );
        // Store-managed builds download through the Store; the local cap is a
        // standalone-only control.
        self.hub_state.download_cap_available = !crate::platform::packaged::is_packaged();
        #[cfg(feature = "cef-engine")]
        self.browser.engine_download_limit.store(
            self.session.prefs.engine_download_limit_kbps,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    fn next_ten_percent(value: u8) -> u8 {
        if value >= 100 {
            0
        } else {
            (value / 10 + 1) * 10
        }
    }

    /// Adjust the focused hub settings row. Confirm/click both land here.
    fn hub_settings_confirm(&mut self) -> Task<Message> {
        match self.hub_state.settings_focus {
            0 => {
                let enabled = !self.session.prefs.start_screen_sounds_enabled;
                let task = self.apply_ui_sounds(enabled);
                self.sync_hub_settings();
                task
            }
            1 => {
                let volume = Self::next_ten_percent(self.session.prefs.start_screen_sound_volume);
                let task = self.apply_ui_sound_volume(volume);
                self.sync_hub_settings();
                task
            }
            2 => {
                let enabled = !self.session.prefs.start_screen_haptics_enabled;
                let task = self.apply_ui_haptics(enabled);
                self.sync_hub_settings();
                task
            }
            3 => {
                let strength =
                    Self::next_ten_percent(self.session.prefs.start_screen_haptics_strength);
                let task = self.apply_ui_haptics_strength(strength);
                self.sync_hub_settings();
                task
            }
            4 => {
                let limit = crate::persist::prefs::cycle_engine_download_limit(
                    self.session.prefs.engine_download_limit_kbps,
                );
                self.session.prefs.engine_download_limit_kbps = limit;
                self.save_prefs();
                // A live install shares this atomic; the change applies at
                // the next chunk boundary.
                #[cfg(feature = "cef-engine")]
                self.browser
                    .engine_download_limit
                    .store(limit, std::sync::atomic::Ordering::Relaxed);
                self.sync_hub_settings();
                Task::none()
            }
            5 => {
                self.session.prefs.show_all_controllers = !self.session.prefs.show_all_controllers;
                self.save_prefs();
                self.sync_hub_settings();
                self.sync_hub_controllers();
                Task::none()
            }
            _ => {
                self.hub_state.close_settings();
                Task::none()
            }
        }
    }

    /// Activate the highlighted tile. The hub closes first so the target app
    /// comes forward without a topmost cover left behind.
    fn hub_activate(&mut self) -> Task<Message> {
        match self.hub_state.selected_tile() {
            hub_view::HubTile::Start => {
                if !self.session.prefs.start_screen_enabled {
                    crate::controller::hid::diag::diag_info(
                        "ui-diag: hub Start selected but start screen disabled",
                    );
                    return Task::none();
                }
                crate::controller::hid::diag::diag_info("ui-diag: hub launch Start");
                // Start and Browser are exclusive; `open_start_screen` closes
                // the Browser before showing Start.
                self.close_hub().chain(self.open_start_screen())
            }
            hub_view::HubTile::Browser => {
                if self.browser.engine.is_none() {
                    crate::controller::hid::diag::diag_info(
                        "ui-diag: hub Browser requested but engine not installed",
                    );
                    self.hub_state.open_apps_menu(0);
                    return Task::none();
                }
                crate::controller::hid::diag::diag_info("ui-diag: hub launch Browser");
                self.close_hub().chain(self.open_browser_window())
            }
        }
    }

    /// Rebuild the hub controller strip from the live session.
    pub(crate) fn sync_hub_controllers(&mut self) {
        let rows = self.controller_rows(self.session.prefs.show_all_controllers);
        self.hub_state.set_controllers(rows);
    }

    /// Identify the focused controller card (Cross / click).
    fn hub_identify_selected(&mut self) -> Task<Message> {
        let Some(row) = self.hub_state.controller_selected() else {
            return Task::none();
        };
        if !row.connected || !row.supports_identify {
            return Task::none();
        }
        let serial = row.serial.clone();
        if self.identify(&serial) {
            self.hub_state.begin_identify_flash(&serial);
            self.play_ui_cue(crate::platform::ui_sound::UiSoundKind::Action);
        }
        Task::none()
    }

    /// Triangle hold completed on the focused Bluetooth pad.
    fn hub_power_off_selected(&mut self) -> Task<Message> {
        let Some(row) = self.hub_state.controller_selected() else {
            return Task::none();
        };
        if !row.show_power_off() {
            return Task::none();
        }
        let serial = row.serial.clone();
        self.play_ui_cue(crate::platform::ui_sound::UiSoundKind::Hold);
        self.hub_state.powering_off = Some(serial.clone());
        self.power_off(&serial);
        Task::none()
    }

    /// Keep the focused controller card inside the horizontal strip.
    fn scroll_to_hub_controller(&self) -> Task<Message> {
        if self.hub_state.focus != hub_view::HubFocus::Controllers {
            return Task::none();
        }
        iced::widget::operation::scroll_to(
            hub_view::controllers_scroll_id(),
            iced::widget::operation::AbsoluteOffset {
                x: hub_view::controller_scroll_x(self.hub_state.controller_focus, self.hub_stage.0),
                y: 0.0,
            },
        )
    }

    /// Pad tick while the hub is the modal owner. Only the chord (close) and
    /// selection/confirm are live; the app underneath gets nothing.
    pub(crate) fn on_hub_pad(&mut self, readings: &[start_input::NavReading]) -> Task<Message> {
        if readings.is_empty() {
            return Task::none();
        }
        self.sync_hub_controllers();
        self.hub_state.tick_identify_flash();
        if self.hub_chord_fires(readings) {
            crate::controller::hid::diag::diag_info("ui-diag: hub chord close");
            return self.close_hub();
        }
        let Some(reading) = crate::domain::pad::preferred_reading(readings) else {
            return Task::none();
        };
        // Cues fired by this pad tick rumble the pad that produced them. The
        // target is restored afterwards so mouse/keyboard actions stay silent
        // on the haptic side.
        let previous_haptic = self.haptic_pad_serial.take();
        self.set_haptic_pad_from_id(Some(&reading.id));
        let mut task = Task::none();
        if let Some(message) = self.hub_state.pad_tick(&reading.sample, Instant::now()) {
            let face_held = reading.sample.cross
                || reading.sample.circle
                || reading.sample.square
                || reading.sample.triangle;
            if message == hub_view::HubMessage::Close && face_held {
                // The hub consumed this press; keep the underlying app from
                // seeing the same hold as a fresh Cancel/Confirm edge. The
                // latch is time-bounded so a missed release sample cannot
                // wedge pad input.
                self.hub_face_latch_until =
                    Some(Instant::now() + std::time::Duration::from_millis(1000));
            }
            task = self.on_hub_message(message);
        }
        self.haptic_pad_serial = previous_haptic;
        task
    }

    /// Keyboard while the hub window has focus.
    pub(crate) fn on_hub_key(&mut self, raw: RawKey) -> Task<Message> {
        if !raw.pressed {
            return Task::none();
        }
        use iced::keyboard::Key;
        use iced::keyboard::key::Named;
        let message = match raw.key {
            Key::Named(Named::ArrowLeft) => Some(hub_view::HubMessage::MoveX(-1)),
            Key::Named(Named::ArrowRight) => Some(hub_view::HubMessage::MoveX(1)),
            Key::Named(Named::ArrowUp) => Some(hub_view::HubMessage::MoveY(-1)),
            Key::Named(Named::ArrowDown) => Some(hub_view::HubMessage::MoveY(1)),
            Key::Named(Named::Enter) | Key::Named(Named::Space) => {
                Some(hub_view::HubMessage::Confirm)
            }
            Key::Named(Named::Tab) => Some(hub_view::HubMessage::SettingsToggle),
            Key::Named(Named::Escape) => Some(hub_view::HubMessage::Close),
            _ => None,
        };
        match message {
            Some(message) => {
                self.note_start_activity();
                self.on_hub_message(message)
            }
            None => Task::none(),
        }
    }

    /// Shared PS chord detector for the hub. Mirrors the Start-era toggle:
    /// a rising chord fires once, and the release gate swallows the sticky
    /// hold that opened the hub.
    pub(crate) fn hub_chord_fires(&mut self, readings: &[start_input::NavReading]) -> bool {
        let required = crate::ui::start::gesture::default_gesture();
        let chord_held = super::chord_held_on_any_pad(&required, readings);
        let now = Instant::now();
        let (tick, glitch) = self.chord_release_gate.tick(chord_held, now);
        if glitch {
            crate::controller::hid::diag::diag_info("ui-diag: hub chord glitch ignored");
        }
        match tick {
            ChordReleaseTick::Blocked => {
                if chord_held {
                    self.gesture_detectors.consume_pending_match(readings);
                }
                false
            }
            ChordReleaseTick::Cleared => {
                let _ = self.gesture_detectors.update(&required, readings);
                false
            }
            ChordReleaseTick::Open => self.gesture_detectors.update(&required, readings),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_phase0_apps_are_installed() {
        let hub = AppHub::new();
        assert!(hub.is_installed(AppId::Start));
        assert!(hub.is_installed(AppId::Browser));
        assert_eq!(hub.active(), None);
    }

    #[test]
    fn first_open_shows_and_resumes() {
        let mut hub = AppHub::new();
        let fx = hub.open(AppId::Browser);
        assert_eq!(
            fx,
            vec![
                AppEffect::Show(AppId::Browser),
                AppEffect::SetFocused {
                    id: AppId::Browser,
                    focused: true
                },
                AppEffect::Resume(AppId::Browser),
            ]
        );
        assert_eq!(hub.active(), Some(AppId::Browser));
        assert!(!hub.is_suspended(AppId::Browser));
    }

    #[test]
    fn opening_another_suspends_the_first() {
        let mut hub = AppHub::new();
        let _ = hub.open(AppId::Browser);
        let fx = hub.open(AppId::Start);
        assert_eq!(
            fx,
            vec![
                AppEffect::SetFocused {
                    id: AppId::Browser,
                    focused: false
                },
                AppEffect::Hide(AppId::Browser),
                AppEffect::Suspend(AppId::Browser),
                AppEffect::Show(AppId::Start),
                AppEffect::SetFocused {
                    id: AppId::Start,
                    focused: true
                },
                AppEffect::Resume(AppId::Start),
            ]
        );
        assert!(hub.is_suspended(AppId::Browser));
        assert!(!hub.is_suspended(AppId::Start));
        assert_eq!(hub.active(), Some(AppId::Start));
    }

    #[test]
    fn reopen_active_is_noop() {
        let mut hub = AppHub::new();
        let _ = hub.open(AppId::Browser);
        assert!(hub.open(AppId::Browser).is_empty());
    }

    #[test]
    fn close_active_clears_foreground_and_emits_close() {
        let mut hub = AppHub::new();
        let _ = hub.open(AppId::Browser);
        let fx = hub.close(AppId::Browser);
        assert_eq!(
            fx,
            vec![
                AppEffect::SetFocused {
                    id: AppId::Browser,
                    focused: false
                },
                AppEffect::Hide(AppId::Browser),
                AppEffect::Close(AppId::Browser),
            ]
        );
        assert_eq!(hub.active(), None);
        assert!(hub.is_installed(AppId::Browser));
        assert!(hub.close(AppId::Browser).is_empty());
    }

    #[test]
    fn suspend_background_is_idempotent() {
        let mut hub = AppHub::new();
        let _ = hub.open(AppId::Browser);
        let fx = hub.suspend_background();
        assert_eq!(
            fx,
            vec![
                AppEffect::SetFocused {
                    id: AppId::Start,
                    focused: false
                },
                AppEffect::Hide(AppId::Start),
                AppEffect::Suspend(AppId::Start),
            ]
        );
        assert!(hub.suspend_background().is_empty());
    }
}
