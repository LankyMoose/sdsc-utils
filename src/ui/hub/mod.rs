//! PS overlay hub: the topmost app switcher. Its window always covers the
//! monitor and dims the whole screen behind a centered card; the toast is
//! composited above that scrim.
//!
//! The hub is the auto-open target; Start and Browser are normal-level apps
//! underneath and mutually exclusive. It is modal for pad input while open:
//! the underlying app keeps its last frame but receives no nav.
//!
//! Tiles are stable identities, never row indices. Icon-first tiles carry
//! a large glyph plus a title (no subtitles); the Browser tile stays visible
//! when its engine is missing and opens the icon-only install catalog.

use crate::ui::svg_icon;
use crate::ui::text_fit;
use crate::ui::theme;
use iced::widget::{button, column, container, row, space, stack, svg, text};
use iced::{Alignment, Background, Border, Color, Element, Fill, Length, Shadow, Theme, Vector};
use std::time::Instant;

/// Nominal hub card width/height. The card is centered in the full-monitor
/// window; `HUB_HEIGHT` is only the pre-open stage default.
pub const HUB_WIDTH: f32 = 620.0;
pub const HUB_HEIGHT: f32 = 220.0;

const TILE_H: f32 = 124.0;
const TILE_GAP: f32 = 12.0;
const ICON: f32 = 48.0;

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
    let card_w = (stage_w - 48.0).clamp(360.0, HUB_WIDTH - 24.0);
    let inner_w = card_w - 32.0;
    let tile_w = ((inner_w - 2.0 * TILE_GAP) / 3.0).max(112.0);
    let mut tiles = row![].spacing(TILE_GAP).align_y(Alignment::Center);
    for (i, tile) in TILES.into_iter().enumerate() {
        tiles = tiles.push(tile_button(state, tile, i, tile_w));
    }
    let header = row![
        text("Apps")
            .size(theme::type_scale::META)
            .color(theme::MUTED),
        space().width(Fill),
        icon_button(
            svg_icon::CLOSE_SVG,
            18.0,
            40.0,
            theme::MUTED,
            false,
            Some(HubMessage::Close),
        ),
    ]
    .align_y(Alignment::Center);
    let card = container(
        column![header, space().height(10), tiles,]
            .spacing(0)
            .align_x(Alignment::Center),
    )
    .padding(16)
    .width(Length::Fixed(card_w))
    .style(card_style);
    frame(card.into(), stage_w, stage_h)
}

/// Full-screen dimmed backdrop with the card centered. The backdrop always
/// covers the window; only the card itself scales with the stage.
fn frame<'a>(
    card: Element<'a, HubMessage>,
    _stage_w: f32,
    _stage_h: f32,
) -> Element<'a, HubMessage> {
    container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .style(theme::scrim(theme::BACKDROP_ALPHA))
        .into()
}

fn catalog_view(state: &State, stage_w: f32, stage_h: f32) -> Element<'_, HubMessage> {
    let card_w = (stage_w - 48.0).clamp(360.0, 560.0);
    let back_focused = state.catalog_focus == 1;
    let back = icon_button(
        svg_icon::CHEVRON_LEFT_SVG,
        22.0,
        44.0,
        theme::INK,
        back_focused,
        Some(HubMessage::CatalogClose),
    );
    let primary_focused = state.catalog_focus == 0;
    let (primary_icon, primary_color, primary_message) = if state.installing {
        (svg_icon::SPINNER_SVG, theme::MUTED, None)
    } else if state.engine_ready {
        (
            svg_icon::TRASH_SVG,
            theme::INK,
            Some(HubMessage::UninstallEngine),
        )
    } else {
        (
            svg_icon::DOWNLOAD_SVG,
            if state.install_error.is_some() {
                theme::WARNING
            } else {
                theme::INK
            },
            Some(HubMessage::InstallEngine),
        )
    };
    let action = icon_button(
        primary_icon,
        28.0,
        52.0,
        primary_color,
        primary_focused,
        primary_message,
    );
    let mut title = row![
        svg(svg::Handle::from_memory(svg_icon::GLOBE_SVG.as_bytes()))
            .width(Length::Fixed(34.0))
            .height(Length::Fixed(34.0))
            .style(|_theme, _status| svg::Style {
                color: Some(theme::INK)
            }),
        text("Browser engine")
            .size(theme::type_scale::HEADING)
            .color(theme::INK),
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    let status_icon = if state.installing {
        Some((svg_icon::SPINNER_SVG, theme::MUTED))
    } else if state.engine_ready {
        Some((svg_icon::CHECK_SVG, theme::SUCCESS))
    } else {
        None
    };
    if let Some((icon, color)) = status_icon {
        title = title.push(
            svg(svg::Handle::from_memory(icon.as_bytes()))
                .width(Length::Fixed(18.0))
                .height(Length::Fixed(18.0))
                .style(move |_theme, _status| svg::Style { color: Some(color) }),
        );
    }
    let card = container(
        row![back, title, space().width(Fill), action]
            .spacing(16)
            .align_y(Alignment::Center),
    )
    .padding(20)
    .width(Length::Fixed(card_w))
    .style(card_style);
    frame(card.into(), stage_w, stage_h)
}

