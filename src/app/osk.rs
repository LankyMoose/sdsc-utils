//! System OSK session: input routing, text-action dispatch, and the topmost
//! no-activate overlay window. The Browser never owns OSK state or layout.

#[cfg(debug_assertions)]
use crate::domain::pad::PadSample;
use crate::domain::pad::{
    NavAction, NavHalf, NavProfile, OskInputSlot, PadAction, PadButton, PadButtonAction,
};
use crate::persist::prefs::OskMode;
use crate::platform::ui_sound::UiSoundKind;
use crate::ui::osk::dual::Half;
use crate::ui::osk::grid::GridKey;
use crate::ui::osk::{Dir, OskMessage, OskState, OskTarget, Output};
use iced::{Point, Size, Task, window};
use std::sync::Arc;

use super::{App, Message};

/// System-owned OSK session. `state` is pure UI state; the overlay widget owns
/// analog polling via the shared slot.
pub struct SystemOsk {
    pub window: Option<window::Id>,
    pub state: OskState,
    pub pending_show: bool,
    pub cover: Option<crate::ui::layout::MonitorCover>,
    pub slot: Arc<OskInputSlot>,
    /// Incremented on open/close so the pull-model widget drops arming and
    /// latch state even if its hidden window missed the transition redraw.
    pub nav_epoch: u64,
}

impl SystemOsk {
    pub fn new(mode: OskMode, slot: Arc<OskInputSlot>) -> Self {
        Self {
            window: None,
            state: OskState::new(mode),
            pending_show: false,
            cover: None,
            slot,
            nav_epoch: 0,
        }
    }
}

impl App {
    /// Open the system keyboard for a text host. Called only from the
    /// first Cross on a page text field (not focus/autofocus).
    pub(crate) fn request_osk_open(&mut self, target: OskTarget) -> Task<Message> {
        if self.osk.state.open {
            if self.osk.state.target != Some(target) {
                self.osk.state.target = Some(target);
            }
            return self.show_osk_window();
        }
        self.osk.state.open(target);
        self.osk.nav_epoch = self.osk.nav_epoch.wrapping_add(1);
        crate::platform::trace::event(crate::platform::trace::Lane::Platform, "osk.open")
            .field("target", target.label())
            .field("mode", self.osk.state.mode.label())
            .emit();
        if self.osk.window.is_some() {
            return self.show_osk_window();
        }
        let cover =
            crate::ui::layout::primary_monitor_cover().unwrap_or(crate::ui::layout::MonitorCover {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            });
        self.osk.cover = Some(cover);
        self.osk.pending_show = true;
        let width = 1120.0_f32.min((cover.width - 40.0).max(320.0));
        let height = 400.0_f32.min((cover.height - 40.0).max(240.0));
        let x = cover.x + (cover.width - width) * 0.5;
        let y = cover.y + cover.height - height - 24.0;
        let (id, open) = window::open(window::Settings {
            size: Size::new(width, height),
            position: window::Position::Specific(Point::new(x, y)),
            // Hidden until the no-activate/topmost style is applied, so the
            // overlay never steals focus from the Browser/app underneath.
            visible: false,
            transparent: crate::ui::chrome::transparent_windows(),
            resizable: false,
            decorations: false,
            level: window::Level::AlwaysOnTop,
            exit_on_close_request: true,
            platform_specific: crate::ui::layout::overlay_platform_specific(),
            ..window::Settings::default()
        });
        self.osk.window = Some(id);
        open.map(Message::OskOpened)
    }

    pub(crate) fn on_osk_opened(&mut self, id: window::Id) -> Task<Message> {
        if self.osk.window != Some(id) {
            return Task::none();
        }
        self.osk.pending_show = false;
        crate::controller::hid::diag::diag_info("ui-diag: osk window open");
        crate::ui::layout::disable_window_transitions(id)
            .chain(crate::ui::layout::show_overlay_without_activate(id))
            .chain(window::set_level(id, window::Level::AlwaysOnTop))
            .chain(crate::ui::layout::invalidate_window(id))
    }

