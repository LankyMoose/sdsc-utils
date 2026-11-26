//! PS overlay Home: the topmost app switcher. Its window always covers the
//! monitor and dims the whole screen behind a centered card; the toast is
//! composited above that scrim.
//!
//! Home is the auto-open target; Launcher and Browser are normal-level apps
//! underneath and mutually exclusive. It is modal for pad input while open:
//! the underlying app keeps its last frame but receives no nav.
//!
//! Tiles are stable identities, never row indices. Icon-first tiles carry
//! a large glyph plus a title (no subtitles); the Browser tile stays visible
//! when its engine is missing and Triangle opens its actions/Info popup.

use crate::app::installs::{DOWNLOAD_LIMIT_OPTIONS_KBPS, download_limit_label};
use crate::controller::dualsense::lightbar;
use crate::controller::model::Serial;
use crate::persist::prefs::{AUTO_OPEN_MODES, HomeAutoOpen};
use crate::ui::face::{
    FaceButton, RingStyle, action_cluster_spaced, face_glyph, hint_text_capsule, hold_hint,
    plain_hint,
};
use crate::ui::popup::ControllerRow;
use crate::ui::svg_icon;
use crate::ui::text_fit;
use crate::ui::theme;
use iced::widget::text::Wrapping;
use iced::widget::{
    column, container, keyed_column, mouse_area, progress_bar, row, scrollable, space, stack, svg,
    text, toggler,
};
use iced::{
    Alignment, Background, Border, Color, Element, Fill, Length, Padding, Shadow, Theme, Vector,
};
use std::time::Instant;

/// Nominal Home card width/height. The card is centered in the full-monitor
/// window; `HOME_HEIGHT` is only the pre-open stage default.
pub const HOME_WIDTH: f32 = 500.0;
pub const HOME_HEIGHT: f32 = 280.0;

/// Main Home section inset (the card itself has no padding).
const SECTION_PAD_X: f32 = 12.0;
/// Vertical row inset, kept equal for header/apps/action hints so the
/// separators read as the same gap throughout the card.
const ROW_PAD_Y: f32 = 4.0;
const TILE_H: f32 = 88.0;
const TILE_GAP: f32 = 6.0;
const ICON: f32 = 42.0;
const TILE_TITLE: f32 = 14.0;

/// Restored Launcher-style controllers dock (top-right, vertical). Collapsed shows the
/// battery-ring column; Select/Share (Create) expands it in place.
const DOCK_RING_INSET: f32 = 6.0;
const DOCK_RING_MIN: f32 = 52.0;
const DOCK_RING_MAX: f32 = 68.0;
const DOCK_PEEK_W: f32 = DOCK_RING_MAX + DOCK_RING_INSET * 2.0;
const DOCK_MAX_W: f32 = 560.0;
const DOCK_ROW_H: f32 = 80.0;
const DOCK_ROW_GAP: f32 = 8.0;
const DOCK_HINT_COL_W: f32 = 210.0;
const DOCK_ANIM_MS: u64 = 220;
const DOCK_TOP: f32 = 10.0;
const DOCK_RIGHT: f32 = 16.0;
/// Home shell panel entrance/exit. Movement is gated by reduced motion;
/// opacity always animates.
pub const PANEL_MS: u64 = 280;
const PANEL_HIDDEN_SCALE: f32 = 0.96;
const PANEL_HIDDEN_Y: f32 = 28.0;

/// Resolved visual state for the centered Home card and top-right controllers
/// dock during the window shell's entrance/exit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelVisual {
    pub alpha: f32,
    pub scale: f32,
    pub offset_y: f32,
}

impl PanelVisual {
    const fn full() -> Self {
        Self {
            alpha: 1.0,
            scale: 1.0,
            offset_y: 0.0,
        }
    }

    const fn hidden() -> Self {
        Self {
            alpha: 0.0,
            scale: PANEL_HIDDEN_SCALE,
            offset_y: PANEL_HIDDEN_Y,
        }
    }
}

/// One sampled frame of Home shell: panel paint plus the full-screen scrim.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShellFrame {
    pub panel: PanelVisual,
    pub backdrop_alpha: f32,
}

impl ShellFrame {
    pub const fn full() -> Self {
        Self {
            panel: PanelVisual::full(),
            backdrop_alpha: theme::BACKDROP_ALPHA,
        }
    }

    const fn hidden() -> Self {
        Self {
            panel: PanelVisual::hidden(),
            backdrop_alpha: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellDir {
    Enter,
    Exit,
}

/// Wall-clock driver for Home window's entrance/exit. Exit captures the
/// current frame so closing mid-entrance never pops to full first.
#[derive(Debug, Clone, Copy)]
pub struct ShellAnim {
    pub dir: ShellDir,
    started: Instant,
    from: ShellFrame,
}

impl ShellAnim {
    pub fn enter(now: Instant) -> Self {
        Self {
            dir: ShellDir::Enter,
            started: now,
            from: ShellFrame::hidden(),
        }
    }

    pub fn exit(now: Instant, from: ShellFrame) -> Self {
        Self {
            dir: ShellDir::Exit,
            started: now,
            from,
        }
    }

    pub fn frame(&self, now: Instant, reduced: bool) -> ShellFrame {
        let elapsed = now.saturating_duration_since(self.started);
        match self.dir {
            ShellDir::Enter => {
                use crate::ui::motion::{ease_out_cubic, progress_linear};
                let t = progress_linear(elapsed.as_millis() as u64, PANEL_MS);
                let e = ease_out_cubic(t);
                let panel = lerp_panel(ShellFrame::hidden().panel, PanelVisual::full(), e);
                ShellFrame {
                    panel: gate_movement(panel, reduced),
                    backdrop_alpha: crate::ui::motion::shell_backdrop_alpha(
                        self.started,
                        now,
                        theme::BACKDROP_ALPHA,
                    ),
                }
            }
            ShellDir::Exit => {
                use crate::ui::motion::{ease_in_out_cubic, progress_linear};
                let t = progress_linear(elapsed.as_millis() as u64, PANEL_MS);
                let e = ease_in_out_cubic(t);
                ShellFrame {
                    panel: gate_movement(
                        lerp_panel(self.from.panel, PanelVisual::hidden(), e),
                        reduced,
                    ),
                    backdrop_alpha: self.from.backdrop_alpha * (1.0 - e),
                }
            }
        }
    }

    pub fn done(&self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.started);
        match self.dir {
            ShellDir::Enter => {
                elapsed
                    >= crate::ui::motion::SHELL_BACKDROP_DELAY
                        + crate::ui::motion::SHELL_BACKDROP_FADE
            }
            ShellDir::Exit => elapsed.as_millis() as u64 >= PANEL_MS,
        }
    }
}

fn lerp_panel(from: PanelVisual, to: PanelVisual, t: f32) -> PanelVisual {
    PanelVisual {
        alpha: from.alpha + (to.alpha - from.alpha) * t,
        scale: from.scale + (to.scale - from.scale) * t,
        offset_y: from.offset_y + (to.offset_y - from.offset_y) * t,
    }
}

fn gate_movement(panel: PanelVisual, reduced: bool) -> PanelVisual {
    PanelVisual {
        alpha: panel.alpha,
        scale: crate::ui::motion::movement_scale(panel.scale, reduced),
        offset_y: crate::ui::motion::movement_offset(panel.offset_y, reduced),
    }
}

/// Stable tile identity. The order in [`TILES`] is the render/selection
/// order; message handlers match on identity, never on an index captured in
/// the UI (the catalog may grow/reorder later).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeTile {
    Launcher,
    Browser,
}

/// Visible tiles, left to right.
pub const TILES: [HomeTile; 2] = [HomeTile::Launcher, HomeTile::Browser];

/// Which row owns the pad highlight on the main Home.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HomeFocus {
    #[default]
    Apps,
    Controllers,
}

/// What Home asks the shell to do. Pure data; the App owns windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeMessage {
    /// Move within the focused row.
    MoveX(i32),
    /// Switch focus between the apps and controllers rows.
    MoveY(i32),
    /// Toggle the top-right controllers dock (Select/Share).
    ToggleControllers,
    /// Toggle Connected / All in the controllers dock footer (Square).
    ToggleShowAll,
    /// Open/close Home shell settings screen (Options).
    SettingsToggle,
    /// Triangle hold completed on the focused Bluetooth pad.
    PowerOff,
    /// Open the per-app action menu (Triangle on a focused app tile).
    AppsMenuOpen(usize),
    AppsMenuMove(i32),
    AppsMenuConfirm,
    AppsMenuClose,
    Confirm,
    Close,
}

impl HomeMessage {
    /// Stable semantic action name for the trace timeline.
    pub fn trace_label(self) -> &'static str {
        match self {
            Self::MoveX(_) => "home.move_x",
            Self::MoveY(_) => "home.move_y",
            Self::ToggleControllers => "home.controllers.toggle",
            Self::ToggleShowAll => "home.controllers.show_all",
            Self::SettingsToggle => "home.settings.toggle",
            Self::PowerOff => "home.power_off",
            Self::AppsMenuOpen(_) => "home.menu.open",
            Self::AppsMenuMove(_) => "home.menu.move",
            Self::AppsMenuConfirm => "home.menu.confirm",
            Self::AppsMenuClose => "home.menu.close",
            Self::Confirm => "home.confirm",
            Self::Close => "home.close",
        }
    }
}

/// One managed app entry. Launcher is built-in and has no install/update
/// state, so it deliberately does not appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedApp {
    Browser,
}

pub const APPS: [ManagedApp; 1] = [ManagedApp::Browser];

impl ManagedApp {
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

/// Install/uninstall phases shown by Home. Mirrors the installer's
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

/// Rows in Home shell settings screen. The download cap is Home policy for
/// every standalone install, not a Browser-specific option. Auto open is Home
/// policy too, so it lives here rather than in the Launcher drawer.
pub const SETTINGS_ROWS: [&str; 6] = [
    "Auto open",
    "UI sounds",
    "Volume",
    "Controller haptics",
    "Strength",
    "Download speed",
];

/// Settings row indices used by the view and the App's apply path.
pub const SETTINGS_AUTO_OPEN: usize = 0;
pub const SETTINGS_SOUNDS: usize = 1;
pub const SETTINGS_VOLUME: usize = 2;
pub const SETTINGS_HAPTICS: usize = 3;
pub const SETTINGS_STRENGTH: usize = 4;
pub const SETTINGS_DOWNLOAD: usize = 5;

/// A committed dropdown choice. Kept pure so the App owns applying prefs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeSettingChoice {
    AutoOpen(HomeAutoOpen),
    DownloadLimit(u32),
}

