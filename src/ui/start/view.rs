//! Start-screen carousel: Games + Controllers, DualSense / keyboard navigable.

use crate::controller::dualsense::lightbar;
use crate::controller::model::PowerState;
use crate::games::GameEntry;
use crate::games::steam::SteamGame;
use crate::persist::prefs::GamesSortMode;
use crate::ui::color::BatterySpectrum;
use crate::ui::layout as window_layout;
use crate::ui::percent_ring::{self, POPUP_SIZE};
use crate::ui::start::icon_cache;
use crate::ui::start::input::FaceHeld;
use crate::ui::svg_icon;
use crate::ui::theme;
use iced::font::Weight;
use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::widget::text::Wrapping;
use iced::widget::{
    Float, button, column, container, row, scrollable, space, stack, svg, text, text_input,
};
use iced::{
    Alignment, Background, Border, Color, ContentFit, Element, Fill, Font, Length, Padding, Point,
    Rectangle, Renderer, Theme,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Logical width of the start-screen window.
pub const WIDTH: f32 = 768.0;
/// Logical height of the start-screen window (same 32∶25 aspect as 640×500).
pub const HEIGHT: f32 = 600.0;

pub const SLIDE_ANIM_MS: u64 = 220;
const SLIDE_ANIM_MIN_MS: u64 = 60;
const HEADER_HEIGHT: f32 = 36.0;
/// Matches the Games footer band (face-cycle toggle + face hints).
const FOOTER_HEIGHT: f32 = 32.0;
const IMMERSIVE_FOOTER_HEIGHT: f32 = 44.0;
const TITLE_ACTIVE: f32 = 20.0;
const TITLE_INACTIVE: f32 = 15.0;
const CUE_SIZE: f32 = 14.0;
/// Width reserved for an L2/R2 cue plus gap while revealed.
const CUE_SLOT_W: f32 = 30.0;
/// Title + up to two labeled meta lines (+ update badge).
const ROW_HEIGHT: f32 = 104.0;
const CONTROLLER_ROW_HEIGHT: f32 = 100.0;
/// Horizontal inset; vertical chrome uses [`PAD_Y`].
const PADDING: f32 = 20.0;
const PAD_Y: f32 = 8.0;
/// Portrait cell matching Steam library capsules (2:3).
const ICON_W: f32 = 48.0;
const ICON_H: f32 = 72.0;
const HOLD_RING_SIZE: f32 = 32.0;
const FACE_GLYPH_SIZE: f32 = 18.0;
/// Peak scale of a face glyph while its button is pressed (layout size unchanged).
const PRESSED_SCALE: f32 = 1.1;
/// Pressed grow and armed-color transitions.
const HINT_ANIM: Duration = Duration::from_millis(100);
const ROW_GAP: f32 = theme::LIST_SEPARATOR_GAP;
const ROW_ACTION_SPACING: f32 = 14.0;
/// Body width inside outer padding — dual-pane strip unit.
const PANE_W: f32 = WIDTH - 2.0 * PADDING;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StartSlide {
    #[default]
    Games,
    Controllers,
}

impl StartSlide {
    pub fn title(self) -> &'static str {
        match self {
            Self::Games => "Games",
            Self::Controllers => "Controllers",
        }
    }
}

#[derive(Debug, Clone)]
pub enum StartMessage {
    Launch(usize),
    /// Immersive strip: select a game without launching.
    SelectGame(usize),
    SelectController(usize),
    MoveUp,
    MoveDown,
    Confirm,
    Close,
    PrevSlide,
    NextSlide,
    ToggleEdit,
    CycleSort,
    AddShortcut,
    /// Open the add/edit modal for the selected manual (edit mode).
    EditManual,
    GamesScrolled(f32, f32),
    ControllersScrolled(f32, f32),
    SettingsScrolled(f32, f32),
    ManualAddTitle(String),
    ManualAddArgs(String),
    ManualPickIcon,
    ManualClearIcon,
    /// Options — open/close in-window Start settings.
    ToggleSettings,
    SetUsbControllers(bool),
    SetAlwaysImmersive(bool),
    SetInactiveSecs(u32),
    SetSleepSecs(u32),
    SetInactiveDimPercent(u8),
    SetSounds(bool),
    SetSoundVolume(u8),
    SetHaptics(bool),
    SetHapticsStrength(u8),
    StartGestureRecord,
    ResetStartGesture,
    CancelGestureRecord,
    /// Mouse-only: stamp lightbar-miss hitch for multi-session diag.
    #[cfg(debug_assertions)]
    ReportLightbarFailure,
    /// Mouse-only: stamp pad-input hitch for multi-session diag.
    #[cfg(debug_assertions)]
    ReportInputFailure,
}

/// Runtime-resolved game art (Steam / custom / shell), as a stable iced handle.
///
/// Built once via [`icon_cache`] (downscaled raster or shell extract). Recreating
/// `Handle::from_rgba` every refresh assigns a new cache id and forces iced's
/// image atlas to re-upload (and often grow) each redraw.
#[derive(Debug, Clone)]
pub struct StartIcon(pub iced::widget::image::Handle);

/// Where to load list / hero art for a start-screen game row.
#[derive(Debug, Clone)]
pub enum IconSource {
    File(PathBuf),
    Shell(PathBuf),
}

/// Subtext under a game title (plain path/URI, or stacked Steam labels).
#[derive(Debug, Clone)]
pub enum StartSubtitle {
    Plain(String),
    Labeled(Vec<crate::games::steam::MetaLine>),
}

impl StartSubtitle {
    /// Empty-catalog Steam fallback — hide when showing an update badge alone.
    pub fn is_steam_fallback(&self) -> bool {
        match self {
            Self::Plain(text) => text == "Steam",
            Self::Labeled(lines) => crate::games::steam::meta_lines_are_steam_fallback(lines),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StartRow {
    pub title: String,
    pub subtitle: Option<StartSubtitle>,
    pub target: String,
    pub args: String,
    pub play_key: String,
    pub icon: Option<StartIcon>,
    /// Path used for immersive hero art (selected + neighbors only).
    pub icon_source: Option<IconSource>,
    /// Landscape Steam art for immersive full-bleed backdrop.
    pub backdrop_path: Option<PathBuf>,
    /// Set in edit mode so Cross/click toggles membership or removes a manual.
    pub edit: Option<EditRow>,
    /// Steam catalog still scanning — paint empty well + muted bars, not AppID text.
    pub skeleton: bool,
    /// Steam install has an update pending (`StateFlags` update-required).
    pub update_required: bool,
}

/// Edit-mode action for a games-list row.
#[derive(Debug, Clone)]
pub enum EditRow {
    Steam { appid: u32, in_catalog: bool },
    Manual { id: String },
}

impl StartRow {
    fn launch_hint_label(&self) -> &'static str {
        if self.update_required {
            "Update & launch"
        } else {
            "Launch"
        }
    }

    pub fn from_entry(
        entry: &GameEntry,
        steam_by_id: &HashMap<u32, SteamGame>,
        steam_scan_pending: bool,
    ) -> Self {
        match entry {
            GameEntry::Steam { appid } => {
                if let Some(game) = steam_by_id.get(appid) {
                    let icon_source = steam_icon_source(game);
                    Self {
                        title: game.name.clone(),
                        subtitle: Some(StartSubtitle::Labeled(
                            crate::games::steam::browse_meta_lines(game),
                        )),
                        target: crate::games::steam::launch_uri(*appid),
                        args: String::new(),
                        play_key: entry.play_key(),
                        icon: steam_icon(game),
                        icon_source,
                        backdrop_path: game.backdrop_path.clone(),
                        edit: None,
                        skeleton: false,
                        update_required: game.update_required,
                    }
                } else if steam_scan_pending {
                    Self {
                        title: String::new(),
                        subtitle: None,
                        target: crate::games::steam::launch_uri(*appid),
                        args: String::new(),
                        play_key: entry.play_key(),
                        icon: None,
                        icon_source: None,
                        backdrop_path: None,
                        edit: None,
                        skeleton: true,
                        update_required: false,
                    }
                } else {
                    Self {
                        title: format!("Steam {appid}"),
                        subtitle: Some(StartSubtitle::Plain(format!("steam://rungameid/{appid}"))),
                        target: crate::games::steam::launch_uri(*appid),
                        args: String::new(),
                        play_key: entry.play_key(),
                        icon: None,
                        icon_source: None,
                        backdrop_path: None,
                        edit: None,
                        skeleton: false,
                        update_required: false,
                    }
                }
            }
            GameEntry::Manual {
                title,
                target,
                args,
                icon,
                ..
            } => {
                let icon_source = manual_icon_source(target, icon.as_deref());
                Self {
                    title: title.clone(),
                    subtitle: Some(StartSubtitle::Plain(target.clone())),
                    target: target.clone(),
                    args: args.clone(),
                    play_key: entry.play_key(),
                    icon: manual_icon(target, icon.as_deref()),
                    icon_source,
                    backdrop_path: None,
                    edit: None,
                    skeleton: false,
                    update_required: false,
                }
            }
        }
    }

    pub fn steam_edit(game: &SteamGame, in_catalog: bool) -> Self {
        let icon_source = steam_icon_source(game);
        Self {
            title: game.name.clone(),
            subtitle: Some(StartSubtitle::Labeled(
                crate::games::steam::browse_meta_lines(game),
            )),
            target: crate::games::steam::launch_uri(game.appid),
            args: String::new(),
            play_key: format!("steam:{}", game.appid),
            icon: steam_icon(game),
            icon_source,
            backdrop_path: game.backdrop_path.clone(),
            edit: Some(EditRow::Steam {
                appid: game.appid,
                in_catalog,
            }),
            skeleton: false,
            update_required: game.update_required,
        }
    }

    pub fn manual_edit(entry: &GameEntry) -> Option<Self> {
        let GameEntry::Manual {
            id,
            title,
            target,
            args,
            icon,
        } = entry
        else {
            return None;
        };
        let icon_source = manual_icon_source(target, icon.as_deref());
        Some(Self {
            title: title.clone(),
            subtitle: Some(StartSubtitle::Plain(target.clone())),
            target: target.clone(),
            args: args.clone(),
            play_key: entry.play_key(),
            icon: manual_icon(target, icon.as_deref()),
            icon_source,
            backdrop_path: None,
            edit: Some(EditRow::Manual { id: id.clone() }),
            skeleton: false,
            update_required: false,
        })
    }

    /// Immersive hero art — peek only (never decodes on the UI thread).
    /// Falls back to the live list-tier peek when the hero tier is still cold.
    pub fn hero_icon(&self) -> Option<StartIcon> {
        match &self.icon_source {
            Some(IconSource::File(path)) => icon_cache::hero_cached(path).map(StartIcon),
            Some(IconSource::Shell(path)) => icon_cache::hero_shell_cached(path).map(StartIcon),
            None => None,
        }
        .or_else(|| self.list_icon_live())
    }

    /// True when the immersive hero tier is already decoded (no list fallback).
    pub fn hero_ready(&self) -> bool {
        match &self.icon_source {
            Some(IconSource::File(path)) => icon_cache::hero_cached(path).is_some(),
            Some(IconSource::Shell(path)) => icon_cache::hero_shell_cached(path).is_some(),
            None => self.icon.is_some(),
        }
    }

    /// Immersive landscape backdrop — peek only (never decodes on the UI thread).
    pub fn backdrop_icon(&self) -> Option<StartIcon> {
        self.backdrop_path
            .as_ref()
            .and_then(|p| icon_cache::backdrop_cached(p).map(StartIcon))
    }

    /// List-tier icon — live peek (never decodes on the UI thread).
    pub fn list_icon_live(&self) -> Option<StartIcon> {
        match &self.icon_source {
            Some(IconSource::File(path)) => icon_cache::icon_cached(path).map(StartIcon),
            Some(IconSource::Shell(path)) => icon_cache::icon_shell_cached(path).map(StartIcon),
            None => None,
        }
        .or_else(|| self.icon.clone())
    }

    pub fn in_catalog(&self) -> bool {
        match &self.edit {
            Some(EditRow::Steam { in_catalog, .. }) => *in_catalog,
            Some(EditRow::Manual { .. }) => true,
            None => true,
        }
    }
}

fn steam_icon_source(game: &SteamGame) -> Option<IconSource> {
    game.icon_path.as_ref().map(|p| IconSource::File(p.clone()))
}

fn steam_icon(game: &SteamGame) -> Option<StartIcon> {
    game.icon_path
        .as_ref()
        .and_then(|p| icon_cache::icon_cached(p))
        .map(StartIcon)
}

fn manual_icon_source(target: &str, custom: Option<&str>) -> Option<IconSource> {
    if let Some(path) = custom.filter(|p| !p.is_empty()) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(IconSource::File(path));
        }
    }
    if target.starts_with("steam://") {
        return None;
    }
    Some(IconSource::Shell(PathBuf::from(target)))
}

/// Peek-only list icon — never decodes on the UI thread.
fn manual_icon(target: &str, custom: Option<&str>) -> Option<StartIcon> {
    if let Some(path) = custom.filter(|p| !p.is_empty()) {
        let path = PathBuf::from(path);
        if path.is_file()
            && let Some(handle) = icon_cache::icon_cached(&path)
        {
            return Some(StartIcon(handle));
        }
        // Fall through to shell icon when the custom image is cold / missing.
    }
    if target.starts_with("steam://") {
        return None;
    }
    let path = PathBuf::from(target);
    icon_cache::icon_shell_cached(&path).map(StartIcon)
}

/// Probe whether a target path yields a shell icon (for the add-manual modal).
pub fn probe_shell_icon(target: &str) -> Option<StartIcon> {
    if target.starts_with("steam://") {
        return None;
    }
    let path = PathBuf::from(target);
    icon_cache::handle_for_shell_pinned(&path).map(StartIcon)
}

/// Controller row for the Controllers slide (live or remembered disconnected).
#[derive(Debug, Clone, PartialEq)]
pub struct StartControllerRow {
    pub serial: String,
    pub title: String,
    pub connection: String,
    pub state: String,
    pub percent: u8,
    pub low: bool,
    pub bluetooth: bool,
    /// Live HID pad vs remembered-but-disconnected.
    pub connected: bool,
    pub eta: Option<String>,
}

impl StartControllerRow {
    /// Triangle hold is shown and accepted only for a live Bluetooth pad.
    pub fn show_power_off(&self) -> bool {
        self.connected && self.bluetooth
    }
}

#[derive(Debug, Clone)]
struct SlideAnim {
    from_x: f32,
    to: StartSlide,
    duration_ms: u64,
    started: Instant,
}

#[derive(Debug, Clone, Copy, Default)]
struct FacePressAnim {
    cross: f32,
    circle: f32,
    square: f32,
    triangle: f32,
    options: f32,
}

#[derive(Clone, Copy, Default)]
struct RowHintState {
    triangle_progress: f32,
    triangle_armed_t: f32,
    cross_progress: f32,
    cross_armed_t: f32,
    held: FaceHeld,
    press_anim: FacePressAnim,
}

/// Which chrome surface may light face / Options / reopen-chord press visuals.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HintPressOwner {
    Browse,
    Settings,
    ManualAdd,
    ReplaceConfirm,
}

impl HintPressOwner {
    fn current(state: &State) -> Self {
        if state.settings.open || state.settings.anim.is_some() {
            Self::Settings
        } else if state.manual_add.is_some() {
            Self::ManualAdd
        } else if state.replace_confirm.is_some() {
            Self::ReplaceConfirm
        } else {
            Self::Browse
        }
    }
}

#[derive(Clone, Copy, Default)]
struct HintPress {
    held: FaceHeld,
    press_anim: FacePressAnim,
    reopen_held: bool,
    reopen_press: f32,
    triangle_progress: f32,
    triangle_armed_t: f32,
    cross_progress: f32,
    cross_armed_t: f32,
}

impl HintPress {
    fn for_owner(state: &State, owner: HintPressOwner) -> Self {
        if HintPressOwner::current(state) != owner {
            return Self::default();
        }
        Self {
            held: state.held,
            press_anim: state.press_anim,
            reopen_held: state.reopen_chord_held,
            reopen_press: state.reopen_chord_press,
            triangle_progress: state.triangle_progress,
            triangle_armed_t: state.triangle_armed_anim,
            cross_progress: state.cross_progress,
            cross_armed_t: state.cross_armed_anim,
        }
    }
}

impl RowHintState {
    fn for_selected(state: &State, selected: bool) -> Self {
        // Browse row/dock/hero share press with the browse footer — idle while an overlay owns input.
        if selected {
            let press = HintPress::for_owner(state, HintPressOwner::Browse);
            Self {
                triangle_progress: press.triangle_progress,
                triangle_armed_t: press.triangle_armed_t,
                cross_progress: press.cross_progress,
                cross_armed_t: press.cross_armed_t,
                held: press.held,
                press_anim: press.press_anim,
            }
        } else {
            Self::default()
        }
    }
}

fn approach_anim(current: &mut f32, target: bool, dt: f32) {
    let target = if target { 1.0 } else { 0.0 };
    let step = dt / HINT_ANIM.as_secs_f32();
    if *current < target {
        *current = (*current + step).min(target);
    } else if *current > target {
        *current = (*current - step).max(target);
    }
}

#[derive(Debug, Clone)]
pub struct ReplaceConfirm {
    pub running_title: String,
    pub next_title: String,
    pub next_index: usize,
}

/// Draft for the edit-mode add/edit-manual modal.
#[derive(Debug, Clone)]
pub struct ManualAddDraft {
    /// When set, confirm updates this manual id instead of inserting.
    pub edit_id: Option<String>,
    pub target: String,
    pub title: String,
    pub args: String,
    pub icon_path: Option<PathBuf>,
    pub shell_icon: Option<StartIcon>,
}

impl ManualAddDraft {
    pub fn is_edit(&self) -> bool {
        self.edit_id.is_some()
    }
}

#[derive(Debug, Clone)]
struct DockAnim {
    from: f32,
    to: f32,
    duration_ms: u64,
    started: Instant,
}

#[derive(Debug, Clone)]
struct StripAnim {
    from: f32,
    to: f32,
    duration_ms: u64,
    started: Instant,
}

/// Frame-capped EnterImmersive veil clock (armed once the window can paint).
#[derive(Debug, Clone)]
struct EnterRevealClock {
    elapsed_ms: u64,
    armed: bool,
    last_tick: Option<Instant>,
}

/// Eased immersive idle/sleep dim crossfade.
#[derive(Debug, Clone)]
struct IdleDimAnim {
    from: f32,
    to: f32,
    started: Instant,
}

/// Background-task chip kind for the immersive BR / compact titlebar queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeStatusKind {
    PreparingArt,
    SteamLibrary,
}

impl ChromeStatusKind {
    fn label(self) -> &'static str {
        match self {
            Self::PreparingArt => "preparing",
            Self::SteamLibrary => "steam",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChromeStatusPhase {
    In,
    Active,
    Success,
    /// Slide off to the right (batch clear).
    ExitRight,
}

#[derive(Debug, Clone, Copy)]
struct ChromeStatusItem {
    kind: ChromeStatusKind,
    phase: ChromeStatusPhase,
    started: Instant,
    /// Target stack slot (0 = bottom).
    stack_slot: f32,
    /// Slot at the start of the current reflow anim.
    stack_from: f32,
    /// When stack reflow began (`None` = settled at `stack_slot`).
    stack_anim_started: Option<Instant>,
}

/// Visual for one chrome status chip in the stacked rail.
#[derive(Debug, Clone, Copy)]
pub struct ChromeStatusVisual {
    pub kind: ChromeStatusKind,
    /// 0 = bottom rest; higher = stacked above; may be &lt; 0 while entering.
    pub stack_slot: f32,
    /// 0 = at rest, 1 = fully slid off to the right.
    pub exit_x: f32,
    pub opacity: f32,
    pub success: bool,
}

/// Backward-compatible alias used by compact titlebar (single chip).
pub type SteamScanBannerVisual = ChromeStatusVisual;

#[derive(Debug, Clone)]
pub struct State {
    pub slide: StartSlide,
    /// Fullscreen console presentation (same data as compact).
    pub immersive: bool,
    /// HWND resize in flight — paint veil until settled.
    pub transition: Option<crate::ui::start::mode::StartTransition>,
    /// Cinematic phase around promote/demote (exit → resize → enter).
    pub transition_phase: Option<(crate::ui::start::mode::TransitionPhase, Instant)>,
    /// Capped enter-reveal progress while [`TransitionPhase::EnterImmersive`] is active.
    enter_reveal: Option<EnterRevealClock>,
    /// First Start HWND of the process: ambient + status ceremony (not warm reopen).
    pub cold_load_active: bool,
    /// When the cold black→ambient ease began (`StartOpened`).
    cold_ambient_started: Option<Instant>,
    /// Mirrored from App: true until first SteamScanDone.
    pub steam_scan_pending: bool,
    /// Immersive BR / compact titlebar status rail (bottom = newest).
    chrome_status: Vec<ChromeStatusItem>,
    /// When every live chip first became Success (`None` if any still working).
    chrome_status_all_success_at: Option<Instant>,
    /// Batch ExitRight stagger clock (`None` until clear starts).
    chrome_status_batch_exit_at: Option<Instant>,
    /// Immersive cold: when the games strip began fading in (`None` = still hidden).
    games_list_reveal_at: Option<Instant>,
    /// When EnterImmersive began waiting for list reveal (art hold / promote).
    enter_list_hold_started: Option<Instant>,
    /// Art is ready but splash reveal is held until the enter veil clears.
    splash_reveal_pending: bool,
    /// Pending reveal should snap opaque (cached) rather than ambient-fade.
    splash_reveal_cached: bool,
    /// Incoming splash uses ease-in-out enter fade (uncached ambient → art).
    enter_splash_fade: bool,
    /// Incoming splash paints fully opaque (cached reopen / after-veil snap).
    splash_opaque: bool,
    /// Immersive controllers dock target (expanded = Controllers focus).
    pub dock_expanded: bool,
    pub game_selected: usize,
    pub rows: Vec<StartRow>,
    pub controller_selected: usize,
    pub controllers: Vec<StartControllerRow>,
    pub running_target: Option<String>,
    pub replace_confirm: Option<ReplaceConfirm>,
    pub manual_add: Option<ManualAddDraft>,
    /// Options settings panel (compact modal / immersive drawer).
    pub settings: crate::ui::start::settings::SettingsPanel,
    pub triangle_progress: f32,
    pub cross_progress: f32,
    /// Face buttons currently held (pad OR keyboard) for action-hint press styling.
    pub held: FaceHeld,
    /// Reopen chord fully held (for immersive-toggle hint press styling).
    pub reopen_chord_held: bool,
    /// Animated 0..=1 press amounts per face (drives pressed scale).
    press_anim: FacePressAnim,
    /// Animated 0..=1 press for the reopen-chord hint glyph.
    reopen_chord_press: f32,
    /// Animated 0..=1 toward white-ish hold arc once triangle hold is armed.
    triangle_armed_anim: f32,
    /// Animated 0..=1 toward white-ish hold arc once cross hold is armed.
    cross_armed_anim: f32,
    hint_anim_tick: Option<Instant>,
    pub editing: bool,
    pub sort_mode: GamesSortMode,
    /// Controllers slide: include remembered disconnected pads.
    pub show_all_controllers: bool,
    /// `play_key` of the row selected when edit mode was entered (restored on Save/Cancel).
    pub edit_anchor_play_key: Option<String>,
    /// Last known games-list scroll offset / viewport height (for keep-selection-visible).
    games_scroll_y: f32,
    games_viewport_h: f32,
    controllers_scroll_y: f32,
    controllers_viewport_h: f32,
    anim: Option<SlideAnim>,
    dock_anim: Option<DockAnim>,
    strip_anim: Option<StripAnim>,
    /// Seconds for ambient shader time uniform.
    pub ambient_time: f32,
    /// Incoming backdrop land/crossfade (`started = None` = waiting for peek art).
    backdrop_current: Option<(String, Option<Instant>)>,
    /// Outgoing backdrop during cover-crossfade: `(play_key, scale, ox_norm, oy_norm)`.
    backdrop_outgoing: Option<(String, f32, f32, f32)>,
    /// Ring flash started with the last successful Identify (Controllers slide).
    identify_flash: Option<IdentifyFlash>,
    /// Keep painting while the art worker may still fill peeks (compact list / cover).
    pub art_awaiting_paint: bool,
    /// Immersive inactive / sleep logical phase.
    pub idle_phase: crate::ui::start::idle::IdlePhase,
    /// In-flight idle dim alpha crossfade.
    idle_dim_anim: Option<IdleDimAnim>,
    /// Settled idle dim alpha when not animating.
    idle_dim_settled: f32,
    /// True from Sleep wake until dim returns to Active (pad fully gated).
    pub idle_sleep_wake_pending: bool,
}

/// Active Identify ring flash for one controller row on the start screen.
#[derive(Debug, Clone)]
struct IdentifyFlash {
    serial: String,
    started: Instant,
}

/// How to pin a selection that has left the viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollReveal {
    /// Scroll only if the row crosses the top; pin to top.
    Up,
    /// Scroll only if the row crosses the bottom; pin to bottom.
    Down,
    /// Scroll if either edge is clipped (bidirectional).
    Either,
}

impl Default for State {
    fn default() -> Self {
        Self {
            slide: StartSlide::Games,
            immersive: false,
            transition: None,
            transition_phase: None,
            enter_reveal: None,
            cold_load_active: false,
            cold_ambient_started: None,
            steam_scan_pending: true,
            chrome_status: Vec::new(),
            chrome_status_all_success_at: None,
            chrome_status_batch_exit_at: None,
            games_list_reveal_at: None,
            enter_list_hold_started: None,
            splash_reveal_pending: false,
            splash_reveal_cached: false,
            enter_splash_fade: false,
            splash_opaque: false,
            dock_expanded: false,
            game_selected: 0,
            rows: Vec::new(),
            controller_selected: 0,
            controllers: Vec::new(),
            running_target: None,
            replace_confirm: None,
            manual_add: None,
            settings: crate::ui::start::settings::SettingsPanel::default(),
            triangle_progress: 0.0,
            cross_progress: 0.0,
            held: FaceHeld::default(),
            reopen_chord_held: false,
            press_anim: FacePressAnim::default(),
            reopen_chord_press: 0.0,
            triangle_armed_anim: 0.0,
            cross_armed_anim: 0.0,
            hint_anim_tick: None,
            editing: false,
            sort_mode: GamesSortMode::default(),
            show_all_controllers: false,
            edit_anchor_play_key: None,
            games_scroll_y: 0.0,
            games_viewport_h: 0.0,
            controllers_scroll_y: 0.0,
            controllers_viewport_h: 0.0,
            anim: None,
            dock_anim: None,
            strip_anim: None,
            ambient_time: 0.0,
            backdrop_current: None,
            backdrop_outgoing: None,
            identify_flash: None,
            art_awaiting_paint: false,
            idle_phase: crate::ui::start::idle::IdlePhase::Active,
            idle_dim_anim: None,
            idle_dim_settled: 0.0,
            idle_sleep_wake_pending: false,
        }
    }
}

impl State {
    pub fn set_rows(&mut self, rows: Vec<StartRow>) {
        self.rows = rows;
        if self.rows.is_empty() {
            self.game_selected = 0;
        } else {
            self.game_selected = self.game_selected.min(self.rows.len() - 1);
        }
    }

