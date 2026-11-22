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
//! when its engine is missing and Triangle opens its actions/Info popup.

use crate::controller::dualsense::lightbar;
use crate::controller::model::Serial;
use crate::ui::face::{FaceButton, action_cluster_spaced, hold_hint, plain_hint};
use crate::ui::popup::ControllerRow;
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
pub const HUB_HEIGHT: f32 = 280.0;

/// Main hub section inset (the card itself has no padding).
const SECTION_PAD_X: f32 = 12.0;
const SECTION_PAD_Y: f32 = 8.0;
const TILE_H: f32 = 88.0;
const TILE_GAP: f32 = 6.0;
const ICON: f32 = 42.0;
const TILE_TITLE: f32 = 14.0;
/// Controller strip card metrics. Unfocused cards are wheel-only; the
/// selected card expands to leave room for name/status and action hints.
pub const CONTROLLER_CARD_W: f32 = 88.0;
pub const CONTROLLER_CARD_W_SELECTED: f32 = 280.0;
pub const CONTROLLER_CARD_H: f32 = 84.0;
const CONTROLLER_GAP: f32 = 6.0;
const CONTROLLER_RING: f32 = 46.0;

/// Stable tile identity. The order in [`TILES`] is the render/selection
/// order; message handlers match on identity, never on an index captured in
/// the UI (the catalog may grow/reorder later).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubTile {
    Start,
    Browser,
}

/// Visible tiles, left to right.
pub const TILES: [HubTile; 2] = [HubTile::Start, HubTile::Browser];

/// Which row owns the pad highlight on the main hub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HubFocus {
    #[default]
    Apps,
    Controllers,
}

/// What the hub asks the shell to do. Pure data; the App owns windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubMessage {
    /// Move within the focused row.
    MoveX(i32),
    /// Switch focus between the apps and controllers rows.
    MoveY(i32),
    /// Open/close the hub shell settings screen (Options).
    SettingsToggle,
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
    /// Mouse click on a controller card: focus it and identify the pad.
    IdentifyController(usize),
    /// Identify the focused controller (Cross).
    IdentifySelected,
    /// Triangle hold completed on the focused Bluetooth pad.
    PowerOff,
    /// Open the per-app action menu (Triangle on a focused app tile).
    AppsMenuOpen(usize),
    AppsMenuMove(i32),
    AppsMenuConfirm,
    AppsMenuAction(AppAction),
    AppsMenuClose,
    AppInfoClose,
    Confirm,
    Close,
}