/// Active Identify ring flash for one controller card.
#[derive(Debug, Clone)]
struct IdentifyFlash {
    serial: Serial,
    started: Instant,
}

/// Peek ↔ expanded controllers dock tween (old Launcher dock: 220ms ease-out).
#[derive(Debug, Clone, Copy)]
struct DockAnim {
    from: f32,
    to: f32,
    started: Instant,
}

/// Home render/input state. Selection is an index into [`TILES`]; activation
/// resolves it through [`State::selected_tile`].
#[derive(Debug)]
pub struct State {
    selected: usize,
    /// Which surface owns the pad input: the apps row, or the expanded
    /// top-right controllers dock.
    pub focus: HomeFocus,
    /// Whether the Launcher tile is enabled in prefs. Disabled tiles still show
    /// (stable layout) but Confirm is a no-op.
    pub launcher_enabled: bool,
    /// Engine worker live. Drives the Browser tile state.
    pub engine_ready: bool,
    /// D-pad/stick repeat for Home movement.
    nav: crate::domain::pad::NavStepper,
    /// Current face holds for hint rings and the SELECT/START capsules.
    pub cross_held: bool,
    pub square_held: bool,
    pub triangle_held: bool,
    pub options_held: bool,
    pub create_held: bool,
    /// Triangle hold tracker (power off on the focused Bluetooth controller).
    triangle_hold: crate::domain::pad::HoldTracker,
    pub triangle_progress: f32,
    /// Armed-color chase for a completed hold hint (accent → ink over 100ms).
    pub triangle_armed_t: f32,
    hint_anim_at: Option<Instant>,
    /// Connected/remembered controller rows for the top-right dock.
    pub controllers: Vec<ControllerRow>,
    /// Controller row under the focus ring.
    pub controller_focus: usize,
    /// Per-row selection wash 0..=1 (animated, never a one-frame swap).
    controller_anim: Vec<f32>,
    controller_anim_at: Option<Instant>,
    /// Peek ↔ expanded dock width/row tween.
    dock_anim: Option<DockAnim>,
    /// Effective reduced-motion gate (prefs vs OS). Dock movement snaps;
    /// opacity (row wash, shell fade) keeps animating.
    pub reduced_motion: bool,
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
    /// Home shell settings screen.
    pub settings_open: bool,
    pub settings_focus: usize,
    /// Expanded selector dropdown: (row, highlighted option index).
    /// `None` = closed; only Auto open / Download speed are selectors.
    pub settings_menu: Option<(usize, usize)>,
    pub home_auto_open: HomeAutoOpen,
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
            focus: HomeFocus::Apps,
            launcher_enabled: true,
            engine_ready: false,
            nav: crate::domain::pad::NavStepper::with_profile(crate::domain::pad::NavProfile::MENU),
            cross_held: false,
            square_held: false,
            triangle_held: false,
            options_held: false,
            create_held: false,
            triangle_hold: crate::domain::pad::HoldTracker::default(),
            triangle_progress: 0.0,
            triangle_armed_t: 0.0,
            hint_anim_at: None,
            controllers: Vec::new(),
            controller_focus: 0,
            controller_anim: Vec::new(),
            controller_anim_at: None,
            dock_anim: None,
            reduced_motion: false,
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
            settings_menu: None,
            home_auto_open: HomeAutoOpen::Any,
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

    pub fn selected_tile(&self) -> HomeTile {
        TILES[self.selected()]
    }

    /// Move the app highlight, wrapping. Returns true when it moved.
    pub fn move_selection(&mut self, delta: i32) -> bool {
        if delta == 0 {
            return false;
        }
        let len = TILES.len() as i32;
        let next = (self.selected() as i32 + delta).rem_euclid(len);
        if next as usize == self.selected() {
            return false;
        }
        self.selected = next as usize;
        true
    }

    /// Move within the focused row. The apps row is horizontal and the
    /// controllers dock is vertical; an axis only drives its own row.
    pub fn move_x(&mut self, delta: i32) -> bool {
        match self.focus {
            HomeFocus::Apps => self.move_selection(delta),
            HomeFocus::Controllers => false,
        }
    }

    pub fn move_y(&mut self, delta: i32) -> bool {
        match self.focus {
            HomeFocus::Apps => false,
            HomeFocus::Controllers => self.move_controller_selection(delta),
        }
    }

    fn move_controller_selection(&mut self, delta: i32) -> bool {
        if delta == 0 || self.controllers.is_empty() {
            return false;
        }
        let len = self.controllers.len() as i32;
        let next = (self.controller_focus as i32 + delta).rem_euclid(len);
        if next as usize == self.controller_focus {
            return false;
        }
        self.controller_focus = next as usize;
        self.reset_triangle_hold();
        true
    }

    /// True while the top-right controllers dock owns input.
    pub fn dock_expanded(&self) -> bool {
        self.focus == HomeFocus::Controllers
    }

    /// Dock visibility: stays on screen while Home Settings is open (it is
    /// display-only there), hidden only while an Info/action modal owns the
    /// screen.
    pub fn dock_visible(&self) -> bool {
        !self.controllers.is_empty() && self.apps_info.is_none() && self.apps_menu.is_none()
    }

    /// Select/Share toggles the dock: collapsed ring column ↔ expanded list.
    /// Returns true when the target changed.
    pub fn toggle_dock(&mut self, now: Instant) -> bool {
        if self.controllers.is_empty() {
            return false;
        }
        self.request_dock(!self.dock_expanded(), now)
    }

    /// Collapse the controllers dock without closing Home.
    pub fn collapse_dock(&mut self, now: Instant) -> bool {
        if !self.dock_expanded() {
            return false;
        }
        self.request_dock(false, now)
    }

    /// Request the controllers dock open/closed. Old Launcher behavior: 220ms
    /// ease-out travel; the selection wash keeps its own chase.
    pub fn request_dock(&mut self, expanded: bool, now: Instant) -> bool {
        let target = if expanded { 1.0 } else { 0.0 };
        if self
            .dock_anim
            .as_ref()
            .is_some_and(|anim| (anim.to - target).abs() < 0.01)
        {
            return false;
        }
        let already = self.dock_expanded() == expanded
            && self.dock_anim.is_none()
            && (self.dock_progress(now) - target).abs() < 0.01;
        if already {
            return false;
        }
        let from = self.dock_progress(now);
        self.focus = if expanded {
            HomeFocus::Controllers
        } else {
            HomeFocus::Apps
        };
        self.reset_triangle_hold();
        if self.reduced_motion || (from - target).abs() < 0.01 {
            self.dock_anim = None;
        } else {
            self.dock_anim = Some(DockAnim {
                from,
                to: target,
                started: now,
            });
        }
        true
    }

    /// 0 = peek ring column, 1 = expanded vertical list.
    pub fn dock_progress(&self, now: Instant) -> f32 {
        if let Some(anim) = self.dock_anim {
            let t = (now.saturating_duration_since(anim.started).as_secs_f32()
                / (DOCK_ANIM_MS.max(1) as f32 / 1000.0))
                .clamp(0.0, 1.0);
            let e = crate::ui::motion::ease_out_cubic(t);
            return anim.from + (anim.to - anim.from) * e;
        }
        if self.dock_expanded() { 1.0 } else { 0.0 }
    }

    pub fn select(&mut self, index: usize, now: Instant) {
        if index < TILES.len() {
            self.selected = index;
            self.request_dock(false, now);
        }
    }

