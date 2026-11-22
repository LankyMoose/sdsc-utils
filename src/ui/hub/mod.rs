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
use iced::widget::text::Wrapping;
use iced::widget::{
    button, column, container, progress_bar, row, scrollable, space, stack, svg, text,
};
use iced::{Alignment, Background, Border, Color, Element, Fill, Length, Shadow, Theme, Vector};
use std::time::Instant;

/// Nominal hub card width/height. The card is centered in the full-monitor
/// window; `HUB_HEIGHT` is only the pre-open stage default.
pub const HUB_WIDTH: f32 = 620.0;
pub const HUB_HEIGHT: f32 = 220.0;

const TILE_H: f32 = 112.0;
const TILE_GAP: f32 = 8.0;
const ICON: f32 = 44.0;
const APP_TILE_H: f32 = 128.0;
const APP_ICON: f32 = 48.0;

/// Stable tile identity. The order in [`TILES`] is the render/selection
/// order; message handlers match on identity, never on an index captured in
/// the UI (the catalog may grow/reorder later).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubTile {
    Start,
    Browser,
    AddApps,
    Settings,
}

/// Visible tiles, left to right.
pub const TILES: [HubTile; 4] = [
    HubTile::Start,
    HubTile::Browser,
    HubTile::AddApps,
    HubTile::Settings,
];

/// What the hub asks the shell to do. Pure data; the App owns windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubMessage {
    Move(i32),
    /// Open the apps management screen.
    AppsOpen,
    /// Back out of the apps screen.
    AppsClose,
    /// Open the hub shell settings screen.
    SettingsOpen,
    SettingsClose,
    /// Move the settings highlight.
    SettingsMove(i32),
    /// Toggle/cycle the focused settings row.
    SettingsConfirm,
    /// Mouse: focus and adjust a settings row.
    SettingsRow(usize),
    /// Mouse click: select and activate a hub tile in one step.
    Activate(usize),
    /// Open the per-app action menu (tile X / Cross on an app).
    AppsMenuOpen(usize),
    AppsMenuMove(i32),
    AppsMenuConfirm,
    AppsMenuAction(AppAction),
    AppsMenuClose,
    AppInfoClose,
    Confirm,
    Close,
}

/// One entry on the apps management screen. Start is a built-in app and has
/// no install/update state, so it deliberately does not appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubApp {
    Browser,
}

pub const APPS: [HubApp; 1] = [HubApp::Browser];

impl HubApp {
    pub fn title(self) -> &'static str {
        match self {
            Self::Browser => "Browser",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::Browser => svg_icon::GLOBE_SVG,
        }
    }
}

/// Install/uninstall phases shown by the hub. Mirrors the installer's
/// progress events without coupling `ui` to the CEF-only module (UI-only
/// builds compile this).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EnginePhase {
    #[default]
    Preparing,
    Downloading,
    Verifying,
    Installing,
    Finishing,
    Uninstalling,
    Cancelling,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EngineProgress {
    pub phase: EnginePhase,
    pub fraction: Option<f32>,
    /// Live throughput while a byte-moving phase is active.
    pub rate_bps: Option<f64>,
}

impl EngineProgress {
    pub fn label(&self) -> &'static str {
        match self.phase {
            EnginePhase::Preparing => "Preparing",
            EnginePhase::Downloading => "Downloading",
            EnginePhase::Verifying => "Verifying",
            EnginePhase::Installing => "Installing",
            EnginePhase::Finishing => "Finishing",
            EnginePhase::Uninstalling => "Uninstalling",
            EnginePhase::Cancelling => "Cancelling",
        }
    }

    pub fn bar_value(&self) -> f32 {
        if matches!(self.phase, EnginePhase::Finishing | EnginePhase::Cancelling) {
            1.0
        } else {
            self.fraction.unwrap_or(0.0).clamp(0.0, 1.0)
        }
    }

    /// Whole-number throughput label (`8 MB/s`). Hidden when no bytes have
    /// moved yet or the phase does not move bytes.
    pub fn rate_label(&self) -> Option<String> {
        let rate = self.rate_bps.filter(|rate| *rate >= 1024.0)?;
        Some(format!("{:.0} MB/s", rate / (1024.0 * 1024.0)))
    }
}

/// A row in the per-app action popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppAction {
    Install,
    Update,
    Cancel,
    Uninstall,
    Info,
}

impl AppAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Install => "Install",
            Self::Update => "Update",
            Self::Cancel => "Cancel",
            Self::Uninstall => "Uninstall",
            Self::Info => "Info",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::Install | Self::Update => svg_icon::DOWNLOAD_SVG,
            Self::Cancel => svg_icon::CLOSE_SVG,
            Self::Uninstall => svg_icon::TRASH_SVG,
            Self::Info => svg_icon::INFO_SVG,
        }
    }
}

/// Rows in the hub shell settings screen.
pub const SETTINGS_ROWS: [&str; 6] = [
    "UI sounds",
    "Volume",
    "Controller haptics",
    "Strength",
    "Download speed",
    "Back",
];

