//! System OSK session: input routing, text-action dispatch, and the topmost
//! no-activate overlay window. The Browser never owns OSK state or layout.

use crate::domain::pad::{NavAction, NavStepper, PadAction, PadButton, PadButtonAction};
use crate::persist::prefs::OskMode;
use crate::platform::ui_sound::UiSoundKind;
use crate::ui::osk::grid::{Dir, GridKey};
use crate::ui::osk::quadrant::{Half, Output};
use crate::ui::osk::{OskState, OskTarget};
use iced::{Point, Size, Task, window};
use std::time::Instant;

use super::{App, Message};

/// System-owned OSK session. `state` is pure UI state; this struct owns the
/// overlay window and pad edge latches.
pub struct SystemOsk {
    pub window: Option<window::Id>,
    pub state: OskState,
    pub pending_show: bool,
    pub cover: Option<crate::ui::layout::MonitorCover>,
    pub r2_latched: bool,
    pub l3_latched: bool,
    pub r3_latched: bool,
    pub grid_stepper: NavStepper,
}

impl SystemOsk {
    pub fn new(mode: OskMode) -> Self {
        Self {
            window: None,
            state: OskState::new(mode),
            pending_show: false,
            cover: None,
            r2_latched: false,
            l3_latched: false,
            r3_latched: false,
            grid_stepper: NavStepper::default(),
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
                if target == OskTarget::BrowserPage {
                    self.remote_browser_osk(true);
                }
            }
            return self.show_osk_window();
        }
        self.osk.state.open(target);
        if target == OskTarget::BrowserPage {
            self.remote_browser_osk(true);
        }
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
        let browser_page = self.osk.state.target == Some(OskTarget::BrowserPage);
        self.osk.state.close();
        if browser_page {
            self.remote_browser_osk(false);
        }
        self.osk.r2_latched = false;
        self.osk.l3_latched = false;
        self.osk.r3_latched = false;
        self.osk.grid_stepper.reset();
        crate::platform::trace::event(crate::platform::trace::Lane::Platform, "osk.close")
            .field("reason", reason)
            .emit();
        let Some(id) = self.osk.window else {
            return Task::none();
        };
        // Hide, don't destroy: the overlay stays warm for the next text field,
        // and keyboard focus never left the host app in the first place.
        window::set_mode(id, window::Mode::Hidden)
    }

    pub(crate) fn set_osk_mode(&mut self, mode: OskMode) -> Task<Message> {
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
        }
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
        let close = self.close_osk("keyboard");
        if escape {
            return close;
        }
        close.chain(self.on_shell_key(id, raw))
    }

    /// Pad tick while the OSK is open. The system owns the pad before the host
    /// app, except PS/Home which stays global (and closes the OSK).
    pub(crate) fn on_osk_pad(
        &mut self,
        readings: &[crate::ui::launcher::input::NavReading],
        tap_actions: &[PadAction],
    ) -> Task<Message> {
        if readings.is_empty() {
            return Task::none();
        }
        if self.home_chord_fires(readings) {
            crate::controller::hid::diag::diag_info("ui-diag: PS opens home over OSK");
            return self.close_osk("home").chain(self.toggle_home());
        }
        let Some(reading) = crate::ui::launcher::input::preferred_reading(readings) else {
            return Task::none();
        };
        let now = Instant::now();
        let sample = &reading.sample;
        let mut outputs: Vec<Output> = Vec::new();

        match self.osk.state.mode {
            OskMode::Quadrant => {
                outputs = self.osk.state.quadrant.update(
                    (sample.lstick_x, sample.lstick_y),
                    (sample.rstick_x, sample.rstick_y),
                );
            }
            OskMode::Grid => {
                let masked = crate::ui::launcher::input::dpad_only(sample);
                if let Some(action) = self.osk.grid_stepper.update(&masked, now, true, true) {
                    let dir = match action {
                        NavAction::Up => Some(Dir::Up),
                        NavAction::Down => Some(Dir::Down),
                        NavAction::Left => Some(Dir::Left),
                        NavAction::Right => Some(Dir::Right),
                        _ => None,
                    };
                    if let Some(dir) = dir {
                        self.osk.state.grid_move(dir);
                        self.play_ui_cue(UiSoundKind::Nav);
                    }
                }
            }
        }

        // R2 Enter (edge): shared by both modes.
        let r2 = sample.r2;
        if r2 && !self.osk.r2_latched {
            outputs.push(Output::Enter);
        }
        self.osk.r2_latched = r2;

        // L3/R3 home keys are Quadrant controls (tap only).
        let l3 = sample
            .held
            .contains(&crate::ui::launcher::gesture::GestureControl::L3);
        if self.osk.state.mode == OskMode::Quadrant && l3 && !self.osk.l3_latched {
            outputs.push(self.osk.state.home_key(Half::Left));
        }
        self.osk.l3_latched = l3;
        let r3 = sample
            .held
            .contains(&crate::ui::launcher::gesture::GestureControl::R3);
        if self.osk.state.mode == OskMode::Quadrant && r3 && !self.osk.r3_latched {
            outputs.push(self.osk.state.home_key(Half::Right));
        }
        self.osk.r3_latched = r3;

        let mut close = false;
        if let Some(action) = tap_actions
            .iter()
            .find(|action| action.action == PadButtonAction::Tap)
        {
            match action.button {
                PadButton::Cross => match self.osk.state.mode {
                    OskMode::Quadrant => outputs.push(Output::Space),
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
                PadButton::Circle => close = true,
                PadButton::L3 if self.osk.state.mode == OskMode::Quadrant => {
                    outputs.push(self.osk.state.home_key(Half::Left));
                }
                PadButton::R3 if self.osk.state.mode == OskMode::Quadrant => {
                    outputs.push(self.osk.state.home_key(Half::Right));
                }
                _ => {}
            }
        }

        let mut task = Task::none();
        for output in outputs {
            task = task.chain(self.apply_osk_output(output));
        }
        if close {
            task = task.chain(self.close_osk("pad"));
        }
        if let Some(id) = self.osk.window {
            task = task.chain(crate::ui::layout::invalidate_window(id));
        }
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

    /// Debug replay: feed one split-stick sample through the real Quadrant
    /// state machine and apply any committed text.
    #[cfg(debug_assertions)]
    pub(crate) fn replay_osk_stick(
        &mut self,
        left: (f32, f32),
        right: (f32, f32),
    ) -> Task<Message> {
        if !self.osk.state.open {
            return Task::none();
        }
        let outputs = self.osk.state.quadrant.update(left, right);
        let mut task = Task::none();
        for output in outputs {
            task = task.chain(self.apply_osk_output(output));
        }
        if let Some(id) = self.osk.window {
            task = task.chain(crate::ui::layout::invalidate_window(id));
        }
        task
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
            return self.on_osk_pad(&[reading], &[action]);
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
