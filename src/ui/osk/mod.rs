//! System controller keyboard (OSK).
//!
//! Owned by the main app/OS, not by the Browser. The OSK renders as a topmost
//! no-activate overlay and receives pad input from the main HID path. Modes:
//! Dual QWERTY (split sticks, default) and Grid (D-pad fallback).

pub mod dual;
pub mod grid;
pub mod surface;

pub use surface::surface;

use crate::domain::pad::{NavAction, NavHalf, PadId};
use crate::persist::prefs::OskMode;
use crate::ui::theme;
use dual::{Cursor, DualKeyboard, Half};
use grid::{GridKey, GridKeyboard};
use iced::widget::{button, column, container, row, space, text};
use iced::{Element, Fill, Length};

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

/// Shared key layer for both keyboard modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layer {
    #[default]
    Alpha,
    Symbol,
}

/// Shared directional step (sticks in DQ, D-pad in Grid).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

/// Text/control output from a keyboard mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Text(char),
    Space,
    Backspace,
    Enter,
}

/// Discrete output from the pull-model OSK surface. The app changes cursor or
/// dispatches text/feedback only on these messages; analog samples stay in the
/// shared input slot.
#[derive(Debug, Clone)]
pub enum OskMessage {
    Step {
        pad: PadId,
        half: NavHalf,
        direction: NavAction,
        phase: crate::domain::pad::NavPhase,
    },
    Commit {
        pad: PadId,
        half: NavHalf,
    },
    GridStep {
        pad: PadId,
        direction: NavAction,
    },
}

/// System-owned OSK UI state. No Browser or app dependencies.
#[derive(Debug, Clone)]
pub struct OskState {
    pub open: bool,
    pub target: Option<OskTarget>,
    pub mode: OskMode,
    pub dual: DualKeyboard,
    pub grid: GridKeyboard,
}

impl OskState {
    pub fn new(mode: OskMode) -> Self {
        Self {
            open: false,
            target: None,
            mode,
            dual: DualKeyboard::new(),
            grid: GridKeyboard::new(),
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
        self.dual.reset();
        self.grid.reset();
    }

    /// Mode changes while open reset both layouts.
    pub fn set_mode(&mut self, mode: OskMode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.reset();
    }

    pub fn layer(&self) -> Layer {
        match self.mode {
            OskMode::Dual => self.dual.layer(),
            OskMode::Grid => self.grid.layer,
        }
    }

    pub fn toggle_layer(&mut self) {
        match self.mode {
            OskMode::Dual => self.dual.toggle_layer(),
            OskMode::Grid => self.grid.toggle_layer(),
        }
    }

    /// Grid D-pad focus movement (no-op in Dual QWERTY mode).
    pub fn grid_move(&mut self, direction: Dir) {
        if self.mode == OskMode::Grid {
            self.grid.move_focus(direction);
        }
    }

    pub fn grid_key(&self) -> GridKey {
        self.grid.focus_key()
    }

    pub fn dual_view(&self) -> dual::DualView {
        self.dual.view()
    }

    /// Move one DQ half's cursor. The app only calls this in Dual mode.
    pub fn dual_move(&mut self, half: Half, direction: Dir) {
        self.dual.move_cursor(half, direction);
    }

    /// Selected DQ key for a half. The app only calls this in Dual mode.
    pub fn dual_key(&self, half: Half) -> char {
        self.dual.key(half)
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
        text(state.mode.label()).size(12.0).color(theme::MUTED),
    ]
    .align_y(iced::Alignment::Center)
    .width(Fill);

    let body: Element<'static, OskMessage> = match state.mode {
        OskMode::Dual => dual_body(state),
        OskMode::Grid => grid_body(state),
    };
    let hint = match state.mode {
        OskMode::Dual => {
            "Sticks move cursors · L2/R2 type · Cross Space · Triangle Bksp · Square Symbols · Options Enter · Circle Close"
        }
        OskMode::Grid => {
            "D-pad Move · Cross Type · Triangle Bksp · Square Symbols · Options Enter · Circle Close"
        }
    };
    let card = container(
        column![header, body, text(hint).size(11.0).color(theme::DIM)]
            .spacing(10)
            .width(Fill)
            .align_x(iced::Alignment::Center),
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

fn dual_body(state: &OskState) -> Element<'static, OskMessage> {
    let view = state.dual_view();
    let body = row![
        half_column(view.left, state.dual.rows(Half::Left)),
        space().width(Length::Fixed(32.0)),
        half_column(view.right, state.dual.rows(Half::Right)),
    ]
    .width(Fill);
    container(body).width(Fill).max_width(900.0).into()
}

fn half_column(
    cursor: Cursor,
    rows: &'static [[char; dual::COLS]; dual::ROWS],
) -> Element<'static, OskMessage> {
    let mut col = column![].spacing(4).width(Fill);
    for (r, keys) in rows.iter().enumerate() {
        let mut line = row![].spacing(4).width(Fill);
        for (c, label) in keys.iter().enumerate() {
            let active = cursor.row == r && cursor.col == c;
            line = line.push(keycap(&label.to_string(), active, Length::Fill));
        }
        col = col.push(line);
    }
    col.into()
}