/// Hub render/input state. Selection is an index into [`TILES`]; activation
/// resolves it through [`State::selected_tile`].
#[derive(Debug)]
pub struct State {
    selected: usize,
    /// Whether the Start tile is enabled in prefs. Disabled tiles still show
    /// (stable layout) but Confirm is a no-op.
    pub start_enabled: bool,
    /// Engine worker live. Drives the Browser tile state.
    pub engine_ready: bool,
    /// D-pad/stick repeat for hub movement.
    nav: crate::domain::pad::NavStepper,
    /// Face-button rising-edge latches (Cross confirm, Circle close).
    pub prev_cross: bool,
    pub prev_circle: bool,
    /// Apps management screen replaces the tile row while open.
    pub apps_open: bool,
    /// App entry under the focus ring.
    pub apps_focus: usize,
    /// Per-app action popup: app index + focused action.
    pub apps_menu: Option<usize>,
    pub apps_menu_focus: usize,
    /// Info popup: app index.
    pub apps_info: Option<usize>,
    /// Engine install/download/update in flight.
    pub installing: bool,
    /// Cancellation requested; progress stays frozen until the worker exits.
    pub cancelling: bool,
    /// Engine uninstall in flight.
    pub uninstalling: bool,
    /// Live phase/fraction for the in-tile progress.
    pub progress: EngineProgress,
    /// Bouncing fraction for the indeterminate uninstall bar.
    pub progress_anim: f32,
    pub progress_anim_dir: f32,
    /// Last install/uninstall failure (user-facing).
    pub install_error: Option<String>,
    /// Info: installed engine version, latest feed version, changelog, size.
    pub engine_version: Option<String>,
    pub engine_latest_version: Option<String>,
    pub engine_changelog: Option<String>,
    pub engine_size: Option<u64>,
    pub engine_pending_size: u64,
    pub engine_update_ready: bool,
    pub engine_managed: bool,
    /// An update is on disk but the old CEF remains loaded until restart.
    pub engine_restart_required: bool,
    /// Hub shell settings screen.
    pub settings_open: bool,
    pub settings_focus: usize,
    pub sound_enabled: bool,
    pub sound_volume: u8,
    pub haptics_enabled: bool,
    pub haptics_strength: u8,
    pub download_limit_kbps: u32,
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
            apps_open: false,
            apps_focus: 0,
            apps_menu: None,
            apps_menu_focus: 0,
            apps_info: None,
            installing: false,
            cancelling: false,
            uninstalling: false,
            progress: EngineProgress::default(),
            progress_anim: 0.0,
            progress_anim_dir: 1.0,
            install_error: None,
            engine_version: None,
            engine_latest_version: None,
            engine_changelog: None,
            engine_size: None,
            engine_pending_size: 0,
            engine_update_ready: false,
            engine_managed: false,
            engine_restart_required: false,
            settings_open: false,
            settings_focus: 0,
            sound_enabled: true,
            sound_volume: 60,
            haptics_enabled: true,
            haptics_strength: 60,
            download_limit_kbps: 0,
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
            HubTile::Browser | HubTile::AddApps | HubTile::Settings => true,
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
        self.apps_open = false;
        self.apps_menu = None;
        self.apps_info = None;
        self.settings_open = false;
        self.settings_focus = 0;
        self.install_error = None;
    }

    pub fn open_apps(&mut self) {
        self.apps_open = true;
        self.apps_focus = 0;
        self.apps_menu = None;
        self.apps_info = None;
        self.settings_open = false;
        self.install_error = None;
    }

    pub fn close_apps(&mut self) {
        self.apps_open = false;
        self.apps_menu = None;
        self.apps_info = None;
        self.install_error = None;
    }

    pub fn open_settings(&mut self) {
        self.settings_open = true;
        self.settings_focus = 0;
        self.apps_open = false;
        self.apps_menu = None;
        self.apps_info = None;
        self.install_error = None;
    }

    pub fn close_settings(&mut self) {
        self.settings_open = false;
        self.settings_focus = 0;
    }

    pub fn settings_move(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        let len = SETTINGS_ROWS.len() as i32;
        self.settings_focus = (self.settings_focus as i32 + delta).rem_euclid(len) as usize;
    }

    pub fn settings_value(&self, row: usize) -> String {
        match row {
            0 => (if self.sound_enabled { "On" } else { "Off" }).to_string(),
            1 => format!("{}%", self.sound_volume),
            2 => (if self.haptics_enabled { "On" } else { "Off" }).to_string(),
            3 => format!("{}%", self.haptics_strength),
            4 => crate::persist::prefs::engine_download_limit_label(self.download_limit_kbps),
            _ => String::new(),
        }
    }

    pub fn set_settings(
        &mut self,
        sounds: bool,
        sound_volume: u8,
        haptics: bool,
        haptics_strength: u8,
        download_limit_kbps: u32,
    ) {
        self.sound_enabled = sounds;
        self.sound_volume = sound_volume;
        self.haptics_enabled = haptics;
        self.haptics_strength = haptics_strength;
        self.download_limit_kbps =
            crate::persist::prefs::clamp_engine_download_limit(download_limit_kbps);
    }

    pub fn apps_focus_app(&self) -> HubApp {
        APPS[self.apps_focus.min(APPS.len() - 1)]
    }

    pub fn apps_move(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        let len = APPS.len() as i32;
        self.apps_focus = (self.apps_focus as i32 + delta).rem_euclid(len) as usize;
    }

    pub fn select_app(&mut self, index: usize) {
        if index < APPS.len() {
            self.apps_focus = index;
        }
    }

    pub fn open_apps_menu(&mut self, index: usize) {
        if index >= APPS.len() {
            return;
        }
        self.apps_focus = index;
        self.apps_menu = Some(index);
        self.apps_menu_focus = 0;
        self.apps_info = None;
    }

    pub fn close_apps_menu(&mut self) {
        self.apps_menu = None;
    }

    pub fn app_actions(&self, index: usize) -> Vec<AppAction> {
        match APPS.get(index).copied() {
            Some(HubApp::Browser) => {
                if self.installing {
                    if self.cancelling {
                        vec![AppAction::Info]
                    } else {
                        vec![AppAction::Cancel, AppAction::Info]
                    }
                } else if self.uninstalling || self.engine_managed {
                    vec![AppAction::Info]
                } else if self.engine_ready {
                    if self.engine_update_ready {
                        vec![AppAction::Update, AppAction::Uninstall, AppAction::Info]
                    } else {
                        vec![AppAction::Uninstall, AppAction::Info]
                    }
                } else {
                    vec![AppAction::Install, AppAction::Info]
                }
            }
            None => Vec::new(),
        }
    }

    pub fn apps_menu_actions(&self) -> Vec<AppAction> {
        self.apps_menu
            .map(|i| self.app_actions(i))
            .unwrap_or_default()
    }

    pub fn apps_menu_action(&self) -> Option<AppAction> {
        self.apps_menu_actions().get(self.apps_menu_focus).copied()
    }

    pub fn apps_menu_move(&mut self, delta: i32) {
        let len = self.apps_menu_actions().len() as i32;
        if delta == 0 || len == 0 {
            return;
        }
        self.apps_menu_focus = (self.apps_menu_focus as i32 + delta).rem_euclid(len) as usize;
    }

    pub fn show_app_info(&mut self, index: usize) {
        if index >= APPS.len() {
            return;
        }
        self.apps_focus = index;
        self.apps_info = Some(index);
        self.apps_menu = None;
    }

    pub fn close_app_info(&mut self) {
        self.apps_info = None;
    }

    pub fn app_dimmed(&self, app: HubApp) -> bool {
        match app {
            HubApp::Browser => !self.engine_ready,
        }
    }

    pub fn app_status(&self, app: HubApp) -> String {
        match app {
            HubApp::Browser if self.installing || self.uninstalling => {
                self.progress.label().to_string()
            }
            HubApp::Browser if self.engine_managed => "Managed by Store".to_string(),
            HubApp::Browser if self.engine_restart_required => "Restart to finish".to_string(),
            HubApp::Browser if self.engine_update_ready => "Update ready".to_string(),
            HubApp::Browser if self.engine_ready => "Installed".to_string(),
            HubApp::Browser => "Not installed".to_string(),
        }
    }

    pub fn busy(&self) -> bool {
        self.installing || self.uninstalling
    }

    pub fn set_installing(&mut self, installing: bool) {
        self.installing = installing;
        if installing {
            self.cancelling = false;
            self.uninstalling = false;
            self.progress = EngineProgress::default();
            self.install_error = None;
        }
    }

    pub fn set_cancelling(&mut self) {
        if !self.installing {
            return;
        }
        self.cancelling = true;
        self.progress = EngineProgress {
            phase: EnginePhase::Cancelling,
            fraction: None,
            rate_bps: None,
        };
    }

    pub fn set_install_progress(&mut self, progress: EngineProgress) {
        if self.cancelling {
            return;
        }
        self.installing = true;
        self.uninstalling = false;
        self.progress = progress;
        self.install_error = None;
    }

    pub fn install_done(&mut self, ready: bool, error: Option<String>) {
        self.installing = false;
        self.cancelling = false;
        self.uninstalling = false;
        self.progress = EngineProgress::default();
        self.engine_ready = ready;
        self.install_error = error;
    }

    pub fn install_cancelled(&mut self, ready: bool) {
        self.installing = false;
        self.cancelling = false;
        self.uninstalling = false;
        self.progress = EngineProgress::default();
        self.engine_ready = ready;
        self.install_error = None;
    }

    pub fn set_uninstalling(&mut self) {
        self.installing = false;
        self.cancelling = false;
        self.uninstalling = true;
        self.progress = EngineProgress {
            phase: EnginePhase::Uninstalling,
            fraction: None,
            rate_bps: None,
        };
        self.progress_anim = 0.0;
        self.progress_anim_dir = 1.0;
        self.install_error = None;
    }

    pub fn uninstall_done(&mut self, error: Option<String>) {
        self.installing = false;
        self.cancelling = false;
        self.uninstalling = false;
        self.progress = EngineProgress::default();
        self.engine_ready = false;
        self.install_error = error;
    }

    pub fn advance_progress_anim(&mut self) {
        self.progress_anim += 0.05 * self.progress_anim_dir;
        if self.progress_anim >= 1.0 {
            self.progress_anim = 1.0;
            self.progress_anim_dir = -1.0;
        } else if self.progress_anim <= 0.0 {
            self.progress_anim = 0.0;
            self.progress_anim_dir = 1.0;
        }
    }

    pub fn progress_bar_value(&self) -> f32 {
        if self.uninstalling {
            self.progress_anim
        } else {
            self.progress.bar_value()
        }
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
pub fn view(
    state: &State,
    stage_w: f32,
    _stage_h: f32,
    backdrop_alpha: f32,
) -> Element<'_, HubMessage> {
    if state.settings_open {
        return settings_view(state, stage_w, backdrop_alpha);
    }
    if state.apps_open {
        return apps_view(state, stage_w, _stage_h, backdrop_alpha);
    }
    let card_w = (stage_w - 32.0).clamp(340.0, HUB_WIDTH - 24.0);
    let inner_w = card_w - 24.0;
    let gaps = (TILES.len().saturating_sub(1)) as f32 * TILE_GAP;
    let tile_w = ((inner_w - gaps) / TILES.len() as f32).max(96.0);
    let mut tiles = row![].spacing(TILE_GAP).align_y(Alignment::Center);
    for (i, tile) in TILES.into_iter().enumerate() {
        tiles = tiles.push(tile_button(state, tile, i, tile_w));
    }
    let card = container(tiles)
        .padding(12)
        .width(Length::Fixed(card_w))
        .style(card_style);
    frame(card.into(), backdrop_alpha)
}