    pub fn set_controllers(&mut self, controllers: Vec<StartControllerRow>) {
        let prev_serial = self
            .controllers
            .get(self.controller_selected)
            .map(|r| r.serial.as_str());
        let next_selected = if controllers.is_empty() {
            0
        } else if let Some(serial) = prev_serial {
            controllers
                .iter()
                .position(|r| r.serial == serial)
                .unwrap_or_else(|| self.controller_selected.min(controllers.len() - 1))
        } else {
            self.controller_selected.min(controllers.len() - 1)
        };
        self.controllers = controllers;
        self.controller_selected = next_selected;
    }

    pub fn set_games_scroll(&mut self, y: f32, viewport_h: f32) {
        self.games_scroll_y = y;
        self.games_viewport_h = viewport_h;
    }

    pub fn set_controllers_scroll(&mut self, y: f32, viewport_h: f32) {
        self.controllers_scroll_y = y;
        self.controllers_viewport_h = viewport_h;
    }

    /// Keep tracked offset in sync with a programmatic `scroll_to` (on_scroll may lag).
    pub fn note_scroll_y(&mut self, id: &iced::widget::Id, y: f32) {
        if *id == games_scroll_id() {
            self.games_scroll_y = y;
        } else if *id == controllers_scroll_id() {
            self.controllers_scroll_y = y;
        }
    }

    /// Absolute Y offset to keep the current slide’s selection in view, if scrolling is needed.
    pub fn selection_scroll_y(&self, direction: ScrollReveal) -> Option<(iced::widget::Id, f32)> {
        match self.slide {
            StartSlide::Games => {
                let y = scroll_y_to_reveal(
                    self.game_selected,
                    self.rows.len(),
                    ROW_HEIGHT,
                    ROW_GAP,
                    self.games_scroll_y,
                    self.games_viewport_h,
                    direction,
                )?;
                Some((games_scroll_id(), y))
            }
            StartSlide::Controllers => {
                let y = scroll_y_to_reveal(
                    self.controller_selected,
                    self.controllers.len(),
                    CONTROLLER_ROW_HEIGHT,
                    ROW_GAP,
                    self.controllers_scroll_y,
                    self.controllers_viewport_h,
                    direction,
                )?;
                Some((controllers_scroll_id(), y))
            }
        }
    }

    /// Center the games selection in the viewport if it is out of view.
    pub fn selection_scroll_y_center(&self) -> Option<(iced::widget::Id, f32)> {
        if self.slide != StartSlide::Games {
            return None;
        }
        let y = scroll_y_center(
            self.game_selected,
            self.rows.len(),
            ROW_HEIGHT,
            ROW_GAP,
            self.games_scroll_y,
            self.games_viewport_h,
            false,
        )?;
        Some((games_scroll_id(), y))
    }

    /// Always scroll so the games selection is as centered as the list allows.
    pub fn selection_scroll_y_center_prefer(&self) -> Option<(iced::widget::Id, f32)> {
        if self.slide != StartSlide::Games {
            return None;
        }
        let y = scroll_y_center(
            self.game_selected,
            self.rows.len(),
            ROW_HEIGHT,
            ROW_GAP,
            self.games_scroll_y,
            self.games_viewport_h,
            true,
        )?;
        Some((games_scroll_id(), y))
    }

    /// Select the row matching `play_key`, or the first row if missing / empty.
    pub fn select_game_by_play_key(&mut self, play_key: Option<&str>) {
        if self.rows.is_empty() {
            self.game_selected = 0;
            return;
        }
        if let Some(key) = play_key
            && let Some(i) = self.rows.iter().position(|r| r.play_key == key)
        {
            self.game_selected = i;
            return;
        }
        self.game_selected = 0;
    }

    /// Remember the current games selection’s `play_key` for edit enter/exit restore.
    pub fn capture_edit_anchor(&mut self) {
        self.edit_anchor_play_key = self
            .rows
            .get(self.game_selected)
            .map(|r| r.play_key.clone());
    }

    /// Restore selection from [`Self::edit_anchor_play_key`] and clear the anchor.
    pub fn restore_edit_anchor(&mut self) {
        let key = self.edit_anchor_play_key.take();
        self.select_game_by_play_key(key.as_deref());
    }

    /// Land on Games with no in-flight anim (used whenever the start screen opens).
    pub fn reset_to_games(&mut self) {
        self.slide = StartSlide::Games;
        self.anim = None;
        self.dock_anim = None;
        self.strip_anim = None;
        self.dock_expanded = false;
        self.replace_confirm = None;
        self.manual_add = None;
        self.settings.reset();
        self.triangle_progress = 0.0;
        self.cross_progress = 0.0;
        self.held = FaceHeld::default();
        self.reopen_chord_held = false;
        self.press_anim = FacePressAnim::default();
        self.reopen_chord_press = 0.0;
        self.triangle_armed_anim = 0.0;
        self.cross_armed_anim = 0.0;
        self.hint_anim_tick = None;
        self.editing = false;
        self.edit_anchor_play_key = None;
        self.game_selected = 0;
        self.controller_selected = 0;
        self.games_scroll_y = 0.0;
        self.controllers_scroll_y = 0.0;
    }

    pub fn overlay_blocking(&self) -> bool {
        self.replace_confirm.is_some()
            || self.manual_add.is_some()
            || self.settings.open
            || self.settings.anim.is_some()
    }

    /// Toggle Options settings. Compact opens instantly; immersive slides the drawer.
    pub fn request_settings(&mut self, open: bool, now: Instant) -> bool {
        let animate = self.immersive;
        if self.settings.request(open, now, animate) {
            if open {
                self.replace_confirm = None;
                self.cross_progress = 0.0;
            }
            true
        } else {
            false
        }
    }

    pub fn animating(&self) -> bool {
        // Strip scroll is retargetable — do not gate pad Up/Down on it.
        self.anim.is_some()
            || (self.immersive && self.dock_anim.is_some())
            || self.transition_phase.is_some()
            || self.transition.is_some()
    }

    /// Immersive strip scroll in flight (visual chase; nav may retarget).
    pub fn strip_busy(&self) -> bool {
        self.immersive && self.strip_anim.is_some()
    }

    pub fn needs_frames(&self) -> bool {
        self.anim.is_some()
            || self.dock_anim.is_some()
            || self.strip_anim.is_some()
            || self.transition.is_some()
            || self.transition_phase.is_some()
            || self.backdrop_current.is_some()
            || self.backdrop_outgoing.is_some()
            || self.immersive
            || self.cold_load_active
            || self.triangle_progress > 0.0
            || self.cross_progress > 0.0
            || self.replace_confirm.is_some()
            || self.manual_add.is_some()
            || self.settings.open
            || self.settings.anim.is_some()
            || self.hint_anims_need_frames()
            || self.identify_flash_active()
            || self.art_awaiting_paint
            || !self.chrome_status.is_empty()
            || self.games_list_reveal_at.is_some()
    }

    /// Immersive dock width progress 0..=1 (eased while animating).
    pub fn dock_progress(&self, now: Instant) -> f32 {
        if let Some(anim) = &self.dock_anim {
            let t = (now.saturating_duration_since(anim.started).as_secs_f32()
                / (anim.duration_ms.max(1) as f32 / 1000.0))
                .clamp(0.0, 1.0);
            let e = window_layout::ease_out_cubic(t);
            return anim.from + (anim.to - anim.from) * e;
        }
        if self.dock_expanded { 1.0 } else { 0.0 }
    }

    /// Request immersive dock expand/collapse. Syncs [`Self::slide`] for focus/footer.
    pub fn request_dock(&mut self, expanded: bool, now: Instant) -> bool {
        let target = if expanded { 1.0 } else { 0.0 };
        if self
            .dock_anim
            .as_ref()
            .is_some_and(|a| (a.to - target).abs() < 0.01)
        {
            return false;
        }
        if self.dock_anim.is_none() && self.dock_expanded == expanded {
            return false;
        }
        let from = self.dock_progress(now);
        self.dock_expanded = expanded;
        self.slide = if expanded {
            StartSlide::Controllers
        } else {
            StartSlide::Games
        };
        if expanded {
            self.editing = false;
            self.edit_anchor_play_key = None;
        }
        self.replace_confirm = None;
        self.manual_add = None;
        self.cross_progress = 0.0;
        if (from - target).abs() < 0.01 {
            self.dock_anim = None;
            return false;
        }
        self.dock_anim = Some(DockAnim {
            from,
            to: target,
            duration_ms: SLIDE_ANIM_MS,
            started: now,
        });
        true
    }

    /// Fractional game index for the immersive vertical strip.
    pub fn strip_scroll(&self, now: Instant) -> f32 {
        if let Some(anim) = &self.strip_anim {
            return crate::ui::start::vstrip::strip_scroll(
                anim.from,
                anim.to,
                anim.started,
                now,
                anim.duration_ms,
            );
        }
        self.game_selected as f32
    }

    /// Chase selection from the current visual scroll (retargets mid-flight for rapid steps).
    ///
    /// `from_visual` is the strip position before `game_selected` changed. When no anim is
    /// in flight, [`strip_scroll`] would already equal the new selection — so callers must
    /// pass the previous settled index (or live visual) explicitly.
    pub fn begin_strip_anim(&mut self, from_visual: f32, now: Instant) {
        let len = self.rows.len() as f32;
        if len < 1.0 {
            self.strip_anim = None;
            return;
        }
        let catching_up = self.strip_anim.is_some();
        // Mid-flight: chase from the live eased scroll. Settled: use caller's prior index.
        let visual = if catching_up {
            self.strip_scroll(now)
        } else {
            from_visual
        };
        let to = self.game_selected as f32;
        let delta = crate::ui::start::vstrip::shortest_circular_delta(visual, to, len);
        // Animate toward `to` from `to - delta` so wraps are short (e.g. -1 → 0).
        let from = to - delta;
        if delta.abs() < 0.01 {
            self.strip_anim = None;
            return;
        }
        let duration_ms = if catching_up {
            crate::ui::start::vstrip::STRIP_ANIM_CATCHUP_MS
        } else {
            crate::ui::start::vstrip::STRIP_ANIM_MS
        };
        self.strip_anim = Some(StripAnim {
            from,
            to,
            duration_ms,
            started: now,
        });
    }

    /// Clear dock/strip/transition and ambient (close / leave immersive fully).
    pub fn clear_immersive_session(&mut self) {
        self.dock_expanded = false;
        self.dock_anim = None;
        self.strip_anim = None;
        self.settings.reset();
        self.transition = None;
        self.transition_phase = None;
        self.enter_reveal = None;
        self.splash_reveal_pending = false;
        self.splash_reveal_cached = false;
        self.enter_splash_fade = false;
        self.splash_opaque = false;
        self.backdrop_current = None;
        self.backdrop_outgoing = None;
        self.ambient_time = 0.0;
        self.reset_idle_dim();
    }

    /// Reset immersive idle/sleep overlay to Active (no anim).
    pub fn reset_idle_dim(&mut self) {
        self.idle_phase = crate::ui::start::idle::IdlePhase::Active;
        self.idle_dim_anim = None;
        self.idle_dim_settled = 0.0;
        self.idle_sleep_wake_pending = false;
    }

    /// Current idle/sleep overlay alpha (0 = clear).
    pub fn idle_dim_amount(&self, now: Instant) -> f32 {
        if let Some(anim) = &self.idle_dim_anim {
            let elapsed = now.saturating_duration_since(anim.started).as_millis() as u64;
            return crate::ui::start::idle::dim_alpha(anim.from, anim.to, elapsed);
        }
        self.idle_dim_settled
    }

    /// True while Sleep, or while waking from Sleep until dim settles on Active.
    pub fn idle_pad_gated(&self) -> bool {
        matches!(self.idle_phase, crate::ui::start::idle::IdlePhase::Sleep)
            || self.idle_sleep_wake_pending
    }

    /// True while an idle dim crossfade is in flight.
    pub fn idle_dim_animating(&self, now: Instant) -> bool {
        self.idle_dim_anim.as_ref().is_some_and(|anim| {
            let elapsed = now.saturating_duration_since(anim.started).as_millis() as u64;
            !crate::ui::start::idle::dim_anim_done(elapsed)
        })
    }

    /// Begin eased crossfade toward `phase` target alpha. Returns true if phase changed.
    pub fn set_idle_phase(
        &mut self,
        phase: crate::ui::start::idle::IdlePhase,
        inactive_dim: f32,
        now: Instant,
        from_sleep_wake: bool,
    ) -> bool {
        use crate::ui::start::idle::IdlePhase;
        if self.idle_phase == phase && self.idle_dim_anim.is_none() && !from_sleep_wake {
            return false;
        }
        let from = self.idle_dim_amount(now);
        let to = phase.target_alpha(inactive_dim);
        let leaving_sleep = matches!(self.idle_phase, IdlePhase::Sleep)
            || from_sleep_wake
            || self.idle_sleep_wake_pending;
        self.idle_phase = phase;
        if from_sleep_wake || (leaving_sleep && matches!(phase, IdlePhase::Active)) {
            self.idle_sleep_wake_pending = true;
        }
        if (from - to).abs() < 0.001 {
            self.idle_dim_anim = None;
            self.idle_dim_settled = to;
        } else {
            self.idle_dim_anim = Some(IdleDimAnim {
                from,
                to,
                started: now,
            });
        }
        true
    }

    /// Advance idle dim anim; returns true when a sleep-wake gate should clear (re-arm pad).
    pub fn tick_idle_dim(&mut self, now: Instant) -> bool {
        if let Some(anim) = &self.idle_dim_anim {
            let elapsed = now.saturating_duration_since(anim.started).as_millis() as u64;
            if !crate::ui::start::idle::dim_anim_done(elapsed) {
                return false;
            }
            self.idle_dim_settled = anim.to;
            self.idle_dim_anim = None;
        }
        if self.idle_sleep_wake_pending
            && matches!(self.idle_phase, crate::ui::start::idle::IdlePhase::Active)
            && self.idle_dim_anim.is_none()
        {
            self.idle_sleep_wake_pending = false;
            return true;
        }
        false
    }