/// One managed app entry. Start is a built-in app and has no install/update
/// state, so it deliberately does not appear here.
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
    /// Bytes completed / total for byte-moving phases.
    pub bytes_done: u64,
    pub bytes_total: Option<u64>,
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

    /// `82 MB / 196 MB` for byte-moving phases, hidden when the feed did not
    /// advertise a total size.
    pub fn size_label(&self) -> Option<String> {
        let total = self.bytes_total.filter(|total| *total > 0)?;
        let done = self.bytes_done.min(total);
        Some(format!("{} / {}", format_size(done), format_size(total)))
    }

    /// Whole-number throughput label (`8 MB/s`). Hidden when no bytes have
    /// moved yet or the phase does not move bytes.
    pub fn rate_label(&self) -> Option<String> {
        const KB: f64 = 1024.0;
        const MB: f64 = 1024.0 * KB;
        let rate = self.rate_bps.filter(|rate| *rate >= KB)?;
        if rate >= MB {
            Some(format!("{:.0} MB/s", rate / MB))
        } else {
            Some(format!("{:.0} KB/s", rate / KB))
        }
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
pub const SETTINGS_ROWS: [&str; 7] = [
    "UI sounds",
    "Volume",
    "Controller haptics",
    "Strength",
    "Download speed",
    "Disconnected controllers",
    "Back",
];

/// Active Identify ring flash for one controller card.
#[derive(Debug, Clone)]
struct IdentifyFlash {
    serial: Serial,
    started: Instant,
}

/// Hub render/input state. Selection is an index into [`TILES`]; activation
/// resolves it through [`State::selected_tile`].
#[derive(Debug)]
pub struct State {
    selected: usize,
    /// Which main row (apps / controllers) owns the pad highlight.
    pub focus: HubFocus,
    /// Whether the Start tile is enabled in prefs. Disabled tiles still show
    /// (stable layout) but Confirm is a no-op.
    pub start_enabled: bool,
    /// Engine worker live. Drives the Browser tile state.
    pub engine_ready: bool,
    /// D-pad/stick repeat for hub movement.
    nav: crate::domain::pad::NavStepper,
    /// Face-button rising-edge latches (Cross confirm, Circle close, Options,
    /// Triangle actions on the apps row).
    pub prev_cross: bool,
    pub prev_circle: bool,
    pub prev_options: bool,
    pub prev_triangle: bool,
    /// Current face holds for the controller-card hint rings.
    pub cross_held: bool,
    pub triangle_held: bool,
    /// Triangle hold tracker (power off on the focused Bluetooth controller).
    triangle_hold: crate::domain::pad::HoldTracker,
    pub triangle_progress: f32,
    /// Armed-color chase for a completed hold hint (accent → ink over 100ms).
    pub triangle_armed_t: f32,
    hint_anim_at: Option<Instant>,
    /// Connected/remembered controller cards for the horizontal strip.
    pub controllers: Vec<ControllerRow>,
    /// Controller card under the focus ring.
    pub controller_focus: usize,
    /// Controller serial currently being powered off (feature report sent).
    pub powering_off: Option<Serial>,
    /// Ring flash started with the last successful Identify.
    identify_flash: Option<IdentifyFlash>,
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
    /// Include remembered-disconnected controllers in the strip.
    pub show_all_controllers: bool,
    /// False in packaged builds: the Store owns engine downloads/updates, so
    /// the local download cap has no effect and its row is hidden.
    pub download_cap_available: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            selected: 0,
            focus: HubFocus::Apps,
            start_enabled: true,
            engine_ready: false,
            nav: crate::domain::pad::NavStepper::default(),
            prev_cross: false,
            prev_circle: false,
            prev_options: false,
            prev_triangle: false,
            cross_held: false,
            triangle_held: false,
            triangle_hold: crate::domain::pad::HoldTracker::default(),
            triangle_progress: 0.0,
            triangle_armed_t: 0.0,
            hint_anim_at: None,
            controllers: Vec::new(),
            controller_focus: 0,
            powering_off: None,
            identify_flash: None,
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
            show_all_controllers: false,
            download_cap_available: true,
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

    /// Move the app highlight, wrapping.
    pub fn move_selection(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        let len = TILES.len() as i32;
        let next = (self.selected() as i32 + delta).rem_euclid(len);
        self.selected = next as usize;
    }

    /// Move within the focused row.
    pub fn move_across(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        match self.focus {
            HubFocus::Apps => self.move_selection(delta),
            HubFocus::Controllers => {
                if self.controllers.is_empty() {
                    return;
                }
                let len = self.controllers.len() as i32;
                self.controller_focus =
                    (self.controller_focus as i32 + delta).rem_euclid(len) as usize;
                self.reset_triangle_hold();
            }
        }
    }

    /// Switch focus between the apps row and the controllers row. Directional:
    /// Down enters the controllers strip, Up leaves it; the controllers row is
    /// skipped while empty (no dead focus).
    pub fn move_row(&mut self, delta: i32) {
        match (self.focus, delta.signum()) {
            (HubFocus::Apps, 1) if !self.controllers.is_empty() => {
                self.focus = HubFocus::Controllers;
                self.reset_triangle_hold();
            }
            (HubFocus::Controllers, -1) => {
                self.focus = HubFocus::Apps;
                self.reset_triangle_hold();
            }
            _ => {}
        }
    }

    pub fn select(&mut self, index: usize) {
        if index < TILES.len() {
            self.selected = index;
            self.focus = HubFocus::Apps;
        }
    }

    pub fn select_controller(&mut self, index: usize) {
        if index < self.controllers.len() {
            self.controller_focus = index;
            self.focus = HubFocus::Controllers;
            self.reset_triangle_hold();
        }
    }

    /// Focused controller card (clamped).
    pub fn controller_selected(&self) -> Option<&ControllerRow> {
        if self.controllers.is_empty() {
            return None;
        }
        self.controllers
            .get(self.controller_focus.min(self.controllers.len() - 1))
    }

    /// Install/refresh the controller strip rows. Keeps focus valid and clears
    /// a power-off marker once the pad has left the list.
    pub fn set_controllers(&mut self, controllers: Vec<ControllerRow>) {
        self.controllers = controllers;
        if self.controllers.is_empty() {
            self.controller_focus = 0;
            self.focus = HubFocus::Apps;
        } else if self.controller_focus >= self.controllers.len() {
            self.controller_focus = self.controllers.len() - 1;
        }
        if self
            .powering_off
            .as_ref()
            .is_some_and(|s| !self.controllers.iter().any(|row| row.serial == *s))
        {
            self.powering_off = None;
        }
    }

    fn reset_triangle_hold(&mut self) {
        self.triangle_hold.reset();
        self.triangle_progress = 0.0;
        self.triangle_armed_t = 0.0;
        self.hint_anim_at = None;
    }

    /// Start the UI ring flash that mirrors the lightbar Identify pattern.
    pub fn begin_identify_flash(&mut self, serial: &Serial) {
        self.identify_flash = Some(IdentifyFlash {
            serial: serial.clone(),
            started: Instant::now(),
        });
    }

    /// True while a ring Identify flash still has frames to draw.
    pub fn identify_flash_active(&self) -> bool {
        self.identify_flash.as_ref().is_some_and(|flash| {
            lightbar::identify_flash_is_white(flash.started, Instant::now()).is_some()
        })
    }

    /// Drop finished flash state; call from the frame subscription.
    pub fn tick_identify_flash(&mut self) {
        let finished = self.identify_flash.as_ref().is_some_and(|flash| {
            lightbar::identify_flash_is_white(flash.started, Instant::now()).is_none()
        });
        if finished {
            self.identify_flash = None;
        }
    }

    fn ring_flash_white(&self, serial: &Serial) -> bool {
        self.identify_flash.as_ref().is_some_and(|flash| {
            flash.serial == *serial
                && lightbar::identify_flash_is_white(flash.started, Instant::now()) == Some(true)
        })
    }

    pub fn tile_enabled(&self, tile: HubTile) -> bool {
        match tile {
            HubTile::Start => self.start_enabled,
            HubTile::Browser => true,
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
        self.prev_options = false;
        self.prev_triangle = false;
        self.cross_held = false;
        self.triangle_held = false;
        self.focus = HubFocus::Apps;
        self.controller_focus = 0;
        self.powering_off = None;
        self.identify_flash = None;
        self.reset_triangle_hold();
        self.apps_menu = None;
        self.apps_info = None;
        self.settings_open = false;
        self.settings_focus = 0;
        self.install_error = None;
    }

    pub fn open_settings(&mut self) {
        self.settings_open = true;
        self.settings_focus = 0;
        self.apps_menu = None;
        self.apps_info = None;
        self.install_error = None;
    }

    pub fn close_settings(&mut self) {
        self.settings_open = false;
        self.settings_focus = 0;
    }

    pub fn settings_visible(&self, row: usize) -> bool {
        row != 4 || self.download_cap_available
    }

    pub fn settings_move(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        let rows: Vec<usize> = (0..SETTINGS_ROWS.len())
            .filter(|row| self.settings_visible(*row))
            .collect();
        if rows.is_empty() {
            return;
        }
        let current = rows
            .iter()
            .position(|row| *row == self.settings_focus)
            .unwrap_or(0) as i32;
        let next = (current + delta).rem_euclid(rows.len() as i32) as usize;
        self.settings_focus = rows[next];
    }

    pub fn settings_value(&self, row: usize) -> String {
        match row {
            0 => (if self.sound_enabled { "On" } else { "Off" }).to_string(),
            1 => format!("{}%", self.sound_volume),
            2 => (if self.haptics_enabled { "On" } else { "Off" }).to_string(),
            3 => format!("{}%", self.haptics_strength),
            4 => crate::persist::prefs::engine_download_limit_label(self.download_limit_kbps),
            5 => (if self.show_all_controllers {
                "Show"
            } else {
                "Hide"
            })
            .to_string(),
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
        show_all_controllers: bool,
    ) {
        self.sound_enabled = sounds;
        self.sound_volume = sound_volume;
        self.haptics_enabled = haptics;
        self.haptics_strength = haptics_strength;
        self.download_limit_kbps =
            crate::persist::prefs::clamp_engine_download_limit(download_limit_kbps);
        self.show_all_controllers = show_all_controllers;
    }

    pub fn open_apps_menu(&mut self, index: usize) {
        if index >= APPS.len() {
            return;
        }
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
            bytes_done: 0,
            bytes_total: None,
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
            bytes_done: 0,
            bytes_total: None,
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
        use crate::domain::pad::NavAction;
        let mut message = None;
        self.cross_held = sample.cross;
        self.triangle_held = sample.triangle;
        let triangle_rising = sample.triangle && !self.prev_triangle;
        let nav = self.nav.update(sample, now, true, true);
        if let Some(action) = nav {
            message = match action {
                NavAction::Left => Some(HubMessage::MoveX(-1)),
                NavAction::Right => Some(HubMessage::MoveX(1)),
                NavAction::Up => Some(HubMessage::MoveY(-1)),
                NavAction::Down => Some(HubMessage::MoveY(1)),
                _ => None,
            };
        }
        // Triangle hold powers off the focused Bluetooth pad; movement or a
        // foreign press cancels it (same discipline as Start's dock row).
        let can_power = self.focus == HubFocus::Controllers
            && self.controller_selected().is_some_and(|row| {
                row.show_power_off() && self.powering_off.as_ref() != Some(&row.serial)
            });
        let foreign =
            sample.cross || sample.circle || sample.square || sample.options || sample.create;
        if self.triangle_hold.is_active() && (nav.is_some() || foreign) {
            self.reset_triangle_hold();
        }
        let (progress, completed) = self.triangle_hold.update(sample.triangle && can_power, now);
        self.triangle_progress = progress;
        let dt = self
            .hint_anim_at
            .map(|at| now.saturating_duration_since(at).as_secs_f32())
            .unwrap_or(0.0)
            .min(0.05);
        self.hint_anim_at = Some(now);
        approach_hint_anim(&mut self.triangle_armed_t, progress >= 1.0, dt);
        if completed && can_power {
            message = Some(HubMessage::PowerOff);
        }

        if sample.cross && !self.prev_cross {
            message = Some(HubMessage::Confirm);
        } else if sample.circle && !self.prev_circle {
            message = Some(HubMessage::Close);
        } else if sample.options && !self.prev_options {
            message = Some(HubMessage::SettingsToggle);
        } else if triangle_rising
            && !self.settings_open
            && self.apps_info.is_none()
            && self.apps_menu.is_none()
            && self.focus == HubFocus::Apps
            && matches!(self.selected_tile(), HubTile::Browser)
        {
            // Apps-row actions live behind Triangle; Cross still activates.
            message = Some(HubMessage::AppsMenuOpen(0));
        }
        self.prev_cross = sample.cross;
        self.prev_circle = sample.circle;
        self.prev_options = sample.options;
        self.prev_triangle = sample.triangle;
        message
    }
}

/// Render the hub card. The window root is transparent; the card is the only
/// opaque surface. `spectrum` colors the controller battery rings.
pub fn view<'a>(
    state: &'a State,
    spectrum: &crate::domain::color::BatterySpectrum,
    stage_w: f32,
    _stage_h: f32,
    backdrop_alpha: f32,
) -> Element<'a, HubMessage> {
    if state.settings_open {
        return settings_view(state, stage_w, backdrop_alpha);
    }
    let card_w = hub_card_width(stage_w);
    let inner_w = (card_w - 2.0 * SECTION_PAD_X).max(1.0);
    let mut content = column![
        container(hub_header(inner_w))
            .padding([8.0, SECTION_PAD_X])
            .width(Length::Fixed(card_w)),
        section_separator(card_w),
        container(apps_row(state, inner_w))
            .padding([SECTION_PAD_Y, SECTION_PAD_X])
            .width(Length::Fixed(card_w)),
    ]
    .spacing(0);
    if let Some(error) = &state.install_error {
        content = content.push(
            container(
                text(error.as_str())
                    .size(theme::type_scale::CAPTION)
                    .color(theme::WARNING)
                    .width(Fill)
                    .wrapping(Wrapping::Word),
            )
            .padding([4.0, SECTION_PAD_X + 4.0])
            .width(Length::Fixed(card_w)),
        );
    }
    content = content.push(section_separator(card_w)).push(
        container(controllers_row(state, spectrum, inner_w))
            .padding([6.0, SECTION_PAD_X])
            .width(Length::Fixed(card_w)),
    );
    let card = container(content)
        .padding(0)
        .width(Length::Fixed(card_w))
        .style(card_style);
    let base: Element<'a, HubMessage> = frame(card.into(), backdrop_alpha);
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

/// Hub card width for a monitor stage (also used for controller scroll math).
pub fn hub_card_width(stage_w: f32) -> f32 {
    (stage_w - 32.0).clamp(340.0, HUB_WIDTH - 24.0)
}

/// Widget id for the horizontal controller strip.
pub fn controllers_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("hub-controllers")
}

/// Horizontal scroll target that centers controller card `index`.
pub fn controller_scroll_x(index: usize, stage_w: f32) -> f32 {
    let card_w = hub_card_width(stage_w);
    let viewport = (card_w - 2.0 * SECTION_PAD_X).max(1.0);
    // Every card before the target collapses to the wheel-only width; the
    // previous selection shrinks before the scroll lands.
    let x = index as f32 * (CONTROLLER_CARD_W + CONTROLLER_GAP);
    (x - (viewport - CONTROLLER_CARD_W_SELECTED).max(0.0) * 0.5).max(0.0)
}

fn section_separator<'a>(width: f32) -> Element<'a, HubMessage> {
    container(space())
        .width(Length::Fixed(width))
        .height(Length::Fixed(1.0))
        .style(|_theme| container::Style {
            background: Some(Background::Color(theme::alpha(theme::INK, 0.06))),
            ..container::Style::default()
        })
        .into()
}

/// Branding + Options hint header row.
fn hub_header<'a>(width: f32) -> Element<'a, HubMessage> {
    let brand = row![
        svg(svg::Handle::from_memory(svg_icon::DUALSENSE_SVG.as_bytes()))
            .width(Length::Fixed(24.0))
            .height(Length::Fixed(24.0))
            .style(|_theme, _status| svg::Style {
                color: Some(theme::ACCENT)
            }),
        text("SDSC Utils")
            .size(15.0)
            .font(iced::Font {
                weight: iced::font::Weight::Semibold,
                ..iced::Font::default()
            })
            .color(theme::INK),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let settings_hint = button(
        row![
            text("Options")
                .size(theme::type_scale::CAPTION)
                .color(theme::MUTED),
            svg(svg::Handle::from_memory(svg_icon::SETTINGS_SVG.as_bytes()))
                .width(Length::Fixed(14.0))
                .height(Length::Fixed(14.0))
                .style(|_theme, _status| svg::Style {
                    color: Some(theme::MUTED)
                }),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding([4, 8])
    .style(options_pill_style)
    .on_press(HubMessage::SettingsOpen);
    row![brand, space().width(Fill), settings_hint]
        .width(Length::Fixed(width))
        .align_y(Alignment::Center)
        .into()
}

/// Start / Browser tiles.
fn apps_row<'a>(state: &State, inner_w: f32) -> Element<'a, HubMessage> {
    let gaps = (TILES.len().saturating_sub(1)) as f32 * TILE_GAP;
    let tile_w = ((inner_w - gaps) / TILES.len() as f32).max(80.0);
    let mut tiles = row![].spacing(TILE_GAP).align_y(Alignment::Center);
    for (i, tile) in TILES.into_iter().enumerate() {
        tiles = tiles.push(tile_button(state, tile, i, tile_w));
    }
    tiles.width(Length::Fixed(inner_w)).into()
}

/// Horizontal, scrollable controller cards.
fn controllers_row<'a>(
    state: &'a State,
    spectrum: &crate::domain::color::BatterySpectrum,
    inner_w: f32,
) -> Element<'a, HubMessage> {
    let strip: Element<'a, HubMessage> = if state.controllers.is_empty() {
        container(
            text("No controllers connected")
                .size(theme::type_scale::CAPTION)
                .color(theme::DIM),
        )
        .width(Length::Fixed(inner_w))
        .height(Length::Fixed(CONTROLLER_CARD_H))
        .padding([0, 8])
        .align_y(Alignment::Center)
        .style(|_theme| container::Style {
            background: Some(Background::Color(theme::alpha(theme::INK, 0.02))),
            border: Border {
                color: theme::alpha(theme::LINE, 0.35),
                width: 1.0,
                radius: theme::radius::LG.into(),
            },
            ..container::Style::default()
        })
        .into()
    } else {
        let mut cards = row![].spacing(CONTROLLER_GAP).align_y(Alignment::Center);
        for (i, controller) in state.controllers.iter().enumerate() {
            cards = cards.push(controller_card(state, spectrum, controller, i));
        }
        scrollable(cards)
            .id(controllers_scroll_id())
            .direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::new(),
            ))
            .width(Length::Fixed(inner_w))
            .height(Length::Fixed(CONTROLLER_CARD_H + 10.0))
            .style(theme::scrollbar)
            .into()
    };
    strip
}

