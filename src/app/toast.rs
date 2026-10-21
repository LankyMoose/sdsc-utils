//! Toast overlay presentation: window lifecycle, slide machine driver,
//! effects, z-order, and update-arm handlers.
//!
//! Behavior-preserving extraction from `app::App`; no logic change.

use super::{App, Message, TOAST_LIFETIME, delay};
use crate::controller::model::Connection;
use crate::persist::analytics;
use crate::platform::app_log;
use crate::platform::crash_restart;
use crate::session::notify::NotifyEvent;
use crate::ui::layout::{
    focus_window, hide_toast, invalidate_toast, move_window, overlay_platform_specific,
    raise_window_topmost, remount_toast_surface, set_toast_topmost, show_toast_without_activate,
    slide_y, toast_local_in_cover, toast_placement,
};
use crate::ui::toast::ToastMessage;
use crate::ui::toast::machine::{self as toast_machine, Effect as ToastEffect};
use crate::ui::toast::view as toast_view;
use iced::widget::Float;
use iced::{Element, Point, Size, Task, Vector, window};
use std::time::{Duration, Instant};

impl App {
    pub(crate) fn immersive_hosts_toast(&self) -> bool {
        self.start_visible && self.start_state.immersive
    }

    pub(crate) fn toast_eta_for(&self, event: &NotifyEvent) -> Option<String> {
        if !self.session.prefs.analytics_enabled {
            return None;
        }
        let status = crate::controller::dualsense::battery::dualsense_status(
            0,
            "DualSense",
            Connection::Usb,
            event.serial.clone(),
            event.percent.unwrap_or(0),
            event.state,
        );
        self.session
            .analytics
            .eta_for(&status)
            .map(analytics::format_eta_ring)
    }

    pub(crate) fn show_position_preview(&mut self) -> Task<Message> {
        self.toast_queue.clear();
        self.toast_queue
            .push_back(ToastMessage::preview(&self.session.prefs.spectrum));
        // Force-finish any in-flight toast without waiting for the machine.
        self.toast_machine = toast_machine::State::Idle;
        let finish = self.effect_toast_finished();
        finish.chain(self.show_next_toast())
    }

    /// Pre-create the toast window hidden so iced keeps a warm GPU compositor.
    /// Also used as the first-toast path; does not show or queue a message.
    pub(crate) fn ensure_toast_window(&mut self) -> Task<Message> {
        if self.toast_window.is_some() {
            return Task::none();
        }
        let (_id, open) = self.create_toast_window();
        open.discard()
    }

    /// One-shot toast after crash auto-relaunch (or `--test-crash-toast`).
    pub(crate) fn maybe_show_crash_restart_toast(&mut self) -> Task<Message> {
        if !crash_restart::take_pending_notice() {
            return Task::none();
        }
        app_log::info("crash-restart: notice toast");
        ToastMessage::enqueue(&mut self.toast_queue, ToastMessage::crash_restart());
        self.show_next_toast()
    }

    pub(crate) fn create_toast_window(&mut self) -> (window::Id, Task<window::Id>) {
        let (id, open) = window::open(window::Settings {
            size: Size::new(toast_view::WIDTH, toast_view::HEIGHT),
            position: window::Position::Default,
            visible: false,
            resizable: false,
            decorations: false,
            transparent: crate::ui::chrome::transparent_windows(),
            level: window::Level::AlwaysOnTop,
            exit_on_close_request: false,
            platform_specific: overlay_platform_specific(),
            ..window::Settings::default()
        });
        self.toast_window = Some(id);
        (id, open)
    }