    pub fn begin_transition_phase(
        &mut self,
        phase: crate::ui::start::mode::TransitionPhase,
        now: Instant,
    ) {
        use crate::ui::start::mode::TransitionPhase;
        self.dock_anim = None;
        self.strip_anim = None;
        self.transition_phase = Some((phase, now));
        if matches!(phase, TransitionPhase::EnterImmersive) {
            self.enter_reveal = Some(EnterRevealClock {
                elapsed_ms: 0,
                armed: false,
                last_tick: None,
            });
            // Hide strip until art sync / timeout begins the list reveal (cold + promote).
            self.games_list_reveal_at = None;
            self.enter_list_hold_started = Some(now);
            self.splash_reveal_pending = false;
            self.splash_reveal_cached = false;
            self.enter_splash_fade = false;
            self.splash_opaque = false;
        } else {
            self.enter_reveal = None;
        }
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: start immersive transition phase={}",
            phase.label()
        ));
    }

    pub fn clear_transition_phase(&mut self) {
        self.transition_phase = None;
        self.enter_reveal = None;
        // Pending splash starts on the next `tick_anim` / explicit try_start (same clock).
    }

    /// True while EnterImmersive is in flight — splash fade waits until the phase ends.
    fn enter_veil_blocks_splash(&self) -> bool {
        matches!(
            self.transition_phase.map(|(p, _)| p),
            Some(crate::ui::start::mode::TransitionPhase::EnterImmersive)
        )
    }

    /// Begin advancing the EnterImmersive veil (cold open after StartOpened, or promote settle).
    pub fn arm_enter_reveal(&mut self, now: Instant) {
        let Some(clock) = self.enter_reveal.as_mut() else {
            return;
        };
        if clock.armed {
            return;
        }
        clock.armed = true;
        clock.last_tick = Some(now);
        clock.elapsed_ms = 0;
        crate::controller::hid::diag::diag_info("ui-diag: start immersive enter reveal armed");
    }

    /// True once the enter-reveal clock is advancing (veil lifting).
    pub fn enter_reveal_armed(&self) -> bool {
        self.enter_reveal.as_ref().is_some_and(|c| c.armed)
    }

    /// Begin the process-first Start load ceremony (immersive ambient + dock; compact skips).
    pub fn begin_cold_load(&mut self) {
        self.cold_load_active = true;
        self.cold_ambient_started = None;
        self.games_list_reveal_at = None;
        self.enter_list_hold_started = None;
        crate::controller::hid::diag::diag_info("ui-diag: start cold load begin");
    }

    /// Arm the black→ambient ease once the HWND can paint.
    pub fn arm_cold_ambient(&mut self, now: Instant) {
        if !self.cold_load_active || self.cold_ambient_started.is_some() {
            return;
        }
        self.cold_ambient_started = Some(now);
        crate::controller::hid::diag::diag_info("ui-diag: start cold load ambient armed");
    }

    /// Solid black remaining over ambient during cold load (1 = black, 0 = ambient clear).
    pub fn cold_black_amount(&self, now: Instant) -> f32 {
        use crate::ui::start::mode::{COLD_AMBIENT_MS, phase_progress};
        if !self.cold_load_active {
            return 0.0;
        }
        let Some(started) = self.cold_ambient_started else {
            return 1.0;
        };
        let elapsed = now.saturating_duration_since(started).as_millis() as u64;
        1.0 - phase_progress(elapsed, COLD_AMBIENT_MS)
    }

    /// End the cold-load ceremony (warm reopens skip it).
    pub fn finish_cold_load(&mut self) {
        if !self.cold_load_active {
            return;
        }
        self.cold_load_active = false;
        self.cold_ambient_started = None;
        crate::controller::hid::diag::diag_info("ui-diag: start cold load done");
    }

    /// Start chrome status for a background task (Steam wrapper).
    pub fn begin_steam_scan_banner(&mut self, now: Instant) {
        self.begin_chrome_status(ChromeStatusKind::SteamLibrary, now);
    }

    /// Steam scan finished — stay Success on the rail (Preparing may begin while cold).
    pub fn note_steam_scan_banner_success(&mut self, now: Instant) {
        self.note_chrome_status_success(ChromeStatusKind::SteamLibrary, now);
        // Cache-miss cold: Steam done but art still pending → stack Preparing under it.
        if self.immersive
            && self.cold_load_active
            && self.games_list_reveal_at.is_none()
            && !self.chrome_status_has_kind(ChromeStatusKind::PreparingArt)
        {
            self.begin_chrome_status(ChromeStatusKind::PreparingArt, now);
        }
    }

    pub fn steam_scan_banner_active(&self) -> bool {
        self.chrome_status_has_kind(ChromeStatusKind::SteamLibrary)
    }

    fn chrome_status_has_kind(&self, kind: ChromeStatusKind) -> bool {
        self.chrome_status.iter().any(|i| {
            i.kind == kind
                && matches!(
                    i.phase,
                    ChromeStatusPhase::In | ChromeStatusPhase::Active | ChromeStatusPhase::Success
                )
        })
    }

    fn chrome_status_display_slot(item: &ChromeStatusItem, now: Instant) -> f32 {
        use crate::ui::start::mode::{CHROME_STATUS_IN_MS, phase_progress};
        if let Some(started) = item.stack_anim_started {
            let t = phase_progress(
                now.saturating_duration_since(started).as_millis() as u64,
                CHROME_STATUS_IN_MS,
            );
            return item.stack_from + (item.stack_slot - item.stack_from) * t;
        }
        item.stack_slot
    }

    /// Begin a chrome status chip at the bottom of the rail (existing chips shift up).
    pub fn begin_chrome_status(&mut self, kind: ChromeStatusKind, now: Instant) {
        // Already on the rail (any phase) — keep stable.
        if self.chrome_status.iter().any(|i| i.kind == kind) {
            return;
        }
        // New work cancels any in-flight batch exit.
        self.chrome_status_all_success_at = None;
        self.chrome_status_batch_exit_at = None;
        // Drop chips mid-exit so a new batch can start clean.
        self.chrome_status
            .retain(|i| i.phase != ChromeStatusPhase::ExitRight);

        // Capture display slots first (immutable), then apply reflow.
        let from_slots: Vec<f32> = self
            .chrome_status
            .iter()
            .map(|i| Self::chrome_status_display_slot(i, now))
            .collect();
        for (item, from) in self.chrome_status.iter_mut().zip(from_slots) {
            item.stack_from = from;
            item.stack_slot += 1.0;
            item.stack_anim_started = Some(now);
        }
        self.chrome_status.insert(
            0,
            ChromeStatusItem {
                kind,
                phase: ChromeStatusPhase::In,
                started: now,
                stack_slot: 0.0,
                stack_from: -1.0,
                // Drive enter with the same reflow clock so layout stays consistent.
                stack_anim_started: Some(now),
            },
        );
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: chrome status begin kind={} n={}",
            kind.label(),
            self.chrome_status.len()
        ));
    }

    /// Mark a status successful; chip stays stacked until the whole batch clears.
    pub fn note_chrome_status_success(&mut self, kind: ChromeStatusKind, now: Instant) {
        let Some(item) = self.chrome_status.iter_mut().find(|i| {
            i.kind == kind
                && matches!(
                    i.phase,
                    ChromeStatusPhase::In | ChromeStatusPhase::Active | ChromeStatusPhase::Success
                )
        }) else {
            return;
        };
        if item.phase == ChromeStatusPhase::Success {
            return;
        }
        item.phase = ChromeStatusPhase::Success;
        item.started = now;
        // Keep any in-flight stack reflow — clearing it snapped chips mid-animation.
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: chrome status success kind={}",
            kind.label()
        ));
        self.note_chrome_status_maybe_all_success(now);
    }

    fn note_chrome_status_maybe_all_success(&mut self, now: Instant) {
        if self.chrome_status.is_empty() {
            self.chrome_status_all_success_at = None;
            return;
        }
        let all_success = self
            .chrome_status
            .iter()
            .all(|i| i.phase == ChromeStatusPhase::Success);
        if all_success {
            if self.chrome_status_all_success_at.is_none() {
                self.chrome_status_all_success_at = Some(now);
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: chrome status all success n={}",
                    self.chrome_status.len()
                ));
            }
        } else {
            self.chrome_status_all_success_at = None;
            self.chrome_status_batch_exit_at = None;
        }
    }

    fn tick_chrome_status(&mut self, now: Instant) {
        use crate::ui::start::mode::{
            CHROME_STATUS_EXIT_MS, CHROME_STATUS_IN_MS, CHROME_STATUS_STAGGER_MS,
            CHROME_STATUS_STEP_HOLD_MS, CHROME_STATUS_SUCCESS_HOLD_MS,
        };

        // Settle stack reflow clocks.
        for item in &mut self.chrome_status {
            if let Some(started) = item.stack_anim_started {
                let elapsed = now.saturating_duration_since(started).as_millis() as u64;
                if elapsed >= CHROME_STATUS_IN_MS {
                    item.stack_anim_started = None;
                    item.stack_from = item.stack_slot;
                }
            }
        }

        // In → Active.
        for item in &mut self.chrome_status {
            if item.phase == ChromeStatusPhase::In {
                let elapsed = now.saturating_duration_since(item.started).as_millis() as u64;
                if elapsed >= CHROME_STATUS_IN_MS {
                    item.phase = ChromeStatusPhase::Active;
                    item.started = now;
                }
            }
        }

        // Start staggered ExitRight once the batch has held Success long enough.
        if self.chrome_status_batch_exit_at.is_none()
            && let Some(all_at) = self.chrome_status_all_success_at
        {
            let hold = CHROME_STATUS_SUCCESS_HOLD_MS + CHROME_STATUS_STEP_HOLD_MS;
            if now.saturating_duration_since(all_at) >= Duration::from_millis(hold) {
                self.chrome_status_batch_exit_at = Some(now);
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: chrome status exit_right begin n={}",
                    self.chrome_status.len()
                ));
            }
        }
        if let Some(batch_at) = self.chrome_status_batch_exit_at {
            let batch_elapsed = now.saturating_duration_since(batch_at).as_millis() as u64;
            for (i, item) in self.chrome_status.iter_mut().enumerate() {
                if item.phase != ChromeStatusPhase::Success {
                    continue;
                }
                let stagger = (i as u64).saturating_mul(CHROME_STATUS_STAGGER_MS);
                if batch_elapsed >= stagger {
                    item.phase = ChromeStatusPhase::ExitRight;
                    item.started = batch_at + Duration::from_millis(stagger);
                }
            }
        }

        // Remove finished ExitRight chips (bottom-first removals OK via reverse index).
        let mut remove = Vec::new();
        for (i, item) in self.chrome_status.iter().enumerate() {
            if item.phase != ChromeStatusPhase::ExitRight {
                continue;
            }
            let elapsed = now.saturating_duration_since(item.started).as_millis() as u64;
            if elapsed >= CHROME_STATUS_EXIT_MS {
                remove.push(i);
            }
        }
        for i in remove.into_iter().rev() {
            if let Some(item) = self.chrome_status.get(i) {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: chrome status exit_right done kind={}",
                    item.kind.label()
                ));
            }
            self.chrome_status.remove(i);
        }
        if self.chrome_status.is_empty() {
            if self.chrome_status_batch_exit_at.is_some() {
                crate::controller::hid::diag::diag_info("ui-diag: chrome status clear n=0");
            }
            self.chrome_status_all_success_at = None;
            self.chrome_status_batch_exit_at = None;
        }
    }

    fn chrome_status_item_visual(item: &ChromeStatusItem, now: Instant) -> ChromeStatusVisual {
        use crate::ui::start::mode::{CHROME_STATUS_EXIT_MS, phase_progress};
        let stack_slot = Self::chrome_status_display_slot(item, now);
        let (exit_x, opacity, success) = match item.phase {
            ChromeStatusPhase::In | ChromeStatusPhase::Active => (0.0, 1.0, false),
            ChromeStatusPhase::Success => (0.0, 1.0, true),
            ChromeStatusPhase::ExitRight => {
                let t = phase_progress(
                    now.saturating_duration_since(item.started).as_millis() as u64,
                    CHROME_STATUS_EXIT_MS,
                );
                // Full fade by the end so any remainder past travel is gone.
                (t, 1.0 - t, true)
            }
        };
        ChromeStatusVisual {
            kind: item.kind,
            stack_slot,
            exit_x: exit_x.clamp(0.0, 1.0),
            opacity: opacity.clamp(0.0, 1.0),
            success,
        }
    }

    /// All live chrome status visuals (bottom → top) for immersive BR stacking.
    pub fn chrome_load_status(&self, now: Instant) -> Vec<ChromeStatusVisual> {
        self.chrome_status
            .iter()
            .map(|i| Self::chrome_status_item_visual(i, now))
            .collect()
    }

    /// Compact titlebar: newest Active, else newest Success / In.
    pub fn chrome_load_status_primary(&self, now: Instant) -> Option<ChromeStatusVisual> {
        let visuals = self.chrome_load_status(now);
        visuals
            .iter()
            .find(|v| !v.success && v.exit_x < 0.01)
            .copied()
            .or_else(|| visuals.first().copied())
    }

    /// Opacity of the immersive games strip (0 = hidden until list reveal, 1 = fully shown).
    pub fn games_list_opacity(&self, now: Instant) -> f32 {
        use crate::ui::start::mode::{GAMES_LIST_FADE_MS, phase_progress};
        if !self.immersive {
            return 1.0;
        }
        let Some(started) = self.games_list_reveal_at else {
            // Cold + promote EnterImmersive: strip stays hidden until art sync / timeout.
            return 0.0;
        };
        let elapsed = now.saturating_duration_since(started).as_millis() as u64;
        phase_progress(elapsed, GAMES_LIST_FADE_MS)
    }

    /// Begin fading the immersive games list in (idempotent).
    pub fn begin_games_list_reveal(&mut self, now: Instant) {
        if self.games_list_reveal_at.is_none() {
            self.games_list_reveal_at = Some(now);
            self.enter_list_hold_started = None;
            crate::controller::hid::diag::diag_info("ui-diag: start games list reveal begin");
            if self.chrome_status_has_kind(ChromeStatusKind::PreparingArt) {
                self.note_chrome_status_success(ChromeStatusKind::PreparingArt, now);
            }
        }
    }

    /// Selected splash + visible strip heroes are decoded (matches idle Steam-scan pre-warm).
    pub fn enter_art_sync_ready(&self) -> bool {
        use crate::ui::start::vstrip::{NEIGHBORS, VISIBLE};
        let len = self.rows.len();
        if len == 0 {
            return true;
        }
        let selected = self.game_selected.min(len - 1);
        let Some(sel) = self.rows.get(selected) else {
            return true;
        };
        // Wait for Steam scan to materialize real rows before syncing enter.
        if sel.skeleton {
            return false;
        }
        if sel.backdrop_path.is_some() && sel.backdrop_icon().is_none() {
            return false;
        }
        if sel.icon_source.is_some() && !sel.hero_ready() {
            return false;
        }
        let circular = self.immersive && len >= VISIBLE;
        for idx in icon_cache::art_window_indices(selected, len, circular, NEIGHBORS) {
            let Some(row) = self.rows.get(idx) else {
                continue;
            };
            if row.skeleton {
                return false;
            }
            if row.icon_source.is_some() && !row.hero_ready() {
                return false;
            }
        }
        true
    }

    /// Arm enter reveal once splash + strip heroes are ready.
    ///
    /// Immersive cold / promote: enter veil may already be lifting onto ambient+dock.
    /// When art is ready, fade the games list / splash in — do not re-black the chrome.
    pub fn try_arm_enter_reveal_when_art_ready(&mut self, now: Instant) -> bool {
        if self.enter_reveal.is_none() || self.enter_reveal_armed() {
            // Veil already lifting/cleared: list reveal after art (cold + promote).
            if self.immersive && self.games_list_reveal_at.is_none() && self.enter_art_sync_ready()
            {
                self.begin_games_list_reveal(now);
                if let Some(row) = self.rows.get(self.game_selected)
                    && row.backdrop_icon().is_some()
                {
                    self.start_enter_splash_fade(row.play_key.clone(), now);
                }
                if self.cold_load_active {
                    self.finish_cold_load();
                    crate::controller::hid::diag::diag_info(
                        "ui-diag: start immersive enter art sync ready cold=1 list_reveal=1",
                    );
                } else {
                    crate::controller::hid::diag::diag_info(
                        "ui-diag: start immersive enter art sync ready list_reveal=1",
                    );
                }
                return true;
            }
            return false;
        }
        if !self.enter_art_sync_ready() {
            return false;
        }
        let hold_ms = self
            .transition_phase
            .map(|(_, started)| now.saturating_duration_since(started).as_millis())
            .unwrap_or(0);
        if self.cold_load_active && self.immersive {
            // Veil never armed yet — start the initial lift, then reveal the list.
            self.begin_games_list_reveal(now);
            if let Some(row) = self.rows.get(self.game_selected)
                && row.backdrop_icon().is_some()
            {
                self.start_enter_splash_fade(row.play_key.clone(), now);
            }
            self.finish_cold_load();
            self.arm_enter_reveal(now);
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: start immersive enter art sync ready ms={hold_ms} cold=1"
            ));
            return true;
        }
        self.begin_games_list_reveal(now);
        if let Some(row) = self.rows.get(self.game_selected)
            && row.backdrop_icon().is_some()
        {
            self.reveal_cached_opaque(row.play_key.clone(), now);
        }
        self.arm_enter_reveal(now);
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: start immersive enter art sync ready ms={hold_ms} list_reveal=1"
        ));
        true
    }

    /// If art stays cold too long, lift the veil / reveal games anyway.
    pub fn maybe_force_enter_reveal_after_art_hold(&mut self, now: Instant) {
        use crate::ui::start::mode::ENTER_ART_HOLD_MAX_MS;
        // Immersive cold: reveal the list even if art never synced.
        if self.cold_load_active && self.immersive {
            let timed_out = self.cold_ambient_started.is_some_and(|t| {
                now.saturating_duration_since(t) >= Duration::from_millis(ENTER_ART_HOLD_MAX_MS)
            });
            if timed_out {
                let hold_ms = self
                    .cold_ambient_started
                    .map(|t| now.saturating_duration_since(t).as_millis())
                    .unwrap_or(0);
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: start immersive enter art hold timeout ms={hold_ms} cold=1"
                ));
                self.begin_games_list_reveal(now);
                if let Some(row) = self.rows.get(self.game_selected)
                    && row.backdrop_icon().is_some()
                {
                    self.start_enter_splash_fade(row.play_key.clone(), now);
                }
                self.finish_cold_load();
                if self.enter_reveal.is_some() && !self.enter_reveal_armed() {
                    self.arm_enter_reveal(now);
                }
                return;
            }
        }
        // Promote / warm enter: list stay-hidden clock outlives the veil phase.
        if self.immersive
            && self.games_list_reveal_at.is_none()
            && let Some(started) = self.enter_list_hold_started
            && now.saturating_duration_since(started)
                >= Duration::from_millis(ENTER_ART_HOLD_MAX_MS)
        {
            let hold_ms = now.saturating_duration_since(started).as_millis();
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: start immersive enter art hold timeout ms={hold_ms} list_reveal=1"
            ));
            self.begin_games_list_reveal(now);
            if let Some(row) = self.rows.get(self.game_selected)
                && row.backdrop_icon().is_some()
            {
                self.start_enter_splash_fade(row.play_key.clone(), now);
            }
            if self.enter_reveal.is_some() && !self.enter_reveal_armed() {
                self.arm_enter_reveal(now);
            }
            return;
        }
        if self.enter_reveal.is_none() || self.enter_reveal_armed() {
            return;
        }
        let Some((_, started)) = self.transition_phase else {
            return;
        };
        if now.saturating_duration_since(started) < Duration::from_millis(ENTER_ART_HOLD_MAX_MS) {
            return;
        }
        let hold_ms = now.saturating_duration_since(started).as_millis();
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: start immersive enter art hold timeout ms={hold_ms}"
        ));
        self.begin_games_list_reveal(now);
        if let Some(row) = self.rows.get(self.game_selected)
            && row.backdrop_icon().is_some()
        {
            self.reveal_cached_opaque(row.play_key.clone(), now);
        }
        self.arm_enter_reveal(now);
    }

    /// Advance the capped EnterImmersive clock. Call once per StartFrame.
    pub fn advance_enter_reveal(&mut self, now: Instant) {
        use crate::ui::start::mode::ENTER_REVEAL_MAX_FRAME_MS;
        let Some(clock) = self.enter_reveal.as_mut() else {
            return;
        };
        if !clock.armed {
            return;
        }
        let last = clock.last_tick.unwrap_or(now);
        let raw_ms = now.saturating_duration_since(last).as_millis() as u64;
        let dt = raw_ms.min(ENTER_REVEAL_MAX_FRAME_MS);
        if raw_ms > ENTER_REVEAL_MAX_FRAME_MS {
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: start immersive enter reveal capped raw_ms={raw_ms}"
            ));
        }
        clock.elapsed_ms = clock.elapsed_ms.saturating_add(dt);
        clock.last_tick = Some(now);
    }

    /// EnterImmersive veil alpha from the capped clock (1 = solid, 0 = clear).
    pub fn enter_veil_amount(&self) -> f32 {
        use crate::ui::start::mode::enter_veil_amount;
        match &self.enter_reveal {
            Some(clock) if clock.armed => enter_veil_amount(clock.elapsed_ms),
            Some(_) => 1.0,
            None => 0.0,
        }
    }

    pub fn phase_progress(&self, now: Instant) -> f32 {
        let Some((phase, started)) = self.transition_phase else {
            return 1.0;
        };
        if matches!(
            phase,
            crate::ui::start::mode::TransitionPhase::EnterImmersive
        ) {
            use crate::ui::start::mode::{ENTER_REVEAL_MS, phase_progress_linear};
            let elapsed = self
                .enter_reveal
                .as_ref()
                .filter(|c| c.armed)
                .map(|c| c.elapsed_ms)
                .unwrap_or(0);
            return phase_progress_linear(elapsed, ENTER_REVEAL_MS);
        }
        let elapsed = now.saturating_duration_since(started).as_millis() as u64;
        crate::ui::start::mode::phase_progress(elapsed, phase.duration_ms())
    }

    /// Linear phase fraction — use for EnterImmersive grow/chrome so ease-out cannot crush timing.
    pub fn phase_progress_linear(&self, now: Instant) -> f32 {
        let Some((phase, started)) = self.transition_phase else {
            return 1.0;
        };
        if matches!(
            phase,
            crate::ui::start::mode::TransitionPhase::EnterImmersive
        ) {
            use crate::ui::start::mode::{ENTER_REVEAL_MS, phase_progress_linear};
            let elapsed = self
                .enter_reveal
                .as_ref()
                .filter(|c| c.armed)
                .map(|c| c.elapsed_ms)
                .unwrap_or(0);
            return phase_progress_linear(elapsed, ENTER_REVEAL_MS);
        }
        let elapsed = now.saturating_duration_since(started).as_millis() as u64;
        crate::ui::start::mode::phase_progress_linear(elapsed, phase.duration_ms())
    }

    /// True when a timed phase (not Resizing) has reached its duration.
    pub fn phase_finished(&self, now: Instant) -> bool {
        let Some((phase, started)) = self.transition_phase else {
            return false;
        };
        let dur = phase.duration_ms();
        if dur == 0 {
            return false;
        }
        if matches!(
            phase,
            crate::ui::start::mode::TransitionPhase::EnterImmersive
        ) {
            return self
                .enter_reveal
                .as_ref()
                .is_some_and(|c| c.armed && c.elapsed_ms >= dur);
        }
        now.saturating_duration_since(started) >= Duration::from_millis(dur)
    }

    /// Drop crossfade/land state (demote / leave immersive without closing).
    pub fn clear_backdrop_transition(&mut self) {
        self.backdrop_current = None;
        self.backdrop_outgoing = None;
        self.splash_reveal_pending = false;
        self.splash_reveal_cached = false;
        self.enter_splash_fade = false;
        self.splash_opaque = false;
    }

    /// Arm backdrop for immersive enter with no prior splash on screen.
    ///
    /// Cached art: paint fully opaque **under** the enter veil so the new window's
    /// image atlas uploads during the blackout — holding at opacity 0 skips the
    /// draw, then after-veil snap shows ambient until the async upload lands.
    /// Uncached: wait for decode, then ease-in-out ambient → splash after the veil.
    pub fn prime_backdrop_for_enter(&mut self, now: Instant) {
        self.backdrop_outgoing = None;
        self.enter_splash_fade = false;
        self.splash_opaque = false;
        let Some(row) = self.rows.get(self.game_selected) else {
            self.backdrop_current = None;
            self.splash_reveal_pending = false;
            self.splash_reveal_cached = false;
            return;
        };
        let key = row.play_key.clone();
        if row.backdrop_icon().is_some() {
            // Warm hit: opaque under veil (or immediately if no veil).
            self.reveal_cached_opaque(key, now);
        } else {
            self.splash_reveal_pending = false;
            self.splash_reveal_cached = false;
            self.backdrop_current = Some((key.clone(), None));
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: backdrop enter prime key={key} mode=waiting"
            ));
        }
    }

    /// Cached splash at full opacity (reopen / warm hit under or after veil).
    fn reveal_cached_opaque(&mut self, key: String, now: Instant) {
        self.enter_splash_fade = false;
        self.splash_opaque = true;
        self.splash_reveal_pending = false;
        self.splash_reveal_cached = false;
        self.backdrop_current = Some((key.clone(), Some(now)));
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: backdrop reveal cached_opaque key={key}"
        ));
    }

    /// Uncached enter: ease-in-out ambient → splash.
    fn start_enter_splash_fade(&mut self, key: String, now: Instant) {
        self.enter_splash_fade = true;
        self.splash_opaque = false;
        self.splash_reveal_pending = false;
        self.splash_reveal_cached = false;
        self.backdrop_current = Some((key.clone(), Some(now)));
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: backdrop ready ambient_fade key={key}"
        ));
    }

    /// Title change: incoming fades in over a solid outgoing underlay.
    fn start_cover_crossfade(&mut self, key: String, now: Instant) {
        self.enter_splash_fade = false;
        self.splash_opaque = false;
        self.splash_reveal_pending = false;
        self.splash_reveal_cached = false;
        self.backdrop_current = Some((key.clone(), Some(now)));
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: backdrop crossfade cover key={key}"
        ));
    }

    /// Begin a held enter splash reveal once the veil phase ends.
    ///
    /// Cached reopen snaps opaque (no ambient). First decode eases ambient → splash.
    pub fn try_start_pending_splash_reveal(&mut self, now: Instant) {
        if !self.splash_reveal_pending || self.enter_veil_blocks_splash() {
            return;
        }
        let Some(row) = self.rows.get(self.game_selected) else {
            return;
        };
        if row.backdrop_icon().is_none() {
            return;
        }
        let key = row.play_key.clone();
        if self.splash_reveal_cached {
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: backdrop reveal after_veil cached key={key}"
            ));
            self.reveal_cached_opaque(key, now);
        } else {
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: backdrop reveal after_veil ambient key={key}"
            ));
            self.start_enter_splash_fade(key, now);
        }
    }

    /// Start backdrop land/crossfade once peek art exists for a waiting selection.
    pub fn note_backdrop_ready(&mut self, now: Instant) {
        let Some(row) = self.rows.get(self.game_selected) else {
            return;
        };
        if row.backdrop_icon().is_none() {
            return;
        }
        let key = row.play_key.clone();
        let waiting = matches!(&self.backdrop_current, Some((k, None)) if k == &key);
        let armed = matches!(&self.backdrop_current, Some((k, Some(_))) if k == &key);
        if waiting || self.splash_reveal_pending {
            if self.enter_veil_blocks_splash() {
                // Preserve splash_reveal_cached from prime (warm reopen → snap).
                // Uncached waiting already has it false → after-veil ambient fade.
                self.splash_reveal_pending = true;
                self.backdrop_current = Some((key.clone(), None));
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: backdrop ready hold_for_veil key={key} cached={}",
                    u8::from(self.splash_reveal_cached)
                ));
            } else if !armed {
                if self.backdrop_outgoing.is_some() {
                    self.start_cover_crossfade(key, now);
                } else if self.splash_reveal_cached {
                    self.reveal_cached_opaque(key, now);
                } else {
                    self.start_enter_splash_fade(key, now);
                }
            }
        } else if !armed {
            // Missing/stale key: start a fade. Do not restart when already armed
            // (enter prime / in-flight land) — that was the cached-splash blink.
            let was = self
                .backdrop_current
                .as_ref()
                .map(|(k, _)| k.as_str())
                .unwrap_or("none");
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: backdrop ready re-arm key={key} was={was}"
            ));
            if self.backdrop_outgoing.is_some() {
                self.start_cover_crossfade(key, now);
            } else {
                self.start_enter_splash_fade(key, now);
            }
        }
    }

    /// Visual params for the incoming backdrop: `(opacity, scale, ox_norm, oy_norm)`.
    pub fn backdrop_incoming_visual(&self, now: Instant) -> Option<(f32, f32, f32, f32)> {
        use crate::ui::start::mode::{
            art_fade_progress, backdrop_pose_offset, backdrop_pose_scale, enter_art_fade_progress,
        };
        let row = self.rows.get(self.game_selected)?;
        row.backdrop_icon()?;
        match &self.backdrop_current {
            Some((key, None)) if key == &row.play_key => {
                let (ox, oy) = backdrop_pose_offset(0);
                Some((0.0, backdrop_pose_scale(0), ox, oy))
            }
            Some((key, Some(started))) if key == &row.play_key => {
                let elapsed = now.saturating_duration_since(*started).as_millis() as u64;
                let opacity = if self.splash_opaque {
                    1.0
                } else if self.enter_splash_fade {
                    enter_art_fade_progress(elapsed)
                } else {
                    art_fade_progress(elapsed)
                };
                let scale = backdrop_pose_scale(elapsed);
                let (ox, oy) = backdrop_pose_offset(elapsed);
                Some((opacity, scale, ox, oy))
            }
            // No state, or stale key after compact nav — still paint current art.
            None | Some(_) => Some((1.0, backdrop_pose_scale(0), 0.0, 0.0)),
        }
    }

    /// Outgoing backdrop during cover-crossfade: `(play_key, opacity=1, scale, ox, oy)`.
    pub fn backdrop_outgoing_visual(&self, _now: Instant) -> Option<(&str, f32, f32, f32, f32)> {
        let (key, scale, ox, oy) = self.backdrop_outgoing.as_ref()?;
        Some((key.as_str(), 1.0, (*scale).max(1.0), *ox, *oy))
    }

    /// Resolve a peek backdrop handle by play_key (for outgoing layer).
    pub fn backdrop_icon_for_key(&self, play_key: &str) -> Option<StartIcon> {
        self.rows
            .iter()
            .find(|r| r.play_key == play_key)
            .and_then(|r| r.backdrop_icon())
    }

    /// Selected-row art to decode first on cold start.
    /// Returns `(hero_file, hero_shell, backdrop)`.
    pub fn immersive_priority_paths(&self) -> (Option<PathBuf>, Option<PathBuf>, Option<PathBuf>) {
        if self.rows.is_empty() {
            return (None, None, None);
        }
        let row = &self.rows[self.game_selected.min(self.rows.len() - 1)];
        let (hero, shell) = match &row.icon_source {
            Some(IconSource::File(path)) => (Some(path.clone()), None),
            Some(IconSource::Shell(path)) => (None, Some(path.clone())),
            None => (None, None),
        };
        (hero, shell, row.backdrop_path.clone())
    }

    /// Art paths for catalog indices within `radius` of the selection.
    ///
    /// Immersive uses a circular index window (strip wrap); compact is linear.
    pub fn art_paths_for_radius(&self, radius: isize) -> icon_cache::ArtRetainSet {
        self.art_paths_for_radius_circular(radius, self.immersive)
    }

    /// Like [`Self::art_paths_for_radius`] with an explicit circular/linear window.
    ///
    /// Promote pre-warm runs under ExitCompact (`immersive` still false) but needs the
    /// circular strip window so settle does not restart decode.
    pub fn art_paths_for_radius_circular(
        &self,
        radius: isize,
        circular: bool,
    ) -> icon_cache::ArtRetainSet {
        let mut set = icon_cache::ArtRetainSet::default();
        let indices =
            icon_cache::art_window_indices(self.game_selected, self.rows.len(), circular, radius);
        for idx in indices {
            let row = &self.rows[idx];
            match &row.icon_source {
                Some(IconSource::File(path)) => {
                    set.list_files.push(path.clone());
                    set.heroes.push(path.clone());
                }
                Some(IconSource::Shell(path)) => {
                    set.list_shells.push(path.clone());
                    set.hero_shells.push(path.clone());
                }
                None => {}
            }
            if let Some(path) = row.backdrop_path.clone() {
                set.backdrops.push(path);
            }
        }
        set
    }

    /// Full retain set: ±[`icon_cache::ART_WINDOW`] plus crossfade / dialog pins.
    pub fn art_retain_set(&self) -> icon_cache::ArtRetainSet {
        self.art_retain_set_circular(self.immersive)
    }

    /// Retain set with an explicit circular/linear near window (promote pre-warm).
    pub fn art_retain_set_circular(&self, circular: bool) -> icon_cache::ArtRetainSet {
        let mut set = self.art_paths_for_radius_circular(icon_cache::ART_WINDOW, circular);
        if let Some((key, _, _, _)) = self.backdrop_outgoing.as_ref()
            && let Some(path) = self
                .rows
                .iter()
                .find(|r| r.play_key == *key)
                .and_then(|r| r.backdrop_path.clone())
            && !set.backdrops.contains(&path)
        {
            set.backdrops.push(path);
        }
        if let Some(draft) = self.manual_add.as_ref() {
            if let Some(path) = draft.icon_path.clone()
                && !set.list_files.contains(&path)
            {
                set.list_files.push(path);
            }
            if !draft.target.starts_with("steam://") {
                let path = PathBuf::from(&draft.target);
                if !set.list_shells.contains(&path) {
                    set.list_shells.push(path);
                }
            }
        }
        set
    }

    /// Visible strip / near-list paths (±[`vstrip::NEIGHBORS`]) — warm immediately.
    pub fn art_near_set(&self) -> icon_cache::ArtRetainSet {
        use crate::ui::start::vstrip::NEIGHBORS;
        self.art_paths_for_radius(NEIGHBORS)
    }

    /// Near set with an explicit circular/linear window (promote pre-warm).
    pub fn art_near_set_circular(&self, circular: bool) -> icon_cache::ArtRetainSet {
        use crate::ui::start::vstrip::NEIGHBORS;
        self.art_paths_for_radius_circular(NEIGHBORS, circular)
    }

    /// Paths to warm for the current selection art window.
    /// Returns `(hero_files, hero_shells, backdrops)`.
    pub fn immersive_warm_paths(&self) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
        let set = self.art_retain_set();
        (set.heroes, set.hero_shells, set.backdrops)
    }

    /// Selection changed: park prior art as solid underlay and land the new one.
    pub fn reset_backdrop_fade(&mut self) {
        use crate::ui::start::mode::{backdrop_pose_offset, backdrop_pose_scale};
        let now = Instant::now();
        self.splash_reveal_pending = false;
        self.splash_reveal_cached = false;
        self.enter_splash_fade = false;
        self.splash_opaque = false;
        // Freeze full ken-burns pose — pan snap-to-center was a visible navigate glitch.
        let (frozen_scale, frozen_ox, frozen_oy) = match &self.backdrop_current {
            Some((_, Some(started))) => {
                let elapsed = now.saturating_duration_since(*started).as_millis() as u64;
                let (ox, oy) = backdrop_pose_offset(elapsed);
                (backdrop_pose_scale(elapsed).max(1.0), ox, oy)
            }
            _ => {
                let (ox, oy) = backdrop_pose_offset(0);
                (backdrop_pose_scale(0).max(1.0), ox, oy)
            }
        };
        if let Some((prev_key, _)) = self.backdrop_current.take() {
            // Cover-crossfade underlay — only if peek art exists for the prior key.
            if self.backdrop_icon_for_key(&prev_key).is_some() {
                self.backdrop_outgoing = Some((prev_key, frozen_scale, frozen_ox, frozen_oy));
            }
        }
        let key = self
            .rows
            .get(self.game_selected)
            .map(|r| r.play_key.clone());
        self.backdrop_current = key.map(|k| (k, None));
        self.note_backdrop_ready(now);
    }

    /// Start the UI ring flash that mirrors the lightbar Identify pattern.
    pub fn begin_identify_flash(&mut self, serial: &str) {
        self.identify_flash = Some(IdentifyFlash {
            serial: serial.to_string(),
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

    pub(crate) fn ring_flash_white(&self, serial: &str) -> bool {
        self.identify_flash.as_ref().is_some_and(|flash| {
            flash.serial == serial
                && lightbar::identify_flash_is_white(flash.started, Instant::now()) == Some(true)
        })
    }

    fn hint_anims_need_frames(&self) -> bool {
        const EPS: f32 = 0.001;
        let press = [
            (self.held.cross, self.press_anim.cross),
            (self.held.circle, self.press_anim.circle),
            (self.held.square, self.press_anim.square),
            (self.held.triangle, self.press_anim.triangle),
            (self.held.options, self.press_anim.options),
            (self.reopen_chord_held, self.reopen_chord_press),
        ];
        if press
            .iter()
            .any(|&(held, t)| (held && t < 1.0 - EPS) || (!held && t > EPS))
        {
            return true;
        }
        let triangle_armed = self.triangle_progress >= 1.0;
        let cross_armed = self.cross_progress >= 1.0;
        (triangle_armed && self.triangle_armed_anim < 1.0 - EPS)
            || (!triangle_armed && self.triangle_armed_anim > EPS)
            || (cross_armed && self.cross_armed_anim < 1.0 - EPS)
            || (!cross_armed && self.cross_armed_anim > EPS)
    }

    /// Advance pressed-scale and armed-color hint animations (~100ms).
    pub fn tick_hint_anims(&mut self, now: Instant) {
        let dt = self
            .hint_anim_tick
            .map(|t| now.saturating_duration_since(t).as_secs_f32())
            .unwrap_or(0.0)
            .min(0.05);
        self.hint_anim_tick = Some(now);
        if dt <= 0.0 {
            return;
        }
        approach_anim(&mut self.press_anim.cross, self.held.cross, dt);
        approach_anim(&mut self.press_anim.circle, self.held.circle, dt);
        approach_anim(&mut self.press_anim.square, self.held.square, dt);
        approach_anim(&mut self.press_anim.triangle, self.held.triangle, dt);
        approach_anim(&mut self.press_anim.options, self.held.options, dt);
        approach_anim(&mut self.reopen_chord_press, self.reopen_chord_held, dt);
        approach_anim(
            &mut self.triangle_armed_anim,
            self.triangle_progress >= 1.0,
            dt,
        );
        approach_anim(&mut self.cross_armed_anim, self.cross_progress >= 1.0, dt);
    }

    pub fn tick_anim(&mut self, now: Instant) -> bool {
        self.maybe_force_enter_reveal_after_art_hold(now);
        let _ = self.try_arm_enter_reveal_when_art_ready(now);
        self.advance_enter_reveal(now);
        self.try_start_pending_splash_reveal(now);
        self.tick_chrome_status(now);
        let mut busy = false;
        if !self.chrome_status.is_empty() {
            busy = true;
        }
        if let Some(started) = self.games_list_reveal_at {
            use crate::ui::start::mode::GAMES_LIST_FADE_MS;
            if now.saturating_duration_since(started) < Duration::from_millis(GAMES_LIST_FADE_MS) {
                busy = true;
            }
        }
        if let Some(anim) = self.anim.as_ref() {
            let elapsed = now.saturating_duration_since(anim.started);
            if elapsed >= Duration::from_millis(anim.duration_ms) {
                self.slide = anim.to;
                self.anim = None;
            } else {
                busy = true;
            }
        }
        if let Some(anim) = self.dock_anim.as_ref() {
            let elapsed = now.saturating_duration_since(anim.started);
            if elapsed >= Duration::from_millis(anim.duration_ms) {
                self.dock_expanded = anim.to >= 0.5;
                self.dock_anim = None;
            } else {
                busy = true;
            }
        }
        if self.settings.tick(now) {
            busy = true;
        }
        if let Some(anim) = self.strip_anim.as_ref() {
            let elapsed = now.saturating_duration_since(anim.started);
            if elapsed >= Duration::from_millis(anim.duration_ms) {
                self.strip_anim = None;
            } else {
                busy = true;
            }
        }
        // Continuous ken-burns + pending splash decode both need frames.
        if self.backdrop_current.is_some() {
            busy = true;
        }
        if self.backdrop_outgoing.is_some() {
            // Cover-crossfade: drop solid underlay once incoming is fully on.
            let incoming_done = self
                .backdrop_incoming_visual(now)
                .is_some_and(|(opacity, _, _, _)| opacity >= 0.99);
            if incoming_done {
                self.backdrop_outgoing = None;
            } else {
                busy = true;
            }
        }
        if self.transition_phase.is_some() {
            busy = true;
        }
        if self.cold_load_active {
            busy = true;
        }
        if self.immersive
            || self.transition.is_some()
            || self.transition_phase.is_some()
            || self.cold_load_active
        {
            self.ambient_time += 1.0 / 60.0;
            busy = true;
        }
        busy
    }

    /// Request a slide target. Interruptible: restarts from the current scroll position.
    /// No-op if already at / animating toward `to`.
    /// Returns `true` when a transition started or completed instantly after a move.
    pub fn request_slide(&mut self, to: StartSlide, now: Instant) -> bool {
        if self.anim.as_ref().is_some_and(|a| a.to == to) {
            return false;
        }
        if self.anim.is_none() && self.slide == to {
            return false;
        }

        let from_x = self.slide_scroll_x(now);
        let to_x = slide_x(to);
        let distance = (to_x - from_x).abs();
        if distance < 0.5 {
            if to != StartSlide::Games {
                self.editing = false;
                self.edit_anchor_play_key = None;
            }
            self.slide = to;
            self.anim = None;
            return false;
        }

        let duration_ms = ((SLIDE_ANIM_MS as f32) * (distance / PANE_W))
            .round()
            .clamp(SLIDE_ANIM_MIN_MS as f32, SLIDE_ANIM_MS as f32) as u64;

        self.replace_confirm = None;
        self.manual_add = None;
        self.cross_progress = 0.0;
        if to != StartSlide::Games {
            self.editing = false;
            self.edit_anchor_play_key = None;
        }
        self.anim = Some(SlideAnim {
            from_x,
            to,
            duration_ms,
            started: now,
        });
        true
    }

    pub fn move_selection(&mut self, delta: i32) -> Option<ScrollReveal> {
        if self.overlay_blocking() || self.anim.is_some() {
            return None;
        }
        match self.slide {
            StartSlide::Games => {
                if self.rows.is_empty() {
                    return None;
                }
                let len = self.rows.len() as i32;
                let before = self.game_selected as i32;
                let after = (before + delta).rem_euclid(len);
                if after == before {
                    return None;
                }
                self.game_selected = after as usize;
                Some(reveal_for_step(before, after, delta))
            }
            StartSlide::Controllers => {
                if self.controllers.is_empty() {
                    return None;
                }
                let len = self.controllers.len() as i32;
                let before = self.controller_selected as i32;
                let after = (before + delta).rem_euclid(len);
                if after == before {
                    return None;
                }
                self.controller_selected = after as usize;
                Some(reveal_for_step(before, after, delta))
            }
        }
    }

    pub fn selected_controller(&self) -> Option<&StartControllerRow> {
        self.controllers.get(self.controller_selected)
    }

    /// Horizontal scroll of the dual-pane strip (0 = Games, PANE_W = Controllers).
    fn slide_scroll_x(&self, now: Instant) -> f32 {
        let Some(anim) = self.anim.as_ref() else {
            return slide_x(self.slide);
        };
        let t = now.saturating_duration_since(anim.started).as_secs_f32()
            / (anim.duration_ms as f32 / 1000.0).max(0.001);
        let eased = window_layout::ease_out_cubic(t.clamp(0.0, 1.0));
        let to_x = slide_x(anim.to);
        anim.from_x + (to_x - anim.from_x) * eased
    }

    /// 0 = fully on Games, 1 = fully on Controllers (follows the carousel ease).
    pub(crate) fn slide_progress(&self, now: Instant) -> f32 {
        (self.slide_scroll_x(now) / PANE_W).clamp(0.0, 1.0)
    }
}

fn slide_x(slide: StartSlide) -> f32 {
    match slide {
        StartSlide::Games => 0.0,
        StartSlide::Controllers => PANE_W,
    }
}

pub fn games_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("start-games-scroll")
}

pub fn controllers_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("start-controllers-scroll")
}