/// Full-screen dimmed backdrop with the card centered. The backdrop always
/// covers the window; only the card itself scales with the stage. The caller
/// fades `backdrop_alpha` in after the card has appeared.
fn frame<'a>(card: Element<'a, HubMessage>, backdrop_alpha: f32) -> Element<'a, HubMessage> {
    container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .style(theme::scrim(backdrop_alpha))
        .into()
}

/// Stable id for the controller/mouse-scrollable changelog in Info.
pub fn info_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("hub-info-changelog")
}

fn settings_view<'a>(state: &State, stage_w: f32, backdrop_alpha: f32) -> Element<'a, HubMessage> {
    let card_w = (stage_w - 48.0).clamp(340.0, 460.0);
    let mut rows = column![].spacing(4);
    for (i, label) in SETTINGS_ROWS.iter().enumerate() {
        let focused = i == state.settings_focus;
        let value = state.settings_value(i);
        let mut content = row![
            text(*label).size(14.0).color(theme::INK),
            space().width(Fill),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        if !value.is_empty() {
            content = content.push(
                text(value)
                    .size(theme::type_scale::CAPTION)
                    .color(theme::MUTED),
            );
        }
        rows = rows.push(
            button(content)
                .width(Fill)
                .padding([10, 12])
                .on_press(HubMessage::SettingsRow(i))
                .style(move |_theme, status| menu_row_style(focused, status)),
        );
    }
    let card = container(
        column![
            row![
                svg(svg::Handle::from_memory(svg_icon::SETTINGS_SVG.as_bytes()))
                    .width(Length::Fixed(22.0))
                    .height(Length::Fixed(22.0))
                    .style(|_theme, _status| svg::Style {
                        color: Some(theme::INK)
                    }),
                text("Settings")
                    .size(theme::type_scale::HEADING)
                    .color(theme::INK),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
            space().height(4),
            text("Applies across the hub, Start, and Browser")
                .size(theme::type_scale::CAPTION)
                .color(theme::DIM),
            space().height(10),
            rows,
            space().height(8),
            text("Cross adjust · Circle back")
                .size(theme::type_scale::CAPTION)
                .color(theme::DIM),
        ]
        .spacing(0),
    )
    .padding(16)
    .width(Length::Fixed(card_w))
    .style(card_style);
    frame(card.into(), backdrop_alpha)
}

fn apps_view(
    state: &State,
    stage_w: f32,
    _stage_h: f32,
    backdrop_alpha: f32,
) -> Element<'_, HubMessage> {
    let card_w = (stage_w - 48.0).clamp(360.0, 480.0);
    let inner_w = card_w - 24.0;
    let app_w = ((inner_w - 8.0) / 2.0).max(150.0);
    let back = icon_button(
        svg_icon::CHEVRON_LEFT_SVG,
        22.0,
        40.0,
        theme::INK,
        false,
        Some(HubMessage::AppsClose),
    );
    let mut grid = row![].spacing(8).align_y(Alignment::Center);
    for (i, app) in APPS.into_iter().enumerate() {
        grid = grid.push(app_tile(state, app, i, app_w));
    }
    let mut content = column![row![back, space().width(Fill)], space().height(8), grid]
        .spacing(0)
        .align_x(Alignment::Center);
    if let Some(error) = &state.install_error {
        content = content.push(space().height(8)).push(
            text(error.as_str())
                .size(theme::type_scale::CAPTION)
                .color(theme::WARNING)
                .width(Fill)
                .wrapping(Wrapping::Word),
        );
    }
    let card = container(content)
        .padding(12)
        .width(Length::Fixed(card_w))
        .style(card_style);
    let base: Element<'_, HubMessage> = frame(card.into(), backdrop_alpha);
    if let Some(index) = state.apps_info {
        return stack![base, app_info(state, index)]
            .width(Fill)
            .height(Fill)
            .into();
    }
    if let Some(index) = state.apps_menu {
        return stack![base, apps_menu(state, index)]
            .width(Fill)
            .height(Fill)
            .into();
    }
    base
}

fn progress_style(_theme: &Theme) -> progress_bar::Style {
    progress_bar::Style {
        background: Background::Color(theme::alpha(theme::INK, 0.10)),
        bar: Background::Color(theme::ACCENT),
        border: Border {
            radius: theme::radius::PILL.into(),
            ..Border::default()
        },
    }
}

/// Busy tile body: icon, title, one-word phase, and a bar inside the card.
fn progress_tile_body<'a>(
    state: &State,
    icon: &'static str,
    title: &'static str,
    width: f32,
    icon_size: f32,
    title_size: f32,
    focused: bool,
) -> Element<'a, HubMessage> {
    let ink = if state.engine_ready {
        theme::INK
    } else {
        theme::DIM
    };
    let font = if focused {
        iced::Font {
            weight: iced::font::Weight::Semibold,
            ..iced::Font::default()
        }
    } else {
        iced::Font::default()
    };
    let mut phase = row![
        text(state.progress.label())
            .size(theme::type_scale::CAPTION)
            .color(theme::MUTED),
    ]
    .spacing(4)
    .align_y(Alignment::Center);
    if let Some(rate) = state.progress.rate_label() {
        phase = phase.push(
            text(rate)
                .size(theme::type_scale::CAPTION)
                .color(theme::DIM),
        );
    }
    column![
        svg(svg::Handle::from_memory(icon.as_bytes()))
            .width(Length::Fixed(icon_size))
            .height(Length::Fixed(icon_size))
            .style(move |_theme, _status| svg::Style { color: Some(ink) }),
        text(text_fit::ellipsize_to_width(
            title,
            title_size,
            width - 24.0
        ))
        .size(title_size)
        .font(font)
        .color(ink),
        phase,
        progress_bar(0.0..=1.0, state.progress_bar_value())
            .length(Length::Fixed((width - 40.0).max(80.0)))
            .girth(3.0)
            .style(progress_style),
    ]
    .spacing(4)
    .align_x(Alignment::Center)
    .into()
}

