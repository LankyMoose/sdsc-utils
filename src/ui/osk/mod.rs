//! System controller keyboard (OSK).
//!
//! Owned by the main app/OS, not by the Browser. The OSK renders as a topmost
//! no-activate overlay and receives pad input from the main HID path. Modes:
//! Quadrant (split sticks, default) and Grid (D-pad fallback).

pub mod grid;
pub mod quadrant;

use crate::persist::prefs::OskMode;
use crate::ui::theme;
use grid::{Dir, GridKey, GridKeyboard};
use iced::widget::{column, container, row, space, text};
use iced::{Element, Fill, Length};
use quadrant::{Half, HalfView, Layer, QuadrantKeyboard};

/// Text host currently owning the keyboard. The Browser is an in-process host
/// in v0; Launcher Settings is the system-app proof host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OskTarget {
    BrowserPage,
    LauncherManualTitle,
    LauncherManualArgs,
}

impl OskTarget {
    pub fn label(self) -> &'static str {
        match self {
            Self::BrowserPage => "Browser page",
            Self::LauncherManualTitle => "Shortcut title",
            Self::LauncherManualArgs => "Shortcut arguments",
        }
    }
}

/// The overlay view has no mouse/controller callbacks: pad input is routed by
/// the system session, and real mouse input dismisses the OSK. The message
/// type exists only to keep the view in iced's message-map shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OskMessage {}

/// System-owned OSK UI state. No Browser or app dependencies.
#[derive(Debug, Clone)]
pub struct OskState {
    pub open: bool,
    pub target: Option<OskTarget>,
    pub mode: OskMode,
    pub quadrant: QuadrantKeyboard,
    pub grid: GridKeyboard,
}

impl OskState {
    pub fn new(mode: OskMode) -> Self {
        Self {
            open: false,
            target: None,
            mode,
            quadrant: QuadrantKeyboard::new(),
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
        self.quadrant.reset();
        self.grid.reset();
    }

    /// Mode changes while open reset both halves/gestures.
    pub fn set_mode(&mut self, mode: OskMode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.reset();
    }

    pub fn layer(&self) -> Layer {
        match self.mode {
            OskMode::Quadrant => self.quadrant.layer(),
            OskMode::Grid => self.grid.layer,
        }
    }

    pub fn toggle_layer(&mut self) {
        match self.mode {
            OskMode::Quadrant => self.quadrant.toggle_layer(),
            OskMode::Grid => self.grid.toggle_layer(),
        }
    }

    /// Grid D-pad focus movement (no-op in Quadrant mode).
    pub fn grid_move(&mut self, direction: Dir) {
        if self.mode == OskMode::Grid {
            self.grid.move_focus(direction);
        }
    }

    pub fn grid_key(&self) -> GridKey {
        self.grid.focus_key()
    }

    pub fn quadrant_views(&self) -> (HalfView, HalfView) {
        self.quadrant.view()
    }

    pub fn home_key(&mut self, half: Half) -> quadrant::Output {
        self.quadrant.home_key(half)
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
    .align_y(iced::Alignment::Center);

    let body: Element<'static, OskMessage> = match state.mode {
        OskMode::Quadrant => quadrant_body(state),
        OskMode::Grid => grid_body(state),
    };
    let hint = match state.mode {
        OskMode::Quadrant => {
            "Sticks type · Cross Space · Triangle Bksp · R2 Enter · Square Symbols · Circle Close · L3/R3 Home"
        }
        OskMode::Grid => "D-pad Move · Cross Type · Triangle Bksp · Square Symbols · Circle Close",
    };
    let card =
        container(column![header, body, text(hint).size(11.0).color(theme::DIM),].spacing(10))
            .padding(14)
            .width(Fill)
            .style(theme::menu_panel);

    let bottom = row![space().width(Fill), card, space().width(Fill)]
        .width(Fill)
        .align_y(iced::Alignment::End);
    container(column![space().height(Fill), bottom])
        .padding([0, 20])
        .width(Fill)
        .height(Fill)
        .into()
}

fn quadrant_body(state: &OskState) -> Element<'static, OskMessage> {
    let (left, right) = state.quadrant_views();
    let layer = state.quadrant.layer();
    let (rows_l, rows_r, home_l, home_r) = match layer {
        Layer::Alpha => (
            [
                ["q", "w", "e", "r", "t"],
                ["a", "s", "d", "f", "g"],
                ["z", "x", "c", "v", "b"],
            ],
            [
                ["y", "u", "i", "o", "p"],
                ["h", "j", "k", "l", ";"],
                ["n", "m", ",", ".", "/"],
            ],
            "D",
            "K",
        ),
        Layer::Symbol => (
            [
                ["1", "2", "3", "4", "5"],
                ["-", "/", ":", ";", "("],
                [".", ",", "?", "!", "'"],
            ],
            [
                ["6", "7", "8", "9", "0"],
                [")", "$", "&", "@", "\""],
                ["#", "%", "+", "=", "~"],
            ],
            ":",
            "&",
        ),
    };
    row![
        half_column(rows_l, home_l, left),
        space().width(Length::Fixed(40.0)),
        half_column(rows_r, home_r, right),
    ]
    .width(Fill)
    .into()
}

