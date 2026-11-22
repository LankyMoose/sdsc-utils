//! Start screen core reducer: message dispatch + selection scroll reveals.
//!
//! Behavior-preserving extraction from `app::App`; no logic change.

#[cfg(windows)]
use super::Win32DialogParent;
#[cfg(debug_assertions)]
use super::stamp_hitch_report;
use super::{
    App, Message, PAD_POLL_SLOW_MS, PAD_POLL_STALL_MS, RELOAD_FEEDBACK_DELAY,
    RUNNING_CHECK_INTERVAL, SCROLL_RESTORE_AUTHORITY, SNAPSHOT_STALE_MS, START_CURSOR_IDLE_HIDE,
    START_NAV_NOT_READY_MS, StartKeyAction, chord_held_on_any_pad, delay, is_manual_target_missing,
    spawn_blocking, steam_appid_from_target,
};
use crate::AppResult;
use crate::browser::cursor::{JumpDir, VIEWPORT_H, VIEWPORT_W};
use crate::browser::engine::{EngineCommand, EngineEvent};
use crate::browser::keys::RawKey;
use crate::games::process_match::RunningSession;
use crate::games::steam::SteamGame;
use crate::games::{self, GamesCatalog, launch, process_match, steam};
use crate::persist::prefs::{
    NavbarVisibility, ReducedMotion, StartAutoOpen, StartSelectionMode,
    clamp_start_screen_idle_timeouts, clamp_start_screen_inactive_dim_percent,
    renderer_restart_needed,
};
use crate::platform::app_log;
use crate::platform::ui_sound::UiSoundKind;
use crate::ui::configure::{PadInputPanel, Section};
use crate::ui::layout::{
    MonitorCover, cursor_on_primary_monitor, focus_window, open_visible, overlay_platform_specific,
    primary_monitor_cover, window_hwnd,
};
use crate::ui::start::gesture::{self, ChordReleaseTick};
use crate::ui::start::input::{self as start_input, FaceHeld, NavAction, NavLogSnapshot};
use crate::ui::start::mode::{self as start_mode};
use crate::ui::start::view::{self as start_view, ReplaceConfirm, StartMessage};
use iced::widget::operation;
use iced::{Task, window};
use std::path::PathBuf;
use std::time::{Duration, Instant};

impl App {
    /// All Start selection changes funnel through here (mouse, keyboard, pad).
    pub(crate) fn on_start_message(&mut self, message: StartMessage) -> Task<Message> {
        self.on_start_message_inner(message)
    }

