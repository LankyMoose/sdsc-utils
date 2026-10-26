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
    START_NAV_NOT_READY_MS, chord_held_on_any_pad, delay, is_manual_target_missing, spawn_blocking,
    steam_appid_from_target,
};
use crate::AppResult;
use crate::controller::dualsense::rumble::MotorPulse;
use crate::games::process_match::RunningSession;
use crate::games::steam::SteamGame;
use crate::games::{self, GamesCatalog, launch, process_match, steam};
use crate::persist::analytics;
use crate::persist::prefs::{
    NavbarVisibility, ReducedMotion, StartAutoOpen, StartSelectionMode,
    clamp_start_screen_haptics_strength, clamp_start_screen_idle_timeouts,
    clamp_start_screen_inactive_dim_percent, clamp_start_screen_sound_volume,
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
            StartMessage::ToggleControllers => self.on_toggle_controllers(),
            StartMessage::ToggleEdit => match self.toggle_start_edit() {
                Some(task) => {
                    self.play_start_cue(UiSoundKind::Action);
                    task
                }
                None => Task::none(),
            },
            StartMessage::CycleSort => match self.on_start_cycle_sort() {
                Some(task) => {
                    self.play_start_cue(UiSoundKind::Action);
                    task
                }
                None => Task::none(),
            },
            StartMessage::AddShortcut => {
                if self.start_state.editing && !self.start_state.overlay_blocking() {
                    self.play_start_cue(UiSoundKind::Action);
                    self.pick_manual_shortcut()
                } else {
                    Task::none()
                }
            }
            StartMessage::EditManual => {
                let before = self.start_state.manual_add.is_some();
                let task = self.begin_manual_edit();
                if !before && self.start_state.manual_add.is_some() {
                    self.play_start_cue(UiSoundKind::Action);
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
                self.play_start_cue(UiSoundKind::Action);
                self.pick_manual_icon()
            }
            StartMessage::ManualClearIcon => {
                if let Some(draft) = self.start_state.manual_add.as_mut() {
                    draft.icon_path = None;
                    self.play_start_cue(UiSoundKind::Action);
                }
                Task::none()
            }
            StartMessage::ToggleSettings => {
                let now = Instant::now();
                let open = !self.start_state.settings.open;
                // The dock owns the right edge while open: never stack the
                // drawer over it (Create is likewise blocked while settings
                // owns input — see `on_toggle_controllers`).
                if open && self.start_state.dock_open_or_animating() {
                    return Task::none();
                }
                if self.start_state.request_settings(open, now) {
                    self.play_start_cue(UiSoundKind::Action);
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
            StartMessage::SetSounds(enabled) => self.apply_start_screen_sounds(enabled),
            StartMessage::SetSoundVolume(volume) => self.apply_start_screen_sound_volume(volume),
            StartMessage::SetHaptics(enabled) => self.apply_start_screen_haptics(enabled),
            StartMessage::SetHapticsStrength(strength) => {
                self.apply_start_screen_haptics_strength(strength)
            }
            StartMessage::SetPerfThrottle(enabled) => self.apply_perf_throttle_immersive(enabled),
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
                    self.play_start_cue(UiSoundKind::Action);
                    task
                }
                None => Task::none(),
            }
        } else if self.launch_selected() {
            self.play_start_cue(UiSoundKind::Action);
            self.close_start_screen()
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
        self.play_start_cue(UiSoundKind::Nav);
        task
    }

    /// Focus-move arm: `delta` is -1 (up) or +1 (down).
    pub(crate) fn on_start_move(&mut self, delta: i32) -> Task<Message> {
        let prev_game = self.start_state.game_selected;
        let direction = self.start_state.move_selection(delta);
        let mut task = Task::none();
        if direction.is_some() {
            self.play_start_cue(UiSoundKind::Nav);
            if !self.start_state.dock_expanded && self.start_state.game_selected != prev_game {
                self.start_state
                    .begin_strip_anim(prev_game as f32, Instant::now());
                self.start_state.reset_backdrop_fade();
                task = task.chain(self.warm_start_art_task());
            }
        }
        task
    }

    /// Close/Cancel arm: unwinds topmost surface first (settings → manual-add
    /// draft → replace confirm → edit mode → Start itself).
    pub(crate) fn on_start_close(&mut self) -> Task<Message> {
        if self.start_state.settings.open {
            self.play_start_cue(UiSoundKind::Action);
            let now = Instant::now();
            if self.start_state.request_settings(false, now) {
                crate::controller::hid::diag::diag_info("ui-diag: start settings close");
            }
            Task::none()
        } else if self.start_state.manual_add.is_some() {
            self.play_start_cue(UiSoundKind::Action);
            self.cancel_manual_add()
        } else if self.start_state.replace_confirm.take().is_some() {
            self.play_start_cue(UiSoundKind::Action);
            Task::none()
        } else if self.start_state.editing {
            self.play_start_cue(UiSoundKind::Action);
            self.cancel_start_edit()
        } else if self.start_visible {
            // Controllers surface open: Circle/Cancel collapses it first.
            if start_mode::dock_collapse_target(self.start_state.dock_expanded).is_some()
                && self.start_state.request_dock(false, Instant::now())
            {
                self.play_start_cue(UiSoundKind::Slide);
                crate::controller::hid::diag::diag_info(
                    "ui-diag: start controllers collapse (cancel)",
                );
                return Task::none();
            }
            self.play_start_cue(UiSoundKind::Action);
            self.close_start_screen()
        } else {
            Task::none()
        }
    }

    /// Create-toggle arm: flips the controllers surface both ways.
    /// An in-progress edit draft is discarded first and a stowed
    /// settle scroll is dropped (the user took over post-settle browsing).
    pub(crate) fn on_toggle_controllers(&mut self) -> Task<Message> {
        if self.start_state.overlay_blocking() {
            return Task::none();
        }
        if self.start_state.editing {
            let cursor = self.start_state.selected_game_key();
            let anchor = self.start_state.edit_anchor_play_key.clone();
            self.discard_edit_draft();
            self.refresh_start_rows();
            let target = self
                .start_state
                .cancel_exit_target_key(cursor.as_deref(), anchor.as_deref());
            self.start_state
                .resolve_edit_exit_selection(target.as_deref());
        }
        let now = Instant::now();
        let next = !self.start_state.dock_expanded;
        let target = if next {
            start_mode::dock_expand_target(self.start_state.dock_expanded)
        } else {
            start_mode::dock_collapse_target(self.start_state.dock_expanded)
        };
        if target.is_some() && self.start_state.request_dock(next, now) {
            self.play_start_cue(UiSoundKind::Slide);
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: start controllers {} (create)",
                if next { "open" } else { "close" },
            ));
        }
        Task::none()
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
                    let mut row = start_view::StartRow::from_entry(
                        entry,
                        &self.steam_by_id,
                        self.steam_scan_pending,
                    );
                    row.played_at_ms = self.effective_played_ms(entry);
                    row.disabled = self.is_entry_disabled(entry, &row);
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
        self.start_state.manual_add = None;
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

    pub(crate) fn cancel_manual_add(&mut self) -> Task<Message> {
        self.start_state.manual_add = None;
        Task::none()
    }

    pub(crate) fn toggle_start_edit(&mut self) -> Option<Task<Message>> {
        if self.start_state.dock_expanded
            || self.start_state.overlay_blocking()
            || self.start_state.animating()
        {
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
        self.start_state.manual_add = Some(start_view::ManualAddDraft {
            edit_id: Some(id.clone()),
            target: row.target,
            title: row.title,
            args: row.args,
            icon_path,
            shell_icon,
        });
        Task::none()
    }

    pub(crate) fn on_start_confirm(&mut self) -> Task<Message> {
        if self.start_state.manual_add.is_some() {
            self.play_start_cue(UiSoundKind::Action);
            return self.confirm_manual_add();
        }
        // Replace confirm requires hold-Cross / hold-Enter (see pad poll / key hold).
        if self.start_state.replace_confirm.is_some() {
            return Task::none();
        }
        if self.start_state.dock_expanded {
            if let Some(row) = self.start_state.selected_controller()
                && row.connected
            {
                let serial = row.serial.clone();
                if self.identify(&serial) {
                    self.popup_state.begin_identify_flash(&serial);
                    self.start_state.begin_identify_flash(&serial);
                    self.play_start_cue(UiSoundKind::Action);
                }
            }
            return Task::none();
        }
        if self.start_state.editing {
            return match self.edit_toggle_selected() {
                Some(task) => {
                    self.play_start_cue(UiSoundKind::Action);
                    task
                }
                None => Task::none(),
            };
        }
        if self.launch_selected() {
            self.play_start_cue(UiSoundKind::Action);
            return self.close_start_screen();
        }
        Task::none()
    }

    /// Launch the selected game. Returns true when launch, replace-confirm, or a real attempt ran.
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

        // Disabled rows never launch (uninstalled Steam or missing manual target).
        // Re-validate at launch time so a stale row (scan race / immediate launch
        // after uninstall) fails gracefully instead of opening Steam / error popup.
        if row.disabled || self.is_row_invalid_now(&row) {
            app_log::warn(format!(
                "launch blocked (no longer installed): {} ({})",
                row.title, row.target
            ));
            // Refresh disabled flags so the row dims immediately.
            self.refresh_start_rows();
            return false;
        }

        // Steam pre-flight (revoked license / wiped files, e.g. an expired
        // alpha): the manifest can linger while the install dir is missing
        // or empty, and ShellExecute would happily hand off to a Steam error
        // dialog. Block before creating a running session so `Playing`
        // never parks on a game that cannot spawn a process.
        if let Some(appid) = steam_appid_from_target(&row.target)
            && !steam::install_ready_for_appid(appid)
        {
            app_log::warn(format!(
                "launch blocked (steam install not ready): {} (appid {appid})",
                row.title
            ));
            self.launch_blocked_steam.insert(appid);
            self.refresh_start_rows();
            return false;
        }

        if let Some(session) = self.running_session.as_ref() {
            if session.matches_target(&row.target) {
                return false;
            }
            if process_match::any_matching_running(&session.match_paths) {
                self.start_state.replace_confirm = Some(ReplaceConfirm {
                    running_title: session.title.clone(),
                    next_title: row.title.clone(),
                    next_index: self.start_state.game_selected,
                });
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
                // refresh so the row dims instead of staying launchable.
                self.refresh_start_rows();
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
        self.start_state.manual_add = Some(start_view::ManualAddDraft {
            edit_id: None,
            target,
            title,
            args: String::new(),
            icon_path: None,
            shell_icon,
        });
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
    pub(crate) fn play_start_sound(&self, kind: UiSoundKind) {
        if !self.session.prefs.start_screen_sounds_enabled {
            return;
        }
        let volume = f32::from(self.session.prefs.start_screen_sound_volume) / 100.0;
        self.sound.play(kind, volume);
    }

    /// Sound + optional pad haptic for a start-screen cue.
    pub(crate) fn play_start_cue(&self, kind: UiSoundKind) {
        self.play_start_sound(kind);
        self.play_start_haptic(kind);
    }

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
            crate::ui::start::settings::SettingsCue::Full => self.play_start_cue(kind),
            crate::ui::start::settings::SettingsCue::HapticOnly => self.play_start_haptic(kind),
            crate::ui::start::settings::SettingsCue::SoundOnly => self.play_start_sound(kind),
        }
    }

    pub(crate) fn play_start_haptic(&self, kind: UiSoundKind) {
        if !self.session.prefs.start_screen_haptics_enabled
            || crate::platform::debug_quiet::enabled()
        {
            return;
        }
        let Some(serial) = self.haptic_pad_serial.as_ref() else {
            return;
        };
        let pulse = match kind {
            UiSoundKind::Nav => MotorPulse::NAV,
            UiSoundKind::Action => MotorPulse::ACTION,
            UiSoundKind::Hold => MotorPulse::HOLD,
            UiSoundKind::Slide => MotorPulse::SLIDE,
        };
        let (right, left, duration) =
            pulse.scaled(self.session.prefs.start_screen_haptics_strength);
        if right == 0 && left == 0 {
            return;
        }
        self.hid_worker
            .rumble(serial.clone(), right, left, duration.as_millis() as u64);
    }

    /// Preview haptic on every connected DualSense (settings toggle / strength slider).
    pub(crate) fn preview_haptic_all(&self, pulse: MotorPulse) {
        if !self.session.prefs.start_screen_haptics_enabled
            || crate::platform::debug_quiet::enabled()
        {
            return;
        }
        let (right, left, duration) =
            pulse.scaled(self.session.prefs.start_screen_haptics_strength);
        if right == 0 && left == 0 {
            return;
        }
        let ms = duration.as_millis() as u64;
        for controller in &self.session.controllers {
            self.hid_worker
                .rumble(controller.serial.clone(), right, left, ms);
        }
    }

    pub(crate) fn set_haptic_pad_from_id(&mut self, pad: Option<&start_input::PadId>) {
        self.haptic_pad_serial = pad.and_then(|id| {
            id.as_str()
                .strip_prefix("hid:")
                .map(crate::controller::model::Serial::normalize)
        });
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
    /// with the controllers surface closed and no overlays
    /// (settings / replace-confirm / manual-add) open. Edit mode is a mode,
    /// not an overlay, so it stays included: left/right move selection (all
    /// rows are enabled in the checklist) while Up/Down are suppressed.
    pub(crate) fn immersive_horizontal_games_nav(&self) -> bool {
        matches!(
            self.session.prefs.start_screen_immersive_layout,
            crate::persist::prefs::ImmersiveLayout::Horizontal
        ) && !self.start_state.dock_expanded
            && !self.start_state.settings.open
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

        // Reopen chord while open is swallowed (PS opens when closed).
        self.swallow_reopen_chord_while_open(readings);

        // Debug R3: hide/show all chrome while tuning (pad-only; R3 carries
        // no NavAction). Tuning-scoped, so it never fires in normal chrome.
        #[cfg(debug_assertions)]
        if let Some(task) = self.toggle_ambient_chrome_from_r3(readings) {
            return task;
        }

        let animating = self.start_state.animating();
        // Mid-anim: skip controller-row rebuilds (those hitch the UI thread).
        let badge_task = if !animating {
            let controllers_started = Instant::now();
            self.refresh_start_controllers();
            let controllers_ms = controllers_started.elapsed().as_millis();

            let badge_started = Instant::now();
            let badge = self.refresh_running_badge();
            let badge_ms = badge_started.elapsed().as_millis();

            if controllers_ms >= PAD_POLL_SLOW_MS || badge_ms >= PAD_POLL_SLOW_MS {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: pad-poll slow controllers_ms={controllers_ms} badge_ms={badge_ms}"
                ));
            }
            badge
        } else {
            Task::none()
        };

        self.start_state.tick_anim(now);

        let replace_confirm = self.start_state.replace_confirm.is_some();
        let manual_add = self.start_state.manual_add.is_some();
        let settings_open = self.start_state.settings.open;
        let allow_nav_move = settings_open || (!animating && !replace_confirm && !manual_add);
        let editing = self.start_state.editing;
        // Only when the selected row shows Close game (running target match).
        let hold_cross_close = !editing
            && !replace_confirm
            && !manual_add
            && !settings_open
            && !animating
            && !self.start_state.dock_expanded
            && self.start_state.running_target.as_ref().is_some_and(|t| {
                self.start_state
                    .rows
                    .get(self.start_state.game_selected)
                    .is_some_and(|row| &row.target == t)
            });
        // Match the row hint: Triangle hold only when Power off is shown.
        let hold_triangle_power = !editing
            && !replace_confirm
            && !manual_add
            && !settings_open
            && !animating
            && self.start_state.dock_expanded
            && self
                .start_state
                .selected_controller()
                .is_some_and(|row| row.show_power_off());
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
        } else if tick.action == Some(NavAction::ToggleSettings) {
            self.start_state.tick_hint_anims(now);
            self.on_start_message(StartMessage::ToggleSettings)
        } else if tick.action == Some(NavAction::ToggleControllers) {
            self.start_state.tick_hint_anims(now);
            self.on_start_message(StartMessage::ToggleControllers)
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
                    NavAction::ToggleControllers => {
                        self.on_start_message(StartMessage::ToggleControllers)
                    }
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
                self.play_start_cue(UiSoundKind::Hold);
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
                    self.play_start_cue(UiSoundKind::Hold);
                    close_task = self.close_running_game_if_selected();
                }
            } else {
                self.start_state.cross_progress = 0.0;
            }

            if hold_triangle_power {
                self.start_state.triangle_progress = tick.triangle_progress;
                if tick.triangle_completed
                    && let Some(row) = self.start_state.selected_controller()
                    && row.show_power_off()
                {
                    let serial = row.serial.clone();
                    self.play_start_cue(UiSoundKind::Hold);
                    self.power_off(&serial);
                }
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
                    // Left/Right: controllers selection when open, else ignore.
                    NavAction::Left if self.start_state.dock_expanded => {
                        self.on_start_message(StartMessage::MoveUp)
                    }
                    NavAction::Right if self.start_state.dock_expanded => {
                        self.on_start_message(StartMessage::MoveDown)
                    }
                    NavAction::Left | NavAction::Right => Task::none(),
                    NavAction::ToggleControllers => {
                        self.on_start_message(StartMessage::ToggleControllers)
                    }
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
            self.open_start_screen()
        } else {
            Task::none()
        }
    }

    /// Rising-edge reopen chord while Start is open is swallowed (PS opens when
    /// closed). Still consumes the sticky hold through the release gate so the
    /// opening press cannot immediately re-fire.
    pub(crate) fn swallow_reopen_chord_while_open(&mut self, readings: &[start_input::NavReading]) {
        if !self.start_visible || readings.is_empty() {
            return;
        }
        if self.start_state.overlay_blocking() {
            return;
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
            }
            ChordReleaseTick::Cleared | ChordReleaseTick::Open => {
                let _ = self.gesture_detectors.update(&required, readings);
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
        }
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
            self.session.start_auto_open_pending = false;
            return Task::none();
        }
        if mode.includes_usb() == old.includes_usb() {
            return Task::none();
        }
        // Scope flip may CloseStart when USB pads no longer qualify; never opens.
        let ctx = crate::session::ApplyContext {
            start_visible: self.start_visible,
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

    pub(crate) fn apply_start_screen_sounds(&mut self, enabled: bool) -> Task<Message> {
        self.session.prefs.start_screen_sounds_enabled = enabled;
        self.save_prefs();
        if enabled {
            self.play_start_sound(UiSoundKind::Nav);
        }
        Task::none()
    }

    pub(crate) fn apply_start_screen_sound_volume(&mut self, volume: u8) -> Task<Message> {
        let volume = clamp_start_screen_sound_volume(volume);
        if self.session.prefs.start_screen_sound_volume != volume {
            self.session.prefs.start_screen_sound_volume = volume;
            self.save_prefs();
            if self.session.prefs.start_screen_sounds_enabled {
                self.play_start_sound(UiSoundKind::Nav);
            }
        }
        Task::none()
    }

    pub(crate) fn apply_start_screen_haptics(&mut self, enabled: bool) -> Task<Message> {
        self.session.prefs.start_screen_haptics_enabled = enabled;
        self.save_prefs();
        if enabled {
            self.preview_haptic_all(MotorPulse::NAV);
        } else {
            self.hid_worker.rumble_stop_all();
        }
        Task::none()
    }

    pub(crate) fn apply_start_screen_haptics_strength(&mut self, strength: u8) -> Task<Message> {
        let strength = clamp_start_screen_haptics_strength(strength);
        if self.session.prefs.start_screen_haptics_strength != strength {
            self.session.prefs.start_screen_haptics_strength = strength;
            self.save_prefs();
            if self.session.prefs.start_screen_haptics_enabled {
                self.preview_haptic_all(MotorPulse::NAV);
            }
        }
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
        self.play_start_cue(UiSoundKind::Nav);
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
                    self.play_start_cue(UiSoundKind::Nav);
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
                    self.play_start_cue(UiSoundKind::Nav);
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
                self.play_start_cue(UiSoundKind::Nav);
                self.scroll_settings_focus_into_view(start_view::ScrollReveal::Either)
            }
            NavAction::Down => {
                let next = (highlight as isize + 1).rem_euclid(count) as usize;
                self.start_state.settings.set_menu_highlight(next);
                self.play_start_cue(UiSoundKind::Nav);
                self.scroll_settings_focus_into_view(start_view::ScrollReveal::Either)
            }
            NavAction::Confirm => {
                let select = row.selector_select(highlight);
                self.play_start_cue(UiSoundKind::Action);
                self.on_start_message(select)
            }
            NavAction::Cancel | NavAction::ToggleSettings => {
                self.start_state.settings.close_menus();
                self.play_start_cue(UiSoundKind::Nav);
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
        // Keep uninstalled Steam entries so rows can render disabled instead of
        // vanishing. Disabled is resolved per-row in refresh_start_rows.
        self.games.merge_sorted(
            None,
            self.session.prefs.games_sort_mode,
            |entry| match entry {
                crate::games::GameEntry::Steam { appid } => steam_by_id
                    .get(appid)
                    .map(|g| g.name.clone())
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

    /// True when scan proves this entry is no longer installed (disabled row).
    pub(crate) fn is_entry_disabled(
        &self,
        entry: &crate::games::GameEntry,
        row: &start_view::StartRow,
    ) -> bool {
        if row.skeleton {
            return false;
        }
        match entry {
            crate::games::GameEntry::Steam { appid } => {
                // Pre-flight failures dim the row until a fresh scan proves
                // the install back.
                if self.launch_blocked_steam.contains(appid) {
                    return true;
                }
                // Unknown scan (None) keeps rows enabled; known scan disables missing.
                match self.steam_installed.as_deref() {
                    Some(ids) => !ids.contains(appid),
                    None => false,
                }
            }
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
        for entry in &catalog.entries {
            if let Some(mut row) = start_view::StartRow::manual_edit(entry) {
                row.played_at_ms = catalog.last_played(entry);
                rows.push(row);
            }
        }
        start_view::sort_edit_rows_by_mode(&mut rows, sort_mode);
        rows
    }

    pub(crate) fn refresh_start_controllers(&mut self) {
        self.start_state.show_all_controllers = self.session.prefs.show_all_controllers;
        let mut rows: Vec<_> = self
            .session
            .controllers
            .iter()
            .map(|c| {
                let nickname = self.session.known.nickname(c.serial.as_ref());
                let eta = if self.session.prefs.analytics_enabled {
                    self.session
                        .analytics
                        .eta_for(c)
                        .map(analytics::format_eta_ring)
                } else {
                    None
                };
                start_view::StartControllerRow {
                    serial: c.serial.clone(),
                    title: start_view::controller_title(&c.product, nickname),
                    connection: c.connection.to_string(),
                    state: if c.is_low_battery(self.session.prefs.low_battery_percent) {
                        "low battery".into()
                    } else {
                        start_view::power_state_label(c.state).into()
                    },
                    percent: c.percent,
                    low: c.is_low_battery(self.session.prefs.low_battery_percent),
                    bluetooth: c.connection.is_bluetooth() && c.capabilities.power_off,
                    connected: true,
                    eta,
                }
            })
            .collect();
        if self.session.prefs.show_all_controllers {
            for controller in self
                .session
                .known
                .remembered_disconnected(&self.session.controllers)
            {
                let nickname = self.session.known.nickname(controller.serial.as_ref());
                let eta = if self.session.prefs.analytics_enabled {
                    self.session
                        .analytics
                        .eta_play_at(controller.serial.as_ref(), controller.percent)
                        .map(analytics::format_eta_ring)
                } else {
                    None
                };
                rows.push(start_view::StartControllerRow {
                    serial: controller.serial.clone(),
                    title: start_view::controller_title(&controller.product, nickname),
                    connection: controller.connection.clone(),
                    state: "disconnected".into(),
                    percent: controller.percent,
                    low: false,
                    bluetooth: false,
                    connected: false,
                    eta,
                });
            }
        }
        self.start_state.set_controllers(rows);
        // Keep a restored selection pinned to its pad across membership changes.
        // Controller axis only (never gated by the game-axis persist pref):
        // the game axis resolves as disabled so it stays untouched.
        if let Some(selection) = self.start_selection_shadow.clone() {
            self.start_state.resolve_start_selection(
                None,
                selection.controller_key.as_deref(),
                false,
                StartSelectionMode::ByGame,
                None,
            );
        }
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
        let Some(draft) = self.start_state.manual_add.take() else {
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
            start_on_controllers: self.start_state.dock_expanded,
            game_key: self
                .start_state
                .rows
                .get(self.start_state.game_selected)
                .map(|r| r.play_key.clone()),
            game_index: Some(self.start_state.game_selected),
            controller_key: self
                .start_state
                .controllers
                .get(self.start_state.controller_selected)
                .map(|r| r.serial.to_string()),
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
    /// rebuilds: the Start open path resets nav state (`reset_to_games`),
    /// which would wipe restored drawer/scroll/selection state. The restore
    /// open bypasses that reset (see `restore_preserve_nav`) and resolves
    /// selections from the shadow keys instead. Scroll offsets land on
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
            if rs.game_key.is_some() || rs.controller_key.is_some() {
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
            if rs.start_on_controllers {
                self.start_state.dock_expanded = true;
            }
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
        self.start_state.editing = false;
        self.start_state.edit_anchor_play_key = None;
        self.edit_draft = None;
        self.refresh_start_rows();
        self.refresh_start_controllers();

        // Restore opens bypass the nav reset: reload restore already set
        // drawer/scroll/selection state above, and `reset_to_games` would
        // wipe it (drawer close, scroll zero, selections zeroed). Pad nav
        // arming still runs either way.
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
            self.session.start_auto_open_pending = false;
            self.last_running_check = None;
            self.arm_chord_release_latch();
            let badge = self.refresh_running_badge();
            return Task::batch([badge, focus_window(id)]);
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
            // Leave start_auto_open_pending set so WindowClosed can retry.
            crate::controller::hid::diag::diag_info(
                "ui-diag: start open skipped (window still closing)",
            );
            return Task::none();
        }

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
            level: window::Level::AlwaysOnTop,
            exit_on_close_request: true,
            platform_specific: overlay_platform_specific(),
            ..window::Settings::default()
        });
        self.start_window = Some(id);
        self.start_visible = true;
        self.session.start_auto_open_pending = false;
        crate::platform::wgpu_diag::note_start_open();
        Task::batch([badge, restored_art, open.map(Message::StartOpened)])
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

    /// Reset to Games and (optionally) require a full control release before nav fires.
    pub(crate) fn prepare_start_nav_on_open(&mut self, arm_immediately: bool) {
        self.start_state.reset_to_games();
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
        self.start_nav_ready = false;
        self.start_revealed_at = None;
        // Tuning is settled state: never reopen with it on.
        #[cfg(debug_assertions)]
        self.start_state.request_ambient_tuning(false);
        self.start_state.clear_immersive_session();
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
        self.start_state.reset_to_games();
        self.discard_edit_draft();
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
            return Task::done(Message::FinishStartClose);
        }
        self.hide_and_close_start_window()
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
        if self.start_state.dock_expanded || self.start_state.overlay_blocking() {
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
        if self.start_state.dock_expanded {
            let next = !self.session.prefs.show_all_controllers;
            self.set_show_all_controllers(next)
        } else {
            self.cycle_games_sort()
        }
    }

    pub(crate) fn set_show_all_controllers(&mut self, show_all: bool) -> Option<Task<Message>> {
        if !self.start_state.dock_expanded
            || self.start_state.overlay_blocking()
            || self.session.prefs.show_all_controllers == show_all
        {
            return None;
        }
        self.session.prefs.show_all_controllers = show_all;
        self.save_prefs();
        app_log::hid_trace(format!(
            "start-controllers: show_all={}",
            u8::from(show_all)
        ));
        self.refresh_start_controllers();
        Some(Task::none())
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
        let Some(confirm) = self.start_state.replace_confirm.take() else {
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
            StartMessage::ToggleControllers,
        ] {
            assert!(
                !start_cold_allows(&msg),
                "{msg:?} must be dropped when cold"
            );
        }
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
            StartMessage::ToggleControllers,
        ] {
            assert!(
                !start_focus_move_frozen(&msg),
                "{msg:?} stays live mid-transition"
            );
        }
    }
}