    pub fn select_controller(&mut self, index: usize, now: Instant) {
        if index < self.controllers.len() {
            self.controller_focus = index;
            self.request_dock(true, now);
            self.focus = HomeFocus::Controllers;
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

    /// Selection wash amount for one controller row (0 = unselected, 1 = full).
    pub fn controller_focus_t(&self, index: usize) -> f32 {
        self.controller_anim
            .get(index)
            .copied()
            .unwrap_or(0.0)
            .clamp(0.0, 1.0)
    }

    /// Whether the dock travel or row selection chase needs UI ticks.
    pub fn controller_anim_needs_frames(&self) -> bool {
        if self.dock_anim.is_some() {
            return true;
        }
        let selected = self
            .controller_focus
            .min(self.controllers.len().saturating_sub(1));
        self.controller_anim.iter().enumerate().any(|(i, t)| {
            let target = if self.focus == HomeFocus::Controllers && i == selected {
                1.0
            } else {
                0.0
            };
            (t - target).abs() > 0.002
        })
    }

    /// Advance dock travel and the selected-row wash (~220/280ms).
    pub fn tick_controller_anim(&mut self, now: Instant) {
        if let Some(anim) = self.dock_anim
            && now.saturating_duration_since(anim.started).as_millis() as u64 >= DOCK_ANIM_MS
        {
            self.dock_anim = None;
        }
        let dt = self
            .controller_anim_at
            .map(|at| now.saturating_duration_since(at).as_secs_f32())
            .unwrap_or(0.0)
            .min(0.05);
        self.controller_anim_at = Some(now);
        if dt <= 0.0 {
            return;
        }
        let selected = self
            .controller_focus
            .min(self.controllers.len().saturating_sub(1));
        let anim_secs = crate::ui::motion::BASE_MS as f32 / 1000.0;
        for (i, t) in self.controller_anim.iter_mut().enumerate() {
            let target = if self.focus == HomeFocus::Controllers && i == selected {
                1.0
            } else {
                0.0
            };
            approach_anim_to(t, target, dt, anim_secs);
        }
    }

    /// Install/refresh the controller strip rows. Keeps focus valid and clears
    /// a power-off marker once the pad has left the list.
    pub fn set_controllers(&mut self, controllers: Vec<ControllerRow>) {
        // Preserve the selected serial and in-flight wash across the per-tick
        // session resync (rows may connect/disconnect between frames).
        let selected = self
            .controllers
            .get(
                self.controller_focus
                    .min(self.controllers.len().saturating_sub(1)),
            )
            .map(|row| row.serial.clone());
        let old_rows = std::mem::take(&mut self.controllers);
        let old_anim = std::mem::take(&mut self.controller_anim);
        self.controller_anim = controllers
            .iter()
            .map(|row| {
                old_rows
                    .iter()
                    .position(|old| old.serial == row.serial)
                    .and_then(|i| old_anim.get(i).copied())
                    .unwrap_or(0.0)
            })
            .collect();
        self.controllers = controllers;
        if let Some(serial) = selected
            && let Some(index) = self.controllers.iter().position(|row| row.serial == serial)
        {
            self.controller_focus = index;
        }
        if self.controllers.is_empty() {
            self.controller_focus = 0;
            self.focus = HomeFocus::Apps;
            self.controller_anim_at = None;
            self.dock_anim = None;
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

    pub fn tile_enabled(&self, tile: HomeTile) -> bool {
        match tile {
            HomeTile::Launcher => self.launcher_enabled,
            HomeTile::Browser => true,
        }
    }

    /// Reset edge latches and movement repeat when Home is opened, so a
    /// held button from the opening gesture cannot fire immediately.
    pub fn on_open(&mut self, launcher_enabled: bool, engine_ready: bool) {
        self.launcher_enabled = launcher_enabled;
        self.engine_ready = engine_ready;
        self.nav =
            crate::domain::pad::NavStepper::with_profile(crate::domain::pad::NavProfile::MENU);
        self.cross_held = false;
        self.square_held = false;
        self.triangle_held = false;
        self.options_held = false;
        self.create_held = false;
        self.focus = HomeFocus::Apps;
        self.controller_focus = 0;
        self.controller_anim.clear();
        self.controller_anim_at = None;
        self.dock_anim = None;
        self.powering_off = None;
        self.identify_flash = None;
        self.reset_triangle_hold();
        self.apps_menu = None;
        self.apps_info = None;
        self.settings_open = false;
        self.settings_focus = 0;
        self.settings_menu = None;
        self.install_error = None;
    }

    pub fn open_settings(&mut self) {
        self.settings_open = true;
        self.settings_focus = 0;
        self.settings_menu = None;
        self.apps_menu = None;
        self.apps_info = None;
        self.install_error = None;
        // Settings owns the screen; a Triangle hold started on the dock must
        // not complete behind it.
        self.reset_triangle_hold();
    }

    pub fn close_settings(&mut self) {
        self.settings_open = false;
        self.settings_focus = 0;
        self.settings_menu = None;
    }

    /// Dynamic visibility: sliders disappear with their master toggle; the
    /// download cap is absent in Store-managed builds.
    pub fn settings_visible(&self, row: usize) -> bool {
        match row {
            SETTINGS_VOLUME => self.sound_enabled,
            SETTINGS_STRENGTH => self.haptics_enabled,
            SETTINGS_DOWNLOAD => self.download_cap_available,
            SETTINGS_AUTO_OPEN | SETTINGS_SOUNDS | SETTINGS_HAPTICS => true,
            _ => false,
        }
    }

    pub fn settings_is_selector(row: usize) -> bool {
        matches!(row, SETTINGS_AUTO_OPEN | SETTINGS_DOWNLOAD)
    }

    pub fn clamp_settings_focus(&mut self) {
        if self
            .settings_menu
            .is_some_and(|(row, _)| !self.settings_visible(row) || !Self::settings_is_selector(row))
        {
            self.settings_menu = None;
        }
        if self.settings_focus >= SETTINGS_ROWS.len() || !self.settings_visible(self.settings_focus)
        {
            self.settings_focus = (0..SETTINGS_ROWS.len())
                .find(|row| self.settings_visible(*row))
                .unwrap_or(0);
        }
    }

    /// Move the settings focus over visible rows. Returns true when it moved.
    pub fn settings_move(&mut self, delta: i32) -> bool {
        if delta == 0 {
            return false;
        }
        let rows: Vec<usize> = (0..SETTINGS_ROWS.len())
            .filter(|row| self.settings_visible(*row))
            .collect();
        if rows.is_empty() {
            return false;
        }
        let current = rows
            .iter()
            .position(|row| *row == self.settings_focus)
            .unwrap_or(0) as i32;
        let next = (current + delta).rem_euclid(rows.len() as i32) as usize;
        if rows[next] == self.settings_focus {
            return false;
        }
        self.settings_focus = rows[next];
        true
    }

    pub fn settings_value(&self, row: usize) -> String {
        match row {
            SETTINGS_AUTO_OPEN => self.home_auto_open.label().to_string(),
            SETTINGS_SOUNDS => (if self.sound_enabled { "On" } else { "Off" }).to_string(),
            SETTINGS_VOLUME => format!("{}%", self.sound_volume),
            SETTINGS_HAPTICS => (if self.haptics_enabled { "On" } else { "Off" }).to_string(),
            SETTINGS_STRENGTH => format!("{}%", self.haptics_strength),
            SETTINGS_DOWNLOAD => download_limit_label(self.download_limit_kbps),
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
        home_auto_open: HomeAutoOpen,
    ) {
        self.sound_enabled = sounds;
        self.sound_volume = sound_volume;
        self.haptics_enabled = haptics;
        self.haptics_strength = haptics_strength;
        self.download_limit_kbps = crate::app::installs::clamp_download_limit(download_limit_kbps);
        self.home_auto_open = home_auto_open;
    }

    pub fn settings_menu_open(&self) -> bool {
        self.settings_menu.is_some()
    }

    pub fn settings_menu_highlight(&self) -> Option<usize> {
        self.settings_menu.map(|(_, highlight)| highlight)
    }

    pub fn open_settings_menu(&mut self, row: usize) -> bool {
        if !self.settings_visible(row) || !Self::settings_is_selector(row) {
            return false;
        }
        self.settings_menu = Some((row, self.settings_initial_highlight(row)));
        true
    }

    pub fn close_settings_menu(&mut self) {
        self.settings_menu = None;
    }

    /// Move the expanded dropdown highlight, wrapping. Returns true when it moved.
    pub fn settings_menu_move(&mut self, delta: i32) -> bool {
        if delta == 0 {
            return false;
        }
        let Some((row, highlight)) = self.settings_menu else {
            return false;
        };
        let count = Self::settings_option_count(row);
        if count == 0 {
            return false;
        }
        let next = (highlight as i32 + delta).rem_euclid(count as i32) as usize;
        if next == highlight {
            return false;
        }
        self.settings_menu = Some((row, next));
        true
    }

    pub fn settings_option_count(row: usize) -> usize {
        match row {
            SETTINGS_AUTO_OPEN => AUTO_OPEN_MODES.len(),
            SETTINGS_DOWNLOAD => DOWNLOAD_LIMIT_OPTIONS_KBPS.len(),
            _ => 0,
        }
    }

    pub fn settings_option_label(&self, row: usize, index: usize) -> String {
        match row {
            SETTINGS_AUTO_OPEN => AUTO_OPEN_MODES
                .get(index)
                .map(|mode| mode.label().to_string())
                .unwrap_or_default(),
            SETTINGS_DOWNLOAD => DOWNLOAD_LIMIT_OPTIONS_KBPS
                .get(index)
                .map(|kbps| download_limit_label(*kbps))
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    pub fn settings_option_selected(&self, row: usize, index: usize) -> bool {
        match row {
            SETTINGS_AUTO_OPEN => AUTO_OPEN_MODES
                .get(index)
                .is_some_and(|mode| *mode == self.home_auto_open),
            SETTINGS_DOWNLOAD => DOWNLOAD_LIMIT_OPTIONS_KBPS
                .get(index)
                .is_some_and(|kbps| *kbps == self.download_limit_kbps),
            _ => false,
        }
    }

    fn settings_initial_highlight(&self, row: usize) -> usize {
        match row {
            SETTINGS_AUTO_OPEN => AUTO_OPEN_MODES
                .iter()
                .position(|mode| *mode == self.home_auto_open)
                .unwrap_or(0),
            SETTINGS_DOWNLOAD => DOWNLOAD_LIMIT_OPTIONS_KBPS
                .iter()
                .position(|kbps| *kbps == self.download_limit_kbps)
                .unwrap_or(0),
            _ => 0,
        }
    }

    pub fn settings_menu_choice(&self) -> Option<HomeSettingChoice> {
        let (row, highlight) = self.settings_menu?;
        match row {
            SETTINGS_AUTO_OPEN => AUTO_OPEN_MODES
                .get(highlight)
                .copied()
                .map(HomeSettingChoice::AutoOpen),
            SETTINGS_DOWNLOAD => DOWNLOAD_LIMIT_OPTIONS_KBPS
                .get(highlight)
                .copied()
                .map(HomeSettingChoice::DownloadLimit),
            _ => None,
        }
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
            Some(ManagedApp::Browser) => {
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

    pub fn app_dimmed(&self, app: ManagedApp) -> bool {
        match app {
            ManagedApp::Browser => !self.engine_ready,
        }
    }

    pub fn app_status(&self, app: ManagedApp) -> String {
        match app {
            ManagedApp::Browser if self.installing || self.uninstalling => {
                self.progress.label().to_string()
            }
            ManagedApp::Browser if self.engine_managed => "Managed by Store".to_string(),
            ManagedApp::Browser if self.engine_restart_required => "Restart to finish".to_string(),
            ManagedApp::Browser if self.engine_update_ready => "Update ready".to_string(),
            ManagedApp::Browser if self.engine_ready => "Installed".to_string(),
            ManagedApp::Browser => "Not installed".to_string(),
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

    /// Feed one pad frame. Taps arrive as source-side actions (`tap`); this
    /// only handles nav repeat, held visuals, and the Triangle power-off hold.
    pub fn pad_tick(
        &mut self,
        sample: &crate::domain::pad::PadSample,
        now: Instant,
        tap: Option<HomeMessage>,
    ) -> Option<HomeMessage> {
        use crate::domain::pad::NavAction;
        let mut message = None;
        self.cross_held = sample.cross;
        self.square_held = sample.square;
        self.triangle_held = sample.triangle;
        self.options_held = sample.options;
        self.create_held = sample.create;
        let nav = self.nav.update(sample, now, true, true);
        if let Some(action) = nav {
            message = match action {
                NavAction::Left => Some(HomeMessage::MoveX(-1)),
                NavAction::Right => Some(HomeMessage::MoveX(1)),
                NavAction::Up => Some(HomeMessage::MoveY(-1)),
                NavAction::Down => Some(HomeMessage::MoveY(1)),
                _ => None,
            };
        }
        // Triangle hold powers off the focused Bluetooth pad; vertical
        // movement or a foreign press cancels it (same discipline as the
        // Launcher dock row). Horizontal input is inert in the vertical dock,
        // so it must not cancel the hold either.
        let can_power = !self.settings_open
            && self.apps_info.is_none()
            && self.apps_menu.is_none()
            && self.focus == HomeFocus::Controllers
            && self.controller_selected().is_some_and(|row| {
                row.show_power_off() && self.powering_off.as_ref() != Some(&row.serial)
            });
        let foreign =
            sample.cross || sample.circle || sample.square || sample.options || sample.create;
        let nav_cancels_hold = nav.is_some_and(|action| {
            !(matches!(action, NavAction::Left | NavAction::Right)
                && self.focus == HomeFocus::Controllers)
        });
        if self.triangle_hold.is_active() && (nav_cancels_hold || foreign) {
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
            message = Some(HomeMessage::PowerOff);
        }

        tap.or(message)
    }

    /// True while the Triangle power hold is charging or its release decay /
    /// armed chase still needs frames. Gates the Home hold clock.
    pub fn hold_animating(&self, now: Instant) -> bool {
        self.triangle_hold.is_animating(now) || self.triangle_armed_t > 0.001
    }

    /// Pull the hold pose from wall time. Called by the Home frame clock while
    /// no pad edges are arriving, so release decay no longer freezes at the
    /// end of the 140ms neutral tail.
    pub fn advance_hold(&mut self, now: Instant) {
        let progress = self.triangle_hold.progress_at(now);
        self.triangle_progress = progress;
        let dt = self
            .hint_anim_at
            .map(|at| now.saturating_duration_since(at).as_secs_f32())
            .unwrap_or(0.0)
            .min(0.05);
        self.hint_anim_at = Some(now);
        approach_hint_anim(&mut self.triangle_armed_t, progress >= 1.0, dt);
    }
}

/// Render Home shell. The window root is transparent; the centered card
/// and top-right controllers dock are the only painted surfaces above the
/// full-screen scrim. `spectrum` colors the controller battery rings.
pub fn view<'a>(
    state: &'a State,
    spectrum: &'a crate::domain::color::BatterySpectrum,
    stage_w: f32,
    stage_h: f32,
    backdrop_alpha: f32,
    panel: PanelVisual,
    now: Instant,
) -> Element<'a, HomeMessage> {
    let fade = panel.alpha.clamp(0.0, 1.0);
    // Controller-first: the only mouse binding is a backdrop click, which
    // mirrors Circle. Card layers are shielded so only clicks outside the
    // active panel reach this backdrop.
    let mut layers: Vec<Element<'a, HomeMessage>> = vec![
        mouse_area(
            container(space())
                .width(Fill)
                .height(Fill)
                .style(theme::scrim(backdrop_alpha)),
        )
        .on_press(HomeMessage::Close)
        .interaction(iced::mouse::Interaction::Pointer)
        .into(),
    ];

    let card: Element<'a, HomeMessage> = if state.settings_open {
        settings_view(state, stage_w, fade)
    } else {
        let card_w = home_card_width(stage_w);
        let inner_w = (card_w - 2.0 * SECTION_PAD_X).max(1.0);
        let mut content = column![
            container(home_header(inner_w, fade))
                .padding([ROW_PAD_Y, SECTION_PAD_X])
                .width(Length::Fixed(card_w)),
            section_separator(card_w, fade),
            container(apps_row(state, inner_w, fade))
                .padding([ROW_PAD_Y, SECTION_PAD_X])
                .width(Length::Fixed(card_w)),
        ]
        .spacing(0);
        if let Some(error) = &state.install_error {
            content = content.push(
                container(
                    text(error.as_str())
                        .size(theme::type_scale::CAPTION)
                        .color(theme::scale_alpha(theme::WARNING, fade))
                        .width(Fill)
                        .wrapping(Wrapping::Word),
                )
                .padding([4.0, SECTION_PAD_X + 4.0])
                .width(Length::Fixed(card_w)),
            );
        }
        content = content.push(section_separator(card_w, fade)).push(
            container(action_hints_row(state, inner_w, fade))
                .padding([ROW_PAD_Y, SECTION_PAD_X])
                .width(Length::Fixed(card_w)),
        );
        container(content)
            .padding(0)
            .width(Length::Fixed(card_w))
            .style(move |theme| card_style(fade, theme))
            .into()
    };

    let modal_open = state.apps_info.is_some() || state.apps_menu.is_some();
    // While a modal card owns the screen, leave Home card unshielded so a
    // click anywhere outside the modal falls through to the backdrop (Circle
    // closes the modal, not Home).
    let card: Element<'a, HomeMessage> = if modal_open {
        card
    } else {
        crate::ui::mouse_shield::mouse_shield(card)
    };
    let card =
        crate::ui::scale::Scale::new(card, panel.scale).offset(Vector::new(0.0, panel.offset_y));
    layers.push(
        container(card)
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill)
            .into(),
    );

    if state.dock_visible() {
        layers.push(controllers_dock_layer(
            state, spectrum, stage_w, stage_h, now, fade,
        ));
    }
    if let Some(index) = state.apps_info {
        layers.push(app_info(state, index, fade));
    }
    if let Some(index) = state.apps_menu {
        layers.push(apps_menu(state, index, fade));
    }

    stack(layers).width(Fill).height(Fill).into()
}

/// Home card width for a monitor stage. The card itself is the outer surface;
/// only very narrow stages inset it.
pub fn home_card_width(stage_w: f32) -> f32 {
    (stage_w - 32.0).clamp(340.0, HOME_WIDTH)
}

fn section_separator<'a>(width: f32, fade: f32) -> Element<'a, HomeMessage> {
    container(space())
        .width(Length::Fixed(width))
        .height(Length::Fixed(1.0))
        .style(move |_theme| container::Style {
            background: Some(Background::Color(theme::scale_alpha(
                theme::alpha(theme::INK, 0.06),
                fade,
            ))),
            ..container::Style::default()
        })
        .into()
}

/// Branding-only header row (Options moved to the action-hint row).
fn home_header<'a>(width: f32, fade: f32) -> Element<'a, HomeMessage> {
    let brand = row![
        faded_svg(svg_icon::DUALSENSE_SVG, 24.0, theme::ACCENT, fade),
        text("SDSC Utils")
            .size(15.0)
            .font(iced::Font {
                weight: iced::font::Weight::Semibold,
                ..iced::Font::default()
            })
            .color(theme::scale_alpha(theme::INK, fade)),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    container(brand)
        .width(Length::Fixed(width))
        .align_x(Alignment::Start)
        .into()
}

/// Launcher / Browser tiles.
fn apps_row<'a>(state: &State, inner_w: f32, fade: f32) -> Element<'a, HomeMessage> {
    let gaps = (TILES.len().saturating_sub(1)) as f32 * TILE_GAP;
    let tile_w = ((inner_w - gaps) / TILES.len() as f32).max(80.0);
    let mut tiles = row![].spacing(TILE_GAP).align_y(Alignment::Center);
    for (index, tile) in TILES.into_iter().enumerate() {
        tiles = tiles.push(tile_button(state, tile, index, tile_w, fade));
    }
    tiles.width(Length::Fixed(inner_w)).into()
}

/// Third row: Select/Share controllers + Options settings. Display-only hints;
/// the pad owns the actions.
fn action_hints_row<'a>(state: &State, inner_w: f32, fade: f32) -> Element<'a, HomeMessage> {
    let controllers = container(capsule_hint(
        "SELECT",
        "Controllers",
        state.create_held,
        fade,
    ))
    .padding([2, 4]);
    let settings =
        container(capsule_hint("START", "Settings", state.options_held, fade)).padding([2, 4]);
    row![controllers, space().width(Fill), settings]
        .width(Length::Fixed(inner_w))
        .align_y(Alignment::Center)
        .into()
}

fn faded_svg<'a>(
    icon: &'static str,
    size: f32,
    color: Color,
    fade: f32,
) -> Element<'a, HomeMessage> {
    // iced's SVG colour filter rewrites RGB only; alpha must be applied as
    // draw opacity so icons fade with the rest of Home shell.
    svg(svg::Handle::from_memory(icon.as_bytes()))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .opacity(fade.clamp(0.0, 1.0))
        .style(move |_theme, _status| svg::Style { color: Some(color) })
        .into()
}

fn capsule_hint(
    capsule: &'static str,
    label: &'static str,
    pressed: bool,
    fade: f32,
) -> Element<'static, HomeMessage> {
    let capsule = text(capsule)
        .size(12.0)
        .font(iced::Font::MONOSPACE)
        .color(theme::scale_alpha(theme::ACCENT, fade));
    let glyph = hint_text_capsule(capsule, 10.0, 1.0, pressed, 0.0, true, fade);
    row![
        glyph,
        text(label)
            .size(14.0)
            .color(theme::scale_alpha(theme::MUTED, fade))
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

/// Top-right dock layer: anchored at the monitor's top-right, above the scrim.
fn controllers_dock_layer<'a>(
    state: &'a State,
    spectrum: &'a crate::domain::color::BatterySpectrum,
    _stage_w: f32,
    stage_h: f32,
    now: Instant,
    fade: f32,
) -> Element<'a, HomeMessage> {
    let dock = crate::ui::mouse_shield::mouse_shield(controllers_dock(
        state, spectrum, stage_h, now, fade,
    ));
    row![
        space().width(Fill),
        column![dock, space().height(Fill)]
            .height(Fill)
            .padding(Padding {
                top: DOCK_TOP,
                right: DOCK_RIGHT,
                bottom: 0.0,
                left: 0.0,
            }),
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

/// Restored Launcher dock: collapsed peek ring column → expanded vertical rows.
fn controllers_dock<'a>(
    state: &'a State,
    spectrum: &'a crate::domain::color::BatterySpectrum,
    _stage_h: f32,
    now: Instant,
    fade: f32,
) -> Element<'a, HomeMessage> {
    let t = state.dock_progress(now).clamp(0.0, 1.0);
    let ring = DOCK_RING_MIN + (DOCK_RING_MAX - DOCK_RING_MIN) * t;
    let row_h = ring + (DOCK_ROW_H - ring) * t;
    let visible_w = DOCK_PEEK_W + (DOCK_MAX_W - DOCK_PEEK_W) * t;
    let pad = (DOCK_PEEK_W - DOCK_RING_MIN) * 0.5;
    let pad_x = pad * t;
    let row_gap = pad + (DOCK_ROW_GAP - pad) * t;
    let peek_radius = DOCK_PEEK_W * 0.5;
    let expanded_island = pad + theme::radius::MD;
    let island_radius = peek_radius + (expanded_island - peek_radius) * t;
    let row_radius = (island_radius - pad).max(0.0);

    let list: Element<'a, HomeMessage> =
        keyed_column(state.controllers.iter().enumerate().map(|(i, row)| {
            (
                dock_row_key(row.serial.as_ref()),
                dock_controller_row(state, spectrum, row, i, ring, row_h, row_radius, fade, t),
            )
        }))
        .spacing(row_gap)
        .width(Length::Fixed(DOCK_MAX_W))
        .into();

    // Old Launcher footer: Square cycles Connected / All.
    let reveal_t = ((t - 0.2) / 0.8).clamp(0.0, 1.0);
    let footer: Element<'a, HomeMessage> = if reveal_t > 0.001 {
        let bar = container(dock_filter(state, fade))
            .height(Length::Fixed(32.0))
            .align_y(Alignment::Center);
        crate::ui::reveal::height_reveal(bar, 32.0, 32.0 * reveal_t)
            .width(Fill)
            .into()
    } else {
        space()
            .width(Length::Fixed(0.0))
            .height(Length::Fixed(0.0))
            .into()
    };

    let full = container(
        column![list, footer]
            .spacing(if reveal_t > 0.001 { 8.0 } else { 0.0 })
            .width(Length::Fixed(DOCK_MAX_W)),
    )
    .padding(Padding {
        top: pad,
        right: 0.0,
        bottom: pad,
        left: 0.0,
    })
    .width(Length::Fixed(DOCK_MAX_W));

    let revealed =
        crate::ui::reveal::width_reveal(full, DOCK_MAX_W, visible_w).height(Length::Shrink);
    container(revealed)
        .padding(Padding {
            top: 0.0,
            right: pad_x,
            bottom: 0.0,
            left: pad_x,
        })
        .style(move |_theme| container::Style {
            background: Some(Background::Color(theme::scale_alpha(
                theme::GLASS_FILL,
                fade,
            ))),
            text_color: Some(theme::scale_alpha(theme::INK, fade)),
            border: Border {
                color: theme::scale_alpha(theme::GLASS_HAIRLINE, fade),
                width: 1.0,
                radius: island_radius.into(),
            },
            ..container::Style::default()
        })
        .clip(true)
        .into()
}

/// Old Launcher dock footer: Square cycles Connected / All with the active value
/// in ink and the inactive value dimmed.
fn dock_filter<'a>(state: &State, fade: f32) -> Element<'a, HomeMessage> {
    let glyph = face_glyph(
        FaceButton::Square,
        if state.square_held {
            RingStyle::Pressed
        } else {
            RingStyle::Idle
        },
        0.0,
        true,
        fade,
    );
    let label = |value: &'static str, active: bool| {
        text(value).size(14.0).color(theme::scale_alpha(
            if active {
                theme::INK
            } else {
                theme::alpha(theme::MUTED, 0.55)
            },
            fade,
        ))
    };
    row![
        glyph,
        label("Connected", !state.show_all_controllers),
        text("·")
            .size(14.0)
            .color(theme::scale_alpha(theme::alpha(theme::MUTED, 0.45), fade)),
        label("All", state.show_all_controllers),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn dock_row_key(serial: &str) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    serial.hash(&mut hasher);
    hasher.finish()
}

#[allow(clippy::too_many_arguments)]
fn dock_controller_row<'a>(
    state: &'a State,
    spectrum: &'a crate::domain::color::BatterySpectrum,
    row: &'a ControllerRow,
    index: usize,
    ring_size: f32,
    row_h: f32,
    row_radius: f32,
    fade: f32,
    dock_t: f32,
) -> Element<'a, HomeMessage> {
    let powering_off = row.connected && state.powering_off.as_ref() == Some(&row.serial);
    let live = row.connected && !powering_off;
    let ring_color = if state.ring_flash_white(&row.serial) {
        theme::from_rgb(lightbar::IDENTIFY_FLASH)
    } else if live {
        theme::from_rgb(spectrum.color_at_percent(row.percent))
    } else {
        theme::DIM
    };
    let eta_t = ((dock_t - 0.2) / 0.8).clamp(0.0, 1.0);
    let ring = crate::ui::percent_ring::percent_ring(
        row.percent,
        ring_color,
        ring_size,
        row.eta.clone(),
        eta_t,
        if live { fade } else { 0.6 * fade },
    );
    let ring_cell = container(ring)
        .width(Length::Fixed(DOCK_PEEK_W))
        .height(Length::Fixed(row_h))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center);

    let title_color = if live { theme::INK } else { theme::MUTED };
    let meta_color = if powering_off {
        theme::MUTED
    } else if row.low {
        theme::WARNING
    } else {
        theme::MUTED
    };
    let meta = if powering_off {
        "Powering off".to_string()
    } else if !row.connected {
        "Disconnected".to_string()
    } else {
        format!("{} · {}", row.connection, row.state)
    };
    let selected = index
        == state
            .controller_focus
            .min(state.controllers.len().saturating_sub(1))
        && state.dock_expanded();

    let hints: Element<'a, HomeMessage> = if selected && !powering_off {
        let mut actions = Vec::new();
        if row.show_identify() {
            let mut hint = plain_hint(FaceButton::Cross, "Identify", state.cross_held);
            hint.alpha = fade;
            actions.push(hint);
        }
        if row.show_power_off() {
            let mut hint = hold_hint(
                FaceButton::Triangle,
                "Power off",
                state.triangle_progress,
                state.triangle_armed_t,
                state.triangle_held,
            );
            hint.alpha = fade;
            actions.push(hint);
        }
        container(action_cluster_spaced::<HomeMessage>(&actions, 10.0, true))
            .width(Length::Fixed(DOCK_HINT_COL_W))
            .height(Length::Fixed(row_h))
            .center_y(Fill)
            .align_x(Alignment::End)
            .clip(true)
            .into()
    } else {
        space()
            .width(Length::Fixed(DOCK_HINT_COL_W))
            .height(Length::Fixed(row_h))
            .into()
    };

    let titles_w = (DOCK_MAX_W - DOCK_PEEK_W - DOCK_HINT_COL_W - 8.0).max(80.0);
    let titles = container(
        column![
            text(text_fit::ellipsize_to_width(
                row.display_name(),
                18.0,
                titles_w
            ))
            .size(18.0)
            .color(theme::scale_alpha(title_color, fade))
            .font(iced::Font {
                weight: if selected {
                    iced::font::Weight::Bold
                } else {
                    iced::font::Weight::Normal
                },
                ..iced::Font::default()
            }),
            text(text_fit::ellipsize_to_width(&meta, 13.0, titles_w))
                .size(13.0)
                .color(theme::scale_alpha(meta_color, fade)),
        ]
        .spacing(3)
        .width(Fill),
    )
    .width(Fill)
    .height(Length::Fixed(row_h))
    .center_y(Fill);

    let details = container(
        row![titles, hints]
            .spacing(8)
            .width(Fill)
            .height(Length::Fixed(row_h))
            .align_y(Alignment::Center),
    )
    .width(Fill)
    .height(Length::Fixed(row_h))
    .padding(Padding {
        top: 0.0,
        right: 16.0,
        bottom: 0.0,
        left: 4.0,
    })
    .clip(true);

    let body = row![ring_cell, details]
        .width(Length::Fixed(DOCK_MAX_W))
        .height(Length::Fixed(row_h))
        .align_y(Alignment::Center);

    let selection_t = state.controller_focus_t(index);
    container(body)
        .width(Length::Fixed(DOCK_MAX_W))
        .height(Length::Fixed(row_h))
        .clip(true)
        .style(move |theme| dock_row_style(selected, selection_t, row_radius, fade, theme))
        .into()
}