fn controller_card<'a>(
    state: &'a State,
    spectrum: &crate::domain::color::BatterySpectrum,
    row: &'a ControllerRow,
    index: usize,
) -> Element<'a, HubMessage> {
    let selected = state.focus == HubFocus::Controllers
        && state
            .controller_focus
            .min(state.controllers.len().saturating_sub(1))
            == index;
    let powering_off = row.connected && state.powering_off.as_ref() == Some(&row.serial);
    let live = row.connected && !powering_off;
    let ring_color = if state.ring_flash_white(&row.serial) {
        theme::from_rgb(lightbar::IDENTIFY_FLASH)
    } else if live {
        theme::from_rgb(spectrum.color_at_percent(row.percent))
    } else {
        theme::DIM
    };
    let ring = crate::ui::percent_ring::percent_ring(
        row.percent,
        ring_color,
        CONTROLLER_RING,
        row.eta.clone(),
        1.0,
        if live { 1.0 } else { 0.6 },
    );
    // Default strip state is wheel-only. Selecting a card expands it to the
    // name/status and the action hints.
    let body: Element<'a, HubMessage> = if selected {
        let details_w = CONTROLLER_CARD_W_SELECTED - 12.0 - CONTROLLER_RING - 8.0;
        let meta = if powering_off {
            "Powering off".to_string()
        } else if !row.connected {
            "Disconnected".to_string()
        } else {
            format!("{} · {}", row.connection, row.state)
        };
        let mut hints = Vec::new();
        if !powering_off {
            if row.show_identify() {
                hints.push(plain_hint(FaceButton::Cross, "Identify", state.cross_held));
            }
            if row.show_power_off() {
                hints.push(hold_hint(
                    FaceButton::Triangle,
                    "Power off",
                    state.triangle_progress,
                    state.triangle_armed_t,
                    state.triangle_held,
                ));
            }
        }
        let mut details = column![
            text(text_fit::ellipsize_to_width(
                row.display_name(),
                13.0,
                details_w
            ))
            .size(13.0)
            .font(iced::Font {
                weight: iced::font::Weight::Semibold,
                ..iced::Font::default()
            })
            .color(if live { theme::INK } else { theme::MUTED })
            .width(Fill),
            text(text_fit::ellipsize_to_width(
                &meta,
                theme::type_scale::CAPTION,
                details_w
            ))
            .size(theme::type_scale::CAPTION)
            .color(if powering_off {
                theme::MUTED
            } else if row.low {
                theme::WARNING
            } else {
                theme::DIM
            })
            .width(Fill),
        ]
        .spacing(2)
        .width(Fill);
        if !hints.is_empty() {
            details = details
                .push(space().height(2))
                .push(action_cluster_spaced::<HubMessage>(&hints, 10.0, true));
        }
        row![ring, details]
            .spacing(8)
            .align_y(Alignment::Center)
            .width(Fill)
            .into()
    } else {
        ring
    };
    let card_w = if selected {
        CONTROLLER_CARD_W_SELECTED
    } else {
        CONTROLLER_CARD_W
    };
    let mut b = button(
        container(body)
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill)
            .padding(0),
    )
    .width(Length::Fixed(card_w))
    .height(Length::Fixed(CONTROLLER_CARD_H))
    .padding(6)
    .style(move |_theme, status| controller_card_style(selected, live, status));
    // Wheel-only cards still need a way to reveal their name/status; connected
    // cards additionally Identify. Disconnected selection is a no-op beyond
    // expanding the card.
    b = b.on_press(HubMessage::IdentifyController(index));
    b.into()
}