fn tile_button<'a>(
    state: &State,
    tile: HubTile,
    index: usize,
    width: f32,
) -> Element<'a, HubMessage> {
    let selected = state.selected() == index;
    let interactive = state.tile_enabled(tile);
    let dim = match tile {
        HubTile::Start => !state.start_enabled,
        HubTile::Browser => !state.engine_ready,
        HubTile::AddApps | HubTile::Settings => false,
    };
    let (icon, title) = tile_copy(tile);
    let ink = if dim { theme::DIM } else { theme::INK };
    let title_font = if selected {
        iced::Font {
            weight: iced::font::Weight::Semibold,
            ..iced::Font::default()
        }
    } else {
        iced::Font::default()
    };
    let normal: Element<'a, HubMessage> = column![
        svg(svg::Handle::from_memory(icon.as_bytes()))
            .width(Length::Fixed(ICON))
            .height(Length::Fixed(ICON))
            .style(move |_theme, _status| svg::Style { color: Some(ink) }),
        text(text_fit::ellipsize_to_width(title, 18.0, width - 24.0))
            .size(18.0)
            .font(title_font)
            .color(ink),
    ]
    .spacing(10)
    .align_x(Alignment::Center)
    .into();
    let content = if matches!(tile, HubTile::Browser) && state.busy() {
        progress_tile_body(state, icon, title, width, 36.0, 17.0, selected)
    } else if matches!(tile, HubTile::Browser) && !state.engine_ready {
        stack![
            normal,
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
    } else if matches!(tile, HubTile::Browser) && state.engine_restart_required {
        stack![
            normal,
            container(
                text("Restart")
                    .size(theme::type_scale::CAPTION)
                    .color(theme::WARNING),
            )
            .width(Fill)
            .height(Fill)
            .align_x(Alignment::End)
            .align_y(Alignment::End),
        ]
        .into()
    } else {
        normal
    };
    let mut b = button(container(content).center_x(Fill).center_y(Fill))
        .width(Length::Fixed(width))
        .height(Length::Fixed(TILE_H))
        .padding(8)
        .style(move |_theme, status| tile_style(selected, interactive, dim, status));
    if interactive {
        b = b.on_press(HubMessage::Activate(index));
    }
    b.into()
}

