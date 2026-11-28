//! One physical shell window shared by Home, Launcher, and Browser.
//!
//! The three screens used to own separate top-level windows. That forced a
//! backdrop-owner handoff and window open/close races on every swap. They now
//! share one hidden-until-configured HWND; `home_window` / `browser_window` /
//! `launcher_window` remain thin aliases for the active mode so the rest of
//! the app can keep routing by screen identity.
//!
//! PS dismiss hides this window (keeping the surface/compositor warm) and the
//! next PS shows it again. Mode switches happen in place.

use super::{App, Message};
use crate::app::home::ShellScreen;
use crate::ui::layout::{self, MonitorCover};
use iced::Task;
use iced::window;

impl App {
    /// True when no shell mode switch or hide/resume animation is in flight.
    pub(crate) fn shell_idle(&self) -> bool {
        !self.shell_pending_show
            && !self.shell_reveal_armed
            && self.pending_shell_mode.is_none()
            && !self.home_closing()
            && !self.browser_closing()
            && !self.launcher_closing()
            && self.home_shell_anim.is_none()
            && self.browser_shell_anim.is_none()
            && self.launcher_shell_anim.is_none()
            && !self.shell_backdrop.animating(std::time::Instant::now())
    }

    pub(crate) fn on_shell_opened(&mut self, id: window::Id) -> Task<Message> {
        if self.shell_window != Some(id) {
            return Task::none();
        }
        crate::controller::hid::diag::diag_info(format!("ui-diag: shell opened id={id:?}"));
        Task::none()
    }

    /// Show the warm/hidden shell and start the active mode's entrance.
    pub(crate) fn on_shell_show_now(&mut self, id: window::Id) -> Task<Message> {
        if self.shell_window != Some(id) || !self.shell_pending_show {
            return Task::none();
        }
        self.shell_pending_show = false;
        self.shell_hidden = false;
        self.shell_opening = false;
        self.shell_presented = false;
        self.shell_reveal_armed = false;
        self.shell_reveal_generation = self.shell_reveal_generation.wrapping_add(1);
        let generation = self.shell_reveal_generation;
        // Invalidate any hide task still queued from the previous dismiss:
        // the raw offscreen show below bypasses `shell_visibility_task`, so
        // this show needs its own epoch to win the ordering race.
        self.shell_visibility_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mode = self.shell_mode;
        if mode == ShellScreen::Browser {
            self.sync_browser_viewport();
        }
        // Prime the warm surface with the hidden/closed pose offscreen, then
        // arm. The entrance clock starts at arm, never here: the prime is not
        // visible and must not consume the entrance. Arming invalidates again
        // so the gate can only accept a frame drawn after it.
        // The timer is only a lost-event fallback. Quiet QA runs the same
        // gate (its present step stays offscreen), so automation exercises
        // the real PS reveal path.
        layout::show_shell_offscreen(id)
            .map(move |_| Message::ShellRevealArmed(id, generation))
            .chain(Task::perform(
                super::delay(std::time::Duration::from_millis(250)),
                move |_| Message::ShellPresent(id, generation),
            ))
            .chain(Task::perform(
                super::delay(std::time::Duration::from_millis(300)),
                move |_| Message::ShellFocusFallback(id, generation),
            ))
    }

    /// The safety-net pass: if the first present was lost, run the full
    /// present; otherwise re-assert the cover rect and foreground. This keeps
    /// a missed 32ms timer from leaving the warm window offscreen.
    pub(crate) fn on_shell_focus_fallback(&mut self, id: window::Id) -> Task<Message> {
        if self.shell_window != Some(id)
            || self.shell_hidden
            || crate::platform::debug_quiet::enabled()
        {
            return Task::none();
        }
        if !self.shell_presented {
            return self.on_shell_present(id, "focus-fallback");
        }
        let cover =
            crate::ui::layout::primary_monitor_cover().unwrap_or(crate::ui::layout::MonitorCover {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            });
        layout::set_shell_level(id, true)
            .chain(layout::show_shell_at_cover(id, cover))
            .chain(layout::force_shell_foreground(id))
    }

