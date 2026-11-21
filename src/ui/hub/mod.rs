//! PS overlay hub: the topmost app switcher. On alpha-capable backends its
//! window covers the monitor and dims the whole screen behind the card.
//!
//! Phase 1 of the hub pivot. The hub is the auto-open target and the only
//! always-on-top shell surface; Start and (later) Browser stay normal-level
//! windows underneath. It is modal for pad input while open: the underlying
//! app keeps its last frame but receives no nav.
//!
//! Tiles are stable identities, never row indices. The Browser tile stays
//! visible even when its engine is missing (inline install affordance lands
//! with the DLC work).

use crate::ui::svg_icon;
use crate::ui::text_fit;
use crate::ui::theme;
use iced::widget::{button, column, container, row, space, svg, text};
use iced::{Alignment, Background, Border, Color, Element, Fill, Length, Shadow, Theme, Vector};
use std::time::Instant;

/// Hub window logical size. Kept small and bottom-weighted so the app
/// underneath stays readable.
pub const HUB_WIDTH: f32 = 620.0;
pub const HUB_HEIGHT: f32 = 196.0;
/// Fraction of monitor height where the hub card starts.
pub const HUB_TOP_FRACTION: f32 = 0.66;

const TILE_W: f32 = 178.0;
const TILE_H: f32 = 92.0;
const TILE_GAP: f32 = 12.0;
const ICON: f32 = 26.0;

/// Stable tile identity. The order in [`TILES`] is the render/selection
/// order; message handlers match on identity, never on an index captured in
/// the UI (the catalog may grow/reorder later).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubTile {
    Start,
    Browser,
    AddApps,
}

/// Visible tiles, left to right.
pub const TILES: [HubTile; 3] = [HubTile::Start, HubTile::Browser, HubTile::AddApps];

/// What the hub asks the shell to do. Pure data; the App owns windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubMessage {
    Move(i32),
    /// Open the app catalog / engine installer.
    CatalogOpen,
    /// Back out of the catalog to the app tiles.
    CatalogClose,
    /// Install/retry the optional BrowserEngine package.
    InstallEngine,
    /// Remove the locally installed BrowserEngine runtime.
    UninstallEngine,
    /// Highlight a tile (hover/pad focus) without activating it.
    Select(usize),
    /// Mouse click: select and activate in one step.
    Activate(usize),
    Confirm,
    Close,
}

/// Hub render/input state. Selection is an index into [`TILES`]; activation
/// resolves it through [`State::selected_tile`].
#[derive(Debug)]
pub struct State {
    selected: usize,
    /// Whether the Start tile is enabled in prefs. Disabled tiles still show
    /// (stable layout) but Confirm is a no-op.
    pub start_enabled: bool,
    /// Engine worker live. Drives the Browser tile subtitle; the tile stays
    /// visible either way.
    pub engine_ready: bool,
    /// D-pad/stick repeat for hub movement.
    nav: crate::domain::pad::NavStepper,
    /// Face-button rising-edge latches (Cross confirm, Circle close).
    pub prev_cross: bool,
    pub prev_circle: bool,
    /// Catalog/install card replaces the tile row while open.
    pub catalog_open: bool,
    /// Engine install/download in flight.
    pub installing: bool,
    /// Last install failure (user-facing).
    pub install_error: Option<String>,
    /// Catalog focus: 0 = primary action, 1 = Back.
    pub catalog_focus: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            selected: 0,
            start_enabled: true,
            engine_ready: false,
            nav: crate::domain::pad::NavStepper::default(),
            prev_cross: false,
            prev_circle: false,
            catalog_open: false,
            installing: false,
            install_error: None,
            catalog_focus: 0,
        }
    }
}

impl State {
    pub fn selected(&self) -> usize {
        self.selected.min(TILES.len() - 1)
    }

    pub fn selected_tile(&self) -> HubTile {
        TILES[self.selected()]
    }

    /// Move the highlight, wrapping. Vertical input is treated like the
    /// matching horizontal direction — the hub is one row.
    pub fn move_selection(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        let len = TILES.len() as i32;
        let next = (self.selected() as i32 + delta).rem_euclid(len);
        self.selected = next as usize;
    }

    pub fn select(&mut self, index: usize) {
        if index < TILES.len() {
            self.selected = index;
        }
    }