    pub(crate) fn on_start_message_inner(&mut self, message: StartMessage) -> Task<Message> {
        // Hub is modal: the app underneath takes no input while it is open.
        if self.hub_visible {
            return Task::none();
        }
        // Cold ambient: only Close is live (escape hatch); ignore nav/launch/edit.
        if self.start_state.cold_load_active && !start_cold_allows(&message) {
            return Task::none();
        }
        // Layout transition reflows main content: drop navigational messages for its
        // duration (pad Up/Down already gate on animating(); this also covers
        // keyboard dispatch and pad slide-switch). Confirm/Cancel/Close and
        // settings-toggle stay live (cancellable chrome), as do deliberate
        // settings commits (open-menu Confirm picks, mouse Set* writes) —
        // only focus moves and slider nudges freeze mid-transition.
        if self.start_state.layout_transition_active() && start_focus_move_frozen(&message) {
            return Task::none();
        }
        // Keyboard / UI actions other than pure scroll cancel an in-progress hold.
        // History-selector messages are excluded: the Open message fires
        // mid-press (resetting the bumper hold would split one physical
        // press into two and forge a tap on release), and Pick/Cancel
        // resolve the open selector (the hold's release carries no event).
        match &message {
            StartMessage::SettingsScrolled(..)
            | StartMessage::ManualAddTitle(_)
            | StartMessage::ManualAddArgs(_) => {}
            #[cfg(debug_assertions)]
            StartMessage::ReportLightbarFailure | StartMessage::ReportInputFailure => {}
            _ => self.cancel_start_holds(),
        }
        match message {
            StartMessage::Launch(index) => self.on_start_launch(index),
            StartMessage::SelectGame(index) => self.on_start_select_game(index),
            StartMessage::MoveUp => self.on_start_move(-1),
            StartMessage::MoveDown => self.on_start_move(1),
            StartMessage::Confirm => self.on_start_confirm(),
            StartMessage::Close => self.on_start_close(),
            StartMessage::Noop => Task::none(),
            StartMessage::DismissEditConfirm => {
                self.play_ui_cue(UiSoundKind::Action);
                self.start_state.dismiss_edit_confirm(Instant::now());
                Task::none()
            }
            StartMessage::ToggleEdit => match self.toggle_start_edit() {
                Some(task) => {
                    self.play_ui_cue(UiSoundKind::Action);
                    task
                }
                None => Task::none(),
            },
            StartMessage::CycleSort => match self.on_start_cycle_sort() {
                Some(task) => {
                    self.play_ui_cue(UiSoundKind::Action);
                    task
                }
                None => Task::none(),
            },
            StartMessage::AddShortcut => {
                if self.start_state.editing && !self.start_state.overlay_blocking() {
                    self.play_ui_cue(UiSoundKind::Action);
                    self.pick_manual_shortcut()
                } else {
                    Task::none()
                }
            }
            StartMessage::EditManual => {
                let before = self.start_state.manual_add.is_some();
                let task = self.begin_manual_edit();
                if !before && self.start_state.manual_add.is_some() {
                    self.play_ui_cue(UiSoundKind::Action);
                }
                task
            }
            StartMessage::SettingsScrolled(y, viewport_h) => {
                self.start_state.record_list_scroll(
                    start_view::StartListId::Settings,
                    y,
                    viewport_h,
                );
                Task::none()
            }
            StartMessage::ManualAddTitle(title) => {
                if let Some(draft) = self.start_state.manual_add.as_mut() {
                    draft.title = title;
                }
                Task::none()
            }
            StartMessage::ManualAddArgs(args) => {
                if let Some(draft) = self.start_state.manual_add.as_mut() {
                    draft.args = args;
                }
                Task::none()
            }
            StartMessage::ManualPickIcon => {
                self.play_ui_cue(UiSoundKind::Action);
                self.pick_manual_icon()
            }
            StartMessage::ManualClearIcon => {
                if let Some(draft) = self.start_state.manual_add.as_mut() {
                    draft.icon_path = None;
                    self.play_ui_cue(UiSoundKind::Action);
                }
                Task::none()
            }
            StartMessage::ToggleSettings => {
                let now = Instant::now();
                let open = !self.start_state.settings.open;
                if self.start_state.request_settings(open, now) {
                    self.play_ui_cue(UiSoundKind::Action);
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: start settings {}",
                        if open { "open" } else { "close" }
                    ));
                    if open {
                        return self
                            .scroll_settings_focus_into_view(start_view::ScrollReveal::Either);
                    }
                }
                Task::none()
            }
            StartMessage::SetAutoOpen(mode) => self.apply_start_screen_auto_open(mode),
            StartMessage::SetSelectionPersist(enabled) => {
                self.apply_start_selection_persist(enabled)
            }
            StartMessage::SetSelectionMode(mode) => self.apply_start_selection_mode(mode),
            StartMessage::ToggleSelectionModeMenu => self
                .toggle_start_selector_menu(crate::ui::start::settings::SettingsRow::SelectionMode),
            StartMessage::ToggleAutoOpenMenu => {
                self.toggle_start_selector_menu(crate::ui::start::settings::SettingsRow::AutoOpen)
            }
            StartMessage::ToggleLayoutMenu => self.toggle_start_selector_menu(
                crate::ui::start::settings::SettingsRow::ImmersiveLayout,
            ),
            StartMessage::ToggleNavbarMenu => {
                self.toggle_start_selector_menu(crate::ui::start::settings::SettingsRow::Navbar)
            }
            StartMessage::ToggleReducedMotionMenu => self
                .toggle_start_selector_menu(crate::ui::start::settings::SettingsRow::ReducedMotion),
            StartMessage::SetClock(enabled) => self.apply_start_screen_clock(enabled),
            StartMessage::SetImmersiveLayout(layout) => {
                self.apply_start_screen_immersive_layout(layout)
            }
            StartMessage::SetNavbarVisibility(navbar) => self.apply_start_screen_navbar(navbar),
            StartMessage::SetReducedMotion(mode) => self.apply_start_screen_reduced_motion(mode),
            StartMessage::SetInactiveSecs(secs) => self.apply_start_screen_inactive_secs(secs),
            StartMessage::SetSleepSecs(secs) => self.apply_start_screen_sleep_secs(secs),
            StartMessage::SetInactiveDimPercent(percent) => {
                self.apply_start_screen_inactive_dim_percent(percent)
            }
            StartMessage::SetSounds(enabled) => self.apply_ui_sounds(enabled),
            StartMessage::SetSoundVolume(volume) => self.apply_ui_sound_volume(volume),
            StartMessage::SetHaptics(enabled) => self.apply_ui_haptics(enabled),
            StartMessage::SetHapticsStrength(strength) => self.apply_ui_haptics_strength(strength),
            StartMessage::SetPerfThrottle(enabled) => self.apply_perf_throttle_immersive(enabled),
            StartMessage::SetShowUninstalledGames(enabled) => {
                self.apply_show_uninstalled_games(enabled)
            }
            StartMessage::SyncSteamLibrary => self.sync_steam_library_now(),
            StartMessage::SetPerfAntialiasing(enabled) => self.apply_perf_antialiasing(enabled),
            StartMessage::SetPerfVsync(enabled) => self.apply_perf_vsync(enabled),
            StartMessage::ReloadUi => self.reload_ui_for_renderer(),
            #[cfg(debug_assertions)]
            StartMessage::OpenAmbientTuning => self.open_ambient_tuning(),
            #[cfg(debug_assertions)]
            StartMessage::CloseAmbientTuning => self.close_ambient_tuning(true),
            #[cfg(debug_assertions)]
            StartMessage::SetAmbientTune(param, value) => self.apply_ambient_tune(param, value),
            #[cfg(debug_assertions)]
            StartMessage::ResetAmbientTuning => {
                self.reset_debug_tuning();
                Task::none()
            }
            #[cfg(debug_assertions)]
            StartMessage::AmbientTuneScrolled(y, h) => {
                self.start_state.sync_ambient_tune_scroll(y, h);
                Task::none()
            }
            #[cfg(debug_assertions)]
            StartMessage::ReportLightbarFailure => {
                self.mark_hitch_now("lightbar");
                Task::none()
            }
            #[cfg(debug_assertions)]
            StartMessage::ReportInputFailure => {
                self.mark_hitch_now("input");
                Task::none()
            }
        }
    }

    /// Launch arm: neighbor click selects only; center/Cross launches.
    /// Editing retargets the click at the checklist toggle instead.
    pub(crate) fn on_start_launch(&mut self, index: usize) -> Task<Message> {
        if index < self.start_state.rows.len() && index != self.start_state.game_selected {
            // Neighbor capsule click selects only (center / Cross launches).
            return self.on_start_message(StartMessage::SelectGame(index));
        }
        if index < self.start_state.rows.len() {
            self.start_state.game_selected = index;
        }
        if self.start_state.editing {
            match self.edit_toggle_selected() {
                Some(task) => {
                    self.play_ui_cue(UiSoundKind::Action);
                    task
                }
                None => Task::none(),
            }
        } else if self.launch_selected() {
            self.play_ui_cue(UiSoundKind::Action);
            // A freshly-armed replace confirm stays open on its modal: the
            // close preserves it now, so this guard only skips the close
            // while the modal owns the screen — the game already launched.
            if self.start_state.replace_confirm.is_none() {
                self.close_start_screen()
            } else {
                Task::none()
            }
        } else {
            Task::none()
        }
    }

    pub(crate) fn on_start_select_game(&mut self, index: usize) -> Task<Message> {
        if index >= self.start_state.rows.len() || index == self.start_state.game_selected {
            return Task::none();
        }
        let prev_game = self.start_state.game_selected;
        self.start_state.game_selected = index;
        self.start_state
            .begin_strip_anim(prev_game as f32, Instant::now());
        self.start_state.reset_backdrop_fade();
        let task = self.warm_start_art_task();
        self.play_ui_cue(UiSoundKind::Nav);
        task
    }

    /// Focus-move arm: `delta` is -1 (up) or +1 (down).
    pub(crate) fn on_start_move(&mut self, delta: i32) -> Task<Message> {
        let prev_game = self.start_state.game_selected;
        let direction = self.start_state.move_selection(delta);
        let mut task = Task::none();
        if direction.is_some() {
            self.play_ui_cue(UiSoundKind::Nav);
            if self.start_state.game_selected != prev_game {
                self.start_state
                    .begin_strip_anim(prev_game as f32, Instant::now());
                self.start_state.reset_backdrop_fade();
                task = task.chain(self.warm_start_art_task());
            }
        }
        task
    }

    /// Close/Cancel arm: unwinds topmost surface first (settings → manual-add
    /// draft → replace confirm → save guard → edit mode → Start itself).
    pub(crate) fn on_start_close(&mut self) -> Task<Message> {
        if self.start_state.settings.open {
            self.play_ui_cue(UiSoundKind::Action);
            let now = Instant::now();
            if self.start_state.request_settings(false, now) {
                crate::controller::hid::diag::diag_info("ui-diag: start settings close");
            }
            Task::none()
        } else if self.start_state.manual_add.is_some() {
            self.play_ui_cue(UiSoundKind::Action);
            self.cancel_manual_add()
        } else if self.start_state.replace_confirm.is_some() {
            // Cancel unwinds through the exit tween (content clears when it
            // settles); reduced motion clears instantly.
            self.play_ui_cue(UiSoundKind::Action);
            self.start_state.dismiss_replace(Instant::now());
            Task::none()
        } else if self.start_state.edit_confirm.is_some() {
            // Second Cancel backs out of the question: draft kept, stay in
            // edit mode (unwinds through the exit tween like any dismiss).
            self.play_ui_cue(UiSoundKind::Action);
            self.start_state.dismiss_edit_confirm(Instant::now());
            Task::none()
        } else if self.start_state.editing {
            self.play_ui_cue(UiSoundKind::Action);
            if self.edit_draft_dirty() {
                // Dirty exit asks first; clean exits cancel silently as before.
                self.start_state.arm_edit_confirm(Instant::now());
                Task::none()
            } else {
                self.cancel_start_edit()
            }
        } else if self.start_visible {
            self.play_ui_cue(UiSoundKind::Action);
            self.close_start_screen()
        } else {
            Task::none()
        }
    }

    /// Browser toggle: foreground the browser app through the multi-app
    /// manager and open the surface; toggling again suspends (close cancels
    /// in-flight work but keeps the cached page for an instant resume).
    /// Match the browser surface and engine viewport to the current 16:9
    /// card layout. No-op when the computed page size is unchanged.
    pub(crate) fn sync_browser_viewport(&mut self) {
        let (sw, sh) = self
            .browser_window_size
            .or_else(|| self.browser_monitor_cover.map(|c| (c.width, c.height)))
            .unwrap_or((1920.0, 1080.0));
        let (page_w, page_h) = crate::ui::browser::view::fullscreen_layout(sw, sh);
        if !self.browser.panel.set_viewport(page_w, page_h) {
            return;
        }
        if let Some(engine) = &self.browser.engine {
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: browser viewport {page_w:.0}x{page_h:.0}"
            ));
            engine.send(EngineCommand::Resize {
                width: page_w.round().max(1.0) as u32,
                height: page_h.round().max(1.0) as u32,
            });
        }
    }

    pub(crate) fn open_browser_session(&mut self) -> Task<Message> {
        self.note_start_activity();
        self.play_ui_cue(UiSoundKind::Action);
        let _ = self.hub.open(crate::app::hub::AppId::Browser);
        self.browser.panel.session.open_surface();
        // The card's 16:9 page area drives the engine viewport; sync before
        // the first frame so the cursor space and image are the same size.
        self.sync_browser_viewport();
        // First fresh surface: restore the persisted strip when one exists,
        // else start at home instead of the empty state. The engine already
        // preloaded the saved active URL at boot, so the session URL is never
        // empty here; a one-shot flag distinguishes the first open from a
        // reopen of the cached page.
        if !self.browser.tabs_restored {
            self.browser.tabs_restored = true;
            let saved = crate::browser::tabs::load();
            if !saved.tabs.is_empty() {
                let task = self.restore_browser_tabs(saved);
                self.resume_browser_engine();
                crate::controller::hid::diag::diag_info("ui-diag: browser session open (restore)");
                return task;
            }
            self.browser.panel.draft = crate::browser::BROWSER_HOME_URL.to_string();
            let task = self.on_browser_navigate();
            self.resume_browser_engine();
            crate::controller::hid::diag::diag_info("ui-diag: browser session open (home)");
            return task;
        }
        self.resume_browser_engine();
        crate::controller::hid::diag::diag_info("ui-diag: browser session open");
        Task::none()
    }

    /// Browser home page. Hardcoded until the settings screen lands, then a
    /// pref. Reuses the navigate path (fresh-page cursor/focusable reset,
    /// theme sync) with the draft pinned to home.
    pub(crate) fn on_browser_go_home(&mut self) -> Task<Message> {
        self.browser.panel.draft = crate::browser::BROWSER_HOME_URL.to_string();
        self.on_browser_navigate()
    }

    /// Active tab id for engine commands. The backend gates input to the
    /// active tab; navigation/history apply per tab.
    pub(crate) fn browser_tab(&self) -> crate::browser::engine::TabId {
        self.browser.panel.active_tab
    }

    /// Re-sync the pad cursor as the engine mouse for the live tab (after
    /// resume, tab switch, or popup adopt — hover state went stale).
    pub(crate) fn sync_live_mouse(&mut self) {
        let tab = self.browser.panel.active_tab;
        // Clamp out of the header zone: the engine's viewport starts at
        // y=0 and has no use for negative points.
        let (qx, qy) = (
            self.browser.panel.cursor.x as i32,
            self.browser.panel.cursor.y.max(0.0) as i32,
        );
        self.browser.panel.last_sent_mouse = Some((qx, qy));
        if let Some(engine) = &self.browser.engine {
            engine.send(EngineCommand::MouseMove {
                tab,
                x: qx as f32,
                y: qy as f32,
            });
        }
    }

    /// Resume a suspended engine under an open browser surface. Close
    /// suspends but keeps the cached page, so reopening Start (or the
    /// surface) resumes where it left off: un-suspend, re-show, and re-sync
    /// the mouse, since hover state went stale while suspended. No-op
    /// unless the surface is open.
    pub(crate) fn resume_browser_engine(&mut self) {
        if !self.browser.panel.session.is_open() {
            return;
        }
        if let Some(engine) = &self.browser.engine {
            engine.send(EngineCommand::Resume);
            engine.send(EngineCommand::SetVisible { visible: true });
        }
        // Re-sync the mouse after suspension: hover state is stale.
        self.sync_live_mouse();
        self.sync_browser_theme();
    }

    pub(crate) fn close_browser_surface(&mut self) -> Task<Message> {
        let _ = self.hub.close(crate::app::hub::AppId::Browser);
        self.browser.panel.close_surface();
        // A held D-pad must not jump the cursor of the next opened page.
        self.browser.dpad_stepper.reset();
        if let Some(engine) = &self.browser.engine {
            engine.send(EngineCommand::SetVisible { visible: false });
            engine.send(EngineCommand::Suspend);
        }
        crate::controller::hid::diag::diag_info("ui-diag: browser session close (suspend)");
        Task::none()
    }

    /// Send a load to the CEF worker (frames arrive via
    /// [`Message::BrowserEngine`]). Generation-guarded so a suspend/close
    /// before completion is ignored.
    /// Push the OS light/dark mode to the engine (`prefers-color-scheme`).
    /// No-op without an engine or when the OS gives no answer.
    pub(crate) fn sync_browser_theme(&mut self) {
        let Some(engine) = &self.browser.engine else {
            return;
        };
        if let Some(dark) = crate::platform::appearance::system_dark_mode() {
            engine.send(EngineCommand::SetDarkMode { dark });
        }
    }

    pub(crate) fn on_browser_navigate(&mut self) -> Task<Message> {
        let draft = self.browser.panel.draft.clone();
        let current = self.browser.panel.session.url().to_string();
        let raw = if draft.trim().is_empty() {
            current
        } else {
            draft
        };
        let url = crate::browser::nav::resolve_navigate_target(&raw);
        if url.is_empty() {
            return Task::none();
        }
        self.browser.panel.draft = url.clone();
        self.browser.panel.scroll_y = 0.0;
        self.browser.panel.url_editing = false;
        self.browser.panel.bookmark_bubble = false;
        self.browser.panel.history_selector = None;
        // Fresh page: cursor back to center; focusables arrive with the
        // accessibility tree. Recenter the existing cursor so the live
        // viewport bounds (not the 800x600 defaults) survive navigation.
        self.browser.panel.cursor.reset_for_page();
        self.browser.panel.focusables.clear();
        let generation = self.browser.panel.session.begin_load(url.clone());
        self.note_start_activity();
        self.play_ui_cue(UiSoundKind::Nav);
        crate::controller::hid::diag::diag_info("ui-diag: browser navigate");
        let Some(engine) = &self.browser.engine else {
            // The engine is mandatory in phase 4; the hub tile handles the
            // missing-engine install flow before a window ever opens.
            self.browser
                .panel
                .session
                .apply_failed(generation, "Browser engine not installed".to_string());
            return Task::none();
        };
        let tab = self.browser_tab();
        engine.send(EngineCommand::Navigate {
            tab,
            url,
            generation,
        });
        self.sync_browser_theme();
        Task::none()
    }

    /// Raw keyboard routing for Start. Controller-first: with the engine
    /// page focused, keys go to the page (typing, shortcuts, focus nav);
    /// URL-edit mode and the reader fall back to the legacy Start-key map
    /// (arrows navigate, Enter confirms, Esc cancels).
    pub(crate) fn on_browser_raw_key(&mut self, id: window::Id, raw: RawKey) -> Task<Message> {
        self.haptic_pad_serial = None;
        if Some(id) == self.hub_window && self.hub_visible {
            return self.on_hub_key(raw);
        }
        if Some(id) == self.browser_window && self.browser_visible {
            return self.on_browser_key(raw);
        }
        if Some(id) != self.start_window || !self.start_visible {
            return Task::none();
        }
        self.note_start_activity();
        // Start key mapping: arrows/Enter act on press; Escape/Enter track
        // both edges for holds.
        if raw.pressed {
            let action = match &raw.key {
                iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape) => {
                    Some(StartKeyAction::Cancel)
                }
                iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter) => {
                    Some(StartKeyAction::Confirm)
                }
                iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowUp) => {
                    Some(StartKeyAction::Up)
                }
                iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowDown) => {
                    Some(StartKeyAction::Down)
                }
                iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowLeft) => {
                    Some(StartKeyAction::Left)
                }
                iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowRight) => {
                    Some(StartKeyAction::Right)
                }
                _ => None,
            };
            if let Some(action) = action {
                return self.on_start_key(id, action, true);
            }
        } else {
            let action = match &raw.key {
                iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape) => {
                    Some(StartKeyAction::Cancel)
                }
                iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter) => {
                    Some(StartKeyAction::Confirm)
                }
                _ => None,
            };
            if let Some(action) = action {
                return self.on_start_key(id, action, false);
            }
        }
        Task::none()
    }

    /// URL-edit mode: keyboard goes to the address field (the widget owns
    /// printable keys natively); the field is focused so typing lands there.
    /// Exits on navigate, Esc/Circle, or surface close.
    pub(crate) fn on_toggle_url_entry(&mut self) -> Task<Message> {
        if !self.browser.panel.session.is_open() {
            return Task::none();
        }
        let next = !self.browser.panel.url_editing;
        self.browser.panel.url_editing = next;
        if next {
            self.browser.panel.history_selector = None;
        }
        self.note_start_activity();
        self.play_ui_cue(UiSoundKind::Action);
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: browser url entry {}",
            if next { "open" } else { "close" }
        ));
        if next {
            return operation::focus(crate::ui::browser::view::browser_url_id());
        }
        Task::none()
    }

    pub(crate) fn on_browser_go_back(&mut self) -> Task<Message> {
        let session = &self.browser.panel.session;
        let (Some(current), true) = (session.nav_current(), session.can_go_back()) else {
            return Task::none();
        };
        if self.browser.engine.is_none() {
            return Task::none();
        }
        let target = current - 1;
        self.browser.panel.session.begin_history_travel(target);
        if let Some(engine) = &self.browser.engine {
            let tab = self.browser_tab();
            engine.send(EngineCommand::GoBack { tab, steps: 1 });
        }
        Task::none()
    }

    pub(crate) fn on_browser_go_forward(&mut self) -> Task<Message> {
        let session = &self.browser.panel.session;
        let (Some(current), true) = (session.nav_current(), session.can_go_forward()) else {
            return Task::none();
        };
        if self.browser.engine.is_none() {
            return Task::none();
        }
        let target = current + 1;
        self.browser.panel.session.begin_history_travel(target);
        if let Some(engine) = &self.browser.engine {
            let tab = self.browser_tab();
            engine.send(EngineCommand::GoForward { tab, steps: 1 });
        }
        Task::none()
    }

    /// Browser: open the hold-bumper history selector (B2.4). `back` picks
    /// the L1/back direction, else R1/forward. No-op when that direction
    /// has no entries (never an empty dropdown) or the selector is already
    /// open (a second hold never restacks). Default highlight is the
    /// closest entry — the quick-press target.
    pub(crate) fn on_browser_history_open(&mut self, back: bool) -> Task<Message> {
        if !self.browser.panel.session.is_open() {
            return Task::none();
        }
        if self.browser.panel.history_selector.is_some() {
            return Task::none();
        }
        let has = if back {
            self.browser.panel.session.can_go_back()
        } else {
            self.browser.panel.session.can_go_forward()
        };
        if !has {
            return Task::none();
        }
        self.browser.panel.history_selector =
            Some(crate::ui::browser::HistorySelector { back, highlight: 0 });
        self.note_start_activity();
        self.play_ui_cue(UiSoundKind::Action);
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: browser history selector open {}",
            if back { "back" } else { "forward" }
        ));
        Task::none()
    }

    /// Browser: selector pick — traverse to the absolute session index.
    /// History travel moves within the existing stack (go_back/go_forward),
    /// so the forward tail survives; only a normal navigation truncates it.
    /// Closes the selector first so the loadbehind never renders under it,
    /// and swallows an in-flight Cross press so the confirming release
    /// cannot also click the fresh page.
    pub(crate) fn on_browser_history_pick(&mut self, index: usize) -> Task<Message> {
        let session = &self.browser.panel.session;
        let Some(current) = session.nav_current() else {
            return Task::none();
        };
        let url = match session.nav_entry(index) {
            Some(entry) => entry.url.clone(),
            None => return Task::none(),
        };
        self.browser.panel.history_selector = None;
        self.browser.nav.cancel_held_face_releases();
        self.browser.panel.draft = url;
        if index == current {
            return Task::none();
        }
        if self.browser.engine.is_none() {
            // Reader fallback has no session-history traversal; keep the
            // old fetch path (forward tail may prune there).
            return self.on_browser_navigate();
        }
        let back = index < current;
        let steps = index.abs_diff(current);
        self.browser.panel.session.begin_history_travel(index);
        self.note_start_activity();
        self.play_ui_cue(UiSoundKind::Nav);
        if let Some(engine) = &self.browser.engine {
            let tab = self.browser_tab();
            engine.send(if back {
                EngineCommand::GoBack { tab, steps }
            } else {
                EngineCommand::GoForward { tab, steps }
            });
        }
        Task::none()
    }

    /// Browser: dismiss the history selector without navigating.
    pub(crate) fn on_browser_history_cancel(&mut self) -> Task<Message> {
        if self.browser.panel.history_selector.is_none() {
            return Task::none();
        }
        self.browser.panel.history_selector = None;
        self.browser.nav.cancel_held_face_releases();
        self.note_start_activity();
        self.play_ui_cue(UiSoundKind::Action);
        crate::controller::hid::diag::diag_info("ui-diag: browser history selector cancel");
        Task::none()
    }

    /// Dock the cursor onto a page-focused element (autofocus attribute, JS
    /// `.focus()`). Returns true when docked. Requires the magnet on, the
    /// stick at rest (an active steer means the user is driving and wins
    /// over the page), no mid-flight jump, and a live target — page intent
    /// never yanks a driven cursor.
    pub(crate) fn try_dock_focused(&mut self, id: u64) -> bool {
        let panel = &mut self.browser.panel;
        if !panel.cursor.magnet || panel.cursor.fly_to.is_some() {
            return false;
        }
        if panel.last_pad_stick.0.hypot(panel.last_pad_stick.1) != 0.0 {
            return false;
        }
        let Some(item) = panel.focusables.iter().find(|o| o.id == id).cloned() else {
            return false;
        };
        let cursor = &mut panel.cursor;
        cursor.attached = Some(id);
        cursor.dock_blend = 1.0;
        cursor.vx = 0.0;
        cursor.vy = 0.0;
        cursor.break_armed = false;
        cursor.docked_for = 0.0;
        if cursor.no_dock == Some(id) {
            cursor.no_dock = None;
        }
        cursor.x = (item.x + item.w / 2.0).clamp(0.0, VIEWPORT_W);
        cursor.y = (item.y + item.h / 2.0).clamp(0.0, VIEWPORT_H);
        cursor.last_dock = Some((item.x, item.y, item.w, item.h));
        crate::controller::hid::diag::diag_info(format!("ui-diag: browser autofocus dock {id}"));
        true
    }

    /// Pad tick while the history selector is open (B2.4). The selector owns
    /// the pad: Up/Down/Left/Right moves the highlight (highlight 0 =
    /// closest = quick-press default), Cross confirms, Circle cancels.
    /// Releasing the bumpers does nothing (the hold is over; the selector
    /// stays open for D-pad browsing — release-to-confirm fired the moment
    /// the user let go to reach the D-pad, making navigation impossible).
    /// Everything else — cursor, scroll, taps, magnetism — stays parked.
    /// Called before the engine/reader split so both modes share it.
    pub(crate) fn tick_browser_history_selector(
        &mut self,
        readings: &[start_input::NavReading],
        action: Option<NavAction>,
    ) -> Task<Message> {
        let _ = readings;
        match action {
            Some(NavAction::Up) | Some(NavAction::Left) => {
                if let Some(sel) = self.browser.panel.history_selector.as_mut()
                    && sel.highlight > 0
                {
                    sel.highlight -= 1;
                    self.note_start_activity();
                    self.play_ui_cue(UiSoundKind::Nav);
                }
                Task::none()
            }
            Some(NavAction::Down) | Some(NavAction::Right) => {
                let len = {
                    let panel = &self.browser.panel;
                    match &panel.history_selector {
                        Some(sel) if sel.back => panel.session.back_entries().len(),
                        Some(_) => panel.session.forward_entries().len(),
                        None => 0,
                    }
                };
                if let Some(sel) = self.browser.panel.history_selector.as_mut()
                    && len > 0
                {
                    sel.highlight += 1;
                    sel.clamp(len);
                    self.note_start_activity();
                    self.play_ui_cue(UiSoundKind::Nav);
                }
                Task::none()
            }
            Some(NavAction::Confirm) => {
                if let Some(sel) = &self.browser.panel.history_selector
                    && let Some(index) = sel.picked(&self.browser.panel.session)
                {
                    return self.on_browser_message(
                        crate::ui::browser::BrowserMessage::HistoryPick(index),
                    );
                }
                self.browser.panel.history_selector = None;
                Task::none()
            }
            Some(NavAction::Cancel) => {
                self.on_browser_message(crate::ui::browser::BrowserMessage::HistoryCancel)
            }
            // Taps, holds, magnetism, and cursor moves stay parked: the
            // hold claimed the press and the selector owns the outcome.
            _ => Task::none(),
        }
    }

    /// Star toggle: bookmark the current page (opening the rename bubble),
    /// or open the bubble for an already-bookmarked page. No page, no-op
    /// (fresh surface navigates home first — nothing to star yet).
    pub(crate) fn on_browser_bookmark_star(&mut self) -> Task<Message> {
        let url = self.browser.panel.session.url().to_string();
        if url.is_empty() {
            return Task::none();
        }
        let panel = &mut self.browser.panel;
        if !panel.is_bookmarked(&url) {
            let title = panel.session.title().unwrap_or(&url).to_string();
            panel.bookmarks.push(crate::browser::bookmarks::Bookmark {
                url: url.clone(),
                title,
            });
            crate::browser::bookmarks::save(&panel.bookmarks);
            panel.rebuild_chrome();
        }
        let draft = panel
            .bookmarks
            .iter()
            .find(|b| b.url == url)
            .map(|b| b.title.clone())
            .unwrap_or_default();
        panel.bookmark_name_draft = draft;
        panel.bookmark_bubble = true;
        self.note_start_activity();
        self.play_ui_cue(UiSoundKind::Action);
        operation::focus(crate::ui::browser::view::bookmark_name_id())
    }

    /// Star bubble Done: apply a non-empty rename, persist, close.
    /// An empty draft keeps the existing name (Chrome requires one; v1
    /// keeps the old instead of deleting).
    pub(crate) fn on_browser_bookmark_save(&mut self) -> Task<Message> {
        let url = self.browser.panel.session.url().to_string();
        let panel = &mut self.browser.panel;
        if let Some(entry) = panel.bookmarks.iter_mut().find(|b| b.url == url) {
            let renamed = panel.bookmark_name_draft.trim().to_string();
            if !renamed.is_empty() {
                entry.title = renamed;
            }
            crate::browser::bookmarks::save(&panel.bookmarks);
        }
        self.browser.panel.bookmark_bubble = false;
        self.note_start_activity();
        self.play_ui_cue(UiSoundKind::Action);
        Task::none()
    }

    /// Star bubble Remove: delete the current page's bookmark, persist,
    /// close.
    pub(crate) fn on_browser_bookmark_remove(&mut self) -> Task<Message> {
        let url = self.browser.panel.session.url().to_string();
        let panel = &mut self.browser.panel;
        panel.bookmarks.retain(|b| b.url != url);
        crate::browser::bookmarks::save(&panel.bookmarks);
        panel.rebuild_chrome();
        panel.bookmark_bubble = false;
        self.note_start_activity();
        self.play_ui_cue(UiSoundKind::Action);
        Task::none()
    }

    pub(crate) fn on_browser_reload(&mut self) -> Task<Message> {
        if self.browser.panel.session.url().is_empty() {
            return self.on_browser_navigate();
        }
        self.browser.panel.draft = self.browser.panel.session.url().to_string();
        self.on_browser_navigate()
    }

    /// Escape hatch for pages the reader backend cannot render.
    pub(crate) fn on_browser_open_external(&mut self) -> Task<Message> {
        let session = &self.browser.panel.session;
        let raw = if session.url().is_empty() {
            self.browser.panel.draft.clone()
        } else {
            session.url().to_string()
        };
        let url = crate::browser::nav::resolve_navigate_target(&raw);
        if url.is_empty() {
            return Task::none();
        }
        if let Err(err) = launch::launch_target(&url, "") {
            app_log::warn(format!("browser: external open failed: {err}"));
        }
        Task::none()
    }

    /// Per-poll analog layer for the engine browser: left stick moves the
    /// virtual cursor (magnetic snap), right stick scrolls. Runs on every
    /// pad tick — including neutral ones — so friction always decays the
    /// cursor velocity; otherwise a released stick would freeze mid-glide
    /// and resume in the stale direction on the next push.
    pub(crate) fn tick_browser_cursor(
        &mut self,
        readings: &[start_input::NavReading],
        now: Instant,
    ) {
        let Some(reading) = start_input::preferred_reading(readings) else {
            return;
        };
        let sample = &reading.sample;
        #[cfg(debug_assertions)]
        let was_docked = self.browser.panel.cursor.attached.is_some();
        let (dx, dy) = self.browser.panel.advance_stick(
            sample.lstick_x,
            sample.lstick_y,
            sample.rstick_x,
            sample.rstick_y,
            sample.r2,
            now,
        );
        // Stick vector for autofocus docking: a deflected stick means the
        // user is driving and wins over page focus.
        self.browser.panel.last_pad_stick = (sample.lstick_x, sample.lstick_y);
        // Dock-triggered shot: capture the exact pixels the fresh wrap sits
        // on (throttled; the 5s cadence rarely coincides with a dock).
        #[cfg(debug_assertions)]
        if self.debug_browser && !was_docked && self.browser.panel.cursor.attached.is_some() {
            self.browser.panel.last_dock_change = Some(now);
        }
        if (dx != 0.0 || dy != 0.0)
            && let Some(engine) = &self.browser.engine
        {
            // Scroll under the pad cursor: Chromium routes to the innermost
            // scroller under the point, so center-targeting stranded page
            // scrolls inside carousels after use.
            let (cx, cy) = (self.browser.panel.cursor.x, self.browser.panel.cursor.y);
            engine.send(EngineCommand::ScrollBy {
                tab: self.browser_tab(),
                dx,
                dy,
                x: cx,
                y: cy,
            });
        }
    }

    /// D-pad cursor jumps for engine mode. The banked nav stepper stays
    /// parked there (stick moves must never double-fire into both cursor
    /// and nav), which would otherwise starve the `jump_cursor` arms of
    /// `tick.action`. Runs a dedicated stepper on a stickless mask instead:
    /// press jumps at once, holds repeat, D-pad release resets at once.
    /// Returns the jump (if any); the caller applies it alongside whatever
    /// `tick.action` carried (face buttons are orthogonal).
    pub(crate) fn poll_browser_dpad_jump(
        &mut self,
        readings: &[start_input::NavReading],
        now: Instant,
    ) -> Option<JumpDir> {
        let reading = start_input::preferred_reading(readings)?;
        let masked = start_input::dpad_only(&reading.sample);
        // No D-pad held: feed neutral so a release resets repeat gating at
        // once (D-pad contacts never flicker — same discipline as the bank).
        let action = self.browser.dpad_stepper.update(&masked, now, true, true);
        match action {
            Some(NavAction::Up) => Some(JumpDir::Up),
            Some(NavAction::Down) => Some(JumpDir::Down),
            Some(NavAction::Left) => Some(JumpDir::Left),
            Some(NavAction::Right) => Some(JumpDir::Right),
            _ => None,
        }
    }

    /// Stream the pad cursor as the one true mouse (see `EngineCommand::MouseMove`):
    /// quantized to whole px so a resting cursor sends nothing. Called once
    /// per browser tick AFTER input handling, so stick glides and D-pad
    /// jumps — every cursor mover — converge on one sync point.
    pub(crate) fn sync_browser_mouse(&mut self) -> Task<Message> {
        // Clamped out of the header zone (see `sync_live_mouse`).
        let (qx, qy) = (
            self.browser.panel.cursor.x as i32,
            self.browser.panel.cursor.y.max(0.0) as i32,
        );
        if self.browser.panel.last_sent_mouse != Some((qx, qy)) {
            self.browser.panel.last_sent_mouse = Some((qx, qy));
            if let Some(engine) = &self.browser.engine {
                engine.send(EngineCommand::MouseMove {
                    tab: self.browser_tab(),
                    x: qx as f32,
                    y: qy as f32,
                });
            }
        }
        // A D-pad jump asks for a fresh target cull once the flight lands
        // (the live set was culled around the previous pull center). Send
        // the reseed after the MouseMove so the engine centers on the
        // arrival position, not the flight origin.
        if self.browser.panel.dom_reseed_pending && self.browser.panel.cursor.fly_to.is_none() {
            self.browser.panel.dom_reseed_pending = false;
            if let Some(engine) = &self.browser.engine {
                engine.send(EngineCommand::ReseedDom {
                    tab: self.browser_tab(),
                });
            }
        }
        Task::none()
    }

    /// Click at the cursor: attached element center, else the free point.
    /// Passes the docked target id so Button-kind targets can activate on
    /// the exact element (`__sdsc_click`); everything else (and free
    /// points) go through the trusted synthetic pair. A chrome dock isn't
    /// a page click at all — it runs the header action instead.
    pub(crate) fn click_browser_cursor(&mut self) -> Task<Message> {
        // Chrome dock first: pad action, never an engine click.
        if let Some(target) = self.browser.panel.chrome_attached() {
            self.browser.panel.last_click = Some((
                (self.browser.panel.cursor.x, self.browser.panel.cursor.y),
                Instant::now(),
            ));
            if let Some(message) = self.browser.panel.chrome_message(target) {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: browser chrome activate {target:?}"
                ));
                return self.on_browser_message(message);
            }
            return Task::none();
        }
        // Precise click inside the header: hit the rect under the point even
        // without a magnet dock. Empty header space must not fall through to
        // a page click at negative coordinates.
        if self.browser.panel.cursor.y < 0.0 {
            if let Some(target) = self
                .browser
                .panel
                .chrome_target_at(self.browser.panel.cursor.x, self.browser.panel.cursor.y)
                && let Some(message) = self.browser.panel.chrome_message(target)
            {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: browser precise chrome activate {target:?}"
                ));
                return self.on_browser_message(message);
            }
            return Task::none();
        }
        let (x, y, target) = {
            let panel = &self.browser.panel;
            let targets = crate::ui::browser::cursor_targets(&panel.focusables, &panel.chrome);
            let (x, y) = panel.cursor.click_point(&targets);
            // Exact-element targets: docked buttons honor the Diagnostics
            // JS-click toggle (untrusted-activation A/B); docked links
            // always go exact — synthetic Down/Up pairs demonstrably never
            // navigated, so there is nothing to A/B there. The backend
            // falls back to synthetic at the point when the element is gone.
            let target = match panel.cursor.attached {
                Some(id)
                    if panel.focusables.iter().any(|o| {
                        o.id == id
                            && (o.kind == crate::browser::cursor::FocusKind::Link
                                || (o.kind == crate::browser::cursor::FocusKind::Button
                                    && panel.js_click))
                    }) =>
                {
                    Some(id)
                }
                _ => None,
            };
            (x, y, target)
        };
        // Instant header feedback ("Opening link…"): the click is
        // acknowledged now; the page changing (or not) tells the rest.
        let label =
            match target.and_then(|id| self.browser.panel.focusables.iter().find(|o| o.id == id)) {
                Some(o) => match o.kind {
                    crate::browser::cursor::FocusKind::Link => "Opening link…",
                    crate::browser::cursor::FocusKind::Field => "Focusing field…",
                    _ => "Activating…",
                },
                None => "Clicking…",
            };
        self.browser.panel.last_click = Some(((x, y), Instant::now()));
        self.browser.panel.action_hint = Some((label.to_string(), Instant::now()));
        if let Some(engine) = &self.browser.engine {
            engine.send(EngineCommand::ClickAt {
                tab: self.browser_tab(),
                x,
                y,
                target,
            });
        }
        Task::none()
    }

    /// R3 issue flag: user reports "clicked and nothing happened". Dumps one
    /// correlatable bundle (cursor, dock, nearest targets, last click age,
    /// scroll, url, wells) and forces a debug shot, so the backend pull log
    /// around this timestamp tells the rest of the story.
    pub(crate) fn flag_browser_issue(&mut self, now: Instant) -> Task<Message> {
        let targets = crate::ui::browser::cursor_targets(
            &self.browser.panel.page.focusables,
            &self.browser.panel.chrome,
        );
        let panel = &mut self.browser.panel;
        let (cx, cy) = (panel.cursor.x, panel.cursor.y);
        let dock = panel.cursor.attached;
        let dock_rect = panel.cursor.attached_rect(&targets);
        let would_click = panel.cursor.click_point(&targets);
        let last_click_ago = panel
            .last_click
            .map(|(_, t)| now.saturating_duration_since(t).as_millis());
        let last_click_at = panel.last_click.map(|(p, _)| p);
        // Nearest 8 targets by center distance: the actionable context.
        // Kind included: distinguishes Button/Link/Field/Other at a glance.
        let mut ranked: Vec<(
            u64,
            crate::browser::cursor::FocusKind,
            f32,
            f32,
            f32,
            f32,
            f32,
        )> = panel
            .focusables
            .iter()
            .map(|o| {
                let d = ((o.x + o.w / 2.0) - cx).abs() + ((o.y + o.h / 2.0) - cy).abs();
                (o.id, o.kind, o.x, o.y, o.w, o.h, d)
            })
            .collect();
        ranked.sort_by(|a, b| a.6.partial_cmp(&b.6).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(8);
        crate::platform::app_log::warn(format!(
            "ISSUE-FLAG cursor=({cx:.0},{cy:.0}) dock={dock:?} dock_rect={dock_rect:?} would_click=({:.0},{:.0}) last_click_at={last_click_at:?} last_click_ago_ms={last_click_ago:?} scroll={:?} wells={} url={} near={ranked:?}",
            would_click.0,
            would_click.1,
            panel.scroll_offset,
            panel.focusables.len(),
            panel.session.url(),
        ));
        #[cfg(debug_assertions)]
        {
            if let Some(engine) = &self.browser.engine {
                engine.send(EngineCommand::ProbeEventLoop {
                    tab: self.browser_tab(),
                });
            }
            self.shoot_browser_window(true)
        }
        #[cfg(not(debug_assertions))]
        {
            Task::none()
        }
    }

    /// Debug-only window shot: every 5s while the browser overlay is on (or
    /// immediately on a fresh dock), plus forced on R3 issue flags.
    /// `force` bypasses the cadence gate. Encode + write happen on a
    /// throwaway thread so the UI never hitches on disk I/O.
    #[cfg(debug_assertions)]
    pub(crate) fn shoot_browser_window(&mut self, force: bool) -> Task<Message> {
        if !self.debug_browser {
            return Task::none();
        }
        let now = Instant::now();
        let panel = &mut self.browser.panel;
        // A fresh dock shoots once it settles (~300ms for the morph to
        // complete); the 5s cadence covers everything else.
        let settled_dock = panel.cursor.attached.is_some()
            && panel.last_dock_change.is_some_and(|t| {
                now.saturating_duration_since(t) >= Duration::from_millis(300)
                    && panel.last_shot.is_none_or(|s| s < t)
            });
        if !force
            && !settled_dock
            && panel
                .last_shot
                .is_some_and(|t| now.saturating_duration_since(t) < Duration::from_secs(5))
        {
            return Task::none();
        }
        panel.last_shot = Some(now);
        let Some(win) = self.browser_window else {
            return Task::none();
        };
        iced::window::screenshot(win)
            .map(crate::ui::browser::BrowserMessage::Shot)
            .map(Message::Browser)
    }

    /// Debug-only: persist a captured window shot with the live cursor /
    /// dock / target state for post-hoc outline placement analysis.
    #[cfg(debug_assertions)]
    pub(crate) fn save_browser_shot(&mut self, shot: iced::window::Screenshot) -> Task<Message> {
        let panel = &self.browser.panel;
        let targets = crate::ui::browser::cursor_targets(&panel.focusables, &panel.chrome);
        let cursor = (panel.cursor.x, panel.cursor.y);
        let magnet = panel.cursor.magnet;
        let dock = panel.cursor.attached;
        let blend = panel.cursor.dock_blend;
        let rect = panel
            .cursor
            .attached_rect(&targets)
            .map(|(x, y, w, h)| format!("({x:.0},{y:.0}) {w:.0}x{h:.0}"))
            .unwrap_or_else(|| "-".to_string());
        let wells = panel.focusables.len();
        let scroll = panel.scroll_offset;
        let (w, h) = (shot.size.width, shot.size.height);
        let scale = shot.scale_factor;
        let rgba = shot.rgba.to_vec();
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        std::thread::spawn(move || {
            let dir = crate::persist::paths::data_dir().join("shots");
            if let Err(err) = std::fs::create_dir_all(&dir) {
                app_log::warn(format!("shot skipped: cannot create {dir:?}: {err}"));
                return;
            }
            let path = dir.join(format!("shot-{ms}.png"));
            match image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8) {
                Ok(()) => app_log::info(format!(
                    "shot saved path={} size={w}x{h} scale={scale} cursor=({:.0},{:.0}) magnet={magnet} dock={dock:?} blend={blend:.2} rect={rect} wells={wells} scroll=({:.0},{:.0})",
                    path.display(),
                    cursor.0,
                    cursor.1,
                    scroll.0,
                    scroll.1,
                )),
                Err(err) => app_log::warn(format!("shot failed path={}: {err}", path.display())),
            }
        });
        Task::none()
    }

    /// Engine-created tab (popup `request_create_new`): the backend already
    /// foregrounded the view — adopt it. Stash the live page, reset the
    /// live slot, point the draft at the incoming navigation.
    pub(crate) fn on_tab_opened(
        &mut self,
        tab: crate::browser::engine::TabId,
        slot: std::sync::Arc<crate::browser::frame_slot::FrameSlot>,
    ) -> Task<Message> {
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: browser tab opened tab{}",
            tab.0
        ));
        {
            let panel = &mut self.browser.panel;
            panel.stash_live();
            panel.reset_live_page();
            panel.page.session.open_surface();
            panel.page.session.set_frame_slot(slot);
            panel.page.action_hint = Some(("New tab…".to_string(), std::time::Instant::now()));
            panel.active_tab = tab;
            if !panel.order.contains(&tab) {
                panel.order.push(tab);
            }
            panel.draft.clear();
            panel.rebuild_chrome();
            panel.save_tabs();
        }
        self.sync_live_mouse();
        Task::none()
    }

    /// Worker-owned frame slot handoff for a tab the UI already knows
    /// (the boot tab, delivered on the first visible tick).
    pub(crate) fn on_frame_slot(
        &mut self,
        tab: crate::browser::engine::TabId,
        slot: std::sync::Arc<crate::browser::frame_slot::FrameSlot>,
    ) -> Task<Message> {
        let panel = &mut self.browser.panel;
        if tab == panel.active_tab {
            panel.session.set_frame_slot(slot);
        } else if let Some(stored) = panel.stored.iter_mut().find(|t| t.id == tab) {
            stored.page.session.set_frame_slot(slot);
        }
        Task::none()
    }

    /// Engine reports a tab's page called `window.close()`: the view is
    /// already gone backend-side — adopt the most recent survivor (or a
    /// fresh home tab when none).
    pub(crate) fn on_tab_closed(&mut self, tab: crate::browser::engine::TabId) -> Task<Message> {
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: browser tab closed tab{}",
            tab.0
        ));
        let active = self.browser.panel.active_tab;
        if tab != active {
            self.browser.panel.drop_stored(tab);
            self.save_strip();
            return Task::none();
        }
        // Active tab closed by its page: forget it, adopt a survivor.
        self.browser.panel.order.retain(|o| *o != tab);
        let survivor = self.browser.panel.order.pop();
        match survivor {
            Some(next) => {
                let needs_open = self
                    .browser
                    .panel
                    .stored
                    .iter()
                    .find(|t| t.id == next)
                    .is_some_and(|t| !t.loaded);
                let ok = self.browser.panel.activate_stored(next);
                debug_assert!(ok);
                let url = self.browser.panel.session.url().to_string();
                self.browser.panel.draft = url.clone();
                self.save_strip();
                if self.browser.engine.is_none() {
                    // Reader fallback: fetch the adopted tab.
                    return self.on_browser_navigate();
                }
                self.foreground_live_tab(next, needs_open, url);
                Task::none()
            }
            None => self.reopen_home_tab(tab),
        }
    }

    /// Fresh home tab reusing `tab` (backend dropped it): recreate the view
    /// with the home URL from birth and navigate the session there. Tab-less
    /// engine has no boot path back, so the last tab never truly closes.
    fn reopen_home_tab(&mut self, tab: crate::browser::engine::TabId) -> Task<Message> {
        let url = crate::browser::BROWSER_HOME_URL.to_string();
        let (generation, frame) = {
            let panel = &mut self.browser.panel;
            panel.reset_live_page();
            panel.page.session.open_surface();
            panel.active_tab = tab;
            panel.order.push(tab);
            panel.draft = url.clone();
            let generation = panel.page.session.begin_load(url.clone());
            let frame = std::sync::Arc::clone(panel.page.session.frame_slot());
            (generation, frame)
        };
        self.save_strip();
        if let Some(engine) = &self.browser.engine {
            engine.send(EngineCommand::OpenTab {
                tab,
                url,
                generation,
                frame,
            });
            engine.send(EngineCommand::SetActiveTab { tab });
        }
        self.sync_browser_theme();
        self.note_start_activity();
        self.play_ui_cue(UiSoundKind::Nav);
        crate::controller::hid::diag::diag_info("ui-diag: browser navigate");
        Task::none()
    }

    /// First-open restore from `tabs.json`: rebuild the strip (live +
    /// stored), pre-seed titles, and drive every tab. Backend views are
    /// fresh, so each tab re-navigates — engine history starts over by
    /// construction. Falls back to the home flow when nothing restorable
    /// survives validation.
    pub(crate) fn restore_browser_tabs(
        &mut self,
        saved: crate::browser::tabs::SavedTabs,
    ) -> Task<Message> {
        use crate::browser::engine::TabId;
        let mut tabs: Vec<crate::browser::tabs::SavedTab> = saved
            .tabs
            .into_iter()
            .filter(|t| !t.url.is_empty() && t.url != crate::browser::BLANK_PAGE_URL)
            .take(crate::browser::tabs::MAX_SAVED_TABS)
            .collect();
        if tabs.is_empty() {
            self.browser.panel.draft = crate::browser::BROWSER_HOME_URL.to_string();
            return self.on_browser_navigate();
        }
        if self.crash_restart_boot {
            // Safe mode: the restored active tab is the page that killed
            // CEF. Start on home and keep every saved tab stored (the
            // user can switch deliberately); never auto-load the crash page
            // again on relaunch.
            crate::platform::app_log::info(
                "crash-restart: safe tab restore (home live, saved tabs stored)",
            );
            let generation = {
                let panel = &mut self.browser.panel;
                panel.stored.clear();
                panel.order.clear();
                panel.active_tab = TabId::BOOT;
                panel.reset_live_page();
                panel.page.session.open_surface();
                let generation = panel
                    .page
                    .session
                    .begin_load(crate::browser::BROWSER_HOME_URL.to_string());
                panel.draft = crate::browser::BROWSER_HOME_URL.to_string();
                panel.order.push(TabId::BOOT);
                let mut next_id = panel.next_tab_id.max(2);
                for saved_tab in tabs {
                    let id = TabId(next_id);
                    next_id += 1;
                    let mut page = crate::ui::browser::BrowserTabPage::default();
                    page.session.open_surface();
                    page.session.note_url(saved_tab.url.clone());
                    if !saved_tab.title.is_empty() {
                        page.session.note_title(Some(saved_tab.title));
                    }
                    panel.stored.push(crate::ui::browser::StoredTab {
                        id,
                        page,
                        loaded: false,
                    });
                    panel.order.push(id);
                }
                panel.next_tab_id = next_id;
                panel.rebuild_chrome();
                generation
            };
            self.save_strip();
            if let Some(engine) = &self.browser.engine {
                engine.send(EngineCommand::Navigate {
                    tab: TabId::BOOT,
                    url: crate::browser::BROWSER_HOME_URL.to_string(),
                    generation,
                });
            }
            return Task::none();
        }
        let active_pos = saved.active.min(tabs.len() - 1);
        // The live tab's load (restored tabs behind it stay unloaded until
        // first switch — the worker only ever pays for visible tabs).
        let mut live_load: Option<(TabId, String, u64)> = None;
        {
            let panel = &mut self.browser.panel;
            panel.stored.clear();
            panel.order.clear();
            let mut next_id = panel.next_tab_id.max(2);
            for (pos, saved_tab) in tabs.drain(..).enumerate() {
                if pos == active_pos {
                    panel.active_tab = TabId::BOOT;
                    panel.reset_live_page();
                    panel.page.session.open_surface();
                    let generation = panel.page.session.begin_load(saved_tab.url.clone());
                    if !saved_tab.title.is_empty() {
                        panel.page.session.note_title(Some(saved_tab.title.clone()));
                    }
                    panel.order.push(TabId::BOOT);
                    panel.draft = saved_tab.url.clone();
                    live_load = Some((TabId::BOOT, saved_tab.url, generation));
                } else {
                    // Unloaded: URL + title recorded for the strip, no
                    // generation, no view — first switch opens and loads it.
                    let id = TabId(next_id);
                    next_id += 1;
                    let mut page = crate::ui::browser::BrowserTabPage::default();
                    page.session.open_surface();
                    page.session.note_url(saved_tab.url.clone());
                    if !saved_tab.title.is_empty() {
                        page.session.note_title(Some(saved_tab.title));
                    }
                    panel.stored.push(crate::ui::browser::StoredTab {
                        id,
                        page,
                        loaded: false,
                    });
                    panel.order.push(id);
                }
            }
            panel.next_tab_id = next_id;
            panel.rebuild_chrome();
        }
        if let Some(engine) = &self.browser.engine {
            if let Some((tab, url, generation)) = live_load {
                engine.send(EngineCommand::Navigate {
                    tab,
                    url,
                    generation,
                });
            }
        } else {
            // Reader fallback: fetch the live tab only; stored tabs fetch
            // when switched to.
            return self.on_browser_navigate();
        }
        self.prefetch_stored_tab_favicons();
        self.sync_browser_theme();
        self.note_start_activity();
        self.play_ui_cue(UiSoundKind::Nav);
        crate::controller::hid::diag::diag_info("ui-diag: browser navigate");
        Task::none()
    }

    /// Restored background tabs have URLs but no loaded page (and therefore
    /// no CEF favicon callback). Fetch their conventional icons so the strip
    /// does not look half-finished until each tab is visited.
    fn prefetch_stored_tab_favicons(&mut self) {
        let Some(engine) = &self.browser.engine else {
            return;
        };
        let mut seen = std::collections::HashSet::new();
        for stored in &self.browser.panel.stored {
            let url = stored.page.session.url().to_string();
            if url.is_empty() {
                continue;
            }
            let key = crate::browser::bookmarks::origin_key(&url);
            if key == url || self.browser.panel.favicons.contains_key(&key) || !seen.insert(key) {
                continue;
            }
            engine.send(crate::browser::engine::EngineCommand::PrefetchFavicon { url });
        }
    }

    /// User-opened tab (`+`): stash live, reset, create the home-loaded view
    /// and foreground it. Reader fallback (no engine) just goes home in
    /// place — tabs need views.
    pub(crate) fn on_browser_tab_new(&mut self) -> Task<Message> {
        if self.browser.engine.is_none() {
            return self.on_browser_go_home();
        }
        let url = crate::browser::BROWSER_HOME_URL.to_string();
        let (tab, generation, frame) = {
            let panel = &mut self.browser.panel;
            panel.stash_live();
            panel.reset_live_page();
            panel.page.session.open_surface();
            let tab = crate::browser::engine::TabId(panel.next_tab_id);
            panel.next_tab_id = panel.next_tab_id.wrapping_add(1).max(2);
            panel.active_tab = tab;
            panel.order.push(tab);
            panel.draft = url.clone();
            let generation = panel.page.session.begin_load(url.clone());
            panel.rebuild_chrome();
            panel.save_tabs();
            let frame = std::sync::Arc::clone(panel.page.session.frame_slot());
            (tab, generation, frame)
        };
        if let Some(engine) = &self.browser.engine {
            engine.send(EngineCommand::OpenTab {
                tab,
                url,
                generation,
                frame,
            });
            engine.send(EngineCommand::SetActiveTab { tab });
        }
        self.sync_browser_theme();
        self.note_start_activity();
        self.play_ui_cue(UiSoundKind::Nav);
        crate::controller::hid::diag::diag_info("ui-diag: browser navigate");
        Task::none()
    }

    /// Foreground the live tab's view: open it on first show (restored tabs
    /// start unloaded), then activate + re-sync the mouse. `needs_open`
    /// comes from the stored entry's `loaded` flag, read before the swap.
    pub(crate) fn foreground_live_tab(
        &mut self,
        tab: crate::browser::engine::TabId,
        needs_open: bool,
        url: String,
    ) {
        if let Some(engine) = &self.browser.engine {
            if needs_open {
                let generation = self.browser.panel.session.begin_load(url.clone());
                engine.send(EngineCommand::OpenTab {
                    tab,
                    url,
                    generation,
                    frame: std::sync::Arc::clone(self.browser.panel.session.frame_slot()),
                });
            }
            engine.send(EngineCommand::SetActiveTab { tab });
        }
        self.sync_live_mouse();
        self.sync_browser_theme();
    }

    /// Strip changed shape or membership: persist it (`tabs.json`) and
    /// rebuild the chrome snap map (rects derive from order/bookmarks).
    /// URL/title-only traffic uses `save_tabs` directly (no geometry
    /// change, mapping stays valid).
    pub(crate) fn save_strip(&mut self) {
        self.browser.panel.rebuild_chrome();
        self.browser.panel.save_tabs();
    }

    /// User switches tab (strip click): swap the live page with the stored
    /// one (order untouched), foreground the view, mirror the URL to the
    /// draft, re-sync the mouse.
    pub(crate) fn on_browser_tab_switch(
        &mut self,
        tab: crate::browser::engine::TabId,
    ) -> Task<Message> {
        if tab == self.browser.panel.active_tab {
            return Task::none();
        }
        let old = self.browser.panel.active_tab;
        let needs_open = self
            .browser
            .panel
            .stored
            .iter()
            .find(|t| t.id == tab)
            .is_some_and(|t| !t.loaded);
        let old_scroll = self.browser.panel.scroll_offset;
        if !self.browser.panel.activate_stored(tab) {
            return Task::none();
        }
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: browser tab switch tab{} -> tab{} scroll ({:.0},{:.0}) -> ({:.0},{:.0})",
            old.0,
            tab.0,
            old_scroll.0,
            old_scroll.1,
            self.browser.panel.scroll_offset.0,
            self.browser.panel.scroll_offset.1
        ));
        // The shelved page keeps its backing view.
        if let Some(prev) = self.browser.panel.stored.iter_mut().find(|t| t.id == old) {
            prev.loaded = true;
        }
        let url = self.browser.panel.session.url().to_string();
        self.browser.panel.draft = url.clone();
        // The visible tab window follows the active tab, so the strip
        // geometry changes on every switch (not just membership).
        self.save_strip();
        if self.browser.engine.is_none() {
            // Reader fallback: no views to foreground — fetch the newly
            // live tab's URL through the normal path instead.
            return self.on_browser_navigate();
        }
        self.note_start_activity();
        self.play_ui_cue(UiSoundKind::Action);
        self.foreground_live_tab(tab, needs_open, url);
        Task::none()
    }

    /// Cycle to the next tab in strip order (wraps). Used by the tab
    /// strip's overflow chip, which stands in for tabs hidden by the
    /// fit pass; switching windows the strip around the new active tab.
    pub(crate) fn on_browser_tab_next(&mut self) -> Task<Message> {
        let order = self.browser.panel.order.clone();
        if order.len() < 2 {
            return Task::none();
        }
        let active = self.browser.panel.active_tab;
        let index = order.iter().position(|id| *id == active).unwrap_or(0);
        let next = order[(index + 1) % order.len()];
        self.on_browser_tab_switch(next)
    }

    /// User closes a tab (`×`): background tabs drop outright; closing the
    /// active tab adopts the most recent survivor first. The last tab
    /// resets to home instead of closing.
    pub(crate) fn on_browser_tab_close(
        &mut self,
        tab: crate::browser::engine::TabId,
    ) -> Task<Message> {
        if tab != self.browser.panel.active_tab {
            let loaded = self
                .browser
                .panel
                .stored
                .iter()
                .find(|t| t.id == tab)
                .map(|t| t.loaded)
                .unwrap_or(false);
            self.browser.panel.drop_stored(tab);
            self.save_strip();
            // Unloaded tabs have no view to close (restored, never shown).
            if loaded && let Some(engine) = &self.browser.engine {
                engine.send(EngineCommand::CloseTab { tab });
            }
            return Task::none();
        }
        // Active tab: survivors live in `stored`.
        if self.browser.panel.stored.is_empty() {
            return self.on_browser_go_home();
        }
        let next = self
            .browser
            .panel
            .order
            .iter()
            .rev()
            .find(|o| **o != tab)
            .copied();
        let Some(next) = next else {
            return self.on_browser_go_home();
        };
        let needs_open = self
            .browser
            .panel
            .stored
            .iter()
            .find(|t| t.id == next)
            .is_some_and(|t| !t.loaded);
        let ok = self.browser.panel.activate_stored(next);
        if !ok {
            return self.on_browser_go_home();
        }
        self.browser.panel.order.retain(|o| *o != tab);
        let url = self.browser.panel.session.url().to_string();
        self.browser.panel.draft = url.clone();
        self.save_strip();
        if self.browser.engine.is_none() {
            // Reader fallback: fetch the adopted tab like a fresh navigate.
            return self.on_browser_navigate();
        }
        self.note_start_activity();
        self.play_ui_cue(UiSoundKind::Action);
        self.foreground_live_tab(next, needs_open, url);
        if let Some(engine) = &self.browser.engine {
            engine.send(EngineCommand::CloseTab { tab });
        }
        Task::none()
    }

    /// Pad/keyboard body scroll. Engine mode scrolls Chromium at the cursor
    /// point (`tick_browser_cursor` handles analog scroll); this path covers
    /// explicit D-pad/keys.
    pub(crate) fn scroll_browser(&mut self, delta: f32) -> Task<Message> {
        if self.browser.panel.session.has_frame() {
            if let Some(engine) = &self.browser.engine {
                let (cx, cy) = (self.browser.panel.cursor.x, self.browser.panel.cursor.y);
                engine.send(EngineCommand::ScrollBy {
                    tab: self.browser_tab(),
                    dx: 0.0,
                    dy: delta,
                    x: cx,
                    y: cy,
                });
            }
            return Task::none();
        }
        let y = self.browser.panel.scroll_by(delta);
        operation::scroll_to(
            crate::ui::browser::view::browser_scroll_id(),
            operation::AbsoluteOffset {
                x: None,
                y: Some(y),
            },
        )
    }

    /// Engine worker event. Deliberately no activity stamp: frames flow
    /// continuously while visible and must not defeat idle-dim.
    ///
    /// Tab routing: events for the active tab apply to the live page state;
    /// background-tab events apply to the matching stored tab (the draft
    /// mirrors the active tab only). Unknown (closed-race) ids are ignored.
    pub(crate) fn on_engine_event(&mut self, event: EngineEvent) -> Task<Message> {
        match event {
            EngineEvent::TabOpened { tab, slot } => return self.on_tab_opened(tab, slot),
            EngineEvent::FrameSlot { tab, slot } => return self.on_frame_slot(tab, slot),
            EngineEvent::TabClosed { tab } => return self.on_tab_closed(tab),
            _ => {}
        }
        let tab = event.tab().expect("tab-carrying event checked above");
        if tab != self.browser.panel.active_tab {
            self.route_background_event(tab, event);
            return Task::none();
        }
        match event {
            EngineEvent::FrameSlot { .. }
            | EngineEvent::TabOpened { .. }
            | EngineEvent::TabClosed { .. } => {}
            EngineEvent::Frame(frame) => {
                if !self.browser.panel.session.is_open() {
                    return Task::none();
                }
                #[cfg(debug_assertions)]
                let shot = self.shoot_browser_window(false);
                // Move the RGBA buffer into the slot; the stable-texture
                // primitive uploads unseen generations into one texture.
                self.browser
                    .panel
                    .session
                    .write_frame(frame.width, frame.height, frame.rgba);
                #[cfg(debug_assertions)]
                return shot;
            }
            EngineEvent::TitleChanged { tab: _, title } => {
                self.browser.panel.session.note_title(title);
                // The boot tab loads before the Browser window opens; writing
                // the strip now would replace the saved multi-tab file with
                // this one boot tab before the first restore reads it.
                if self.browser.tabs_restored {
                    self.browser.panel.save_tabs();
                }
            }
            EngineEvent::UrlChanged { tab: _, url } => {
                self.browser.panel.session.note_url(url.clone());
                self.browser.panel.draft = url;
                if self.browser.tabs_restored {
                    self.browser.panel.save_tabs();
                }
                // A new document underneath invalidates the selector's
                // snapshot (link click, redirect, history travel).
                self.browser.panel.history_selector = None;
            }
            EngineEvent::FaviconChanged {
                tab: _,
                url,
                width,
                height,
                rgba,
            } => {
                let handle = iced::widget::image::Handle::from_rgba(width, height, rgba);
                crate::platform::app_log::info(format!(
                    "browser-dom: favicon {} {width}x{height}",
                    crate::browser::bookmarks::origin_key(&url)
                ));
                self.browser.panel.note_favicon(url, handle);
            }
            EngineEvent::A11yTree { tab: _, items } => {
                self.browser.panel.page.focusables = items;
                let targets = {
                    let panel = &self.browser.panel;
                    crate::ui::browser::cursor_targets(&panel.page.focusables, &panel.chrome)
                };
                self.browser.panel.page.cursor.retarget(&targets);
                // Parked page-focus target: dock when it resolves live; drop
                // after one tree if it never does (culled targets never
                // dock). A mid-flight jump keeps the target parked.
                if self.browser.panel.pending_focus.is_some()
                    && self.browser.panel.cursor.fly_to.is_none()
                {
                    let id = self
                        .browser
                        .panel
                        .pending_focus
                        .take()
                        .expect("pending focus checked");
                    if !self.try_dock_focused(id) {
                        crate::controller::hid::diag::diag_info(format!(
                            "ui-diag: browser focus dropped {id}"
                        ));
                    }
                }
            }
            EngineEvent::ScrollOffset { tab, x, y } => {
                let before = self.browser.panel.scroll_offset;
                if before != (x, y) {
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: scroll tab{} ({:.0},{:.0}) -> ({:.0},{:.0}) active={}",
                        tab.0, before.0, before.1, x, y, self.browser.panel.active_tab.0
                    ));
                }
                self.browser.panel.scroll_offset = (x, y);
            }
            EngineEvent::ScrollExtent { extent, .. } => {
                self.browser.panel.page_extent = extent;
            }
            EngineEvent::ActivityChanged { tab: _, phase } => {
                self.browser.panel.engine_activity = phase;
            }
            EngineEvent::HistoryChanged {
                tab: _,
                entries,
                current,
            } => {
                self.browser
                    .panel
                    .session
                    .note_session_history(entries, current);
                // The stack moved under an open selector: clamp the highlight,
                // close if the direction ran dry.
                let dir_is_back = self
                    .browser
                    .panel
                    .page
                    .history_selector
                    .as_ref()
                    .map(|sel| sel.back);
                if let Some(back) = dir_is_back {
                    let panel = &mut self.browser.panel;
                    let len = if back {
                        panel.session.back_entries().len()
                    } else {
                        panel.session.forward_entries().len()
                    };
                    if len == 0 {
                        panel.history_selector = None;
                    } else if let Some(sel) = panel.history_selector.as_mut() {
                        sel.clamp(len);
                    }
                }
            }
            EngineEvent::FocusChanged { tab: _, id } => {
                // Magnet off means never dock — drop any parked target too.
                if !self.browser.panel.cursor.magnet {
                    self.browser.panel.pending_focus = None;
                } else if !self.try_dock_focused(id) {
                    // Not live / mid-flight / stick busy: park for the next
                    // tree (one shot). Logged: a park with no later dock is
                    // the missed-autofocus signature.
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: browser focus parked {id}"
                    ));
                    self.browser.panel.pending_focus = Some(id);
                } else {
                    self.browser.panel.pending_focus = None;
                }
            }
            EngineEvent::LoadFinished {
                tab: _,
                generation,
                result,
            } => match result {
                Ok(()) => {
                    self.browser.panel.session.mark_loaded(generation);
                }
                Err(err) => {
                    self.browser.panel.session.apply_failed(generation, err);
                }
            },
        }
        Task::none()
    }

    /// Background-tab event: same per-tab state updates as the live path,
    /// but never touches chrome (draft, selector UI, docking). Favicons are
    /// the exception — the cache is shared by origin across tabs.
    fn route_background_event(&mut self, tab: crate::browser::engine::TabId, event: EngineEvent) {
        // Favicons need no stored state (shared origin cache) — handle
        // before the stored borrow so the borrow below stays clean.
        if let EngineEvent::FaviconChanged {
            url,
            width,
            height,
            rgba,
            ..
        } = event
        {
            let handle = iced::widget::image::Handle::from_rgba(width, height, rgba);
            self.browser.panel.note_favicon(url, handle);
            return;
        }
        // URL/title changes persist the strip; the borrow ends before the
        // save (which reborrows the panel). Chrome entries are cloned up
        // front so the stored borrow below stays uncontended.
        let mut strip_changed = false;
        let chrome = self.browser.panel.chrome.clone();
        {
            let Some(stored) = self.browser.panel.stored_mut(tab) else {
                return;
            };
            match event {
                EngineEvent::TitleChanged { title, .. } => {
                    stored.page.session.note_title(title);
                    strip_changed = true;
                }
                EngineEvent::UrlChanged { url, .. } => {
                    stored.page.session.note_url(url);
                    strip_changed = true;
                }
                EngineEvent::FaviconChanged { .. } => {}
                EngineEvent::A11yTree { items, .. } => {
                    stored.page.focusables = items;
                    let mut all = stored.page.focusables.clone();
                    all.extend(chrome.iter().map(|(_, f)| f.clone()));
                    stored.page.cursor.retarget(&all);
                }
                EngineEvent::ScrollOffset { tab, x, y } => {
                    let before = stored.page.scroll_offset;
                    if before != (x, y) {
                        crate::controller::hid::diag::diag_info(format!(
                            "ui-diag: bg scroll tab{} ({:.0},{:.0}) -> ({:.0},{:.0})",
                            tab.0, before.0, before.1, x, y
                        ));
                    }
                    stored.page.scroll_offset = (x, y);
                }
                EngineEvent::ScrollExtent { extent, .. } => {
                    stored.page.page_extent = extent;
                }
                EngineEvent::ActivityChanged { phase, .. } => {
                    stored.page.engine_activity = phase;
                }
                EngineEvent::HistoryChanged {
                    entries, current, ..
                } => {
                    stored.page.session.note_session_history(entries, current);
                }
                EngineEvent::FocusChanged { id, .. } => {
                    // Docked when foregrounded (if still live then).
                    stored.page.pending_focus = Some(id);
                }
                EngineEvent::LoadFinished {
                    generation, result, ..
                } => match result {
                    Ok(()) => {
                        stored.page.session.mark_loaded(generation);
                    }
                    Err(err) => {
                        stored.page.session.apply_failed(generation, err);
                    }
                },
                EngineEvent::Frame(_)
                | EngineEvent::FrameSlot { .. }
                | EngineEvent::TabOpened { .. }
                | EngineEvent::TabClosed { .. } => {}
            }
        }
        if strip_changed && self.browser.tabs_restored {
            self.browser.panel.save_tabs();
        }
    }
}