/// Compute a new scroll Y so `index` stays fully visible, or `None` if already in view.
pub(crate) fn scroll_y_to_reveal(
    index: usize,
    len: usize,
    row_h: f32,
    gap: f32,
    scroll_y: f32,
    viewport_h: f32,
    direction: ScrollReveal,
) -> Option<f32> {
    if len == 0 || index >= len {
        return None;
    }
    let stride = row_h + gap;
    let row_top = index as f32 * stride;
    // Before the first on_scroll, fall back to a layout estimate — never force a
    // top pin on every step (that made controller nav look glued to the top).
    let viewport_h = if viewport_h <= 1.0 {
        estimated_list_viewport_h()
    } else {
        viewport_h
    };
    scroll_y_to_reveal_bounds(row_top, row_h, scroll_y, viewport_h, direction)
}

/// Pin-top / pin-bottom reveal for a row with known content bounds.
pub fn scroll_y_to_reveal_bounds(
    row_top: f32,
    row_h: f32,
    scroll_y: f32,
    viewport_h: f32,
    direction: ScrollReveal,
) -> Option<f32> {
    if row_h <= 0.0 || viewport_h <= 0.0 {
        return None;
    }
    let row_bottom = row_top + row_h;
    let view_top = scroll_y;
    let view_bottom = scroll_y + viewport_h;
    match direction {
        ScrollReveal::Down => {
            if row_bottom > view_bottom {
                Some((row_bottom - viewport_h).max(0.0))
            } else {
                None
            }
        }
        ScrollReveal::Up => {
            if row_top < view_top {
                Some(row_top.max(0.0))
            } else {
                None
            }
        }
        ScrollReveal::Either => {
            if row_top < view_top {
                Some(row_top.max(0.0))
            } else if row_bottom > view_bottom {
                Some((row_bottom - viewport_h).max(0.0))
            } else {
                None
            }
        }
    }
}

/// Approximate list viewport when iced has not reported bounds yet.
fn estimated_list_viewport_h() -> f32 {
    // Window minus vertical chrome: pads, header, rule, column gaps, footer.
    (HEIGHT - 2.0 * PAD_Y - HEADER_HEIGHT - 1.0 - 30.0 - FOOTER_HEIGHT).max(120.0)
}

/// Center `index` in the viewport.
///
/// When `force` is false, returns `None` if the row is already fully visible.
pub(crate) fn scroll_y_center(
    index: usize,
    len: usize,
    row_h: f32,
    gap: f32,
    scroll_y: f32,
    viewport_h: f32,
    force: bool,
) -> Option<f32> {
    if len == 0 || index >= len {
        return None;
    }
    let stride = row_h + gap;
    let row_top = index as f32 * stride;
    let row_bottom = row_top + row_h;
    let viewport_h = if viewport_h <= 1.0 {
        estimated_list_viewport_h()
    } else {
        viewport_h
    };
    if !force {
        let view_top = scroll_y;
        let view_bottom = scroll_y + viewport_h;
        if row_top >= view_top && row_bottom <= view_bottom {
            return None;
        }
    }
    Some((row_top - (viewport_h - row_h) / 2.0).max(0.0))
}

fn reveal_for_step(before: i32, after: i32, delta: i32) -> ScrollReveal {
    if delta > 0 && after < before {
        // Wrap last → first: pin to top.
        ScrollReveal::Up
    } else if delta < 0 && after > before {
        // Wrap first → last: pin to bottom.
        ScrollReveal::Down
    } else if delta < 0 {
        ScrollReveal::Up
    } else {
        ScrollReveal::Down
    }
}

/// Indeterminate scan spinner phase from a process-local epoch.
pub(crate) fn scan_spinner_phase(now: Instant) -> f32 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let epoch = *EPOCH.get_or_init(Instant::now);
    now.saturating_duration_since(epoch).as_secs_f32() * 2.8
}

/// Small indeterminate arc spinner (compact titlebar / immersive BR).
pub(crate) fn scan_spinner<'a>(size: f32, now: Instant, opacity: f32) -> Element<'a, StartMessage> {
    let phase = scan_spinner_phase(now);
    iced::widget::canvas(ScanSpinner {
        phase,
        size,
        opacity: opacity.clamp(0.0, 1.0),
    })
    .width(Length::Fixed(size))
    .height(Length::Fixed(size))
    .into()
}

struct ScanSpinner {
    phase: f32,
    size: f32,
    opacity: f32,
}

impl canvas::Program<StartMessage> for ScanSpinner {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let center = Point::new(bounds.width / 2.0, bounds.height / 2.0);
        let radius = (self.size / 2.0) - 2.0;
        let stroke_w = if self.size <= 16.0 { 1.75 } else { 2.25 };
        let track = Path::circle(center, radius);
        frame.stroke(
            &track,
            Stroke::default().with_width(stroke_w).with_color(Color {
                a: 0.22 * self.opacity,
                ..theme::MUTED
            }),
        );
        let sweep = std::f32::consts::TAU * 0.28;
        let start = self.phase;
        let end = start + sweep;
        let steps = 18usize;
        let arc = Path::new(|builder| {
            for i in 0..=steps {
                let t = i as f32 / steps as f32;
                let a = start + (end - start) * t;
                let p = Point::new(center.x + radius * a.cos(), center.y + radius * a.sin());
                if i == 0 {
                    builder.move_to(p);
                } else {
                    builder.line_to(p);
                }
            }
        });
        frame.stroke(
            &arc,
            Stroke::default().with_width(stroke_w).with_color(Color {
                a: 0.9 * self.opacity,
                ..theme::ACCENT
            }),
        );
        vec![frame.into_geometry()]
    }
}

/// Shared scan status chip (spinner or check + label) for compact titlebar / immersive BR.
pub(crate) fn scan_status_chip<'a>(
    spinner_size: f32,
    label_size: f32,
    now: Instant,
    visual: ChromeStatusVisual,
) -> Element<'a, StartMessage> {
    let op = visual.opacity.clamp(0.0, 1.0);
    let icon: Element<'_, StartMessage> = if visual.success {
        svg(svg::Handle::from_memory(svg_icon::CHECK_SVG.as_bytes()))
            .width(Length::Fixed(spinner_size))
            .height(Length::Fixed(spinner_size))
            .opacity(op)
            .style(|_theme, _status| svg::Style {
                color: Some(theme::SUCCESS),
            })
            .into()
    } else {
        scan_spinner(spinner_size, now, op)
    };
    let label = match (visual.kind, visual.success) {
        (ChromeStatusKind::PreparingArt, true) => "Artwork ready",
        (ChromeStatusKind::PreparingArt, false) => "Preparing artwork…",
        (ChromeStatusKind::SteamLibrary, true) => "Steam library synced",
        (ChromeStatusKind::SteamLibrary, false) => "Scanning Steam library…",
    };
    row![
        icon,
        text(label)
            .size(label_size)
            .wrapping(Wrapping::None)
            .color(theme::alpha(theme::MUTED, 0.9 * op)),
    ]
    .spacing(if spinner_size >= 20.0 { 12.0 } else { 6.0 })
    .align_y(Alignment::Center)
    .into()
}