    /// The offscreen prime ran; arm the drawn-frame gate and start the one
    /// entrance clock for this show.
    pub(crate) fn on_shell_reveal_armed(&mut self, id: window::Id) -> Task<Message> {
        if self.shell_window != Some(id) || self.shell_hidden || self.shell_presented {
            return Task::none();
        }
        // The show may have been superseded by a close between the offscreen
        // prime and this arm. Never restart an entrance for a screen that is
        // no longer visible; the close/switch path owns the next state.
        let mode_visible = match self.shell_mode {
            ShellScreen::Home => self.home_visible,
            ShellScreen::Browser => self.browser_visible,
            ShellScreen::Launcher => self.launcher_visible,
        };
        if !mode_visible {
            return Task::none();
        }
        self.shell_reveal_armed = true;
        self.shell_reveal_started = Some(std::time::Instant::now());
        // Only a view built after this point carries the hidden/entrance
        // pose. The invalidation below builds one; the gate rejects every
        // earlier draw. The entrance itself starts at first present, not here,
        // so a slow first paint can never advance it to a settled pose.
        self.shell_reveal_arm_draw = self.shell_draw_epoch.get();
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: shell reveal armed generation={} draw_epoch={}",
            self.shell_reveal_generation, self.shell_reveal_arm_draw
        ));
        crate::ui::layout::invalidate_window(id)
    }

    /// First redraw of the primed shell after arm: a post-arm frame has been
    /// built and painted, so it is safe to reveal.
    pub(crate) fn on_shell_redraw(&mut self, id: window::Id, generation: u64) -> Task<Message> {
        if self.shell_window == Some(id) && self.reveal_frame_ready(generation) {
            return self.on_shell_present(id, "redraw");
        }
        Task::none()
    }

    /// The gate: armed for this show, matching generation, and the shell view
    /// has been rebuilt after arm (a pre-arm/queued redraw can never pass).
    fn reveal_frame_ready(&self, generation: u64) -> bool {
        reveal_frame_ready(
            self.shell_reveal_armed,
            self.shell_presented,
            self.shell_hidden,
            generation,
            self.shell_reveal_generation,
            self.shell_draw_epoch.get(),
            self.shell_reveal_arm_draw,
        )
    }

    /// Present the shell on the monitor after the post-arm hidden frame, then
    /// start the one entrance clock from zero.
    pub(crate) fn on_shell_present(
        &mut self,
        id: window::Id,
        source: &'static str,
    ) -> Task<Message> {
        if self.shell_window != Some(id)
            || self.shell_hidden
            || self.shell_presented
            || !self.reveal_frame_ready(self.shell_reveal_generation)
            || self.shell_drawn_generation.get() != self.shell_reveal_generation
        {
            return Task::none();
        }
        let mode = self.shell_mode;
        self.shell_reveal_armed = false;
        self.shell_presented = true;
        let now = std::time::Instant::now();
        // Report the pose of the frame the view actually built (already drawn
        // by the time this message runs), never a recomputed wall-clock pose.
        let drawn = self.shell_drawn_frame.get();
        let panel = drawn.panel;
        let backdrop = drawn.backdrop_alpha;
        let draw_epoch = self.shell_draw_epoch.get();
        let arm_draw = self.shell_reveal_arm_draw;
        let elapsed_ms = self
            .shell_reveal_started
            .map(|started| now.saturating_duration_since(started).as_millis())
            .unwrap_or(0);
        self.shell_reveal_started = None;
        // Launcher never applies scale/offset (fade only); report the pose
        // actually rendered so a resize shows up as scale != 1.
        let applied_scale = if mode == ShellScreen::Launcher {
            1.0
        } else {
            panel.scale
        };
        let applied_y = if mode == ShellScreen::Launcher {
            0.0
        } else {
            panel.offset_y
        };
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: shell first present mode={mode:?} source={source} generation={} draw_generation={} draw_epoch={draw_epoch} arm_draw={arm_draw} elapsed_ms={elapsed_ms} scale={applied_scale:.3} offset_y={applied_y:.1} alpha={:.3} backdrop={backdrop:.3} veil={:.3}",
            self.shell_reveal_generation,
            self.shell_drawn_generation.get(),
            panel.alpha,
            1.0 - panel.alpha
        ));
        // The visible window starts at the hidden/zero pose. Start the one
        // entrance clock now so the fade always runs 0 → target on-screen,
        // even when the offscreen prime took longer than the animation.
        let enter = self.start_shell_enter(mode);
        let level = self.shell_level_task(mode);
        let toast = self.shell_toast_task(mode);
        let show = if crate::platform::debug_quiet::enabled() {
            self.shell_visibility_task(id, true)
                .chain(layout::focus_window(id))
        } else {
            let cover = crate::ui::layout::primary_monitor_cover().unwrap_or(
                crate::ui::layout::MonitorCover {
                    x: 0.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                },
            );
            layout::show_shell_at_cover(id, cover)
                .chain(layout::force_shell_foreground(id))
                .chain(self.sync_toast_zorder())
        };
        let launcher = if mode == ShellScreen::Launcher {
            Task::done(Message::LauncherOpened(id))
        } else {
            Task::none()
        };
        level.chain(enter).chain(show).chain(toast).chain(launcher)
    }

    /// Resolve the shell pose for a view build. A hidden/closed shell holds
    /// its hidden pose even when no animation is live; `None` must never mean
    /// the settled `full` pose while the warm window is hidden.
    pub(crate) fn resolved_shell_frame(
        &self,
        anim: Option<crate::ui::home::ShellAnim>,
        now: std::time::Instant,
    ) -> crate::ui::home::ShellFrame {
        let reduced =
            crate::ui::motion::reduced_motion_effective(self.session.prefs.reduced_motion);
        resolve_shell_frame(self.shell_render_hidden, anim, now, reduced)
    }

    /// Record the pose of the shell view currently being built. `view()` gets
    /// `&self`; the epoch is diagnostic/reveal state only.
    pub(crate) fn note_shell_draw(&self, frame: crate::ui::home::ShellFrame) {
        let epoch = self.shell_draw_epoch.get().wrapping_add(1);
        self.shell_draw_epoch.set(epoch);
        self.shell_drawn_generation
            .set(self.shell_reveal_generation);
        self.shell_drawn_frame.set(frame);
    }

    /// Cheap hidden/closed shell surface: no card, no page, and no full pose
    /// left in the warm surface while the HWND is hidden or mid-hide.
    pub(crate) fn hidden_shell_view(&self) -> iced::Element<'_, Message> {
        use iced::Length::Fill;
        use iced::widget::{container, space};
        let veil = if self.shell_mode == ShellScreen::Launcher {
            1.0
        } else {
            0.0
        };
        container(space())
            .width(Fill)
            .height(Fill)
            .style(crate::ui::theme::scrim(veil))
            .into()
    }

    /// Point the per-screen aliases at the one shell HWND for `mode`.
    pub(crate) fn set_shell_aliases(&mut self, mode: ShellScreen) {
        let id = self.shell_window;
        self.shell_mode = mode;
        self.home_window = (mode == ShellScreen::Home).then_some(id).flatten();
        self.browser_window = if mode == ShellScreen::Browser {
            id
        } else {
            None
        };
        self.launcher_window = if mode == ShellScreen::Launcher {
            id
        } else {
            None
        };
        self.home_visible = mode == ShellScreen::Home;
        self.browser_visible = mode == ShellScreen::Browser;
        self.launcher_visible = mode == ShellScreen::Launcher;
    }

    /// Clear the active-mode aliases without destroying the warm shell HWND.
    pub(crate) fn clear_shell_aliases(&mut self) {
        self.home_window = None;
        self.browser_window = None;
        self.launcher_window = None;
        self.home_visible = false;
        self.browser_visible = false;
        self.launcher_visible = false;
    }

    /// Ensure the one shell HWND exists, showing it again when PS left it warm.
    pub(crate) fn ensure_shell_window(
        &mut self,
        mode: ShellScreen,
        cover: MonitorCover,
    ) -> Task<Message> {
        self.set_shell_aliases(mode);
        if let Some(id) = self.shell_window {
            if self.shell_hidden {
                self.shell_hidden = false;
                self.shell_pending_show = true;
                return Task::done(Message::WindowShowNow(id));
            }
            return Task::none();
        }
        let (id, open) = window::open(window::Settings {
            size: iced::Size::new(cover.width, cover.height),
            position: window::Position::Specific(iced::Point::new(cover.x, cover.y)),
            // Hidden until the cover size + DWM transition setup are final.
            visible: false,
            transparent: crate::ui::chrome::transparent_windows(),
            resizable: false,
            decorations: false,
            // Home is raised to topmost when it becomes the active mode; the
            // shared HWND starts normal so Launcher/Browser keep their band.
            level: window::Level::Normal,
            exit_on_close_request: true,
            platform_specific: layout::overlay_platform_specific(),
            ..window::Settings::default()
        });
        self.shell_window = Some(id);
        self.shell_hidden = false;
        self.shell_pending_show = true;
        self.set_shell_aliases(mode);
        open.map(Message::ShellOpened)
            .chain(layout::disable_window_transitions(id))
            .chain(Task::done(Message::WindowShowNow(id)))
    }

    /// Activate `mode` in the shared window. If the outgoing mode is still
    /// animating out, defer the switch until its finish callback runs.
    pub(crate) fn activate_shell_mode(
        &mut self,
        mode: ShellScreen,
        cover: MonitorCover,
    ) -> Task<Message> {
        if self.shell_window.is_some() && !self.shell_hidden {
            let outgoing = self.home_closing() || self.browser_closing() || self.launcher_closing();
            if outgoing {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: shell switch {mode:?} deferred home_closing={} browser_closing={} launcher_closing={} home_anim={:?} browser_anim={:?} launcher_anim={:?}",
                    self.home_closing(),
                    self.browser_closing(),
                    self.launcher_closing(),
                    self.home_shell_anim.map(|a| a.dir),
                    self.browser_shell_anim.map(|a| a.dir),
                    self.launcher_shell_anim.map(|a| a.dir),
                ));
                self.pending_shell_mode = Some(mode);
                return Task::none();
            }
        }
        self.shell_opening = true;
        self.reset_shell_cursor_clock();
        self.set_shell_aliases(mode);
        let was_hidden = self.shell_hidden || self.shell_window.is_none();
        // While hidden, winit still believes a raw-hidden window is visible;
        // a level change would re-show it before the entrance is ready.
        let level = if was_hidden {
            Task::none()
        } else {
            self.shell_level_task(mode)
        };
        let enter = if was_hidden {
            Task::none()
        } else {
            self.start_shell_enter(mode)
        };
        let ensure = self.ensure_shell_window(mode, cover);
        let show = match self.shell_window {
            Some(id) if !self.shell_hidden => self.shell_visibility_task(id, true),
            _ => Task::none(),
        };
        let toast = self.shell_toast_task(mode);
        // A warm, still-visible resume (rapid PS) does not go through
        // `on_shell_show_now`, so it must arm Launcher pad nav explicitly.
        // Without this the Launcher stays `launcher_nav_ready = false` and
        // ignores the controller until a hide/show cycle.
        let opened = if mode == ShellScreen::Launcher && !was_hidden {
            match self.shell_window {
                Some(id) => Task::done(Message::LauncherOpened(id)),
                None => Task::none(),
            }
        } else {
            Task::none()
        };
        level
            .chain(enter)
            .chain(show)
            .chain(toast)
            .chain(opened)
            .chain(ensure)
    }

    /// Switch to a mode in an already-visible shell window.
    pub(crate) fn switch_shell_mode(&mut self, mode: ShellScreen) -> Task<Message> {
        self.pending_shell_mode = None;
        self.shell_opening = false;
        // This is a new in-place entrance: a delayed arm/present from the
        // outgoing show must not satisfy it.
        self.shell_reveal_generation = self.shell_reveal_generation.wrapping_add(1);
        self.shell_reveal_armed = false;
        self.shell_reveal_started = None;
        self.set_shell_aliases(mode);
        let level = self.shell_level_task(mode);
        let enter = self.start_shell_enter(mode);
        let opened = if mode == ShellScreen::Launcher {
            match self.shell_window {
                Some(id) => Task::done(Message::LauncherOpened(id)),
                None => Task::none(),
            }
        } else {
            Task::none()
        };
        let show = match self.shell_window {
            Some(id) => self.shell_visibility_task(id, true),
            None => Task::none(),
        };
        let toast = self.shell_toast_task(mode);
        level.chain(enter).chain(show).chain(toast).chain(opened)
    }

    /// Consume a deferred switch requested while the outgoing card faded.
    pub(crate) fn take_pending_shell_mode(&mut self) -> Option<ShellScreen> {
        self.pending_shell_mode.take()
    }

    fn shell_visibility_task(&self, id: window::Id, visible: bool) -> Task<Message> {
        use std::sync::atomic::Ordering;
        let token = self.shell_visibility_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        layout::set_shell_visible(
            id,
            visible,
            std::sync::Arc::clone(&self.shell_visibility_epoch),
            token,
        )
    }

    /// Keep the toast on the right host after a mode switch: Browser uses the
    /// HWND overlay, Home/Launcher composite it in their own view.
    fn shell_toast_task(&self, mode: ShellScreen) -> Task<Message> {
        if self.toast_message.is_none() {
            return Task::none();
        }
        match mode {
            ShellScreen::Browser => self.rehost_toast_after_composite(),
            ShellScreen::Home | ShellScreen::Launcher => self
                .toast_window
                .map(layout::hide_toast)
                .unwrap_or_else(Task::none),
        }
    }

    fn shell_level_task(&self, _mode: ShellScreen) -> Task<Message> {
        let Some(id) = self.shell_window else {
            return Task::none();
        };
        // The whole shell is a console cover: keep it in the topmost band so
        // a running borderless game can never cover the Launcher/Browser.
        layout::set_shell_level(id, true)
    }

    pub(crate) fn start_shell_enter(&mut self, mode: ShellScreen) -> Task<Message> {
        let now = std::time::Instant::now();
        self.shell_render_hidden = false;
        match mode {
            ShellScreen::Home => {
                self.home_shell_anim = Some(crate::ui::home::ShellAnim::enter(now));
                self.browser_shell_anim = None;
                self.launcher_shell_anim = None;
            }
            ShellScreen::Browser => {
                self.browser_shell_anim = Some(crate::ui::home::ShellAnim::enter(now));
                self.home_shell_anim = None;
                self.launcher_shell_anim = None;
            }
            ShellScreen::Launcher => {
                self.home_shell_anim = None;
                self.browser_shell_anim = None;
                self.launcher_shell_anim = Some(crate::ui::home::ShellAnim::enter(now));
            }
        }
        self.claim_shell_backdrop(now);
        self.shell_hidden = false;
        match self.shell_window {
            Some(id) => layout::invalidate_window(id),
            None => Task::none(),
        }
    }

    /// Hide the warm shell window after the exit animation settles.
    pub(crate) fn hide_shell_window(&mut self) -> Task<Message> {
        // Hold the hidden pose in the warm surface before clearing animation
        // state; a redraw between here and the raw hide must never fall back
        // to a settled full card.
        self.shell_render_hidden = true;
        self.home_shell_anim = None;
        self.browser_shell_anim = None;
        self.launcher_shell_anim = None;
        self.clear_shell_aliases();
        // Close the reveal epoch: any queued arm/present from a show that was
        // dismissed before its gate ran must not start an entrance later.
        self.shell_reveal_generation = self.shell_reveal_generation.wrapping_add(1);
        self.shell_reveal_armed = false;
        self.shell_reveal_started = None;
        // A second PS during the exit asks to resume the same screen. Cancel
        // the hide and let the normal resume path re-enter it in place. Keep
        // the current backdrop alpha: the window never left the screen, so
        // resetting it here would be a visible 0/1 jump.
        if self.ps_resume_pending {
            self.shell_pending_show = false;
            self.shell_opening = false;
            return self.maybe_resume_ps_after_close();
        }
        self.shell_hidden = true;
        self.shell_presented = false;
        self.shell_pending_show = false;
        self.pending_shell_mode = None;
        self.shell_backdrop = crate::ui::motion::ShellBackdrop::Hidden;
        self.shell_opening = false;
        self.reset_shell_cursor_clock();
        self.set_shell_cursor_hidden(false);
        let Some(id) = self.shell_window else {
            return Task::none();
        };
        self.shell_visibility_task(id, false)
            .chain(self.rehost_toast_after_composite())
    }
}