/// Move a 0..=1 animation value toward its target at a fixed duration.
fn approach_anim_to(current: &mut f32, target: f32, dt: f32, duration_secs: f32) {
    let target = target.clamp(0.0, 1.0);
    let step = dt / duration_secs.max(f32::EPSILON);
    if *current < target {
        *current = (*current + step).min(target);
    } else if *current > target {
        *current = (*current - step).max(target);
    }
}

/// ~100ms accent → ink chase mirroring the Launcher's armed hold-hint animation.
fn approach_hint_anim(current: &mut f32, target: bool, dt: f32) {
    const HINT_ANIM_SECS: f32 = crate::ui::motion::FAST_MS as f32 / 1000.0;
    approach_anim_to(current, if target { 1.0 } else { 0.0 }, dt, HINT_ANIM_SECS);
}

/// Controller dock row: transparent at rest, selection wash crossfades with
/// `selection_t`; there is no hover state (mouse is ignored).
fn dock_row_style(
    selected: bool,
    selection_t: f32,
    radius: f32,
    fade: f32,
    _theme: &Theme,
) -> container::Style {
    let wash = selected.then(|| {
        theme::scale_alpha(
            theme::alpha(theme::INK, 0.05 * selection_t.clamp(0.0, 1.0)),
            fade,
        )
    });
    container::Style {
        background: wash.map(Background::Color),
        text_color: Some(theme::scale_alpha(theme::INK, fade)),
        border: Border {
            radius: radius.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// Stable id for the pad-scrollable changelog in Info.
pub fn info_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("home-info-changelog")
}

fn settings_view<'a>(state: &'a State, stage_w: f32, fade: f32) -> Element<'a, HomeMessage> {
    let card_w = (stage_w - 32.0).clamp(340.0, HOME_WIDTH);
    let mut rows = column![].spacing(6).width(Fill);
    for (i, label) in SETTINGS_ROWS.iter().enumerate() {
        if !state.settings_visible(i) {
            continue;
        }
        match i {
            SETTINGS_AUTO_OPEN => {
                rows = rows.push(settings_section("Home", fade));
            }
            SETTINGS_SOUNDS => {
                rows = rows.push(settings_section("Feedback", fade));
            }
            SETTINGS_DOWNLOAD => {
                rows = rows.push(space().height(Length::Fixed(16.0)));
                rows = rows.push(settings_section("App installs", fade));
            }
            _ => {}
        }
        let focused = i == state.settings_focus;
        let row = match i {
            SETTINGS_AUTO_OPEN => {
                settings_selector_row(label, None, &state.settings_value(i), focused, fade)
            }
            SETTINGS_SOUNDS => {
                settings_toggle_row("UI sounds", None, state.sound_enabled, focused, fade)
            }
            SETTINGS_VOLUME => settings_slider_row(
                &format!("Volume {}%", state.sound_volume),
                None,
                state.sound_volume,
                focused,
                fade,
            ),
            SETTINGS_HAPTICS => settings_toggle_row(
                "Controller haptics",
                None,
                state.haptics_enabled,
                focused,
                fade,
            ),
            SETTINGS_STRENGTH => settings_slider_row(
                &format!("Strength {}%", state.haptics_strength),
                None,
                state.haptics_strength,
                focused,
                fade,
            ),
            SETTINGS_DOWNLOAD => settings_selector_row(
                label,
                Some("Applies to apps installed outside the Store"),
                &state.settings_value(i),
                focused,
                fade,
            ),
            _ => continue,
        };
        rows = rows.push(row);
        if state
            .settings_menu
            .is_some_and(|(open_row, _)| open_row == i)
        {
            rows = rows.push(settings_menu_panel(state, i, fade));
        }
    }
    container(rows)
        .padding(16)
        .width(Length::Fixed(card_w))
        .style(move |theme| card_style(fade, theme))
        .into()
}

