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
        self.shell_reveal_started = Some(std::time::Instant::now());
        let generation = self.shell_reveal_generation;
        // Invalidate any hide task still queued from the previous dismiss:
        // the raw offscreen show below bypasses `shell_visibility_task`, so
        // this show needs its own epoch to win the ordering race.
        self.shell_visibility_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mode = self.shell_mode;
        let enter = self.start_shell_enter(mode);
        if mode == ShellScreen::Browser {
            self.sync_browser_viewport();
        }
        // Prime an offscreen frame with the entrance pose, then move the warm
        // surface onscreen. The arming message is produced only after the
        // window action returns, so it cannot arm before the prime exists.
        // A redraw already queued from the old surface is ignored; arming
        // invalidates again so the gate sees a frame drawn after the prime.
        // The timer is only a lost-event fallback. Quiet QA runs the same
        // gate (its present step stays offscreen), so automation exercises
        // the real PS reveal path.
        enter
            .chain(
                layout::show_shell_offscreen(id)
                    .map(move |_| Message::ShellRevealArmed(id, generation)),
            )
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

    /// The offscreen prime ran; wait for that window's first redraw before
    /// moving it onscreen.
    pub(crate) fn on_shell_reveal_armed(&mut self, id: window::Id) -> Task<Message> {
        if self.shell_window != Some(id) || self.shell_hidden || self.shell_presented {
            return Task::none();
        }
        self.shell_reveal_armed = true;
        // Restart the entrance at the arm point. Preparing the offscreen
        // prime (and any CEF/session work) can take 100ms+, so an entrance
        // clock started in `on_shell_show_now` would already be near the
        // resting pose. The gate then waits for this fresh t=0 redraw.
        self.shell_reveal_started = Some(std::time::Instant::now());
        crate::controller::hid::diag::diag_info("ui-diag: shell reveal armed");
        self.start_shell_enter(self.shell_mode)
    }

    /// First redraw of the primed shell: the entrance pose has painted (the
    /// event is broadcast after draw/present), so it is safe to reveal.
    pub(crate) fn on_shell_redraw(&mut self, id: window::Id) -> Task<Message> {
        if self.shell_reveal_armed && self.shell_window == Some(id) {
            return self.on_shell_present(id, "redraw");
        }
        Task::none()
    }

    /// Present the shell on the monitor after the offscreen entrance frame.
    pub(crate) fn on_shell_present(
        &mut self,
        id: window::Id,
        source: &'static str,
    ) -> Task<Message> {
        if self.shell_window != Some(id) || self.shell_hidden || self.shell_presented {
            return Task::none();
        }
        let mode = self.shell_mode;
        self.shell_reveal_armed = false;
        self.shell_presented = true;
        let now = std::time::Instant::now();
        let reduced =
            crate::ui::motion::reduced_motion_effective(self.session.prefs.reduced_motion);
        let anim = match mode {
            ShellScreen::Home => self.home_shell_anim,
            ShellScreen::Browser => self.browser_shell_anim,
            ShellScreen::Launcher => self.launcher_shell_anim,
        };
        let panel = anim
            .map(|anim| anim.frame(now, reduced).panel)
            .unwrap_or_else(crate::ui::home::PanelVisual::full);
        let elapsed_ms = anim
            .map(|anim| now.saturating_duration_since(anim.started()).as_millis())
            .or_else(|| {
                self.shell_reveal_started
                    .map(|started| now.saturating_duration_since(started).as_millis())
            })
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
            "ui-diag: shell first present mode={mode:?} source={source} elapsed_ms={elapsed_ms} scale={applied_scale:.3} offset_y={applied_y:.1} alpha={:.3} veil={:.3}",
            panel.alpha,
            1.0 - panel.alpha
        ));
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
        level.chain(show).chain(toast).chain(launcher)
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
        self.home_shell_anim = None;
        self.browser_shell_anim = None;
        self.clear_shell_aliases();
        // A second PS during the exit asks to resume the same screen. Cancel
        // the hide and let the normal resume path re-enter it in place.
        if self.ps_resume_pending {
            self.shell_pending_show = false;
            self.shell_opening = false;
            return self.maybe_resume_ps_after_close();
        }
        self.shell_hidden = true;
        self.shell_presented = false;
        self.shell_reveal_armed = false;
        self.shell_reveal_started = None;
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
