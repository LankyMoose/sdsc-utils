//! System controller keyboard (OSK).
//!
//! Owned by the main app/OS, not by the Browser. The OSK renders as a topmost
//! no-activate overlay and receives pad input from the main HID path. One
//! layout: QWERTY, with D-pad focus movement and Cross activation.

pub mod qwerty;
pub mod surface;

pub use qwerty::OskKey;
pub use surface::surface;

use crate::domain::pad::{NavAction, PadId};
use crate::ui::theme;
use iced::widget::{button, column, container, row, space, text};
use iced::{Alignment, Element, Fill, Length};
use qwerty::QwertyKeyboard;

/// Text host currently owning the keyboard. The Browser is an in-process host
/// in v0; Launcher Settings is the system-app proof host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OskTarget {
    BrowserPage,
    BrowserUrl,
    LauncherManualTitle,
    LauncherManualArgs,
}

impl OskTarget {
    pub fn label(self) -> &'static str {
        match self {
            Self::BrowserPage => "Browser page",
            Self::BrowserUrl => "Browser URL",
            Self::LauncherManualTitle => "Shortcut title",
            Self::LauncherManualArgs => "Shortcut arguments",
        }
    }
}

/// Shared key layer for the QWERTY keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layer {
    #[default]
    Alpha,
    Symbol,
}

/// Shared directional step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

/// Text/control output from the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Text(char),
    Space,
    Backspace,
    Enter,
}

/// Discrete movement from the pull-model OSK surface. The app changes focus
/// only on these messages; analog samples stay in the shared input slot.
#[derive(Debug, Clone)]
pub enum OskMessage {
    Move { pad: PadId, direction: NavAction },
}

/// System-owned OSK UI state. No Browser or app dependencies.
#[derive(Debug, Clone)]
pub struct OskState {
    pub open: bool,
    pub target: Option<OskTarget>,
    pub qwerty: QwertyKeyboard,
}

impl OskState {
    pub fn new() -> Self {
        Self {
            open: false,
            target: None,
            qwerty: QwertyKeyboard::new(),
        }
    }

    pub fn open(&mut self, target: OskTarget) {
        self.open = true;
        self.target = Some(target);
        self.reset();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.target = None;
        self.reset();
    }

    pub fn reset(&mut self) {
        self.qwerty.reset();
    }

    pub fn layer(&self) -> Layer {
        self.qwerty.layer
    }

    pub fn toggle_layer(&mut self) {
        self.qwerty.toggle_layer();
    }

    /// Move the single focus cursor.
    pub fn move_focus(&mut self, direction: Dir) {
        self.qwerty.move_focus(direction);
    }

    pub fn key(&self) -> OskKey {
        self.qwerty.focus_key()
    }
}

impl Default for OskState {
    fn default() -> Self {
        Self::new()
    }
}

/// Full-window overlay root: transparent around a bottom-centered keyboard
/// card. Keys are static (non-interactive); the pad session drives state.
pub fn view(state: &OskState) -> Element<'static, OskMessage> {
    if !state.open {
        return space().into();
    }
    let header = row![
        text("Controller keyboard").size(15.0).color(theme::INK),
        space().width(Fill),
        text(state.target.map(OskTarget::label).unwrap_or("Text"))
            .size(12.0)
            .color(theme::MUTED),
        text(" · ").size(12.0).color(theme::DIM),
        text("QWERTY").size(12.0).color(theme::MUTED),
    ]
    .align_y(Alignment::Center)
    .width(Fill);

    let hint =
        "D-pad Move · Cross Type · Triangle Bksp · Square Symbols · Options Enter · Circle Close";
    let card = container(
        column![
            header,
            qwerty_body(state),
            text(hint).size(11.0).color(theme::DIM)
        ]
        .spacing(10)
        .width(Fill)
        .align_x(Alignment::Center),
    )
    .padding(14)
    .width(Fill)
    .style(theme::menu_panel);

    // The card owns the full overlay width. Do not wrap it in Fill spacers:
    // iced divides Fill width between every Fill child, which made the card a
    // third of the window and clipped the right half of the keyboard.
    container(column![space().height(Fill), card])
        .padding([0, 20])
        .width(Fill)
        .height(Fill)
        .into()
}

fn qwerty_body(state: &OskState) -> Element<'static, OskMessage> {
    let mut rows = column![].spacing(4).width(Fill);
    let mut flat = 0;
    for key_row in state.qwerty.rows() {
        let mut line = row![].spacing(4).width(Fill);
        for key in *key_row {
            let focused = flat == state.qwerty.focus;
            let label = key.label(state.qwerty.layer);
            line = line.push(keycap(&label, focused, Length::Fill));
            flat += 1;
        }
        rows = rows.push(line);
    }
    container(rows).width(Fill).max_width(900.0).into()
}

fn keycap(label: &str, active: bool, width: impl Into<Length>) -> Element<'static, OskMessage> {
    // Static key: no on_press handler (OSK keys are not mouse-clickable).
    button(text(label.to_string()).size(15.0).color(if active {
        theme::BASE_BG
    } else {
        theme::INK
    }))
    .padding([8, 10])
    .height(Length::Fixed(44.0))
    .width(width.into())
    .style(theme::chip(active))
    .into()
}

/// Cheap placeholder for the warm-but-closed OSK window.
///
/// The window is kept allocated for fast reopen; without this the full
/// keyboard layout would be rebuilt on every app update even while closed.
pub fn empty<'a, Message: 'a>() -> Element<'a, Message> {
    container(space())
        .width(Length::Shrink)
        .height(Length::Shrink)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_close_resets_focus() {
        let mut state = OskState::new();
        state.open(OskTarget::BrowserPage);
        assert!(state.open);
        assert_eq!(state.target, Some(OskTarget::BrowserPage));
        state.move_focus(Dir::Right);
        assert_eq!(state.key(), OskKey::Char('w'));
        state.close();
        assert!(!state.open);
        assert_eq!(state.target, None);
        assert_eq!(state.key(), OskKey::Char('q'));
    }

    #[test]
    fn layer_toggles_and_focus_stays_in_range() {
        let mut state = OskState::new();
        state.open(OskTarget::BrowserPage);
        state.move_focus(Dir::Down);
        assert_eq!(state.key(), OskKey::Char('a'));
        state.toggle_layer();
        assert_eq!(state.layer(), Layer::Symbol);
        assert_eq!(state.key(), OskKey::Char('-'));
        assert!(state.qwerty.focus < state.qwerty.key_count());
    }

    #[test]
    fn view_composes() {
        let mut state = OskState::new();
        state.open(OskTarget::BrowserPage);
        let _ = view(&state);
    }
}