/// Expanded dropdown panel for a selector row: same ✓/○ option language as
/// the Launcher settings drawer, rendered beneath its row.
fn settings_menu_panel<'a>(state: &State, row: usize, fade: f32) -> Element<'a, HomeMessage> {
    let highlight = state.settings_menu_highlight().unwrap_or(0);
    let mut options = column![].spacing(4).width(Fill);
    for i in 0..State::settings_option_count(row) {
        options = options.push(settings_option_row(
            &state.settings_option_label(row, i),
            state.settings_option_selected(row, i),
            i == highlight,
            fade,
        ));
    }
    container(options)
        .padding(6)
        .width(Fill)
        .style(theme::menu_panel)
        .into()
}

fn settings_option_row<'a>(
    label: &str,
    selected: bool,
    highlighted: bool,
    fade: f32,
) -> Element<'a, HomeMessage> {
    let (mark, mark_color) = if selected {
        ("✓", theme::ACCENT)
    } else {
        ("○", theme::alpha(theme::MUTED, 0.55))
    };
    container(
        row![
            text(mark)
                .size(14.0)
                .color(theme::scale_alpha(mark_color, fade))
                .width(Length::Fixed(20.0)),
            text(label.to_string())
                .size(14.0)
                .color(theme::scale_alpha(theme::INK, fade))
                .width(Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding(8)
    .width(Fill)
    .style(settings_focus_style(highlighted, fade))
    .into()
}

fn settings_section<'a>(label: &'static str, fade: f32) -> Element<'a, HomeMessage> {
    column![
        text(label)
            .size(15.0)
            .font(iced::Font {
                weight: iced::font::Weight::Semibold,
                ..iced::Font::default()
            })
            .color(theme::scale_alpha(theme::DIM, fade)),
        container(space().height(Length::Fixed(1.0)))
            .width(Fill)
            .style(move |_theme| container::Style {
                background: Some(Background::Color(theme::scale_alpha(theme::LINE, fade))),
                ..container::Style::default()
            }),
    ]
    .spacing(6)
    .width(Fill)
    .into()
}