fn app_tile<'a>(state: &State, app: HubApp, index: usize, width: f32) -> Element<'a, HubMessage> {
    let selected = state.apps_focus.min(APPS.len() - 1) == index;
    let dim = state.app_dimmed(app);
    let ink = if dim { theme::DIM } else { theme::INK };
    let title_font = if selected {
        iced::Font {
            weight: iced::font::Weight::Semibold,
            ..iced::Font::default()
        }
    } else {
        iced::Font::default()
    };
    let normal: Element<'a, HubMessage> = column![
        svg(svg::Handle::from_memory(app.icon().as_bytes()))
            .width(Length::Fixed(APP_ICON))
            .height(Length::Fixed(APP_ICON))
            .style(move |_theme, _status| svg::Style { color: Some(ink) }),
        text(text_fit::ellipsize_to_width(
            app.title(),
            18.0,
            width - 28.0
        ))
        .size(18.0)
        .font(title_font)
        .color(ink),
    ]
    .spacing(10)
    .align_x(Alignment::Center)
    .into();
    let body: Element<'a, HubMessage> = if state.busy() {
        progress_tile_body(state, app.icon(), app.title(), width, 38.0, 17.0, selected)
    } else if state.engine_ready && (state.engine_update_ready || state.engine_restart_required) {
        let label = if state.engine_restart_required {
            "Restart"
        } else {
            "Update"
        };
        stack![
            normal,
            container(
                text(label)
                    .size(theme::type_scale::CAPTION)
                    .color(theme::WARNING),
            )
            .width(Fill)
            .height(Fill)
            .align_x(Alignment::End)
            .align_y(Alignment::End),
        ]
        .into()
    } else {
        normal
    };
    // The whole tile opens the per-app actions popup; there is no nested
    // button or redundant corner affordance.
    button(container(body).center_x(Fill).center_y(Fill))
        .width(Length::Fixed(width))
        .height(Length::Fixed(APP_TILE_H))
        .padding(10)
        .on_press(HubMessage::AppsMenuOpen(index))
        .style(move |_theme, status| app_tile_style(selected, dim, status))
        .into()
}