/// ~100ms accent → ink chase mirroring Start's armed hold-hint animation.
fn approach_hint_anim(current: &mut f32, target: bool, dt: f32) {
    const HINT_ANIM_SECS: f32 = crate::ui::motion::FAST_MS as f32 / 1000.0;
    let target = if target { 1.0 } else { 0.0 };
    let step = dt / HINT_ANIM_SECS;
    if *current < target {
        *current = (*current + step).min(target);
    } else if *current > target {
        *current = (*current - step).max(target);
    }
}

fn options_pill_style(_theme: &Theme, status: button::Status) -> button::Style {
    let fill = if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        theme::alpha(theme::INK, 0.10)
    } else {
        theme::alpha(theme::INK, 0.04)
    };
    button::Style {
        background: Some(Background::Color(fill)),
        text_color: theme::MUTED,
        border: Border {
            color: theme::alpha(theme::INK, 0.12),
            width: 1.0,
            radius: theme::radius::PILL.into(),
        },
        ..button::Style::default()
    }
}

fn controller_card_style(selected: bool, live: bool, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    let (fill, border) = if selected {
        (
            theme::alpha(theme::INK, 0.10),
            theme::alpha(theme::INK, 0.34),
        )
    } else if hovered {
        (
            theme::alpha(theme::INK, 0.07),
            theme::alpha(theme::INK, 0.20),
        )
    } else {
        (
            theme::alpha(theme::INK, if live { 0.03 } else { 0.015 }),
            theme::GLASS_HAIRLINE,
        )
    };
    button::Style {
        background: Some(Background::Color(fill)),
        text_color: theme::INK,
        border: Border {
            color: border,
            width: 1.0,
            radius: theme::radius::LG.into(),
        },
        shadow: if selected {
            Shadow {
                color: theme::alpha(theme::INK, 0.10),
                offset: Vector::new(0.0, 0.0),
                blur_radius: 14.0,
            }
        } else {
            Shadow::default()
        },
        ..button::Style::default()
    }
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
        if !state.settings_visible(i) {
            continue;
        }
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
            text("Cross adjust · Options/Circle back")
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
    let mut body = column![
        svg(svg::Handle::from_memory(icon.as_bytes()))
            .width(Length::Fixed(icon_size))
            .height(Length::Fixed(icon_size))
            .style(move |_theme, _status| svg::Style { color: Some(ink) }),
        text(text_fit::ellipsize_to_width(
            title,
            title_size,
            width - 12.0
        ))
        .size(title_size)
        .font(font)
        .color(ink),
        phase,
    ];
    if let Some(size) = state.progress.size_label() {
        body = body.push(
            text(size)
                .size(theme::type_scale::CAPTION)
                .color(theme::MUTED),
        );
    }
    body.push(
        progress_bar(0.0..=1.0, state.progress_bar_value())
            .length(Length::Fixed((width - 40.0).max(80.0)))
            .girth(3.0)
            .style(progress_style),
    )
    .spacing(2)
    .align_x(Alignment::Center)
    .into()
}

