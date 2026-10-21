//! Start screen core reducer: message dispatch + selection scroll reveals.
//!
//! Behavior-preserving extraction from `app::App`; no logic change.

use super::{App, Message, steam_appid_from_target};
use crate::games::{launch, process_match, steam};
use crate::platform::app_log;
use crate::platform::ui_sound::UiSoundKind;
use crate::ui::start::mode as start_mode;
use crate::ui::start::view::{self as start_view, ReplaceConfirm, StartMessage, StartSlide};
use iced::Task;
use iced::widget::operation;
use std::path::PathBuf;
use std::time::Instant;

impl App {
    /// All Start selection changes funnel through here (mouse, keyboard, pad).
    pub(crate) fn on_start_message(&mut self, message: StartMessage) -> Task<Message> {
        self.on_start_message_inner(message)
    }

    pub(crate) fn on_start_message_inner(&mut self, message: StartMessage) -> Task<Message> {
        // Cold ambient: only Close is live (escape hatch); ignore nav/launch/edit.
        if self.start_state.cold_load_active && !matches!(message, StartMessage::Close) {
            return Task::none();
        }
        // Layout transition reflows main content: drop navigational messages for its
        // duration (pad Up/Down already gate on animating(); this also covers
        // keyboard dispatch and pad slide-switch). Confirm/Cancel/Close and
        // settings-toggle stay live (cancellable chrome), as do deliberate
        // settings commits (open-menu Confirm picks, mouse Set* writes) —
        // only focus moves and slider nudges freeze mid-transition.
        if self.start_state.layout_transition_active()
            && matches!(
                message,
                StartMessage::MoveUp
                    | StartMessage::MoveDown
                    | StartMessage::PrevSlide
                    | StartMessage::NextSlide
            )
        {
            return Task::none();
        }
        // Keyboard / UI actions other than pure scroll cancel an in-progress hold.
        match &message {
            StartMessage::GamesScrolled(..)
            | StartMessage::ControllersScrolled(..)
            | StartMessage::SettingsScrolled(..)
            | StartMessage::ManualAddTitle(_)
            | StartMessage::ManualAddArgs(_) => {}
            #[cfg(debug_assertions)]
            StartMessage::ReportLightbarFailure | StartMessage::ReportInputFailure => {}
            _ => self.cancel_start_holds(),
        }
        match message {
            StartMessage::Launch(index) => {
                if self.start_state.immersive
                    && index < self.start_state.rows.len()
                    && index != self.start_state.game_selected
                {
                    // Neighbor capsule click selects only (center / Cross launches).
                    return self.on_start_message(StartMessage::SelectGame(index));
                }
                if index < self.start_state.rows.len() {
                    self.start_state.game_selected = index;
                }
                let scroll = self.scroll_start_selection_into_view();
                if self.start_state.editing {
                    match self.edit_toggle_selected() {
                        Some(task) => {
                            self.play_start_cue(UiSoundKind::Action);
                            scroll.chain(task)
                        }
                        None => scroll,
                    }
                } else if self.launch_selected() {
                    self.play_start_cue(UiSoundKind::Action);
                    if self.start_state.immersive {
                        scroll.chain(self.close_start_screen())
                    } else {
                        scroll
                    }
                } else {
                    scroll
                }
            }
            StartMessage::SelectGame(index) => {
                if index >= self.start_state.rows.len() || index == self.start_state.game_selected {
                    return Task::none();
                }
                let prev_game = self.start_state.game_selected;
                self.start_state.game_selected = index;
                let mut task = self.scroll_start_selection_into_view();
                if self.start_state.immersive {
                    self.start_state
                        .begin_strip_anim(prev_game as f32, Instant::now());
                    self.start_state.reset_backdrop_fade();
                }
                task = task.chain(self.warm_start_art_task());
                self.play_start_cue(UiSoundKind::Nav);
                task
            }
            StartMessage::SelectController(index) => {
                if index < self.start_state.controllers.len()
                    && self.start_state.controller_selected != index
                {
                    self.start_state.controller_selected = index;
                    self.play_start_cue(UiSoundKind::Nav);
                }
                if self.start_state.immersive && !self.start_state.dock_expanded {
                    let _ = self.start_state.request_dock(true, Instant::now());
                }
                self.scroll_start_selection_into_view()
            }
            StartMessage::MoveUp => {
                let prev_game = self.start_state.game_selected;
                let direction = self.start_state.move_selection(-1);
                let mut task = self.scroll_start_selection_into_view_dir(
                    direction.unwrap_or(start_view::ScrollReveal::Up),
                );
                if direction.is_some() {
                    self.play_start_cue(UiSoundKind::Nav);
                    if matches!(self.start_state.slide, StartSlide::Games)
                        && self.start_state.game_selected != prev_game
                    {
                        if self.start_state.immersive {
                            self.start_state
                                .begin_strip_anim(prev_game as f32, Instant::now());
                            self.start_state.reset_backdrop_fade();
                        }
                        task = task.chain(self.warm_start_art_task());
                    }
                }
                task
            }
            StartMessage::MoveDown => {
                let prev_game = self.start_state.game_selected;
                let direction = self.start_state.move_selection(1);
                let mut task = self.scroll_start_selection_into_view_dir(
                    direction.unwrap_or(start_view::ScrollReveal::Down),
                );
                if direction.is_some() {
                    self.play_start_cue(UiSoundKind::Nav);
                    if matches!(self.start_state.slide, StartSlide::Games)
                        && self.start_state.game_selected != prev_game
                    {
                        if self.start_state.immersive {
                            self.start_state
                                .begin_strip_anim(prev_game as f32, Instant::now());
                            self.start_state.reset_backdrop_fade();
                        }
                        task = task.chain(self.warm_start_art_task());
                    }
                }
                task
            }
            StartMessage::Confirm => self.on_start_confirm(),
            StartMessage::Close => {
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
                    // Immersive + dock open: Circle collapses the drawer first (same as Left).
                    if self.start_state.immersive
                        && start_mode::dock_collapse_target(self.start_state.dock_expanded)
                            .is_some()
                        && self.start_state.request_dock(false, Instant::now())
                    {
                        self.play_start_cue(UiSoundKind::Slide);
                        crate::controller::hid::diag::diag_info(
                            "ui-diag: start dock collapse (cancel)",
                        );
                        return Task::none();
                    }
                    self.play_start_cue(UiSoundKind::Action);
                    match start_mode::on_cancel(
                        self.start_presentation(),
                        self.session.prefs.start_screen_always_immersive,
                    ) {
                        start_mode::ImmersiveTransition::Demote => self.leave_start_immersive(),
                        _ => self.close_start_screen(),
                    }
                } else {
                    Task::none()
                }
            }
            StartMessage::PrevSlide => {
                if self.start_state.overlay_blocking() || self.start_state.transition.is_some() {
                    return Task::none();
                }
                // User took over post-settle browsing: a stowed settle
                // scroll is moot (its target predates this slide change).
                self.deferred_settle_scroll = None;
                if self.start_state.editing {
                    // Slide-switch discards the draft (cancel-equivalent): keep
                    // navigation like cancel (cursor first, anchor fallback) so
                    // the stale edit-mode index (checklist order) never lands
                    // on the wrong browse row.
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
                if self.start_state.immersive {
                    // Left: collapse controllers island.
                    if start_mode::dock_collapse_target(self.start_state.dock_expanded).is_some()
                        && self.start_state.request_dock(false, now)
                    {
                        self.play_start_cue(UiSoundKind::Slide);
                        crate::controller::hid::diag::diag_info("ui-diag: start dock collapse");
                    }
                } else if self.start_state.request_slide(StartSlide::Games, now) {
                    // Left: Controllers → Games only (no wrap). Interruptible mid-anim.
                    self.play_start_cue(UiSoundKind::Slide);
                }
                Task::none()
            }
            StartMessage::NextSlide => {
                if self.start_state.overlay_blocking() || self.start_state.transition.is_some() {
                    return Task::none();
                }
                // Same stowed-scroll drop as PrevSlide (user took over).
                self.deferred_settle_scroll = None;
                if self.start_state.editing {
                    // Same cancel-equivalent discard as PrevSlide (see above).
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
                if self.start_state.immersive {
                    // Right: expand controllers island.
                    if start_mode::dock_expand_target(self.start_state.dock_expanded).is_some()
                        && self.start_state.request_dock(true, now)
                    {
                        self.play_start_cue(UiSoundKind::Slide);
                        crate::controller::hid::diag::diag_info("ui-diag: start dock expand");
                    }
                } else if self.start_state.request_slide(StartSlide::Controllers, now) {
                    // Right: Games → Controllers only (no wrap). Interruptible mid-anim.
                    self.play_start_cue(UiSoundKind::Slide);
                }
                Task::none()
            }
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
            StartMessage::GamesScrolled(y, viewport_h) => {
                self.start_state
                    .record_list_scroll(start_view::StartListId::Games, y, viewport_h);
                // Demote remount correction: fresh reports carry the real
                // viewport (settle math may have used a stale/estimate one).
                // Re-land the center invisibly behind the enter veil instead
                // of snapping at lift end. Nav is blocked while the phase is
                // live, so reports here are mount/scroll_to echoes, never
                // user browsing.
                if matches!(
                    self.start_state.transition_phase.map(|(p, _)| p),
                    Some(start_mode::TransitionPhase::EnterCompact)
                ) {
                    return self.correct_demoted_games_reveal(y);
                }
                Task::none()
            }
            StartMessage::ControllersScrolled(y, viewport_h) => {
                self.start_state.record_list_scroll(
                    start_view::StartListId::Controllers,
                    y,
                    viewport_h,
                );
                Task::none()
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
            StartMessage::SetAlwaysImmersive(enabled) => {
                self.apply_start_screen_always_immersive(enabled)
            }
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

    pub(crate) fn scroll_start_selection_into_view(&mut self) -> Task<Message> {
        self.scroll_start_selection_into_view_dir(start_view::ScrollReveal::Either)
    }

    pub(crate) fn scroll_start_selection_into_view_dir(
        &mut self,
        direction: start_view::ScrollReveal,
    ) -> Task<Message> {
        let Some((id, y)) = self.start_state.selection_scroll_y(direction) else {
            return Task::none();
        };
        self.start_state.note_scroll_y(&id, y);
        operation::scroll_to(
            id,
            operation::AbsoluteOffset {
                x: None,
                y: Some(y),
            },
        )
    }

    pub(crate) fn scroll_start_selection_to_center(&mut self, force: bool) -> Task<Message> {
        let target = if force {
            self.start_state.selection_scroll_y_center_prefer()
        } else {
            self.start_state.selection_scroll_y_center()
        };
        let Some((id, y)) = target else {
            return Task::none();
        };
        self.start_state.note_scroll_y(&id, y);
        operation::scroll_to(
            id,
            operation::AbsoluteOffset {
                x: None,
                y: Some(y),
            },
        )
    }

    /// Open-time reveal for the current slide's selection (Games force-centers,
    /// Controllers pins bidirectionally). Must run only once the window's
    /// scrollables exist (`StartOpened`, re-focus, post-resize settle) — never
    /// batched with `window::open`. No-op when already in view; harmless when
    /// immersive (strip/dock own the selection, the compact id is unmounted).
    pub(crate) fn scroll_start_selection_for_open(&mut self) -> Task<Message> {
        let Some((id, y)) = self.start_state.open_reveal_scroll() else {
            return Task::none();
        };
        self.start_state.note_scroll_y(&id, y);
        operation::scroll_to(
            id,
            operation::AbsoluteOffset {
                x: None,
                y: Some(y),
            },
        )
    }

    /// Re-issue the demote center when a fresh mount report disagrees with it.
    /// Converges in one step: the correction's own echo reports the target
    /// back, inside tolerance, and no further op issues.
    pub(crate) fn correct_demoted_games_reveal(&mut self, reported_y: f32) -> Task<Message> {
        let Some((id, target)) = self.start_state.selection_scroll_y_center_prefer() else {
            return Task::none();
        };
        if (target - reported_y).abs() <= 2.0 {
            return Task::none();
        }
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: start demote reveal correct reported={reported_y:.0} target={target:.0}"
        ));
        self.start_state.note_scroll_y(&id, target);
        operation::scroll_to(
            id,
            operation::AbsoluteOffset {
                x: None,
                y: Some(target),
            },
        )
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
        self.scroll_start_selection_to_center(true)
            .chain(self.warm_start_art_task())
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
        self.scroll_start_selection_to_center(false)
            .chain(self.warm_start_art_task())
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
        self.scroll_start_selection_to_center(true)
            .chain(self.warm_start_art_task())
    }

    pub(crate) fn cancel_manual_add(&mut self) -> Task<Message> {
        self.start_state.manual_add = None;
        Task::none()
    }

    pub(crate) fn toggle_start_edit(&mut self) -> Option<Task<Message>> {
        if self.start_state.slide != StartSlide::Games
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
        match self.start_state.slide {
            StartSlide::Games if self.start_state.editing => match self.edit_toggle_selected() {
                Some(task) => {
                    self.play_start_cue(UiSoundKind::Action);
                    task
                }
                None => Task::none(),
            },
            StartSlide::Games => {
                if self.launch_selected() {
                    self.play_start_cue(UiSoundKind::Action);
                    if self.start_state.immersive {
                        return self.close_start_screen();
                    }
                }
                Task::none()
            }
            StartSlide::Controllers => {
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
                Task::none()
            }
        }
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