/// Clip + slide one scan chip. `progress` 0 = below rest, 1 = resting (compact titlebar).
pub(crate) fn scan_status_slide<'a>(
    chip: Element<'a, StartMessage>,
    progress: f32,
    chip_h: f32,
    clip_h: f32,
    bottom_inset: f32,
) -> Element<'a, StartMessage> {
    let chip_h = chip_h.max(1.0);
    let bottom_inset = bottom_inset.max(0.0);
    let content_h = chip_h + bottom_inset;
    let clip_h = clip_h.max(content_h);
    let travel = content_h;
    let rest_gap = clip_h - content_h;
    let gap = rest_gap + (1.0 - progress.clamp(0.0, 1.0)) * travel;
    container(column![
        space().height(Length::Fixed(gap)),
        chip,
        space().height(Length::Fixed(bottom_inset)),
    ])
    .height(Length::Fixed(clip_h))
    .clip(true)
    .into()
}

/// Stack N chrome status chips, bottom-right anchored.
///
/// Clip height follows chip *count* (not animated slots) so inserts grow the rail
/// upward without shifting slot 0. New chips enter from below (slot &lt; 0, clipped).
/// ExitRight translates via [`Float`] so layout width stays chip-tight on the right edge.
pub(crate) fn scan_status_stack<'a>(
    chips: Vec<(ChromeStatusVisual, Element<'a, StartMessage>)>,
    chip_h: f32,
    bottom_inset: f32,
) -> Element<'a, StartMessage> {
    use crate::ui::start::mode::CHROME_STATUS_EXIT_TRAVEL_PX;
    use iced::Vector;

    let chip_h = chip_h.max(1.0);
    let bottom_inset = bottom_inset.max(0.0);
    let travel = CHROME_STATUS_EXIT_TRAVEL_PX.max(0.0);
    // Stable height from membership count — slot 0 stays put when N increases.
    let n = chips.len().max(1) as f32;
    let clip_h = bottom_inset + n * chip_h;

    let mut layers: Vec<Element<'_, StartMessage>> = Vec::new();
    for (visual, chip) in chips {
        let slot = visual.stack_slot;
        let y_from_bottom = bottom_inset + slot * chip_h;
        let gap_above = (clip_h - chip_h - y_from_bottom).max(0.0);
        let exit = visual.exit_x.clamp(0.0, 1.0);
        let dx = exit * travel;
        // Always Float while exiting so the first frames also translate (threshold
        // previously skipped early motion).
        let chip: Element<'_, StartMessage> = if exit > 0.001 {
            Float::new(chip)
                .translate(move |_bounds, _viewport| Vector::new(dx, 0.0))
                .into()
        } else {
            chip
        };
        layers.push(
            container(
                column![space().height(Length::Fixed(gap_above)), chip,]
                    .height(Length::Fixed(clip_h)),
            )
            .height(Length::Fixed(clip_h))
            .width(Fill)
            .align_x(Alignment::End)
            .into(),
        );
    }
    container(stack(layers).width(Fill).height(Length::Fixed(clip_h)))
        .width(Fill)
        .height(Length::Fixed(clip_h))
        .clip(true)
        .into()
}

pub fn view<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    now: Instant,
    always_immersive: bool,
    promote_gesture: &'a [crate::domain::gesture::GestureControl],
    stage_h: f32,
    settings_snapshot: &crate::ui::start::settings::StartSettingsSnapshot,
) -> Element<'a, StartMessage> {
    use crate::ui::start::mode::TransitionPhase;

    if matches!(
        state.transition_phase.map(|(p, _)| p),
        Some(TransitionPhase::Resizing)
    ) || state.transition.is_some()
    {
        return crate::ui::start::immersive::veil_view(state, now);
    }

    let chrome_status = state.chrome_load_status(now);
    let phase = state.transition_phase.map(|(p, _)| p);
    let show_immersive = state.immersive
        || matches!(
            phase,
            Some(TransitionPhase::EnterImmersive | TransitionPhase::ExitImmersive)
        );
    if show_immersive {
        // Enter/exit: immersive chrome under a single full-bleed veil (no compact flash).
        return crate::ui::start::immersive::view(
            state,
            spectrum,
            now,
            always_immersive,
            promote_gesture,
            stage_h,
            settings_snapshot,
        );
    }

    // Always keep the same outer stack as ExitCompact/EnterCompact so the games
    // scrollable is not remounted (and scroll reset) when promote begins.
    let veil = match phase {
        Some(TransitionPhase::ExitCompact) => state.phase_progress(now),
        Some(TransitionPhase::EnterCompact) => 1.0 - state.phase_progress(now),
        _ => 0.0,
    };
    // Flat hints under the dim veil — Float face glyphs would paint above it.
    let flat_hints = veil > 0.001;
    let primary = chrome_status
        .iter()
        .find(|v| !v.success && v.exit_x < 0.01)
        .copied()
        .or_else(|| chrome_status.first().copied());
    let compact = compact_chrome(
        state,
        spectrum,
        now,
        always_immersive,
        promote_gesture,
        flat_hints,
        primary,
    );
    let with_veil = crate::ui::start::immersive::compact_transition_overlay(compact, veil, false);
    let settings_p = state.settings.progress(now);
    if state.settings.visible(now) {
        stack![
            with_veil,
            crate::ui::start::settings::compact_overlay(state, settings_snapshot, settings_p),
        ]
        .width(Fill)
        .height(Fill)
        .into()
    } else {
        with_veil
    }
}

fn compact_chrome<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    now: Instant,
    always_immersive: bool,
    promote_gesture: &'a [crate::domain::gesture::GestureControl],
    flat_hints: bool,
    steam_scan_banner: Option<SteamScanBannerVisual>,
) -> Element<'a, StartMessage> {
    let header = slide_header(state.slide_progress(now), steam_scan_banner, now);

    let body = if state.manual_add.is_some() {
        manual_add_view(state)
    } else if state.replace_confirm.is_some() {
        replace_confirm_view(state)
    } else {
        carousel_body(state, spectrum, now)
    };

    let hint = footer_hint(state, false, always_immersive, promote_gesture, flat_hints);
    #[cfg(debug_assertions)]
    let diag = diag_report_bar();
    #[cfg(not(debug_assertions))]
    let diag: Element<'_, StartMessage> = space().height(Length::Fixed(0.0)).into();

    theme::framed(
        column![
            column![
                header,
                container(space())
                    .width(Fill)
                    .height(Length::Fixed(1.0))
                    .style(theme::configure_header_rule),
            ]
            .spacing(0)
            .width(Fill),
            container(body)
                .width(Fill)
                .height(Fill)
                .style(theme::content),
            diag,
            hint,
        ]
        .spacing(10)
        .padding([PAD_Y, PADDING])
        .width(Fill)
        .height(Fill),
    )
}

/// Identify / Power-off hints for an immersive dock row (mirrors compact Controllers).
/// Uses layout-stable glyphs (no Float scale) so dock row height cannot escape.
pub(crate) fn immersive_dock_row_hints(
    row: &StartControllerRow,
    state: &State,
    selected: bool,
) -> Option<Element<'static, StartMessage>> {
    if !selected || !row.connected {
        return None;
    }
    let hint = RowHintState::for_selected(state, true);
    let mut actions = vec![face_hint(
        FaceButton::Cross,
        "Identify",
        hint.held,
        hint.press_anim,
    )];
    if row.show_power_off() {
        actions.push(face_hold_hint(
            FaceButton::Triangle,
            "Power off",
            hint.triangle_progress,
            hint.triangle_armed_t,
            hint.held,
            hint.press_anim,
        ));
    }
    Some(action_cluster_dock(&actions))
}

/// Edit-mode membership mark for the immersive hero meta column.
pub(crate) fn immersive_game_membership_label(row: &StartRow) -> Element<'static, StartMessage> {
    let in_lib = row.in_catalog();
    let mark = if in_lib { "✓" } else { "○" };
    let mark_color = if in_lib {
        theme::ACCENT
    } else {
        theme::alpha(theme::MUTED, 0.7)
    };
    let label = if in_lib {
        "In library"
    } else {
        "Not in library"
    };
    // Match [`action_hint_dock`] label size + color.
    row![
        text(mark).size(14.0).color(mark_color),
        text(label).size(13.0).color(theme::alpha(theme::INK, 0.85)),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

/// Launch / Close game / edit face cues beside the immersive selected hero.
///
/// Layout-stable (`action_cluster_dock`) so veil/Float cannot resize the meta column.
pub(crate) fn immersive_game_hints(
    row: &StartRow,
    state: &State,
) -> Element<'static, StartMessage> {
    let hint = RowHintState::for_selected(state, true);
    let mut actions = Vec::new();
    if state.editing {
        let label = match row.edit.as_ref() {
            Some(EditRow::Manual { .. }) => "Remove",
            Some(EditRow::Steam { .. }) | None => "Toggle",
        };
        actions.push(face_hint(
            FaceButton::Cross,
            label,
            hint.held,
            hint.press_anim,
        ));
        if matches!(row.edit, Some(EditRow::Manual { .. })) {
            actions.push(face_hint(
                FaceButton::Square,
                "Edit",
                hint.held,
                hint.press_anim,
            ));
        }
    } else if state
        .running_target
        .as_ref()
        .is_some_and(|t| t == &row.target)
    {
        actions.push(face_hold_hint(
            FaceButton::Cross,
            "Close game",
            hint.cross_progress,
            hint.cross_armed_t,
            hint.held,
            hint.press_anim,
        ));
    } else {
        actions.push(face_hint(
            FaceButton::Cross,
            row.launch_hint_label(),
            hint.held,
            hint.press_anim,
        ));
    }
    action_cluster_dock(&actions)
}

/// Mouse + global hotkey hitch markers (not pad-navigable). Stamps `HITCH_MARK` in the logs.
/// Debug builds only.
#[cfg(debug_assertions)]
fn diag_report_bar() -> Element<'static, StartMessage> {
    row![
        button(
            text("Report lightbar write failure (F7)")
                .size(11.0)
                .color(theme::MUTED),
        )
        .padding([4, 8])
        .on_press(StartMessage::ReportLightbarFailure)
        .style(theme::ghost),
        button(
            text("Report input failure (F8)")
                .size(11.0)
                .color(theme::MUTED),
        )
        .padding([4, 8])
        .on_press(StartMessage::ReportInputFailure)
        .style(theme::ghost),
    ]
    .spacing(8)
    .into()
}

/// Header titles and L2/R2 cues interpolate with carousel progress (0 = Games, 1 = Controllers).
pub(crate) fn slide_header(
    progress: f32,
    steam_scan_banner: Option<SteamScanBannerVisual>,
    now: Instant,
) -> Element<'static, StartMessage> {
    slide_header_metrics(
        progress,
        HEADER_HEIGHT,
        TITLE_ACTIVE,
        TITLE_INACTIVE,
        CUE_SIZE,
        CUE_SLOT_W,
        steam_scan_banner,
        now,
    )
}

#[allow(clippy::too_many_arguments)]
fn slide_header_metrics(
    progress: f32,
    height: f32,
    title_active: f32,
    title_inactive: f32,
    cue_size: f32,
    cue_slot_w: f32,
    steam_scan_banner: Option<SteamScanBannerVisual>,
    now: Instant,
) -> Element<'static, StartMessage> {
    let games_t = progress;
    let controllers_t = 1.0 - progress;

    let games_label = text(StartSlide::Games.title())
        .size(lerp(title_active, title_inactive, games_t))
        .color(lerp_color(
            theme::INK,
            theme::alpha(theme::MUTED, 0.45),
            games_t,
        ));
    let controllers_label = text(StartSlide::Controllers.title())
        .size(lerp(title_active, title_inactive, controllers_t))
        .color(lerp_color(
            theme::INK,
            theme::alpha(theme::MUTED, 0.45),
            controllers_t,
        ));

    let left = row![
        slide_cue("L2", games_t, Alignment::Start, cue_size, cue_slot_w),
        games_label,
    ]
    .spacing(0)
    .align_y(Alignment::End);

    let right = row![
        controllers_label,
        slide_cue("R2", controllers_t, Alignment::End, cue_size, cue_slot_w),
    ]
    .spacing(0)
    .align_y(Alignment::End);

    let center: Element<'_, StartMessage> = if let Some(visual) = steam_scan_banner {
        // Full-header clip at the underline; chip rests above it and slides the whole way.
        let chip_h = 16.0;
        let progress = (visual.stack_slot + 1.0).clamp(0.0, 1.0);
        container(scan_status_slide(
            scan_status_chip(12.0, 11.0, now, visual),
            progress,
            chip_h,
            height,
            6.0,
        ))
        .width(Fill)
        .align_x(Alignment::Center)
        .into()
    } else {
        space().width(Length::Fixed(8.0)).into()
    };

    container(
        row![
            container(left)
                .width(Fill)
                .height(Fill)
                .align_x(Alignment::Start)
                .align_y(Alignment::End),
            center,
            container(right)
                .width(Fill)
                .height(Fill)
                .align_x(Alignment::End)
                .align_y(Alignment::End),
        ]
        .height(Length::Fixed(height)),
    )
    .width(Fill)
    .height(Length::Fixed(height))
    .clip(true)
    .into()
}

/// Width-revealed + faded L2/R2 cue (`amount` 0 = hidden, 1 = fully shown).
fn slide_cue(
    label: &'static str,
    amount: f32,
    align: Alignment,
    cue_size: f32,
    cue_slot_w: f32,
) -> Element<'static, StartMessage> {
    let amount = amount.clamp(0.0, 1.0);
    let width = cue_slot_w * amount;
    container(
        text(label)
            .size(cue_size)
            .color(theme::alpha(theme::ACCENT, amount)),
    )
    .width(Length::Fixed(width))
    .align_x(align)
    .clip(true)
    .into()
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color {
        r: lerp(a.r, b.r, t),
        g: lerp(a.g, b.g, t),
        b: lerp(a.b, b.b, t),
        a: lerp(a.a, b.a, t),
    }
}

fn carousel_body<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    now: Instant,
) -> Element<'a, StartMessage> {
    let scroll = state.slide_scroll_x(now);
    crate::ui::start::carousel::carousel(
        scroll,
        PANE_W,
        games_list(state),
        controllers_list(state, spectrum),
    )
    .width(Length::Fixed(PANE_W))
    .height(Fill)
    .into()
}

fn games_list(state: &State) -> Element<'_, StartMessage> {
    if state.rows.is_empty() {
        let empty: Element<'_, StartMessage> = if state.editing {
            text("No installed Steam games — add a manual shortcut below.")
                .size(15.0)
                .color(theme::MUTED)
                .into()
        } else {
            row![
                text("No games yet — press").size(15.0).color(theme::MUTED),
                face_svg(FaceButton::Triangle, FACE_GLYPH_SIZE),
                text("to edit.").size(15.0).color(theme::MUTED),
            ]
            .spacing(6)
            .align_y(Alignment::Center)
            .into()
        };
        return container(empty)
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill)
            .into();
    }
    let items = state.rows.iter().enumerate().fold(
        column![].spacing(0).width(Fill),
        |col, (index, row)| {
            let selected = index == state.game_selected;
            let running = !state.editing
                && state
                    .running_target
                    .as_ref()
                    .is_some_and(|t| t == &row.target);
            let col = if index > 0 {
                col.push(theme::list_separator())
            } else {
                col
            };
            col.push(game_row(
                index,
                row,
                selected,
                running,
                state.editing,
                RowHintState::for_selected(state, selected),
            ))
        },
    );
    scrollable(items)
        .id(games_scroll_id())
        .height(Fill)
        .width(Fill)
        .spacing(8)
        .on_scroll(|viewport| {
            let y = viewport.absolute_offset().y;
            StartMessage::GamesScrolled(y, viewport.bounds().height)
        })
        .into()
}

fn controllers_list<'a>(state: &'a State, spectrum: &BatterySpectrum) -> Element<'a, StartMessage> {
    if state.controllers.is_empty() {
        return container(
            text("No DualSense connected")
                .size(15.0)
                .color(theme::MUTED),
        )
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .into();
    }
    let items = state.controllers.iter().enumerate().fold(
        column![].spacing(0).width(Fill),
        |col, (index, row)| {
            let selected = index == state.controller_selected;
            let col = if index > 0 {
                col.push(theme::list_separator())
            } else {
                col
            };
            col.push(controller_row(
                index,
                row,
                selected,
                spectrum,
                state.ring_flash_white(&row.serial),
                RowHintState::for_selected(state, selected),
            ))
        },
    );
    scrollable(items)
        .id(controllers_scroll_id())
        .height(Fill)
        .width(Fill)
        .spacing(8)
        .on_scroll(|viewport| {
            let y = viewport.absolute_offset().y;
            StartMessage::ControllersScrolled(y, viewport.bounds().height)
        })
        .into()
}

pub(crate) fn manual_add_view(state: &State) -> Element<'_, StartMessage> {
    let Some(draft) = state.manual_add.as_ref() else {
        return space().into();
    };

    let icon_el: Element<'_, StartMessage> = draft
        .icon_path
        .as_ref()
        .filter(|p| p.is_file())
        .and_then(|p| icon_cache::handle_for_path_warn(p))
        .map(|handle| start_icon_image(&StartIcon(handle)))
        .or_else(|| draft.shell_icon.as_ref().map(start_icon_image))
        .unwrap_or_else(manual_icon_placeholder);

    let icon_hint = if draft.shell_icon.is_none() && draft.icon_path.is_none() {
        text("No icon found — choose an image (optional)")
            .size(12.0)
            .color(theme::DIM)
    } else {
        text(" ").size(12.0).color(theme::DIM)
    };

    let mut icon_actions = row![].spacing(8).align_y(Alignment::Center);
    icon_actions = icon_actions.push(
        button(text("Choose image…").size(13.0))
            .padding([5, 10])
            .on_press(StartMessage::ManualPickIcon)
            .style(theme::ghost),
    );
    if draft.icon_path.is_some() {
        icon_actions = icon_actions.push(
            button(text("Clear").size(13.0))
                .padding([5, 10])
                .on_press(StartMessage::ManualClearIcon)
                .style(theme::ghost),
        );
    }

    container(
        column![
            text(if draft.is_edit() {
                "Edit shortcut"
            } else {
                "Add shortcut"
            })
            .size(20.0)
            .color(theme::INK),
            space().height(6),
            text_input("Title", &draft.title)
                .size(14.0)
                .padding(8)
                .on_input(StartMessage::ManualAddTitle)
                .style(theme::input),
            text(&draft.target).size(12.0).color(theme::MUTED),
            text_input("Arguments (optional)", &draft.args)
                .size(14.0)
                .padding(8)
                .on_input(StartMessage::ManualAddArgs)
                .style(theme::input),
            row![icon_el, column![icon_actions, icon_hint].spacing(6)]
                .spacing(14)
                .align_y(Alignment::Center),
            space().height(8),
            row![
                button(text(if draft.is_edit() { "Save" } else { "Add" }).size(14.0))
                    .padding([6, 14])
                    .on_press(StartMessage::Confirm)
                    .style(theme::primary),
                button(text("Cancel").size(14.0))
                    .padding([6, 14])
                    .on_press(StartMessage::Close)
                    .style(theme::ghost),
            ]
            .spacing(10),
            {
                let press = HintPress::for_owner(state, HintPressOwner::ManualAdd);
                action_cluster(
                    &[
                        face_hint(
                            FaceButton::Cross,
                            if draft.is_edit() { "Save" } else { "Add" },
                            press.held,
                            press.press_anim,
                        ),
                        face_hint(FaceButton::Circle, "Cancel", press.held, press.press_anim),
                    ],
                    false,
                )
            },
        ]
        .spacing(10)
        .width(Length::Fixed(420.0))
        .align_x(Alignment::Start),
    )
    .width(Fill)
    .height(Fill)
    .center_x(Fill)
    .center_y(Fill)
    .into()
}

fn start_icon_image(icon: &StartIcon) -> Element<'static, StartMessage> {
    iced::widget::image(icon.0.clone())
        .width(Length::Fixed(ICON_W))
        .height(Length::Fixed(ICON_H))
        .content_fit(ContentFit::Cover)
        .into()
}

fn manual_icon_placeholder() -> Element<'static, StartMessage> {
    container(space())
        .width(Length::Fixed(ICON_W))
        .height(Length::Fixed(ICON_H))
        .style(|_t: &Theme| container::Style {
            background: Some(Background::Color(theme::alpha(theme::PANEL, 0.85))),
            border: Border {
                radius: 6.0.into(),
                ..Border::default()
            },
            ..container::Style::default()
        })
        .into()
}

pub(crate) fn replace_confirm_view(state: &State) -> Element<'_, StartMessage> {
    let Some(confirm) = state.replace_confirm.as_ref() else {
        return space().into();
    };
    container(
        column![
            text("Game already running").size(20.0).color(theme::INK),
            text(format!(
                "Close {} and launch {}?",
                confirm.running_title, confirm.next_title
            ))
            .size(14.0)
            .color(theme::MUTED),
            space().height(8),
            {
                let press = HintPress::for_owner(state, HintPressOwner::ReplaceConfirm);
                action_cluster(
                    &[
                        face_hold_hint(
                            FaceButton::Cross,
                            "Proceed",
                            press.cross_progress,
                            press.cross_armed_t,
                            press.held,
                            press.press_anim,
                        ),
                        face_hint(FaceButton::Circle, "Cancel", press.held, press.press_anim),
                    ],
                    false,
                )
            },
        ]
        .spacing(12)
        .align_x(Alignment::Center),
    )
    .width(Fill)
    .height(Fill)
    .center_x(Fill)
    .center_y(Fill)
    .into()
}

#[derive(Clone, Copy)]
enum FaceButton {
    Cross,
    Circle,
    Square,
    Triangle,
}

impl FaceButton {
    fn svg(self) -> &'static str {
        match self {
            Self::Cross => svg_icon::FACE_CROSS_SVG,
            Self::Circle => svg_icon::FACE_CIRCLE_SVG,
            Self::Square => svg_icon::FACE_SQUARE_SVG,
            Self::Triangle => svg_icon::FACE_TRIANGLE_SVG,
        }
    }

    fn held(self, held: FaceHeld) -> bool {
        match self {
            Self::Cross => held.cross,
            Self::Circle => held.circle,
            Self::Square => held.square,
            Self::Triangle => held.triangle,
        }
    }
}

struct ActionHint {
    face: Option<FaceButton>,
    text_glyph: Option<&'static str>,
    label: &'static str,
    hold: Option<f32>,
    /// 0..=1 blend of hold arc toward ink once armed (animated).
    armed_t: f32,
    /// Regular (non-hold) action: light the ring while the face button is pressed.
    pressed: bool,
    /// 0..=1 animated press amount (drives layout-stable scale).
    press_t: f32,
}

fn face_hint(
    face: FaceButton,
    label: &'static str,
    held: FaceHeld,
    press_anim: FacePressAnim,
) -> ActionHint {
    ActionHint {
        face: Some(face),
        text_glyph: None,
        label,
        hold: None,
        armed_t: 0.0,
        pressed: face.held(held),
        press_t: press_anim.for_face(face),
    }
}

fn face_hold_hint(
    face: FaceButton,
    label: &'static str,
    progress: f32,
    armed_t: f32,
    held: FaceHeld,
    press_anim: FacePressAnim,
) -> ActionHint {
    ActionHint {
        face: Some(face),
        text_glyph: None,
        label,
        hold: Some(progress),
        armed_t,
        pressed: face.held(held),
        press_t: press_anim.for_face(face),
    }
}

impl FacePressAnim {
    fn for_face(self, face: FaceButton) -> f32 {
        match face {
            FaceButton::Cross => self.cross,
            FaceButton::Circle => self.circle,
            FaceButton::Square => self.square,
            FaceButton::Triangle => self.triangle,
        }
    }
}