impl App {
    pub(crate) fn refresh_start_rows(&mut self) {
        // Sort persists across edit enter/exit: `sort_mode` always mirrors the
        // prefs sort here, and `cycle_games_sort` stays live while editing, so
        // a mid-edit sort change carries into the browse list on save/cancel.
        self.start_state.sort_mode = self.session.prefs.games_sort_mode;
        // Pin the edit cursor by stable key across the rebuild: toggles,
        // manual adds, sort cycles, and Steam-scan reorders shift checklist
        // indices, so a bare index would drift to a different game mid-edit.
        let edit_pin = if self.start_state.editing {
            self.start_state.selected_game_key()
        } else {
            None
        };
        let rows = if self.start_state.editing {
            self.edit_checklist_rows()
        } else {
            self.display_catalog()
                .iter()
                .map(|entry| {
                    let (remembered, known_missing) = match entry {
                        crate::games::GameEntry::Steam { appid } => (
                            self.games.steam_display_name(*appid),
                            matches!(&self.steam_installed, Some(ids) if !ids.contains(appid)),
                        ),
                        crate::games::GameEntry::Manual { .. } => (None, false),
                    };
                    let mut row = start_view::StartRow::from_entry(
                        entry,
                        &self.steam_by_id,
                        self.steam_scan_pending,
                        remembered,
                        known_missing,
                    );
                    row.played_at_ms = self.effective_played_ms(entry);
                    row.disabled = self.is_entry_disabled(entry, &row);
                    // Pre-flight failures (manifest lingers, files gone) keep
                    // their scanned title but route Cross to View in Steam.
                    if let crate::games::GameEntry::Steam { appid } = entry
                        && self.launch_blocked_steam.contains(appid)
                    {
                        row.view_in_steam = Some(*appid);
                    }
                    row
                })
                .collect()
        };
        self.start_state.set_rows(rows);
        if self.start_state.editing {
            // Keep the cursor on the same checklist game; a removed manual
            // keeps the clamped neighbor. Never fall back to 0 or re-pin the
            // IPC shadow here: the shadow still holds the pre-toggle key while
            // a toggle message is in flight, which would yank the cursor
            // (manual removal's key is already gone → shadow fallback is 0).
            self.start_state
                .select_game_by_play_key_or_keep(edit_pin.as_deref());
        } else if let Some(selection) = self.start_selection_shadow.clone() {
            self.start_state.resolve_start_selection(
                selection.game_key.as_deref(),
                None,
                self.session.prefs.start_selection_persist,
                self.session.prefs.start_selection_mode,
                selection.game_index,
            );
        }
    }

