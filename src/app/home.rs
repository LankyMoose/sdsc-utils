//! Managed-app Home: which controller-first apps exist, which one is active,
//! and the shell effects a transition requires.
//!
//! Home owns the window lifecycle and pad/key routing for the topmost overlay;
//! this module keeps the installed/active app bookkeeping the shell applies.
//! Launcher and Browser are the managed apps; Home itself is the shell.

use std::collections::HashSet;

use crate::domain::pad::{PadAction, PadButton, PadButtonAction};
use crate::platform::ui_sound::UiSoundKind;

/// Stable app identity. Never index rows by order; these ids survive UI
/// reflows and persist into future catalog entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppId {
    Launcher,
    Browser,
}

/// Shell work required after a Home transition. Order matters: unfocus/hide
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
pub struct HomeApps {
    installed: HashSet<AppId>,
    active: Option<AppId>,
    suspended: HashSet<AppId>,
}

impl Default for HomeApps {
    fn default() -> Self {
        Self::new()
    }
}

// Install/catalog/suspend-background effects flow through the Home window.
#[allow(dead_code)]
impl HomeApps {
    /// Launcher and Browser are both listed. The Browser tile owns the
    /// engine install affordance when its engine is missing.
    pub fn new() -> Self {
        let installed = HashSet::from([AppId::Launcher, AppId::Browser]);
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
    /// caller decides whether to return to Launcher or Home.
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
// Home window lifecycle + input (phase 1)
// ---------------------------------------------------------------------------

use super::{App, Message};
use crate::browser::keys::RawKey;
use crate::ui::home as home_view;
use crate::ui::launcher::gesture::ChordReleaseTick;
use crate::ui::launcher::input as launcher_input;
use crate::ui::layout::{
    MonitorCover, focus_window, overlay_platform_specific, primary_monitor_cover,
};
use iced::{Point, Size, Task, window};
use std::time::Instant;

/// Map source-side tap actions to the Home shell's gated messages.
fn home_tap_message(
    state: &home_view::State,
    action: &PadAction,
) -> Option<home_view::HomeMessage> {
    if action.action != PadButtonAction::Tap {
        return None;
    }
    let modal = state.apps_info.is_some() || state.apps_menu.is_some();
    match action.button {
        PadButton::Cross => Some(home_view::HomeMessage::Confirm),
        PadButton::Circle => Some(home_view::HomeMessage::Close),
        PadButton::Options => Some(home_view::HomeMessage::SettingsToggle),
        PadButton::Create if !state.settings_open && !modal && !state.controllers.is_empty() => {
            Some(home_view::HomeMessage::ToggleControllers)
        }
        PadButton::Square
            if state.focus == home_view::HomeFocus::Controllers
                && !state.settings_open
                && !modal =>
        {
            Some(home_view::HomeMessage::ToggleShowAll)
        }
        PadButton::Triangle
            if !state.settings_open
                && !modal
                && state.focus == home_view::HomeFocus::Apps
                && matches!(state.selected_tile(), home_view::HomeTile::Browser) =>
        {
            Some(home_view::HomeMessage::AppsMenuOpen(0))
        }
        PadButton::Create
        | PadButton::Square
        | PadButton::Triangle
        | PadButton::L1
        | PadButton::R1
        | PadButton::L3
        | PadButton::R3 => None,
    }
}

impl App {
    /// Toggle the PS Home from anywhere. PS is the only global shell gesture:
    /// while Home is open it closes; otherwise it opens over whatever is
    /// running (Launcher, a game, or the desktop).
    pub(crate) fn toggle_home(&mut self) -> Task<Message> {
        if self.home_visible {
            self.close_home()
        } else {
            self.open_home()
        }
    }

    /// Show the topmost Home overlay. Idempotent when already visible.
    pub(crate) fn open_home(&mut self) -> Task<Message> {
        if self.home_closing() {
            return Task::none();
        }
        if self.home_visible
            && let Some(id) = self.home_window
        {
            return focus_window(id).chain(self.sync_toast_zorder());
        }
        if self.home_window.is_some() {
            // Close in flight; the next attempt can open once WindowClosed lands.
            crate::controller::hid::diag::diag_info("ui-diag: home open skipped (window closing)");
            return Task::none();
        }
        let cover = primary_monitor_cover().unwrap_or(MonitorCover {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        });
        let close_osk = self.close_osk("home");
        // Home suspends the Browser; never leave a URL edit armed underneath.
        self.browser.panel.cancel_url_edit();
        // The backdrop always covers the monitor; the card scales/centers
        // inside it. On an opaque backend the scrim blends over the opaque
        // clear color instead of the desktop.
        self.home_stage = (cover.width, cover.height);
        self.home_monitor_cover = Some(cover);
        self.home_pending_show = true;
        if self.launcher_visible {
            self.launcher_home_pause.get_or_insert_with(Instant::now);
        }
        let window_size = Size::new(cover.width, cover.height);
        let window_position = window::Position::Specific(Point::new(cover.x, cover.y));
        self.refresh_engine_status();
        self.sync_home_engine_info();
        self.sync_home_settings();
        self.home_state.on_open(
            self.session.prefs.home_enabled,
            self.browser.engine_status.is_ready(),
        );
        // `on_open` resets transient screens; engine info and settings are
        // persistent shell state, so re-apply them after the reset.
        self.sync_home_engine_info();
        self.sync_home_settings();
        self.sync_home_controllers();
        // The opening PS press must not toggle Home straight back closed.
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
        self.home_window = Some(id);
        self.home_visible = true;
        self.session.auto_open_pending = false;
        crate::controller::hid::diag::diag_info("ui-diag: home open");
        open.map(Message::HomeOpened)
            .chain(crate::ui::layout::disable_window_transitions(id))
            .chain(Task::done(Message::WindowShowNow(id)))
            .chain(self.check_engine_manifest())
            .chain(close_osk)
    }

    /// Begin hiding Home. The window stays alive (and modal) through the
    /// exit fade; [`Self::finish_home_close`] tears it down when the shell
    /// animation settles. Returns focus to Launcher when it is the app underneath.
    pub(crate) fn close_home(&mut self) -> Task<Message> {
        if self
            .home_shell_anim
            .is_some_and(|anim| anim.dir == crate::ui::home::ShellDir::Exit)
        {
            return Task::none();
        }
        let now = Instant::now();
        let Some(id) = self.home_window else {
            self.home_visible = false;
            self.home_pending_show = false;
            self.home_shell_anim = None;
            self.resume_launcher_after_home(now);
            return self.rehost_toast_after_composite();
        };
        if self.home_pending_show {
            // Dismissed before the window was shown: nothing to animate.
            return self.finish_home_close(now);
        }
        crate::controller::hid::diag::diag_info("ui-diag: home close");
        let reduced =
            crate::ui::motion::reduced_motion_effective(self.session.prefs.reduced_motion);
        let from = self
            .home_shell_anim
            .map(|anim| anim.frame(now, reduced))
            .unwrap_or_else(crate::ui::home::ShellFrame::full);
        self.home_shell_anim = Some(crate::ui::home::ShellAnim::exit(now, from));
        crate::ui::layout::invalidate_window(id)
    }

    /// Complete a Home close after the exit fade. Owns window teardown, focus
    /// return, toast rehost, and thawing the Launcher animation clock.
    pub(crate) fn finish_home_close(&mut self, now: Instant) -> Task<Message> {
        self.home_shell_anim = None;
        self.home_pending_show = false;
        self.home_visible = false;
        self.resume_launcher_after_home(now);
        let Some(id) = self.home_window.take() else {
            return self.rehost_toast_after_composite();
        };
        let task = window::close(id).chain(self.rehost_toast_after_composite());
        if self.browser_visible
            && let Some(browser) = self.browser_window
        {
            return task.chain(focus_window(browser));
        }
        if self.launcher_visible
            && let Some(start) = self.launcher_window
        {
            return task.chain(focus_window(start));
        }
        task
    }

    pub(crate) fn home_closing(&self) -> bool {
        self.home_shell_anim
            .is_some_and(|anim| anim.dir == crate::ui::home::ShellDir::Exit)
    }

    /// Shift frozen Launcher animation/idle stamps across Home session so the
    /// splash art resumes where it left off instead of snapping ahead.
    pub(crate) fn resume_launcher_after_home(&mut self, now: Instant) {
        let Some(paused_at) = self.launcher_home_pause.take() else {
            return;
        };
        let pause = now.saturating_duration_since(paused_at);
        if pause.is_zero() {
            return;
        }
        self.launcher_state.shift_animation_clock(paused_at, pause);
        if self.launcher_last_activity <= paused_at {
            self.launcher_last_activity += pause;
        }
        if self.launcher_cursor_last_active <= paused_at {
            self.launcher_cursor_last_active += pause;
        }
    }

    pub(crate) fn on_home_opened(&mut self, id: window::Id) -> Task<Message> {
        // Home may have been dismissed before the renderer came up; accept
        // the open only while it is still the tracked window.
        if self.home_window != Some(id) {
            return Task::none();
        }
        self.home_visible = true;
        crate::controller::hid::diag::diag_info(format!("ui-diag: home opened id={id:?}"));
        Task::none()
    }

    pub(crate) fn on_home_window_sized(&mut self, size: iced::Size) -> Task<Message> {
        let Some(cover) = self.home_monitor_cover else {
            return Task::none();
        };
        if (size.width - cover.width).abs() > 1.0 || (size.height - cover.height).abs() > 1.0 {
            return Task::none();
        }
        self.home_stage = (size.width, size.height);
        Task::none()
    }

    pub(crate) fn on_home_show_now(&mut self) -> Task<Message> {
        self.show_home_now()
    }

    fn show_home_now(&mut self) -> Task<Message> {
        if !self.home_pending_show {
            return Task::none();
        }
        let Some(id) = self.home_window else {
            return Task::none();
        };
        self.home_pending_show = false;
        self.home_shell_anim = Some(crate::ui::home::ShellAnim::enter(Instant::now()));
        // Home composites the toast card; keep the HWND hidden so it
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

    pub(crate) fn on_home_message(&mut self, message: home_view::HomeMessage) -> Task<Message> {
        use home_view::HomeMessage;
        if self.home_closing() {
            return Task::none();
        }
        match message {
            HomeMessage::ToggleControllers => {
                let now = Instant::now();
                if self.home_state.settings_open
                    || self.home_state.apps_info.is_some()
                    || self.home_state.apps_menu.is_some()
                {
                    return Task::none();
                }
                if self.home_state.toggle_dock(now) {
                    self.play_ui_cue(UiSoundKind::Slide);
                }
                Task::none()
            }
            HomeMessage::ToggleShowAll => {
                self.session.prefs.show_all_controllers = !self.session.prefs.show_all_controllers;
                self.save_prefs();
                self.sync_home_settings();
                self.sync_home_controllers();
                self.play_ui_cue(UiSoundKind::Action);
                Task::none()
            }
            HomeMessage::MoveX(delta) => {
                if delta == 0 {
                    return Task::none();
                }
                if self.home_state.apps_info.is_some() {
                    // Info owns the screen; movement scrolls the changelog.
                    self.play_ui_cue(UiSoundKind::Nav);
                    return iced::widget::operation::scroll_by(
                        home_view::info_scroll_id(),
                        iced::widget::operation::AbsoluteOffset {
                            x: 0.0,
                            y: 56.0 * delta as f32,
                        },
                    );
                }
                if self.home_state.settings_open {
                    // Selector dropdowns are menu-driven; Left/Right is inert.
                    return self.home_settings_adjust(delta);
                }
                if self.home_state.apps_menu.is_some() {
                    self.home_state.apps_menu_move(delta);
                    self.play_ui_cue(UiSoundKind::Nav);
                    return Task::none();
                }
                // The controllers dock is a vertical surface: horizontal
                // input must not move its selection or play feedback.
                if self.home_state.focus == home_view::HomeFocus::Controllers {
                    return Task::none();
                }
                if self.home_state.move_x(delta) {
                    self.play_ui_cue(UiSoundKind::Nav);
                }
                Task::none()
            }
            HomeMessage::MoveY(delta) => {
                if delta == 0 {
                    return Task::none();
                }
                if self.home_state.apps_info.is_some() {
                    self.play_ui_cue(UiSoundKind::Nav);
                    return iced::widget::operation::scroll_by(
                        home_view::info_scroll_id(),
                        iced::widget::operation::AbsoluteOffset {
                            x: 0.0,
                            y: 56.0 * delta as f32,
                        },
                    );
                }
                if self.home_state.settings_open {
                    let moved = if self.home_state.settings_menu_open() {
                        self.home_state.settings_menu_move(delta)
                    } else {
                        self.home_state.settings_move(delta)
                    };
                    if moved {
                        self.play_ui_cue(UiSoundKind::Nav);
                    }
                    return Task::none();
                }
                if self.home_state.apps_menu.is_some() {
                    self.home_state.apps_menu_move(delta);
                    self.play_ui_cue(UiSoundKind::Nav);
                    return Task::none();
                }
                // The apps row is horizontal: Up/Down must not play feedback.
                if self.home_state.focus == home_view::HomeFocus::Controllers {
                    // The restored dock is a vertical list: Up/Down (and the
                    // keyboard arrows) move the controller selection.
                    if self.home_state.move_y(delta) {
                        self.play_ui_cue(UiSoundKind::Nav);
                    }
                }
                Task::none()
            }
            HomeMessage::SettingsToggle => {
                self.play_ui_cue(UiSoundKind::Action);
                if self.home_state.settings_open {
                    self.home_state.close_settings();
                } else {
                    self.home_state.open_settings();
                    self.sync_home_settings();
                }
                Task::none()
            }
            HomeMessage::PowerOff => self.home_power_off_selected(),
            HomeMessage::AppsMenuOpen(index) => {
                self.play_ui_cue(UiSoundKind::Action);
                self.home_state.open_apps_menu(index);
                Task::none()
            }
            HomeMessage::AppsMenuMove(delta) => {
                self.home_state.apps_menu_move(delta);
                Task::none()
            }
            HomeMessage::AppsMenuConfirm => self.apps_menu_confirm(),
            HomeMessage::AppsMenuClose => {
                self.home_state.close_apps_menu();
                Task::none()
            }
            HomeMessage::Confirm => {
                if self.home_state.apps_info.is_some() {
                    self.home_state.close_app_info();
                    Task::none()
                } else if self.home_state.settings_open {
                    if self.home_state.settings_menu_open() {
                        self.home_settings_menu_confirm()
                    } else {
                        self.home_settings_confirm()
                    }
                } else if self.home_state.apps_menu.is_some() {
                    self.apps_menu_confirm()
                } else if self.home_state.focus == home_view::HomeFocus::Controllers {
                    self.home_identify_selected()
                } else {
                    self.home_activate()
                }
            }
            HomeMessage::Close => {
                if self.home_state.apps_info.is_some() {
                    self.home_state.close_app_info();
                    Task::none()
                } else if self.home_state.settings_open {
                    if self.home_state.settings_menu_open() {
                        self.home_state.close_settings_menu();
                        self.play_ui_cue(UiSoundKind::Nav);
                    } else {
                        self.home_state.close_settings();
                    }
                    Task::none()
                } else if self.home_state.apps_menu.is_some() {
                    self.home_state.close_apps_menu();
                    Task::none()
                } else if self.home_state.collapse_dock(Instant::now()) {
                    self.play_ui_cue(UiSoundKind::Slide);
                    Task::none()
                } else if self.home_state.installing && !self.home_state.cancelling {
                    self.cancel_engine_install()
                } else {
                    self.close_home()
                }
            }
        }
    }

    fn apps_menu_confirm(&mut self) -> Task<Message> {
        let Some(action) = self.home_state.apps_menu_action() else {
            return Task::none();
        };
        self.apps_menu_action(action)
    }

    fn apps_menu_action(&mut self, action: home_view::AppAction) -> Task<Message> {
        let Some(index) = self.home_state.apps_menu else {
            return Task::none();
        };
        self.play_ui_cue(UiSoundKind::Action);
        match action {
            home_view::AppAction::Install | home_view::AppAction::Update => {
                self.home_state.close_apps_menu();
                self.start_engine_install()
            }
            home_view::AppAction::Cancel => {
                self.home_state.close_apps_menu();
                self.cancel_engine_install()
            }
            home_view::AppAction::Uninstall => {
                self.home_state.close_apps_menu();
                self.start_engine_uninstall()
            }
            home_view::AppAction::Info => {
                self.home_state.show_app_info(index);
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

    /// Copy the live prefs into Home settings screen.
    fn sync_home_settings(&mut self) {
        // Store-managed builds download through the Store; the local cap is a
        // standalone-Home policy shared by every managed-app installer.
        self.install_policy
            .set(self.session.prefs.apps_download_limit_kbps);
        self.home_state.set_settings(
            self.session.prefs.ui_sounds_enabled,
            self.session.prefs.ui_sound_volume,
            self.session.prefs.ui_haptics_enabled,
            self.session.prefs.ui_haptics_strength,
            self.install_policy.limit(),
            self.session.prefs.home_auto_open,
        );
        self.home_state.show_all_controllers = self.session.prefs.show_all_controllers;
        self.home_state.download_cap_available = !crate::platform::packaged::is_packaged();
        self.home_state.clamp_settings_focus();
    }

    fn next_ten_percent(value: u8) -> u8 {
        if value >= 100 {
            0
        } else {
            (value / 10 + 1) * 10
        }
    }

    fn step_percent(value: u8, delta: i32) -> u8 {
        (i16::from(value) + delta.signum() as i16 * 5).clamp(0, 100) as u8
    }

    /// Row-aware cue for Home settings: the apply side previews the changed
    /// modality, so the pre-cue covers only the other half. Without this a
    /// volume change plays the nav sound plus its preview (and a strength
    /// change rumbles twice).
    fn play_home_settings_cue(&self, row: usize, kind: UiSoundKind) {
        match row {
            home_view::SETTINGS_SOUNDS | home_view::SETTINGS_VOLUME => self.play_ui_haptic(kind),
            home_view::SETTINGS_HAPTICS | home_view::SETTINGS_STRENGTH => self.play_ui_sound(kind),
            _ => self.play_ui_cue(kind),
        }
    }

    /// Confirm the focused Home settings row. Selectors open their dropdown;
    /// toggles flip; sliders take a coarse step.
    fn home_settings_confirm(&mut self) -> Task<Message> {
        let row = self.home_state.settings_focus;
        match row {
            home_view::SETTINGS_AUTO_OPEN | home_view::SETTINGS_DOWNLOAD => {
                self.play_home_settings_cue(row, UiSoundKind::Action);
                self.home_state.open_settings_menu(row);
                Task::none()
            }
            home_view::SETTINGS_SOUNDS => {
                self.play_home_settings_cue(row, UiSoundKind::Action);
                let enabled = !self.session.prefs.ui_sounds_enabled;
                let task = self.apply_ui_sounds(enabled);
                self.sync_home_settings();
                task
            }
            home_view::SETTINGS_VOLUME => {
                self.play_home_settings_cue(row, UiSoundKind::Action);
                let volume = Self::next_ten_percent(self.session.prefs.ui_sound_volume);
                let task = self.apply_ui_sound_volume(volume);
                self.sync_home_settings();
                task
            }
            home_view::SETTINGS_HAPTICS => {
                self.play_home_settings_cue(row, UiSoundKind::Action);
                let enabled = !self.session.prefs.ui_haptics_enabled;
                let task = self.apply_ui_haptics(enabled);
                self.sync_home_settings();
                task
            }
            home_view::SETTINGS_STRENGTH => {
                self.play_home_settings_cue(row, UiSoundKind::Action);
                let strength = Self::next_ten_percent(self.session.prefs.ui_haptics_strength);
                let task = self.apply_ui_haptics_strength(strength);
                self.sync_home_settings();
                task
            }
            _ => Task::none(),
        }
    }

    /// Commit the highlighted option in the open Home settings dropdown.
    fn home_settings_menu_confirm(&mut self) -> Task<Message> {
        let Some(choice) = self.home_state.settings_menu_choice() else {
            self.home_state.close_settings_menu();
            return Task::none();
        };
        self.play_ui_cue(UiSoundKind::Action);
        self.home_state.close_settings_menu();
        match choice {
            home_view::HomeSettingChoice::AutoOpen(mode) => {
                let task = self.apply_home_auto_open(mode);
                self.sync_home_settings();
                task
            }
            home_view::HomeSettingChoice::DownloadLimit(limit) => {
                self.apply_install_download_limit(limit);
                Task::none()
            }
        }
    }

    /// Left/Right adjustment for the focused Home settings row. Selector
    /// rows are dropdown-driven and ignore horizontal input.
    fn home_settings_adjust(&mut self, delta: i32) -> Task<Message> {
        if delta == 0 {
            return Task::none();
        }
        let row = self.home_state.settings_focus;
        let task = match row {
            home_view::SETTINGS_SOUNDS => {
                let want_on = delta > 0;
                if self.session.prefs.ui_sounds_enabled == want_on {
                    return Task::none();
                }
                self.play_home_settings_cue(row, UiSoundKind::Nav);
                self.apply_ui_sounds(want_on)
            }
            home_view::SETTINGS_VOLUME => {
                let current = self.session.prefs.ui_sound_volume;
                let next = Self::step_percent(current, delta);
                if next == current {
                    return Task::none();
                }
                self.play_home_settings_cue(row, UiSoundKind::Nav);
                self.apply_ui_sound_volume(next)
            }
            home_view::SETTINGS_HAPTICS => {
                let want_on = delta > 0;
                if self.session.prefs.ui_haptics_enabled == want_on {
                    return Task::none();
                }
                self.play_home_settings_cue(row, UiSoundKind::Nav);
                self.apply_ui_haptics(want_on)
            }
            home_view::SETTINGS_STRENGTH => {
                let current = self.session.prefs.ui_haptics_strength;
                let next = Self::step_percent(current, delta);
                if next == current {
                    return Task::none();
                }
                self.play_home_settings_cue(row, UiSoundKind::Nav);
                self.apply_ui_haptics_strength(next)
            }
            home_view::SETTINGS_AUTO_OPEN | home_view::SETTINGS_DOWNLOAD => return Task::none(),
            _ => return Task::none(),
        };
        self.sync_home_settings();
        task
    }

    /// Update Home-owned install cap, persist it, and publish the live
    /// value to any in-flight installer (including a running engine download).
    fn apply_install_download_limit(&mut self, limit_kbps: u32) {
        self.session.prefs.apps_download_limit_kbps = limit_kbps;
        self.save_prefs();
        self.install_policy.set(limit_kbps);
        self.sync_home_settings();
    }

    /// Activate the highlighted tile. Home closes first so the target app
    /// comes forward without a topmost cover left behind.
    fn home_activate(&mut self) -> Task<Message> {
        match self.home_state.selected_tile() {
            home_view::HomeTile::Launcher => {
                if !self.session.prefs.home_enabled {
                    crate::controller::hid::diag::diag_info(
                        "ui-diag: home Launcher selected but home disabled",
                    );
                    return Task::none();
                }
                crate::controller::hid::diag::diag_info("ui-diag: home launch Launcher");
                // Launcher and Browser are exclusive; `open_launcher` closes
                // the Browser before showing the Launcher.
                self.close_home().chain(self.open_launcher())
            }
            home_view::HomeTile::Browser => {
                if self.browser.engine.is_none() {
                    crate::controller::hid::diag::diag_info(
                        "ui-diag: home Browser requested but engine not installed",
                    );
                    self.home_state.open_apps_menu(0);
                    return Task::none();
                }
                crate::controller::hid::diag::diag_info("ui-diag: home launch Browser");
                self.close_home().chain(self.open_browser_window())
            }
        }
    }

    /// Rebuild Home controller strip from the live session.
    pub(crate) fn sync_home_controllers(&mut self) {
        let rows = self.controller_rows(self.session.prefs.show_all_controllers);
        self.home_state.set_controllers(rows);
    }

    /// Identify the focused controller card (Cross).
    fn home_identify_selected(&mut self) -> Task<Message> {
        let Some(row) = self.home_state.controller_selected() else {
            return Task::none();
        };
        if !row.connected || !row.supports_identify {
            return Task::none();
        }
        let serial = row.serial.clone();
        if self.identify(&serial) {
            self.home_state.begin_identify_flash(&serial);
            self.play_ui_cue(crate::platform::ui_sound::UiSoundKind::Action);
        }
        Task::none()
    }

    /// Triangle hold completed on the focused Bluetooth pad.
    fn home_power_off_selected(&mut self) -> Task<Message> {
        let Some(row) = self.home_state.controller_selected() else {
            return Task::none();
        };
        if !row.show_power_off() {
            return Task::none();
        }
        let serial = row.serial.clone();
        self.play_ui_cue(crate::platform::ui_sound::UiSoundKind::Hold);
        self.home_state.powering_off = Some(serial.clone());
        self.power_off(&serial);
        Task::none()
    }

    /// Pad tick while Home is the modal owner. Only the chord (close) and
    /// selection/confirm are live; the app underneath gets nothing.
    pub(crate) fn on_home_pad(
        &mut self,
        readings: &[launcher_input::NavReading],
        tap_actions: &[PadAction],
    ) -> Task<Message> {
        if self.home_closing() || readings.is_empty() {
            return Task::none();
        }
        self.sync_home_controllers();
        self.home_state.tick_identify_flash();
        if self.home_chord_fires(readings) {
            crate::controller::hid::diag::diag_info("ui-diag: home chord close");
            return self.close_home();
        }
        let Some(reading) = crate::domain::pad::preferred_reading(readings) else {
            return Task::none();
        };
        // Cues fired by this pad tick rumble the pad that produced them. The
        // target is restored afterwards so non-pad actions stay silent on the
        // haptic side.
        let previous_haptic = self.haptic_pad_serial.take();
        self.set_haptic_pad_from_id(Some(&reading.id));
        let mut task = Task::none();
        let tap = tap_actions
            .iter()
            .find_map(|action| home_tap_message(&self.home_state, action));
        if let Some(message) = self
            .home_state
            .pad_tick(&reading.sample, Instant::now(), tap)
        {
            let face_held = reading.sample.cross
                || reading.sample.circle
                || reading.sample.square
                || reading.sample.triangle;
            if message == home_view::HomeMessage::Close && face_held {
                // Home consumed this press; keep the underlying app from
                // seeing the same hold as a fresh Cancel/Confirm edge. The
                // latch is time-bounded so a missed release sample cannot
                // wedge pad input.
                self.home_face_latch_until =
                    Some(Instant::now() + std::time::Duration::from_millis(1000));
            }
            task = self.on_home_message(message);
        }
        self.haptic_pad_serial = previous_haptic;
        task
    }

    /// Keyboard while Home window has focus.
    pub(crate) fn on_home_key(&mut self, raw: RawKey) -> Task<Message> {
        if !raw.pressed || self.home_closing() {
            return Task::none();
        }
        use iced::keyboard::Key;
        use iced::keyboard::key::Named;
        let message = match raw.key {
            Key::Named(Named::ArrowLeft) => Some(home_view::HomeMessage::MoveX(-1)),
            Key::Named(Named::ArrowRight) => Some(home_view::HomeMessage::MoveX(1)),
            Key::Named(Named::ArrowUp) => Some(home_view::HomeMessage::MoveY(-1)),
            Key::Named(Named::ArrowDown) => Some(home_view::HomeMessage::MoveY(1)),
            Key::Named(Named::Enter) | Key::Named(Named::Space) => {
                Some(home_view::HomeMessage::Confirm)
            }
            Key::Named(Named::Tab) => Some(home_view::HomeMessage::SettingsToggle),
            Key::Named(Named::Escape) => Some(home_view::HomeMessage::Close),
            _ => None,
        };
        match message {
            Some(message) => {
                self.note_launcher_activity();
                self.on_home_message(message)
            }
            None => Task::none(),
        }
    }

    /// Shared PS chord detector for Home. Mirrors the Launcher-era toggle:
    /// a rising chord fires once, and the release gate swallows the sticky
    /// hold that opened Home.
    pub(crate) fn home_chord_fires(&mut self, readings: &[launcher_input::NavReading]) -> bool {
        let required = crate::ui::launcher::gesture::default_gesture();
        let chord_held = super::chord_held_on_any_pad(&required, readings);
        let now = Instant::now();
        let (tick, glitch) = self.chord_release_gate.tick(chord_held, now);
        if glitch {
            crate::controller::hid::diag::diag_info("ui-diag: home chord glitch ignored");
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
    use crate::domain::pad::PadId;

    fn tap(button: PadButton) -> PadAction {
        PadAction {
            pad: PadId("hid:a".into()),
            button,
            action: PadButtonAction::Tap,
        }
    }

    #[test]
    fn home_taps_map_to_gated_messages() {
        let mut state = home_view::State::default();
        assert_eq!(
            home_tap_message(&state, &tap(PadButton::Cross)),
            Some(home_view::HomeMessage::Confirm)
        );
        assert_eq!(
            home_tap_message(&state, &tap(PadButton::Circle)),
            Some(home_view::HomeMessage::Close)
        );
        assert_eq!(
            home_tap_message(&state, &tap(PadButton::Options)),
            Some(home_view::HomeMessage::SettingsToggle)
        );
        assert_eq!(home_tap_message(&state, &tap(PadButton::L3)), None);
        assert_eq!(home_tap_message(&state, &tap(PadButton::Triangle)), None);
        state.select(1, Instant::now());
        assert_eq!(
            home_tap_message(&state, &tap(PadButton::Triangle)),
            Some(home_view::HomeMessage::AppsMenuOpen(0))
        );
        state.settings_open = true;
        assert_eq!(home_tap_message(&state, &tap(PadButton::Triangle)), None);
    }

    #[test]
    fn both_phase0_apps_are_installed() {
        let apps = HomeApps::new();
        assert!(apps.is_installed(AppId::Launcher));
        assert!(apps.is_installed(AppId::Browser));
        assert_eq!(apps.active(), None);
    }

    #[test]
    fn first_open_shows_and_resumes() {
        let mut apps = HomeApps::new();
        let fx = apps.open(AppId::Browser);
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
        assert_eq!(apps.active(), Some(AppId::Browser));
        assert!(!apps.is_suspended(AppId::Browser));
    }

    #[test]
    fn opening_another_suspends_the_first() {
        let mut apps = HomeApps::new();
        let _ = apps.open(AppId::Browser);
        let fx = apps.open(AppId::Launcher);
        assert_eq!(
            fx,
            vec![
                AppEffect::SetFocused {
                    id: AppId::Browser,
                    focused: false
                },
                AppEffect::Hide(AppId::Browser),
                AppEffect::Suspend(AppId::Browser),
                AppEffect::Show(AppId::Launcher),
                AppEffect::SetFocused {
                    id: AppId::Launcher,
                    focused: true
                },
                AppEffect::Resume(AppId::Launcher),
            ]
        );
        assert!(apps.is_suspended(AppId::Browser));
        assert!(!apps.is_suspended(AppId::Launcher));
        assert_eq!(apps.active(), Some(AppId::Launcher));
    }

    #[test]
    fn reopen_active_is_noop() {
        let mut apps = HomeApps::new();
        let _ = apps.open(AppId::Browser);
        assert!(apps.open(AppId::Browser).is_empty());
    }

    #[test]
    fn close_active_clears_foreground_and_emits_close() {
        let mut apps = HomeApps::new();
        let _ = apps.open(AppId::Browser);
        let fx = apps.close(AppId::Browser);
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
        assert_eq!(apps.active(), None);
        assert!(apps.is_installed(AppId::Browser));
        assert!(apps.close(AppId::Browser).is_empty());
    }

    #[test]
    fn suspend_background_is_idempotent() {
        let mut apps = HomeApps::new();
        let _ = apps.open(AppId::Browser);
        let fx = apps.suspend_background();
        assert_eq!(
            fx,
            vec![
                AppEffect::SetFocused {
                    id: AppId::Launcher,
                    focused: false
                },
                AppEffect::Hide(AppId::Launcher),
                AppEffect::Suspend(AppId::Launcher),
            ]
        );
        assert!(apps.suspend_background().is_empty());
    }
}