fn apps_menu<'a>(state: &State, index: usize) -> Element<'a, HubMessage> {
    let app = APPS[index];
    let mut rows = column![].spacing(4);
    for (i, action) in state.app_actions(index).into_iter().enumerate() {
        let focused = i == state.apps_menu_focus;
        rows = rows.push(
            button(
                row![
                    svg(svg::Handle::from_memory(action.icon().as_bytes()))
                        .width(Length::Fixed(18.0))
                        .height(Length::Fixed(18.0))
                        .style(|_theme, _status| svg::Style {
                            color: Some(theme::INK)
                        }),
                    text(action.label()).size(14.0).color(theme::INK),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            )
            .width(Fill)
            .padding([8, 10])
            .on_press(HubMessage::AppsMenuAction(action))
            .style(move |_theme, status| menu_row_style(focused, status)),
        );
    }
    let card = container(
        column![
            row![
                svg(svg::Handle::from_memory(app.icon().as_bytes()))
                    .width(Length::Fixed(22.0))
                    .height(Length::Fixed(22.0))
                    .style(|_theme, _status| svg::Style {
                        color: Some(theme::INK)
                    }),
                text(app.title()).size(16.0).color(theme::INK),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
            space().height(6),
            rows,
        ]
        .spacing(0),
    )
    .padding(10)
    .width(Length::Fixed(230.0))
    .style(card_style);
    container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .into()
}

fn app_info<'a>(state: &'a State, index: usize) -> Element<'a, HubMessage> {
    let app = APPS[index];
    let dim = state.app_dimmed(app);
    let ink = if dim { theme::DIM } else { theme::INK };
    let mut details = column![].spacing(6);

    if let Some(version) = &state.engine_version {
        details = details.push(
            text(format!("Browser engine {version}"))
                .size(theme::type_scale::CAPTION)
                .color(theme::MUTED),
        );
    }
    if state.engine_managed {
        details = details.push(
            text("Managed by the Microsoft Store")
                .size(theme::type_scale::CAPTION)
                .color(theme::MUTED),
        );
    }

    if state.engine_update_ready
        && let Some(latest) = &state.engine_latest_version
    {
        details = details.push(
            text(format!("Update available: {latest}"))
                .size(theme::type_scale::CAPTION)
                .color(theme::WARNING),
        );
    } else if state.engine_version.is_none()
        && let Some(latest) = &state.engine_latest_version
    {
        details = details.push(
            text(format!("Latest: {latest}"))
                .size(theme::type_scale::CAPTION)
                .color(theme::MUTED),
        );
    }

    if let Some(size) = state.engine_size.filter(|size| *size > 0) {
        details = details.push(
            text(format!("Size on disk: {}", format_size(size)))
                .size(theme::type_scale::CAPTION)
                .color(theme::MUTED),
        );
    }
    if state.engine_pending_size > 0 {
        details = details.push(
            text(format!(
                "Pending cleanup: {}",
                format_size(state.engine_pending_size)
            ))
            .size(theme::type_scale::CAPTION)
            .color(theme::DIM),
        );
    }
    if state.engine_restart_required {
        details = details.push(
            text("Restart SDSC Utils to finish the update.")
                .size(theme::type_scale::CAPTION)
                .color(theme::WARNING),
        );
    }
    if let Some(error) = &state.install_error {
        details = details.push(
            text(error.as_str())
                .size(theme::type_scale::CAPTION)
                .color(theme::WARNING)
                .width(Fill)
                .wrapping(Wrapping::Word),
        );
    }

    if let Some(changelog) = state
        .engine_changelog
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        details = details
            .push(space().height(6))
            .push(
                text("What's new")
                    .size(theme::type_scale::META)
                    .color(theme::INK),
            )
            .push(
                scrollable(
                    text(changelog)
                        .size(theme::type_scale::CAPTION)
                        .color(theme::MUTED)
                        .width(Fill)
                        .wrapping(Wrapping::Word),
                )
                .id(info_scroll_id())
                .height(Length::Fixed(170.0))
                .style(theme::scrollbar),
            );
    }

    let card = container(
        column![
            row![
                svg(svg::Handle::from_memory(app.icon().as_bytes()))
                    .width(Length::Fixed(44.0))
                    .height(Length::Fixed(44.0))
                    .style(move |_theme, _status| svg::Style { color: Some(ink) }),
                column![
                    text(app.title())
                        .size(theme::type_scale::HEADING)
                        .color(theme::INK),
                    text(state.app_status(app))
                        .size(theme::type_scale::META)
                        .color(theme::MUTED),
                ]
                .spacing(2),
                space().width(Fill),
                icon_button(
                    svg_icon::CLOSE_SVG,
                    16.0,
                    36.0,
                    theme::MUTED,
                    false,
                    Some(HubMessage::AppInfoClose),
                ),
            ]
            .spacing(14)
            .align_y(Alignment::Center),
            space().height(10),
            details,
        ]
        .spacing(0),
    )
    .padding(16)
    .width(Length::Fixed(420.0))
    .style(card_style);
    container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .into()
}

fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * KB;
    const GB: f64 = 1024.0 * MB;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.0} MB", bytes / MB)
    } else {
        format!("{:.0} KB", bytes / KB)
    }
}

fn tile_copy(tile: HubTile) -> (&'static str, &'static str) {
    match tile {
        HubTile::Start => (svg_icon::GAME_SVG, "Start"),
        HubTile::Browser => (svg_icon::GLOBE_SVG, "Browser"),
        HubTile::AddApps => (svg_icon::PLUS_SVG, "Add apps"),
        HubTile::Settings => (svg_icon::SETTINGS_SVG, "Settings"),
    }
}

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

fn menu_row_style(focused: bool, status: button::Status) -> button::Style {
    let fill = if focused {
        theme::alpha(theme::INK, 0.12)
    } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        theme::alpha(theme::INK, 0.08)
    } else {
        theme::alpha(theme::INK, 0.02)
    };
    button::Style {
        background: Some(Background::Color(fill)),
        text_color: theme::INK,
        border: Border {
            color: if focused {
                theme::alpha(theme::INK, 0.28)
            } else {
                Color::TRANSPARENT
            },
            width: 1.0,
            radius: theme::radius::MD.into(),
        },
        ..button::Style::default()
    }
}

fn app_tile_style(selected: bool, dim: bool, status: button::Status) -> button::Style {
    let (fill, border, ink) = if selected {
        (
            theme::alpha(theme::INK, 0.10),
            theme::alpha(theme::INK, 0.34),
            if dim { theme::DIM } else { theme::INK },
        )
    } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        (
            theme::alpha(theme::INK, 0.08),
            theme::alpha(theme::INK, 0.22),
            if dim { theme::DIM } else { theme::INK },
        )
    } else if dim {
        (
            theme::alpha(theme::INK, 0.02),
            theme::alpha(theme::LINE, 0.35),
            theme::DIM,
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

fn tile_style(
    selected: bool,
    interactive: bool,
    dim: bool,
    status: button::Status,
) -> button::Style {
    let (fill, border, ink) = if !interactive {
        (
            theme::alpha(theme::INK, 0.02),
            theme::alpha(theme::LINE, 0.4),
            theme::DIM,
        )
    } else if selected {
        (
            theme::alpha(theme::INK, 0.10),
            theme::alpha(theme::INK, 0.34),
            if dim { theme::DIM } else { theme::INK },
        )
    } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        (
            theme::alpha(theme::INK, 0.08),
            theme::alpha(theme::INK, 0.22),
            if dim { theme::DIM } else { theme::INK },
        )
    } else if dim {
        (
            theme::alpha(theme::INK, 0.02),
            theme::alpha(theme::LINE, 0.35),
            theme::DIM,
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
        assert_eq!(state.selected_tile(), HubTile::Settings);
        state.move_selection(1);
        assert_eq!(state.selected_tile(), HubTile::Start);
        state.move_selection(-1);
        assert_eq!(state.selected_tile(), HubTile::Settings);
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
    fn uninstalled_app_offers_install_and_dims() {
        let mut state = State {
            engine_ready: false,
            ..State::default()
        };
        assert_eq!(
            state.app_actions(0),
            vec![AppAction::Install, AppAction::Info]
        );
        assert!(state.app_dimmed(HubApp::Browser));
        state.engine_ready = true;
        assert_eq!(
            state.app_actions(0),
            vec![AppAction::Uninstall, AppAction::Info]
        );
        assert!(!state.app_dimmed(HubApp::Browser));
    }

    #[test]
    fn install_and_uninstall_progress_is_visible_in_status() {
        let mut state = State::default();
        state.set_installing(true);
        assert_eq!(state.app_status(HubApp::Browser), "Preparing");
        state.set_install_progress(EngineProgress {
            phase: EnginePhase::Downloading,
            fraction: Some(0.42),
            rate_bps: Some(8.2 * 1024.0 * 1024.0),
        });
        assert_eq!(state.app_status(HubApp::Browser), "Downloading");
        assert_eq!(state.progress.label(), "Downloading");
        state.install_done(true, None);
        assert!(!state.installing);
        assert_eq!(state.app_status(HubApp::Browser), "Installed");
        state.set_uninstalling();
        assert_eq!(state.app_status(HubApp::Browser), "Uninstalling");
        assert!(state.busy());
        state.advance_progress_anim();
        assert!(state.progress_bar_value() > 0.0);
        state.uninstall_done(None);
        assert_eq!(state.app_status(HubApp::Browser), "Not installed");
        assert!(!state.busy());
    }

    #[test]
    fn update_ready_offers_update_and_managed_hides_uninstall() {
        let mut state = State {
            engine_ready: true,
            engine_version: Some("1.0.0".to_string()),
            engine_latest_version: Some("1.0.1".to_string()),
            engine_update_ready: true,
            ..State::default()
        };
        assert_eq!(
            state.app_actions(0),
            vec![AppAction::Update, AppAction::Uninstall, AppAction::Info]
        );
        assert_eq!(state.app_status(HubApp::Browser), "Update ready");
        state.engine_managed = true;
        assert_eq!(state.app_actions(0), vec![AppAction::Info]);
        assert_eq!(state.app_status(HubApp::Browser), "Managed by Store");
    }

    #[test]
    fn cancel_is_offered_only_while_installing() {
        let mut state = State::default();
        state.set_installing(true);
        assert_eq!(
            state.app_actions(0),
            vec![AppAction::Cancel, AppAction::Info]
        );
        state.set_cancelling();
        assert_eq!(state.app_status(HubApp::Browser), "Cancelling");
        assert_eq!(state.app_actions(0), vec![AppAction::Info]);
        state.install_cancelled(false);
        assert!(!state.busy());
        assert_eq!(state.app_status(HubApp::Browser), "Not installed");
    }

    #[test]
    fn settings_rows_cycle_and_report_values() {
        let mut state = State::default();
        state.set_settings(true, 70, false, 40, 5 * 1024);
        assert_eq!(state.settings_value(0), "On");
        assert_eq!(state.settings_value(1), "70%");
        assert_eq!(state.settings_value(2), "Off");
        assert_eq!(state.settings_value(3), "40%");
        assert_eq!(state.settings_value(4), "5 MB/s");
        state.settings_move(1);
        state.settings_move(-1);
        assert_eq!(state.settings_focus, 0);
        state.settings_move(-1);
        assert_eq!(state.settings_focus, SETTINGS_ROWS.len() - 1);
    }

    #[test]
    fn info_size_uses_whole_megabytes() {
        assert_eq!(format_size(196 * 1024 * 1024), "196 MB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0 GB");
        assert_eq!(format_size(512 * 1024), "512 KB");
    }

    #[test]
    fn apps_menu_focus_wraps_over_actions() {
        let mut state = State {
            engine_ready: false,
            ..State::default()
        };
        state.open_apps();
        state.open_apps_menu(0);
        assert_eq!(state.apps_menu_action(), Some(AppAction::Install));
        state.apps_menu_move(1);
        assert_eq!(state.apps_menu_action(), Some(AppAction::Info));
        state.apps_menu_move(1);
        assert_eq!(state.apps_menu_action(), Some(AppAction::Install));
        state.apps_menu_move(-1);
        assert_eq!(state.apps_menu_action(), Some(AppAction::Info));
    }

    #[test]
    fn apps_screen_actions_back_out_one_level() {
        let mut state = State::default();
        state.open_apps();
        assert!(state.apps_open);
        state.open_apps_menu(0);
        assert!(state.apps_menu.is_some());
        state.close_apps_menu();
        assert!(state.apps_menu.is_none());
        assert!(state.apps_open);
        state.close_apps();
        assert!(!state.apps_open);
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