    pub(crate) fn discard_edit_draft(&mut self) {
        self.edit_draft = None;
        self.start_state.editing = false;
        self.start_state.edit_anchor_play_key = None;
        self.start_state.clear_manual();
    }

    pub(crate) fn enter_start_edit(&mut self) -> Task<Message> {
        self.start_state.capture_edit_anchor();
        self.edit_draft = Some(self.games.clone());
        self.start_state.editing = true;
        self.refresh_start_rows();
        let key = self.start_state.edit_anchor_play_key.clone();
        self.start_state.select_game_by_play_key(key.as_deref());
        self.warm_start_art_task()
    }

    pub(crate) fn commit_start_edit(&mut self) -> Task<Message> {
        // Resolve the exit target while still on the checklist rows: keep the
        // cursor game when still toggled-on, else nearest toggled-on above /
        // after (`None` when the draft catalog is empty → empty view shows).
        // The anchor is intentionally NOT used here: the user may have
        // navigated mid-edit and the cursor must persist.
        let target = self.start_state.edit_exit_target_key();
        if let Some(draft) = self.edit_draft.take() {
            self.games = draft;
            self.games.save();
        }
        self.start_state.editing = false;
        self.start_state.edit_anchor_play_key = None;
        self.refresh_start_rows();
        // `refresh_start_rows` re-syncs `sort_mode` from prefs (a mid-edit
        // sort cycle persists), so only the selection needs resolving here.
        self.start_state
            .resolve_edit_exit_selection(target.as_deref());
        self.warm_start_art_task()
    }

    pub(crate) fn cancel_start_edit(&mut self) -> Task<Message> {
        // Cancel discards membership changes but keeps navigation: the cursor
        // game survives when still in the restored catalog, else the pre-edit
        // anchor revives, else the nearest-enabled rule applies.
        let cursor = self.start_state.selected_game_key();
        let anchor = self.start_state.edit_anchor_play_key.take();
        self.discard_edit_draft();
        self.refresh_start_rows();
        let target = self
            .start_state
            .cancel_exit_target_key(cursor.as_deref(), anchor.as_deref());
        self.start_state
            .resolve_edit_exit_selection(target.as_deref());
        self.warm_start_art_task()
    }

    /// True when the edit draft would change the saved games list on
    /// commit (entries-only: background launch stamps and scan-name
    /// refreshes don't count — see `GamesCatalog::entries_differ`).
    pub(crate) fn edit_draft_dirty(&self) -> bool {
        self.edit_draft
            .as_ref()
            .is_some_and(|draft| self.games.entries_differ(draft))
    }

    /// Save-guard choice: discard the draft and leave edit mode.
    pub(crate) fn complete_edit_discard(&mut self) -> Task<Message> {
        self.start_state.clear_edit_confirm();
        self.cancel_start_edit()
    }

    pub(crate) fn cancel_manual_add(&mut self) -> Task<Message> {
        // Cancel drains through the exit tween (form clears when it
        // settles); reduced motion clears instantly.
        self.start_state.dismiss_manual(Instant::now());
        Task::none()
    }

    pub(crate) fn toggle_start_edit(&mut self) -> Option<Task<Message>> {
        if self.start_state.overlay_blocking() || self.start_state.animating() {
            return None;
        }
        Some(if self.start_state.editing {
            self.commit_start_edit()
        } else {
            self.enter_start_edit()
        })
    }

    pub(crate) fn pick_manual_shortcut(&mut self) -> Task<Message> {
        self.start_file_dialog_open = true;
        self.pick_file_dialog(
            "Programs",
            &["exe", "lnk", "url"],
            Message::ManualFilePicked,
        )
    }

    pub(crate) fn pick_manual_icon(&mut self) -> Task<Message> {
        self.start_file_dialog_open = true;
        self.pick_file_dialog(
            "Images",
            &["png", "jpg", "jpeg", "ico", "webp"],
            Message::ManualIconPicked,
        )
    }

    pub(crate) fn begin_manual_edit(&mut self) -> Task<Message> {
        if !self.start_state.editing || self.start_state.overlay_blocking() {
            return Task::none();
        }
        let Some(row) = self
            .start_state
            .rows
            .get(self.start_state.game_selected)
            .cloned()
        else {
            return Task::none();
        };
        let Some(start_view::EditRow::Manual { id }) = row.edit.as_ref() else {
            return Task::none();
        };
        let icon_path = self.edit_catalog().entries.iter().find_map(|e| match e {
            crate::games::GameEntry::Manual { id: mid, icon, .. } if mid == id => {
                icon.as_ref().map(PathBuf::from)
            }
            _ => None,
        });
        let shell_icon = start_view::probe_shell_icon(&row.target);
        self.start_state.arm_manual(
            start_view::ManualAddDraft {
                edit_id: Some(id.clone()),
                target: row.target,
                title: row.title,
                args: row.args,
                icon_path,
                shell_icon,
            },
            Instant::now(),
        );
        Task::none()
    }

    pub(crate) fn on_start_confirm(&mut self) -> Task<Message> {
        // Save guard owns Confirm while open (tap Cross confirms the
        // discard). Display-only drain must not apply twice.
        if self.start_state.edit_confirm.is_some() {
            if self.start_state.edit_confirm_closing() {
                return Task::none();
            }
            self.play_ui_cue(UiSoundKind::Action);
            return self.complete_edit_discard();
        }
        if self.start_state.manual_add.is_some() {
            self.play_ui_cue(UiSoundKind::Action);
            return self.confirm_manual_add();
        }
        // Replace confirm requires hold-Cross / hold-Enter (see pad poll / key hold).
        if self.start_state.replace_confirm.is_some() {
            return Task::none();
        }
        if self.start_state.editing {
            return match self.edit_toggle_selected() {
                Some(task) => {
                    self.play_ui_cue(UiSoundKind::Action);
                    task
                }
                None => Task::none(),
            };
        }
        if self.launch_selected() {
            self.play_ui_cue(UiSoundKind::Action);
            // Same as `on_start_launch`: keep a fresh replace-confirm modal
            // alive instead of closing (and wiping) it on the arming update.
            if self.start_state.replace_confirm.is_some() {
                return Task::none();
            }
            return self.close_start_screen();
        }
        Task::none()
    }

    /// Launch the selected game. Returns true when launch, replace-confirm, or a real attempt ran.
    /// Callers must check `replace_confirm` before closing: a `true` from the
    /// replace arm means "modal armed, stay open", not "launched, close".
    pub(crate) fn launch_selected(&mut self) -> bool {
        let Some(row) = self
            .start_state
            .rows
            .get(self.start_state.game_selected)
            .cloned()
        else {
            return false;
        };

        // Don't relaunch while this row is still closing.
        if self
            .start_state
            .closing_target
            .as_ref()
            .is_some_and(|t| t == &row.target)
        {
            return false;
        }

        // Uninstalled Steam rows stay enabled: Cross opens the game's store
        // page in Steam instead of launching. Fire-and-forget: no running
        // session, badges, or last-played stamp.
        if let Some(appid) = row.view_in_steam {
            return self.open_view_in_steam(appid, &row.title);
        }

        // Disabled rows never launch (missing manual target). Steam rows are
        // never disabled — unlaunchable ones route through `view_in_steam`
        // above — but re-validate at launch time so a stale row (scan race /
        // immediate launch after uninstall) fails gracefully instead of
        // opening Steam / error popup.
        if row.disabled || self.is_row_invalid_now(&row) {
            app_log::warn(format!(
                "launch blocked (no longer installed): {} ({})",
                row.title, row.target
            ));
            // Refresh so the row converts to View in Steam immediately.
            self.refresh_start_rows();
            return false;
        }

        // Steam pre-flight (revoked license / wiped files, e.g. an expired
        // alpha): the manifest can linger while the install dir is missing
        // or empty, and ShellExecute would happily hand off to a Steam error
        // dialog. Route to View in Steam instead, so `Playing` never parks
        // on a game that cannot spawn a process.
        if let Some(appid) = steam_appid_from_target(&row.target)
            && !steam::install_ready_for_appid(appid)
        {
            app_log::warn(format!(
                "launch blocked (steam install not ready): {} (appid {appid})",
                row.title
            ));
            self.launch_blocked_steam.insert(appid);
            self.refresh_start_rows();
            return self.open_view_in_steam(appid, &row.title);
        }

        if let Some(session) = self.running_session.as_ref() {
            if session.matches_target(&row.target) {
                return false;
            }
            if process_match::any_matching_running(&session.match_paths) {
                self.start_state.arm_replace(
                    ReplaceConfirm {
                        running_title: session.title.clone(),
                        next_title: row.title.clone(),
                        next_index: self.start_state.game_selected,
                    },
                    Instant::now(),
                );
                return true;
            }
            self.running_session = None;
            self.sync_launch_badges();
        }

        match launch::launch_target(&row.target, &row.args) {
            Ok(()) => {
                // No last-played stamp here: it lands on the first confirm
                // (process or Steam verdict) in `apply_running_badge_from_images`.
                self.begin_running_session(row.title, row.target, row.play_key);
                true
            }
            Err(err) => {
                app_log::warn(format!("launch failed for {}: {err}", row.target));
                // Immediate-launch race (uninstalled between scan and press):
                // refresh so the row converts to View in Steam.
                self.refresh_start_rows();
                false
            }
        }
    }

    /// Open the Steam store page for an unlaunchable appid.
    /// Returns true when Steam accepted the URI (callers close Start).
    pub(crate) fn open_view_in_steam(&mut self, appid: u32, title: &str) -> bool {
        let uri = steam::store_page_uri(appid);
        match launch::launch_target(&uri, "") {
            Ok(()) => {
                app_log::info(format!("view in steam: {title} ({uri})"));
                true
            }
            Err(err) => {
                app_log::warn(format!("view in steam failed for {uri}: {err}"));
                false
            }
        }
    }
}

impl App {
    pub(crate) fn seed_steam_library_from_cache(&mut self) {
        let Some(cache) = crate::games::steam::load_library_cache() else {
            return;
        };
        if self.games.remember_steam_names(&cache.games) {
            self.games.save();
        }
        self.steam_last_synced_ms = (cache.scanned_at_ms > 0).then_some(cache.scanned_at_ms);
        self.steam_installed = Some(cache.games.iter().map(|g| g.appid).collect());
        self.steam_by_id = cache.games.into_iter().map(|g| (g.appid, g)).collect();
        self.steam_scan_pending = false;
        self.start_state.steam_scan_pending = false;
        self.refresh_start_rows();
    }

