//! Managed-app Home: which controller-first apps exist, which one is active,
//! and the shell effects a transition requires.
//!
//! Home owns the window lifecycle and pad/key routing for the topmost overlay;
//! this module keeps the installed/active app bookkeeping the shell applies.
//! Launcher and Browser are the managed apps; Home itself is the shell.

use std::collections::HashSet;

use crate::domain::navigation::{ActionOutcome, MoveOutcome, NavAxis, NavCue, NavSurface};
use crate::domain::pad::{PadAction, PadButton, PadButtonAction};
use crate::platform::ui_sound::UiSoundKind;

/// Stable app identity. Never index rows by order; these ids survive UI
/// reflows and persist into future catalog entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppId {
    Launcher,
    Browser,
}

/// Screen the shell can show one at a time. Home is a peer screen, never an
/// overlay on top of a managed app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellScreen {
    Home,
    Launcher,
    Browser,
}

/// Which screen currently owns the foreground. Home wins if several flags
/// briefly overlap during a close; managed apps are mutually exclusive.
pub fn visible_screen(home: bool, browser: bool, launcher: bool) -> Option<ShellScreen> {
    if home {
        Some(ShellScreen::Home)
    } else if browser {
        Some(ShellScreen::Browser)
    } else if launcher {
        Some(ShellScreen::Launcher)
    } else {
        None
    }
}