    pub fn tile_enabled(&self, tile: HubTile) -> bool {
        match tile {
            HubTile::Start => self.start_enabled,
            HubTile::Browser | HubTile::AddApps => true,
        }
    }

    /// Reset edge latches and movement repeat when the hub is opened, so a
    /// held button from the opening gesture cannot fire immediately.
    pub fn on_open(&mut self, start_enabled: bool, engine_ready: bool) {
        self.start_enabled = start_enabled;
        self.engine_ready = engine_ready;
        self.nav = crate::domain::pad::NavStepper::default();
        self.prev_cross = false;
        self.prev_circle = false;
        self.catalog_open = false;
        self.install_error = None;
    }

    pub fn open_catalog(&mut self) {
        self.catalog_open = true;
        self.install_error = None;
        self.catalog_focus = 0;
    }

    pub fn catalog_move(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        self.catalog_focus = if delta > 0 { 1 } else { 0 };
    }

    pub fn catalog_primary_is_back(&self) -> bool {
        self.catalog_focus == 1
    }

    pub fn close_catalog(&mut self) {
        self.catalog_open = false;
        self.install_error = None;
    }

    pub fn set_installing(&mut self, installing: bool) {
        self.installing = installing;
        if installing {
            self.install_error = None;
        }
    }

    pub fn install_done(&mut self, ready: bool, error: Option<String>) {
        self.installing = false;
        self.engine_ready = ready;
        self.install_error = error;
    }

    /// Feed one pad tick. Returns a hub message for a movement or face edge.
    /// Callers may also read [`Self::prev_cross`] etc. after the call.
    pub fn pad_tick(
        &mut self,
        sample: &crate::domain::pad::PadSample,
        now: Instant,
    ) -> Option<HubMessage> {
        let mut message = None;
        if let Some(action) = self.nav.update(sample, now, true, true) {
            message = match action {
                crate::domain::pad::NavAction::Left | crate::domain::pad::NavAction::Up => {
                    Some(HubMessage::Move(-1))
                }
                crate::domain::pad::NavAction::Right | crate::domain::pad::NavAction::Down => {
                    Some(HubMessage::Move(1))
                }
                _ => None,
            };
        }
        if sample.cross && !self.prev_cross {
            message = Some(HubMessage::Confirm);
        } else if sample.circle && !self.prev_circle {
            message = Some(HubMessage::Close);
        }
        self.prev_cross = sample.cross;
        self.prev_circle = sample.circle;
        message
    }
}

/// Render the hub card. The window root is transparent; the card is the only
/// opaque surface.
pub fn view(state: &State, stage_w: f32, stage_h: f32) -> Element<'_, HubMessage> {
    if state.catalog_open {
        return catalog_view(state, stage_w, stage_h);
    }
    let mut tiles = row![].spacing(TILE_GAP).align_y(Alignment::Center);
    for (i, tile) in TILES.into_iter().enumerate() {
        tiles = tiles.push(tile_button(state, tile, i));
    }
    let card = container(
        column![
            row![
                text("Apps")
                    .size(theme::type_scale::META)
                    .color(theme::MUTED),
                space().width(Fill),
                text("PS  Close")
                    .size(theme::type_scale::CAPTION)
                    .color(theme::DIM),
            ]
            .align_y(Alignment::Center),
            space().height(10),
            tiles,
            space().height(12),
            row![
                hint("Cross", "Open"),
                hint("Circle", "Back"),
                hint("D-pad", "Select"),
            ]
            .spacing(18)
            .align_y(Alignment::Center),
        ]
        .spacing(0),
    )
    .padding(16)
    .width(Length::Fixed(HUB_WIDTH - 24.0))
    .style(card_style);
    frame(card.into(), stage_w, stage_h)
}

/// Wrap the hub card in its window root. A full-screen stage (alpha-capable
/// backend) gets the translucent-black scrim and keeps the card
/// bottom-weighted; the small opaque fallback centers the card in its window.
fn frame<'a>(card: Element<'a, HubMessage>, stage_w: f32, stage_h: f32) -> Element<'a, HubMessage> {
    if stage_w <= HUB_WIDTH + 1.0 {
        return container(card)
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill)
            .into();
    }
    let top = (stage_h * HUB_TOP_FRACTION).min((stage_h - HUB_HEIGHT - 24.0).max(0.0));
    let body = column![space().height(top), card]
        .width(Fill)
        .height(Fill)
        .align_x(Alignment::Center);
    container(body)
        .width(Fill)
        .height(Fill)
        .style(theme::scrim(theme::BACKDROP_ALPHA))
        .into()
}