    pub(crate) fn refresh_steam_library(&mut self) -> Task<Message> {
        // Metadata only — list / immersive art decode runs after SteamScanDone so the
        // selected game can paint before the rest of the library is warm.
        let now = Instant::now();
        self.steam_scan_started = Some(now);
        self.start_state.begin_steam_scan_banner(now);
        crate::controller::hid::diag::diag_info("ui-diag: steam scan start deferred=1");
        // Floor so chrome spinners stay visible even on a warm/fast refresh.
        let min_ms = crate::ui::start::mode::STEAM_SCAN_MIN_MS;
        let extra_paths: Vec<PathBuf> = self.session.prefs.extra_steam_paths.clone();
        Task::perform(
            async move {
                let started = Instant::now();
                let result =
                    spawn_blocking(move || steam::list_installed_games(&extra_paths)).await;
                let min = Duration::from_millis(min_ms);
                let elapsed = started.elapsed();
                if elapsed < min {
                    let pad = min - elapsed;
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: steam scan pad ms={}",
                        pad.as_millis()
                    ));
                    delay(pad).await;
                }
                result
            },
            |result| match result {
                Ok(Ok(games)) => Message::SteamScanDone(Ok(games)),
                Ok(Err(err)) => Message::SteamScanDone(Err(err)),
                Err(err) => Message::SteamScanDone(Err(err)),
            },
        )
    }

    /// Kick the first live Steam scan/refresh this process. Idempotent.
    pub(crate) fn maybe_kick_deferred_steam_scan(&mut self) -> Task<Message> {
        if self.steam_library_refreshed || self.steam_scan_started.is_some() {
            return Task::none();
        }
        self.refresh_steam_library()
    }

    /// Registry-watch callback: badges refresh every time, but a full
    /// library rescan only runs when the installed set actually changed.
    ///
    /// The `Apps` key fires on `Running` flips too (every launch/exit), so
    /// an unconditional rescan would re-walk the libraries twice per play
    /// session. Comparing the cheap registry membership snapshot against
    /// the last successful scan filters those out; installs, uninstalls,
    /// and license changes still rescan within a second or so.
    pub(crate) fn on_steam_apps_changed(&mut self) -> Task<Message> {
        let mut task = self.refresh_running_badge_inner(true);
        if let Some(resync) = self.steam_library_resync_task() {
            task = task.chain(resync);
        }
        task
    }

    /// Rescan when the registry installed-set disagrees with the last scan.
    ///
    /// `None` when already current, still unknown (first scan owns that),
    /// or a scan is already in flight. Besides the registry watcher, Start
    /// open calls this: changes made while the app was closed (or while no
    /// watcher event fired) would otherwise stay stale, since the deferred
    /// boot scan runs only once per process.
    pub(crate) fn steam_library_resync_task(&mut self) -> Option<Task<Message>> {
        if self.steam_installed.is_none() || self.steam_scan_started.is_some() {
            return None;
        }
        let mut registry = steam::registry_installed_appids();
        registry.sort_unstable();
        let mut known = self.steam_installed.clone().unwrap_or_default();
        known.sort_unstable();
        if registry == known {
            return None;
        }
        app_log::info(format!(
            "steam library changed: registry installed={} known={} — rescanning",
            registry.len(),
            known.len()
        ));
        Some(self.refresh_steam_library())
    }

    pub(crate) fn on_steam_scan_done(
        &mut self,
        result: AppResult<Vec<SteamGame>>,
    ) -> Task<Message> {
        let scan_ms = self
            .steam_scan_started
            .take()
            .map(|t| t.elapsed().as_millis())
            .unwrap_or(0);
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: steam scan done ms={scan_ms} ok={}",
            u8::from(result.is_ok())
        ));
        self.steam_library_refreshed = true;
        match result {
            Ok(games) => {
                if self.games.remember_steam_names(&games) {
                    self.games.save();
                }
                self.steam_last_synced_ms = Some(steam::library_now_ms());
                self.steam_installed = Some(games.iter().map(|g| g.appid).collect());
                self.steam_by_id = games.iter().map(|g| (g.appid, g.clone())).collect();
                // A fresh scan is ground truth again: drop pre-flight blocks
                // (reinstall / license restored re-enables the row).
                self.launch_blocked_steam.clear();
                let root = steam::steam_root().ok().flatten();
                crate::games::steam::save_library_cache(&games, root.as_deref());
            }
            Err(err) => {
                steam::warn_scan_error(&err.to_string());
                // Keep prior successful install list if any; otherwise stay Unknown so
                // curated Steam rows remain visible.
            }
        }
        self.steam_scan_pending = false;
        self.start_state.steam_scan_pending = false;
        let now = Instant::now();
        self.start_state.note_steam_scan_banner_success(now);
        self.match_path_cache.clear();
        self.refresh_start_rows();
        // Rows just rebuilt (order may have changed): while the restore
        // settle window is fresh, re-pin the persisted selection by key
        // (index clamping alone can leave the highlight on the wrong row).
        // Row-independent state (drawer, config) is untouched.
        let mut task = self.prepare_immersive_library_task();
        if self.start_visible
            && self.session.prefs.start_selection_persist
            && self
                .scroll_restore_until
                .is_some_and(|until| Instant::now() < until)
            && let Some(shadow) = self.start_selection_shadow.clone()
            && self.apply_start_selection(&shadow)
        {
            task = task.chain(self.warm_start_art_task());
        }
        task
    }

    /// Post a single-flight art job for the current selection (cheap; no Task spawn).
    pub(crate) fn request_start_art(&mut self) {
        // Linear strip window — Games no longer wraps.
        self.request_start_art_circular(false);
    }

    /// Like [`Self::request_start_art`] with an explicit circular/linear near window.
    pub(crate) fn request_start_art_circular(&mut self, circular: bool) {
        let generation = crate::ui::start::icon_cache::bump_art_warm_generation();
        let (selected_hero, selected_shell, selected_backdrop) =
            self.start_state.immersive_priority_paths();
        let near = self.start_state.art_near_set_circular(circular);
        let prefetch = self.start_state.art_retain_set_circular(circular);
        self.start_state.art_awaiting_paint = true;
        self.art.submit(crate::ui::start::art_worker::ArtJob {
            generation,
            selected_backdrop,
            selected_hero,
            selected_shell,
            near,
            prefetch,
        });
    }

    pub(crate) fn warm_start_art_task(&mut self) -> Task<Message> {
        self.request_start_art();
        Task::none()
    }

    /// After open: skip gen bump when selection art is ready or still decoding.
    pub(crate) fn warm_immersive_art_task(&mut self) -> Task<Message> {
        if self.start_state.enter_art_sync_ready() {
            crate::controller::hid::diag::diag_info("ui-diag: art warm skip (open already sync)");
            return Task::none();
        }
        if self.start_state.art_awaiting_paint {
            crate::controller::hid::diag::diag_info(
                "ui-diag: art warm skip (open still in flight)",
            );
            return Task::none();
        }
        self.request_start_art();
        Task::none()
    }

    pub(crate) fn on_manual_file_picked(&mut self, path: Option<PathBuf>) -> Task<Message> {
        self.start_file_dialog_open = false;
        let Some(path) = path else {
            return Task::none();
        };
        if !self.start_state.editing {
            return Task::none();
        }
        let target = path.to_string_lossy().into_owned();
        let title = games::title_from_target(&target);
        let shell_icon = start_view::probe_shell_icon(&target);
        self.start_state.arm_manual(
            start_view::ManualAddDraft {
                edit_id: None,
                target,
                title,
                args: String::new(),
                icon_path: None,
                shell_icon,
            },
            Instant::now(),
        );
        Task::none()
    }

    pub(crate) fn on_manual_icon_picked(&mut self, path: Option<PathBuf>) -> Task<Message> {
        self.start_file_dialog_open = false;
        let Some(draft) = self.start_state.manual_add.as_mut() else {
            return Task::none();
        };
        if let Some(path) = path {
            draft.icon_path = Some(path);
        }
        Task::none()
    }

    pub(crate) fn on_steam_path_picked(&mut self, path: Option<PathBuf>) -> Task<Message> {
        let Some(path) = path else {
            return Task::none();
        };
        if !steam::validate_steam_library_path(&path) {
            self.configure_state.steam_path_error =
                Some("No Steam library found in that folder".to_string());
            return Task::none();
        }
        if self
            .session
            .prefs
            .extra_steam_paths
            .iter()
            .any(|existing| existing == &path)
        {
            self.configure_state.steam_path_error =
                Some("That folder is already in the list".to_string());
            return Task::none();
        }
        self.configure_state.steam_path_error = None;
        self.session.prefs.extra_steam_paths.push(path);
        self.save_prefs();
        self.refresh_steam_library()
    }
}

impl App {
    /// Row-aware settings nav cue: splits sound/haptic so the pre-flip nav
    /// cue never double-plays the modality the apply side previews after
    /// saving (volume rows → sound preview, strength rows → haptic preview).
    /// Sound rows play haptic-only here; haptic rows play sound-only.
    pub(crate) fn play_start_settings_cue(
        &self,
        row: crate::ui::start::settings::SettingsRow,
        kind: UiSoundKind,
    ) {
        match crate::ui::start::settings::settings_nav_cue(row) {
            crate::ui::start::settings::SettingsCue::Full => self.play_ui_cue(kind),
            crate::ui::start::settings::SettingsCue::HapticOnly => self.play_ui_haptic(kind),
            crate::ui::start::settings::SettingsCue::SoundOnly => self.play_ui_sound(kind),
        }
    }

    pub(crate) fn cancel_start_holds(&mut self) {
        self.pad_nav.cancel_holds();
        self.pad_nav.cancel_held_face_releases();
        self.keyboard_cross_hold.cancel();
        self.start_state.triangle_progress = 0.0;
        self.start_state.cross_progress = 0.0;
    }

    pub(crate) fn sync_start_held(&mut self) {
        let mut held = self.pad_held;
        if self.confirm_key_held {
            held.cross = true;
        }
        if self.cancel_key_held {
            held.circle = true;
        }
        self.start_state.held = held;
    }
}

impl App {
    pub(crate) fn on_pad_input(&mut self, edge: crate::domain::pad::InputEdge) -> Task<Message> {
        let poll_started = Instant::now();
        let start_open = self.start_visible;
        // Edge transport is lossy by design (try_send drops when full): log
        // seq gaps while Start is open so silent d-pad losses are visible.
        // (Seq also advances on unpublished cold snapshots, so a gap at open
        // while input-hot flaps is expected; steady-state gaps mean drops.)
        if start_open {
            if let Some(prev) = self.last_edge_seq
                && edge.seq > prev + 1
            {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: pad-input edge gap dropped={} expected={} got={} reason={}",
                    edge.seq - prev - 1,
                    prev + 1,
                    edge.seq,
                    edge.reason.as_str(),
                ));
            }
            self.last_edge_seq = Some(edge.seq);
        } else {
            self.last_edge_seq = None;
        }
        let meta = Some(edge.meta());
        let readings = edge.readings;

        // Hub is modal: while open it owns the pad; the app underneath keeps
        // its last frame but gets no nav.
        if self.hub_visible {
            return self.on_hub_pad(&readings);
        }
        // The hub may have just closed on a face press. Swallow pad input
        // until that press is fully released so it cannot also dismiss the
        // Start/Browser popup underneath.
        if let Some(until) = self.hub_face_latch_until {
            let face_held = readings
                .iter()
                .any(|r| r.sample.cross || r.sample.circle || r.sample.square || r.sample.triangle);
            if face_held && Instant::now() < until {
                return Task::none();
            }
            self.hub_face_latch_until = None;
        }
        if self.browser_visible {
            return self.on_browser_pad(&readings);
        }

        if start_open {
            if let Some(prev) = self.last_pad_poll_at {
                let gap_ms = prev.elapsed().as_millis();
                if gap_ms >= PAD_POLL_STALL_MS {
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: pad-input stall gap_ms={gap_ms}"
                    ));
                }
            }
            self.last_pad_poll_at = Some(poll_started);

            if !self.start_nav_ready
                && !self.nav_not_ready_warned
                && self
                    .start_opened_at
                    .is_some_and(|t| t.elapsed().as_millis() >= START_NAV_NOT_READY_MS)
            {
                let for_ms = self
                    .start_opened_at
                    .map(|t| t.elapsed().as_millis())
                    .unwrap_or(0);
                app_log::warn(format!("start-nav: not ready for_ms={for_ms}"));
                self.nav_not_ready_warned = true;
            }
        }

        let pad_input_open =
            self.configure_window.is_some() && self.configure_state.section == Section::PadInput;
        let listening = self.wants_pad_input();

        if pad_input_open {
            self.apply_pad_input_panel_from_readings(&readings);
        }

        if !listening {
            // Start is closed and the PadInput tab is not open: the only
            // live consumer is the reopen chord (PS press to open Start).
            // Edges only arrive here while pads are connected, so this is
            // exactly the "PS to open" path — never drop it.
            if !start_open {
                return self.on_reopen_gesture(&readings);
            }
            return Task::none();
        }

        // Continue with the same body as the old on_pad_poll from start_open onward.
        self.on_pad_readings(readings, meta, start_open)
    }

    pub(crate) fn on_pad_readings(
        &mut self,
        readings: Vec<start_input::NavReading>,
        meta: Option<start_input::SnapshotMeta>,
        start_open: bool,
    ) -> Task<Message> {
        if start_open {
            if !self.start_nav_ready {
                return Task::none();
            }
            if readings.is_empty() {
                if let Some(meta) = &meta {
                    let age_ms = meta.published_at.elapsed().as_millis();
                    if self.nav_missing_since.is_none() {
                        self.nav_missing_since = Some(Instant::now());
                    }
                    if !self.nav_missing_warned && !self.session.controllers.is_empty() {
                        app_log::warn(format!(
                            "start-nav: no hid-worker input snapshot yet; keyboard/mouse still work reason={} age_ms={age_ms} seq={}",
                            meta.reason.as_str(),
                            meta.seq
                        ));
                        self.nav_missing_warned = true;
                    }
                }
                return Task::none();
            }

            if let Some(since) = self.nav_missing_since.take() {
                let gap_ms = since.elapsed().as_millis();
                let reason = meta.as_ref().map(|m| m.reason.as_str()).unwrap_or("-");
                app_log::info(format!(
                    "start-nav: snapshot restored gap_ms={gap_ms} pads={} reason={reason}",
                    readings.len()
                ));
                self.nav_missing_warned = false;
            }

            if let Some(meta) = &meta {
                let age_ms = meta.published_at.elapsed().as_millis();
                if age_ms >= SNAPSHOT_STALE_MS {
                    if !self.nav_stale_warned {
                        app_log::warn(format!(
                            "start-nav: snapshot stale age_ms={age_ms} seq={} reason={}",
                            meta.seq,
                            meta.reason.as_str()
                        ));
                        self.nav_stale_warned = true;
                    }
                } else {
                    self.nav_stale_warned = false;
                }
            }

            return self.handle_start_nav_readings(&readings);
        }

        self.on_reopen_gesture(&readings)
    }

    /// True when left/right should scroll the Games strip: horizontal layout
    /// with no overlays (settings / replace-confirm / manual-add) open. Edit
    /// mode is a mode, not an overlay, so it stays included: left/right move
    /// selection (all rows are enabled in the checklist) while Up/Down are
    /// suppressed.
    pub(crate) fn immersive_horizontal_games_nav(&self) -> bool {
        matches!(
            self.session.prefs.start_screen_immersive_layout,
            crate::persist::prefs::ImmersiveLayout::Horizontal
        ) && !self.start_state.settings.open
            && !self.browser.panel.session.is_open()
            && self.start_state.replace_confirm.is_none()
            && self.start_state.manual_add.is_none()
    }

    pub(crate) fn handle_start_nav_readings(
        &mut self,
        readings: &[start_input::NavReading],
    ) -> Task<Message> {
        self.nav_missing_warned = false;

        let now = Instant::now();
        let pad_active = readings
            .iter()
            .any(|r| !start_input::sample_nav_resting(&r.sample));

        // Sleep / sleep-wake: pad may only begin wake — no nav, dock, cancel, or cues.
        if self.start_state.idle_pad_gated() {
            if pad_active {
                self.start_last_activity = now;
                self.wake_start_idle(now, true);
                crate::controller::hid::diag::diag_info(
                    "ui-diag: start idle pad gated (sleep wake)",
                );
            }
            return Task::none();
        }

        // Cold ambient: no pad nav / launch (keyboard Escape still closes).
        if self.start_state.cold_load_active {
            if pad_active {
                self.note_start_activity();
            }
            return Task::none();
        }

        if pad_active {
            self.note_start_activity();
        }

        // PS while open toggles Start closed (PS opens when closed); a close
        // task preempts pad nav for this tick.
        if let Some(task) = self.toggle_hub_on_reopen_chord(readings) {
            return task;
        }

        // Debug R3: hide/show all chrome while tuning (pad-only; R3 carries
        // no NavAction). Tuning-scoped, so it never fires in normal chrome.
        #[cfg(debug_assertions)]
        if let Some(task) = self.toggle_ambient_chrome_from_r3(readings) {
            return task;
        }

        let animating = self.start_state.animating();
        // Mid-anim: skip the running-badge rebuild (it hitches the UI thread).
        let badge_task = if !animating {
            let badge_started = Instant::now();
            let badge = self.refresh_running_badge();
            let badge_ms = badge_started.elapsed().as_millis();
            if badge_ms >= PAD_POLL_SLOW_MS {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: pad-poll slow badge_ms={badge_ms}"
                ));
            }
            badge
        } else {
            Task::none()
        };

        self.start_state.tick_anim(now);

        let replace_confirm = self.start_state.replace_confirm.is_some();
        let manual_add = self.start_state.manual_add.is_some();
        let save_modal = self.start_state.edit_confirm.is_some();
        let settings_open = self.start_state.settings.open;
        let allow_nav_move = start_nav_move_allowed(&self.start_state);
        let editing = self.start_state.editing;
        // Only when the selected row shows Close game (running target match).
        let hold_cross_close = !editing
            && !replace_confirm
            && !manual_add
            && !save_modal
            && !settings_open
            && !animating
            && self.start_state.running_target.as_ref().is_some_and(|t| {
                self.start_state
                    .rows
                    .get(self.start_state.game_selected)
                    .is_some_and(|row| &row.target == t)
            });
        // Triangle hold power-off lives in the PS hub controllers row.
        let hold_triangle_power = false;
        // Left/right move. Settings ignores Left/Right (form controls are
        // mouse-driven); Up/Down moves focus.
        let horizontal_nav = true;
        // Horizontal Games strip: left/right scroll games.
        let horizontal_games_nav = self.immersive_horizontal_games_nav();
        let tick = self.pad_nav.tick(
            readings,
            now,
            allow_nav_move,
            horizontal_nav,
            horizontal_games_nav,
            replace_confirm && !animating,
            editing,
            hold_cross_close,
            hold_triangle_power,
        );
        if let Some(diag) = &tick.diag {
            self.log_start_nav_diag(diag, readings.len());
        }
        if tick.unarmed_pads > 0 && tick.armed_pads == 0 {
            if !self.nav_unarmed_warned {
                if let Some(diag) = &tick.diag {
                    app_log::warn(format!(
                        "start-nav: pad unarmed waiting for rest (stick_x={:.3} stick_y={:.3} dpad_l={} dpad_r={} cross={} circle={})",
                        diag.sample.stick_x,
                        diag.sample.stick_y,
                        u8::from(diag.sample.dpad_left),
                        u8::from(diag.sample.dpad_right),
                        u8::from(diag.sample.cross),
                        u8::from(diag.sample.circle),
                    ));
                }
                self.nav_unarmed_warned = true;
            }
        } else if tick.armed_pads > 0 {
            self.nav_unarmed_warned = false;
        }

        self.pad_held = tick.held;
        self.sync_start_held();

        // Pad-originated cues rumble the pad that fired (or completed a hold).
        let haptic_pad = tick
            .action_pad
            .as_ref()
            .or(tick.cross_completed_pad.as_ref())
            .or(tick.triangle_completed_pad.as_ref());
        self.set_haptic_pad_from_id(haptic_pad);

        let nav_task = if self.start_state.ambient_tuning_active() {
            // Debug tuning panel owns pad nav while open: Up/Down focus,
            // Left/Right nudge, Cross confirm, Circle done. Holds reset so
            // game close / power-off cannot arm under the panel, and
            // overlay_blocking already parks the message-driven game paths.
            self.start_state.cross_progress = 0.0;
            self.start_state.triangle_progress = 0.0;
            self.keyboard_cross_hold.reset();
            self.start_state.tick_hint_anims(now);
            self.on_ambient_tuning_nav(tick.action)
        } else if save_modal {
            // Save guard demands an explicit choice: park nav, holds, and
            // surface toggles (Start/Options, Create) so nothing bypasses
            // the modal; only Confirm (discard) and Cancel (back to edit)
            // stay live.
            self.start_state.cross_progress = 0.0;
            self.start_state.triangle_progress = 0.0;
            self.keyboard_cross_hold.reset();
            self.start_state.tick_hint_anims(now);
            if let Some(action) = tick.action {
                match action {
                    NavAction::Confirm | NavAction::Cancel => self.on_start_message(match action {
                        NavAction::Confirm => StartMessage::Confirm,
                        _ => StartMessage::Close,
                    }),
                    _ => Task::none(),
                }
            } else {
                Task::none()
            }
        } else if tick.action == Some(NavAction::ToggleSettings) {
            self.start_state.tick_hint_anims(now);
            self.on_start_message(StartMessage::ToggleSettings)
        } else if settings_open {
            self.start_state.cross_progress = 0.0;
            self.start_state.triangle_progress = 0.0;
            self.keyboard_cross_hold.reset();
            self.start_state.tick_hint_anims(now);
            self.on_start_settings_nav(tick.action)
        } else if animating {
            self.start_state.tick_hint_anims(now);
            if let Some(action) = tick.action {
                match action {
                    NavAction::Up => self.on_start_message(StartMessage::MoveUp),
                    NavAction::Down => self.on_start_message(StartMessage::MoveDown),
                    // Left/Right while chrome animates: move the selection.
                    NavAction::Left => self.on_start_message(StartMessage::MoveUp),
                    NavAction::Right => self.on_start_message(StartMessage::MoveDown),
                    NavAction::Cancel => self.on_start_message(StartMessage::Close),
                    _ => Task::none(),
                }
            } else {
                Task::none()
            }
        } else if replace_confirm {
            let mut cross_progress = tick.cross_progress;
            let mut cross_completed = tick.cross_completed;
            let (k_progress, k_completed) =
                self.keyboard_cross_hold.update(self.confirm_key_held, now);
            cross_progress = cross_progress.max(k_progress);
            cross_completed = cross_completed || k_completed;
            self.start_state.cross_progress = cross_progress;
            self.start_state.triangle_progress = 0.0;
            self.start_state.tick_hint_anims(now);
            if cross_completed {
                // Keyboard-only complete: no pad serial → no rumble.
                if k_completed && tick.cross_completed_pad.is_none() {
                    self.haptic_pad_serial = None;
                }
                self.play_ui_cue(UiSoundKind::Hold);
                self.complete_replace_confirm()
            } else if tick.action == Some(NavAction::Cancel) {
                self.keyboard_cross_hold.cancel();
                self.start_state.cross_progress = 0.0;
                self.on_start_message(StartMessage::Close)
            } else {
                Task::none()
            }
        } else if manual_add {
            self.start_state.cross_progress = 0.0;
            self.start_state.triangle_progress = 0.0;
            self.keyboard_cross_hold.reset();
            self.start_state.tick_hint_anims(now);
            if let Some(action) = tick.action {
                match action {
                    NavAction::Confirm | NavAction::Cancel => self.on_start_message(match action {
                        NavAction::Confirm => StartMessage::Confirm,
                        _ => StartMessage::Close,
                    }),
                    _ => Task::none(),
                }
            } else {
                Task::none()
            }
        } else {
            self.keyboard_cross_hold.reset();

            let mut close_task = Task::none();
            if hold_cross_close {
                self.start_state.cross_progress = tick.cross_progress;
                if tick.cross_completed {
                    self.play_ui_cue(UiSoundKind::Hold);
                    close_task = self.close_running_game_if_selected();
                }
            } else {
                self.start_state.cross_progress = 0.0;
            }

            if hold_triangle_power {
                self.start_state.triangle_progress = tick.triangle_progress;
            } else {
                self.start_state.triangle_progress = 0.0;
            }

            self.start_state.tick_hint_anims(now);

            if tick.cross_completed && hold_cross_close {
                // Hold-close already handled; do not also Confirm/Launch.
                close_task
            } else if let Some(action) = tick.action {
                match action {
                    NavAction::Up => self.on_start_message(StartMessage::MoveUp),
                    NavAction::Down => self.on_start_message(StartMessage::MoveDown),
                    NavAction::Confirm => self.on_start_message(StartMessage::Confirm),
                    NavAction::Cancel => self.on_start_message(StartMessage::Close),
                    // Left/Right: ignored on games.
                    NavAction::Left | NavAction::Right => Task::none(),
                    // Bumpers are browser-only (history); L3 toggles the
                    // browser cursor magnet — all ignored on games.
                    NavAction::BumperLeft
                    | NavAction::BumperRight
                    | NavAction::BumperHoldLeft
                    | NavAction::BumperHoldRight => Task::none(),
                    NavAction::ToggleMagnetism => Task::none(),
                    NavAction::ToggleControllers => Task::none(),
                    NavAction::ToggleEdit => self.on_start_message(StartMessage::ToggleEdit),
                    NavAction::CycleSort => self.on_start_message(StartMessage::CycleSort),
                    NavAction::Triangle if self.start_state.editing => {
                        self.on_start_message(StartMessage::EditManual)
                    }
                    NavAction::Triangle => Task::none(),
                    NavAction::ToggleSettings => {
                        self.on_start_message(StartMessage::ToggleSettings)
                    }
                }
            } else {
                Task::none()
            }
        };

        Task::batch([badge_task, nav_task])
    }
}