fn settings_focus_style(focused: bool, fade: f32) -> impl Fn(&Theme) -> container::Style + 'static {
    move |_theme| {
        if focused {
            container::Style {
                background: Some(Background::Color(theme::scale_alpha(
                    theme::alpha(theme::ACCENT, 0.12),
                    fade,
                ))),
                border: Border {
                    color: theme::scale_alpha(theme::alpha(theme::ACCENT, 0.55), fade),
                    width: 1.0,
                    radius: 8.0.into(),
                },
                ..container::Style::default()
            }
        } else {
            container::Style::default()
        }
    }
}

fn settings_row_labels<'a>(
    label: impl Into<String>,
    helper: Option<&'static str>,
    size: f32,
    fade: f32,
) -> Element<'a, HomeMessage> {
    let mut col = column![
        text(label.into())
            .size(size)
            .color(theme::scale_alpha(theme::INK, fade))
    ]
    .spacing(2)
    .width(Fill);
    if let Some(helper) = helper {
        col = col.push(
            text(helper)
                .size(14.0)
                .color(theme::scale_alpha(theme::DIM, fade))
                .wrapping(Wrapping::None),
        );
    }
    col.into()
}

fn settings_toggle<'a>(on: bool, fade: f32) -> Element<'a, HomeMessage> {
    toggler(on)
        .size(22.0)
        .style(move |_theme, _status| {
            let (track, knob) = if on {
                (theme::ACCENT, theme::INK)
            } else {
                (theme::alpha(theme::INK, 0.14), theme::MUTED)
            };
            iced::widget::toggler::Style {
                background: Background::Color(theme::scale_alpha(track, fade)),
                background_border_width: 0.0,
                background_border_color: Color::TRANSPARENT,
                foreground: Background::Color(theme::scale_alpha(knob, fade)),
                foreground_border_width: 0.0,
                foreground_border_color: Color::TRANSPARENT,
                text_color: Some(theme::scale_alpha(theme::INK, fade)),
                border_radius: None,
                padding_ratio: 0.1,
            }
        })
        .into()
}

fn settings_slider<'a>(percent: u8, fade: f32) -> Element<'a, HomeMessage> {
    let percent = percent.min(100);
    let p = u16::from(percent);
    let track = progress_bar(0.0..=1.0, f32::from(percent) / 100.0)
        .girth(4.0)
        .style(move |_theme| progress_bar::Style {
            background: Background::Color(theme::scale_alpha(theme::alpha(theme::INK, 0.14), fade)),
            bar: Background::Color(theme::scale_alpha(theme::ACCENT, fade)),
            border: Border {
                radius: theme::radius::PILL.into(),
                ..Border::default()
            },
        });
    let knob = container(space())
        .width(Length::Fixed(16.0))
        .height(Length::Fixed(16.0))
        .style(move |_theme| container::Style {
            background: Some(Background::Color(theme::scale_alpha(theme::INK, fade))),
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 8.0.into(),
            },
            ..container::Style::default()
        });
    stack![
        container(track)
            .width(Fill)
            .height(Length::Fixed(16.0))
            .align_y(Alignment::Center),
        row![
            space().width(Length::FillPortion(p)),
            knob,
            space().width(Length::FillPortion(100u16.saturating_sub(p))),
        ]
        .width(Fill)
        .height(Length::Fixed(16.0))
        .align_y(Alignment::Center),
    ]
    .width(Fill)
    .height(Length::Fixed(16.0))
    .into()
}

fn settings_toggle_row<'a>(
    label: &'static str,
    helper: Option<&'static str>,
    on: bool,
    focused: bool,
    fade: f32,
) -> Element<'a, HomeMessage> {
    container(
        row![
            settings_row_labels(label, helper, 16.0, fade),
            settings_toggle(on, fade),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding(10)
    .width(Fill)
    .style(settings_focus_style(focused, fade))
    .into()
}

fn settings_slider_row<'a>(
    label: &str,
    helper: Option<&'static str>,
    percent: u8,
    focused: bool,
    fade: f32,
) -> Element<'a, HomeMessage> {
    let mut content = column![
        text(label.to_string())
            .size(14.0)
            .color(theme::scale_alpha(theme::MUTED, fade)),
        settings_slider(percent, fade),
    ]
    .spacing(4)
    .width(Fill);
    if let Some(helper) = helper {
        content = content.push(
            text(helper)
                .size(14.0)
                .color(theme::scale_alpha(theme::DIM, fade))
                .wrapping(Wrapping::None),
        );
    }
    container(content)
        .padding(10)
        .width(Fill)
        .style(settings_focus_style(focused, fade))
        .into()
}

fn settings_selector_row<'a>(
    label: &'static str,
    helper: Option<&'static str>,
    value: &str,
    focused: bool,
    fade: f32,
) -> Element<'a, HomeMessage> {
    container(
        row![
            settings_row_labels(label, helper, 16.0, fade),
            text(value.to_string())
                .size(14.0)
                .color(theme::scale_alpha(theme::INK, fade)),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding(10)
    .width(Fill)
    .style(settings_focus_style(focused, fade))
    .into()
}

fn progress_style(fade: f32, _theme: &Theme) -> progress_bar::Style {
    progress_bar::Style {
        background: Background::Color(theme::scale_alpha(theme::alpha(theme::INK, 0.10), fade)),
        bar: Background::Color(theme::scale_alpha(theme::ACCENT, fade)),
        border: Border {
            radius: theme::radius::PILL.into(),
            ..Border::default()
        },
    }
}

/// Busy tile body: icon, title, one-word phase, and a bar inside the card.
#[allow(clippy::too_many_arguments)]
fn progress_tile_body<'a>(
    state: &State,
    icon: &'static str,
    title: &'static str,
    width: f32,
    icon_size: f32,
    title_size: f32,
    focused: bool,
    fade: f32,
) -> Element<'a, HomeMessage> {
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
            .color(theme::scale_alpha(theme::MUTED, fade)),
    ]
    .spacing(4)
    .align_y(Alignment::Center);
    if let Some(rate) = state.progress.rate_label() {
        phase = phase.push(
            text(rate)
                .size(theme::type_scale::CAPTION)
                .color(theme::scale_alpha(theme::DIM, fade)),
        );
    }
    let mut body = column![
        faded_svg(icon, icon_size, ink, fade),
        text(text_fit::ellipsize_to_width(
            title,
            title_size,
            width - 12.0
        ))
        .size(title_size)
        .font(font)
        .color(theme::scale_alpha(ink, fade)),
        phase,
    ];
    if let Some(size) = state.progress.size_label() {
        body = body.push(
            text(size)
                .size(theme::type_scale::CAPTION)
                .color(theme::scale_alpha(theme::MUTED, fade)),
        );
    }
    body.push(
        progress_bar(0.0..=1.0, state.progress_bar_value())
            .length(Length::Fixed((width - 40.0).max(80.0)))
            .girth(3.0)
            .style(move |theme| progress_style(fade, theme)),
    )
    .spacing(2)
    .align_x(Alignment::Center)
    .into()
}