fn half_column(
    rows: [[&'static str; 5]; 3],
    home: &'static str,
    view: HalfView,
) -> Element<'static, OskMessage> {
    let mut col = column![].spacing(4);
    for key_row in rows {
        let mut line = row![].spacing(4);
        for label in key_row {
            let active = view.preview.is_some_and(|ch| ch.to_string() == label);
            line = line.push(keycap(label, active, 54.0));
        }
        col = col.push(line);
    }
    let home_active = view.quadrant == Some(quadrant::Quadrant::Home);
    col.push(
        row![
            text("Home").size(10.0).color(theme::DIM),
            keycap(home, home_active, 54.0),
            space().width(Fill),
            text(match view.quadrant {
                Some(quadrant::Quadrant::Left) => "left",
                Some(quadrant::Quadrant::Right) => "right",
                Some(quadrant::Quadrant::Up) => "up",
                Some(quadrant::Quadrant::Down) => "down",
                Some(quadrant::Quadrant::Home) => "home",
                None => "",
            })
            .size(10.0)
            .color(theme::DIM),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center),
    )
    .width(Length::Fixed(360.0))
    .into()
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
            line = line.push(keycap(&label, focused, width));
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

fn keycap(label: &str, active: bool, width: f32) -> Element<'static, OskMessage> {
    // Static key: no on_press handler (OSK keys are not mouse-clickable).
    iced::widget::button(text(label.to_string()).size(15.0).color(if active {
        theme::BASE_BG
    } else {
        theme::INK
    }))
    .padding([8, 10])
    .width(Length::Fixed(width))
    .style(theme::chip(active))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_close_resets_target_and_halves() {
        let mut state = OskState::new(OskMode::Quadrant);
        state.open(OskTarget::BrowserPage);
        assert!(state.open);
        assert_eq!(state.target, Some(OskTarget::BrowserPage));
        state.close();
        assert!(!state.open);
        assert_eq!(state.target, None);
    }

    #[test]
    fn mode_switch_resets_and_selects_layout() {
        let mut state = OskState::new(OskMode::Quadrant);
        state.open(OskTarget::BrowserPage);
        state.quadrant.toggle_layer();
        assert_eq!(state.layer(), Layer::Symbol);
        state.set_mode(OskMode::Grid);
        assert_eq!(state.mode, OskMode::Grid);
        assert_eq!(state.layer(), Layer::Alpha);
        state.grid_move(Dir::Down);
        assert_eq!(state.grid_key(), GridKey::Char('a'));
        assert_eq!(OskMode::parse("grid"), Some(OskMode::Grid));
        assert_eq!(OskMode::parse("bogus"), None);
    }

    #[test]
    fn view_composes_in_both_modes() {
        let mut state = OskState::new(OskMode::Quadrant);
        state.open(OskTarget::BrowserPage);
        let _ = view(&state);
        state.set_mode(OskMode::Grid);
        let _ = view(&state);
    }
}