pub(crate) fn footer_hint<'a>(
    state: &'a State,
    immersive: bool,
    always_immersive: bool,
    promote_gesture: &'a [crate::domain::gesture::GestureControl],
    flat_hints: bool,
) -> Element<'a, StartMessage> {
    // Settings keeps its own footer inside the panel — main chrome footer stays browse/modals.
    if state.manual_add.is_some() {
        let press = HintPress::for_owner(state, HintPressOwner::ManualAdd);
        let label = if state.manual_add.as_ref().is_some_and(|d| d.is_edit()) {
            "Save"
        } else {
            "Add"
        };
        return footer_band(
            action_cluster(
                &[
                    face_hint(FaceButton::Cross, label, press.held, press.press_anim),
                    face_hint(FaceButton::Circle, "Cancel", press.held, press.press_anim),
                ],
                flat_hints,
            ),
            immersive,
        );
    }
    if state.replace_confirm.is_some() {
        let press = HintPress::for_owner(state, HintPressOwner::ReplaceConfirm);
        return footer_band(
            action_cluster(
                &[
                    face_hold_hint(
                        FaceButton::Cross,
                        "Proceed",
                        press.cross_progress,
                        press.cross_armed_t,
                        press.held,
                        press.press_anim,
                    ),
                    face_hint(FaceButton::Circle, "Cancel", press.held, press.press_anim),
                ],
                flat_hints,
            ),
            immersive,
        );
    }

    let press = HintPress::for_owner(state, HintPressOwner::Browse);
    let circle_label =
        crate::ui::start::mode::cancel_circle_label(immersive, always_immersive, state.editing);
    // Compact footer: reopen chord → Immersive. Immersive demote is Circle (Back) when
    // always-immersive is off — do not also advertise Compact on the reopen chord.
    let toggle_label = if !state.editing
        && !immersive
        && crate::ui::start::mode::promote_gesture_usable(promote_gesture)
    {
        Some("Immersive")
    } else {
        None
    };
    let promote_cue: Option<Element<'a, StartMessage>> = toggle_label.map(|label| {
        gesture_chord_hint(
            promote_gesture,
            label,
            press.reopen_held,
            press.reopen_press,
            flat_hints,
        )
    });

    let cluster: Element<'_, StartMessage> = match state.slide {
        StartSlide::Games => {
            let hints = [
                face_hint(
                    FaceButton::Triangle,
                    if state.editing { "Save" } else { "Edit" },
                    press.held,
                    press.press_anim,
                ),
                face_hint(
                    FaceButton::Circle,
                    circle_label,
                    press.held,
                    press.press_anim,
                ),
            ];
            if state.editing {
                // Add shortcut first, then Save / Cancel.
                iced::widget::row![
                    button(text("Add shortcut…").size(14.0).color(theme::ACCENT))
                        .padding([6, 12])
                        .on_press(StartMessage::AddShortcut)
                        .style(theme::ghost),
                    action_cluster(&hints, flat_hints),
                ]
                .spacing(28)
                .align_y(Alignment::Center)
                .into()
            } else {
                let mut row = iced::widget::row![face_cycle_toggle(
                    FaceButton::Square,
                    press.held,
                    press.press_anim,
                    &[
                        (
                            "Last played",
                            matches!(state.sort_mode, GamesSortMode::LastPlayed),
                        ),
                        (
                            "A–Z",
                            matches!(state.sort_mode, GamesSortMode::Alphabetical),
                        ),
                    ],
                    flat_hints,
                ),]
                .spacing(28)
                .align_y(Alignment::Center);
                if let Some(cue) = promote_cue {
                    row = row.push(cue);
                }
                row = row.push(options_hint_el(
                    "Settings",
                    press.held,
                    press.press_anim,
                    flat_hints,
                ));
                row.push(action_cluster(&hints, flat_hints)).into()
            }
        }
        StartSlide::Controllers => {
            let mut row = iced::widget::row![face_cycle_toggle(
                FaceButton::Square,
                press.held,
                press.press_anim,
                &[
                    ("Connected", !state.show_all_controllers),
                    ("All", state.show_all_controllers),
                ],
                flat_hints,
            ),]
            .spacing(28)
            .align_y(Alignment::Center);
            if let Some(cue) = promote_cue {
                row = row.push(cue);
            }
            row = row.push(options_hint_el(
                "Settings",
                press.held,
                press.press_anim,
                flat_hints,
            ));
            row.push(action_cluster(
                &[face_hint(
                    FaceButton::Circle,
                    circle_label,
                    press.held,
                    press.press_anim,
                )],
                flat_hints,
            ))
            .into()
        }
    };

    footer_band(cluster, immersive)
}

fn options_hint_el(
    label: &'static str,
    held: FaceHeld,
    press_anim: FacePressAnim,
    flat: bool,
) -> Element<'static, StartMessage> {
    let glyph = start_options_capsule(held.options, press_anim.options, flat);
    row![glyph, text(label).size(14.0).color(theme::MUTED)]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
}

/// Visible idle diameter of face / text rings (inset so ×[`PRESSED_SCALE`] stays in-slot).
fn hint_ring_idle_diameter() -> f32 {
    let half = HOLD_RING_SIZE / 2.0;
    let max_radius = half - 2.5;
    (max_radius / PRESSED_SCALE) * 2.0
}

/// DualSense Options → START capsule (visual height matches face-ring idle diameter).
fn start_options_capsule(
    pressed: bool,
    press_t: f32,
    flat: bool,
) -> Element<'static, StartMessage> {
    let label = text("START")
        .size(12.0)
        .font(Font::MONOSPACE)
        .color(theme::ACCENT);
    // Monospace caps sit high in the em box — nudge down for optical center.
    hint_text_capsule(label, 10.0, 1.0, pressed, press_t, flat)
}

/// Reopen-chord cue: short single control → circle; else one capsule with joined text.
fn gesture_chord_hint(
    controls: &[crate::domain::gesture::GestureControl],
    label: &'static str,
    pressed: bool,
    press_t: f32,
    flat: bool,
) -> Element<'static, StartMessage> {
    let glyph = if gesture_uses_circle(controls) {
        text_glyph_circle(controls[0].as_str(), pressed, press_t, flat)
    } else {
        text_glyph_capsule(
            crate::domain::gesture::format_gesture(controls),
            pressed,
            press_t,
            flat,
        )
    };
    row![glyph, text(label).size(14.0).color(theme::MUTED)]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
}

/// Circle for a single control whose label is at most 2 characters (e.g. PS, L3).
fn gesture_uses_circle(controls: &[crate::domain::gesture::GestureControl]) -> bool {
    matches!(controls, [c] if c.as_str().chars().count() <= 2)
}

fn text_glyph_circle(
    glyph: &'static str,
    pressed: bool,
    press_t: f32,
    flat: bool,
) -> Element<'static, StartMessage> {
    let half = HOLD_RING_SIZE / 2.0;
    let max_radius = half - 2.5;
    let idle_radius = max_radius / PRESSED_SCALE;
    let style = if pressed {
        RingStyle::Pressed
    } else {
        RingStyle::Idle
    };
    let stack = iced::widget::stack![
        action_ring(style, idle_radius),
        container(text(glyph).size(12.0).color(theme::ACCENT))
            .width(Length::Fixed(HOLD_RING_SIZE))
            .height(Length::Fixed(HOLD_RING_SIZE))
            .center_x(Fill)
            .center_y(Fill),
    ]
    .width(Length::Fixed(HOLD_RING_SIZE))
    .height(Length::Fixed(HOLD_RING_SIZE));
    if flat {
        stack.into()
    } else {
        let scale = 1.0 + (PRESSED_SCALE - 1.0) * press_t.clamp(0.0, 1.0);
        Float::new(stack).scale(scale).into()
    }
}

fn text_glyph_capsule(
    glyph: String,
    pressed: bool,
    press_t: f32,
    flat: bool,
) -> Element<'static, StartMessage> {
    hint_text_capsule(
        text(glyph).size(12.0).color(theme::ACCENT),
        10.0,
        0.0,
        pressed,
        press_t,
        flat,
    )
}

/// Pill glyph centered in a [`HOLD_RING_SIZE`] slot so height matches face rings.
///
/// `optical_top` adds extra top padding inside the pill (monospace caps often sit high).
fn hint_text_capsule(
    label: text::Text<'static>,
    pad_x: f32,
    optical_top: f32,
    pressed: bool,
    press_t: f32,
    flat: bool,
) -> Element<'static, StartMessage> {
    hint_element_capsule(label, pad_x, optical_top, pressed, press_t, flat)
}

/// Shared capsule chrome for text and multi-glyph pills.
///
/// Important: use [`Alignment::Center`], never `center_y(Fill)` — Fill expands the
/// capsule to the parent height (settings footer became a tall vertical pill).
fn hint_element_capsule(
    content: impl Into<Element<'static, StartMessage>>,
    pad_x: f32,
    optical_top: f32,
    pressed: bool,
    press_t: f32,
    flat: bool,
) -> Element<'static, StartMessage> {
    let border_color = if pressed {
        theme::ACCENT
    } else {
        Color {
            a: 0.25,
            ..theme::MUTED
        }
    };
    let border_w = if pressed { 2.5 } else { 2.0 };
    let pill_h = hint_ring_idle_diameter();
    let pill = container(content)
        .padding(Padding {
            top: optical_top,
            right: pad_x,
            bottom: 0.0,
            left: pad_x,
        })
        .height(Length::Fixed(pill_h))
        .align_y(Alignment::Center)
        .style(move |_theme| container::Style {
            border: Border {
                color: border_color,
                width: border_w,
                radius: (pill_h / 2.0).into(),
            },
            ..container::Style::default()
        });
    // Same outer slot as [`text_glyph_circle`] / face rings so labels share a baseline.
    let slot = container(pill)
        .height(Length::Fixed(HOLD_RING_SIZE))
        .align_y(Alignment::Center);
    if flat {
        slot.into()
    } else {
        let scale = 1.0 + (PRESSED_SCALE - 1.0) * press_t.clamp(0.0, 1.0);
        Float::new(slot).scale(scale).into()
    }
}

fn footer_band(content: Element<'_, StartMessage>, immersive: bool) -> Element<'_, StartMessage> {
    let height = if immersive {
        IMMERSIVE_FOOTER_HEIGHT
    } else {
        FOOTER_HEIGHT
    };
    // Immersive: shrink to the action cluster so the footer capsule hugs content.
    // Compact: fill the footer band as before.
    let band = container(content)
        .height(Length::Fixed(height))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center);
    if immersive {
        band.into()
    } else {
        band.width(Fill).into()
    }
}

/// Square glyph + segmented labels; pad Square cycles (presentational — no mouse).
fn face_cycle_toggle(
    face: FaceButton,
    held: FaceHeld,
    press_anim: FacePressAnim,
    options: &[(&'static str, bool)],
    flat: bool,
) -> Element<'static, StartMessage> {
    let dim = theme::alpha(theme::MUTED, 0.45);
    let glyph = face_glyph(
        face,
        if face.held(held) {
            RingStyle::Pressed
        } else {
            RingStyle::Idle
        },
        press_anim.for_face(face),
        flat,
    );
    let mut labels = row![].spacing(8).align_y(Alignment::Center);
    labels = labels.push(glyph);
    for (i, (label, active)) in options.iter().copied().enumerate() {
        if i > 0 {
            labels = labels.push(text("·").size(14.0).color(dim));
        }
        labels = labels.push(toggle_option_label(label, active));
    }
    labels.into()
}

fn toggle_option_label(label: &'static str, active: bool) -> Element<'static, StartMessage> {
    let color = if active {
        theme::INK
    } else {
        theme::alpha(theme::MUTED, 0.55)
    };
    text(label).size(14.0).color(color).into()
}

fn action_cluster(hints: &[ActionHint], flat: bool) -> Element<'static, StartMessage> {
    action_cluster_spaced(hints, 28.0, flat)
}

fn action_cluster_spaced(
    hints: &[ActionHint],
    spacing: f32,
    flat: bool,
) -> Element<'static, StartMessage> {
    let mut row = row![].spacing(spacing).align_y(Alignment::Center);
    for hint in hints {
        row = row.push(action_hint(hint, flat));
    }
    row.into()
}

/// Dock row cluster — no Float scale (keeps fixed row height).
fn action_cluster_dock(hints: &[ActionHint]) -> Element<'static, StartMessage> {
    let mut row = row![]
        .spacing(ROW_ACTION_SPACING)
        .align_y(Alignment::Center);
    for hint in hints {
        row = row.push(action_hint_dock(hint));
    }
    row.into()
}

fn action_hint(hint: &ActionHint, flat: bool) -> Element<'static, StartMessage> {
    let glyph: Element<'static, StartMessage> = if let Some(face) = hint.face {
        let style = if let Some(progress) = hint.hold {
            RingStyle::Hold {
                progress,
                armed_t: hint.armed_t,
            }
        } else if hint.pressed {
            RingStyle::Pressed
        } else {
            RingStyle::Idle
        };
        face_glyph(face, style, hint.press_t, flat)
    } else {
        text_glyph_circle(
            hint.text_glyph.unwrap_or("?"),
            hint.pressed,
            hint.press_t,
            flat,
        )
    };

    row![glyph, text(hint.label).size(14.0).color(theme::MUTED)]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
}

fn action_hint_dock(hint: &ActionHint) -> Element<'static, StartMessage> {
    let glyph: Element<'static, StartMessage> = if let Some(face) = hint.face {
        let style = if let Some(progress) = hint.hold {
            RingStyle::Hold {
                progress,
                armed_t: hint.armed_t,
            }
        } else if hint.pressed {
            RingStyle::Pressed
        } else {
            RingStyle::Idle
        };
        face_glyph_dock(face, style)
    } else {
        text(hint.text_glyph.unwrap_or("?"))
            .size(14.0)
            .color(theme::ACCENT)
            .into()
    };

    // Brighter than footer MUTED — dock/meta cues sit over translucent backdrop wash.
    row![
        glyph,
        text(hint.label)
            .size(13.0)
            .color(theme::alpha(theme::INK, 0.85)),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

fn face_svg(face: FaceButton, size: f32) -> Element<'static, StartMessage> {
    svg(svg::Handle::from_memory(face.svg().as_bytes()))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .into()
}

#[derive(Clone, Copy)]
enum RingStyle {
    Idle,
    Pressed,
    Hold { progress: f32, armed_t: f32 },
}

fn face_glyph(
    face: FaceButton,
    style: RingStyle,
    press_t: f32,
    flat: bool,
) -> Element<'static, StartMessage> {
    let stack = face_glyph_stack(face, style);
    if flat {
        return stack;
    }
    let press_t = press_t.clamp(0.0, 1.0);
    let scale = 1.0 + (PRESSED_SCALE - 1.0) * press_t;
    // Idle radius is inset so full press (× PRESSED_SCALE) + stroke stays inside the slot.
    // Geometry stays fixed; Float scales the stack so SVG/canvas are not re-rasterized each frame.
    Float::new(stack).scale(scale).into()
}

/// Dock-safe glyph: same art, no Float (scaled Float becomes an overlay and breaks row height).
fn face_glyph_dock(face: FaceButton, style: RingStyle) -> Element<'static, StartMessage> {
    face_glyph_stack(face, style)
}

fn face_glyph_stack(face: FaceButton, style: RingStyle) -> Element<'static, StartMessage> {
    let half = HOLD_RING_SIZE / 2.0;
    let max_radius = half - 2.5; // leave room for ~2.5px stroke
    let idle_radius = max_radius / PRESSED_SCALE;
    let glyph_size = FACE_GLYPH_SIZE * (idle_radius / (half - 2.0));
    iced::widget::stack![
        action_ring(style, idle_radius),
        container(face_svg(face, glyph_size))
            .width(Length::Fixed(HOLD_RING_SIZE))
            .height(Length::Fixed(HOLD_RING_SIZE))
            .center_x(Fill)
            .center_y(Fill),
    ]
    .width(Length::Fixed(HOLD_RING_SIZE))
    .height(Length::Fixed(HOLD_RING_SIZE))
    .into()
}

fn action_ring(style: RingStyle, radius: f32) -> Element<'static, StartMessage> {
    iced::widget::canvas(ActionRing { style, radius })
        .width(Length::Fixed(HOLD_RING_SIZE))
        .height(Length::Fixed(HOLD_RING_SIZE))
        .into()
}

struct ActionRing {
    style: RingStyle,
    radius: f32,
}

impl canvas::Program<StartMessage> for ActionRing {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let center = Point::new(bounds.width / 2.0, bounds.height / 2.0);
        let radius = self.radius;
        let track = Path::circle(center, radius);

        match self.style {
            RingStyle::Pressed => {
                frame.stroke(
                    &track,
                    Stroke::default().with_width(2.5).with_color(theme::ACCENT),
                );
            }
            RingStyle::Idle => {
                frame.stroke(
                    &track,
                    Stroke::default().with_width(2.0).with_color(Color {
                        a: 0.25,
                        ..theme::MUTED
                    }),
                );
            }
            RingStyle::Hold { progress, armed_t } => {
                frame.stroke(
                    &track,
                    Stroke::default().with_width(2.0).with_color(Color {
                        a: 0.25,
                        ..theme::MUTED
                    }),
                );
                if progress > 0.01 {
                    let start = -std::f32::consts::FRAC_PI_2;
                    let end = start + progress * std::f32::consts::TAU;
                    let steps = ((progress * 48.0).ceil() as usize).max(2);
                    let arc = Path::new(|builder| {
                        for i in 0..=steps {
                            let t = i as f32 / steps as f32;
                            let a = start + (end - start) * t;
                            let p = Point::new(
                                center.x + radius * a.cos(),
                                center.y + radius * a.sin(),
                            );
                            if i == 0 {
                                builder.move_to(p);
                            } else {
                                builder.line_to(p);
                            }
                        }
                    });
                    let arc_color = lerp_color(theme::ACCENT, theme::MUTED, armed_t);
                    frame.stroke(
                        &arc,
                        Stroke::default().with_width(2.5).with_color(arc_color),
                    );
                }
            }
        }
        vec![frame.into_geometry()]
    }
}

fn skeleton_bar(width: f32, height: f32) -> Element<'static, StartMessage> {
    container(space())
        .width(Length::Fixed(width))
        .height(Length::Fixed(height))
        .style(|_theme| container::Style {
            background: Some(Background::Color(theme::alpha(theme::MUTED, 0.22))),
            border: Border {
                radius: theme::RADIUS_SM.into(),
                ..Default::default()
            },
            ..container::Style::default()
        })
        .into()
}

/// Soft pill for pending Steam updates (compact accent; immersive grey).
pub(crate) fn update_required_badge(
    selected: bool,
    muted: bool,
    fade: f32,
    immersive: bool,
) -> Element<'static, StartMessage> {
    let fade = fade.clamp(0.0, 1.0);
    let a = if muted { 0.55 * fade } else { fade };
    let label_size = if selected { 13.0 } else { 11.0 };
    let (fill, stroke) = if immersive {
        (
            theme::alpha(theme::MUTED, 0.22 * a),
            theme::alpha(theme::MUTED, 0.40 * a),
        )
    } else {
        (
            theme::alpha(theme::ACCENT, 0.22 * a),
            theme::alpha(theme::ACCENT, 0.45 * a),
        )
    };
    container(
        text("Update required")
            .size(label_size)
            .color(theme::alpha(theme::INK, 0.92 * a))
            .font(Font {
                weight: Weight::Semibold,
                ..Font::DEFAULT
            }),
    )
    .padding(Padding {
        top: 3.0,
        right: 9.0,
        bottom: 3.0,
        left: 9.0,
    })
    .style(move |_theme: &Theme| container::Style {
        background: Some(Background::Color(fill)),
        border: Border {
            color: stroke,
            width: 1.0,
            radius: 999.0.into(),
        },
        ..container::Style::default()
    })
    .into()
}

/// Meta under a game title: plain path, or stacked `Label: value` lines.
pub(crate) fn game_subtitle_block<'a>(
    subtitle: &'a StartSubtitle,
    value_color: Color,
    label_color: Color,
    value_size: f32,
) -> Element<'a, StartMessage> {
    match subtitle {
        StartSubtitle::Plain(plain) => text(plain.as_str())
            .size(value_size)
            .color(value_color)
            .wrapping(Wrapping::Word)
            .width(Fill)
            .into(),
        StartSubtitle::Labeled(lines) => {
            let mut col = column![].spacing(2).width(Fill);
            for line in lines {
                col = col.push(
                    row![
                        text(format!("{}:", line.label))
                            .size(value_size)
                            .color(label_color),
                        text(line.value.as_str())
                            .size(value_size)
                            .color(value_color)
                            .wrapping(Wrapping::Word),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                );
            }
            col.into()
        }
    }
}

/// Compact combined status string (tests / fallbacks). UI prefers meta + badge.
#[cfg(test)]
fn compact_status_label(row: &StartRow) -> Option<String> {
    let show_meta = row
        .subtitle
        .as_ref()
        .is_some_and(|sub| !(row.update_required && sub.is_steam_fallback()));
    match (&row.subtitle, row.update_required, show_meta) {
        (None, false, _) => None,
        (None, true, _) | (Some(_), true, false) => Some("Update required".into()),
        (Some(StartSubtitle::Plain(text)), false, _) => Some(text.clone()),
        (Some(StartSubtitle::Plain(text)), true, true) => Some(format!("{text} · Update required")),
        (Some(StartSubtitle::Labeled(lines)), false, _) => Some(
            lines
                .iter()
                .map(|l| format!("{}: {}", l.label, l.value))
                .collect::<Vec<_>>()
                .join(" · "),
        ),
        (Some(StartSubtitle::Labeled(lines)), true, true) => Some(format!(
            "{} · Update required",
            lines
                .iter()
                .map(|l| format!("{}: {}", l.label, l.value))
                .collect::<Vec<_>>()
                .join(" · ")
        )),
    }
}