/// Resolve the shell pose for one view build. A hidden/closed shell keeps the
/// hidden pose even when no animation is live; clearing an animation can never
/// fall back to `full`.
pub(crate) fn resolve_shell_frame(
    hidden: bool,
    anim: Option<crate::ui::home::ShellAnim>,
    now: std::time::Instant,
    reduced: bool,
) -> crate::ui::home::ShellFrame {
    if hidden {
        return crate::ui::home::ShellFrame::hidden();
    }
    anim.map(|anim| anim.frame(now, reduced))
        .unwrap_or_else(crate::ui::home::ShellFrame::full)
}

/// The reveal gate: only a frame drawn after this show's arm, for the current
/// generation, may move the warm shell onscreen.
pub(crate) fn reveal_frame_ready(
    armed: bool,
    presented: bool,
    hidden: bool,
    message_generation: u64,
    current_generation: u64,
    draw_epoch: u64,
    arm_draw: u64,
) -> bool {
    armed
        && !presented
        && !hidden
        && message_generation == current_generation
        && draw_epoch > arm_draw
}

#[cfg(test)]
mod tests {
    use super::{resolve_shell_frame, reveal_frame_ready};
    use crate::ui::home::{PanelVisual, ShellAnim, ShellFrame};
    use std::time::Instant;