/// Backdrop alpha the shell view may paint. A hidden/closed shell always
/// paints zero; the shared timeline only matters while a screen is live.
pub(crate) fn resolve_shell_backdrop_alpha(
    hidden: bool,
    backdrop: crate::ui::motion::ShellBackdrop,
    now: std::time::Instant,
) -> f32 {
    if hidden { 0.0 } else { backdrop.alpha(now) }
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
use crate::ui::launcher::input as launcher_input;
use crate::ui::layout::{MonitorCover, focus_window, primary_monitor_cover};
use iced::{Task, window};
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

/// Home app-menu action surface: Confirm is accepted only while an action is
/// present; everything else is rejected silently.
struct HomeActionSurface {
    available: bool,
}

impl NavSurface for HomeActionSurface {
    fn try_move(&mut self, _dir: crate::domain::navigation::NavDir) -> MoveOutcome {
        MoveOutcome::Rejected
    }

    fn try_action(&mut self, action: crate::domain::pad::NavAction) -> ActionOutcome {
        if self.available && action == crate::domain::pad::NavAction::Confirm {
            ActionOutcome::Accepted {
                cue: NavCue::Action,
            }
        } else {
            ActionOutcome::Rejected
        }
    }
}

impl App {
    /// PS is a screen hide/resume toggle: dismiss the visible screen to the
    /// desktop, then restore that exact screen on the next press. Circle is
    /// the only path back to the Home screen.
    pub(crate) fn ps_screen_toggle(&mut self) -> Task<Message> {
        self.screen_toggle("ps")
    }

    /// Dismiss the visible shell screen to the desktop. PS and a backdrop
    /// click share this exact path; only the trace source differs.
    pub(crate) fn screen_toggle(&mut self, source: &'static str) -> Task<Message> {
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: PS toggle home={} browser={} launcher={} windows=({},{},{}) resume={:?} pending={}",
            self.home_visible,
            self.browser_visible,
            self.launcher_visible,
            self.home_window.is_some(),
            self.browser_window.is_some(),
            self.launcher_window.is_some(),
            self.ps_resume,
            self.ps_resume_pending,
        ));
        match visible_screen(
            self.home_visible,
            self.browser_visible,
            self.launcher_visible,
        ) {
            Some(ShellScreen::Home) => {
                if self.home_closing() {
                    return Task::none();
                }
                crate::platform::trace::event(
                    crate::platform::trace::Lane::Platform,
                    "screen.dismiss",
                )
                .field("screen", "home")
                .field("source", source)
                .emit();
                let task = self.close_home();
                // `close_home` clears resume state (Circle/auto-close
                // semantics); PS owns the restore target.
                self.ps_resume = Some(ShellScreen::Home);
                self.ps_resume_pending = false;
                self.consume_ps_release();
                task
            }
            Some(ShellScreen::Browser) => {
                crate::platform::trace::event(
                    crate::platform::trace::Lane::Platform,
                    "screen.dismiss",
                )
                .field("screen", "browser")
                .field("source", source)
                .emit();
                let task = self.close_osk("ps").chain(self.close_browser_window());
                self.ps_resume = Some(ShellScreen::Browser);
                self.ps_resume_pending = false;
                self.consume_ps_release();
                crate::controller::hid::diag::diag_info("ui-diag: PS dismiss screen=Browser");
                task
            }
            Some(ShellScreen::Launcher) => {
                crate::platform::trace::event(
                    crate::platform::trace::Lane::Platform,
                    "screen.dismiss",
                )
                .field("screen", "launcher")
                .field("source", source)
                .emit();
                let task = self.close_osk("ps").chain(self.close_launcher());
                self.ps_resume = Some(ShellScreen::Launcher);
                self.ps_resume_pending = false;
                crate::controller::hid::diag::diag_info("ui-diag: PS dismiss screen=Launcher");
                task
            }
            None => {
                // A Launcher window can still be closing (its id lives until
                // WindowClosed), so defer the resume instead of losing the
                // second press.
                if self.home_window.is_some()
                    || self.browser_window.is_some()
                    || self.launcher_window.is_some()
                {
                    self.ps_resume.get_or_insert(ShellScreen::Home);
                    self.ps_resume_pending = true;
                    crate::controller::hid::diag::diag_info(
                        "ui-diag: PS resume deferred (window closing)",
                    );
                    return Task::none();
                }
                self.ps_resume_pending = false;
                let screen = self.ps_resume.take().unwrap_or(ShellScreen::Home);
                crate::platform::trace::event(
                    crate::platform::trace::Lane::Platform,
                    "screen.resume",
                )
                .field("screen", format!("{screen:?}").to_lowercase())
                .field("source", "ps")
                .emit();
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: PS resume screen={screen:?}"
                ));
                self.open_shell_screen(screen, true)
            }
        }
    }

    /// Retry a deferred PS resume after a window finished closing.
    pub(crate) fn maybe_resume_ps_after_close(&mut self) -> Task<Message> {
        if !self.ps_resume_pending {
            return Task::none();
        }
        if self.home_window.is_some()
            || self.browser_window.is_some()
            || self.launcher_window.is_some()
        {
            return Task::none();
        }
        self.ps_resume_pending = false;
        let screen = self.ps_resume.take().unwrap_or(ShellScreen::Home);
        crate::platform::trace::event(crate::platform::trace::Lane::Platform, "screen.resume")
            .field("screen", format!("{screen:?}").to_lowercase())
            .field("source", "ps")
            .emit();
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: PS resume deferred->screen={screen:?}"
        ));
        self.open_shell_screen(screen, true)
    }

    fn open_shell_screen(&mut self, screen: ShellScreen, resuming: bool) -> Task<Message> {
        match screen {
            ShellScreen::Home => {
                if resuming {
                    self.open_home_resumed()
                } else {
                    self.open_home()
                }
            }
            ShellScreen::Launcher => {
                if !self.session.prefs.home_enabled {
                    return self.open_home();
                }
                self.open_launcher()
            }
            ShellScreen::Browser => {
                if self.browser.engine.is_none() {
                    crate::controller::hid::diag::diag_info(
                        "ui-diag: PS resume Browser skipped (engine missing)",
                    );
                    return self.open_home();
                }
                self.open_browser_window()
            }
        }
    }

    /// Circle from a managed app: close/suspend it, then show the Home screen.
    pub(crate) fn return_to_home_screen(&mut self) -> Task<Message> {
        self.ps_resume = None;
        self.ps_resume_pending = false;
        if self.home_visible {
            return Task::none();
        }
        // One commit cue for the screen change; the close paths themselves
        // stay silent. A close already in flight owns the cue.
        if (self.browser_visible || self.launcher_visible)
            && !self.browser_closing()
            && !self.launcher_closing()
        {
            self.apply_action_outcome(ActionOutcome::Accepted {
                cue: NavCue::Action,
            });
        }
        // Tell the outgoing screen's close path that another shell screen is
        // taking over, so the native cursor stays hidden across the swap.
        self.shell_opening = true;
        let close = if self.browser_visible {
            let close_osk = self.close_osk("circle_home");
            self.browser.panel.cancel_url_edit();
            // Circle from the tabs view selects the focused card for the next
            // open, even when the user never entered it.
            let persist = self.persist_grid_selection();
            close_osk.chain(persist).chain(self.close_browser_window())
        } else if self.launcher_visible {
            let close_osk = self.close_osk("circle_home");
            close_osk.chain(self.retire_launcher_for_home())
        } else {
            Task::none()
        };
        crate::platform::trace::event(crate::platform::trace::Lane::Platform, "screen.home")
            .field("source", "circle")
            .emit();
        crate::controller::hid::diag::diag_info("ui-diag: circle returns to Home");
        // State switch is immediate; cleanup runs in parallel. Chaining made
        // the incoming show wait on toast/window effects that can settle late.
        Task::batch([close, self.open_home()])
    }

    pub(crate) fn clear_ps_resume(&mut self) {
        self.ps_resume = None;
        self.ps_resume_pending = false;
    }

    /// Backdrop alpha the shell view is allowed to paint. A hidden/closed
    /// shell forces zero even if the shared timeline still holds a scrim: the
    /// warm surface must never carry a settled dim into the next reveal.
    pub(crate) fn shell_backdrop_view_alpha(&self, now: Instant) -> f32 {
        resolve_shell_backdrop_alpha(self.shell_render_hidden, self.shell_backdrop, now)
    }

    /// The shared backdrop is drawn by the one shell window.
    pub(crate) fn shell_backdrop_window(&self) -> Option<window::Id> {
        self.shell_window
    }

    /// A screen is now visible. If the backdrop is already up (a swap, or an
    /// exit still in flight) rise from the current alpha so the dim never
    /// dips or snaps; otherwise start the delayed entrance fade.
    pub(crate) fn claim_shell_backdrop(&mut self, now: Instant) {
        self.shell_backdrop.raise(now);
    }

    /// Begin fading the shared backdrop for a hide-to-desktop transition.
    pub(crate) fn begin_shell_backdrop_exit(&mut self, now: Instant) {
        self.shell_backdrop.exit(now);
    }

    pub(crate) fn open_home(&mut self) -> Task<Message> {
        self.open_home_inner(true)
    }

    /// PS resume of a dismissed Home keeps its tile/focus/settings state.
    fn open_home_resumed(&mut self) -> Task<Message> {
        self.open_home_inner(false)
    }

    fn open_home_inner(&mut self, reset: bool) -> Task<Message> {
        self.clear_ps_resume();
        if self.home_closing() {
            return Task::none();
        }
        if self.home_visible
            && let Some(id) = self.shell_window
        {
            return focus_window(id).chain(self.sync_toast_zorder());
        }
        self.shell_opening = true;
        self.reset_shell_cursor_clock();
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
        if self.launcher_visible {
            self.launcher_home_pause.get_or_insert_with(Instant::now);
        }
        self.refresh_engine_status();
        self.sync_home_engine_info();
        self.sync_home_settings();
        if reset {
            self.home_state.on_open(
                self.session.prefs.home_enabled,
                self.browser.engine_status.is_ready(),
            );
        } else {
            // PS resume: keep the tile/focus/settings the user left behind,
            // but re-arm edge latches so the opening press cannot fire twice.
            self.home_state.rearm_on_open(
                self.session.prefs.home_enabled,
                self.browser.engine_status.is_ready(),
            );
        }
        // `on_open` resets transient screens; engine info and settings are
        // persistent shell state, so re-apply them after the reset.
        self.sync_home_engine_info();
        self.sync_home_settings();
        self.sync_home_controllers();
        // The opening PS press must not toggle Home straight back closed.
        self.consume_ps_release();
        self.session.auto_open_pending = false;
        crate::controller::hid::diag::diag_info("ui-diag: home open");
        self.activate_shell_mode(ShellScreen::Home, cover)
            .chain(self.check_engine_manifest())
            .chain(close_osk)
    }

    /// Begin hiding/switching Home. The shared shell window stays alive through
    /// the exit fade; [`Self::finish_home_close`] switches mode or hides it.
    pub(crate) fn close_home(&mut self) -> Task<Message> {
        // Circle/auto-close semantics: a later PS starts at Home, not the
        // screen that was dismissed before this close.
        self.clear_ps_resume();
        let last_shell = !self.browser_visible && !self.launcher_visible && !self.shell_opening;
        if self.shell_cursor_hidden && last_shell {
            self.reset_shell_cursor_clock();
            self.set_shell_cursor_hidden(false);
        }
        if self.home_closing() {
            return Task::none();
        }
        let now = Instant::now();
        let Some(id) = self.home_window else {
            self.home_visible = false;
            self.home_shell_anim = None;
            self.resume_launcher_after_home(now);
            return self.hide_shell_window();
        };
        crate::controller::hid::diag::diag_info("ui-diag: home close");
        self.home_visible = false;
        // Only a pending in-place swap keeps the dim up. A dismiss (including
        // one during the opening transition) fades it out so hide cannot snap
        // a visible scrim to zero.
        if self.pending_shell_mode.is_none() {
            self.begin_shell_backdrop_exit(now);
        } else {
            self.shell_backdrop.raise(now);
        }
        let reduced =
            crate::ui::motion::reduced_motion_effective(self.session.prefs.reduced_motion);
        let from = self
            .home_shell_anim
            .map(|anim| anim.frame(now, reduced))
            .unwrap_or_else(crate::ui::home::ShellFrame::full);
        self.home_shell_anim = Some(crate::ui::home::ShellAnim::exit(now, from));
        crate::ui::layout::invalidate_window(id)
    }

    /// Complete a Home close. A waiting screen switches in place; otherwise
    /// the warm shell hides and the pointer returns to the desktop.
    pub(crate) fn finish_home_close(&mut self, now: Instant) -> Task<Message> {
        self.home_shell_anim = None;
        self.home_visible = false;
        self.home_window = None;
        self.resume_launcher_after_home(now);
        // A second PS while the outgoing card was fading out wins over a
        // queued Home->app swap; hide_shell_window resumes the PS target.
        if self.ps_resume_pending {
            self.pending_shell_mode = None;
            return self.hide_shell_window();
        }
        if let Some(mode) = self.take_pending_shell_mode() {
            return self.switch_shell_mode(mode);
        }
        self.hide_shell_window()
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
        if self.shell_cursor_last_active <= paused_at {
            self.shell_cursor_last_active += pause;
        }
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
                self.apply_action_outcome(ActionOutcome::Accepted {
                    cue: NavCue::Action,
                });
                Task::none()
            }
            HomeMessage::MoveX(delta) => {
                if delta == 0 {
                    return Task::none();
                }
                if self.home_state.apps_info.is_some() {
                    // Info owns the screen; movement scrolls the changelog.
                    let step = 56.0 * delta as f32;
                    let Some(offset) = self.home_state.scroll_info(step) else {
                        return Task::none();
                    };
                    self.apply_nav_cue(NavCue::Nav);
                    return iced::widget::operation::scroll_to(
                        home_view::info_scroll_id(),
                        iced::widget::operation::AbsoluteOffset { x: 0.0, y: offset },
                    );
                }
                if self.home_state.settings_open {
                    // Selector dropdowns are menu-driven; Left/Right is inert.
                    return self.home_settings_adjust(delta);
                }
                if self.home_state.apps_menu.is_some() {
                    let outcome = self.home_state.apps_menu_move(delta, NavAxis::Horizontal);
                    self.apply_move_outcome(outcome);
                    return Task::none();
                }
                // The controllers dock is a vertical surface: horizontal
                // input must not move its selection or play feedback.
                if self.home_state.focus == home_view::HomeFocus::Controllers {
                    return Task::none();
                }
                let outcome = self.home_state.move_x(delta);
                self.apply_move_outcome(outcome);
                Task::none()
            }
            HomeMessage::MoveY(delta) => {
                if delta == 0 {
                    return Task::none();
                }
                if self.home_state.apps_info.is_some() {
                    let step = 56.0 * delta as f32;
                    let Some(offset) = self.home_state.scroll_info(step) else {
                        return Task::none();
                    };
                    self.apply_nav_cue(NavCue::Nav);
                    return iced::widget::operation::scroll_to(
                        home_view::info_scroll_id(),
                        iced::widget::operation::AbsoluteOffset { x: 0.0, y: offset },
                    );
                }
                if self.home_state.settings_open {
                    let outcome = if self.home_state.settings_menu_open() {
                        self.home_state.settings_menu_move(delta)
                    } else {
                        self.home_state.settings_move(delta)
                    };
                    self.apply_move_outcome(outcome);
                    return Task::none();
                }
                if self.home_state.apps_menu.is_some() {
                    let outcome = self.home_state.apps_menu_move(delta, NavAxis::Vertical);
                    self.apply_move_outcome(outcome);
                    return Task::none();
                }
                // The apps row is horizontal: Up/Down must not play feedback.
                if self.home_state.focus == home_view::HomeFocus::Controllers {
                    // The restored dock is a vertical list: Up/Down (and the
                    // keyboard arrows) move the controller selection.
                    let outcome = self.home_state.move_y(delta);
                    self.apply_move_outcome(outcome);
                }
                Task::none()
            }
            HomeMessage::SettingsToggle => {
                self.apply_action_outcome(ActionOutcome::Accepted {
                    cue: NavCue::Action,
                });
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
                self.apply_action_outcome(ActionOutcome::Accepted {
                    cue: NavCue::Action,
                });
                self.home_state.open_apps_menu(index);
                Task::none()
            }
            HomeMessage::AppsMenuMove(delta) => {
                let outcome = self.home_state.apps_menu_move(delta, NavAxis::Vertical);
                self.apply_move_outcome(outcome);
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
                        self.apply_nav_cue(NavCue::Nav);
                    } else {
                        self.home_state.close_settings();
                        self.apply_nav_cue(NavCue::Action);
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
            HomeMessage::Backdrop => self.screen_toggle("backdrop"),
        }
    }

    fn apps_menu_confirm(&mut self) -> Task<Message> {
        let Some(action) = self.home_state.apps_menu_action() else {
            return Task::none();
        };
        let mut surface = HomeActionSurface { available: true };
        let outcome = self
            .home_state
            .dispatch_action(&mut surface, crate::domain::pad::NavAction::Confirm);
        self.apply_action_outcome(outcome);
        if !outcome.accepted() {
            return Task::none();
        }
        self.apps_menu_action(action)
    }

    fn apps_menu_action(&mut self, action: home_view::AppAction) -> Task<Message> {
        let Some(index) = self.home_state.apps_menu else {
            return Task::none();
        };
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
        self.apply_action_outcome(ActionOutcome::Accepted {
            cue: NavCue::Action,
        });
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
                self.apply_action_outcome(ActionOutcome::Accepted {
                    cue: NavCue::Action,
                });
                // Launcher and Browser are exclusive; `open_launcher` closes
                // the Browser before showing the Launcher. Keep the pointer
                // hidden across the Home->Launcher swap.
                self.shell_opening = true;
                Task::batch([self.close_home(), self.open_launcher()])
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
                self.apply_action_outcome(ActionOutcome::Accepted {
                    cue: NavCue::Action,
                });
                self.shell_opening = true;
                self.browser.start_in_grid = true;
                Task::batch([self.close_home(), self.open_browser_window()])
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
            self.apply_action_outcome(ActionOutcome::Accepted {
                cue: NavCue::Action,
            });
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
        if self.ps_released(readings) {
            crate::controller::hid::diag::diag_info("ui-diag: home chord dismiss");
            return self.ps_screen_toggle();
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
        if let Some(message) = self.home_state.pad_tick(reading, Instant::now(), tap) {
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

    /// Home hold clock: pull the Triangle power ring from wall time while the
    /// pad edge stream is quiet. No-op when the Home overlay is closed.
    pub(crate) fn on_home_frame(&mut self) -> Task<Message> {
        if !self.home_visible {
            return Task::none();
        }
        self.home_state.advance_hold(Instant::now());
        Task::none()
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

    /// Shared PS detector for Home/Browser/OSK. PS actions fire on the
    /// release edge only: a held sample can never toggle a screen.
    pub(crate) fn ps_released(&mut self, readings: &[launcher_input::NavReading]) -> bool {
        let required = crate::ui::launcher::gesture::default_gesture();
        self.ps_release.update(&required, readings)
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
    fn ps_visibility_prefers_home_then_browser_then_launcher() {
        assert_eq!(visible_screen(false, false, false), None);
        assert_eq!(
            visible_screen(false, false, true),
            Some(ShellScreen::Launcher)
        );
        assert_eq!(
            visible_screen(false, true, true),
            Some(ShellScreen::Browser)
        );
        assert_eq!(visible_screen(true, true, true), Some(ShellScreen::Home));
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
    fn hidden_shell_forces_backdrop_zero() {
        use crate::ui::motion::ShellBackdrop;
        use std::time::Duration;

        let now = Instant::now();
        assert_eq!(
            resolve_shell_backdrop_alpha(true, ShellBackdrop::Full, now),
            0.0,
            "a hidden shell must never paint a settled scrim"
        );
        assert_eq!(
            resolve_shell_backdrop_alpha(false, ShellBackdrop::Full, now),
            crate::ui::theme::BACKDROP_ALPHA
        );
        let later = now + Duration::from_millis(400);
        assert_eq!(
            resolve_shell_backdrop_alpha(true, ShellBackdrop::Entering { started: now }, later),
            0.0
        );
        assert!(
            resolve_shell_backdrop_alpha(false, ShellBackdrop::Entering { started: now }, later)
                > 0.5
        );
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