    fn show_osk_window(&mut self) -> Task<Message> {
        let Some(id) = self.osk.window else {
            return Task::none();
        };
        if self.osk.pending_show {
            return Task::none();
        }
        window::set_level(id, window::Level::AlwaysOnTop)
            .chain(crate::ui::layout::show_overlay_without_activate(id))
            .chain(crate::ui::layout::invalidate_window(id))
    }

    pub(crate) fn close_osk(&mut self, reason: &'static str) -> Task<Message> {
        if !self.osk.state.open {
            return Task::none();
        }
        let target = self.osk.state.target;
        self.osk.state.close();
        self.osk.nav_epoch = self.osk.nav_epoch.wrapping_add(1);
        // Hardware/IME handoff keeps the URL editor alive: the key that closed
        // the OSK is about to be handled as URL text. Every other close path
        // (mouse, pad, Home, Browser close) cancels the edit.
        if target == Some(OskTarget::BrowserUrl) && !matches!(reason, "keyboard" | "ime") {
            self.browser.panel.cancel_url_edit();
        }
        crate::platform::trace::event(crate::platform::trace::Lane::Platform, "osk.close")
            .field("reason", reason)
            .emit();
        let Some(id) = self.osk.window else {
            return Task::none();
        };
        // Hide, don't destroy: the overlay stays warm for the next text field,
        // and keyboard focus never left the host app in the first place.
        let hide = window::set_mode(id, window::Mode::Hidden);
        if target == Some(OskTarget::BrowserUrl) {
            return hide.chain(self.invalidate_browser_window());
        }
        hide
    }

    /// Close only when the OSK is currently targeting `target` (e.g. the page
    /// text session blurred).
    pub(crate) fn close_osk_if_target(&mut self, target: OskTarget, reason: &'static str) {
        if self.osk.state.open && self.osk.state.target == Some(target) {
            let _ = self.close_osk(reason);
        }
    }

    pub(crate) fn set_osk_mode(&mut self, mode: OskMode) -> Task<Message> {
        if self.osk.state.mode == mode {
            return Task::none();
        }
        self.osk.state.set_mode(mode);
        if let Some(id) = self.osk.window {
            return crate::ui::layout::invalidate_window(id);
        }
        Task::none()
    }

    pub(crate) fn on_osk_window_closed(&mut self, id: window::Id) -> Task<Message> {
        if self.osk.window == Some(id) {
            self.osk.window = None;
            self.osk.pending_show = false;
            self.osk.state.close();
            self.osk.nav_epoch = self.osk.nav_epoch.wrapping_add(1);
        }
        Task::none()
    }

    /// Debug-only: capture the OSK overlay window exactly as rendered. The
    /// screenshot carries the window alpha, so QA can prove both halves exist
    /// instead of trusting behavioural state alone.
    #[cfg(debug_assertions)]
    pub(crate) fn capture_osk_window(&mut self) -> Task<Message> {
        let Some(win) = self.osk.window else {
            return Task::none();
        };
        iced::window::screenshot(win).map(Message::OskShot)
    }