    #[test]
    fn pre_arm_or_stale_generation_cannot_present() {
        // Armed at epoch 10 and presented / stale message both fail.
        assert!(!reveal_frame_ready(true, true, false, 3, 3, 11, 10));
        assert!(!reveal_frame_ready(true, false, false, 2, 3, 11, 10));
        assert!(!reveal_frame_ready(false, false, false, 3, 3, 11, 10));
        // A redraw drawn before/at arm is not the entrance frame.
        assert!(!reveal_frame_ready(true, false, false, 3, 3, 10, 10));
        // A frame built after arm for this generation may present.
        assert!(reveal_frame_ready(true, false, false, 3, 3, 11, 10));
    }

    #[test]
    fn hidden_shell_never_falls_back_to_full() {
        let now = Instant::now();
        // Exit completion clears the animation; hidden must still win.
        assert_eq!(
            resolve_shell_frame(true, None, now, false),
            ShellFrame::hidden()
        );
        // Even a live animation cannot make a hidden shell draw a full pose.
        assert_eq!(
            resolve_shell_frame(true, Some(ShellAnim::enter(now)), now, false),
            ShellFrame::hidden()
        );
        assert_eq!(
            resolve_shell_frame(false, None, now, false),
            ShellFrame::full()
        );
        assert_eq!(ShellFrame::hidden().panel, PanelVisual::hidden());
        assert!(ShellFrame::hidden().panel.alpha < 0.01);
    }
}