fn catalog_view(state: &State, stage_w: f32, stage_h: f32) -> Element<'_, HubMessage> {
    let status = if state.installing {
        "Installing…".to_string()
    } else if state.engine_ready {
        "Installed".to_string()
    } else if let Some(err) = &state.install_error {
        err.clone()
    } else {
        "Not installed".to_string()
    };
    let status_color = if state.engine_ready {
        theme::SUCCESS
    } else if state.install_error.is_some() {
        theme::WARNING
    } else {
        theme::MUTED
    };
    let primary_label = if state.installing {
        "Installing…"
    } else if state.engine_ready {
        "Uninstall"
    } else {
        "Install"
    };
    let primary_focused = state.catalog_focus == 0;
    let mut action = button(text(primary_label).size(13.0).color(theme::INK))
        .padding([8, 18])
        .style(move |theme_, status| {
            if primary_focused {
                theme::primary(theme_, status)
            } else {
                theme::chip(false)(theme_, status)
            }
        });
    if !state.installing {
        action = action.on_press(if state.engine_ready {
            HubMessage::UninstallEngine
        } else {
            HubMessage::InstallEngine
        });
    }
    let back_focused = state.catalog_focus == 1;
    let back = button(text("Back").size(13.0).color(theme::INK))
        .padding([8, 14])
        .on_press(HubMessage::CatalogClose)
        .style(move |theme_, status| {
            if back_focused {
                theme::primary(theme_, status)
            } else {
                theme::chip(false)(theme_, status)
            }
        });
    let row = row![
        svg(svg::Handle::from_memory(svg_icon::GLOBE_SVG.as_bytes()))
            .width(Length::Fixed(24.0))
            .height(Length::Fixed(24.0))
            .style(|_theme, _status| svg::Style {
                color: Some(theme::INK)
            }),
        column![
            text("Browser engine").size(15.0).color(theme::INK),
            text(status).size(11.0).color(status_color),
        ]
        .spacing(2)
        .width(Fill),
        row![action, back].spacing(8),
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    let card = container(
        column![
            row![
                text("Add apps")
                    .size(theme::type_scale::META)
                    .color(theme::MUTED),
                space().width(Fill),
                text("Catalog")
                    .size(theme::type_scale::CAPTION)
                    .color(theme::DIM),
            ]
            .align_y(Alignment::Center),
            space().height(12),
            row,
            space().height(8),
            text("Optional · enables the Browser app. Installed from the Microsoft Store or a configured package feed.")
                .size(10.0)
                .color(theme::DIM),
            space().height(6),
            row![
                hint("Cross", "Select"),
                hint("D-pad", "Focus"),
                hint("Circle", "Back"),
            ]
            .spacing(18),
        ]
        .spacing(0),
    )
    .padding(16)
    .width(Length::Fixed(HUB_WIDTH - 24.0))
    .style(card_style);
    frame(card.into(), stage_w, stage_h)
}

fn tile_button<'a>(state: &State, tile: HubTile, index: usize) -> Element<'a, HubMessage> {
    let selected = state.selected() == index;
    let enabled = state.tile_enabled(tile);
    let (icon, title, subtitle) = tile_copy(state, tile);
    let ink = if enabled { theme::INK } else { theme::DIM };
    let content = column![
        svg(svg::Handle::from_memory(icon.as_bytes()))
            .width(Length::Fixed(ICON))
            .height(Length::Fixed(ICON))
            .style(move |_theme, _status| svg::Style { color: Some(ink) }),
        space().height(8),
        text(text_fit::ellipsize_to_width(title, 15.0, TILE_W - 28.0))
            .size(15.0)
            .color(ink),
        text(text_fit::ellipsize_to_width(subtitle, 11.0, TILE_W - 28.0))
            .size(11.0)
            .color(if enabled { theme::MUTED } else { theme::DIM }),
    ]
    .spacing(0)
    .align_x(Alignment::Center);
    let mut b = button(container(content).center_x(Fill).center_y(Fill))
        .width(Length::Fixed(TILE_W))
        .height(Length::Fixed(TILE_H))
        .padding(8)
        .style(move |_theme, status| tile_style(selected, enabled, status));
    if enabled {
        b = b.on_press(HubMessage::Activate(index));
    }
    b.into()
}