/// Compact icon-only button for close/back/install actions.
fn icon_button<'a>(
    icon: &'static str,
    icon_size: f32,
    box_size: f32,
    color: Color,
    focused: bool,
    on_press: Option<HubMessage>,
) -> Element<'a, HubMessage> {
    let glyph = svg(svg::Handle::from_memory(icon.as_bytes()))
        .width(Length::Fixed(icon_size))
        .height(Length::Fixed(icon_size))
        .style(move |_theme, _status| svg::Style { color: Some(color) });
    let mut b = button(glyph)
        .width(Length::Fixed(box_size))
        .height(Length::Fixed(box_size))
        .padding(0)
        .style(move |_theme, status| icon_button_style(focused, status));
    if let Some(message) = on_press {
        b = b.on_press(message);
    }
    b.into()
}

fn tile_button<'a>(
    state: &State,
    tile: HubTile,
    index: usize,
    width: f32,
) -> Element<'a, HubMessage> {
    let selected = state.selected() == index;
    let enabled = state.tile_enabled(tile);
    let (icon, title) = tile_copy(tile);
    let ink = if enabled { theme::INK } else { theme::DIM };
    let title_font = if selected {
        iced::Font {
            weight: iced::font::Weight::Semibold,
            ..iced::Font::default()
        }
    } else {
        iced::Font::default()
    };
    let content: Element<'a, HubMessage> = column![
        svg(svg::Handle::from_memory(icon.as_bytes()))
            .width(Length::Fixed(ICON))
            .height(Length::Fixed(ICON))
            .style(move |_theme, _status| svg::Style { color: Some(ink) }),
        text(text_fit::ellipsize_to_width(title, 18.0, width - 28.0))
            .size(18.0)
            .font(title_font)
            .color(ink),
    ]
    .spacing(12)
    .align_x(Alignment::Center)
    .into();
    // Missing engine: a small download badge, no explanatory subtitle.
    let content = if matches!(tile, HubTile::Browser) && !state.engine_ready {
        stack![
            content,
            container(
                svg(svg::Handle::from_memory(svg_icon::DOWNLOAD_SVG.as_bytes()))
                    .width(Length::Fixed(15.0))
                    .height(Length::Fixed(15.0))
                    .style(|_theme, _status| svg::Style {
                        color: Some(theme::WARNING)
                    }),
            )
            .width(Fill)
            .height(Fill)
            .align_x(Alignment::End)
            .align_y(Alignment::Start),
        ]
        .into()
    } else {
        content
    };
    let mut b = button(container(content).center_x(Fill).center_y(Fill))
        .width(Length::Fixed(width))
        .height(Length::Fixed(TILE_H))
        .padding(10)
        .style(move |_theme, status| tile_style(selected, enabled, status));
    if enabled {
        b = b.on_press(HubMessage::Activate(index));
    }
    b.into()
}

fn tile_copy(tile: HubTile) -> (&'static str, &'static str) {
    match tile {
        HubTile::Start => (svg_icon::GAME_SVG, "Start"),
        HubTile::Browser => (svg_icon::GLOBE_SVG, "Browser"),
        HubTile::AddApps => (svg_icon::PLUS_SVG, "Add apps"),
    }
}

fn icon_button_style(focused: bool, status: button::Status) -> button::Style {
    let (fill, border) = if focused {
        (
            theme::alpha(theme::INK, 0.12),
            theme::alpha(theme::INK, 0.38),
        )
    } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        (
            theme::alpha(theme::INK, 0.09),
            theme::alpha(theme::INK, 0.22),
        )
    } else {
        (theme::alpha(theme::INK, 0.04), theme::GLASS_HAIRLINE)
    };
    button::Style {
        background: Some(Background::Color(fill)),
        text_color: theme::INK,
        border: Border {
            color: border,
            width: 1.0,
            radius: theme::radius::PILL.into(),
        },
        shadow: if focused {
            Shadow {
                color: theme::alpha(theme::INK, 0.12),
                offset: Vector::new(0.0, 0.0),
                blur_radius: 16.0,
            }
        } else {
            Shadow::default()
        },
        ..button::Style::default()
    }
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
        (
            theme::alpha(theme::INK, 0.10),
            theme::alpha(theme::INK, 0.34),
            theme::INK,
        )
    } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        (
            theme::alpha(theme::INK, 0.08),
            theme::alpha(theme::INK, 0.22),
            theme::INK,
        )
    } else {
        (
            theme::alpha(theme::INK, 0.04),
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
                color: theme::alpha(theme::INK, 0.10),
                offset: Vector::new(0.0, 0.0),
                blur_radius: 18.0,
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