impl App {
    pub(crate) fn on_reopen_gesture(
        &mut self,
        readings: &[start_input::NavReading],
    ) -> Task<Message> {
        if readings.is_empty() {
            return Task::none();
        }
        // 0→1 auto-open owns this window; a PS power-on chord must not open Start
        // while the connect toast is still Placing / SlidingIn.
        if self.toast_machine.suppresses_reopen_gesture() {
            crate::controller::hid::diag::diag_info(
                "ui-diag: reopen gesture suppressed (toast OpenStart pending)",
            );
            return Task::none();
        }
        if !self.session.prefs.start_screen_enabled {
            return Task::none();
        }
        let required = gesture::default_gesture();
        let chord_held = chord_held_on_any_pad(&required, readings);
        let now = Instant::now();
        let (tick, glitch) = self.chord_release_gate.tick(chord_held, now);
        if glitch {
            crate::controller::hid::diag::diag_info("ui-diag: reopen chord glitch ignored");
        }
        match tick {
            ChordReleaseTick::Blocked => {
                if chord_held {
                    self.gesture_detectors.consume_pending_match(readings);
                }
                return Task::none();
            }
            ChordReleaseTick::Cleared => {
                // Absent sample disarms detectors; next held rising edge may fire.
                let _ = self.gesture_detectors.update(&required, readings);
                return Task::none();
            }
            ChordReleaseTick::Open => {}
        }
        if self.gesture_detectors.update(&required, readings) {
            let held = start_input::preferred_reading(readings)
                .map(|r| {
                    gesture::format_gesture(&r.sample.held.iter().copied().collect::<Vec<_>>())
                })
                .unwrap_or_else(|| "none".to_string());
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: reopen gesture fire held={held}"
            ));
            self.open_hub()
        } else {
            Task::none()
        }
    }

    /// Rising-edge PS chord while Start is open opens the hub. The hub is
    /// the only PS target now: PS never closes Start directly. True modals
    /// (replace/edit/manual dialogs, open or animating settings, ambient
    /// tuning) swallow the chord so PS cannot strand them mid-dialog. The
    /// release gate consumes the sticky hold so the opening press cannot
    /// immediately re-fire.
    pub(crate) fn toggle_hub_on_reopen_chord(
        &mut self,
        readings: &[start_input::NavReading],
    ) -> Option<Task<Message>> {
        if !self.start_visible || self.hub_visible || readings.is_empty() {
            return None;
        }
        let modal_blocking = self.start_state.replace_confirm.is_some()
            || self.start_state.manual_add.is_some()
            || self.start_state.edit_confirm.is_some()
            || self.start_state.settings.open
            || self.start_state.settings.anim.is_some()
            || self.start_state.ambient_tuning_active();
        if modal_blocking {
            return None;
        }
        let required = gesture::default_gesture();
        let chord_held = chord_held_on_any_pad(&required, readings);
        let now = Instant::now();
        let (tick, glitch) = self.chord_release_gate.tick(chord_held, now);
        if glitch {
            crate::controller::hid::diag::diag_info("ui-diag: reopen chord glitch ignored");
        }
        match tick {
            ChordReleaseTick::Blocked => {
                if chord_held {
                    self.gesture_detectors.consume_pending_match(readings);
                }
                None
            }
            ChordReleaseTick::Cleared => {
                // Absent sample disarms detectors; next held rising edge may fire.
                let _ = self.gesture_detectors.update(&required, readings);
                None
            }
            ChordReleaseTick::Open => {
                // Rising-edge guard: the gate reports Open on every tick once
                // unlatched, even with the chord absent. Only a genuine new
                // press (detector rising edge) opens the hub.
                if self.gesture_detectors.update(&required, readings) {
                    crate::controller::hid::diag::diag_info("ui-diag: PS opens hub");
                    Some(self.toggle_hub())
                } else {
                    None
                }
            }
        }
    }
}

impl App {
    /// Apply a stored selection to the start lists (keys → indices).
    /// Game-axis resolve honors the persistence prefs; the controller axis is
    /// never gated. Returns true when an index moved.
    pub(crate) fn apply_start_selection(
        &mut self,
        selection: &crate::session::StartSelection,
    ) -> bool {
        self.start_selection_shadow = Some(selection.clone());
        self.start_state.resolve_start_selection(
            selection.game_key.as_deref(),
            selection.controller_key.as_deref(),
            self.session.prefs.start_selection_persist,
            self.session.prefs.start_selection_mode,
            selection.game_index,
        )
    }

    pub(crate) fn start_settings_snapshot(
        &self,
    ) -> crate::ui::start::settings::StartSettingsSnapshot {
        crate::ui::start::settings::StartSettingsSnapshot {
            auto_open: self.session.prefs.start_screen_auto_open,
            selection_persist: self.session.prefs.start_selection_persist,
            selection_mode: self.session.prefs.start_selection_mode,
            clock_enabled: self.session.prefs.start_screen_clock_enabled,
            immersive_layout: self.session.prefs.start_screen_immersive_layout,
            navbar: self.session.prefs.start_screen_navbar,
            reduced_motion: self.session.prefs.reduced_motion,
            inactive_secs: self.session.prefs.start_screen_inactive_secs,
            sleep_secs: self.session.prefs.start_screen_sleep_secs,
            inactive_dim_percent: self.session.prefs.start_screen_inactive_dim_percent,
            sounds_enabled: self.session.prefs.start_screen_sounds_enabled,
            sound_volume: self.session.prefs.start_screen_sound_volume,
            haptics_enabled: self.session.prefs.start_screen_haptics_enabled,
            haptics_strength: self.session.prefs.start_screen_haptics_strength,
            perf_throttle_immersive: self.session.prefs.perf_throttle_immersive,
            perf_antialiasing: self.session.prefs.perf_antialiasing,
            perf_vsync: self.session.prefs.perf_vsync,
            perf_aa_boot: self.renderer_boot_aa,
            perf_vsync_boot: self.renderer_boot_vsync,
            perf_reload_queued: self.renderer_reload_queued,
            show_uninstalled_games: self.session.prefs.show_uninstalled_games_enabled(),
            steam_syncing: self.steam_scan_started.is_some(),
        }
    }

    pub(crate) fn apply_show_uninstalled_games(&mut self, enabled: bool) -> Task<Message> {
        if self.session.prefs.show_uninstalled_games_enabled() != enabled {
            // An explicit choice: record `Some` so later default changes
            // never override it.
            self.session.prefs.show_uninstalled_games = Some(enabled);
            self.save_prefs();
            self.refresh_start_rows();
        }
        Task::none()
    }

    /// Manual library rescan from Settings (drawer or Configure). No-op
    /// while a scan is already in flight.
    pub(crate) fn sync_steam_library_now(&mut self) -> Task<Message> {
        if self.steam_scan_started.is_some() {
            return Task::none();
        }
        self.refresh_steam_library()
    }

    pub(crate) fn apply_start_screen_clock(&mut self, enabled: bool) -> Task<Message> {
        self.session.prefs.start_screen_clock_enabled = enabled;
        self.save_prefs();
        Task::none()
    }

    pub(crate) fn apply_start_screen_immersive_layout(
        &mut self,
        layout: crate::persist::prefs::ImmersiveLayout,
    ) -> Task<Message> {
        // Picking from the menu collapses it, even when re-picking the
        // current value.
        self.start_state.settings.close_menus();
        if self.session.prefs.start_screen_immersive_layout == layout {
            return Task::none();
        }
        let from = self.session.prefs.start_screen_immersive_layout;
        self.session.prefs.start_screen_immersive_layout = layout;
        self.save_prefs();
        // Covers both the Configure pick-list and the Start drawer dropdown:
        // animate only on an actual change, showing the old orientation
        // through the first half of the transition.
        self.start_state
            .begin_layout_transition(from, layout, Instant::now());
        Task::none()
    }

    pub(crate) fn apply_start_screen_navbar(&mut self, navbar: NavbarVisibility) -> Task<Message> {
        // Picking from the menu collapses it, even when re-picking the
        // current value.
        self.start_state.settings.close_menus();
        if self.session.prefs.start_screen_navbar == navbar {
            return Task::none();
        }
        self.session.prefs.start_screen_navbar = navbar;
        self.save_prefs();
        // Pin (or release) the position rail immediately; while pinned the
        // show path stays (1, 1) and the tick hide-timer is skipped.
        self.start_state
            .sync_navbar_pinned(matches!(navbar, NavbarVisibility::AlwaysOn), Instant::now());
        Task::none()
    }

    pub(crate) fn apply_start_screen_reduced_motion(
        &mut self,
        mode: ReducedMotion,
    ) -> Task<Message> {
        // Picking from the menu collapses it, even when re-picking the
        // current value.
        self.start_state.settings.close_menus();
        if self.session.prefs.reduced_motion == mode {
            return Task::none();
        }
        self.session.prefs.reduced_motion = mode;
        self.save_prefs();
        // Gate (or release) movement immediately; enabling also settles any
        // in-flight chase to its target.
        self.start_state
            .sync_reduced_motion(crate::ui::motion::reduced_motion_effective(mode));
        Task::none()
    }

    pub(crate) fn apply_start_screen_inactive_secs(&mut self, secs: u32) -> Task<Message> {
        let (inactive, sleep) =
            clamp_start_screen_idle_timeouts(secs, self.session.prefs.start_screen_sleep_secs);
        if self.session.prefs.start_screen_inactive_secs != inactive
            || self.session.prefs.start_screen_sleep_secs != sleep
        {
            self.session.prefs.start_screen_inactive_secs = inactive;
            self.session.prefs.start_screen_sleep_secs = sleep;
            self.save_prefs();
        }
        Task::none()
    }

    pub(crate) fn apply_start_screen_sleep_secs(&mut self, secs: u32) -> Task<Message> {
        let (inactive, sleep) =
            clamp_start_screen_idle_timeouts(self.session.prefs.start_screen_inactive_secs, secs);
        if self.session.prefs.start_screen_inactive_secs != inactive
            || self.session.prefs.start_screen_sleep_secs != sleep
        {
            self.session.prefs.start_screen_inactive_secs = inactive;
            self.session.prefs.start_screen_sleep_secs = sleep;
            self.save_prefs();
        }
        Task::none()
    }

    pub(crate) fn apply_start_screen_inactive_dim_percent(&mut self, percent: u8) -> Task<Message> {
        let percent = clamp_start_screen_inactive_dim_percent(percent);
        if self.session.prefs.start_screen_inactive_dim_percent != percent {
            self.session.prefs.start_screen_inactive_dim_percent = percent;
            self.save_prefs();
        }
        Task::none()
    }

    pub(crate) fn apply_start_screen_auto_open(&mut self, mode: StartAutoOpen) -> Task<Message> {
        // Picking from the menu collapses it, even when re-picking the
        // current value.
        self.start_state.settings.close_menus();
        let old = self.session.prefs.start_screen_auto_open;
        if old == mode {
            return Task::none();
        }
        self.session.prefs.start_screen_auto_open = mode;
        self.save_prefs();
        if !mode.auto_opens() {
            // Disabling auto-open clears any latched auto-open; never opens
            // or closes here.
            self.session.auto_open_pending = false;
            return Task::none();
        }
        if mode.includes_usb() == old.includes_usb() {
            return Task::none();
        }
        // Scope flip may CloseStart when USB pads no longer qualify; never opens.
        let ctx = crate::session::ApplyContext {
            hub_visible: self.hub_visible,
            fullscreen: start_input::foreground_is_exclusive_fullscreen(),
            now: Instant::now(),
            lightbar_enabled: self.claims.is_enabled(),
        };
        let effects = self.session.reevaluate_start_presence(ctx);
        self.apply_session_effects(effects)
    }

    /// Game-axis selection persistence master switch (drawer toggle +
    /// Configure toggle share this path). Turning it off stops reports and
    /// re-pins; turning it on re-pins from the shadow on the next rebuild.
    pub(crate) fn apply_start_selection_persist(&mut self, enabled: bool) -> Task<Message> {
        if self.session.prefs.start_selection_persist == enabled {
            return Task::none();
        }
        self.session.prefs.start_selection_persist = enabled;
        self.save_prefs();
        Task::none()
    }

    /// Game-axis restore mode (drawer dropdown + Configure pick-list share
    /// this path). Picking from the menu collapses it, even when re-picking
    /// the current value.
    pub(crate) fn apply_start_selection_mode(&mut self, mode: StartSelectionMode) -> Task<Message> {
        self.start_state.settings.close_menus();
        if self.session.prefs.start_selection_mode == mode {
            return Task::none();
        }
        self.session.prefs.start_selection_mode = mode;
        self.save_prefs();
        Task::none()
    }

    /// Generic dropdown toggle for selector rows: collapsing the open row
    /// just closes it; opening inits the highlight to the current value,
    /// closes any other menu (single menu), and scrolls it into view.
    pub(crate) fn toggle_start_selector_menu(
        &mut self,
        row: crate::ui::start::settings::SettingsRow,
    ) -> Task<Message> {
        let snapshot = self.start_settings_snapshot();
        let opened = self.start_state.settings.toggle_menu(row, &snapshot);
        self.play_ui_cue(UiSoundKind::Nav);
        if opened {
            self.scroll_settings_focus_into_view(start_view::ScrollReveal::Either)
        } else {
            Task::none()
        }
    }

    pub(crate) fn scroll_settings_focus_into_view(
        &mut self,
        direction: start_view::ScrollReveal,
    ) -> Task<Message> {
        let snapshot = self.start_settings_snapshot();
        let Some(y) = self
            .start_state
            .settings
            .focus_scroll_y(&snapshot, direction)
        else {
            return Task::none();
        };
        self.start_state.sync_settings_scroll(y);
        operation::scroll_to(
            crate::ui::start::settings::settings_scroll_id(),
            operation::AbsoluteOffset {
                x: None,
                y: Some(y),
            },
        )
    }

    /// Reveal the trailing reload row after an AA/vsync toggle dirtied the
    /// renderer prefs (the row mounts below the viewport while focus stays
    /// on the toggle). No-op when prefs are clean again (unmount needs no
    /// scroll; nav clamps focus), when the settings surface is closed (e.g.
    /// the Configure pick-list path), or when the row is already visible.
    pub(crate) fn scroll_perf_reload_into_view(&mut self) -> Task<Message> {
        if !self.start_state.settings.open {
            return Task::none();
        }
        let snapshot = self.start_settings_snapshot();
        if !renderer_restart_needed(
            snapshot.perf_antialiasing,
            snapshot.perf_vsync,
            snapshot.perf_aa_boot,
            snapshot.perf_vsync_boot,
        ) {
            return Task::none();
        }
        let Some(y) = self
            .start_state
            .settings
            .reload_scroll_y(&snapshot, start_view::ScrollReveal::Either)
        else {
            return Task::none();
        };
        self.start_state.sync_settings_scroll(y);
        operation::scroll_to(
            crate::ui::start::settings::settings_scroll_id(),
            operation::AbsoluteOffset {
                x: None,
                y: Some(y),
            },
        )
    }

    pub(crate) fn on_start_settings_nav(&mut self, action: Option<NavAction>) -> Task<Message> {
        let Some(action) = action else {
            return Task::none();
        };
        // Layout transition reflows main content: drop focus moves
        // (pad and keyboard alike) while it runs; Confirm/Cancel stay live, and
        // an open-menu Confirm still commits — deliberate picks are never
        // frozen, only navigation is. Left/Right are no-ops in settings.
        if self.start_state.layout_transition_active()
            && matches!(
                action,
                NavAction::Up | NavAction::Down | NavAction::Left | NavAction::Right
            )
        {
            return Task::none();
        }
        // An expanded dropdown captures all directional + confirm/cancel input.
        if self.start_state.settings.any_menu_open() {
            return self.on_selector_menu_nav(action);
        }
        let snapshot = self.start_settings_snapshot();
        self.start_state.settings.clamp_focus(&snapshot);
        match action {
            NavAction::Up => {
                let before = self.start_state.settings.focus;
                if self.start_state.settings.move_focus(-1, &snapshot) {
                    self.play_ui_cue(UiSoundKind::Nav);
                    let after = self.start_state.settings.focus;
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: start settings focus={after}"
                    ));
                    let dir = crate::ui::start::settings::reveal_for_focus_step(before, after, -1);
                    return self.scroll_settings_focus_into_view(dir);
                }
                Task::none()
            }
            NavAction::Down => {
                let before = self.start_state.settings.focus;
                if self.start_state.settings.move_focus(1, &snapshot) {
                    self.play_ui_cue(UiSoundKind::Nav);
                    let after = self.start_state.settings.focus;
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: start settings focus={after}"
                    ));
                    let dir = crate::ui::start::settings::reveal_for_focus_step(before, after, 1);
                    return self.scroll_settings_focus_into_view(dir);
                }
                Task::none()
            }
            NavAction::Left | NavAction::Right => {
                // Form controls are mouse-driven: Left/Right never nudge
                // sliders, cycle selectors, or flip toggles. Up/Down moves
                // focus, Confirm opens/commits, mouse drag adjusts sliders.
                Task::none()
            }
            NavAction::Confirm => {
                let Some(row) = self.start_state.settings.focused_row(&snapshot) else {
                    return Task::none();
                };
                if let Some(msg) = crate::ui::start::settings::confirm_message(row, &snapshot) {
                    self.play_start_settings_cue(row, UiSoundKind::Action);
                    let task = self.on_start_message(msg);
                    let snap = self.start_settings_snapshot();
                    self.start_state.settings.clamp_focus(&snap);
                    return task;
                }
                // Slider rows: Cross does nothing (use Left/Right).
                Task::none()
            }
            NavAction::Cancel | NavAction::ToggleSettings => {
                self.on_start_message(StartMessage::Close)
            }
            _ => Task::none(),
        }
    }

    /// Pad navigation while a dropdown selector menu is expanded.
    /// Directionals move the highlight (wrapping), Cross selects and closes,
    /// Circle/Options close without changing anything. The open menu's row
    /// parameterizes the option list; committing routes through the row's
    /// select message so apply + close run even when re-picking the value.
    pub(crate) fn on_selector_menu_nav(&mut self, action: NavAction) -> Task<Message> {
        let Some((row, highlight)) = self.start_state.settings.menu else {
            return Task::none();
        };
        let count = row.selector_count() as isize;
        match action {
            NavAction::Up => {
                let next = (highlight as isize - 1).rem_euclid(count) as usize;
                self.start_state.settings.set_menu_highlight(next);
                self.play_ui_cue(UiSoundKind::Nav);
                self.scroll_settings_focus_into_view(start_view::ScrollReveal::Either)
            }
            NavAction::Down => {
                let next = (highlight as isize + 1).rem_euclid(count) as usize;
                self.start_state.settings.set_menu_highlight(next);
                self.play_ui_cue(UiSoundKind::Nav);
                self.scroll_settings_focus_into_view(start_view::ScrollReveal::Either)
            }
            NavAction::Confirm => {
                let select = row.selector_select(highlight);
                self.play_ui_cue(UiSoundKind::Action);
                self.on_start_message(select)
            }
            NavAction::Cancel | NavAction::ToggleSettings => {
                self.start_state.settings.close_menus();
                self.play_ui_cue(UiSoundKind::Nav);
                Task::none()
            }
            _ => Task::none(),
        }
    }
}

impl App {
    pub(crate) fn effective_played_ms(&self, entry: &crate::games::GameEntry) -> Option<u64> {
        let catalog_ms = self.games.last_played(entry).unwrap_or(0);
        let steam_ms = match entry {
            crate::games::GameEntry::Steam { appid } => self
                .steam_by_id
                .get(appid)
                .and_then(|g| g.last_played_unix)
                .map(|secs| secs.saturating_mul(1000))
                .unwrap_or(0),
            crate::games::GameEntry::Manual { .. } => 0,
        };
        let ms = catalog_ms.max(steam_ms);
        (ms > 0).then_some(ms)
    }

    pub(crate) fn display_catalog(&self) -> Vec<crate::games::GameEntry> {
        let steam_by_id = &self.steam_by_id;
        // Keep uninstalled Steam entries so rows can render as View in Steam
        // instead of vanishing — unless the user opted out (and the scan
        // proved absence; unknown scans keep everything launchable).
        // Launchability is resolved per-row in refresh_start_rows.
        let installed_filter = if self.session.prefs.show_uninstalled_games_enabled() {
            None
        } else {
            self.steam_installed.as_deref()
        };
        self.games.merge_sorted(
            installed_filter,
            self.session.prefs.games_sort_mode,
            |entry| match entry {
                crate::games::GameEntry::Steam { appid } => steam_by_id
                    .get(appid)
                    .map(|g| g.name.clone())
                    .or_else(|| self.games.steam_display_name(*appid).map(str::to_string))
                    .unwrap_or_else(|| format!("Steam {appid}")),
                crate::games::GameEntry::Manual { title, .. } => title.clone(),
            },
            |entry| {
                let catalog_ms = self.games.last_played(entry).unwrap_or(0);
                let steam_ms = match entry {
                    crate::games::GameEntry::Steam { appid } => steam_by_id
                        .get(appid)
                        .and_then(|g| g.last_played_unix)
                        .map(|secs| secs.saturating_mul(1000))
                        .unwrap_or(0),
                    crate::games::GameEntry::Manual { .. } => 0,
                };
                let ms = catalog_ms.max(steam_ms);
                (ms > 0).then_some(ms)
            },
        )
    }