fn game_row(
    _index: usize,
    row: &StartRow,
    selected: bool,
    running: bool,
    editing: bool,
    hint: RowHintState,
) -> Element<'_, StartMessage> {
    let muted = editing && !row.in_catalog();
    let title_color = if muted {
        theme::alpha(theme::INK, 0.45)
    } else {
        theme::INK
    };
    let sub_color = if muted {
        theme::alpha(theme::MUTED, 0.55)
    } else {
        theme::MUTED
    };

    let icon_inner: Element<'_, StartMessage> = if row.skeleton {
        container(space())
            .width(Length::Fixed(ICON_W))
            .height(Length::Fixed(ICON_H))
            .style(theme::well)
            .into()
    } else {
        match row.list_icon_live() {
            Some(icon) => iced::widget::image(icon.0)
                .width(Length::Fixed(ICON_W))
                .height(Length::Fixed(ICON_H))
                .content_fit(ContentFit::Cover)
                .into(),
            None => container(
                svg(svg::Handle::from_memory(svg_icon::GAME_SVG.as_bytes()))
                    .width(Length::Fixed(ICON_W * 0.5))
                    .height(Length::Fixed(ICON_W * 0.5))
                    .style(|_theme, _status| svg::Style {
                        color: Some(theme::MUTED),
                    }),
            )
            .width(Length::Fixed(ICON_W))
            .height(Length::Fixed(ICON_H))
            .center_x(Fill)
            .center_y(Fill)
            .style(theme::well)
            .into(),
        }
    };

    let icon = container(icon_inner)
        .width(Length::Fixed(ICON_W))
        .height(Length::Fixed(ICON_H))
        .clip(true)
        .style(|_| container::Style {
            border: Border {
                radius: theme::RADIUS_SM.into(),
                ..Default::default()
            },
            ..container::Style::default()
        });

    let titles: Element<'_, StartMessage> = if row.skeleton {
        column![skeleton_bar(220.0, 14.0), skeleton_bar(160.0, 10.0),]
            .spacing(8)
            .width(Fill)
            .into()
    } else {
        let title_text = text(&row.title)
            .size(19.0)
            .color(title_color)
            .font(if selected {
                Font {
                    weight: Weight::Bold,
                    ..Font::DEFAULT
                }
            } else {
                Font::DEFAULT
            })
            .wrapping(Wrapping::None);

        // Update badge sits on the title row in compact (immersive keeps it under meta).
        let title: Element<'_, StartMessage> = if !running && row.update_required {
            row![
                title_text,
                update_required_badge(selected, muted, 1.0, false),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .width(Fill)
            .into()
        } else {
            title_text.width(Fill).into()
        };

        let status: Element<'_, StartMessage> = if running {
            text("Running")
                .size(13.0)
                .color(theme::SUCCESS)
                .wrapping(Wrapping::None)
                .width(Fill)
                .into()
        } else {
            let show_meta = row
                .subtitle
                .as_ref()
                .is_some_and(|sub| !(row.update_required && sub.is_steam_fallback()));
            if show_meta && let Some(sub) = row.subtitle.as_ref() {
                game_subtitle_block(sub, sub_color, theme::alpha(sub_color, 0.8), 12.0)
            } else {
                text(" ")
                    .size(13.0)
                    .color(Color::TRANSPARENT)
                    .wrapping(Wrapping::None)
                    .width(Fill)
                    .into()
            }
        };

        column![title, status]
            .spacing(4)
            .width(Fill)
            .clip(true)
            .into()
    };

    let mut content = row![icon].spacing(14).align_y(Alignment::Center);

    if editing {
        let mark = if row.in_catalog() { "✓" } else { "○" };
        let mark_color = if row.in_catalog() {
            theme::ACCENT
        } else {
            theme::alpha(theme::MUTED, 0.55)
        };
        content = content.push(
            text(mark)
                .size(20.0)
                .color(mark_color)
                .width(Length::Fixed(22.0)),
        );
    }

    content = content.push(titles);

    if selected {
        let mut actions = Vec::new();
        if editing {
            let label = match row.edit.as_ref() {
                Some(EditRow::Manual { .. }) => "Remove",
                Some(EditRow::Steam { .. }) | None => "Toggle",
            };
            actions.push(face_hint(
                FaceButton::Cross,
                label,
                hint.held,
                hint.press_anim,
            ));
            if matches!(row.edit, Some(EditRow::Manual { .. })) {
                actions.push(face_hint(
                    FaceButton::Square,
                    "Edit",
                    hint.held,
                    hint.press_anim,
                ));
            }
        } else {
            if running {
                actions.push(face_hold_hint(
                    FaceButton::Cross,
                    "Close game",
                    hint.cross_progress,
                    hint.cross_armed_t,
                    hint.held,
                    hint.press_anim,
                ));
            } else {
                actions.push(face_hint(
                    FaceButton::Cross,
                    row.launch_hint_label(),
                    hint.held,
                    hint.press_anim,
                ));
            }
        }
        content = content.push(action_cluster_spaced(&actions, ROW_ACTION_SPACING, false));
    }

    // Pad/keyboard navigate — no mouse press/hover on list rows (edit actions stay clickable).
    container(content.width(Fill).height(Length::Fixed(ROW_HEIGHT)))
        .padding([0, 12])
        .width(Fill)
        .height(Length::Fixed(ROW_HEIGHT))
        .style(theme::menu_row_surface(selected))
        .into()
}

fn controller_row<'a>(
    _index: usize,
    row: &'a StartControllerRow,
    selected: bool,
    spectrum: &BatterySpectrum,
    ring_flash_white: bool,
    hint: RowHintState,
) -> Element<'a, StartMessage> {
    let ring_color = if ring_flash_white {
        theme::from_rgb(lightbar::IDENTIFY_FLASH)
    } else if row.connected {
        theme::from_rgb(spectrum.color_at_percent(row.percent))
    } else {
        theme::DIM
    };
    let ring = percent_ring::percent_ring(
        row.percent,
        ring_color,
        POPUP_SIZE * 0.95,
        row.eta.clone(),
        1.0,
    );

    let meta_color = if row.low {
        theme::WARNING
    } else {
        theme::MUTED
    };
    let title_color = if row.connected {
        theme::INK
    } else {
        theme::MUTED
    };
    let titles = column![
        text(&row.title)
            .size(19.0)
            .color(title_color)
            .font(if selected {
                Font {
                    weight: Weight::Bold,
                    ..Font::DEFAULT
                }
            } else {
                Font::DEFAULT
            }),
        text(format!("{} · {}", row.connection, row.state))
            .size(13.0)
            .color(meta_color),
    ]
    .spacing(4)
    .width(Fill)
    .clip(true);

    let mut content = row![ring, titles].spacing(14).align_y(Alignment::Center);

    if selected && row.connected {
        let mut actions = vec![face_hint(
            FaceButton::Cross,
            "Identify",
            hint.held,
            hint.press_anim,
        )];
        if row.show_power_off() {
            actions.push(face_hold_hint(
                FaceButton::Triangle,
                "Power off",
                hint.triangle_progress,
                hint.triangle_armed_t,
                hint.held,
                hint.press_anim,
            ));
        }
        content = content.push(action_cluster_spaced(&actions, ROW_ACTION_SPACING, false));
    }

    container(
        content
            .width(Fill)
            .height(Length::Fixed(CONTROLLER_ROW_HEIGHT)),
    )
    .padding([0, 12])
    .width(Fill)
    .height(Length::Fixed(CONTROLLER_ROW_HEIGHT))
    .style(theme::menu_row_surface(selected))
    .into()
}

pub fn controller_title(product: &str, nickname: Option<&str>) -> String {
    nickname
        .filter(|n| !n.is_empty())
        .unwrap_or(product)
        .to_string()
}

