//! System OSK session: input routing, text-action dispatch, and the topmost
//! no-activate overlay window. The Browser never owns OSK state or layout.

#[cfg(debug_assertions)]
use crate::domain::pad::PadSample;
use crate::domain::pad::{NavAction, OskInputSlot, PadAction, PadButton, PadButtonAction};
use crate::platform::ui_sound::UiSoundKind;
use crate::ui::osk::qwerty::OskKey;
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
    pub fn new(slot: Arc<OskInputSlot>) -> Self {
        Self {
            window: None,
            state: OskState::new(),
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
            .field("mode", "qwerty")
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
        // Must use the raw hide: the OSK was shown with SW_SHOWNOACTIVATE, so
        // winit thinks it is already hidden and `set_mode(Hidden)` no-ops.
        let hide = crate::ui::layout::hide_overlay_without_activate(id);
        if target == Some(OskTarget::BrowserUrl) {
            return hide.chain(self.invalidate_browser_window());
        }
        hide
    }

    /// Close only when the OSK is currently targeting `target` (e.g. the page
    /// text session blurred). Returns the hide task so event-driven callers do
    /// not drop the raw `ShowWindow(SW_HIDE)` (the window would stay topmost).
    pub(crate) fn close_osk_if_target(
        &mut self,
        target: OskTarget,
        reason: &'static str,
    ) -> Task<Message> {
        if self.osk.state.open && self.osk.state.target == Some(target) {
            self.close_osk(reason)
        } else {
            Task::none()
        }
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
        Task::perform(
            super::spawn_blocking(move || {
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
            }),
            |_| Message::ReplayCaptureDone,
        )
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

    /// Discrete move from the pull-model OSK surface.
    pub(crate) fn on_osk_message(&mut self, message: OskMessage) -> Task<Message> {
        let OskMessage::Move { pad, direction } = message;
        let previous_haptic = self.haptic_pad_serial.take();
        let Some(direction) = nav_dir(direction) else {
            self.haptic_pad_serial = previous_haptic;
            return Task::none();
        };
        self.set_haptic_pad_from_id(Some(&pad));
        self.osk.state.move_focus(direction);
        self.play_ui_cue(UiSoundKind::Nav);
        self.haptic_pad_serial = previous_haptic;
        Task::none()
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
            PadButton::Cross => match self.osk.state.key() {
                OskKey::Char(ch) => outputs.push(Output::Text(ch)),
                OskKey::Space => outputs.push(Output::Space),
                OskKey::Backspace => outputs.push(Output::Backspace),
                OskKey::Enter => outputs.push(Output::Enter),
                OskKey::Layer => self.osk.state.toggle_layer(),
                OskKey::Close => close = true,
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

    /// Debug replay: directional pad press. Routes the manual-shortcut form,
    /// else the Browser cursor stepper (one complete press/release).
    #[cfg(debug_assertions)]
    pub(crate) fn replay_pad_dir(&mut self, action: NavAction) -> Task<Message> {
        if self.osk.state.open {
            return self.replay_osk_dir(action);
        }
        if self.launcher_state.manual_add.is_some() {
            return self.on_manual_add_pad_action(action);
        }
        if self.home_visible || !self.browser_visible {
            return Task::none();
        }
        let (up, down, left, right) = match action {
            NavAction::Up => (true, false, false, false),
            NavAction::Down => (false, true, false, false),
            NavAction::Left => (false, false, true, false),
            NavAction::Right => (false, false, false, true),
            _ => return Task::none(),
        };
        let pad = crate::domain::pad::PadId("replay".into());
        let press = crate::ui::launcher::input::NavReading {
            id: pad.clone(),
            sample: PadSample {
                dpad_up: up,
                dpad_down: down,
                dpad_left: left,
                dpad_right: right,
                ..PadSample::default()
            },
            source: "replay",
        };
        let release = crate::ui::launcher::input::NavReading {
            id: pad,
            sample: PadSample::default(),
            source: "replay",
        };
        // Two real owner ticks: the press fires the jump, the release resets
        // the stepper so the next synthetic press is a fresh First phase.
        let jump = self.on_browser_pad(&[press], &[]);
        let reset = self.on_browser_pad(&[release], &[]);
        jump.chain(reset)
    }

    /// Debug replay: one D-pad press/release through the pull-model OSK slot.
    /// A neutral frame first arms the nav bank, then the press moves focus and
    /// the release resets the stepper for the next step.
    #[cfg(debug_assertions)]
    fn replay_osk_dir(&mut self, action: NavAction) -> Task<Message> {
        if !self.osk.state.open {
            return Task::none();
        }
        let (up, down, left, right) = match action {
            NavAction::Up => (true, false, false, false),
            NavAction::Down => (false, true, false, false),
            NavAction::Left => (false, false, true, false),
            NavAction::Right => (false, false, false, true),
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
                dpad_up: up,
                dpad_down: down,
                dpad_left: left,
                dpad_right: right,
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
        self.osk
            .window
            .map(crate::ui::layout::invalidate_window)
            .unwrap_or_else(Task::none)
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

/// Map a shared nav action to an OSK direction.
fn nav_dir(action: NavAction) -> Option<Dir> {
    match action {
        NavAction::Up => Some(Dir::Up),
        NavAction::Down => Some(Dir::Down),
        NavAction::Left => Some(Dir::Left),
        NavAction::Right => Some(Dir::Right),
        _ => None,
    }
}