fn tile_button<'a>(
    state: &State,
    tile: HomeTile,
    index: usize,
    width: f32,
    fade: f32,
) -> Element<'a, HomeMessage> {
    let selected = state.focus == HomeFocus::Apps && state.selected() == index;
    let interactive = state.tile_enabled(tile);
    let dim = match tile {
        HomeTile::Launcher => !state.launcher_enabled,
        HomeTile::Browser => !state.engine_ready,
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
    let normal: Element<'a, HomeMessage> = column![
        faded_svg(icon, ICON, ink, fade),
        text(text_fit::ellipsize_to_width(
            title,
            TILE_TITLE,
            width - 16.0
        ))
        .size(TILE_TITLE)
        .font(title_font)
        .color(theme::scale_alpha(ink, fade)),
    ]
    .spacing(4)
    .align_x(Alignment::Center)
    .into();
    let content = if matches!(tile, HomeTile::Browser) && state.busy() {
        progress_tile_body(state, icon, title, width, 22.0, TILE_TITLE, selected, fade)
    } else if matches!(tile, HomeTile::Browser) && !state.engine_ready {
        stack![
            normal,
            container(faded_svg(
                svg_icon::DOWNLOAD_SVG,
                14.0,
                theme::WARNING,
                fade,
            ))
            .width(Fill)
            .height(Fill)
            .align_x(Alignment::End)
            .align_y(Alignment::Start),
        ]
        .into()
    } else if matches!(tile, HomeTile::Browser) && state.engine_restart_required {
        stack![
            normal,
            container(
                text("Restart")
                    .size(theme::type_scale::CAPTION)
                    .color(theme::scale_alpha(theme::WARNING, fade)),
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
    container(content)
        .width(Length::Fixed(width))
        .height(Length::Fixed(TILE_H))
        .padding(4)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(move |theme| tile_style(selected, interactive, dim, fade, theme))
        .into()
}

fn apps_menu<'a>(state: &State, index: usize, fade: f32) -> Element<'a, HomeMessage> {
    let app = APPS[index];
    let mut rows = column![].spacing(4);
    for (i, action) in state.app_actions(index).into_iter().enumerate() {
        let focused = i == state.apps_menu_focus;
        rows = rows.push(
            container(
                row![
                    faded_svg(action.icon(), 18.0, theme::INK, fade),
                    text(action.label())
                        .size(14.0)
                        .color(theme::scale_alpha(theme::INK, fade)),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            )
            .width(Fill)
            .padding([8, 10])
            .style(move |theme| menu_row_style(focused, fade, theme)),
        );
    }
    let card = container(
        column![
            row![
                faded_svg(app.icon(), 22.0, theme::INK, fade),
                text(app.title())
                    .size(16.0)
                    .color(theme::scale_alpha(theme::INK, fade)),
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
    .style(move |theme| card_style(fade, theme));
    let card = crate::ui::mouse_shield::mouse_shield(card);
    container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .into()
}

fn app_info<'a>(state: &'a State, index: usize, fade: f32) -> Element<'a, HomeMessage> {
    let app = APPS[index];
    let dim = state.app_dimmed(app);
    let ink = if dim { theme::DIM } else { theme::INK };
    let ink = theme::scale_alpha(ink, fade);
    let muted = theme::scale_alpha(theme::MUTED, fade);
    let dimc = theme::scale_alpha(theme::DIM, fade);
    let warning = theme::scale_alpha(theme::WARNING, fade);
    let mut details = column![].spacing(6);

    if let Some(version) = &state.engine_version {
        details = details.push(
            text(format!("Browser engine {version}"))
                .size(theme::type_scale::CAPTION)
                .color(muted),
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
        details = details.push(text(live).size(theme::type_scale::CAPTION).color(warning));
    }
    if state.engine_managed {
        details = details.push(
            text("Managed by the Microsoft Store")
                .size(theme::type_scale::CAPTION)
                .color(muted),
        );
    }

    if state.engine_update_ready
        && let Some(latest) = &state.engine_latest_version
    {
        details = details.push(
            text(format!("Update available: {latest}"))
                .size(theme::type_scale::CAPTION)
                .color(warning),
        );
    } else if state.engine_version.is_none()
        && let Some(latest) = &state.engine_latest_version
    {
        details = details.push(
            text(format!("Latest: {latest}"))
                .size(theme::type_scale::CAPTION)
                .color(muted),
        );
    }

    if let Some(size) = state.engine_size.filter(|size| *size > 0) {
        details = details.push(
            text(format!("Size on disk: {}", format_size(size)))
                .size(theme::type_scale::CAPTION)
                .color(muted),
        );
    }
    if state.engine_pending_size > 0 {
        details = details.push(
            text(format!(
                "Pending cleanup: {}",
                format_size(state.engine_pending_size)
            ))
            .size(theme::type_scale::CAPTION)
            .color(dimc),
        );
    }
    if state.engine_restart_required {
        details = details.push(
            text("Restart SDSC Utils to finish the update.")
                .size(theme::type_scale::CAPTION)
                .color(warning),
        );
    }
    if let Some(error) = &state.install_error {
        details = details.push(
            text(error.as_str())
                .size(theme::type_scale::CAPTION)
                .color(warning)
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
                    .color(theme::scale_alpha(theme::INK, fade)),
            )
            .push(
                scrollable(
                    text(changelog)
                        .size(theme::type_scale::CAPTION)
                        .color(muted)
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
                faded_svg(app.icon(), 44.0, ink, fade),
                column![
                    text(app.title())
                        .size(theme::type_scale::HEADING)
                        .color(theme::scale_alpha(theme::INK, fade)),
                    text(state.app_status(app))
                        .size(theme::type_scale::META)
                        .color(muted),
                ]
                .spacing(2),
                space().width(Fill),
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
    .style(move |theme| card_style(fade, theme));
    let card = crate::ui::mouse_shield::mouse_shield(card);
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

fn tile_copy(tile: HomeTile) -> (&'static str, &'static str) {
    match tile {
        HomeTile::Launcher => (svg_icon::GAME_SVG, "Launcher"),
        HomeTile::Browser => (svg_icon::GLOBE_SVG, "Browser"),
    }
}

fn menu_row_style(focused: bool, fade: f32, _theme: &Theme) -> container::Style {
    let fill = if focused {
        theme::alpha(theme::INK, 0.12)
    } else {
        theme::alpha(theme::INK, 0.02)
    };
    container::Style {
        background: Some(Background::Color(theme::scale_alpha(fill, fade))),
        text_color: Some(theme::scale_alpha(theme::INK, fade)),
        border: Border {
            color: if focused {
                theme::scale_alpha(theme::alpha(theme::INK, 0.28), fade)
            } else {
                Color::TRANSPARENT
            },
            width: 1.0,
            radius: theme::radius::MD.into(),
        },
        ..container::Style::default()
    }
}

fn tile_style(
    selected: bool,
    interactive: bool,
    dim: bool,
    fade: f32,
    _theme: &Theme,
) -> container::Style {
    let (fill, border) = if !interactive {
        (
            theme::alpha(theme::INK, 0.02),
            theme::alpha(theme::LINE, 0.4),
        )
    } else if selected {
        (
            theme::alpha(theme::INK, 0.10),
            theme::alpha(theme::INK, 0.34),
        )
    } else if dim {
        (
            theme::alpha(theme::INK, 0.02),
            theme::alpha(theme::LINE, 0.35),
        )
    } else {
        (theme::alpha(theme::INK, 0.04), theme::GLASS_HAIRLINE)
    };
    container::Style {
        background: Some(Background::Color(theme::scale_alpha(fill, fade))),
        border: Border {
            color: theme::scale_alpha(border, fade),
            width: 1.0,
            radius: theme::radius::LG.into(),
        },
        shadow: if selected {
            Shadow {
                color: theme::scale_alpha(theme::alpha(theme::INK, 0.10), fade),
                offset: Vector::new(0.0, 0.0),
                blur_radius: 18.0,
            }
        } else {
            Shadow::default()
        },
        ..container::Style::default()
    }
}

fn card_style(fade: f32, _theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(theme::scale_alpha(
            theme::alpha(theme::BASE_BG, 0.94),
            fade,
        ))),
        text_color: Some(theme::scale_alpha(theme::INK, fade)),
        border: Border {
            color: theme::scale_alpha(theme::GLASS_HAIRLINE, fade),
            width: 1.0,
            radius: theme::radius::XL.into(),
        },
        shadow: Shadow {
            color: theme::scale_alpha(Color::BLACK, fade),
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
    fn card_width_hits_nominal_on_wide_stages() {
        assert_eq!(home_card_width(1920.0), HOME_WIDTH);
        assert_eq!(home_card_width(400.0), 368.0);
    }

    #[test]
    fn selection_wraps_both_ways() {
        let mut state = State::default();
        assert_eq!(state.selected_tile(), HomeTile::Launcher);
        assert!(state.move_x(1));
        assert_eq!(state.selected_tile(), HomeTile::Browser);
        assert!(state.move_x(1));
        assert_eq!(state.selected_tile(), HomeTile::Launcher);
        assert!(state.move_x(-1));
        assert_eq!(state.selected_tile(), HomeTile::Browser);
        // The apps row is horizontal: vertical input is inert.
        assert!(!state.move_y(1));
        assert_eq!(state.selected_tile(), HomeTile::Browser);
    }

    #[test]
    fn select_ignores_out_of_range() {
        let mut state = State::default();
        let now = Instant::now();
        state.select(1, now);
        assert_eq!(state.selected_tile(), HomeTile::Browser);
        state.select(99, now);
        assert_eq!(state.selected_tile(), HomeTile::Browser);
    }

    #[test]
    fn create_toggles_dock_and_moves_selection() {
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
        let now = Instant::now();
        assert_eq!(state.focus, HomeFocus::Apps);
        assert!(state.toggle_dock(now));
        assert_eq!(state.focus, HomeFocus::Controllers);
        // The dock is vertical: horizontal input must not move it.
        assert!(!state.move_x(1));
        assert_eq!(
            state.controller_selected().map(|r| r.serial.as_ref()),
            Some("abc")
        );
        assert!(state.move_y(1));
        assert_eq!(
            state.controller_selected().map(|r| r.serial.as_ref()),
            Some("def")
        );
        assert!(state.move_y(1));
        assert_eq!(
            state.controller_selected().map(|r| r.serial.as_ref()),
            Some("abc")
        );
        assert!(state.collapse_dock(now));
        assert_eq!(state.focus, HomeFocus::Apps);
    }

    #[test]
    fn empty_dock_cannot_expand() {
        let mut state = State::default();
        assert!(!state.toggle_dock(Instant::now()));
        assert_eq!(state.focus, HomeFocus::Apps);
    }

    #[test]
    fn disabled_start_tile_cannot_be_activated() {
        let mut state = State::default();
        state.on_open(false, true);
        assert!(!state.tile_enabled(HomeTile::Launcher));
        assert!(state.tile_enabled(HomeTile::Browser));
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
        assert!(state.app_dimmed(ManagedApp::Browser));
        state.engine_ready = true;
        assert_eq!(
            state.app_actions(0),
            vec![AppAction::Uninstall, AppAction::Info]
        );
        assert!(!state.app_dimmed(ManagedApp::Browser));
    }

    #[test]
    fn install_and_uninstall_progress_is_visible_in_status() {
        let mut state = State::default();
        state.set_installing(true);
        assert_eq!(state.app_status(ManagedApp::Browser), "Preparing");
        state.set_install_progress(EngineProgress {
            phase: EnginePhase::Downloading,
            fraction: Some(0.42),
            bytes_done: 82 * 1024 * 1024,
            bytes_total: Some(196 * 1024 * 1024),
            rate_bps: Some(8.2 * 1024.0 * 1024.0),
        });
        assert_eq!(state.app_status(ManagedApp::Browser), "Downloading");
        assert_eq!(state.progress.label(), "Downloading");
        assert_eq!(
            state.progress.size_label().as_deref(),
            Some("82 MB / 196 MB")
        );
        state.install_done(true, None);
        assert!(!state.installing);
        assert_eq!(state.app_status(ManagedApp::Browser), "Installed");
        state.set_uninstalling();
        assert_eq!(state.app_status(ManagedApp::Browser), "Uninstalling");
        assert!(state.busy());
        state.advance_progress_anim();
        assert!(state.progress_bar_value() > 0.0);
        state.uninstall_done(None);
        assert_eq!(state.app_status(ManagedApp::Browser), "Not installed");
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
        assert_eq!(state.app_status(ManagedApp::Browser), "Update ready");
        state.engine_managed = true;
        assert_eq!(state.app_actions(0), vec![AppAction::Info]);
        assert_eq!(state.app_status(ManagedApp::Browser), "Managed by Store");
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
        assert_eq!(state.app_status(ManagedApp::Browser), "Cancelling");
        assert_eq!(state.app_actions(0), vec![AppAction::Info]);
        state.install_cancelled(false);
        assert!(!state.busy());
        assert_eq!(state.app_status(ManagedApp::Browser), "Not installed");
    }

    #[test]
    fn settings_rows_cycle_and_report_values() {
        let mut state = State::default();
        state.set_settings(true, 70, false, 40, 5 * 1024, HomeAutoOpen::Bluetooth);
        assert_eq!(
            state.settings_value(SETTINGS_AUTO_OPEN),
            "When a Bluetooth controller connects"
        );
        assert_eq!(state.settings_value(SETTINGS_SOUNDS), "On");
        assert_eq!(state.settings_value(SETTINGS_VOLUME), "70%");
        assert_eq!(state.settings_value(SETTINGS_HAPTICS), "Off");
        assert_eq!(state.settings_value(SETTINGS_STRENGTH), "40%");
        assert_eq!(state.settings_value(SETTINGS_DOWNLOAD), "5 MB/s");
        assert!(state.settings_move(1));
        assert_eq!(state.settings_focus, SETTINGS_SOUNDS);
        assert!(state.settings_move(-1));
        assert_eq!(state.settings_focus, SETTINGS_AUTO_OPEN);
        assert!(state.settings_move(-1));
        assert_eq!(state.settings_focus, SETTINGS_DOWNLOAD);
    }

    #[test]
    fn sliders_hide_with_their_master_toggle() {
        let mut state = State::default();
        assert!(state.settings_visible(SETTINGS_VOLUME));
        assert!(state.settings_visible(SETTINGS_STRENGTH));
        state.sound_enabled = false;
        state.haptics_enabled = false;
        assert!(!state.settings_visible(SETTINGS_VOLUME));
        assert!(!state.settings_visible(SETTINGS_STRENGTH));
        assert!(state.settings_visible(SETTINGS_AUTO_OPEN));
        assert!(state.settings_visible(SETTINGS_SOUNDS));
        assert!(state.settings_visible(SETTINGS_HAPTICS));
    }

    #[test]
    fn download_cap_row_is_hidden_for_packaged_builds() {
        let mut state = State {
            download_cap_available: false,
            ..State::default()
        };
        assert!(!state.settings_visible(SETTINGS_DOWNLOAD));
        assert!(state.settings_visible(SETTINGS_STRENGTH));
        state.settings_focus = SETTINGS_STRENGTH;
        assert!(state.settings_move(1));
        assert_eq!(
            state.settings_focus, SETTINGS_AUTO_OPEN,
            "movement skips the hidden cap row and wraps"
        );
        assert!(state.settings_move(1));
        assert_eq!(state.settings_focus, SETTINGS_SOUNDS);
        assert!(state.settings_move(-1));
        assert_eq!(state.settings_focus, SETTINGS_AUTO_OPEN);
    }

    #[test]
    fn settings_dropdowns_track_options_and_commit_choices() {
        let mut state = State::default();
        state.set_settings(true, 70, true, 40, 2 * 1024, HomeAutoOpen::Bluetooth);
        assert_eq!(State::settings_option_count(SETTINGS_AUTO_OPEN), 3);
        assert_eq!(State::settings_option_count(SETTINGS_DOWNLOAD), 4);

        assert!(state.open_settings_menu(SETTINGS_AUTO_OPEN));
        assert_eq!(state.settings_menu_highlight(), Some(1));
        assert!(state.settings_menu_move(1));
        assert_eq!(state.settings_menu_highlight(), Some(2));
        assert_eq!(
            state.settings_menu_choice(),
            Some(HomeSettingChoice::AutoOpen(HomeAutoOpen::Any))
        );
        state.close_settings_menu();
        assert!(!state.settings_menu_open());

        assert!(state.open_settings_menu(SETTINGS_DOWNLOAD));
        assert_eq!(state.settings_menu_highlight(), Some(1));
        assert!(state.settings_menu_move(-1));
        assert_eq!(state.settings_menu_highlight(), Some(0));
        assert_eq!(
            state.settings_menu_choice(),
            Some(HomeSettingChoice::DownloadLimit(0))
        );
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
    fn controller_dock_expands_then_collapses() {
        use crate::controller::model::{
            Capabilities, Connection, ControllerKind, ControllerStatus, PowerState,
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
        state.set_controllers(vec![row]);
        let start = Instant::now();
        assert!(state.toggle_dock(start));
        for i in 1..=8 {
            state.tick_controller_anim(start + std::time::Duration::from_millis(50 * i));
        }
        assert!(
            (state.dock_progress(start + std::time::Duration::from_millis(400)) - 1.0).abs() < 1e-4
        );
        assert!((state.controller_focus_t(0) - 1.0).abs() < 1e-4);
        assert!(!state.controller_anim_needs_frames());
        assert!(state.collapse_dock(start + std::time::Duration::from_millis(400)));
        for i in 1..=8 {
            state.tick_controller_anim(start + std::time::Duration::from_millis(500 + 50 * i));
        }
        assert!(
            state
                .dock_progress(start + std::time::Duration::from_millis(1000))
                .abs()
                < 1e-4
        );
        assert!(state.controller_focus_t(0).abs() < 1e-4);
    }

    #[test]
    fn shell_panel_enters_from_below_and_fades_out() {
        let start = Instant::now();
        let enter = ShellAnim::enter(start);
        let hidden = enter.frame(start, false);
        assert_eq!(hidden.panel.alpha, 0.0);
        assert!(hidden.panel.scale < 1.0);
        assert!(hidden.panel.offset_y > 0.0);
        let full = enter.frame(start + std::time::Duration::from_millis(400), false);
        assert_eq!(full.panel, PanelVisual::full());
        assert!(enter.done(start + std::time::Duration::from_millis(400)));
        let exit = ShellAnim::exit(start + std::time::Duration::from_millis(400), full);
        let out = exit.frame(start + std::time::Duration::from_millis(700), false);
        assert_eq!(out.panel.alpha, 0.0);
        assert!(out.panel.offset_y > 0.0);
        assert!(exit.done(start + std::time::Duration::from_millis(700)));
    }

    #[test]
    fn shell_panel_reduced_motion_snaps_movement() {
        let start = Instant::now();
        let enter = ShellAnim::enter(start);
        let hidden = enter.frame(start, true);
        assert_eq!(hidden.panel.alpha, 0.0);
        assert_eq!(hidden.panel.scale, 1.0);
        assert_eq!(hidden.panel.offset_y, 0.0);
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
        state.pad_tick(&sample, now, None);
        assert!(state.cross_held);
        assert!(state.triangle_held);
        state.pad_tick(
            &PadSample::default(),
            now + std::time::Duration::from_millis(16),
            None,
        );
        assert!(!state.cross_held);
        assert!(!state.triangle_held);
    }

    #[test]
    fn dock_stays_visible_under_settings_but_not_modals() {
        use crate::controller::model::{
            Capabilities, Connection, ControllerKind, ControllerStatus, PowerState,
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
        assert!(!state.dock_visible());
        state.set_controllers(vec![row]);
        assert!(state.dock_visible());
        state.settings_open = true;
        assert!(state.dock_visible(), "settings leaves the dock on screen");
        state.apps_menu = Some(0);
        assert!(!state.dock_visible(), "action modal hides the dock");
        state.apps_menu = None;
        state.apps_info = Some(0);
        assert!(!state.dock_visible(), "info modal hides the dock");
    }

    #[test]
    fn dpad_vertical_emits_move_y_for_the_dock() {
        use crate::domain::pad::PadSample;
        let mut state = State::default();
        let now = Instant::now();
        let down = PadSample {
            dpad_down: true,
            ..PadSample::default()
        };
        assert_eq!(
            state.pad_tick(&down, now, None),
            Some(HomeMessage::MoveY(1))
        );
        state.pad_tick(
            &PadSample::default(),
            now + std::time::Duration::from_millis(16),
            None,
        );
        let up = PadSample {
            dpad_up: true,
            ..PadSample::default()
        };
        assert_eq!(
            state.pad_tick(&up, now + std::time::Duration::from_millis(32), None),
            Some(HomeMessage::MoveY(-1))
        );
    }
}