    pub(crate) fn show_next_toast(&mut self) -> Task<Message> {
        if self.toast_message.is_some() {
            return Task::none();
        }
        let Some(message) = self.toast_queue.pop_front() else {
            return Task::none();
        };

        self.toast_generation = self.toast_generation.wrapping_add(1);
        let generation = self.toast_generation;
        let after = message.after;
        let body = message.body.clone();
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: toast show heading={:?} body={body} percent={} after={after:?} queue_left={}",
            message.heading,
            message.percent(),
            self.toast_queue.len()
        ));
        self.toast_message = Some(message);

        let expire = Task::perform(delay(TOAST_LIFETIME), move |()| {
            Message::ToastDismiss(generation)
        });
        self.toast_step(toast_machine::Event::Show {
            generation,
            after,
            body,
        })
        .chain(expire)
    }

    /// Drive the presentation machine; log phase transitions in debug builds.
    pub(crate) fn toast_step(&mut self, event: toast_machine::Event) -> Task<Message> {
        let prev = self
            .toast_machine
            .phase()
            .map(toast_machine::phase_name)
            .unwrap_or("Idle");
        let prev_gen = self.toast_machine.generation();
        let (next, effects) = toast_machine::step(std::mem::take(&mut self.toast_machine), event);
        let next_name = next
            .phase()
            .map(toast_machine::phase_name)
            .unwrap_or("Idle");
        if prev != next_name || (!effects.is_empty() && prev == "Idle") {
            let body = match &next {
                toast_machine::State::Active(active) => active.body.as_str(),
                toast_machine::State::Idle => "-",
            };
            let generation = next.generation().or(prev_gen).unwrap_or(0);
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: toast {prev}->{next_name} gen={generation} body={body}"
            ));
        }
        // Shown starts the slide clock after the HWND is visible at outside_y.
        if next_name == "SlidingIn" && prev == "Placing" {
            self.toast_anim_started = Instant::now();
            if self.start_visible && self.start_state.immersive {
                let generation = next.generation().or(prev_gen).unwrap_or(0);
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: immersive toast composite gen={generation}"
                ));
            }
        }
        if next_name == "SlidingOut" && prev != "SlidingOut" {
            self.toast_anim_started = Instant::now();
        }
        self.toast_machine = next;
        self.apply_toast_effects(effects)
    }

    /// Draw the active toast inside immersive Start (cover occludes the toast HWND).
    pub(crate) fn immersive_toast_overlay(&self) -> Option<Element<'_, Message>> {
        if !self.start_visible || !self.start_state.immersive {
            return None;
        }
        let message = self.toast_message.as_ref()?;
        let (progress, dismissing) = self.toast_machine.slide_pose()?;
        let placement = self.toast_placement?;
        let cover = self.start_monitor_cover?;
        let local = toast_local_in_cover(placement, progress, dismissing, cover);
        let card = toast_view::view(
            message,
            self.toast_generation,
            Message::ToastDismiss(self.toast_generation),
        );
        // Layout at (0,0), then Float-translate into cover-local pose. Non-zero
        // translate (or a tiny nudge) puts the card in the overlay band above the
        // stage/veil and dock Floats; only the card's mouse_area captures clicks.
        let x = local.x;
        let y = local.y;
        Some(
            Float::new(card)
                .scale(1.0)
                .translate(move |_bounds, _viewport| {
                    if x == 0.0 && y == 0.0 {
                        Vector::new(0.0, 0.001)
                    } else {
                        Vector::new(x, y)
                    }
                })
                .into(),
        )
    }

    pub(crate) fn apply_toast_effects(&mut self, effects: Vec<ToastEffect>) -> Task<Message> {
        let mut task = Task::none();
        for effect in effects {
            task = task.chain(self.apply_toast_effect(effect));
        }
        task
    }

    pub(crate) fn apply_toast_effect(&mut self, effect: ToastEffect) -> Task<Message> {
        match effect {
            ToastEffect::PlaceShow => self.effect_place_show(),
            ToastEffect::Move {
                progress,
                dismissing,
            } => self.effect_toast_move(progress, dismissing),
            ToastEffect::OpenStart => self.effect_open_start(),
            ToastEffect::Finished => self.effect_toast_finished(),
            ToastEffect::RaiseInteractive { focus } => self.raise_interactive_ui_above_toast(focus),
        }
    }

    pub(crate) fn effect_place_show(&mut self) -> Task<Message> {
        let generation = self.toast_generation;
        if let Some(id) = self.toast_window {
            return Task::done(Message::PlaceToast { id, generation });
        }
        let (_id, open) = self.create_toast_window();
        open.map(move |id| Message::PlaceToast { id, generation })
    }

    pub(crate) fn effect_toast_move(&self, progress: f32, dismissing: bool) -> Task<Message> {
        // Immersive composite owns the slide pose; leave the toast HWND alone.
        if self.immersive_hosts_toast() {
            return Task::none();
        }
        let Some(id) = self.toast_window else {
            return Task::none();
        };
        let Some(placement) = self.toast_placement else {
            return Task::none();
        };
        let y = slide_y(placement, progress, dismissing);
        move_window(id, Point::new(placement.x, y))
    }

    pub(crate) fn effect_open_start(&mut self) -> Task<Message> {
        if crate::session::should_flush_latched_start(
            true,
            self.session.prefs.start_screen_enabled
                && self.session.prefs.start_screen_auto_open.auto_opens(),
            self.start_visible,
        ) {
            crate::controller::hid::diag::diag_info("ui-diag: start open after toast settle");
            self.open_start_screen()
        } else {
            crate::controller::hid::diag::diag_info(
                "ui-diag: latched start skipped (disabled or already open)",
            );
            Task::none()
        }
    }

    pub(crate) fn effect_toast_finished(&mut self) -> Task<Message> {
        let _ = self.toast_message.take();
        // Invalidate any in-flight expiry for this toast.
        self.toast_generation = self.toast_generation.wrapping_add(1);
        self.toast_placement = None;

        // Stay visible across handoff so the next message can remount/present on
        // the same HWND (closing/recreating dropped the follow-up toast).
        if !self.toast_queue.is_empty() {
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: toast handoff remount queue={}",
                self.toast_queue.len()
            ));
            return self.show_next_toast();
        }

        // Keep the window alive but hidden: it doubles as the GPU compositor
        // sentinel so popup/Settings can open without a cold wgpu init.
        // When Start is visible, defer hide — Crash A lined up create_renderer
        // panics with toast Idle/hide while Start was already open.
        if self.start_visible {
            crate::controller::hid::diag::diag_info("ui-diag: toast hide deferred (start visible)");
            return Task::perform(delay(Duration::from_millis(50)), |()| {
                Message::ToastHideDeferred
            });
        }
        match self.toast_window {
            Some(id) => {
                crate::controller::hid::diag::diag_info("ui-diag: toast hide");
                crate::platform::wgpu_diag::note_toast_hide();
                hide_toast(id)
            }
            None => Task::none(),
        }
    }

    /// Prefer Settings, then Start, then popup as the focused presenting window.
    pub(crate) fn refocus_interactive_ui(&self) -> Task<Message> {
        if let Some(id) = self.configure_window {
            return focus_window(id);
        }
        if self.start_visible
            && let Some(id) = self.start_window
        {
            return focus_window(id);
        }
        if let Some(id) = self.popup_window {
            return focus_window(id);
        }
        Task::none()
    }

    /// Raise interactive UI into the topmost band above the toast (last wins).
    /// Toast stays TOPMOST vs Cursor; UI above toast keeps presents (iced#3320).
    /// Only called when the machine emits RaiseInteractive (Resting / settle).
    /// Hidden warm Start must not be raised — that resurfaced Start after close.
    /// Immersive cover occludes the toast HWND; see `immersive_toast_overlay`.
    pub(crate) fn raise_interactive_ui_above_toast(&self, focus: bool) -> Task<Message> {
        let mut task = Task::none();
        if let Some(id) = self.popup_window {
            task = task.chain(raise_window_topmost(id));
        }
        if self.start_visible {
            if let Some(id) = self.start_window {
                task = task.chain(raise_window_topmost(id));
            }
        } else if focus && self.start_window.is_some() {
            // Resting frames raise every tick — only log on settle/sync (focus).
            crate::controller::hid::diag::diag_info("ui-diag: start raise skipped (hidden)");
        }
        if let Some(id) = self.configure_window {
            task = task.chain(raise_window_topmost(id));
        }
        if focus {
            task.chain(self.refocus_interactive_ui())
        } else {
            task
        }
    }

    /// Re-assert toast topmost, then UI above it, after Settings / Start / popup
    /// open or close — only while the toast is Resting (safe to share presents).
    pub(crate) fn sync_toast_zorder(&self) -> Task<Message> {
        let Some(id) = self.toast_window else {
            return Task::none();
        };
        if self.toast_message.is_none() || !self.toast_machine.is_resting() {
            return Task::none();
        }
        crate::controller::hid::diag::diag_info("ui-diag: sync toast z-order (toast + raise UI)");
        window::set_level(id, window::Level::AlwaysOnTop)
            .chain(set_toast_topmost(id))
            .chain(self.raise_interactive_ui_above_toast(true))
    }

    pub(crate) fn handle_place_toast(&mut self, id: window::Id, generation: u64) -> Task<Message> {
        if generation != self.toast_generation || self.toast_message.is_none() {
            return Task::none();
        }
        // Sync placement (Win32 work area on Windows); no monitor_size hop.
        let placement = toast_placement(self.session.prefs.toast_position, None);
        self.toast_placement = Some(placement);
        let start = Point::new(placement.x, placement.outside_y);
        let percent = self
            .toast_message
            .as_ref()
            .map(|m| m.percent())
            .unwrap_or(0);
        let body = self
            .toast_message
            .as_ref()
            .map(|m| m.body.as_str())
            .unwrap_or("-");
        let composite_only = self.immersive_hosts_toast();
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: place toast gen={generation} body={body} percent={percent} composite_only={}",
            u8::from(composite_only)
        ));
        crate::platform::wgpu_diag::note_toast_remount();
        // Do not raise interactive UI here — machine emits RaiseInteractive
        // only after Resting so Start create cannot starve the slide.
        // Immersive Start hosts the card; keep the toast HWND hidden.
        let mut place = remount_toast_surface(id, generation)
            .chain(move_window(id, start))
            .chain(window::set_level(id, window::Level::AlwaysOnTop));
        if !composite_only {
            place = place
                .chain(show_toast_without_activate(id))
                .chain(invalidate_toast(id));
        }
        place.chain(Task::done(Message::ToastShown { generation }))
    }

    pub(crate) fn handle_toast_frame(&mut self) -> Task<Message> {
        let Some(generation) = self.toast_machine.generation() else {
            return Task::none();
        };
        let now = Instant::now();
        let raw_dt = now.saturating_duration_since(self.toast_anim_started);
        self.toast_anim_started = now;
        self.toast_step(toast_machine::Event::Frame { generation, raw_dt })
    }

    pub(crate) fn handle_toast_hide_deferred(&mut self) -> Task<Message> {
        if self.toast_message.is_some() || !self.toast_queue.is_empty() {
            return Task::none();
        }
        match self.toast_window {
            Some(id) => {
                crate::controller::hid::diag::diag_info("ui-diag: toast hide (deferred)");
                crate::platform::wgpu_diag::note_toast_hide();
                hide_toast(id)
            }
            None => Task::none(),
        }
    }
}