fn grid_body(state: &OskState) -> Element<'static, OskMessage> {
    let mut flat = 0;
    let mut rows: Vec<Element<'static, OskMessage>> = Vec::new();
    for key_row in state.grid.rows() {
        let mut line = row![].spacing(4);
        for key in *key_row {
            let focused = flat == state.grid.focus;
            let label = key.label(state.grid.layer);
            let width = match key {
                GridKey::Char(_) => 54.0,
                _ => 74.0,
            };
            line = line.push(keycap(&label, focused, Length::Fixed(width)));
            flat += 1;
        }
        rows.push(line.into());
    }
    let mut col = column![].spacing(4).width(Fill);
    for line in rows {
        col = col.push(line);
    }
    col.into()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_close_resets_target_and_cursors() {
        let mut state = OskState::new(OskMode::Dual);
        state.open(OskTarget::BrowserPage);
        assert!(state.open);
        assert_eq!(state.target, Some(OskTarget::BrowserPage));
        state.dual_move(Half::Left, Dir::Right);
        assert_eq!(state.dual_key(Half::Left), 'f');
        state.close();
        assert!(!state.open);
        assert_eq!(state.target, None);
        assert_eq!(state.dual_key(Half::Left), 'd');
        assert_eq!(state.dual_key(Half::Right), 'k');
    }

    #[test]
    fn mode_switch_resets_and_selects_layout() {
        let mut state = OskState::new(OskMode::Dual);
        state.open(OskTarget::BrowserPage);
        state.toggle_layer();
        assert_eq!(state.layer(), Layer::Symbol);
        state.set_mode(OskMode::Grid);
        assert_eq!(state.mode, OskMode::Grid);
        assert_eq!(state.layer(), Layer::Alpha);
        state.grid_move(Dir::Down);
        assert_eq!(state.grid_key(), GridKey::Char('a'));
        assert_eq!(OskMode::parse("grid"), Some(OskMode::Grid));
        assert_eq!(OskMode::parse("dual"), Some(OskMode::Dual));
    }

    #[test]
    fn dual_mode_moves_only_the_addressed_half() {
        let mut state = OskState::new(OskMode::Dual);
        state.open(OskTarget::BrowserUrl);
        state.dual_move(Half::Right, Dir::Left);
        assert_eq!(state.dual_key(Half::Left), 'd');
        assert_eq!(state.dual_key(Half::Right), 'j');
        state.dual_move(Half::Left, Dir::Up);
        assert_eq!(state.dual_key(Half::Left), 'e');
        assert_eq!(state.dual_key(Half::Right), 'j');
    }

    #[test]
    fn view_composes_in_both_modes() {
        let mut state = OskState::new(OskMode::Dual);
        state.open(OskTarget::BrowserPage);
        let _ = view(&state);
        state.set_mode(OskMode::Grid);
        let _ = view(&state);
    }
}