    /// True when this entry provably cannot launch (disabled row).
    ///
    /// Steam entries are never disabled: scan-missing rows become View in
    /// Steam rows (`view_in_steam`, set in [`Self::refresh_start_rows`] /
    /// [`StartRow::from_entry`]) and stay navigable. Only manual entries
    /// with provably-missing targets dim.
    pub(crate) fn is_entry_disabled(
        &self,
        entry: &crate::games::GameEntry,
        row: &start_view::StartRow,
    ) -> bool {
        if row.skeleton {
            return false;
        }
        match entry {
            crate::games::GameEntry::Steam { .. } => false,
            crate::games::GameEntry::Manual { target, .. } => is_manual_target_missing(target),
        }
    }

    pub(crate) fn edit_catalog(&self) -> &GamesCatalog {
        self.edit_draft.as_ref().unwrap_or(&self.games)
    }

    pub(crate) fn edit_catalog_mut(&mut self) -> &mut GamesCatalog {
        self.edit_draft.as_mut().unwrap_or(&mut self.games)
    }

    pub(crate) fn edit_checklist_rows(&self) -> Vec<start_view::StartRow> {
        let catalog = self.edit_catalog();
        let sort_mode = self.session.prefs.games_sort_mode;
        let mut rows: Vec<_> = self
            .steam_by_id
            .values()
            .map(|game| {
                let mut row =
                    start_view::StartRow::steam_edit(game, catalog.contains_steam(game.appid));
                let catalog_ms = catalog
                    .last_played(&crate::games::GameEntry::Steam { appid: game.appid })
                    .unwrap_or(0);
                let steam_ms = game.last_played_unix.unwrap_or(0).saturating_mul(1000);
                row.played_at_ms =
                    (catalog_ms.max(steam_ms) > 0).then_some(catalog_ms.max(steam_ms));
                row
            })
            .collect();
        // Curated-but-uninstalled games surface as toggled-on rows (last-known
        // title) so they can be untoggled — previously they were invisible
        // here. Hidden entirely when the user opts out of uninstalled games.
        if self.session.prefs.show_uninstalled_games_enabled() {
            for entry in &catalog.entries {
                if let crate::games::GameEntry::Steam { appid } = entry
                    && !self.steam_by_id.contains_key(appid)
                {
                    let title = catalog
                        .steam_display_name(*appid)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("Steam {appid}"));
                    rows.push(start_view::StartRow::steam_uninstalled(
                        *appid,
                        title,
                        catalog.last_played(entry),
                    ));
                }
            }
        }
        for entry in &catalog.entries {
            if let Some(mut row) = start_view::StartRow::manual_edit(entry) {
                row.played_at_ms = catalog.last_played(entry);
                rows.push(row);
            }
        }
        start_view::sort_edit_rows_by_mode(&mut rows, sort_mode);
        rows
    }

    pub(crate) fn refresh_running_badge(&mut self) -> Task<Message> {
        self.refresh_running_badge_inner(false)
    }

    pub(crate) fn refresh_running_badge_inner(&mut self, force: bool) -> Task<Message> {
        let now = Instant::now();

        // No grace early-return: enums run from launch onward (throttled) so
        // a Steam handoff that never spawns a process fast-fails instead of
        // holding `Playing` blind for `LAUNCH_GRACE`. Optimism lives in
        // `apply_running_badge_from_images` via `decide_running_badge`, which
        // also means a reopen after the window spawns a real check instead of
        // re-asserting the stale badge.
        if self.process_enum_inflight {
            return Task::none();
        }

        // Registry-watch events bypass the cadence (rare: a few per launch /
        // exit) so real launches confirm immediately; ticks stay throttled.
        if !force
            && self
                .last_running_check
                .is_some_and(|t| now.duration_since(t) < RUNNING_CHECK_INTERVAL)
        {
            return Task::none();
        }
        self.last_running_check = Some(now);
        self.process_enum_inflight = true;

        Task::perform(
            spawn_blocking(process_match::list_process_images),
            Message::ProcessEnumResult,
        )
    }

    pub(crate) fn on_process_enum_result(
        &mut self,
        result: AppResult<Vec<(u32, PathBuf)>>,
    ) -> Task<Message> {
        self.process_enum_inflight = false;
        let images = match result {
            Ok(images) => images,
            Err(err) => {
                app_log::warn(format!("process enum failed: {err}"));
                return Task::none();
            }
        };
        self.apply_running_badge_from_images(&images);
        Task::none()
    }

    pub(crate) fn apply_running_badge_from_images(&mut self, images: &[(u32, PathBuf)]) {
        let now = Instant::now();

        if let Some(session) = self.running_session.as_mut() {
            let process_matched =
                process_match::any_matching_in_images(&session.match_paths, images);
            // Steam's own running flag (Windows registry; None = unknown /
            // off-platform / non-Steam target). Authoritative when present:
            // it covers launcher/JVM games path matching cannot see, and it
            // never flips for license-refused launches.
            let steam_running =
                steam_appid_from_target(&session.target).and_then(steam::is_appid_running);
            let was_matched = session.matched;
            if process_match::decide_running_badge(session, now, process_matched, steam_running) {
                // Stamp last-played on the false→true confirm edge only:
                // failed handoffs (Steam error, never a process) must not
                // pollute Last Played sort/metadata.
                let confirm_key = if !was_matched && session.matched {
                    session.play_key.clone()
                } else {
                    None
                };
                if let Some(play_key) = confirm_key {
                    self.touch_last_played(&play_key);
                }
                self.sync_launch_badges();
                // Keep any unrelated Closing visible; don't clear it here.
                return;
            }
            let target = session.target.clone();
            app_log::info(format!("running session cleared (process miss): {target}"));
            self.running_session = None;
            self.sync_launch_badges();
        } else {
            self.sync_launch_badges();
        }

        // "Closing" stays until its process actually exits (or paths were empty).
        if self.start_state.closing_target.is_some() {
            if !self.closing_match_paths.is_empty()
                && process_match::any_matching_in_images(&self.closing_match_paths, images)
            {
                // Still exiting: keep Closing, don't restore it as Running.
                return;
            }
            self.start_state.closing_target = None;
            self.closing_match_paths.clear();
        }

        self.try_restore_running_from_catalog(images);
    }

    pub(crate) fn cached_match_paths(&mut self, target: &str) -> Vec<PathBuf> {
        if let Some(paths) = self.match_path_cache.get(target) {
            return paths.clone();
        }
        let paths = process_match::match_paths_for_target(target, &self.seen_cache);
        self.match_path_cache
            .insert(target.to_string(), paths.clone());
        paths
    }

    /// Scan catalog targets for a live process (single process snapshot).
    pub(crate) fn try_restore_running_from_catalog(&mut self, images: &[(u32, PathBuf)]) {
        let candidates: Vec<(String, String)> = self
            .start_state
            .rows
            .iter()
            .map(|r| (r.title.clone(), r.target.clone()))
            .collect();
        if candidates.is_empty() {
            return;
        }

        let path_sets: Vec<Vec<PathBuf>> = candidates
            .iter()
            .map(|(_, target)| self.cached_match_paths(target))
            .collect();

        let Some(idx) = process_match::first_matching_index_in_images(&path_sets, images) else {
            return;
        };
        let (title, target) = candidates[idx].clone();
        if self
            .running_session
            .as_ref()
            .is_some_and(|s| s.matches_target(&target))
        {
            // A live process matches the session target: confirm it.
            if let Some(session) = self.running_session.as_mut() {
                session.matched = true;
            }
            self.sync_launch_badges();
            return;
        }
        let match_paths = path_sets[idx].clone();
        app_log::info(format!("running session restored: {target}"));
        self.running_session = Some(RunningSession::restored(title, target.clone(), match_paths));
        self.sync_launch_badges();
    }

    pub(crate) fn begin_running_session(
        &mut self,
        title: String,
        target: String,
        play_key: String,
    ) {
        let match_paths = self.cached_match_paths(&target);
        if match_paths.is_empty() {
            app_log::warn(format!(
                "running session: empty match_paths for {target} (badge may clear after grace)"
            ));
        }
        self.running_session = Some(RunningSession::new(
            title,
            target.clone(),
            match_paths,
            Some(play_key),
        ));
        self.sync_launch_badges();
    }

    /// Mirror the launch session into Start badges: a confirmed session
    /// (`matched`: process or Steam verdict) shows Running, a live but
    /// unconfirmed one shows Launching, no session clears both.
    pub(crate) fn sync_launch_badges(&mut self) {
        if let Some(session) = self.running_session.as_ref() {
            let target = session.target.clone();
            if session.matched {
                self.start_state.running_target = Some(target);
                self.start_state.launching_target = None;
            } else {
                self.start_state.running_target = None;
                self.start_state.launching_target = Some(target);
            }
        } else {
            self.start_state.running_target = None;
            self.start_state.launching_target = None;
        }
    }

    pub(crate) fn touch_last_played(&mut self, play_key: &str) {
        self.games.touch_played_key(play_key);
        self.games.save();
    }

    pub(crate) fn close_running_game(&mut self) -> Task<Message> {
        let Some(session) = self.running_session.clone() else {
            return Task::none();
        };
        // Show "Closing" immediately; clear "Playing" now so the status flips
        // on the next frame even though WM_CLOSE + terminate blocks.
        self.start_state.closing_target = Some(session.target.clone());
        self.closing_match_paths = session.match_paths.clone();
        self.running_session = None;
        self.sync_launch_badges();
        self.start_state.cross_progress = 0.0;
        let match_paths = session.match_paths.clone();
        let target = session.target.clone();
        crate::controller::hid::diag::diag_info(format!("ui-diag: game closing target={target}"));
        Task::perform(
            spawn_blocking(move || launch::close_matching(&match_paths)),
            |res: AppResult<AppResult<()>>| Message::CloseGameDone(res.unwrap_or_else(Err)),
        )
    }

    pub(crate) fn on_close_game_done(&mut self, result: AppResult<()>) -> Task<Message> {
        if let Err(err) = result {
            app_log::warn(format!("close game failed: {err}"));
        }
        // Force a process check so "Closing" clears as soon as the process exits.
        self.last_running_check = None;
        self.refresh_running_badge()
    }

    /// Hold-Cross close: only when the selected row is the running game.
    pub(crate) fn close_running_game_if_selected(&mut self) -> Task<Message> {
        let Some(session) = self.running_session.as_ref() else {
            return Task::none();
        };
        let Some(row) = self.start_state.rows.get(self.start_state.game_selected) else {
            return Task::none();
        };
        if !session.matches_target(&row.target) {
            return Task::none();
        }
        self.close_running_game()
    }

    pub(crate) fn steam_scan_in_flight(&self) -> bool {
        self.steam_scan_started.is_some() || self.start_state.steam_scan_banner_active()
    }

    /// First Start HWND of the process has finished ambient+status; later opens are warm.
    ///
    /// Immersive: kick the deferred Steam refresh immediately so its status chip
    /// can stack under Preparing Success (event-based; no settle delay).
    pub(crate) fn note_cold_load_finished(&mut self) -> Task<Message> {
        if self.start_state.cold_load_active {
            self.start_state.finish_cold_load();
        }
        self.start_cold_load_remaining = false;
        crate::controller::hid::diag::diag_info(
            "ui-diag: chrome status queue kick steam (art ready)",
        );
        self.maybe_kick_deferred_steam_scan()
    }

    /// Pin ±ART_WINDOW and enqueue a latest-wins decode job on the art worker.
    pub(crate) fn prepare_immersive_library_task(&mut self) -> Task<Message> {
        self.request_start_art();
        Task::none()
    }

    pub(crate) fn confirm_manual_add(&mut self) -> Task<Message> {
        // Display-only drain (already committed, tween running) must not
        // apply twice.
        if self.start_state.manual_closing() {
            return Task::none();
        }
        let Some(draft) = self.start_state.manual_add.clone() else {
            return Task::none();
        };
        let title = draft.title.trim();
        let title = if title.is_empty() {
            games::title_from_target(&draft.target)
        } else {
            title.to_string()
        };
        let icon = draft
            .icon_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let args = draft.args.trim().to_string();
        if let Some(id) = draft.edit_id.as_deref() {
            self.edit_catalog_mut().update_manual(id, title, args, icon);
        } else {
            self.edit_catalog_mut()
                .add_manual(title, draft.target, args, icon);
        }
        self.refresh_start_rows();
        // Data is committed; the form stays mounted as display-only through
        // the exit tween and clears when it settles.
        self.start_state.dismiss_manual(Instant::now());
        Task::none()
    }

    pub(crate) fn apply_perf_throttle_immersive(&mut self, enabled: bool) -> Task<Message> {
        if self.session.prefs.perf_throttle_immersive == enabled {
            return Task::none();
        }
        self.session.prefs.perf_throttle_immersive = enabled;
        self.save_prefs();
        self.start_state.sync_perf_throttle(enabled);
        Task::none()
    }

    pub(crate) fn apply_perf_antialiasing(&mut self, enabled: bool) -> Task<Message> {
        if self.session.prefs.perf_antialiasing == enabled {
            return Task::none();
        }
        self.session.prefs.perf_antialiasing = enabled;
        self.save_prefs();
        self.scroll_perf_reload_into_view()
    }

    pub(crate) fn apply_perf_vsync(&mut self, enabled: bool) -> Task<Message> {
        if self.session.prefs.perf_vsync == enabled {
            return Task::none();
        }
        self.session.prefs.perf_vsync = enabled;
        self.save_prefs();
        self.scroll_perf_reload_into_view()
    }

    /// Explicit UI reload for boot-only renderer prefs (AA/vsync are baked
    /// into the iced/wgpu renderer at boot; iced 0.14 exposes no runtime
    /// flip). The reload button shows only while prefs differ from the boot
    /// values. Queues the restart after a short feedback delay so the button
    /// can flip to its disabled "Reloading…" state; repeat presses no-op.
    pub(crate) fn reload_ui_for_renderer(&mut self) -> Task<Message> {
        if self.renderer_reload_queued {
            return Task::none();
        }
        if !renderer_restart_needed(
            self.session.prefs.perf_antialiasing,
            self.session.prefs.perf_vsync,
            self.renderer_boot_aa,
            self.renderer_boot_vsync,
        ) {
            return Task::none();
        }
        self.renderer_reload_queued = true;
        Task::perform(delay(RELOAD_FEEDBACK_DELAY), |()| {
            Message::RendererReloadDue
        })
    }

    /// Fire a queued renderer reload. Re-checks the dirty predicate (a toggle
    /// flipped back during the feedback delay cancels the restart).
    pub(crate) fn restart_app_for_renderer(&mut self) -> Task<Message> {
        self.renderer_reload_queued = false;
        if !renderer_restart_needed(
            self.session.prefs.perf_antialiasing,
            self.session.prefs.perf_vsync,
            self.renderer_boot_aa,
            self.renderer_boot_vsync,
        ) {
            return Task::none();
        }
        app_log::info("perf: renderer prefs changed; reloading UI to apply");
        // Snapshot the app UI so the relaunched process reopens to the same
        // state (windows, selection, scroll offsets). Written before Exit
        // clears Start state; consumed once at boot when fresh.
        crate::persist::restart_ui::save(&self.capture_restart_ui());
        self.restarting_for_renderer = true;
        // Relaunch detached first: the `--replace` child waits out this
        // process's single-instance guard while it exits. If the relaunch
        // fails, stay running on the old renderer rather than quitting.
        match std::env::current_exe() {
            Ok(exe) => {
                // `--replace`: the child retries the single-instance guard
                // while this process exits (see `sdsc-utils` main).
                if let Err(err) = std::process::Command::new(&exe)
                    .arg(crate::platform::app_meta::REPLACE_FLAG)
                    .spawn()
                {
                    app_log::error(format!("perf: relaunch failed: {err}"));
                    return Task::none();
                }
            }
            Err(err) => {
                app_log::error(format!("perf: current_exe failed: {err}"));
                return Task::none();
            }
        }
        self.update(Message::Exit)
    }

    /// Snapshot the app UI for a renderer-pref reload (see `restart_ui`).
    pub(crate) fn capture_restart_ui(&self) -> crate::persist::restart_ui::RestartUiState {
        use crate::persist::restart_ui::RestartUiState;
        RestartUiState {
            saved_at_ms: RestartUiState::now_ms(),
            start_visible: self.start_visible,
            start_on_controllers: false,
            game_key: self
                .start_state
                .rows
                .get(self.start_state.game_selected)
                .map(|r| r.play_key.clone()),
            game_index: Some(self.start_state.game_selected),
            controller_key: None,
            settings_scroll_y: self
                .start_state
                .list_offset(start_view::StartListId::Settings),
            settings_open: self.start_state.settings.open,
            settings_focus: self.start_state.settings.focus,
            configure_open: self.configure_window.is_some(),
            configure_section: Some(self.configure_state.section),
            configure_scroll_y: self.configure_state.content_scroll_y,
            popup_open: self.popup_window.is_some(),
            popup_anchor: Some(self.tray_anchor),
        }
    }

    /// Reopen to a consumed restart record (renderer-pref restart only).
    /// Settings state restores FIRST, before any window opens or list
    /// rebuilds; the restore open resolves selections from the shadow keys
    /// instead (see `restore_preserve_nav`). Scroll offsets land on
    /// `StartOpened`/`ConfigureOpened`, when the scrollables exist.
    pub(crate) fn apply_restart_ui(
        &mut self,
        rs: crate::persist::restart_ui::RestartUiState,
    ) -> Task<Message> {
        // --- Settings state first (drawer, configure, anchor). ---
        if let Some(anchor) = rs.popup_anchor {
            self.tray_anchor = anchor;
        }
        if rs.configure_open {
            if let Some(section) = rs.configure_section {
                // No scroll-to here: the content scrollable is not mounted
                // yet, so the op would be dropped. Section applies now; the
                // saved offset lands on `ConfigureOpened`.
                self.configure_state.select_section(section);
            }
            self.configure_state.content_scroll_y = rs.configure_scroll_y;
            self.pending_configure_scroll = Some(rs.configure_scroll_y);
        }
        if rs.start_visible && self.session.prefs.start_screen_enabled {
            if rs.game_key.is_some() {
                // Fresh boot: seed the shadow from the restore record so row
                // rebuilds re-pin the pre-restart selection by key.
                self.start_selection_shadow = Some(crate::session::StartSelection {
                    game_key: rs.game_key,
                    controller_key: rs.controller_key,
                    game_index: rs.game_index,
                });
            }
            // Bypass the open-time nav reset below; everything it would
            // clear is restored here instead.
            self.restore_preserve_nav = true;
            // Controllers live in the PS hub now; legacy Start surface state
            // in the record is ignored.
            let _ = rs.start_on_controllers;
            if rs.settings_open {
                self.start_state.request_settings(true, Instant::now());
                // Instant restore: no drawer travel on boot.
                self.start_state.settings.anim = None;
            }
            self.start_state.settings.focus = rs.settings_focus;
            self.start_state.sync_settings_scroll(rs.settings_scroll_y);
            self.pending_settings_scroll = Some(rs.settings_scroll_y);
            // Settle window for selection re-pinning after row rebuilds (the
            // offsets themselves are one-shot and never re-asserted).
            self.scroll_restore_until = Some(Instant::now() + SCROLL_RESTORE_AUTHORITY);
        }
        // --- Windows after: configure + start first, popup last. ---
        let mut task = Task::none();
        if rs.configure_open {
            task = task.chain(self.open_configure());
        }
        if rs.start_visible && self.session.prefs.start_screen_enabled {
            task = task.chain(self.open_start_screen());
        }
        if rs.popup_open {
            task = task.chain(self.open_popup());
        }
        task
    }

    /// Re-apply the saved settings offset once, on `StartOpened` when the
    /// scrollable exists. Never re-asserted afterwards.
    pub(crate) fn restore_settings_scroll(&mut self, settings_y: f32) -> Task<Message> {
        let id = start_view::StartListId::Settings.scroll_id();
        self.start_state.note_scroll_y(&id, settings_y);
        operation::scroll_to(
            id,
            operation::AbsoluteOffset {
                x: None,
                y: Some(settings_y),
            },
        )
    }
}