    /// Debug-only: persist an OSK overlay shot to the replay sandbox's
    /// `shots/` directory and log its size for QA.
    #[cfg(debug_assertions)]
    pub(crate) fn save_osk_shot(&mut self, shot: iced::window::Screenshot) -> Task<Message> {
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
                crate::platform::app_log::warn(format!(
                    "osk-shot skipped: cannot create {dir:?}: {err}"
                ));
                return;
            }
            let path = dir.join(format!("osk-shot-{ms}.png"));
            match image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8) {
                Ok(()) => crate::platform::app_log::info(format!(
                    "osk-shot saved path={} size={w}x{h} scale={scale}",
                    path.display()
                )),
                Err(err) => crate::platform::app_log::warn(format!(
                    "osk-shot failed path={}: {err}",
                    path.display()
                )),
            }
        });
        Task::none()
    }

    /// Keyboard while OSK is open: close first, consume Escape, then hand the
    /// event to the focused host/app normally.
    pub(crate) fn on_osk_key(
        &mut self,
        id: window::Id,
        raw: crate::browser::keys::RawKey,
    ) -> Task<Message> {
        let escape = matches!(
            raw.key,
            iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)
        );
        // Escape while the URL well owns the keyboard cancels the edit, not
        // just the overlay; page-text OSK keeps the existing consume-and-close
        // hand-off.
        let reason = if escape && self.browser.panel.url_editing {
            "url_cancel"
        } else {
            "keyboard"
        };
        let close = self.close_osk(reason);
        if escape {
            return close;
        }
        close.chain(self.on_browser_raw_key(id, raw))
    }

    /// Discrete step/commit from the pull-model OSK surface.
    pub(crate) fn on_osk_message(&mut self, message: OskMessage) -> Task<Message> {
        let previous_haptic = self.haptic_pad_serial.take();
        let task = match message {
            OskMessage::Step {
                pad,
                half,
                direction,
                phase,
            } => {
                let Some(direction) = nav_dir(direction) else {
                    self.haptic_pad_serial = previous_haptic;
                    return Task::none();
                };
                self.set_haptic_pad_from_id(Some(&pad));
                self.osk.state.dual_move(to_half(half), direction);
                let feedback = match phase {
                    crate::domain::pad::NavPhase::First => NavProfile::MENU.step_feedback,
                    crate::domain::pad::NavPhase::Repeat => NavProfile::MENU.repeat_feedback,
                    crate::domain::pad::NavPhase::Release => false,
                };
                if feedback {
                    self.play_ui_cue(UiSoundKind::Nav);
                }
                Task::none()
            }
            OskMessage::Commit { pad, half } => {
                self.set_haptic_pad_from_id(Some(&pad));
                self.play_ui_cue(UiSoundKind::Action);
                let key = self.osk.state.dual_key(to_half(half));
                self.apply_osk_output(Output::Text(key))
            }
            OskMessage::GridStep { pad, direction } => {
                let Some(direction) = nav_dir(direction) else {
                    self.haptic_pad_serial = previous_haptic;
                    return Task::none();
                };
                self.set_haptic_pad_from_id(Some(&pad));
                self.osk.state.grid_move(direction);
                self.play_ui_cue(UiSoundKind::Nav);
                Task::none()
            }
        };
        self.haptic_pad_serial = previous_haptic;
        task
    }

    /// Source-side taps while the OSK owns the pad. Analog movement is handled
    /// by the pull-model surface; taps publish immediately on the edge stream.
    pub(crate) fn on_osk_tap_actions(&mut self, tap_actions: &[PadAction]) -> Task<Message> {
        let Some(action) = tap_actions
            .iter()
            .find(|action| action.action == PadButtonAction::Tap)
        else {
            return Task::none();
        };
        let previous_haptic = self.haptic_pad_serial.take();
        self.set_haptic_pad_from_id(Some(&action.pad));
        self.play_ui_cue(UiSoundKind::Action);
        let mut outputs: Vec<Output> = Vec::new();
        let mut close = false;
        match action.button {
            PadButton::Cross => match self.osk.state.mode {
                OskMode::Dual => outputs.push(Output::Space),
                OskMode::Grid => match self.osk.state.grid_key() {
                    GridKey::Char(ch) => outputs.push(Output::Text(ch)),
                    GridKey::Space => outputs.push(Output::Space),
                    GridKey::Backspace => outputs.push(Output::Backspace),
                    GridKey::Enter => outputs.push(Output::Enter),
                    GridKey::Layer => self.osk.state.toggle_layer(),
                    GridKey::Close => close = true,
                },
            },
            PadButton::Triangle => outputs.push(Output::Backspace),
            PadButton::Square => self.osk.state.toggle_layer(),
            PadButton::Options => outputs.push(Output::Enter),
            PadButton::Circle => close = true,
            _ => {}
        }
        let mut task = Task::none();
        for output in outputs {
            task = task.chain(self.apply_osk_output(output));
        }
        if close {
            task = task.chain(self.close_osk("pad"));
        }
        self.haptic_pad_serial = previous_haptic;
        task
    }

    fn apply_osk_output(&mut self, output: Output) -> Task<Message> {
        match output {
            Output::Text(ch) => self.apply_osk_text(&ch.to_string()),
            Output::Space => self.apply_osk_text(" "),
            Output::Backspace => self.apply_osk_backspace(),
            Output::Enter => self.apply_osk_enter(),
        }
    }

    fn apply_osk_text(&mut self, text: &str) -> Task<Message> {
        let target = self.osk.state.target;
        crate::platform::trace::event(crate::platform::trace::Lane::Platform, "osk.text")
            .field("target", target.map(OskTarget::label))
            .field("text", text)
            .emit();
        match target {
            Some(OskTarget::BrowserPage) => self.browser_text_insert(text),
            Some(OskTarget::BrowserUrl) => {
                self.browser.panel.url_edit_insert(text);
                return self.invalidate_browser_window();
            }
            Some(OskTarget::LauncherManualTitle) => {
                if let Some(draft) = self.launcher_state.manual_add.as_mut() {
                    draft.title.push_str(text);
                }
            }
            Some(OskTarget::LauncherManualArgs) => {
                if let Some(draft) = self.launcher_state.manual_add.as_mut() {
                    draft.args.push_str(text);
                }
            }
            None => {}
        }
        Task::none()
    }

    fn apply_osk_backspace(&mut self) -> Task<Message> {
        let target = self.osk.state.target;
        crate::platform::trace::event(crate::platform::trace::Lane::Platform, "osk.backspace")
            .field("target", target.map(OskTarget::label))
            .emit();
        match target {
            Some(OskTarget::BrowserPage) => self.browser_text_backspace(),
            Some(OskTarget::BrowserUrl) => {
                self.browser.panel.url_edit_backspace();
                return self.invalidate_browser_window();
            }
            Some(OskTarget::LauncherManualTitle) => {
                if let Some(draft) = self.launcher_state.manual_add.as_mut() {
                    draft.title.pop();
                }
            }
            Some(OskTarget::LauncherManualArgs) => {
                if let Some(draft) = self.launcher_state.manual_add.as_mut() {
                    draft.args.pop();
                }
            }
            None => {}
        }
        Task::none()
    }

    fn apply_osk_enter(&mut self) -> Task<Message> {
        let target = self.osk.state.target;
        crate::platform::trace::event(crate::platform::trace::Lane::Platform, "osk.enter")
            .field("target", target.map(OskTarget::label))
            .emit();
        match target {
            Some(OskTarget::BrowserPage) => {
                self.browser_text_enter();
                Task::none()
            }
            Some(OskTarget::BrowserUrl) => self.commit_browser_url_edit(),
            Some(OskTarget::LauncherManualTitle) | Some(OskTarget::LauncherManualArgs) => {
                self.confirm_manual_add()
            }
            None => Task::none(),
        }
    }

    /// Debug replay: directional pad press for the manual-shortcut form.
    #[cfg(debug_assertions)]
    pub(crate) fn replay_pad_dir(&mut self, action: NavAction) -> Task<Message> {
        if self.launcher_state.manual_add.is_some() {
            return self.on_manual_add_pad_action(action);
        }
        Task::none()
    }

    /// Debug replay: feed one split-stick sample through the live DQ path.
    /// Send a neutral step between same-direction moves so the stepper's
    /// release grace can reset, exactly like a real stick.
    #[cfg(debug_assertions)]
    pub(crate) fn replay_osk_stick(
        &mut self,
        left: (f32, f32),
        right: (f32, f32),
    ) -> Task<Message> {
        if !self.osk.state.open {
            return Task::none();
        }
        let mut sample = PadSample {
            lstick_x: left.0,
            lstick_y: left.1,
            rstick_x: right.0,
            rstick_y: right.1,
            ..PadSample::default()
        };
        sample.stick_x = sample.lstick_x;
        sample.stick_y = sample.lstick_y;
        let pad = crate::domain::pad::PadId("replay".into());
        let neutral = crate::ui::launcher::input::NavReading {
            id: pad.clone(),
            sample: PadSample::default(),
            source: "replay",
        };
        let reading = crate::ui::launcher::input::NavReading {
            id: pad,
            sample,
            source: "replay",
        };
        self.osk.slot.push_replay(vec![neutral]);
        self.osk.slot.push_replay(vec![reading]);
        Task::none()
    }

    /// Debug replay: press/release one trigger (or both) so DQ commits through
    /// the real edge latch. Release in the same step keeps the next synthetic
    /// press independent.
    #[cfg(debug_assertions)]
    pub(crate) fn replay_osk_trigger(&mut self, side: &str) -> Task<Message> {
        if !self.osk.state.open {
            return Task::none();
        }
        let (l2, r2) = match side {
            "left" => (true, false),
            "right" => (false, true),
            "both" => (true, true),
            _ => return Task::none(),
        };
        let pad = crate::domain::pad::PadId("replay".into());
        let neutral = crate::ui::launcher::input::NavReading {
            id: pad.clone(),
            sample: PadSample::default(),
            source: "replay",
        };
        let press = crate::ui::launcher::input::NavReading {
            id: pad.clone(),
            sample: PadSample {
                l2,
                r2,
                ..PadSample::default()
            },
            source: "replay",
        };
        let release = crate::ui::launcher::input::NavReading {
            id: pad,
            sample: PadSample::default(),
            source: "replay",
        };
        self.osk.slot.push_replay(vec![neutral]);
        self.osk.slot.push_replay(vec![press]);
        self.osk.slot.push_replay(vec![release]);
        Task::none()
    }

    /// Debug replay: inject one source-side tap through the live owner (OSK
    /// first, then Browser). Used to exercise the Cross-opens-OSK policy.
    #[cfg(debug_assertions)]
    pub(crate) fn replay_pad_tap(&mut self, button: PadButton) -> Task<Message> {
        crate::platform::trace::event(crate::platform::trace::Lane::Platform, "replay.pad_tap")
            .field("button", format!("{button:?}"))
            .emit();
        if self.osk.state.open {
            let pad = crate::domain::pad::PadId("replay".into());
            let action = PadAction {
                pad,
                button,
                action: PadButtonAction::Tap,
            };
            return self.on_osk_tap_actions(&[action]);
        }
        if self.launcher_state.manual_add.is_some() {
            let action = match button {
                PadButton::Cross => Some(NavAction::Confirm),
                PadButton::Circle => Some(NavAction::Cancel),
                _ => None,
            };
            if let Some(action) = action {
                return self.on_manual_add_pad_action(action);
            }
        }
        let pad = crate::domain::pad::PadId("replay".into());
        let reading = crate::ui::launcher::input::NavReading {
            id: pad.clone(),
            sample: crate::domain::pad::PadSample::default(),
            source: "replay",
        };
        let action = PadAction {
            pad,
            button,
            action: PadButtonAction::Tap,
        };
        if self.browser_visible {
            return self.on_browser_pad(&[reading], &[action]);
        }
        Task::none()
    }
}

/// Map a shared nav half to the DQ layout half.
fn to_half(nav: NavHalf) -> Half {
    match nav {
        NavHalf::Left => Half::Left,
        NavHalf::Right => Half::Right,
    }
}

fn nav_dir(action: NavAction) -> Option<Dir> {
    match action {
        NavAction::Up => Some(Dir::Up),
        NavAction::Down => Some(Dir::Down),
        NavAction::Left => Some(Dir::Left),
        NavAction::Right => Some(Dir::Right),
        _ => None,
    }
}