fn tile_button<'a>(
    state: &State,
    tile: HubTile,
    index: usize,
    width: f32,
) -> Element<'a, HubMessage> {
    let selected = state.focus == HubFocus::Apps && state.selected() == index;
    let interactive = state.tile_enabled(tile);
    let dim = match tile {
        HubTile::Start => !state.start_enabled,
        HubTile::Browser => !state.engine_ready,
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
        text(text_fit::ellipsize_to_width(
            title,
            TILE_TITLE,
            width - 16.0
        ))
        .size(TILE_TITLE)
        .font(title_font)
        .color(ink),
    ]
    .spacing(4)
    .align_x(Alignment::Center)
    .into();
    let content = if matches!(tile, HubTile::Browser) && state.busy() {
        progress_tile_body(state, icon, title, width, 22.0, TILE_TITLE, selected)
    } else if matches!(tile, HubTile::Browser) && !state.engine_ready {
        stack![
            normal,
            container(
                svg(svg::Handle::from_memory(svg_icon::DOWNLOAD_SVG.as_bytes()))
                    .width(Length::Fixed(14.0))
                    .height(Length::Fixed(14.0))
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
        .padding(4)
        .style(move |_theme, status| tile_style(selected, interactive, dim, status));
    if interactive {
        b = b.on_press(HubMessage::Activate(index));
    }
    b.into()
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
    if state.installing {
        let mut live = state.progress.label().to_string();
        if let Some(size) = state.progress.size_label() {
            live.push_str(" · ");
            live.push_str(&size);
        }
        if let Some(rate) = state.progress.rate_label() {
            live.push_str(" · ");
            live.push_str(&rate);
        }
        details = details.push(
            text(live)
                .size(theme::type_scale::CAPTION)
                .color(theme::WARNING),
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
        state.move_across(1);
        assert_eq!(state.selected_tile(), HubTile::Browser);
        state.move_across(1);
        assert_eq!(state.selected_tile(), HubTile::Start);
        state.move_across(-1);
        assert_eq!(state.selected_tile(), HubTile::Browser);
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
    fn controller_row_focus_switches_rows_and_wraps() {
        use crate::controller::model::{
            Capabilities, Connection, ControllerKind, ControllerStatus, PowerState, Serial,
        };
        let row = ControllerRow::connected(
            &ControllerStatus {
                index: 1,
                kind: ControllerKind::DualSense,
                serial: Serial::from("abc"),
                product: "DualSense".into(),
                connection: Connection::Bluetooth,
                percent: 80,
                state: PowerState::Discharging,
                capabilities: Capabilities::FULL,
            },
            false,
            true,
            None,
            20,
            None,
        );
        let mut state = State::default();
        state.set_controllers(vec![row.clone(), {
            let mut second = row.clone();
            second.serial = Serial::from("def");
            second
        }]);
        assert_eq!(state.focus, HubFocus::Apps);
        state.move_row(1);
        assert_eq!(state.focus, HubFocus::Controllers);
        state.move_row(1);
        assert_eq!(state.focus, HubFocus::Controllers);
        state.move_across(1);
        assert_eq!(
            state.controller_selected().map(|r| r.serial.as_ref()),
            Some("def")
        );
        state.move_across(1);
        assert_eq!(
            state.controller_selected().map(|r| r.serial.as_ref()),
            Some("abc")
        );
        state.move_row(-1);
        assert_eq!(state.focus, HubFocus::Apps);
    }

    #[test]
    fn empty_controller_row_is_not_focused() {
        let mut state = State::default();
        state.move_row(1);
        assert_eq!(state.focus, HubFocus::Apps);
    }

    #[test]
    fn options_edge_toggles_settings_request() {
        use crate::domain::pad::PadSample;
        let mut state = State::default();
        let now = Instant::now();
        let mut sample = PadSample {
            options: true,
            ..PadSample::default()
        };
        assert_eq!(
            state.pad_tick(&sample, now),
            Some(HubMessage::SettingsToggle)
        );
        assert!(!matches!(
            state.pad_tick(&sample, now),
            Some(HubMessage::SettingsToggle)
        ));
        sample.options = false;
        state.pad_tick(&sample, now);
        sample.options = true;
        assert_eq!(
            state.pad_tick(&sample, now),
            Some(HubMessage::SettingsToggle)
        );
    }

    #[test]
    fn disabled_start_tile_cannot_be_activated() {
        let mut state = State::default();
        state.on_open(false, true);
        assert!(!state.tile_enabled(HubTile::Start));
        assert!(state.tile_enabled(HubTile::Browser));
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
            bytes_done: 82 * 1024 * 1024,
            bytes_total: Some(196 * 1024 * 1024),
            rate_bps: Some(8.2 * 1024.0 * 1024.0),
        });
        assert_eq!(state.app_status(HubApp::Browser), "Downloading");
        assert_eq!(state.progress.label(), "Downloading");
        assert_eq!(
            state.progress.size_label().as_deref(),
            Some("82 MB / 196 MB")
        );
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
        state.set_settings(true, 70, false, 40, 5 * 1024, true);
        assert_eq!(state.settings_value(0), "On");
        assert_eq!(state.settings_value(1), "70%");
        assert_eq!(state.settings_value(2), "Off");
        assert_eq!(state.settings_value(3), "40%");
        assert_eq!(state.settings_value(4), "5 MB/s");
        assert_eq!(state.settings_value(5), "Show");
        state.settings_move(1);
        state.settings_move(-1);
        assert_eq!(state.settings_focus, 0);
        state.settings_move(-1);
        assert_eq!(state.settings_focus, SETTINGS_ROWS.len() - 1);
    }

    #[test]
    fn download_cap_row_is_hidden_for_packaged_builds() {
        let mut state = State {
            download_cap_available: false,
            ..State::default()
        };
        assert!(!state.settings_visible(4));
        assert!(state.settings_visible(3));
        assert!(state.settings_visible(5));
        state.settings_focus = 3;
        state.settings_move(1);
        assert_eq!(state.settings_focus, 5, "movement skips the hidden cap row");
        state.settings_move(1);
        assert_eq!(state.settings_focus, 6);
        state.settings_move(1);
        assert_eq!(state.settings_focus, 0);
    }

    #[test]
    fn info_size_uses_whole_megabytes() {
        assert_eq!(format_size(196 * 1024 * 1024), "196 MB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0 GB");
        assert_eq!(format_size(512 * 1024), "512 KB");
    }

    #[test]
    fn throughput_label_switches_units() {
        let mut progress = EngineProgress {
            phase: EnginePhase::Downloading,
            fraction: Some(0.5),
            bytes_done: 0,
            bytes_total: None,
            rate_bps: Some(512.0 * 1024.0),
        };
        assert_eq!(progress.rate_label().as_deref(), Some("512 KB/s"));
        progress.rate_bps = Some(8.2 * 1024.0 * 1024.0);
        assert_eq!(progress.rate_label().as_deref(), Some("8 MB/s"));
        progress.rate_bps = Some(0.0);
        assert_eq!(progress.rate_label(), None);
    }

    #[test]
    fn apps_menu_focus_wraps_over_actions() {
        let mut state = State {
            engine_ready: false,
            ..State::default()
        };
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
    fn triangle_opens_browser_actions_on_the_apps_row() {
        use crate::domain::pad::PadSample;
        let mut state = State::default();
        let now = Instant::now();
        let press = PadSample {
            triangle: true,
            ..PadSample::default()
        };
        let release = PadSample::default();

        // Start has no managed actions: Triangle is inert there.
        assert_eq!(state.pad_tick(&press, now), None);

        // Browser opens the same per-app popup the old catalog used.
        state.pad_tick(&release, now + std::time::Duration::from_millis(16));
        state.select(1);
        assert_eq!(
            state.pad_tick(&press, now + std::time::Duration::from_millis(32)),
            Some(HubMessage::AppsMenuOpen(0))
        );
        assert_eq!(
            state.pad_tick(&press, now + std::time::Duration::from_millis(48)),
            None
        );
    }

    #[test]
    fn controller_scroll_accounts_for_compact_and_selected_widths() {
        assert_eq!(controller_scroll_x(0, 1920.0), 0.0);
        let expected = 2.0 * (CONTROLLER_CARD_W + CONTROLLER_GAP)
            - ((hub_card_width(1920.0) - 2.0 * SECTION_PAD_X - CONTROLLER_CARD_W_SELECTED) * 0.5);
        assert!((controller_scroll_x(2, 1920.0) - expected).abs() < 0.01);
    }

    #[test]
    fn hold_hint_armed_chase_hits_full_and_returns() {
        let mut armed = 0.0;
        for _ in 0..10 {
            approach_hint_anim(&mut armed, true, 0.01);
        }
        assert!((armed - 1.0).abs() < 1e-4);
        for _ in 0..10 {
            approach_hint_anim(&mut armed, false, 0.01);
        }
        assert!(armed.abs() < 1e-4);
    }

    #[test]
    fn controller_cards_track_face_holds() {
        use crate::domain::pad::PadSample;
        let mut state = State::default();
        let now = Instant::now();
        let sample = PadSample {
            cross: true,
            triangle: true,
            ..PadSample::default()
        };
        state.pad_tick(&sample, now);
        assert!(state.cross_held);
        assert!(state.triangle_held);
        state.pad_tick(
            &PadSample::default(),
            now + std::time::Duration::from_millis(16),
        );
        assert!(!state.cross_held);
        assert!(!state.triangle_held);
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