fn tile_copy(state: &State, tile: HubTile) -> (&'static str, &'static str, String) {
    match tile {
        HubTile::Start => (svg_icon::GAME_SVG, "Start", "Game launcher".to_string()),
        HubTile::Browser => (
            svg_icon::GLOBE_SVG,
            "Browser",
            if state.engine_ready {
                "Engine ready".to_string()
            } else {
                "Install engine".to_string()
            },
        ),
        HubTile::AddApps => (svg_icon::PLUS_SVG, "Add apps", "Catalog".to_string()),
    }
}

fn hint(label: &str, action: &str) -> Element<'static, HubMessage> {
    row![
        text(label.to_string())
            .size(theme::type_scale::CAPTION)
            .color(theme::INK),
        text(action.to_string())
            .size(theme::type_scale::CAPTION)
            .color(theme::DIM),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

fn card_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(theme::alpha(theme::BASE_BG, 0.94))),
        text_color: Some(theme::INK),
        border: Border {
            color: theme::GLASS_HAIRLINE,
            width: 1.0,
            radius: theme::radius::XL.into(),
        },
        shadow: Shadow {
            color: Color::BLACK,
            offset: Vector::new(0.0, 8.0),
            blur_radius: 24.0,
        },
        ..container::Style::default()
    }
}

fn tile_style(selected: bool, enabled: bool, status: button::Status) -> button::Style {
    let (fill, border, ink) = if !enabled {
        (
            theme::alpha(theme::INK, 0.02),
            theme::alpha(theme::LINE, 0.4),
            theme::DIM,
        )
    } else if selected {
        (theme::alpha(theme::ACCENT, 0.24), theme::ACCENT, theme::INK)
    } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        (
            theme::alpha(theme::INK, 0.09),
            theme::alpha(theme::INK, 0.22),
            theme::INK,
        )
    } else {
        (
            theme::alpha(theme::INK, 0.05),
            theme::GLASS_HAIRLINE,
            theme::INK,
        )
    };
    button::Style {
        background: Some(Background::Color(fill)),
        text_color: ink,
        border: Border {
            color: border,
            width: 1.0,
            radius: theme::radius::LG.into(),
        },
        shadow: if selected {
            Shadow {
                color: theme::alpha(theme::ACCENT, 0.35),
                offset: Vector::new(0.0, 0.0),
                blur_radius: 14.0,
            }
        } else {
            Shadow::default()
        },
        ..button::Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_wraps_both_ways() {
        let mut state = State::default();
        assert_eq!(state.selected_tile(), HubTile::Start);
        state.move_selection(1);
        assert_eq!(state.selected_tile(), HubTile::Browser);
        state.move_selection(1);
        assert_eq!(state.selected_tile(), HubTile::AddApps);
        state.move_selection(1);
        assert_eq!(state.selected_tile(), HubTile::Start);
        state.move_selection(-1);
        assert_eq!(state.selected_tile(), HubTile::AddApps);
    }

    #[test]
    fn select_ignores_out_of_range() {
        let mut state = State::default();
        state.select(1);
        assert_eq!(state.selected_tile(), HubTile::Browser);
        state.select(99);
        assert_eq!(state.selected_tile(), HubTile::Browser);
    }

    #[test]
    fn disabled_start_tile_cannot_be_activated() {
        let mut state = State::default();
        state.on_open(false, true);
        assert!(!state.tile_enabled(HubTile::Start));
        assert!(state.tile_enabled(HubTile::Browser));
        assert!(state.tile_enabled(HubTile::AddApps));
    }

    #[test]
    fn face_edges_fire_once_per_press() {
        let mut state = State::default();
        let mut sample = crate::domain::pad::PadSample::default();
        let t0 = Instant::now();
        sample.cross = true;
        assert_eq!(state.pad_tick(&sample, t0), Some(HubMessage::Confirm));
        assert_eq!(
            state.pad_tick(&sample, t0 + std::time::Duration::from_millis(16)),
            None
        );
        sample.cross = false;
        let _ = state.pad_tick(&sample, t0 + std::time::Duration::from_millis(32));
        sample.circle = true;
        assert_eq!(
            state.pad_tick(&sample, t0 + std::time::Duration::from_millis(48)),
            Some(HubMessage::Close)
        );
    }
}