impl App {
    pub(crate) fn open_start_screen(&mut self) -> Task<Message> {
        if !self.session.prefs.start_screen_enabled {
            return Task::none();
        }
        // An explicit open wins over a pending deferred destroy.
        self.start_close_armed = false;
        // Edit state (editing mode, drafts, confirms) survives closes like
        // everything else: PS toggles never wipe in-progress work.
        self.refresh_start_rows();

        // Restore opens resolve from the shadow keys: reload restore already
        // set drawer/scroll/selection state above. Pad nav arming still runs
        // either way; no visual state is ever reset on open anymore.
        if std::mem::take(&mut self.restore_preserve_nav) {
            self.pad_nav.prepare_on_open(false);
        } else {
            self.prepare_start_nav_on_open(false);
        }
        // Pin state for the first paint (before the first StartFrame tick).
        self.start_state.sync_navbar_pinned(
            matches!(
                self.session.prefs.start_screen_navbar,
                NavbarVisibility::AlwaysOn
            ),
            Instant::now(),
        );
        self.start_state
            .sync_perf_throttle(self.session.prefs.perf_throttle_immersive);
        // App restart: resolve the stored shadow keys back to indices now
        // that rows/controllers are rebuilt.
        let mut restored_art = Task::none();
        if let Some(selection) = self.start_selection_shadow.clone()
            && self.start_state.resolve_start_selection(
                selection.game_key.as_deref(),
                selection.controller_key.as_deref(),
                self.session.prefs.start_selection_persist,
                self.session.prefs.start_selection_mode,
                selection.game_index,
            )
        {
            restored_art = self.warm_start_art_task();
        }

        // Already open: re-focus; force running restore in case a game started.
        // Duplicate OpenStart (e.g. a second effect before the first open
        // lands) must re-arm the release latch so the opening press cannot
        // re-fire.
        if self.start_visible
            && let Some(id) = self.start_window
        {
            self.session.auto_open_pending = false;
            self.last_running_check = None;
            self.arm_chord_release_latch();
            let badge = self.refresh_running_badge();
            let close_browser = if self.browser_visible {
                self.close_browser_window()
            } else {
                Task::none()
            };
            return close_browser.chain(Task::batch([badge, focus_window(id)]));
        }

        // Mark the current chord consumed (do not reset): a held PS from power-on
        // must not reopen Start immediately after a focus-close.
        self.arm_chord_release_latch();
        self.clear_start_nav_diag();
        self.start_opened_at = Some(Instant::now());
        self.start_revealed_at = Some(Instant::now());
        // refresh_start_rows already ran a throttled check; force one restore on open.
        self.last_running_check = None;
        let badge = self.refresh_running_badge();

        // Cold open — OS window entrance. Do not chain sync_toast_zorder here;
        // StartOpened raises after the window/renderer exists.
        if self.start_window.is_some() {
            // Stale id without visibility (close in flight) — wait for WindowClosed.
            // Leave auto_open_pending set so WindowClosed can retry.
            crate::controller::hid::diag::diag_info(
                "ui-diag: start open skipped (window still closing)",
            );
            return Task::none();
        }
        // Start and Browser are mutually exclusive.
        let close_browser = if self.browser_visible {
            self.close_browser_window()
        } else {
            Task::none()
        };

        let cover = primary_monitor_cover().unwrap_or(MonitorCover {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        });
        self.start_monitor_cover = Some(cover);
        self.clear_start_activity();
        // Enter veil stays solid until StartOpened arms the capped clock.
        self.start_state
            .begin_transition_phase(start_mode::TransitionPhase::EnterImmersive, Instant::now());
        crate::controller::hid::diag::diag_info("ui-diag: start immersive enter");
        let size = cover.size();
        let position = window::Position::Specific(cover.origin());

        self.start_nav_ready = false;
        crate::controller::hid::diag::diag_info("ui-diag: start cold open");
        if self.start_cold_load_remaining {
            self.start_state.begin_cold_load(Instant::now());
        }
        let (id, open) = window::open(window::Settings {
            size,
            position,
            visible: open_visible(),
            transparent: crate::ui::chrome::transparent_windows(),
            resizable: false,
            decorations: false,
            // Start is a normal-level window now: the PS hub is the only
            // always-on-top shell surface. Bring-to-front happens on open.
            level: window::Level::Normal,
            exit_on_close_request: true,
            platform_specific: overlay_platform_specific(),
            ..window::Settings::default()
        });
        self.start_window = Some(id);
        self.start_visible = true;
        self.session.auto_open_pending = false;
        crate::platform::wgpu_diag::note_start_open();
        // Start composites the toast card; hide the HWND so it cannot float
        // above the cover as a duplicate.
        let hide_toast = if self.toast_message.is_some() {
            self.toast_window
                .map(crate::ui::layout::hide_toast)
                .unwrap_or_else(Task::none)
        } else {
            Task::none()
        };
        close_browser.chain(hide_toast.chain(Task::batch([
            badge,
            restored_art,
            open.map(Message::StartOpened),
        ])))
    }

    /// Advance the enter ceremony: once the veil clock finishes, clear the
    /// phase and begin any held splash reveal on the same tick.
    pub(crate) fn tick_enter_phase(&mut self, now: Instant) -> Option<Task<Message>> {
        if !self.start_state.phase_finished(now) {
            return None;
        }
        let (phase, started) = self.start_state.transition_phase?;
        // Overrun witness: completion is wall-clock, so ending far past spec
        // means ticks starved (the window outlived the ceremony in paint-time).
        let elapsed_ms = now.saturating_duration_since(started).as_millis() as u64;
        let allowance_ms = crate::ui::start::mode::ENTER_ART_HOLD_MAX_MS
            + crate::ui::start::mode::ENTER_REVEAL_MS
            + 1000;
        if elapsed_ms > allowance_ms {
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: start phase overrun phase={} elapsed_ms={elapsed_ms} spec_ms={}",
                phase.label(),
                phase.duration_ms()
            ));
        }
        self.start_state.clear_transition_phase();
        // Veil phase over — begin held ambient→splash on this same clock tick.
        self.start_state.try_start_pending_splash_reveal(now);
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: start enter phase={} done",
            phase.label()
        ));
        None
    }

    /// Arm the debounced release latch and mark any locally visible chord consumed.
    ///
    /// When no fresh readings are published, leave detectors alone (do not
    /// reset/disarm). Live pad edges while latched call `consume_pending_match`.
    pub(crate) fn arm_chord_release_latch(&mut self) {
        self.chord_release_gate.arm();
        self.consume_reopen_gesture_chord();
    }

    /// Mark the live reopen chord as already matched so a sticky hold cannot fire.
    pub(crate) fn consume_reopen_gesture_chord(&mut self) {
        match self.input_hub.read() {
            start_input::NavReadingsOutcome::Readings { readings, .. } => {
                self.gesture_detectors.consume_pending_match(&readings);
            }
            start_input::NavReadingsOutcome::Missing { .. } => {
                // No fresh readings — resetting would disarm detectors
                // and let the same press reopen after a one-sample gap.
            }
        }
    }

    /// Re-arm pad nav for an open: selection, drawer, scrolls and drafts all
    /// survive closes now (PS toggles; every close preserves state), so this
    /// only re-arms input, it never resets visual state.
    pub(crate) fn prepare_start_nav_on_open(&mut self, arm_immediately: bool) {
        self.pad_nav.prepare_on_open(arm_immediately);
        self.keyboard_cross_hold.reset();
        self.confirm_key_held = false;
        self.cancel_key_held = false;
        self.pad_held = FaceHeld::default();
    }

    /// Stamp idle/sleep activity (does not unhide the cursor — pad/keyboard stay mouse-free).
    pub(crate) fn note_start_activity(&mut self) {
        let now = Instant::now();
        self.start_last_activity = now;
        self.wake_start_idle(now, false);
    }

    /// Mouse move/click/wheel: wake idle/sleep and show the cursor.
    pub(crate) fn note_start_cursor_activity(&mut self) {
        let now = Instant::now();
        self.start_last_activity = now;
        self.start_cursor_last_active = now;
        self.set_start_cursor_hidden(false);
        self.wake_start_idle(now, false);
    }

    /// Reset activity clocks without treating it as a sleep-wake (open/close).
    pub(crate) fn clear_start_activity(&mut self) {
        let now = Instant::now();
        self.start_last_activity = now;
        self.start_cursor_last_active = now;
        self.set_start_cursor_hidden(false);
        self.start_state.reset_idle_dim();
    }

    pub(crate) fn set_start_cursor_hidden(&mut self, hidden: bool) {
        if self.start_cursor_hidden == hidden {
            return;
        }
        self.start_cursor_hidden = hidden;
        if hidden {
            crate::controller::hid::diag::diag_info("ui-diag: start cursor hide");
        } else {
            crate::controller::hid::diag::diag_info("ui-diag: start cursor show");
        }
    }

    pub(crate) fn tick_start_cursor_idle(&mut self, now: Instant) {
        if !self.start_visible {
            self.set_start_cursor_hidden(false);
            return;
        }
        if !cursor_on_primary_monitor() {
            self.set_start_cursor_hidden(false);
            return;
        }
        // The cover spans the full monitor including the taskbar, but
        // taskbar input never reaches Start — hiding there strands the
        // cursor with no self-unhide path (notably across a blocking
        // tray-menu modal loop).
        if !crate::platform::host::Host::cursor_in_work_area() {
            self.set_start_cursor_hidden(false);
            return;
        }
        let idle = now.saturating_duration_since(self.start_cursor_last_active);
        if idle >= START_CURSOR_IDLE_HIDE {
            self.set_start_cursor_hidden(true);
        }
    }

    /// Wake idle/sleep overlay toward Active. `from_sleep` marks pad-gated wake.
    pub(crate) fn wake_start_idle(&mut self, now: Instant, from_sleep: bool) {
        use crate::ui::start::idle::IdlePhase;
        if !self.start_visible {
            return;
        }
        if matches!(self.start_state.idle_phase, IdlePhase::Active)
            && !self.start_state.idle_sleep_wake_pending
            && !self.start_state.idle_dim_animating(now)
        {
            return;
        }
        let was_sleep = matches!(self.start_state.idle_phase, IdlePhase::Sleep) || from_sleep;
        let inactive_dim = f32::from(self.session.prefs.start_screen_inactive_dim_percent) / 100.0;
        if self
            .start_state
            .set_idle_phase(IdlePhase::Active, inactive_dim, now, was_sleep)
        {
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: start idle phase={} wake_from_sleep={}",
                IdlePhase::Active.label(),
                u8::from(was_sleep),
            ));
        }
    }

    /// Advance idle/sleep timeouts + dim anim. Returns true if pad should re-arm after sleep wake.
    pub(crate) fn tick_start_idle(&mut self, now: Instant) -> bool {
        use crate::ui::start::idle::{self as start_idle};

        if !self.start_visible {
            self.start_state.reset_idle_dim();
            return false;
        }
        // Don't advance idle during the enter ceremony.
        if self.start_state.transition_phase.is_some() {
            let clear_gate = self.start_state.tick_idle_dim(now);
            return clear_gate;
        }

        let clear_gate = self.start_state.tick_idle_dim(now);

        // While waking from sleep, keep targeting Active (activity already stamped).
        if self.start_state.idle_sleep_wake_pending {
            return clear_gate;
        }

        let idle_secs = now
            .saturating_duration_since(self.start_last_activity)
            .as_secs_f32();
        let target = start_idle::phase_for_idle_secs(
            idle_secs,
            self.session.prefs.start_screen_inactive_secs,
            self.session.prefs.start_screen_sleep_secs,
        );
        if target != self.start_state.idle_phase {
            let inactive_dim =
                f32::from(self.session.prefs.start_screen_inactive_dim_percent) / 100.0;
            if self
                .start_state
                .set_idle_phase(target, inactive_dim, now, false)
            {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: start idle phase={}",
                    target.label()
                ));
            }
        }
        clear_gate
    }

    pub(crate) fn close_start_screen(&mut self) -> Task<Message> {
        // A close arriving while a deferred destroy is armed is already
        // handled: the armed phase owns the teardown.
        if self.start_close_armed {
            return Task::none();
        }
        // Capture before clear: destroying a hidden-cursor window skips the
        // restore present (winit only pushes on change) and leaks a global
        // ShowCursor(FALSE). The armed phase below lets the restore land.
        let was_hidden = self.start_cursor_hidden;
        // Clear visibility before any hide task so Resting toast RaiseInteractive
        // cannot resurface Start while the hide is in flight.
        self.start_visible = false;
        let rehost_toast = self.rehost_toast_after_composite();
        self.start_nav_ready = false;
        self.start_revealed_at = None;
        // Tuning is settled state: never reopen with it on.
        #[cfg(debug_assertions)]
        self.start_state.request_ambient_tuning(false);
        self.start_state.clear_transition_phase();
        // Invalidate in-flight art events; keep caches for fast reopen.
        // Process Exit is what wipes to a blank slate (`ArtOwner::shutdown`).
        let generation = crate::ui::start::icon_cache::bump_art_warm_generation();
        self.art.cancel(generation);
        self.start_state.art_awaiting_paint = false;
        self.start_monitor_cover = None;
        self.clear_start_activity();
        self.haptic_pad_serial = None;
        self.hid_worker.rumble_stop_all();
        self.pad_nav.reset();
        self.keyboard_cross_hold.reset();
        self.confirm_key_held = false;
        self.cancel_key_held = false;
        self.pad_held = FaceHeld::default();
        // Visual state (selection, dock, drawer, scrolls, edit draft) is
        // never reset on close anymore: every close preserves state and the
        // next open resumes where it left off (see `toggle_start_on_reopen_chord`).
        // No window (never opened / already destroyed): nothing to do —
        // there is no teardown pending and no reset to run.
        self.clear_start_nav_diag();
        // Focus-close often leaves the reopen chord held (e.g. PS); latch until
        // a debounced full release so the next PadPoll cannot immediately reopen.
        self.arm_chord_release_latch();
        if was_hidden && self.start_window.is_some() {
            // Deferred destroy: the flag already cleared above, so the next
            // loop pass rebuilds with a visible interaction and pushes the
            // restore to the OS before FinishStartClose tears down the HWND.
            crate::controller::hid::diag::diag_info("ui-diag: start close armed (cursor restore)");
            self.start_close_armed = true;
            return rehost_toast.chain(Task::done(Message::FinishStartClose));
        }
        rehost_toast.chain(self.hide_and_close_start_window())
    }

    /// Second close phase: destroy the Start HWND after the cursor-restore
    /// present has landed (see `start_close_armed`).
    pub(crate) fn finish_start_close(&mut self) -> Task<Message> {
        if !self.start_close_armed {
            return Task::none();
        }
        self.start_close_armed = false;
        self.hide_and_close_start_window()
    }

    /// Hide-then-close the Start HWND (avoids a DWM default-brush flash
    /// while the wgpu surface tears down). No-op without a window.
    fn hide_and_close_start_window(&mut self) -> Task<Message> {
        match self.start_window {
            Some(id) => {
                crate::controller::hid::diag::diag_info("ui-diag: start close");
                window::set_mode(id, window::Mode::Hidden).chain(window::close(id))
            }
            None => Task::none(),
        }
    }

    pub(crate) fn clear_start_nav_diag(&mut self) {
        self.nav_missing_warned = false;
        self.nav_unarmed_warned = false;
        self.nav_source_logged = None;
        self.last_nav_log = None;
        self.last_pad_poll_at = None;
        self.last_edge_seq = None;
        self.nav_missing_since = None;
        self.nav_stale_warned = false;
        self.nav_not_ready_warned = false;
        self.start_opened_at = None;
    }

    /// Mouse report buttons — stamp both logs with kind + live snapshot context.
    #[cfg(debug_assertions)]
    pub(crate) fn mark_hitch_now(&mut self, kind: &str) {
        let start_open = self.start_visible;
        let nav_ready = self.start_nav_ready;
        let controllers = self.session.controllers.len();
        stamp_hitch_report(
            &self.input_hub,
            kind,
            "button",
            Some(format!(
                "start_open={start_open} nav_ready={nav_ready} controllers={controllers}"
            )),
        );
    }

    pub(crate) fn log_start_nav_diag(
        &mut self,
        reading: &start_input::NavReading,
        pad_count: usize,
    ) {
        if self.nav_source_logged.is_none() {
            app_log::info(format!(
                "start-nav: source={} pads={} id={}",
                reading.source,
                pad_count,
                reading.id.as_str()
            ));
            self.nav_source_logged = Some(reading.source);
        }

        let snap = NavLogSnapshot::from_sample(&reading.sample);
        if self.last_nav_log != Some(snap) {
            app_log::info(snap.format_line(reading.source));
            self.last_nav_log = Some(snap);
        }
    }

    pub(crate) fn cycle_games_sort(&mut self) -> Option<Task<Message>> {
        // Sort stays live in edit mode: the checklist re-sorts under the same
        // mode and the prefs write persists across save/cancel.
        if self.start_state.overlay_blocking() {
            return None;
        }
        self.session.prefs.games_sort_mode = self.session.prefs.games_sort_mode.cycle();
        self.save_prefs();
        self.refresh_start_rows();
        self.start_state
            .note_position_bar_scroll(std::time::Instant::now());
        Some(self.warm_start_art_task())
    }

    pub(crate) fn on_start_cycle_sort(&mut self) -> Option<Task<Message>> {
        self.cycle_games_sort()
    }

    /// Native file picker; on Windows parents to Start so it clears immersive cover.
    pub(crate) fn pick_file_dialog(
        &self,
        filter_name: &'static str,
        extensions: &'static [&'static str],
        to_message: impl Fn(Option<PathBuf>) -> Message + Send + 'static,
    ) -> Task<Message> {
        let parent = match self.start_window.filter(|_| self.start_visible) {
            Some(id) => window_hwnd(id),
            None => Task::done(None),
        };
        let mut to_message = Some(to_message);
        parent.then(move |parent_hwnd| {
            let to_message = to_message
                .take()
                .expect("file dialog mapping consumed once");
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: file dialog parent hwnd={}",
                parent_hwnd
                    .map(|h| format!("0x{h:x}"))
                    .unwrap_or_else(|| "none".into())
            ));
            Task::perform(
                spawn_blocking(move || {
                    let mut dialog = rfd::FileDialog::new().add_filter(filter_name, extensions);
                    #[cfg(windows)]
                    if let Some(parent) = parent_hwnd.and_then(Win32DialogParent::new) {
                        dialog = dialog.set_parent(&parent);
                    }
                    #[cfg(not(windows))]
                    let _ = parent_hwnd;
                    dialog.pick_file().map(|p| p.to_string_lossy().into_owned())
                }),
                move |result| match result {
                    Ok(path) => to_message(path.map(PathBuf::from)),
                    Err(_) => to_message(None),
                },
            )
        })
    }

    pub(crate) fn edit_toggle_selected(&mut self) -> Option<Task<Message>> {
        let row = self
            .start_state
            .rows
            .get(self.start_state.game_selected)
            .cloned()?;
        let changed = match row.edit.as_ref() {
            Some(start_view::EditRow::Steam { appid, in_catalog }) => {
                self.edit_catalog_mut().toggle_steam(*appid, !*in_catalog)
            }
            Some(start_view::EditRow::Manual { id }) => {
                self.edit_catalog_mut().remove_manual_id(id)
            }
            None => false,
        };
        if changed {
            self.refresh_start_rows();
            Some(Task::none())
        } else {
            None
        }
    }

    pub(crate) fn complete_replace_confirm(&mut self) -> Task<Message> {
        let Some(confirm) = self.start_state.take_replace() else {
            return Task::none();
        };
        self.keyboard_cross_hold.reset();
        self.confirm_key_held = false;
        self.start_state.cross_progress = 0.0;
        let close_task = self.close_running_game();
        self.start_state.game_selected = confirm.next_index;
        if self.launch_selected() {
            return close_task.chain(self.close_start_screen());
        }
        close_task
    }

    /// Re-validate a row at press time (scan may be stale for immediate launches).
    pub(crate) fn is_row_invalid_now(&self, row: &start_view::StartRow) -> bool {
        if row.skeleton {
            return true;
        }
        // Steam URI: check installed list when known.
        if let Some(appid) = steam_appid_from_target(&row.target) {
            if let Some(ids) = self.steam_installed.as_deref() {
                return !ids.contains(&appid);
            }
            return false;
        }
        // Manual file target: absolute missing path is invalid.
        is_manual_target_missing(&row.target)
    }

    pub(crate) fn apply_pad_input_panel_from_readings(
        &mut self,
        readings: &[start_input::NavReading],
    ) {
        let next = if let Some(reading) = start_input::preferred_reading(readings) {
            let held: Vec<_> = reading.sample.held.iter().copied().collect();
            PadInputPanel {
                has_sample: true,
                source: reading.source.to_string(),
                buttons0: None,
                cross: reading.sample.cross,
                circle: reading.sample.circle,
                dpad_up: reading.sample.dpad_up,
                dpad_down: reading.sample.dpad_down,
                dpad_left: reading.sample.dpad_left,
                dpad_right: reading.sample.dpad_right,
                stick_band: start_input::StickBand::from_stick(
                    reading.sample.stick_x,
                    reading.sample.stick_y,
                )
                .as_str()
                .to_string(),
                stick_x: reading.sample.stick_x,
                stick_y: reading.sample.stick_y,
                held: gesture::format_gesture(&held),
            }
        } else {
            PadInputPanel::default()
        };
        self.pad_input_panel = next;
    }
}

// ---------------------------------------------------------------------------
// Pure dispatch guards (unit-tested; no iced runtime needed).
//
// These are the exact predicates `on_start_message_inner` (and its callers)
// gate on, extracted so the reducer stays a thin dispatch table.
// ---------------------------------------------------------------------------

/// Cold-ambient gate: only Close is live, everything else is dropped.
pub(crate) fn start_cold_allows(message: &StartMessage) -> bool {
    matches!(message, StartMessage::Close)
}

/// Whether pad directionals flow into the nav stepper, read off live Start
/// state. Settings and the reader browser own directionals (settings focus
/// moves / reader body scroll); otherwise they park while chrome animates
/// or a modal owns input. The stepper suppresses directionals entirely when
/// false. The engine browser drives its cursor manually per poll, so its
/// directionals must NOT flow here (or every pad move would double-fire).
pub(crate) fn start_nav_move_allowed(state: &start_view::State) -> bool {
    state.settings.open
        || (!state.animating()
            && state.replace_confirm.is_none()
            && state.manual_add.is_none()
            && state.edit_confirm.is_none())
}

/// Layout-transition gate: focus moves freeze mid-transition; Confirm /
/// Cancel / Close, settings-toggle, and controllers-toggle stay live.
pub(crate) fn start_focus_move_frozen(message: &StartMessage) -> bool {
    matches!(message, StartMessage::MoveUp | StartMessage::MoveDown)
}

#[cfg(test)]
mod tests {
    use super::{start_cold_allows, start_focus_move_frozen};
    use crate::ui::start::view::StartMessage;

    #[test]
    fn cold_ambient_allows_only_close() {
        assert!(start_cold_allows(&StartMessage::Close));
        for msg in [
            StartMessage::MoveUp,
            StartMessage::MoveDown,
            StartMessage::Confirm,
            StartMessage::ToggleEdit,
            StartMessage::ToggleSettings,
        ] {
            assert!(
                !start_cold_allows(&msg),
                "{msg:?} must be dropped when cold"
            );
        }
    }

    #[test]
    fn start_nav_movement_follows_settings_and_modals() {
        use super::start_nav_move_allowed;
        use crate::ui::start::view::{EditConfirm, ManualAddDraft, ReplaceConfirm, State};
        let mut state = State::default();
        assert!(start_nav_move_allowed(&state));
        state.settings.open = true;
        assert!(start_nav_move_allowed(&state));
        state.settings.open = false;
        state.manual_add = Some(ManualAddDraft {
            edit_id: None,
            target: String::new(),
            title: String::new(),
            args: String::new(),
            icon_path: None,
            shell_icon: None,
        });
        assert!(!start_nav_move_allowed(&state));
        state.manual_add = None;
        state.replace_confirm = Some(ReplaceConfirm {
            running_title: String::new(),
            next_title: String::new(),
            next_index: 0,
        });
        assert!(!start_nav_move_allowed(&state));
        state.replace_confirm = None;
        state.edit_confirm = Some(EditConfirm);
        assert!(!start_nav_move_allowed(&state));
    }

    #[test]
    fn layout_transition_freezes_only_focus_moves() {
        for msg in [StartMessage::MoveUp, StartMessage::MoveDown] {
            assert!(start_focus_move_frozen(&msg));
        }
        for msg in [
            StartMessage::Confirm,
            StartMessage::Close,
            StartMessage::ToggleSettings,
        ] {
            assert!(
                !start_focus_move_frozen(&msg),
                "{msg:?} stays live mid-transition"
            );
        }
    }
}