pub fn power_state_label(state: PowerState) -> &'static str {
    state.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_controller(connected: bool, bluetooth: bool) -> StartControllerRow {
        StartControllerRow {
            serial: "pad".into(),
            title: "DualSense".into(),
            connection: if bluetooth { "Bluetooth" } else { "USB" }.into(),
            state: if connected {
                "charging"
            } else {
                "disconnected"
            }
            .into(),
            percent: 50,
            low: false,
            bluetooth,
            connected,
            eta: None,
        }
    }

    #[test]
    fn power_off_hint_only_for_live_bluetooth() {
        assert!(sample_controller(true, true).show_power_off());
        assert!(!sample_controller(true, false).show_power_off());
        assert!(!sample_controller(false, true).show_power_off());
        assert!(!sample_controller(false, false).show_power_off());
    }

    #[test]
    fn from_entry_skeleton_while_scan_pending() {
        let entry = GameEntry::Steam { appid: 1371980 };
        let empty = HashMap::new();
        let row = StartRow::from_entry(&entry, &empty, true);
        assert!(row.skeleton);
        assert!(row.title.is_empty());
        assert!(row.icon.is_none());
        assert_eq!(row.play_key, "steam:1371980");
        assert!(!row.target.is_empty());
    }

    #[test]
    fn from_entry_appid_fallback_after_scan() {
        let entry = GameEntry::Steam { appid: 1371980 };
        let empty = HashMap::new();
        let row = StartRow::from_entry(&entry, &empty, false);
        assert!(!row.skeleton);
        assert_eq!(row.title, "Steam 1371980");
        assert!(row.icon.is_none());
        assert_eq!(row.play_key, "steam:1371980");
        assert!(matches!(
            row.subtitle.as_ref(),
            Some(StartSubtitle::Plain(uri)) if uri == "steam://rungameid/1371980"
        ));
        assert!(!row.target.is_empty());
    }

    #[test]
    fn from_entry_resolved_steam_name() {
        let entry = GameEntry::Steam { appid: 238960 };
        let mut map = HashMap::new();
        map.insert(
            238960,
            SteamGame {
                appid: 238960,
                name: "Path of Exile".into(),
                icon_path: None,
                backdrop_path: None,
                playtime_minutes: None,
                last_played_unix: None,
                size_bytes: None,
                update_required: true,
            },
        );
        let row = StartRow::from_entry(&entry, &map, true);
        assert!(!row.skeleton);
        assert_eq!(row.title, "Path of Exile");
        assert!(
            row.subtitle
                .as_ref()
                .is_some_and(|sub| sub.is_steam_fallback())
        );
        assert!(row.update_required);
        assert_eq!(
            compact_status_label(&row).as_deref(),
            Some("Update required")
        );
        assert_eq!(row.launch_hint_label(), "Update & launch");
        assert!(row.icon.is_none());
    }

    #[test]
    fn scroll_y_reveals_below_and_above_viewport() {
        let gap = ROW_GAP;
        let stride = ROW_HEIGHT + gap;
        // Row 5 below viewport 300 starting at 0 → need scroll (Either/Down).
        let y =
            scroll_y_to_reveal(5, 20, ROW_HEIGHT, gap, 0.0, 300.0, ScrollReveal::Either).unwrap();
        assert!((y - (5.0 * stride + ROW_HEIGHT - 300.0)).abs() < 0.1);

        // Already visible near top with estimated/real viewport — no scroll.
        assert!(
            scroll_y_to_reveal(1, 20, ROW_HEIGHT, gap, 0.0, 300.0, ScrollReveal::Either).is_none()
        );
        // Unknown viewport uses estimate; early rows should still not force a top pin.
        assert!(scroll_y_to_reveal(1, 20, ROW_HEIGHT, gap, 0.0, 0.0, ScrollReveal::Down).is_none());

        // Scrolled past selection → scroll back up to row top.
        let y =
            scroll_y_to_reveal(2, 20, ROW_HEIGHT, gap, 400.0, 300.0, ScrollReveal::Either).unwrap();
        assert!((y - 2.0 * stride).abs() < 0.1);
    }

    #[test]
    fn scroll_y_leading_edge_only() {
        let gap = ROW_GAP;
        let stride = ROW_HEIGHT + gap;
        // Row above viewport: Down must not scroll; Up must pin to top.
        assert!(
            scroll_y_to_reveal(2, 20, ROW_HEIGHT, gap, 400.0, 300.0, ScrollReveal::Down).is_none()
        );
        let y = scroll_y_to_reveal(2, 20, ROW_HEIGHT, gap, 400.0, 300.0, ScrollReveal::Up).unwrap();
        assert!((y - 2.0 * stride).abs() < 0.1);

        // Row below viewport: Up must not scroll; Down must pin to bottom.
        assert!(scroll_y_to_reveal(5, 20, ROW_HEIGHT, gap, 0.0, 300.0, ScrollReveal::Up).is_none());
        let y = scroll_y_to_reveal(5, 20, ROW_HEIGHT, gap, 0.0, 300.0, ScrollReveal::Down).unwrap();
        assert!((y - (5.0 * stride + ROW_HEIGHT - 300.0)).abs() < 0.1);
    }

    #[test]
    fn reveal_for_step_wraps_flip_pin() {
        assert_eq!(reveal_for_step(9, 0, 1), ScrollReveal::Up);
        assert_eq!(reveal_for_step(0, 9, -1), ScrollReveal::Down);
        assert_eq!(reveal_for_step(3, 4, 1), ScrollReveal::Down);
        assert_eq!(reveal_for_step(4, 3, -1), ScrollReveal::Up);
    }

    #[test]
    fn scroll_y_center_when_out_of_view() {
        let gap = ROW_GAP;
        let stride = ROW_HEIGHT + gap;
        assert!(scroll_y_center(1, 20, ROW_HEIGHT, gap, 0.0, 300.0, false).is_none());
        let y = scroll_y_center(5, 20, ROW_HEIGHT, gap, 0.0, 300.0, false).unwrap();
        let expected = 5.0 * stride - (300.0 - ROW_HEIGHT) / 2.0;
        assert!((y - expected).abs() < 0.1);
        // Force recenters even when already visible.
        let y = scroll_y_center(1, 20, ROW_HEIGHT, gap, 0.0, 300.0, true).unwrap();
        let expected = (1.0_f32 * stride - (300.0 - ROW_HEIGHT) / 2.0).max(0.0);
        assert!((y - expected).abs() < 0.1);
    }

    #[test]
    fn immersive_priority_paths_are_selected_row_only() {
        let hero0 = PathBuf::from("hero0.png");
        let hero1 = PathBuf::from("hero1.png");
        let hero2 = PathBuf::from("hero2.png");
        let bd0 = PathBuf::from("bd0.png");
        let bd1 = PathBuf::from("bd1.png");
        let mut state = State {
            rows: vec![
                StartRow {
                    title: "g0".into(),
                    subtitle: None,
                    target: "t0".into(),
                    args: String::new(),
                    play_key: "k0".into(),
                    icon: None,
                    icon_source: Some(IconSource::File(hero0.clone())),
                    backdrop_path: Some(bd0.clone()),
                    edit: None,
                    skeleton: false,
                    update_required: false,
                },
                StartRow {
                    title: "g1".into(),
                    subtitle: None,
                    target: "t1".into(),
                    args: String::new(),
                    play_key: "k1".into(),
                    icon: None,
                    icon_source: Some(IconSource::File(hero1.clone())),
                    backdrop_path: Some(bd1.clone()),
                    edit: None,
                    skeleton: false,
                    update_required: false,
                },
                StartRow {
                    title: "g2".into(),
                    subtitle: None,
                    target: "t2".into(),
                    args: String::new(),
                    play_key: "k2".into(),
                    icon: None,
                    icon_source: Some(IconSource::File(hero2.clone())),
                    backdrop_path: None,
                    edit: None,
                    skeleton: false,
                    update_required: false,
                },
            ],
            game_selected: 0,
            ..Default::default()
        };

        let (hero, shell, backdrop) = state.immersive_priority_paths();
        assert_eq!(hero.as_deref(), Some(hero0.as_path()));
        assert!(shell.is_none());
        assert_eq!(backdrop.as_deref(), Some(bd0.as_path()));

        let (warm_heroes, warm_shells, warm_backdrops) = state.immersive_warm_paths();
        assert!(warm_shells.is_empty());
        // Neighbors include the selected path; priority decode runs first so the
        // rest job may hit a cache — selected must not be the sole warm entry.
        assert!(warm_heroes.contains(&hero0));
        assert!(warm_heroes.contains(&hero1));
        assert!(warm_backdrops.contains(&bd0));
        assert!(warm_backdrops.contains(&bd1));

        state.game_selected = 1;
        let (hero, shell, backdrop) = state.immersive_priority_paths();
        assert_eq!(hero.as_deref(), Some(hero1.as_path()));
        assert!(shell.is_none());
        assert_eq!(backdrop.as_deref(), Some(bd1.as_path()));
    }

    #[test]
    fn begin_strip_anim_uses_prior_index_when_settled() {
        let mut state = State {
            rows: (0..5)
                .map(|i| StartRow {
                    title: format!("g{i}"),
                    subtitle: None,
                    target: format!("t{i}"),
                    args: String::new(),
                    play_key: format!("k{i}"),
                    icon: None,
                    icon_source: None,
                    backdrop_path: None,
                    edit: None,
                    skeleton: false,
                    update_required: false,
                })
                .collect(),
            game_selected: 3,
            ..Default::default()
        };
        let now = Instant::now();
        // After selection advances, strip_scroll alone would equal `to` — must pass prior index.
        state.begin_strip_anim(2.0, now);
        let anim = state.strip_anim.as_ref().expect("settled step starts anim");
        assert!((anim.from - 2.0).abs() < 0.01);
        assert!((anim.to - 3.0).abs() < 0.01);
        assert_eq!(anim.duration_ms, crate::ui::start::vstrip::STRIP_ANIM_MS);

        // Mid-flight retarget uses live visual, not the stale from_visual hint.
        let mid = now + Duration::from_millis(crate::ui::start::vstrip::STRIP_ANIM_MS / 2);
        state.game_selected = 4;
        state.begin_strip_anim(0.0, mid);
        let anim = state.strip_anim.as_ref().expect("retarget keeps anim");
        assert!((anim.to - 4.0).abs() < 0.01);
        assert!(anim.from > 2.0 && anim.from < 3.0);
        assert_eq!(
            anim.duration_ms,
            crate::ui::start::vstrip::STRIP_ANIM_CATCHUP_MS
        );
    }

    fn write_tiny_png(path: &std::path::Path) {
        let mut enc = png::Encoder::new(std::fs::File::create(path).unwrap(), 2, 2);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer
            .write_image_data(&[
                255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
            ])
            .unwrap();
    }

    fn state_with_backdrop(path: PathBuf) -> State {
        State {
            rows: vec![StartRow {
                title: "g0".into(),
                subtitle: None,
                target: "t0".into(),
                args: String::new(),
                play_key: "k0".into(),
                icon: None,
                icon_source: None,
                backdrop_path: Some(path),
                edit: None,
                skeleton: false,
                update_required: false,
            }],
            game_selected: 0,
            immersive: true,
            ..Default::default()
        }
    }

    #[test]
    fn prime_backdrop_cached_snaps_opaque_without_restart() {
        icon_cache::with_cache_lock(|| {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("sdsc-backdrop-prime-cached-{n}"));
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("bd.png");
            write_tiny_png(&path);
            assert!(icon_cache::backdrop_for_path(&path).is_some());

            let mut state = state_with_backdrop(path);
            let now = Instant::now();
            state.prime_backdrop_for_enter(now);
            let (opacity, _, _, _) = state
                .backdrop_incoming_visual(now)
                .expect("cached enter paints");
            assert!((opacity - 1.0).abs() < 0.001);
            assert!(state.splash_opaque);
            assert!(!state.enter_splash_fade);

            // Late ImmersiveArtReady must not restart a fade from zero.
            state.note_backdrop_ready(now + Duration::from_millis(16));
            let (opacity, _, _, _) = state
                .backdrop_incoming_visual(now + Duration::from_millis(16))
                .expect("still paints");
            assert!((opacity - 1.0).abs() < 0.001);

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn prime_backdrop_uncached_waits_then_fades_from_zero() {
        icon_cache::with_cache_lock(|| {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("sdsc-backdrop-prime-cold-{n}"));
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("bd.png");
            write_tiny_png(&path);
            assert!(icon_cache::backdrop_cached(&path).is_none());

            let mut state = state_with_backdrop(path.clone());
            let now = Instant::now();
            state.prime_backdrop_for_enter(now);
            assert!(state.backdrop_incoming_visual(now).is_none());

            assert!(icon_cache::backdrop_for_path(&path).is_some());
            state.note_backdrop_ready(now);
            let (opacity, _, _, _) = state
                .backdrop_incoming_visual(now)
                .expect("fade starts once ready");
            assert!(opacity < 0.05);

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn note_backdrop_ready_missing_state_fades_cached() {
        icon_cache::with_cache_lock(|| {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("sdsc-backdrop-ready-miss-{n}"));
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("bd.png");
            write_tiny_png(&path);
            assert!(icon_cache::backdrop_for_path(&path).is_some());

            let mut state = state_with_backdrop(path);
            let now = Instant::now();
            assert!(state.backdrop_current.is_none());
            state.note_backdrop_ready(now);
            let (opacity, _, _, _) = state
                .backdrop_incoming_visual(now)
                .expect("missing+cached starts fade");
            assert!(opacity < 0.05);

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn enter_reveal_stays_solid_until_armed() {
        use crate::ui::start::mode::TransitionPhase;
        let mut state = State::default();
        let now = Instant::now();
        state.begin_transition_phase(TransitionPhase::EnterImmersive, now);
        assert!((state.enter_veil_amount() - 1.0).abs() < 0.001);
        assert!(!state.phase_finished(now + Duration::from_millis(500)));
        // Wall time alone must not advance the capped clock.
        state.advance_enter_reveal(now + Duration::from_millis(200));
        assert!((state.enter_veil_amount() - 1.0).abs() < 0.001);
    }

    #[test]
    fn enter_holds_veil_until_splash_and_strip_heroes_ready() {
        icon_cache::with_cache_lock(|| {
            use crate::ui::start::mode::TransitionPhase;
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("sdsc-enter-art-sync-{n}"));
            let _ = std::fs::create_dir_all(&dir);
            let hero = dir.join("hero.png");
            let bd = dir.join("bd.png");
            write_tiny_png(&hero);
            write_tiny_png(&bd);

            let mut state = State {
                rows: vec![StartRow {
                    title: "g".into(),
                    subtitle: None,
                    target: "t".into(),
                    args: String::new(),
                    play_key: "k".into(),
                    icon: None,
                    icon_source: Some(IconSource::File(hero.clone())),
                    backdrop_path: Some(bd.clone()),
                    edit: None,
                    skeleton: false,
                    update_required: false,
                }],
                game_selected: 0,
                immersive: true,
                steam_scan_pending: false,
                ..Default::default()
            };
            let now = Instant::now();
            state.begin_transition_phase(TransitionPhase::EnterImmersive, now);
            state.prime_backdrop_for_enter(now);
            assert!(!state.enter_art_sync_ready());
            assert!(!state.try_arm_enter_reveal_when_art_ready(now));
            assert!(!state.enter_reveal_armed());
            assert!((state.enter_veil_amount() - 1.0).abs() < 0.001);

            assert!(icon_cache::hero_for_path(&hero).is_some());
            assert!(icon_cache::backdrop_for_path(&bd).is_some());
            assert!(state.enter_art_sync_ready());
            assert!(state.try_arm_enter_reveal_when_art_ready(now));
            assert!(state.enter_reveal_armed());
            assert!(state.splash_opaque);
            assert!(
                state.games_list_reveal_at.is_some(),
                "art sync must begin list reveal"
            );

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn promote_enter_hides_list_until_art_sync() {
        use crate::ui::start::mode::TransitionPhase;
        let mut state = State {
            rows: vec![StartRow {
                title: "g".into(),
                subtitle: None,
                target: "t".into(),
                args: String::new(),
                play_key: "k".into(),
                icon: None,
                icon_source: None,
                backdrop_path: None,
                edit: None,
                skeleton: true,
                update_required: false,
            }],
            game_selected: 0,
            immersive: true,
            steam_scan_pending: true,
            ..Default::default()
        };
        let now = Instant::now();
        state.begin_transition_phase(TransitionPhase::EnterImmersive, now);
        state.prime_backdrop_for_enter(now);
        // Promote settle arms veil onto ambient immediately; list stays hidden.
        state.arm_enter_reveal(now);
        assert!(state.enter_reveal_armed());
        assert!(state.games_list_reveal_at.is_none());
        assert!((state.games_list_opacity(now) - 0.0).abs() < 0.001);
        assert!(!state.enter_art_sync_ready());
        assert!(!state.try_arm_enter_reveal_when_art_ready(now));
        assert!(state.games_list_reveal_at.is_none());
    }

    #[test]
    fn promote_enter_art_ready_reveals_list_while_veil_armed() {
        icon_cache::with_cache_lock(|| {
            use crate::ui::start::mode::TransitionPhase;
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("sdsc-promote-list-reveal-{n}"));
            let _ = std::fs::create_dir_all(&dir);
            let hero = dir.join("hero.png");
            let bd = dir.join("bd.png");
            write_tiny_png(&hero);
            write_tiny_png(&bd);
            assert!(icon_cache::hero_for_path(&hero).is_some());
            assert!(icon_cache::backdrop_for_path(&bd).is_some());

            let mut state = State {
                rows: vec![StartRow {
                    title: "g".into(),
                    subtitle: None,
                    target: "t".into(),
                    args: String::new(),
                    play_key: "k".into(),
                    icon: None,
                    icon_source: Some(IconSource::File(hero.clone())),
                    backdrop_path: Some(bd.clone()),
                    edit: None,
                    skeleton: false,
                    update_required: false,
                }],
                game_selected: 0,
                immersive: true,
                steam_scan_pending: false,
                ..Default::default()
            };
            let now = Instant::now();
            state.begin_transition_phase(TransitionPhase::EnterImmersive, now);
            state.prime_backdrop_for_enter(now);
            state.arm_enter_reveal(now);
            assert!(state.games_list_reveal_at.is_none());
            assert!(state.enter_art_sync_ready());
            assert!(state.try_arm_enter_reveal_when_art_ready(now));
            assert!(state.games_list_reveal_at.is_some());

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn promote_enter_list_hold_timeout_reveals_without_art() {
        use crate::ui::start::mode::{ENTER_ART_HOLD_MAX_MS, TransitionPhase};
        let mut state = State {
            rows: vec![StartRow {
                title: "g".into(),
                subtitle: None,
                target: "t".into(),
                args: String::new(),
                play_key: "k".into(),
                icon: None,
                icon_source: None,
                backdrop_path: None,
                edit: None,
                skeleton: true,
                update_required: false,
            }],
            game_selected: 0,
            immersive: true,
            ..Default::default()
        };
        let t0 = Instant::now();
        state.begin_transition_phase(TransitionPhase::EnterImmersive, t0);
        state.arm_enter_reveal(t0);
        assert!(state.games_list_reveal_at.is_none());
        let later = t0 + Duration::from_millis(ENTER_ART_HOLD_MAX_MS + 10);
        state.maybe_force_enter_reveal_after_art_hold(later);
        assert!(state.games_list_reveal_at.is_some());
    }

    #[test]
    fn enter_reveal_ease_in_out_midpoint_and_stall_cap() {
        use crate::ui::start::mode::{ENTER_REVEAL_MAX_FRAME_MS, ENTER_REVEAL_MS, TransitionPhase};
        let mut state = State::default();
        let t0 = Instant::now();
        state.begin_transition_phase(TransitionPhase::EnterImmersive, t0);
        state.arm_enter_reveal(t0);
        assert!((state.enter_veil_amount() - 1.0).abs() < 0.001);

        // A 200ms stall advances only one capped step.
        let mut now = t0 + Duration::from_millis(200);
        state.advance_enter_reveal(now);
        let after_stall = state.enter_reveal.as_ref().unwrap().elapsed_ms;
        assert_eq!(after_stall, ENTER_REVEAL_MAX_FRAME_MS);

        // Walk exactly to midpoint (capped steps) so ease-in-out lands at ~0.5.
        let midpoint = ENTER_REVEAL_MS / 2;
        while state.enter_reveal.as_ref().unwrap().elapsed_ms < midpoint {
            let remain = midpoint - state.enter_reveal.as_ref().unwrap().elapsed_ms;
            let step = remain.min(ENTER_REVEAL_MAX_FRAME_MS);
            now += Duration::from_millis(step);
            state.advance_enter_reveal(now);
        }
        assert_eq!(state.enter_reveal.as_ref().unwrap().elapsed_ms, midpoint);
        let mid = state.enter_veil_amount();
        assert!((mid - 0.5).abs() < 0.02, "mid veil={mid}");

        while !state.phase_finished(now) {
            now += Duration::from_millis(ENTER_REVEAL_MAX_FRAME_MS);
            state.advance_enter_reveal(now);
        }
        assert!(state.enter_veil_amount() < 0.01);
    }

    #[test]
    fn enter_paints_cached_splash_opaque_under_veil() {
        icon_cache::with_cache_lock(|| {
            use crate::ui::start::mode::{ENTER_REVEAL_MAX_FRAME_MS, TransitionPhase};
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("sdsc-enter-hold-splash-{n}"));
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("bd.png");
            write_tiny_png(&path);
            assert!(icon_cache::backdrop_for_path(&path).is_some());

            let mut state = state_with_backdrop(path);
            let t0 = Instant::now();
            state.begin_transition_phase(TransitionPhase::EnterImmersive, t0);
            state.arm_enter_reveal(t0);
            state.prime_backdrop_for_enter(t0);
            // Must paint under the veil (opacity > 0) so the new-window atlas uploads
            // before unveil — holding at 0 skips the draw and flashes ambient after.
            assert!(state.splash_opaque);
            assert!(!state.splash_reveal_pending);
            let (opacity, _, _, _) = state
                .backdrop_incoming_visual(t0)
                .expect("cached splash paints under veil");
            assert!((opacity - 1.0).abs() < 0.001);

            let mut now = t0;
            while !state.phase_finished(now) {
                now += Duration::from_millis(ENTER_REVEAL_MAX_FRAME_MS);
                state.advance_enter_reveal(now);
                state.try_start_pending_splash_reveal(now);
                let (opacity, _, _, _) = state.backdrop_incoming_visual(now).unwrap();
                assert!((opacity - 1.0).abs() < 0.001);
            }
            // Late ArtReady must not restart a fade from zero.
            state.note_backdrop_ready(now);
            state.clear_transition_phase();
            state.try_start_pending_splash_reveal(now);
            assert!(state.splash_opaque);
            assert!(!state.enter_splash_fade);
            let (opacity, _, _, _) = state
                .backdrop_incoming_visual(now)
                .expect("still opaque after veil");
            assert!((opacity - 1.0).abs() < 0.001);

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn cold_start_late_art_fades_ambient_to_splash() {
        icon_cache::with_cache_lock(|| {
            use crate::ui::start::mode::{
                ART_FADE_MS, ENTER_REVEAL_MAX_FRAME_MS, TransitionPhase, enter_art_fade_progress,
            };
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("sdsc-cold-ambient-fade-{n}"));
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("bd.png");
            write_tiny_png(&path);
            assert!(icon_cache::backdrop_cached(&path).is_none());

            let mut state = state_with_backdrop(path.clone());
            let t0 = Instant::now();
            state.begin_transition_phase(TransitionPhase::EnterImmersive, t0);
            state.arm_enter_reveal(t0);
            state.prime_backdrop_for_enter(t0);
            assert!(!state.splash_reveal_pending);
            assert!(state.backdrop_incoming_visual(t0).is_none());

            let mut now = t0;
            while !state.phase_finished(now) {
                now += Duration::from_millis(ENTER_REVEAL_MAX_FRAME_MS);
                state.advance_enter_reveal(now);
            }
            state.clear_transition_phase();

            assert!(icon_cache::backdrop_for_path(&path).is_some());
            state.note_backdrop_ready(now);
            assert!(state.enter_splash_fade);
            let (opacity, _, _, _) = state
                .backdrop_incoming_visual(now)
                .expect("late art starts fade");
            assert!(opacity < 0.05, "must fade from zero, got {opacity}");

            let mid = now + Duration::from_millis(ART_FADE_MS / 2);
            let (mid_opacity, _, _, _) = state.backdrop_incoming_visual(mid).expect("mid fade");
            assert!(
                (mid_opacity - enter_art_fade_progress(ART_FADE_MS / 2)).abs() < 0.02,
                "mid={mid_opacity}"
            );

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn title_change_cover_crossfade_keeps_outgoing_solid() {
        icon_cache::with_cache_lock(|| {
            use crate::ui::start::mode::ART_FADE_MS;
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("sdsc-cover-crossfade-{n}"));
            let _ = std::fs::create_dir_all(&dir);
            let path0 = dir.join("bd0.png");
            let path1 = dir.join("bd1.png");
            write_tiny_png(&path0);
            write_tiny_png(&path1);
            assert!(icon_cache::backdrop_for_path(&path0).is_some());
            assert!(icon_cache::backdrop_for_path(&path1).is_some());

            let mut state = State {
                rows: vec![
                    StartRow {
                        title: "g0".into(),
                        subtitle: None,
                        target: "t0".into(),
                        args: String::new(),
                        play_key: "k0".into(),
                        icon: None,
                        icon_source: None,
                        backdrop_path: Some(path0),
                        edit: None,
                        skeleton: false,
                        update_required: false,
                    },
                    StartRow {
                        title: "g1".into(),
                        subtitle: None,
                        target: "t1".into(),
                        args: String::new(),
                        play_key: "k1".into(),
                        icon: None,
                        icon_source: None,
                        backdrop_path: Some(path1),
                        edit: None,
                        skeleton: false,
                        update_required: false,
                    },
                ],
                game_selected: 0,
                immersive: true,
                ..Default::default()
            };
            use crate::ui::start::mode::backdrop_pose_scale;
            let now = Instant::now();
            state.prime_backdrop_for_enter(now);
            assert!(state.splash_opaque);

            state.game_selected = 1;
            state.reset_backdrop_fade();
            let mid = Instant::now() + Duration::from_millis(ART_FADE_MS / 2);
            let (out_key, out_opacity, out_scale, out_ox, out_oy) = state
                .backdrop_outgoing_visual(mid)
                .expect("outgoing underlay");
            assert_eq!(out_key, "k0");
            assert!(
                (out_opacity - 1.0).abs() < 0.001,
                "outgoing must stay solid"
            );
            let fresh = backdrop_pose_scale(0);
            assert!(
                (out_scale - fresh).abs() < 0.001,
                "fresh pose freezes at {fresh}, got {out_scale}"
            );
            assert!(out_ox.abs() < 0.001 && out_oy.abs() < 0.001);
            let (in_opacity, _, _, _) = state
                .backdrop_incoming_visual(mid)
                .expect("incoming fading");
            assert!(in_opacity > 0.4 && in_opacity < 1.0, "in={in_opacity}");
            assert!(!state.enter_splash_fade);

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn reset_backdrop_freezes_outgoing_ken_burns_scale() {
        icon_cache::with_cache_lock(|| {
            use crate::ui::start::mode::{BACKDROP_ZOOM_PERIOD_MS, backdrop_pose_scale};
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("sdsc-freeze-scale-{n}"));
            let _ = std::fs::create_dir_all(&dir);
            let path0 = dir.join("bd0.png");
            let path1 = dir.join("bd1.png");
            write_tiny_png(&path0);
            write_tiny_png(&path1);
            assert!(icon_cache::backdrop_for_path(&path0).is_some());
            assert!(icon_cache::backdrop_for_path(&path1).is_some());

            let mut state = State {
                rows: vec![
                    StartRow {
                        title: "g0".into(),
                        subtitle: None,
                        target: "t0".into(),
                        args: String::new(),
                        play_key: "k0".into(),
                        icon: None,
                        icon_source: None,
                        backdrop_path: Some(path0),
                        edit: None,
                        skeleton: false,
                        update_required: false,
                    },
                    StartRow {
                        title: "g1".into(),
                        subtitle: None,
                        target: "t1".into(),
                        args: String::new(),
                        play_key: "k1".into(),
                        icon: None,
                        icon_source: None,
                        backdrop_path: Some(path1),
                        edit: None,
                        skeleton: false,
                        update_required: false,
                    },
                ],
                game_selected: 0,
                immersive: true,
                ..Default::default()
            };
            let t0 = Instant::now();
            state.prime_backdrop_for_enter(t0);
            // Advance pose clock without wall-waiting: rewrite started into the past.
            let mid_pose = BACKDROP_ZOOM_PERIOD_MS / 4;
            if let Some((_, started)) = &mut state.backdrop_current {
                *started = Some(t0 - Duration::from_millis(mid_pose));
            }
            use crate::ui::start::mode::backdrop_pose_offset;
            let expected = backdrop_pose_scale(mid_pose);
            let (exp_ox, exp_oy) = backdrop_pose_offset(mid_pose);
            assert!(expected > 1.01, "test needs mid-pose scale, got {expected}");

            // Selection moves first (matches app), then reset parks prior pose.
            state.game_selected = 1;
            state.reset_backdrop_fade();
            let (_, _, frozen, ox, oy) = state
                .backdrop_outgoing_visual(Instant::now())
                .expect("outgoing");
            assert!(
                (frozen - expected).abs() < 0.005,
                "frozen={frozen} expected={expected}"
            );
            assert!(
                (ox - exp_ox).abs() < 0.005 && (oy - exp_oy).abs() < 0.005,
                "frozen pan=({ox},{oy}) expected=({exp_ox},{exp_oy})"
            );

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn immersive_warm_paths_include_neighbor_backdrops() {
        let bd: Vec<_> = (0..5)
            .map(|i| PathBuf::from(format!("bd{i}.png")))
            .collect();
        let state = State {
            rows: (0..5)
                .map(|i| StartRow {
                    title: format!("g{i}"),
                    subtitle: None,
                    target: format!("t{i}"),
                    args: String::new(),
                    play_key: format!("k{i}"),
                    icon: None,
                    icon_source: Some(IconSource::File(PathBuf::from(format!("h{i}.png")))),
                    backdrop_path: Some(bd[i].clone()),
                    edit: None,
                    skeleton: false,
                    update_required: false,
                })
                .collect(),
            game_selected: 2,
            ..Default::default()
        };
        let (_heroes, _, backdrops) = state.immersive_warm_paths();
        // Compact (linear) ±ART_WINDOW around selection 2 → all 5 rows.
        for path in &bd {
            assert!(
                backdrops.contains(path),
                "missing window backdrop {path:?} in {backdrops:?}"
            );
        }
    }

    #[test]
    fn art_retain_set_circular_when_immersive() {
        let state = State {
            rows: (0..30)
                .map(|i| StartRow {
                    title: format!("g{i}"),
                    subtitle: None,
                    target: format!("t{i}"),
                    args: String::new(),
                    play_key: format!("k{i}"),
                    icon: None,
                    icon_source: Some(IconSource::File(PathBuf::from(format!("h{i}.png")))),
                    backdrop_path: Some(PathBuf::from(format!("bd{i}.png"))),
                    edit: None,
                    skeleton: false,
                    update_required: false,
                })
                .collect(),
            game_selected: 0,
            immersive: true,
            ..Default::default()
        };
        let set = state.art_retain_set();
        let w = icon_cache::ART_WINDOW as usize;
        assert!(set.backdrops.contains(&PathBuf::from("bd0.png")));
        assert!(set.backdrops.contains(&PathBuf::from("bd29.png")));
        assert!(
            set.backdrops
                .contains(&PathBuf::from(format!("bd{}.png", 30 - w)))
        );
        assert!(!set.backdrops.contains(&PathBuf::from("bd15.png")));
    }

    #[test]
    fn list_icon_live_peeks_cache_without_row_snapshot() {
        icon_cache::with_cache_lock(|| {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("sdsc-list-live-{n}"));
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("icon.png");
            {
                let mut enc = png::Encoder::new(std::fs::File::create(&path).unwrap(), 2, 2);
                enc.set_color(png::ColorType::Rgba);
                enc.set_depth(png::BitDepth::Eight);
                let mut writer = enc.write_header().unwrap();
                writer
                    .write_image_data(&[
                        255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
                    ])
                    .unwrap();
            }
            let row = StartRow {
                title: "g".into(),
                subtitle: None,
                target: "t".into(),
                args: String::new(),
                play_key: "k".into(),
                icon: None,
                icon_source: Some(IconSource::File(path.clone())),
                backdrop_path: None,
                edit: None,
                skeleton: false,
                update_required: false,
            };
            assert!(row.list_icon_live().is_none());
            assert!(icon_cache::handle_for_path(&path).is_some());
            assert!(row.list_icon_live().is_some());
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn chrome_status_solo_success_exits_right_after_hold() {
        use crate::ui::start::mode::{
            CHROME_STATUS_EXIT_MS, CHROME_STATUS_IN_MS, CHROME_STATUS_STEP_HOLD_MS,
            CHROME_STATUS_SUCCESS_HOLD_MS,
        };
        let t0 = Instant::now();
        let mut state = State {
            immersive: true,
            ..State::default()
        };
        state.begin_chrome_status(ChromeStatusKind::PreparingArt, t0);
        state.tick_chrome_status(t0 + Duration::from_millis(CHROME_STATUS_IN_MS));
        state.note_chrome_status_success(ChromeStatusKind::PreparingArt, t0);
        let v = state.chrome_load_status(t0);
        assert_eq!(v.len(), 1);
        assert!(v[0].success);
        assert!((v[0].stack_slot - 0.0).abs() < 0.01);
        assert!(v[0].exit_x < 0.01);

        let hold = CHROME_STATUS_SUCCESS_HOLD_MS + CHROME_STATUS_STEP_HOLD_MS;
        let t_exit = t0 + Duration::from_millis(hold);
        state.tick_chrome_status(t_exit);
        let mid =
            state.chrome_load_status(t_exit + Duration::from_millis(CHROME_STATUS_EXIT_MS / 2));
        assert_eq!(mid.len(), 1);
        assert!(
            mid[0].exit_x > 0.2,
            "solo clears with slide-right, not lift"
        );
        assert!((mid[0].stack_slot - 0.0).abs() < 0.01);

        state.tick_chrome_status(t_exit + Duration::from_millis(CHROME_STATUS_EXIT_MS));
        assert!(
            state
                .chrome_load_status(t_exit + Duration::from_millis(CHROME_STATUS_EXIT_MS))
                .is_empty()
        );
    }

    #[test]
    fn chrome_status_new_job_stacks_success_upward() {
        use crate::ui::start::mode::CHROME_STATUS_IN_MS;
        let t0 = Instant::now();
        let mut state = State {
            immersive: true,
            ..State::default()
        };
        state.begin_chrome_status(ChromeStatusKind::PreparingArt, t0);
        state.tick_chrome_status(t0 + Duration::from_millis(CHROME_STATUS_IN_MS));
        state.note_chrome_status_success(ChromeStatusKind::PreparingArt, t0);
        assert!((state.chrome_load_status(t0)[0].stack_slot - 0.0).abs() < 0.01);

        // Steam begins while Preparing is Success — both stay; Preparing shifts up.
        state.begin_chrome_status(ChromeStatusKind::SteamLibrary, t0);
        let at_start = state.chrome_load_status(t0);
        let prep0 = at_start
            .iter()
            .find(|v| v.kind == ChromeStatusKind::PreparingArt)
            .unwrap();
        let steam0 = at_start
            .iter()
            .find(|v| v.kind == ChromeStatusKind::SteamLibrary)
            .unwrap();
        assert!(prep0.success);
        assert!(!steam0.success);
        // Reflow just started: Preparing ~0, Steam ~-1.
        assert!(prep0.stack_slot < 0.15);
        assert!(steam0.stack_slot < -0.85);

        let t_mid = t0 + Duration::from_millis(CHROME_STATUS_IN_MS / 2);
        let mid = state.chrome_load_status(t_mid);
        let prep = mid
            .iter()
            .find(|v| v.kind == ChromeStatusKind::PreparingArt)
            .unwrap();
        let steam = mid
            .iter()
            .find(|v| v.kind == ChromeStatusKind::SteamLibrary)
            .unwrap();
        assert!(
            prep.stack_slot > 0.5,
            "ease-out reflow should be past halfway at t=50%: {}",
            prep.stack_slot
        );
        assert!(
            steam.stack_slot > -0.5,
            "entering chip should be past halfway: {}",
            steam.stack_slot
        );

        state.tick_chrome_status(t0 + Duration::from_millis(CHROME_STATUS_IN_MS));
        let done = state.chrome_load_status(t0 + Duration::from_millis(CHROME_STATUS_IN_MS));
        let prep = done
            .iter()
            .find(|v| v.kind == ChromeStatusKind::PreparingArt)
            .unwrap();
        let steam = done
            .iter()
            .find(|v| v.kind == ChromeStatusKind::SteamLibrary)
            .unwrap();
        assert!((prep.stack_slot - 1.0).abs() < 0.01);
        assert!((steam.stack_slot - 0.0).abs() < 0.01);
        assert!(prep.exit_x < 0.01);
    }

    #[test]
    fn chrome_status_batch_exit_staggers_bottom_first() {
        use crate::ui::start::mode::{
            CHROME_STATUS_STAGGER_MS, CHROME_STATUS_STEP_HOLD_MS, CHROME_STATUS_SUCCESS_HOLD_MS,
        };
        let t0 = Instant::now();
        let mut state = State {
            immersive: true,
            ..State::default()
        };
        state.begin_chrome_status(ChromeStatusKind::PreparingArt, t0);
        state.begin_chrome_status(ChromeStatusKind::SteamLibrary, t0);
        // Force Active then Success without waiting In.
        for item in &mut state.chrome_status {
            item.phase = ChromeStatusPhase::Success;
            item.started = t0;
            item.stack_anim_started = None;
        }
        state.chrome_status[0].stack_slot = 0.0; // Steam (newer, bottom) after second begin
        state.chrome_status[1].stack_slot = 1.0; // Preparing
        state.note_chrome_status_maybe_all_success(t0);

        let hold = CHROME_STATUS_SUCCESS_HOLD_MS + CHROME_STATUS_STEP_HOLD_MS;
        let t_batch = t0 + Duration::from_millis(hold);
        state.tick_chrome_status(t_batch);
        // Bottom (index 0) is ExitRight; top still Success until stagger.
        assert_eq!(state.chrome_status[0].phase, ChromeStatusPhase::ExitRight);
        assert_eq!(state.chrome_status[1].phase, ChromeStatusPhase::Success);

        let t_stagger = t_batch + Duration::from_millis(CHROME_STATUS_STAGGER_MS);
        state.tick_chrome_status(t_stagger);
        assert_eq!(state.chrome_status[1].phase, ChromeStatusPhase::ExitRight);
        let v = state.chrome_load_status(t_stagger);
        let bottom = v
            .iter()
            .find(|x| x.kind == state.chrome_status[0].kind)
            .unwrap();
        let top = v
            .iter()
            .find(|x| x.kind == state.chrome_status[1].kind)
            .unwrap();
        assert!(bottom.exit_x > top.exit_x);
    }

    #[test]
    fn chrome_status_third_kind_blocks_batch_exit() {
        use crate::ui::start::mode::{CHROME_STATUS_STEP_HOLD_MS, CHROME_STATUS_SUCCESS_HOLD_MS};
        let t0 = Instant::now();
        let mut state = State::default();
        state.begin_chrome_status(ChromeStatusKind::PreparingArt, t0);
        state.begin_chrome_status(ChromeStatusKind::SteamLibrary, t0);
        for item in &mut state.chrome_status {
            item.phase = ChromeStatusPhase::Success;
            item.started = t0;
            item.stack_anim_started = None;
        }
        state.note_chrome_status_maybe_all_success(t0);
        // Simulate a third kind by re-using Steam after clearing it — instead push via
        // temporarily renaming: begin no-ops duplicates, so clear Steam then re-add as Active.
        // Use Preparing+Steam Success then begin is blocked; force-insert Active stand-in by
        // flipping Steam back to Active.
        state.chrome_status[0].phase = ChromeStatusPhase::Active;
        state.chrome_status_all_success_at = None;

        let hold = CHROME_STATUS_SUCCESS_HOLD_MS + CHROME_STATUS_STEP_HOLD_MS + 500;
        state.tick_chrome_status(t0 + Duration::from_millis(hold));
        assert!(
            state
                .chrome_status
                .iter()
                .all(|i| i.phase != ChromeStatusPhase::ExitRight),
            "batch exit must wait until every chip is Success"
        );
    }
}
