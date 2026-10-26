//! Start screen: fullscreen console game launcher, DualSense / keyboard navigable.

use crate::controller::dualsense::lightbar;
use crate::controller::model::{PowerState, Serial};
use crate::games::GameEntry;
use crate::games::steam::SteamGame;
use crate::persist::prefs::{
    GamesSortMode, ImmersiveLayout, NavbarVisibility, ReducedMotion, StartAutoOpen,
    StartSelectionMode,
};
use crate::ui::color::BatterySpectrum;
use crate::ui::layout as window_layout;
use crate::ui::start::face::FacePressAnim;
use crate::ui::start::icon_cache;
use crate::ui::start::input::FaceHeld;
use crate::ui::svg_icon;
use crate::ui::theme;
use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::widget::text::Wrapping;
use iced::widget::{Float, column, container, row, space, stack, svg, text};
use iced::{Alignment, Color, Element, Fill, Length, Point, Rectangle, Renderer, Theme};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Logical width of the start-screen window.
pub const WIDTH: f32 = 768.0;

pub const DOCK_ANIM_MS: u64 = 220;

/// Layout-transition duration when swapping immersive strip orientation
/// (ease in-out, orientation swaps at the midpoint).
pub const LAYOUT_TRANSITION_MS: u64 = 300;
/// Peak slide distance (logical px) of the directional exit+enter: the old
/// orientation slides out along its scroll axis while fading, the new one
/// slides in from the opposite side while fading back in.
const LAYOUT_TRANSITION_SLIDE_PX: f32 = 72.0;
/// Portrait cell matching Steam library capsules (2:3).
pub(crate) const ICON_W: f32 = 48.0;
pub(crate) const ICON_H: f32 = 72.0;
/// Pressed grow and armed-color transitions.
const HINT_ANIM: Duration = Duration::from_millis(crate::ui::motion::FAST_MS);
pub(crate) const ROW_ACTION_SPACING: f32 = 14.0;

#[derive(Debug, Clone)]
pub enum StartMessage {
    Launch(usize),
    /// Immersive strip: select a game without launching.
    SelectGame(usize),

    MoveUp,
    MoveDown,
    Confirm,
    Close,
    /// Create (Select): toggle the controllers surface.
    ToggleControllers,
    ToggleEdit,
    CycleSort,
    AddShortcut,
    /// Open the add/edit modal for the selected manual (edit mode).
    EditManual,

    SettingsScrolled(f32, f32),
    ManualAddTitle(String),
    ManualAddArgs(String),
    ManualPickIcon,
    ManualClearIcon,
    /// Options — open/close in-window Start settings.
    ToggleSettings,
    SetAutoOpen(StartAutoOpen),
    ToggleAutoOpenMenu,
    ToggleLayoutMenu,
    ToggleNavbarMenu,
    ToggleReducedMotionMenu,
    ToggleSelectionModeMenu,
    SetSelectionPersist(bool),
    SetSelectionMode(StartSelectionMode),

    SetClock(bool),
    SetImmersiveLayout(ImmersiveLayout),
    SetNavbarVisibility(NavbarVisibility),
    SetReducedMotion(ReducedMotion),
    SetInactiveSecs(u32),
    SetSleepSecs(u32),
    SetInactiveDimPercent(u8),
    SetSounds(bool),
    SetSoundVolume(u8),
    SetHaptics(bool),
    SetHapticsStrength(u8),
    SetPerfThrottle(bool),
    SetPerfAntialiasing(bool),
    SetPerfVsync(bool),
    /// Reload the UI to apply boot-only renderer prefs (AA/vsync).
    ReloadUi,
    SetShowUninstalledGames(bool),
    /// Rescan the Steam library now (no-op while a scan is in flight).
    SyncSteamLibrary,
    /// Debug-only: open the in-immersive ambience tuning panel.
    #[cfg(debug_assertions)]
    OpenAmbientTuning,
    /// Debug-only: close the tuning panel (the shell reopens the drawer).
    #[cfg(debug_assertions)]
    CloseAmbientTuning,
    /// Debug-only: absolute tuning commit from the tuning panel (pad + mouse).
    #[cfg(debug_assertions)]
    SetAmbientTune(crate::ui::start::ambient_tune::AmbientTuneParam, f32),
    /// Debug-only: restore shipped tuning from the tuning panel.
    #[cfg(debug_assertions)]
    ResetAmbientTuning,
    /// Debug-only: tuning list scroll report (focus-follow + viewport track).
    #[cfg(debug_assertions)]
    AmbientTuneScrolled(f32, f32),
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
    /// Scan determined this target cannot launch (manual file missing). Row is
    /// dimmed and cannot launch. Steam rows are never disabled: unlaunchable
    /// ones stay enabled with `view_in_steam` set.
    pub disabled: bool,
    /// Steam row Steam cannot launch: Cross opens the game's store page
    /// instead (`Some(appid)`). Set for scan-proven-uninstalled rows and
    /// pre-flight-blocked rows; `None` for launchable rows, skeletons, and
    /// manual entries. Rows with this set stay enabled and navigable.
    pub view_in_steam: Option<u32>,
    /// Effective last-played time (catalog touch or Steam), milliseconds. `None` = never.
    pub played_at_ms: Option<u64>,
}

/// Edit-mode action for a games-list row.
#[derive(Debug, Clone)]
pub enum EditRow {
    Steam { appid: u32, in_catalog: bool },
    Manual { id: String },
}

impl StartRow {
    pub(crate) fn launch_hint_label(&self) -> &'static str {
        if self.view_in_steam.is_some() {
            "View in Steam"
        } else if self.update_required {
            "Update & launch"
        } else {
            "Launch"
        }
    }

    pub fn from_entry(
        entry: &GameEntry,
        steam_by_id: &HashMap<u32, SteamGame>,
        steam_scan_pending: bool,
        remembered_name: Option<&str>,
        known_missing: bool,
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
                        disabled: false,
                        view_in_steam: None,
                        played_at_ms: None,
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
                        disabled: false,
                        view_in_steam: None,
                        played_at_ms: None,
                    }
                } else {
                    // Live scan missed this appid: prefer the last-known name
                    // over the bare `Steam {appid}` fallback, and explain why
                    // instead of leaking the launch URI as the subtitle.
                    let title = remembered_name
                        .filter(|name| !name.trim().is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("Steam {appid}"));
                    let subtitle = if known_missing {
                        StartSubtitle::Plain("Not installed".into())
                    } else {
                        StartSubtitle::Plain("Steam".into())
                    };
                    Self {
                        title,
                        subtitle: Some(subtitle),
                        target: crate::games::steam::launch_uri(*appid),
                        args: String::new(),
                        play_key: entry.play_key(),
                        icon: None,
                        icon_source: None,
                        backdrop_path: None,
                        edit: None,
                        skeleton: false,
                        update_required: false,
                        // Rows the scan proved missing stay enabled: Cross
                        // opens the game's store page (`view_in_steam`)
                        // instead of launching. Unknown-scan fallbacks stay
                        // launchable via the rungameid target.
                        disabled: false,
                        view_in_steam: known_missing.then_some(*appid),
                        played_at_ms: None,
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
                    disabled: false,
                    view_in_steam: None,
                    played_at_ms: None,
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
            disabled: false,
            view_in_steam: None,
            played_at_ms: None,
        }
    }

    /// Edit-checklist row for a curated Steam app the live scan missed
    /// (uninstalled). Carries the last-known title so it can be untoggled;
    /// Cross toggles membership here, never View in Steam.
    pub fn steam_uninstalled(appid: u32, title: String, played_at_ms: Option<u64>) -> Self {
        Self {
            title,
            subtitle: Some(StartSubtitle::Plain("Not installed".into())),
            target: crate::games::steam::launch_uri(appid),
            args: String::new(),
            play_key: format!("steam:{appid}"),
            icon: None,
            icon_source: None,
            backdrop_path: None,
            edit: Some(EditRow::Steam {
                appid,
                in_catalog: true,
            }),
            skeleton: false,
            update_required: false,
            disabled: false,
            view_in_steam: None,
            played_at_ms,
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
            disabled: false,
            view_in_steam: None,
            played_at_ms: None,
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

    /// Browse-selectable ("enabled") row. Mirrors the browse
    /// [`State::move_selection`] skip logic: disabled rows (uninstalled Steam
    /// / missing manual target) are skipped, while scan skeletons stay
    /// navigable. Edit-exit selection resolution uses this so the cursor can
    /// never strand on a disabled row.
    pub fn browse_enabled(&self) -> bool {
        !self.disabled || self.skeleton
    }
}

/// Deterministic edit-checklist order: title A–Z with a `play_key` tiebreak.
///
/// The checklist is built from `HashMap` iteration (`steam_by_id`), whose
/// order is random per process; a bare title sort would reshuffle
/// duplicate-title rows on every rebuild (toggle / scan / enter), which reads
/// as the list re-sorting itself. The tiebreak keeps rebuilds byte-stable.
pub fn sort_edit_rows(rows: &mut [StartRow]) {
    rows.sort_by(|a, b| {
        a.title
            .to_lowercase()
            .cmp(&b.title.to_lowercase())
            .then_with(|| a.play_key.cmp(&b.play_key))
    });
}

/// Edit-checklist order under the active browse sort: A–Z, or last-played
/// newest-first with the same A–Z + `play_key` tiebreaks as browse
/// ([`crate::games::GamesCatalog::merge_sorted`]). `played_at_ms` must be
/// populated before calling (see `App::edit_checklist_rows`); `None` sorts
/// as never-played, matching browse.
pub fn sort_edit_rows_by_mode(rows: &mut [StartRow], mode: GamesSortMode) {
    match mode {
        GamesSortMode::Alphabetical => sort_edit_rows(rows),
        GamesSortMode::LastPlayed => rows.sort_by(|a, b| {
            b.played_at_ms
                .unwrap_or(0)
                .cmp(&a.played_at_ms.unwrap_or(0))
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
                .then_with(|| a.play_key.cmp(&b.play_key))
        }),
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
    pub serial: Serial,
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

#[derive(Clone, Copy, Default)]
pub(crate) struct RowHintState {
    pub(crate) triangle_progress: f32,
    pub(crate) triangle_armed_t: f32,
    pub(crate) cross_progress: f32,
    pub(crate) cross_armed_t: f32,
    pub(crate) held: FaceHeld,
    pub(crate) press_anim: FacePressAnim,
}

/// Which chrome surface may light face / Options / reopen-chord press visuals.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HintPressOwner {
    Browse,
    Settings,
    ManualAdd,
    ReplaceConfirm,
    Dock,
}

impl HintPressOwner {
    fn current(state: &State) -> Self {
        if state.settings.open || state.settings.anim.is_some() {
            Self::Settings
        } else if state.manual_add.is_some() {
            Self::ManualAdd
        } else if state.replace_confirm.is_some() {
            Self::ReplaceConfirm
        } else if state.dock_expanded {
            Self::Dock
        } else {
            Self::Browse
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct HintPress {
    pub(crate) held: FaceHeld,
    pub(crate) press_anim: FacePressAnim,
    pub(crate) triangle_progress: f32,
    pub(crate) triangle_armed_t: f32,
    pub(crate) cross_progress: f32,
    pub(crate) cross_armed_t: f32,
}

impl HintPress {
    pub(crate) fn for_owner(state: &State, owner: HintPressOwner) -> Self {
        if HintPressOwner::current(state) != owner {
            return Self::default();
        }
        Self {
            held: state.held,
            press_anim: state.press_anim,
            triangle_progress: state.triangle_progress,
            triangle_armed_t: state.triangle_armed_anim,
            cross_progress: state.cross_progress,
            cross_armed_t: state.cross_armed_anim,
        }
    }
}

impl RowHintState {
    pub(crate) fn for_selected(state: &State, selected: bool, owner: HintPressOwner) -> Self {
        // Hints light only for the surface that owns input — idle otherwise.
        if selected {
            let press = HintPress::for_owner(state, owner);
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

/// Directional slide+fade when swapping the immersive games-strip orientation.
///
/// The old orientation slides out along its scroll axis while fading out; at
/// the midpoint the rendered orientation swaps and the new one slides in from
/// the opposite side while fading back in. Interrupt policy: a new request
/// replaces the in-flight transition (same restart-wins precedent as the
/// strip scroll chase; layout switches are rare and nav is gated meanwhile).
#[derive(Debug, Clone)]
struct LayoutTransitionAnim {
    from: ImmersiveLayout,
    to: ImmersiveLayout,
    started: Instant,
}

#[derive(Debug, Clone)]
struct PositionSectionAnim {
    from: f32,
    to: f32,
    duration_ms: u64,
    started: Instant,
}

/// Horizontal show/hide of the position bar (0 = off the left edge, 1 = resting).
/// `opacity_from` eases to `to` over position::OPACITY_MS, ahead of the slide.
#[derive(Debug, Clone)]
struct PositionSlideAnim {
    from: f32,
    opacity_from: f32,
    to: f32,
    started: Instant,
}

/// Wall-clock EnterImmersive veil clock (armed once the window can paint).
///
/// Progress derives from the arm [`Instant`], so the lift lasts exactly
/// [`ENTER_REVEAL_MS`](crate::ui::start::mode::ENTER_REVEAL_MS) no matter the
/// frame rate — a stall jumps time-correctly along the curve instead of
/// stretching the phase past its specified duration.
#[derive(Debug, Clone)]
struct EnterRevealClock {
    armed: bool,
    armed_at: Option<Instant>,
}

impl EnterRevealClock {
    fn elapsed_ms(&self, now: Instant) -> u64 {
        match (self.armed, self.armed_at) {
            (true, Some(at)) => now.saturating_duration_since(at).as_millis() as u64,
            _ => 0,
        }
    }
}

/// Eased immersive idle/sleep dim crossfade.
#[derive(Debug, Clone)]
struct IdleDimAnim {
    from: f32,
    to: f32,
    started: Instant,
}

/// Background-task chip kind for the bottom-right status rail.
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

#[derive(Debug, Clone)]
pub struct State {
    /// Enter ceremony phase after open (veil + splash-art hold).
    pub transition_phase: Option<(crate::ui::start::mode::TransitionPhase, Instant)>,
    /// Capped enter-reveal progress while [`TransitionPhase::EnterImmersive`] is active.
    enter_reveal: Option<EnterRevealClock>,
    /// First Start HWND of the process: ambient + status ceremony (not warm reopen).
    pub cold_load_active: bool,
    /// When the cold black→ambient ease began (`StartOpened`).
    cold_ambient_started: Option<Instant>,
    /// Mirrored from App: true until first SteamScanDone.
    pub steam_scan_pending: bool,
    /// Status rail in the bottom-right corner (bottom = newest).
    chrome_status: Vec<ChromeStatusItem>,
    /// When every live chip first became Success (`None` if any still working).
    chrome_status_all_success_at: Option<Instant>,
    /// Batch ExitRight stagger clock (`None` until clear starts).
    chrome_status_batch_exit_at: Option<Instant>,
    /// Cold: when the games strip began fading in (`None` = still hidden).
    games_list_reveal_at: Option<Instant>,
    /// When EnterImmersive began waiting for list reveal (art hold).
    enter_list_hold_started: Option<Instant>,
    /// Art is ready but splash reveal is held until the enter veil clears.
    splash_reveal_pending: bool,
    /// Pending reveal should snap opaque (cached) rather than ambient-fade.
    splash_reveal_cached: bool,
    /// Incoming splash uses ease-in-out enter fade (uncached ambient → art).
    enter_splash_fade: bool,
    /// Incoming splash paints fully opaque (cached reopen / after-veil snap).
    splash_opaque: bool,
    /// Controllers surface target (open = controllers focus).
    pub dock_expanded: bool,
    pub game_selected: usize,
    pub rows: Vec<StartRow>,
    pub controller_selected: usize,
    pub controllers: Vec<StartControllerRow>,
    pub running_target: Option<String>,
    /// Target with a live launch session not yet confirmed running (no
    /// process match, no Steam `Running` verdict). Shows "Launching" instead
    /// of "Running" until the session confirms or clears.
    pub launching_target: Option<String>,
    /// Target currently being closed (WM_CLOSE sent, waiting for process exit).
    /// Shows "Closing" immediately instead of clearing to nothing.
    pub closing_target: Option<String>,
    /// Controller serial currently being powered off (feature report sent,
    /// waiting for the pad to disappear). Shows "Powering off" the same way.
    pub powering_off: Option<Serial>,
    pub replace_confirm: Option<ReplaceConfirm>,
    pub manual_add: Option<ManualAddDraft>,
    /// Options settings panel (right drawer).
    pub settings: crate::ui::start::settings::SettingsPanel,
    pub triangle_progress: f32,
    pub cross_progress: f32,
    /// Face buttons currently held (pad OR keyboard) for action-hint press styling.
    pub held: FaceHeld,

    /// Animated 0..=1 press amounts per face (drives pressed scale).
    press_anim: FacePressAnim,

    /// Animated 0..=1 toward white-ish hold arc once triangle hold is armed.
    triangle_armed_anim: f32,
    /// Animated 0..=1 toward white-ish hold arc once cross hold is armed.
    cross_armed_anim: f32,
    hint_anim_tick: Option<Instant>,
    pub editing: bool,
    pub sort_mode: GamesSortMode,
    /// Controllers surface: include remembered disconnected pads.
    pub show_all_controllers: bool,
    /// `play_key` of the row selected when edit mode was entered (restored on Save/Cancel).
    pub edit_anchor_play_key: Option<String>,
    /// Unified scroll-offset tracking for every Start list (games / controllers /
    /// settings). Owns offset + viewport per list; `on_scroll` records live
    /// values via [`State::record_list_scroll`], programmatic `scroll_to`
    /// targets sync optimistically via [`State::note_scroll_y`] (on_scroll lags
    /// a frame). Future lists add a [`StartListId`] variant + geometry below.
    pub(crate) scrolls: ListScrollTracker,

    dock_anim: Option<DockAnim>,
    strip_anim: Option<StripAnim>,
    /// In-flight strip orientation transition (`None` = settled on prefs layout).
    layout_transition: Option<LayoutTransitionAnim>,
    /// In-flight section index chase (None = settled on position_section_at).
    position_section: Option<PositionSectionAnim>,
    /// Last settled section index. Not the live target; a chase starts from here.
    position_section_at: f32,
    /// Last games-strip scroll / section change (drives auto-hide).
    position_bar_last_scroll: Option<Instant>,
    /// Slide in from the left / out to the left. None = settled.
    position_bar_slide: Option<PositionSlideAnim>,
    /// NavbarVisibility::AlwaysOn pins the rail at (1, 1) (hide timer skipped).
    /// Synced from prefs by the shell (see `sync_navbar_pinned`).
    pub navbar_pinned: bool,
    /// Effective reduce-motion gate (prefs choice resolved against the OS
    /// "Show animations" toggle; see `crate::ui::motion`). Synced from prefs
    /// by the shell (see `sync_reduced_motion`). While set, movement snaps
    /// (durations to 0, offsets to 0, scales to 1) and only opacity animates.
    pub reduced_motion: bool,
    /// Perf toggle: throttle idle immersive Start to anim-driven frames only
    /// (default off = legacy always-60Hz). Synced from prefs by the shell.
    pub perf_throttle_immersive: bool,
    /// Seconds for ambient shader time uniform.
    pub ambient_time: f32,
    /// Last wall-clock stamp advancing [`Self::ambient_time`]. `tick_anim`
    /// advances the clock by capped wall-clock deltas (never fixed steps per
    /// call) so report-rate pad edges sharing the tick cannot fast-forward it.
    ambient_last_tick: Option<Instant>,
    /// Debug-only in-immersive ambience tuning panel ("tune ambience").
    /// While open the drawer stays closed, splash art is hidden, and R3
    /// toggles all chrome (including the panel) for a clean preview.
    #[cfg(debug_assertions)]
    pub ambient_tune_open: bool,
    /// Debug-only R3 chrome hide while tuning (panel included; R3 restores).
    #[cfg(debug_assertions)]
    pub ambient_tune_hide_chrome: bool,
    /// Debug-only tuning panel focus (pad Up/Down, wraps).
    #[cfg(debug_assertions)]
    pub ambient_tune_focus: usize,
    /// Debug-only tuning list scroll offset + viewport (focus-follow).
    #[cfg(debug_assertions)]
    pub ambient_tune_scroll_y: f32,
    /// Debug-only tuning list viewport height (focus-follow fallback).
    #[cfg(debug_assertions)]
    pub ambient_tune_viewport_h: f32,
    /// Debug-only ambient/ken-burns tuning active (Diagnostics sliders).
    /// While set, immersive keeps frames flowing even under the idle
    /// throttle so tuning (including freeze toggles) repaints immediately.
    /// Shell-local, never persisted; synced from the shell each update.
    #[cfg(debug_assertions)]
    pub ambient_tuning_keepalive: bool,
    /// Incoming backdrop land/crossfade (`started = None` = waiting for peek art).
    backdrop_current: Option<(String, Option<Instant>)>,
    /// Outgoing backdrop during cover-crossfade: `(play_key, scale, ox_norm, oy_norm)`.
    backdrop_outgoing: Option<(String, f32, f32, f32)>,
    /// Ring flash started with the last successful Identify (Controllers slide).
    identify_flash: Option<IdentifyFlash>,
    /// Keep painting while the art worker may still fill peeks.
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
    serial: Serial,
    started: Instant,
}

/// Which Start list a scroll offset belongs to.
///
/// Maps 1:1 to the scrollable widget [`Id`](iced::widget::Id) used in the
/// view, so `on_scroll` / programmatic `scroll_to` targets resolve through one
/// place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StartListId {
    Settings,
}

impl StartListId {
    /// Scrollable widget id for this list.
    pub fn scroll_id(self) -> iced::widget::Id {
        match self {
            Self::Settings => crate::ui::start::settings::settings_scroll_id(),
        }
    }

    /// Resolve a scrollable id back to its list (`None` for unknown / future ids).
    pub fn from_scroll_id(id: &iced::widget::Id) -> Option<Self> {
        if *id == Self::Settings.scroll_id() {
            Some(Self::Settings)
        } else {
            None
        }
    }
}

/// Tracked offset + viewport for the settings list.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListScrollState {
    pub offset_y: f32,
    pub viewport_h: f32,
}

/// Single owner for the settings list's scroll offset + viewport.
///
/// `on_scroll` feeds [`Self::record`]; programmatic `scroll_to` targets sync
/// optimistically via [`Self::note_programmatic`] / [`Self::note_programmatic_id`]
/// because `on_scroll` lags a frame.
#[derive(Debug, Clone, Default)]
pub struct ListScrollTracker {
    settings: ListScrollState,
}

impl ListScrollTracker {
    fn slot_mut(&mut self, list: StartListId) -> &mut ListScrollState {
        match list {
            StartListId::Settings => &mut self.settings,
        }
    }

    fn slot(&self, list: StartListId) -> &ListScrollState {
        match list {
            StartListId::Settings => &self.settings,
        }
    }

    /// Record a live `on_scroll` viewport report.
    pub fn record(&mut self, list: StartListId, offset_y: f32, viewport_h: f32) {
        let slot = self.slot_mut(list);
        slot.offset_y = offset_y;
        slot.viewport_h = viewport_h;
    }

    /// Sync the tracked offset after issuing a programmatic `scroll_to`
    /// (`on_scroll` may lag a frame behind).
    pub fn note_programmatic(&mut self, list: StartListId, y: f32) {
        self.slot_mut(list).offset_y = y;
    }

    /// [`Self::note_programmatic`] by widget id. Returns false for unknown /
    /// future ids (no-op, never panics).
    pub fn note_programmatic_id(&mut self, id: &iced::widget::Id, y: f32) -> bool {
        match StartListId::from_scroll_id(id) {
            Some(list) => {
                self.note_programmatic(list, y);
                true
            }
            None => false,
        }
    }

    pub fn offset(&self, list: StartListId) -> f32 {
        self.slot(list).offset_y
    }

    pub fn viewport(&self, list: StartListId) -> f32 {
        self.slot(list).viewport_h
    }

    pub fn reset(&mut self, list: StartListId) {
        *self.slot_mut(list) = ListScrollState::default();
    }

    pub fn reset_all(&mut self) {
        *self = Self::default();
    }
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
            launching_target: None,
            closing_target: None,
            powering_off: None,
            replace_confirm: None,
            manual_add: None,
            settings: crate::ui::start::settings::SettingsPanel::default(),
            triangle_progress: 0.0,
            cross_progress: 0.0,
            held: FaceHeld::default(),

            press_anim: FacePressAnim::default(),

            triangle_armed_anim: 0.0,
            cross_armed_anim: 0.0,
            hint_anim_tick: None,
            editing: false,
            sort_mode: GamesSortMode::default(),
            show_all_controllers: false,
            edit_anchor_play_key: None,
            scrolls: ListScrollTracker::default(),

            dock_anim: None,
            strip_anim: None,
            layout_transition: None,
            position_section: None,
            position_section_at: 0.0,
            position_bar_last_scroll: None,
            position_bar_slide: None,
            navbar_pinned: false,
            reduced_motion: false,
            perf_throttle_immersive: false,
            ambient_time: 0.0,
            ambient_last_tick: None,
            #[cfg(debug_assertions)]
            ambient_tune_open: false,
            #[cfg(debug_assertions)]
            ambient_tune_hide_chrome: false,
            #[cfg(debug_assertions)]
            ambient_tune_focus: 0,
            #[cfg(debug_assertions)]
            ambient_tune_scroll_y: 0.0,
            #[cfg(debug_assertions)]
            ambient_tune_viewport_h: 0.0,
            #[cfg(debug_assertions)]
            ambient_tuning_keepalive: false,
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

/// Launch/close badge for a game row (see [`State::launch_status`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchStatus {
    Closing,
    Launching,
    Running,
}

impl State {
    /// Launch/close badge for a row target. Precedence is Closing, then
    /// Launching (session alive but unconfirmed), then Running. Callers gate
    /// on `!editing` themselves (edit mode owns the hints).
    pub fn launch_status(&self, target: &str) -> Option<LaunchStatus> {
        if self.closing_target.as_deref() == Some(target) {
            Some(LaunchStatus::Closing)
        } else if self.launching_target.as_deref() == Some(target) {
            Some(LaunchStatus::Launching)
        } else if self.running_target.as_deref() == Some(target) {
            Some(LaunchStatus::Running)
        } else {
            None
        }
    }

    pub fn set_rows(&mut self, rows: Vec<StartRow>) {
        // Background rebuilds (Steam-scan reorder, sort change) can permute
        // the catalog without moving the selection: re-show the position rail
        // so the new order is oriented. Any play-key sequence change counts
        // (reorder or membership); disabled-only refreshes keep keys and stay
        // quiet. Immersive only — the open-path refresh runs while immersive
        // is still false, so this never fires before `reset_to_games`. Pinned
        // (`AlwaysOn`) is a no-op show; reduced motion snaps the slide
        // downstream (the trigger only starts the chase).
        let order_changed = self.rows.len() != rows.len()
            || self
                .rows
                .iter()
                .zip(rows.iter())
                .any(|(a, b)| a.play_key != b.play_key);
        self.rows = rows;
        if self.rows.is_empty() {
            self.game_selected = 0;
        } else {
            self.game_selected = self.game_selected.min(self.rows.len() - 1);
            // If the selected row became disabled (uninstall race), prefer the
            // nearest enabled row so launch never targets a disabled entry.
            if !self.editing
                && self
                    .rows
                    .get(self.game_selected)
                    .is_some_and(|r| r.disabled)
                && !self.rows[self.game_selected].skeleton
            {
                // Search forward first, then backward.
                let forward = (self.game_selected..self.rows.len())
                    .find(|&i| !self.rows[i].disabled || self.rows[i].skeleton);
                let backward = (0..self.game_selected)
                    .rev()
                    .find(|&i| !self.rows[i].disabled || self.rows[i].skeleton);
                if let Some(i) = forward.or(backward) {
                    self.game_selected = i;
                }
            }
        }
        self.snap_position_section();
        if order_changed {
            self.note_position_bar_scroll(Instant::now());
        }
    }

    pub fn set_controllers(&mut self, controllers: Vec<StartControllerRow>) {
        let prev_serial = self
            .controllers
            .get(self.controller_selected)
            .map(|r| &r.serial);
        let next_selected = if controllers.is_empty() {
            0
        } else if let Some(serial) = prev_serial {
            controllers
                .iter()
                .position(|r| r.serial == *serial)
                .unwrap_or_else(|| self.controller_selected.min(controllers.len() - 1))
        } else {
            self.controller_selected.min(controllers.len() - 1)
        };
        self.controllers = controllers;
        self.controller_selected = next_selected;
        // A powered-off pad is done once no *connected* row carries it
        // (remembered-disconnected rows reuse the serial).
        if self.powering_off.as_ref().is_some_and(|s| {
            !self
                .controllers
                .iter()
                .any(|r| r.connected && r.serial == *s)
        }) {
            self.powering_off = None;
        }
    }

    /// Record a live `on_scroll` viewport report for the settings list.
    pub fn record_list_scroll(&mut self, list: StartListId, y: f32, viewport_h: f32) {
        self.scrolls.record(list, y, viewport_h);
        if list == StartListId::Settings {
            self.settings.scroll_y = y;
            self.settings.viewport_h = viewport_h;
        }
    }

    /// Tracked offset for one list (for tests / reveal math).
    pub fn list_offset(&self, list: StartListId) -> f32 {
        self.scrolls.offset(list)
    }

    /// Tracked viewport height for one list (for tests / reveal math).
    pub fn list_viewport(&self, list: StartListId) -> f32 {
        self.scrolls.viewport(list)
    }

    /// Sync the tracked offset for a settings `scroll_to` that bypasses
    /// [`Self::note_scroll_y`] (keeps the tracker mirror + panel consistent).
    pub fn sync_settings_scroll(&mut self, y: f32) {
        self.scrolls.note_programmatic(StartListId::Settings, y);
        self.settings.scroll_y = y;
    }

    /// Keep tracked offset in sync with a programmatic `scroll_to` (on_scroll may lag).
    ///
    /// Unknown / future list ids are a silent no-op (returns false) so a new
    /// list id can never break existing reveal paths.
    pub fn note_scroll_y(&mut self, id: &iced::widget::Id, y: f32) -> bool {
        let noted = self.scrolls.note_programmatic_id(id, y);
        if noted && StartListId::from_scroll_id(id) == Some(StartListId::Settings) {
            self.settings.scroll_y = y;
        }
        noted
    }

    /// Select the row matching `play_key`, or the first row if missing / empty.
    pub fn select_game_by_play_key(&mut self, play_key: Option<&str>) {
        if self.rows.is_empty() {
            self.game_selected = 0;
            self.snap_position_section();
            return;
        }
        if let Some(key) = play_key
            && let Some(i) = self.rows.iter().position(|r| r.play_key == key)
        {
            self.game_selected = i;
            self.snap_position_section();
            return;
        }
        self.game_selected = 0;
        self.snap_position_section();
    }

    /// Remember the current games selection’s `play_key` for edit enter/exit restore.
    pub fn capture_edit_anchor(&mut self) {
        self.edit_anchor_play_key = self
            .rows
            .get(self.game_selected)
            .map(|r| r.play_key.clone());
    }

    /// Re-select `play_key` when it is still present, else keep the (already
    /// clamped) index. Unlike [`Self::select_game_by_play_key`] this never
    /// falls back to 0 — used to pin the edit cursor across checklist rebuilds
    /// (toggle / manual add / Steam scan reorder shift A–Z indices, so a bare
    /// index would drift to a different game; a 0-fallback would yank the
    /// cursor, e.g. after removing a manual shortcut).
    pub fn select_game_by_play_key_or_keep(&mut self, play_key: Option<&str>) {
        let Some(key) = play_key else {
            return;
        };
        if let Some(i) = self.rows.iter().position(|r| r.play_key == key)
            && i != self.game_selected
        {
            self.game_selected = i;
            self.snap_position_section();
        }
    }

    /// Edit-save exit target in CHECKLIST space (call while still editing, on
    /// the A–Z checklist rows, before the draft is committed).
    ///
    /// Keeps the cursor game when it is still toggled-on (`in_catalog`: Steam
    /// rows carry the flag, manual rows are on while present); otherwise the
    /// nearest toggled-on game ABOVE the cursor, else the nearest toggled-on
    /// game AFTER it. Returns `None` when nothing is toggled on (draft catalog
    /// emptied) — the post-save browse rows are then empty and the existing
    /// empty-games view shows.
    pub fn edit_exit_target_key(&self) -> Option<String> {
        if self.rows.is_empty() {
            return None;
        }
        let cur = self.game_selected.min(self.rows.len() - 1);
        if self.rows[cur].in_catalog() {
            return Some(self.rows[cur].play_key.clone());
        }
        if let Some(row) = self.rows[..cur].iter().rev().find(|r| r.in_catalog()) {
            return Some(row.play_key.clone());
        }
        if let Some(row) = self.rows[cur + 1..].iter().find(|r| r.in_catalog()) {
            return Some(row.play_key.clone());
        }
        None
    }

    /// Cancel-exit preferred key (call after leaving edit mode, on the
    /// rebuilt browse rows).
    ///
    /// Keeps navigation: the checklist `cursor_key` when it is still present
    /// in the restored catalog, else the pre-edit `anchor_key` (e.g. the
    /// cursor sat on a toggled-off Steam game or a newly added manual, both
    /// absent from browse). `None` when neither survives — the caller then
    /// falls back to the nearest-enabled rule inside
    /// [`Self::resolve_edit_exit_selection`]. Presence alone decides here;
    /// enabled-ness stays with `resolve_edit_exit_selection`, so a disabled
    /// cursor still holds its neighborhood instead of jumping to the anchor.
    pub fn cancel_exit_target_key(
        &self,
        cursor_key: Option<&str>,
        anchor_key: Option<&str>,
    ) -> Option<String> {
        cursor_key
            .filter(|key| self.rows.iter().any(|r| r.play_key == *key))
            .or_else(|| anchor_key.filter(|key| self.rows.iter().any(|r| r.play_key == *key)))
            .map(str::to_string)
    }

    /// Browse-space edit-exit selection resolution (call after leaving edit
    /// mode, on the rebuilt browse rows).
    ///
    /// Keeps `preferred_key` when it is still present AND enabled
    /// ([`StartRow::browse_enabled`]); otherwise the nearest enabled game
    /// ABOVE that position, else the nearest enabled game AFTER it. This is
    /// deliberately above-first, unlike [`Self::set_rows`] (which prefers
    /// forward) — exiting edit should hold the cursor where it was, not slide
    /// it down the list.
    ///
    /// Save exits pass the [`Self::edit_exit_target_key`] cursor target;
    /// cancel/discard exits pass [`Self::cancel_exit_target_key`]
    /// (cursor-first, anchor fallback — the catalog is restored, so a missing
    /// cursor revives the pre-edit game when still present, else the same
    /// nearest rule applies). Empty rows land on index 0 and
    /// the existing empty-games view shows; all-disabled non-empty rows keep
    /// the clamped index (`set_rows` parity — launch stays blocked).
    pub fn resolve_edit_exit_selection(&mut self, preferred_key: Option<&str>) {
        if self.rows.is_empty() {
            self.game_selected = 0;
            self.snap_position_section();
            return;
        }
        let start = preferred_key
            .and_then(|key| self.rows.iter().position(|r| r.play_key == key))
            .unwrap_or_else(|| self.game_selected.min(self.rows.len() - 1));
        if self.rows[start].browse_enabled() {
            self.game_selected = start;
        } else if let Some(i) = (0..start).rev().find(|&i| self.rows[i].browse_enabled()) {
            self.game_selected = i;
        } else if let Some(i) =
            ((start + 1)..self.rows.len()).find(|&i| self.rows[i].browse_enabled())
        {
            self.game_selected = i;
        } else {
            self.game_selected = start;
        }
        self.snap_position_section();
    }

    /// Current games selection as a stable key for service-side memory.
    pub fn selected_game_key(&self) -> Option<String> {
        self.rows
            .get(self.game_selected)
            .map(|r| r.play_key.clone())
    }

    /// Current controllers selection as a stable key for service-side memory.
    pub fn selected_controller_key(&self) -> Option<Serial> {
        self.controllers
            .get(self.controller_selected)
            .map(|r| r.serial.clone())
    }

    /// Resolve service-stored keys to indices after a shell restart.
    ///
    /// Game axis honors the persistence prefs; the controller axis is never
    /// gated (controller persistence is out of scope):
    /// - `persist_enabled == false` → game axis is a no-op (keeps the clamped
    ///   index; callers additionally skip report + scan re-pin).
    /// - [`StartSelectionMode::ByGame`] → current key lookup (unknown → 0).
    /// - [`StartSelectionMode::ByPosition`] → `index.min(len - 1)` clamp;
    ///   missing index keeps the clamped index, never 0.
    ///
    /// `None` keys leave that axis untouched. Returns true when an index moved.
    pub fn resolve_start_selection(
        &mut self,
        game_key: Option<&str>,
        controller_key: Option<&str>,
        persist_enabled: bool,
        selection_mode: StartSelectionMode,
        game_index: Option<usize>,
    ) -> bool {
        let mut changed = false;
        if persist_enabled {
            let index = match selection_mode {
                StartSelectionMode::ByGame => {
                    if let Some(key) = game_key {
                        if self.rows.is_empty() {
                            0
                        } else {
                            self.rows
                                .iter()
                                .position(|r| r.play_key == key)
                                .unwrap_or(0)
                        }
                    } else {
                        // No key: axis untouched.
                        self.game_selected
                    }
                }
                StartSelectionMode::ByPosition => {
                    if self.rows.is_empty() {
                        0
                    } else if let Some(i) = game_index {
                        i.min(self.rows.len() - 1)
                    } else {
                        // No index: keep the clamped position, never 0.
                        self.game_selected.min(self.rows.len() - 1)
                    }
                }
            };
            if index != self.game_selected {
                self.game_selected = index;
                changed = true;
            }
        }
        if let Some(serial) = controller_key {
            let index = if self.controllers.is_empty() {
                0
            } else {
                self.controllers
                    .iter()
                    .position(|r| r.serial.as_ref() == serial)
                    .unwrap_or(0)
            };
            if index != self.controller_selected {
                self.controller_selected = index;
                changed = true;
            }
        }
        if changed {
            self.snap_position_section();
        }
        changed
    }

    /// Land on Games with the controllers surface closed (used whenever the start screen opens).
    pub fn reset_to_games(&mut self) {
        self.dock_anim = None;
        self.strip_anim = None;
        self.layout_transition = None;
        self.position_bar_last_scroll = None;
        self.position_bar_slide = None;
        self.snap_position_section();
        self.dock_expanded = false;
        self.replace_confirm = None;
        self.manual_add = None;
        self.settings.reset();
        self.triangle_progress = 0.0;
        self.cross_progress = 0.0;
        self.held = FaceHeld::default();
        self.press_anim = FacePressAnim::default();
        self.triangle_armed_anim = 0.0;
        self.cross_armed_anim = 0.0;
        self.hint_anim_tick = None;
        self.editing = false;
        self.edit_anchor_play_key = None;
        self.game_selected = 0;
        self.controller_selected = 0;
        // Fresh window mounts scrollables at 0 with no on_scroll yet; the
        // open-time reveal (see `open_reveal_scroll`, issued on StartOpened)
        // recomputes from here with the estimate-viewport fallback.
        self.scrolls.reset_all();
    }

    pub fn overlay_blocking(&self) -> bool {
        self.replace_confirm.is_some()
            || self.manual_add.is_some()
            || self.settings.open
            || self.settings.anim.is_some()
            || self.ambient_tuning_active()
    }

    /// Toggle Options settings drawer.
    /// Reduced motion opens the drawer instantly too (no drawer travel).
    pub fn request_settings(&mut self, open: bool, now: Instant) -> bool {
        let animate = !self.reduced_motion;
        if self.settings.request(open, now, animate) {
            if open {
                self.replace_confirm = None;
                self.cross_progress = 0.0;
                // Fresh settings open resets the panel offset to 0 internally;
                // keep the tracker mirror consistent.
                self.scrolls.reset(StartListId::Settings);
            }
            true
        } else {
            false
        }
    }

    /// Transition nav policy: overlay/chrome transitions (surface open/close,
    /// strip/title, settings, position-bar anims) stay cancellable and never
    /// block nav; transitions that reflow main content (horizontal↔vertical
    /// layout transition) block pad + keyboard nav for their duration.
    ///
    /// Mechanism: this gates pad nav (surface/enter-phase/transition) but deliberately
    /// ignores retargetable chrome (strip/title) and settings; modal overlays
    /// use [`Self::overlay_blocking`]; the transition-scoped keyboard + Move
    /// gate is [`Self::layout_transition_active`].
    pub fn animating(&self) -> bool {
        // Strip scroll + title crossfade are retargetable — do not gate pad Up/Down on them.
        // The layout transition is not retargetable: block pad nav mid-transition.
        self.dock_anim.is_some()
            || self.transition_phase.is_some()
            || self.layout_transition.is_some()
    }

    /// True while the immersive layout transition is in flight.
    ///
    /// The transition reflows main content (horizontal↔vertical swap), so navigational
    /// messages (MoveUp/MoveDown, Left/Right equivalents,
    /// settings focus/nudge) are dropped for its duration, while Confirm/Cancel/
    /// Close/settings-toggle stay live (cancellable chrome). Dock/strip/
    /// title/settings anims never consult this gate.
    pub fn layout_transition_active(&self) -> bool {
        self.layout_transition.is_some()
    }

    /// Strip scroll in flight (visual chase; nav may retarget).
    pub fn strip_busy(&self) -> bool {
        self.strip_anim.is_some()
    }

    pub fn needs_frames(&self) -> bool {
        self.dock_anim.is_some()
            || self.strip_anim.is_some()
            || self.layout_transition.is_some()
            || self.transition_phase.is_some()
            || self.backdrop_current.is_some()
            || self.backdrop_outgoing.is_some()
            || !self.perf_throttle_immersive
            || self.debug_tuning_keepalive()
            || self.ambient_tuning_active()
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
            || self.position_section.is_some()
            || self.position_bar_slide.is_some()
            || (self.position_bar_last_scroll.is_some() && !self.navbar_pinned)
            || self.idle_dim_anim.is_some()
            || self.enter_reveal.is_some()
            || self.splash_reveal_pending
            || self.enter_splash_fade
    }

    /// True while Start has anim/visual work that must keep frames
    /// flowing under the idle throttle (excludes the bare idle arm).
    /// Used to document the gated-vs-ungated `needs_frames()` contract; the
    /// subscription itself consults `needs_frames()` (which gates only the
    /// bare arm behind `perf_throttle_immersive`).
    pub fn immersive_anim_active(&self) -> bool {
        self.dock_anim.is_some()
            || self.strip_anim.is_some()
            || self.layout_transition.is_some()
            || self.transition_phase.is_some()
            || self.backdrop_current.is_some()
            || self.backdrop_outgoing.is_some()
            || self.debug_tuning_keepalive()
            || self.ambient_tuning_active()
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
            || self.position_section.is_some()
            || self.position_bar_slide.is_some()
            || (self.position_bar_last_scroll.is_some() && !self.navbar_pinned)
            || self.idle_dim_anim.is_some()
            || self.enter_reveal.is_some()
            || self.splash_reveal_pending
            || self.enter_splash_fade
    }

    /// Sync the immersive idle-throttle gate from prefs (no anim snapping;
    /// the next subscription evaluation simply stops/starts `StartFrame`).
    pub fn sync_perf_throttle(&mut self, throttle: bool) {
        self.perf_throttle_immersive = throttle;
    }

    /// In-immersive ambience tuning panel visible (debug "tune ambience").
    /// Release: always false (no tuning exists).
    pub fn ambient_tuning_active(&self) -> bool {
        #[cfg(debug_assertions)]
        {
            self.ambient_tune_open
        }
        #[cfg(not(debug_assertions))]
        {
            false
        }
    }

    /// Open/close the tuning panel (debug-only "tune ambience"). The chrome
    /// hide never survives a close (hide implies open); opening resets
    /// focus/scroll.
    #[cfg(debug_assertions)]
    pub fn request_ambient_tuning(&mut self, open: bool) {
        self.ambient_tune_open = open;
        self.ambient_tune_hide_chrome = false;
        if open {
            self.ambient_tune_focus = 0;
            self.ambient_tune_scroll_y = 0.0;
        }
    }

    /// Move tuning focus with wrap (debug-only pad/keyboard nav).
    #[cfg(debug_assertions)]
    pub fn move_ambient_tune_focus(&mut self, delta: isize) -> bool {
        let n = crate::ui::start::ambient_tune::TuneRow::count().max(1) as isize;
        let next = (self.ambient_tune_focus as isize + delta).rem_euclid(n) as usize;
        if next == self.ambient_tune_focus {
            return false;
        }
        self.ambient_tune_focus = next;
        true
    }

    /// Sync tuning list scroll report (debug-only focus-follow + viewport).
    #[cfg(debug_assertions)]
    pub fn sync_ambient_tune_scroll(&mut self, y: f32, viewport_h: f32) {
        self.ambient_tune_scroll_y = y;
        self.ambient_tune_viewport_h = viewport_h;
    }

    /// Debug tuning keep-alive (see [`State::ambient_tuning_keepalive`]).
    /// Release: always false (no tuning exists).
    fn debug_tuning_keepalive(&self) -> bool {
        #[cfg(debug_assertions)]
        {
            self.ambient_tuning_keepalive
        }
        #[cfg(not(debug_assertions))]
        {
            false
        }
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

    /// True while the controllers surface is open or still traveling
    /// (used to keep the settings drawer from opening over it).
    pub fn dock_open_or_animating(&self) -> bool {
        self.dock_expanded || self.dock_anim.is_some()
    }

    /// Request the controllers surface open/close. Exits edit mode when opening.
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
        // Reduced motion: snap the surface (no travel); the stage scale gate
        // below keeps the games stage at 1x while open.
        if self.reduced_motion {
            let snapped = self.dock_anim.is_some();
            let changed = self.dock_expanded != expanded;
            self.dock_expanded = expanded;
            if expanded {
                self.editing = false;
                self.edit_anchor_play_key = None;
            }
            self.replace_confirm = None;
            self.manual_add = None;
            self.cross_progress = 0.0;
            self.dock_anim = None;
            return changed || snapped;
        }
        let from = self.dock_progress(now);
        self.dock_expanded = expanded;
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
            duration_ms: DOCK_ANIM_MS,
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
    ///
    /// Linear only — Games strip does not wrap, so `from`/`to` stay in catalog range.
    pub fn begin_strip_anim(&mut self, from_visual: f32, now: Instant) {
        let len = self.rows.len();
        if len < 1 {
            self.strip_anim = None;
            self.sync_position_section(now, crate::ui::start::vstrip::STRIP_ANIM_MS);
            return;
        }
        // Reduced motion: snap the strip (and the position rail) to the selection.
        if self.reduced_motion {
            self.strip_anim = None;
            self.sync_position_section(now, 0);
            return;
        }
        let catching_up = self.strip_anim.is_some();
        // Mid-flight: chase from the live eased scroll. Settled: use caller's prior index.
        let from = if catching_up {
            self.strip_scroll(now)
        } else {
            from_visual
        };
        let to = self.game_selected as f32;
        let delta = to - from;
        if delta.abs() < 0.01 {
            self.strip_anim = None;
            self.sync_position_section(now, crate::ui::start::vstrip::STRIP_ANIM_MS);
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
        self.sync_position_section(now, duration_ms);
    }

    /// Begin the orientation transition after an actual layout change. No-op when
    /// `from == to` so settings opens / redraws never animate. Replaces any
    /// in-flight transition (layout switches are rare; latest target wins).
    /// Reduced motion skips the transition: the new orientation renders
    /// immediately with no slide or fade.
    pub fn begin_layout_transition(
        &mut self,
        from: ImmersiveLayout,
        to: ImmersiveLayout,
        now: Instant,
    ) {
        if from == to {
            return;
        }
        if self.reduced_motion {
            self.layout_transition = None;
            return;
        }
        self.layout_transition = Some(LayoutTransitionAnim {
            from,
            to,
            started: now,
        });
    }

    /// Transition visual for this frame: `(render_layout, opacity_mul, dx, dy)`.
    ///
    /// First half renders the old orientation sliding out along its scroll axis
    /// while fading out; at the midpoint the orientation swaps and the new one
    /// slides in from the opposite side while fading back in. `current` is the
    /// prefs layout (the settled target once no transition is in flight).
    /// Offsets skip while flattening upstream (`Float`s paint above veils).
    pub fn layout_transition_visual(
        &self,
        current: ImmersiveLayout,
        now: Instant,
    ) -> (ImmersiveLayout, f32, f32, f32) {
        let Some(anim) = &self.layout_transition else {
            return (current, 1.0, 0.0, 0.0);
        };
        // Reduced motion (e.g. enabled mid-transition): no travel, only the
        // crossfade opacity.
        if self.reduced_motion {
            return (current, layout_transition_opacity(1.0), 0.0, 0.0);
        }
        let elapsed_ms = now.saturating_duration_since(anim.started).as_millis() as u64;
        let eased = layout_transition_eased(elapsed_ms);
        let layout = if eased < 0.5 { anim.from } else { anim.to };
        let (dx, dy) = layout_transition_offset(eased, layout);
        (layout, layout_transition_opacity(eased), dx, dy)
    }

    pub(crate) fn games_section_mode(&self) -> crate::ui::start::position::SectionMode {
        if matches!(self.sort_mode, GamesSortMode::Alphabetical) {
            crate::ui::start::position::SectionMode::Alphabetical
        } else {
            crate::ui::start::position::SectionMode::LastPlayed
        }
    }

    /// Eased fractional section index (active label centered in the position bar).
    ///
    /// When no chase is in flight this is the last settled index, not the live
    /// target. The target already follows game_selected, so reading it here
    /// would skip the slide (the games strip has the same trap).
    pub(crate) fn position_section_visual(&self, now: Instant) -> f32 {
        let Some(anim) = &self.position_section else {
            return self.position_section_at;
        };
        let t = (now.saturating_duration_since(anim.started).as_secs_f32()
            / (anim.duration_ms.max(1) as f32 / 1000.0))
            .clamp(0.0, 1.0);
        let eased = window_layout::ease_out_cubic(t);
        anim.from + (anim.to - anim.from) * eased
    }

    /// Horizontal show amount: 0 is off the left edge, 1 is resting.
    pub(crate) fn position_bar_slide(&self, now: Instant) -> f32 {
        self.position_bar_show(now).0
    }

    /// Show/hide opacity. Reaches the target over position::OPACITY_MS, before the slide.
    pub(crate) fn position_bar_opacity(&self, now: Instant) -> f32 {
        self.position_bar_show(now).1
    }

    /// Opacity for one slot's selected-hero cue overlay (the black box over
    /// the art carrying Playing/membership + face hints).
    ///
    /// 0 when the slot is out of range or the list is hidden; otherwise
    /// proximity (`1 - |dist|`) times `list_opacity`, so the box crossfades
    /// with the strip scroll instead of popping. The target selection fades
    /// in on arrival; the strip chase's fade-out party (`strip_anim.from`,
    /// once settled elsewhere) fades out on departure. Slots the chase
    /// merely passes through (multi-index jumps) stay quiet — only `from`
    /// and the target ever render. Pure for tests; sampled every frame like
    /// the other strip visuals. Opacity-only, so reduced motion leaves it
    /// animating.
    pub(crate) fn hero_overlay_alpha(
        &self,
        idx: usize,
        selected: usize,
        dist: f32,
        list_opacity: f32,
    ) -> f32 {
        let base = (1.0 - dist.abs()).clamp(0.0, 1.0) * list_opacity.clamp(0.0, 1.0);
        if base <= 0.01 {
            return 0.0;
        }
        if idx == selected {
            return base;
        }
        if let Some(anim) = &self.strip_anim {
            let from_idx = anim.from.round().max(0.0) as usize;
            let to_idx = anim.to.round().max(0.0) as usize;
            if from_idx != to_idx && idx == from_idx {
                return base;
            }
        }
        0.0
    }

    /// In-flight show/hide clock. The canvas samples this at draw time.
    /// `None` while pinned: the rail draws settled at (1, 1), never mid-hide.
    /// `None` under reduced motion too: the bar snaps to its settled slide
    /// while opacity still eases (see `position_bar_show`).
    pub(crate) fn position_bar_motion(&self) -> Option<crate::ui::start::position::ShowHide> {
        if self.navbar_pinned || self.reduced_motion {
            return None;
        }
        self.position_bar_slide
            .as_ref()
            .map(|anim| crate::ui::start::position::ShowHide {
                from_slide: anim.from,
                from_opacity: anim.opacity_from,
                to: anim.to,
                started: anim.started,
            })
    }

    fn position_bar_show(&self, now: Instant) -> (f32, f32) {
        use crate::ui::motion::movement_duration_ms;
        use crate::ui::start::position::{FADE_MS, OPACITY_MS, show_hide_amount};
        // AlwaysOn: the rail stays (1, 1); the hide timer is skipped in `tick_anim`.
        if self.navbar_pinned {
            return (1.0, 1.0);
        }
        if let Some(anim) = &self.position_bar_slide {
            let elapsed = now.saturating_duration_since(anim.started).as_secs_f32() * 1000.0;
            // Reduced motion snaps the slide (duration 0) while opacity keeps
            // its shorter OPACITY_MS clock.
            let slide = show_hide_amount(
                anim.from,
                anim.to,
                elapsed,
                movement_duration_ms(FADE_MS, self.reduced_motion),
            );
            let opacity = show_hide_amount(anim.opacity_from, anim.to, elapsed, OPACITY_MS);
            return (slide, opacity);
        }
        if self.position_bar_last_scroll.is_some() {
            (1.0, 1.0)
        } else {
            (0.0, 0.0)
        }
    }

    fn position_section_target(&self) -> f32 {
        let mode = self.games_section_mode();
        if self.rows.is_empty() {
            return 0.0;
        }
        let idx = self.game_selected.min(self.rows.len() - 1);
        let row = &self.rows[idx];
        crate::ui::start::position::section_index(
            mode,
            &row.title,
            row.played_at_ms,
            crate::ui::start::position::now_ms(),
            crate::ui::start::position::local_utc_offset_secs(),
        ) as f32
    }

    pub(crate) fn note_position_bar_scroll(&mut self, now: Instant) {
        let (current, current_op) = self.position_bar_show(now);
        self.position_bar_last_scroll = Some(now);
        if current >= 0.99 && current_op >= 0.99 {
            self.position_bar_slide = None;
            return;
        }
        if self
            .position_bar_slide
            .as_ref()
            .is_some_and(|anim| (anim.to - 1.0).abs() < 0.01)
        {
            return;
        }
        self.position_bar_slide = Some(PositionSlideAnim {
            from: current,
            opacity_from: current_op,
            to: 1.0,
            started: now,
        });
    }

    /// Sync the AlwaysOn pin from prefs. Enabling snaps any in-flight hide
    /// back to visible and stamps the scroll clock so the rail draws (1, 1)
    /// even before the next scroll; disabling releases the hide timer.
    pub fn sync_navbar_pinned(&mut self, always_on: bool, now: Instant) {
        self.navbar_pinned = always_on;
        if always_on {
            if self
                .position_bar_slide
                .as_ref()
                .is_some_and(|anim| anim.to < 0.5)
            {
                self.position_bar_slide = None;
            }
            if self.position_bar_last_scroll.is_none() {
                self.position_bar_last_scroll = Some(now);
            }
        }
    }

    /// Sync the effective reduce-motion gate from prefs. Enabling snaps
    /// in-flight movement chases to their settled targets (dock/strip state
    /// already sits target-side; only the visual chase is dropped) so the
    /// gate takes effect immediately. Opacity clocks (fades, dims, veils,
    /// position-bar show/hide) are untouched.
    pub fn sync_reduced_motion(&mut self, reduced: bool) {
        self.reduced_motion = reduced;
        if !reduced {
            return;
        }
        // Dock/strip state is set target-side at request time; drop the chase.
        self.dock_anim = None;
        self.strip_anim = None;
        self.layout_transition = None;
        if let Some(anim) = self.position_section.take() {
            self.position_section_at = anim.to;
        }
    }

    fn sync_position_section(&mut self, now: Instant, duration_ms: u64) {
        self.note_position_bar_scroll(now);
        let target = self.position_section_target();
        // Reduced motion snaps the rail labels (and the bar) to the section.
        if self.reduced_motion {
            self.position_section = None;
            self.position_section_at = target;
            return;
        }
        let current = self.position_section_visual(now);
        if (current - target).abs() < 0.004 {
            self.position_section = None;
            self.position_section_at = target;
            return;
        }
        if self
            .position_section
            .as_ref()
            .is_some_and(|anim| (anim.to - target).abs() < 0.004)
        {
            return;
        }
        self.position_section = Some(PositionSectionAnim {
            from: current,
            to: target,
            duration_ms,
            started: now,
        });
    }

    /// Jump to the current section with no notch slide (catalog reload / restore).
    fn snap_position_section(&mut self) {
        self.position_section = None;
        self.position_section_at = self.position_section_target();
    }

    /// Clear dock/strip/enter-phase and ambient (close fully).
    pub fn clear_immersive_session(&mut self) {
        self.dock_expanded = false;
        self.dock_anim = None;
        self.strip_anim = None;
        self.layout_transition = None;
        self.position_bar_last_scroll = None;
        self.position_bar_slide = None;
        self.snap_position_section();
        self.settings.reset();
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
        self.position_bar_last_scroll = None;
        self.position_bar_slide = None;
        self.snap_position_section();
        self.transition_phase = Some((phase, now));
        if matches!(phase, TransitionPhase::EnterImmersive) {
            self.enter_reveal = Some(EnterRevealClock {
                armed: false,
                armed_at: None,
            });
            // Hide strip until art sync / timeout begins the list reveal.
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

    /// Begin advancing the EnterImmersive veil (cold open after StartOpened).
    pub fn arm_enter_reveal(&mut self, now: Instant) {
        let Some(clock) = self.enter_reveal.as_mut() else {
            return;
        };
        if clock.armed {
            return;
        }
        clock.armed = true;
        clock.armed_at = Some(now);
        crate::controller::hid::diag::diag_info("ui-diag: start immersive enter reveal armed");
    }

    /// True once the enter-reveal clock is advancing (veil lifting).
    pub fn enter_reveal_armed(&self) -> bool {
        self.enter_reveal.as_ref().is_some_and(|c| c.armed)
    }

    /// Begin the process-first Start load ceremony (ambient + dock).
    ///
    /// Re-shows the position rail: runs after the `reset_to_games` /
    /// transition-phase clears on the open path, so the rail is up on enter.
    /// Pinned (`AlwaysOn`) is a no-op show; reduced motion snaps the slide
    /// downstream (the trigger only starts the chase).
    pub fn begin_cold_load(&mut self, now: Instant) {
        self.cold_load_active = true;
        self.cold_ambient_started = None;
        self.games_list_reveal_at = None;
        self.enter_list_hold_started = None;
        self.note_position_bar_scroll(now);
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
        if self.cold_load_active
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

    fn chrome_status_item_visual(
        &self,
        item: &ChromeStatusItem,
        now: Instant,
    ) -> ChromeStatusVisual {
        use crate::ui::motion::movement_offset;
        use crate::ui::start::mode::{CHROME_STATUS_EXIT_MS, phase_progress};
        // Reduced motion snaps stack reflow; ExitRight keeps its fade but
        // loses its travel.
        let stack_slot = if self.reduced_motion {
            item.stack_slot
        } else {
            Self::chrome_status_display_slot(item, now)
        };
        let (exit_x, opacity, success) = match item.phase {
            ChromeStatusPhase::In | ChromeStatusPhase::Active => (0.0, 1.0, false),
            ChromeStatusPhase::Success => (0.0, 1.0, true),
            ChromeStatusPhase::ExitRight => {
                let t = phase_progress(
                    now.saturating_duration_since(item.started).as_millis() as u64,
                    CHROME_STATUS_EXIT_MS,
                );
                // Full fade by the end so any remainder past travel is gone.
                (movement_offset(t, self.reduced_motion), 1.0 - t, true)
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
            .map(|i| self.chrome_status_item_visual(i, now))
            .collect()
    }

    /// Opacity of the games strip (0 = hidden until list reveal, 1 = fully shown).
    pub fn games_list_opacity(&self, now: Instant) -> f32 {
        use crate::ui::start::mode::{GAMES_LIST_FADE_MS, phase_progress};
        let Some(started) = self.games_list_reveal_at else {
            // Cold EnterImmersive: strip stays hidden until art sync / timeout.
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
        use crate::ui::start::vstrip::NEIGHBORS;
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
        // Linear strip window — Games no longer wraps.
        for idx in icon_cache::art_window_indices(selected, len, false, NEIGHBORS) {
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
    /// Cold enter: the veil may already be lifting onto ambient+dock.
    /// When art is ready, fade the games list / splash in — do not re-black the chrome.
    pub fn try_arm_enter_reveal_when_art_ready(&mut self, now: Instant) -> bool {
        if self.enter_reveal.is_none() || self.enter_reveal_armed() {
            // Veil already lifting/cleared: list reveal after art (cold + warm).
            if self.games_list_reveal_at.is_none() && self.enter_art_sync_ready() {
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
        if self.cold_load_active {
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
        if self.cold_load_active {
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
        // Warm enter: list stay-hidden clock outlives the veil phase.
        if self.games_list_reveal_at.is_none()
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

    /// EnterImmersive veil alpha from the wall-clock reveal time (1 = solid, 0 = clear).
    pub fn enter_veil_amount(&self, now: Instant) -> f32 {
        use crate::ui::start::mode::enter_veil_amount;
        match &self.enter_reveal {
            Some(clock) if clock.armed => enter_veil_amount(clock.elapsed_ms(now)),
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
                .map(|c| c.elapsed_ms(now))
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
                .map(|c| c.elapsed_ms(now))
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
                .is_some_and(|c| c.armed && c.elapsed_ms(now) >= dur);
        }
        now.saturating_duration_since(started) >= Duration::from_millis(dur)
    }

    /// Drop crossfade/land state when closing.
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
    /// Reduced motion freezes the ken-burns pose at rest (Cover, centered);
    /// the selection crossfade opacity is unchanged.
    pub fn backdrop_incoming_visual(&self, now: Instant) -> Option<(f32, f32, f32, f32)> {
        use crate::ui::motion::{movement_offset, movement_scale};
        use crate::ui::start::mode::{
            art_fade_progress, backdrop_pose_offset, backdrop_pose_scale, enter_art_fade_progress,
        };
        let row = self.rows.get(self.game_selected)?;
        row.backdrop_icon()?;
        let still = |elapsed_ms: u64| {
            let scale = movement_scale(backdrop_pose_scale(elapsed_ms), self.reduced_motion);
            let (ox, oy) = backdrop_pose_offset(elapsed_ms);
            (
                scale,
                movement_offset(ox, self.reduced_motion),
                movement_offset(oy, self.reduced_motion),
            )
        };
        match &self.backdrop_current {
            Some((key, None)) if key == &row.play_key => {
                let (scale, ox, oy) = still(0);
                Some((0.0, scale, ox, oy))
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
                let (scale, ox, oy) = still(elapsed);
                Some((opacity, scale, ox, oy))
            }
            // No state, or stale key after nav — still paint current art.
            None | Some(_) => Some((1.0, still(0).0, 0.0, 0.0)),
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

    /// Art paths for catalog indices within `radius` of the selection (linear).
    pub fn art_paths_for_radius(&self, radius: isize) -> icon_cache::ArtRetainSet {
        self.art_paths_for_radius_circular(radius, false)
    }

    /// Like [`Self::art_paths_for_radius`] with an explicit circular/linear window.
    ///
    /// Games strip is linear; callers pass `circular: false`. The flag remains for
    /// tests / legacy call sites.
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
        self.art_retain_set_circular(false)
    }

    /// Retain set with an explicit circular/linear near window.
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

    /// Near set with an explicit circular/linear window.
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
        use crate::ui::motion::{movement_offset, movement_scale};
        use crate::ui::start::mode::{backdrop_pose_offset, backdrop_pose_scale};
        let now = Instant::now();
        self.splash_reveal_pending = false;
        self.splash_reveal_cached = false;
        self.enter_splash_fade = false;
        self.splash_opaque = false;
        // Freeze full ken-burns pose — pan snap-to-center was a visible navigate glitch.
        // Reduced motion parks at rest (Cover, centered) instead.
        let (frozen_scale, frozen_ox, frozen_oy) = match &self.backdrop_current {
            Some((_, Some(started))) => {
                let elapsed = now.saturating_duration_since(*started).as_millis() as u64;
                let (ox, oy) = backdrop_pose_offset(elapsed);
                (
                    movement_scale(backdrop_pose_scale(elapsed).max(1.0), self.reduced_motion),
                    movement_offset(ox, self.reduced_motion),
                    movement_offset(oy, self.reduced_motion),
                )
            }
            _ => {
                let (ox, oy) = backdrop_pose_offset(0);
                (
                    movement_scale(backdrop_pose_scale(0).max(1.0), self.reduced_motion),
                    movement_offset(ox, self.reduced_motion),
                    movement_offset(oy, self.reduced_motion),
                )
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

    pub(crate) fn ring_flash_white(&self, serial: &Serial) -> bool {
        self.identify_flash.as_ref().is_some_and(|flash| {
            flash.serial == *serial
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
            (self.held.create, self.press_anim.create),
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
    /// Reduced motion snaps press feedback instead of gliding (end states identical).
    pub fn tick_hint_anims(&mut self, now: Instant) {
        // Full-step dt snaps every approach below to its target in one tick.
        let dt = if self.reduced_motion {
            1.0
        } else {
            self.hint_anim_tick
                .map(|t| now.saturating_duration_since(t).as_secs_f32())
                .unwrap_or(0.0)
                .min(0.05)
        };
        self.hint_anim_tick = Some(now);
        if dt <= 0.0 {
            return;
        }
        approach_anim(&mut self.press_anim.cross, self.held.cross, dt);
        approach_anim(&mut self.press_anim.circle, self.held.circle, dt);
        approach_anim(&mut self.press_anim.square, self.held.square, dt);
        approach_anim(&mut self.press_anim.triangle, self.held.triangle, dt);
        approach_anim(&mut self.press_anim.options, self.held.options, dt);
        approach_anim(&mut self.press_anim.create, self.held.create, dt);
        approach_anim(
            &mut self.triangle_armed_anim,
            self.triangle_progress >= 1.0,
            dt,
        );
        approach_anim(&mut self.cross_armed_anim, self.cross_progress >= 1.0, dt);
    }

    pub fn tick_anim(&mut self, now: Instant) {
        self.maybe_force_enter_reveal_after_art_hold(now);
        let _ = self.try_arm_enter_reveal_when_art_ready(now);
        self.try_start_pending_splash_reveal(now);
        self.tick_chrome_status(now);
        if let Some(anim) = self.dock_anim.as_ref() {
            let elapsed = now.saturating_duration_since(anim.started);
            if elapsed >= Duration::from_millis(anim.duration_ms) {
                self.dock_expanded = anim.to >= 0.5;
                self.dock_anim = None;
            }
        }
        let _ = self.settings.tick(now);
        if let Some(anim) = self.strip_anim.as_ref() {
            let elapsed = now.saturating_duration_since(anim.started);
            if elapsed >= Duration::from_millis(anim.duration_ms) {
                self.strip_anim = None;
            }
        }
        if let Some(anim) = self.layout_transition.as_ref() {
            let elapsed = now.saturating_duration_since(anim.started);
            if elapsed >= Duration::from_millis(LAYOUT_TRANSITION_MS) {
                self.layout_transition = None;
            }
        }
        if let Some(anim) = self.position_section.take() {
            let elapsed = now.saturating_duration_since(anim.started);
            if elapsed >= Duration::from_millis(anim.duration_ms) {
                self.position_section_at = anim.to;
            } else {
                self.position_section = Some(anim);
            }
        }
        if let Some(anim) = self.position_bar_slide.take() {
            let elapsed = now.saturating_duration_since(anim.started);
            if elapsed >= Duration::from_millis(crate::ui::start::position::FADE_MS) {
                if anim.to < 0.01 {
                    self.position_bar_last_scroll = None;
                }
            } else {
                self.position_bar_slide = Some(anim);
            }
        } else if let Some(last) = self.position_bar_last_scroll {
            // NavbarVisibility::AlwaysOn pins the rail at (1, 1): skip the
            // hide timer entirely while pinned.
            if !self.navbar_pinned {
                use crate::ui::start::position::HIDE_AFTER_MS;
                let elapsed = now.saturating_duration_since(last).as_millis() as u64;
                if elapsed >= HIDE_AFTER_MS {
                    self.position_bar_slide = Some(PositionSlideAnim {
                        from: 1.0,
                        opacity_from: 1.0,
                        to: 0.0,
                        started: now,
                    });
                }
            }
        }
        // Continuous ken-burns + pending splash decode both need frames.
        if self.backdrop_outgoing.is_some() {
            // Cover-crossfade: drop solid underlay once incoming is fully on.
            let incoming_done = self
                .backdrop_incoming_visual(now)
                .is_some_and(|(opacity, _, _, _)| opacity >= 0.99);
            if incoming_done {
                self.backdrop_outgoing = None;
            }
        }
        // Ambient clock runs on wall-clock deltas (capped per frame), never on
        // call count: StartFrame (60Hz) and report-rate pad edges share this
        // `tick_anim`, and fixed steps per call would fast-forward time
        // whenever edges outnumber frames. All callers share one stamp, so
        // their advances telescope to exactly the wall clock.
        let ambient_dt = match self.ambient_last_tick {
            Some(last) => crate::ui::shader::ambient::ambient_dt_for_elapsed(
                now.saturating_duration_since(last),
            ),
            // First frame after boot: one nominal step (matches the old
            // fixed-step open frame).
            None => crate::ui::shader::ambient::ambient_tick_dt(),
        };
        self.ambient_last_tick = Some(now);
        // The window only exists while open: always advance the ambient clock.
        // Debug time scale applied (release always advances wall-clock).
        self.ambient_time += ambient_dt;
    }

    pub fn move_selection(&mut self, delta: i32) -> Option<ScrollReveal> {
        if self.overlay_blocking() || self.layout_transition_active() {
            return None;
        }
        if self.dock_expanded {
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
            return Some(reveal_for_step(before, after, delta));
        }
        if self.rows.is_empty() {
            return None;
        }
        // Skip disabled rows (uninstalled) so nav never lands on them.
        // Editing mode shows checklist (all enabled); skip only in browse.
        let step = delta.signum();
        if step == 0 {
            return None;
        }
        let len = self.rows.len() as i32;
        let before = self.game_selected as i32;
        let mut after = before + step;
        while after >= 0 && after < len {
            let enabled = !self.rows[after as usize].disabled || self.editing;
            // Skeletons are still navigable (scan in flight).
            if enabled || self.rows[after as usize].skeleton {
                break;
            }
            after += step;
        }
        // Clamp: no wrap — Up on first / Down on last is a no-op.
        // If the scan ran past the end over disabled rows, stay put.
        if after < 0 || after >= len {
            return None;
        }
        if after == before {
            return None;
        }
        self.game_selected = after as usize;
        self.sync_position_section(Instant::now(), crate::ui::start::vstrip::STRIP_ANIM_MS);
        Some(reveal_for_step(before, after, delta))
    }

    pub fn selected_controller(&self) -> Option<&StartControllerRow> {
        self.controllers.get(self.controller_selected)
    }
}

/// Eased transition progress 0..=1 (ease in-out over [`LAYOUT_TRANSITION_MS`]).
/// Pure so unit tests can pin the curve without a clock.
pub fn layout_transition_eased(elapsed_ms: u64) -> f32 {
    crate::ui::motion::ease_in_out_cubic(crate::ui::motion::progress_linear(
        elapsed_ms,
        LAYOUT_TRANSITION_MS,
    ))
}

/// Strip opacity multiplier at eased transition progress: 1 at both ends,
/// 0 at the midpoint swap (the old layout is fully out before the new one
/// slides in, so the handoff never pops).
pub fn layout_transition_opacity(eased: f32) -> f32 {
    let eased = eased.clamp(0.0, 1.0);
    if eased < 0.5 {
        1.0 - 2.0 * eased
    } else {
        2.0 * eased - 1.0
    }
}

/// Slide offset `(dx, dy)` at eased transition progress for the rendered layout.
///
/// Each half travels along its own scroll axis: vertical exits up and enters
/// from below; horizontal exits left and enters from the right. Exit eases in
/// (accelerates away), enter eases out (soft landing); travel peaks at
/// [`LAYOUT_TRANSITION_SLIDE_PX`] at the midpoint swap where opacity is 0.
pub fn layout_transition_offset(eased: f32, layout: ImmersiveLayout) -> (f32, f32) {
    let eased = eased.clamp(0.0, 1.0);
    let (half_t, entering) = if eased < 0.5 {
        (eased * 2.0, false)
    } else {
        ((eased - 0.5) * 2.0, true)
    };
    let mag = if entering {
        LAYOUT_TRANSITION_SLIDE_PX * (1.0 - window_layout::ease_out_cubic(half_t))
    } else {
        LAYOUT_TRANSITION_SLIDE_PX * crate::ui::motion::ease_in_cubic(half_t)
    };
    let signed = if entering { mag } else { -mag };
    match layout {
        ImmersiveLayout::Vertical => (0.0, signed),
        ImmersiveLayout::Horizontal => (signed, 0.0),
    }
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

/// Small indeterminate arc spinner for the status rail.
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

/// Shared scan status chip (spinner or check + label) for the status rail.
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
        (ChromeStatusKind::SteamLibrary, false) => "Syncing Steam library…",
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

#[allow(clippy::too_many_arguments)]
pub fn view<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    now: Instant,
    clock_enabled: bool,
    stage_h: f32,
    stage_w: f32,
    settings_snapshot: &crate::ui::start::settings::StartSettingsSnapshot,
    #[cfg(debug_assertions)] debug_bounds: bool,
) -> Element<'a, StartMessage> {
    crate::ui::start::immersive::view(
        state,
        spectrum,
        now,
        clock_enabled,
        stage_h,
        stage_w,
        settings_snapshot,
        settings_snapshot.immersive_layout,
        #[cfg(debug_assertions)]
        debug_bounds,
    )
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
    fn navbar_pin_holds_rail_visible_and_skips_hide_timer() {
        use crate::ui::start::position::HIDE_AFTER_MS;
        let mut state = State::default();
        let now = Instant::now();
        // Unpinned and never scrolled: rail hidden.
        assert_eq!(state.position_bar_show(now), (0.0, 0.0));
        // Pinning stamps the clock and holds (1, 1) with no motion.
        state.sync_navbar_pinned(true, now);
        assert!(state.navbar_pinned);
        assert!(state.position_bar_last_scroll.is_some());
        assert_eq!(state.position_bar_show(now), (1.0, 1.0));
        assert!(state.position_bar_motion().is_none());
        // The hide timer never fires while pinned, however idle.
        let idle = now + Duration::from_millis(HIDE_AFTER_MS + 5_000);
        state.tick_anim(idle);
        assert!(state.position_bar_slide.is_none());
        assert_eq!(state.position_bar_show(idle), (1.0, 1.0));
        // Releasing the pin restores auto-hide from the stamped clock.
        state.sync_navbar_pinned(false, idle);
        state.tick_anim(idle);
        assert!(state.position_bar_slide.is_some());
    }

    #[test]
    fn hero_overlay_crossfades_with_strip_scroll() {
        let state = State::default();
        // At rest: only the selection shows, fully opaque with the list.
        assert!((state.hero_overlay_alpha(3, 3, 0.0, 1.0) - 1.0).abs() < 0.001);
        assert_eq!(state.hero_overlay_alpha(2, 3, -1.0, 1.0), 0.0);
        assert_eq!(state.hero_overlay_alpha(4, 3, 1.0, 1.0), 0.0);
        // Hidden list: nothing shows.
        assert_eq!(state.hero_overlay_alpha(3, 3, 0.0, 0.0), 0.0);
        // Mid-scroll alpha tracks proximity times list opacity.
        assert!((state.hero_overlay_alpha(3, 3, 0.4, 0.5) - 0.3).abs() < 0.001);
    }

    #[test]
    fn hero_overlay_fades_out_the_departed_slot() {
        let mut state = State::default();
        let now = Instant::now();
        state.strip_anim = Some(StripAnim {
            from: 2.0,
            to: 3.0,
            duration_ms: 200,
            started: now,
        });
        // Target fades in, departed slot fades out, by the same proximity rule.
        assert!((state.hero_overlay_alpha(3, 3, 0.4, 1.0) - 0.6).abs() < 0.001);
        assert!((state.hero_overlay_alpha(2, 3, -0.6, 1.0) - 0.4).abs() < 0.001);
        // A near slot that is neither party stays quiet even inside the window.
        assert_eq!(state.hero_overlay_alpha(4, 3, 0.9, 1.0), 0.0);
    }

    #[test]
    fn hero_overlay_skips_pass_through_slots_on_jumps() {
        let mut state = State::default();
        let now = Instant::now();
        state.strip_anim = Some(StripAnim {
            from: 0.0,
            to: 5.0,
            duration_ms: 200,
            started: now,
        });
        // Intermediate slots never render, even crossing center mid-jump.
        assert_eq!(state.hero_overlay_alpha(2, 5, 0.0, 1.0), 0.0);
        assert_eq!(state.hero_overlay_alpha(3, 5, -0.2, 1.0), 0.0);
        // Only `from` (out) and the target (in) participate.
        assert!((state.hero_overlay_alpha(0, 5, -0.3, 1.0) - 0.7).abs() < 0.001);
        assert!((state.hero_overlay_alpha(5, 5, 0.3, 1.0) - 0.7).abs() < 0.001);
    }

    #[test]
    fn cold_load_begin_shows_rail_after_reset() {
        use crate::ui::start::position::FADE_MS;
        // Open-path order: reset_to_games clears, begin_cold_load re-shows.
        let mut state = State { ..State::default() };
        let now = Instant::now();
        state.note_position_bar_scroll(now);
        assert!(state.position_bar_last_scroll.is_some());
        state.reset_to_games();
        assert!(state.position_bar_last_scroll.is_none());
        assert_eq!(state.position_bar_show(now), (0.0, 0.0));
        let open = now + Duration::from_millis(50);
        state.begin_cold_load(open);
        assert!(state.position_bar_last_scroll.is_some());
        // Show chase eases to resting (not instant).
        let settled = open + Duration::from_millis(FADE_MS + 50);
        assert_eq!(state.position_bar_show(settled), (1.0, 1.0));
    }

    #[test]
    fn cold_load_begin_is_noop_show_when_pinned() {
        let mut state = State { ..State::default() };
        let now = Instant::now();
        state.sync_navbar_pinned(true, now);
        state.begin_cold_load(now);
        assert_eq!(state.position_bar_show(now), (1.0, 1.0));
        assert!(state.position_bar_slide.is_none());
        assert!(state.position_bar_motion().is_none());
    }

    #[test]
    fn set_rows_reorder_shows_rail() {
        use crate::ui::start::position::FADE_MS;
        let mut state = State {
            rows: vec![
                game_row_with("steam:1"),
                game_row_with("steam:2"),
                game_row_with("steam:3"),
            ],

            ..State::default()
        };
        // Same-order rebuild (disabled-only refresh) stays hidden.
        state.set_rows(vec![
            game_row_with("steam:1"),
            game_row_with("steam:2"),
            game_row_with("steam:3"),
        ]);
        assert!(state.position_bar_last_scroll.is_none());
        // Reorder (Steam-scan reorder / sort change) re-shows.
        state.set_rows(vec![
            game_row_with("steam:3"),
            game_row_with("steam:1"),
            game_row_with("steam:2"),
        ]);
        assert!(state.position_bar_last_scroll.is_some());
        let settled = Instant::now() + Duration::from_millis(FADE_MS + 50);
        assert_eq!(state.position_bar_show(settled), (1.0, 1.0));
        // Membership change re-shows too.
        let mut added = State {
            rows: vec![game_row_with("steam:1"), game_row_with("steam:2")],

            ..State::default()
        };
        added.set_rows(vec![
            game_row_with("steam:1"),
            game_row_with("steam:2"),
            game_row_with("steam:3"),
        ]);
        assert!(added.position_bar_last_scroll.is_some());
    }

    #[test]
    fn throttle_gates_idle_but_not_anims() {
        // Idle: ungated needs frames, gated does not.
        let idle = State { ..State::default() };
        assert!(!idle.perf_throttle_immersive);
        assert!(idle.needs_frames());
        assert!(!idle.immersive_anim_active());
        let mut throttled = State { ..State::default() };
        throttled.sync_perf_throttle(true);
        assert!(!throttled.needs_frames());
        assert!(!throttled.immersive_anim_active());
        // Any anim keeps frames in both modes.
        let now = Instant::now();
        let mut ungated = State { ..State::default() };
        ungated.game_selected = 1;
        ungated.begin_strip_anim(0.0, now);
        assert!(ungated.needs_frames());
        assert!(ungated.immersive_anim_active());
        let mut gated = State { ..State::default() };
        gated.sync_perf_throttle(true);
        gated.game_selected = 1;
        gated.begin_strip_anim(0.0, now);
        assert!(gated.needs_frames());
        assert!(gated.immersive_anim_active());

        let mut ungated = State { ..State::default() };
        ungated.request_dock(true, now);
        assert!(ungated.needs_frames());
        assert!(ungated.immersive_anim_active());
        let mut gated = State { ..State::default() };
        gated.sync_perf_throttle(true);
        gated.request_dock(true, now);
        assert!(gated.needs_frames());
        assert!(gated.immersive_anim_active());

        let mut ungated = State { ..State::default() };
        ungated.replace_confirm = Some(crate::ui::start::view::ReplaceConfirm {
            running_title: "a".into(),
            next_title: "b".into(),
            next_index: 0,
        });
        assert!(ungated.needs_frames());
        assert!(ungated.immersive_anim_active());
        let mut gated = State { ..State::default() };
        gated.sync_perf_throttle(true);
        gated.replace_confirm = Some(crate::ui::start::view::ReplaceConfirm {
            running_title: "a".into(),
            next_title: "b".into(),
            next_index: 0,
        });
        assert!(gated.needs_frames());
        assert!(gated.immersive_anim_active());
    }

    #[test]
    #[cfg(debug_assertions)]
    fn ambient_tuning_panel_blocks_overlays_and_needs_frames() {
        // Opening the tuning panel blocks game nav like other modals, and
        // keeps frames flowing even under the idle throttle so the live
        // preview never freezes.
        let mut state = State { ..State::default() };
        state.sync_perf_throttle(true);
        assert!(!state.ambient_tuning_active());
        assert!(!state.needs_frames());
        assert!(!state.overlay_blocking());
        state.request_ambient_tuning(true);
        assert!(state.ambient_tuning_active());
        assert!(state.needs_frames());
        assert!(state.immersive_anim_active());
        assert!(state.overlay_blocking());
        // Focus wraps; hide toggle is independent of focus.
        assert!(state.move_ambient_tune_focus(1));
        assert_eq!(state.ambient_tune_focus, 1);
        assert!(state.move_ambient_tune_focus(-1));
        assert_eq!(state.ambient_tune_focus, 0);
        state.ambient_tune_hide_chrome = true;
        assert!(state.overlay_blocking());
        state.request_ambient_tuning(false);
        assert!(!state.ambient_tuning_active());
        assert!(!state.needs_frames());
        assert!(!state.overlay_blocking());
    }

    #[cfg(debug_assertions)]
    #[test]
    fn debug_tuning_keepalive_overrides_idle_throttle() {
        // Tuning (incl. freeze toggles) repaints even under the idle
        // throttle; clearing it restores the throttle gate.
        let mut throttled = State { ..State::default() };
        throttled.sync_perf_throttle(true);
        assert!(!throttled.needs_frames());
        throttled.ambient_tuning_keepalive = true;
        assert!(throttled.needs_frames());
        assert!(throttled.immersive_anim_active());
        throttled.ambient_tuning_keepalive = false;
        assert!(!throttled.needs_frames());
    }

    #[test]
    fn request_settings_preserves_selections() {
        // Settings open/close must not disturb selections; the settings
        // panel's own scroll reset (fresh open) is intended.
        let mut state = State {
            game_selected: 3,
            controller_selected: 2,
            ..State::default()
        };
        let now = Instant::now();
        assert!(state.request_settings(true, now));
        assert_eq!(state.game_selected, 3);
        assert_eq!(state.controller_selected, 2);
        assert!((state.settings.scroll_y - 0.0).abs() < f32::EPSILON);
        assert!((state.list_offset(StartListId::Settings) - 0.0).abs() < f32::EPSILON);
        assert!(state.request_settings(false, now));
        assert_eq!(state.game_selected, 3);
        assert_eq!(state.controller_selected, 2);
    }

    #[test]
    fn tracker_records_and_notes_settings_list() {
        let mut state = State::default();
        state.record_list_scroll(StartListId::Settings, 42.0, 480.0);
        assert!((state.list_offset(StartListId::Settings) - 42.0).abs() < f32::EPSILON);
        assert!((state.list_viewport(StartListId::Settings) - 480.0).abs() < f32::EPSILON);
        assert!((state.settings.scroll_y - 42.0).abs() < f32::EPSILON);
        // Programmatic scroll_to syncs optimistically (on_scroll lags).
        assert!(state.note_scroll_y(&StartListId::Settings.scroll_id(), 60.0));
        assert!((state.list_offset(StartListId::Settings) - 60.0).abs() < f32::EPSILON);
        assert!((state.settings.scroll_y - 60.0).abs() < f32::EPSILON);
    }

    #[test]
    fn tracker_unknown_list_id_is_noop() {
        // Unknown / future list ids must never disturb tracked lists.
        let mut state = State::default();
        state.record_list_scroll(StartListId::Settings, 42.0, 480.0);
        let unknown = iced::widget::Id::new("start-future-list-xyz");
        assert!(StartListId::from_scroll_id(&unknown).is_none());
        assert!(!state.note_scroll_y(&unknown, 999.0));
        assert!((state.list_offset(StartListId::Settings) - 42.0).abs() < f32::EPSILON);
    }

    fn controller_row_with(serial: &str, connected: bool) -> StartControllerRow {
        StartControllerRow {
            serial: serial.into(),
            title: "DualSense".into(),
            connection: "Bluetooth".into(),
            state: "discharging".into(),
            percent: 50,
            low: false,
            bluetooth: true,
            connected,
            eta: None,
        }
    }

    fn game_row_with(play_key: &str) -> StartRow {
        StartRow {
            title: play_key.into(),
            subtitle: None,
            target: play_key.into(),
            args: String::new(),
            play_key: play_key.into(),
            icon: None,
            icon_source: None,
            backdrop_path: None,
            edit: None,
            skeleton: false,
            update_required: false,
            disabled: false,
            view_in_steam: None,
            played_at_ms: None,
        }
    }

    #[test]
    fn launch_status_precedence_closing_launching_running() {
        let mut state = State::default();
        assert_eq!(state.launch_status("steam://rungameid/1"), None);
        state.running_target = Some("steam://rungameid/1".into());
        assert_eq!(
            state.launch_status("steam://rungameid/1"),
            Some(LaunchStatus::Running)
        );
        // Unconfirmed session outranks a stale Running badge.
        state.launching_target = Some("steam://rungameid/1".into());
        assert_eq!(
            state.launch_status("steam://rungameid/1"),
            Some(LaunchStatus::Launching)
        );
        // Closing outranks both.
        state.closing_target = Some("steam://rungameid/1".into());
        assert_eq!(
            state.launch_status("steam://rungameid/1"),
            Some(LaunchStatus::Closing)
        );
        // Other targets unaffected.
        assert_eq!(state.launch_status("steam://rungameid/2"), None);
    }

    #[test]
    fn selection_keys_snapshot_current_indices() {
        let mut state = State::default();
        assert_eq!(state.selected_game_key(), None);
        assert_eq!(state.selected_controller_key(), None);
        state.rows = vec![
            game_row_with("steam:1"),
            game_row_with("steam:2"),
            game_row_with("steam:3"),
        ];
        state.controllers = vec![
            controller_row_with("aa", true),
            controller_row_with("bb", true),
        ];
        state.game_selected = 2;
        state.controller_selected = 1;
        assert_eq!(state.selected_game_key().as_deref(), Some("steam:3"));
        assert_eq!(state.selected_controller_key(), Some(Serial::from("bb")));
    }

    #[test]
    fn resolve_start_selection_matches_stable_keys() {
        let mut state = State {
            rows: vec![
                game_row_with("steam:1"),
                game_row_with("steam:2"),
                game_row_with("steam:3"),
            ],
            controllers: vec![
                controller_row_with("aa", true),
                controller_row_with("bb", true),
            ],
            ..Default::default()
        };
        assert!(state.resolve_start_selection(
            Some("steam:3"),
            Some("bb"),
            true,
            StartSelectionMode::ByGame,
            None,
        ));
        assert_eq!(state.game_selected, 2);
        assert_eq!(state.controller_selected, 1);
        // Resolving the same keys again is a no-op.
        assert!(!state.resolve_start_selection(
            Some("steam:3"),
            Some("bb"),
            true,
            StartSelectionMode::ByGame,
            None,
        ));
    }

    #[test]
    fn resolve_start_selection_miss_falls_back_to_zero() {
        let mut state = State {
            rows: vec![game_row_with("steam:1"), game_row_with("steam:2")],
            controllers: vec![controller_row_with("aa", true)],
            game_selected: 1,
            ..Default::default()
        };
        // Unknown game key (uninstalled since the service stored it).
        assert!(state.resolve_start_selection(
            Some("steam:gone"),
            None,
            true,
            StartSelectionMode::ByGame,
            None,
        ));
        assert_eq!(state.game_selected, 0);
        assert_eq!(state.controller_selected, 0);
        // Unknown controller serial.
        state.controller_selected = 0;
        state.controllers = vec![
            controller_row_with("aa", true),
            controller_row_with("bb", true),
        ];
        state.controller_selected = 1;
        assert!(state.resolve_start_selection(
            None,
            Some("zz"),
            true,
            StartSelectionMode::ByGame,
            None,
        ));
        assert_eq!(state.controller_selected, 0);
    }

    #[test]
    fn resolve_start_selection_none_keys_leave_axes_untouched() {
        let mut state = State {
            rows: vec![game_row_with("steam:1"), game_row_with("steam:2")],
            controllers: vec![
                controller_row_with("aa", true),
                controller_row_with("bb", true),
            ],
            game_selected: 1,
            controller_selected: 1,
            ..Default::default()
        };
        assert!(
            !state.resolve_start_selection(None, None, true, StartSelectionMode::ByGame, None,)
        );
        assert_eq!(state.game_selected, 1);
        assert_eq!(state.controller_selected, 1);
        // One axis only.
        assert!(state.resolve_start_selection(
            Some("steam:1"),
            None,
            true,
            StartSelectionMode::ByGame,
            None,
        ));
        assert_eq!(state.game_selected, 0);
        assert_eq!(state.controller_selected, 1);
    }

    #[test]
    fn resolve_start_selection_empty_lists_stay_zero() {
        let mut state = State::default();
        assert!(!state.resolve_start_selection(
            Some("steam:1"),
            Some("aa"),
            true,
            StartSelectionMode::ByGame,
            None,
        ));
        assert_eq!(state.game_selected, 0);
        assert_eq!(state.controller_selected, 0);
    }

    #[test]
    fn resolve_start_selection_disabled_leaves_game_axis_untouched() {
        let mut state = State {
            rows: vec![game_row_with("steam:1"), game_row_with("steam:2")],
            controllers: vec![
                controller_row_with("aa", true),
                controller_row_with("bb", true),
            ],
            game_selected: 1,
            ..Default::default()
        };
        // Disabled: game key + index are ignored (clamped index kept)…
        assert!(!state.resolve_start_selection(
            Some("steam:1"),
            None,
            false,
            StartSelectionMode::ByGame,
            Some(0),
        ));
        assert_eq!(state.game_selected, 1);
        // …while the controller axis still resolves (never gated).
        assert!(state.resolve_start_selection(
            Some("steam:1"),
            Some("bb"),
            false,
            StartSelectionMode::ByGame,
            Some(0),
        ));
        assert_eq!(state.game_selected, 1);
        assert_eq!(state.controller_selected, 1);
    }

    #[test]
    fn resolve_start_selection_by_position_clamps() {
        let mut state = State {
            rows: vec![
                game_row_with("steam:1"),
                game_row_with("steam:2"),
                game_row_with("steam:3"),
            ],
            controllers: vec![controller_row_with("aa", true)],
            ..Default::default()
        };
        // Stored index restores directly, ignoring the key.
        assert!(state.resolve_start_selection(
            Some("steam:1"),
            None,
            true,
            StartSelectionMode::ByPosition,
            Some(2),
        ));
        assert_eq!(state.game_selected, 2);
        // Out-of-range index clamps to the last row.
        state.game_selected = 0;
        assert!(state.resolve_start_selection(
            None,
            None,
            true,
            StartSelectionMode::ByPosition,
            Some(99),
        ));
        assert_eq!(state.game_selected, 2);
        // …while a missing index keeps the clamped position, never 0.
        state.game_selected = 1;
        assert!(!state.resolve_start_selection(
            Some("steam:gone"),
            None,
            true,
            StartSelectionMode::ByPosition,
            None,
        ));
        assert_eq!(state.game_selected, 1);
    }

    #[test]
    fn powering_off_clears_only_when_no_connected_row_remains() {
        let mut state = State {
            powering_off: Some("aa".into()),
            ..Default::default()
        };
        // Still connected: flag survives rebuilds.
        state.set_controllers(vec![
            controller_row_with("aa", true),
            controller_row_with("bb", true),
        ]);
        assert_eq!(state.powering_off.as_ref().map(AsRef::as_ref), Some("aa"));
        // Gone entirely: cleared.
        state.set_controllers(vec![controller_row_with("bb", true)]);
        assert_eq!(state.powering_off, None);

        // Remembered-disconnected rows reuse the serial: still cleared.
        state.powering_off = Some("aa".into());
        state.set_controllers(vec![
            controller_row_with("bb", true),
            controller_row_with("aa", false),
        ]);
        assert_eq!(state.powering_off, None);
    }

    #[test]
    fn from_entry_skeleton_while_scan_pending() {
        let entry = GameEntry::Steam { appid: 1371980 };
        let empty = HashMap::new();
        let row = StartRow::from_entry(&entry, &empty, true, None, false);
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
        let row = StartRow::from_entry(&entry, &empty, false, None, false);
        assert!(!row.skeleton);
        assert_eq!(row.title, "Steam 1371980");
        assert!(row.icon.is_none());
        assert_eq!(row.play_key, "steam:1371980");
        assert!(matches!(
            row.subtitle.as_ref(),
            Some(StartSubtitle::Plain(label)) if label == "Steam"
        ));
        assert!(!row.target.is_empty());
        // Unknown scan: no proven absence, so the row stays launchable.
        assert_eq!(row.view_in_steam, None);
        assert_eq!(row.launch_hint_label(), "Launch");
    }

    #[test]
    fn from_entry_prefers_remembered_name_and_explains_missing() {
        let entry = GameEntry::Steam { appid: 1314000 };
        let empty = HashMap::new();
        // Proven uninstalled: last-known title, "Not installed" subtitle,
        // Cross routes to View in Steam instead of launching.
        let row = StartRow::from_entry(
            &entry,
            &empty,
            false,
            Some("Achilles: Legends Untold"),
            true,
        );
        assert!(!row.skeleton);
        assert_eq!(row.title, "Achilles: Legends Untold");
        assert!(matches!(
            row.subtitle.as_ref(),
            Some(StartSubtitle::Plain(label)) if label == "Not installed"
        ));
        assert_eq!(row.target, "steam://rungameid/1314000");
        assert_eq!(row.view_in_steam, Some(1314000));
        assert_eq!(row.launch_hint_label(), "View in Steam");
        // Blank remembered names never blank the title.
        let blank = StartRow::from_entry(&entry, &empty, false, Some("   "), true);
        assert_eq!(blank.title, "Steam 1314000");
        assert_eq!(blank.view_in_steam, Some(1314000));
        // Remembered name without proven absence: title only, still launches.
        let unsure = StartRow::from_entry(&entry, &empty, false, Some("Achilles"), false);
        assert_eq!(unsure.title, "Achilles");
        assert_eq!(unsure.view_in_steam, None);
        assert_eq!(unsure.launch_hint_label(), "Launch");
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
        let row = StartRow::from_entry(&entry, &map, true, None, false);
        assert!(!row.skeleton);
        assert_eq!(row.title, "Path of Exile");
        assert!(
            row.subtitle
                .as_ref()
                .is_some_and(|sub| sub.is_steam_fallback())
        );
        assert!(row.update_required);
        assert_eq!(row.launch_hint_label(), "Update & launch");
        assert!(row.icon.is_none());
    }

    #[test]
    fn steam_uninstalled_row_is_toggleable_not_viewable() {
        let row = StartRow::steam_uninstalled(1314000, "Achilles: Legends Untold".into(), Some(99));
        assert_eq!(row.title, "Achilles: Legends Untold");
        assert_eq!(row.play_key, "steam:1314000");
        assert_eq!(row.target, "steam://rungameid/1314000");
        assert!(matches!(
            row.edit,
            Some(EditRow::Steam {
                appid: 1314000,
                in_catalog: true
            })
        ));
        // Edit mode owns Cross (toggle): never View in Steam, never disabled.
        assert_eq!(row.view_in_steam, None);
        assert!(!row.disabled);
        assert!(!row.skeleton);
        assert_eq!(row.played_at_ms, Some(99));
    }

    #[test]
    fn reveal_for_step_wraps_flip_pin() {
        assert_eq!(reveal_for_step(9, 0, 1), ScrollReveal::Up);
        assert_eq!(reveal_for_step(0, 9, -1), ScrollReveal::Down);
        assert_eq!(reveal_for_step(3, 4, 1), ScrollReveal::Down);
        assert_eq!(reveal_for_step(4, 3, -1), ScrollReveal::Up);
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
                    disabled: false,
                    view_in_steam: None,
                    played_at_ms: None,
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
                    disabled: false,
                    view_in_steam: None,
                    played_at_ms: None,
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
                    disabled: false,
                    view_in_steam: None,
                    played_at_ms: None,
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
                    disabled: false,
                    view_in_steam: None,
                    played_at_ms: None,
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

    #[test]
    fn begin_strip_anim_linear_mid_list_step() {
        let mut state = State {
            rows: (0..2)
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
                    disabled: false,
                    view_in_steam: None,
                    played_at_ms: None,
                })
                .collect(),
            game_selected: 0,
            ..Default::default()
        };
        let now = Instant::now();
        state.begin_strip_anim(1.0, now);
        let anim = state.strip_anim.as_ref().expect("linear step starts anim");
        assert!((anim.from - 1.0).abs() < 0.01);
        assert!((anim.to - 0.0).abs() < 0.01);
    }

    fn title_rows(n: usize) -> Vec<StartRow> {
        (0..n)
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
                disabled: false,
                view_in_steam: None,
                played_at_ms: None,
            })
            .collect()
    }

    #[test]
    fn strip_scroll_carries_the_selected_title() {
        // Titles live inside their slot now, so the strip scroll *is* the
        // title animation — no separate crossfade clock exists to drift out
        // of sync with the card.
        let mut state = State {
            rows: title_rows(5),
            game_selected: 3,
            ..Default::default()
        };
        let now = Instant::now();
        assert!((state.strip_scroll(now) - 3.0).abs() < 0.01);
        state.begin_strip_anim(2.0, now);
        let anim = state.strip_anim.as_ref().expect("settled step animates");
        assert_eq!(anim.duration_ms, crate::ui::start::vstrip::STRIP_ANIM_MS);
        // Visual chases 2 -> 3 on the strip clock, so each slot's title and
        // meta travel with its art.
        assert!((state.strip_scroll(now) - 2.0).abs() < 0.01);
        let mid = now + Duration::from_millis(crate::ui::start::vstrip::STRIP_ANIM_MS / 2);
        let s = state.strip_scroll(mid);
        assert!(s > 2.0 && s < 3.0);
        let done = now + Duration::from_millis(crate::ui::start::vstrip::STRIP_ANIM_MS + 16);
        state.tick_anim(done);
        assert!(state.strip_anim.is_none());
        assert!((state.strip_scroll(done) - 3.0).abs() < 0.01);
        // Backward steps work the same (no direction special-case left).
        let mut back = State {
            rows: title_rows(5),
            game_selected: 1,
            ..Default::default()
        };
        back.begin_strip_anim(2.0, now);
        let anim = back.strip_anim.as_ref().expect("backward step animates");
        assert!((anim.from - 2.0).abs() < 0.01 && (anim.to - 1.0).abs() < 0.01);
    }

    #[test]
    fn strip_anim_retarget_chases_and_clears_on_noop() {
        let mut state = State {
            rows: title_rows(5),
            game_selected: 3,
            ..Default::default()
        };
        let now = Instant::now();
        state.begin_strip_anim(2.0, now);
        // Rapid second step mid-flight retargets from the live visual.
        let mid = now + Duration::from_millis(crate::ui::start::vstrip::STRIP_ANIM_MS / 2);
        state.game_selected = 4;
        state.begin_strip_anim(0.0, mid);
        let anim = state.strip_anim.as_ref().expect("retarget keeps anim");
        assert!((anim.to - 4.0).abs() < 0.01);
        assert_eq!(
            anim.duration_ms,
            crate::ui::start::vstrip::STRIP_ANIM_CATCHUP_MS
        );
        // Same index (no strip motion): nothing animates.
        let mut same = State {
            rows: title_rows(5),
            game_selected: 2,
            ..Default::default()
        };
        same.begin_strip_anim(2.0, now);
        assert!(same.strip_anim.is_none());
    }

    #[test]
    fn strip_anim_never_blocks_nav_but_keeps_frames() {
        let mut state = State {
            rows: title_rows(5),
            game_selected: 2,
            ..Default::default()
        };
        let now = Instant::now();
        state.game_selected = 3;
        state.begin_strip_anim(2.0, now);
        assert!(state.strip_anim.is_some());
        // Retargetable chase: pad nav is not gated while the strip animates.
        assert!(!state.animating());
        assert!(state.move_selection(1).is_some());
        assert_eq!(state.game_selected, 4);
        // ... but frames keep flowing until the scroll settles.
        assert!(state.needs_frames());
    }

    #[test]
    fn layout_transition_curve_hits_endpoints_and_midpoint() {
        use crate::persist::prefs::ImmersiveLayout;
        // Progress curve: settled ends, clamped past the end.
        assert_eq!(layout_transition_eased(0), 0.0);
        assert_eq!(layout_transition_eased(LAYOUT_TRANSITION_MS), 1.0);
        assert_eq!(layout_transition_eased(LAYOUT_TRANSITION_MS * 4), 1.0);
        assert!((layout_transition_eased(LAYOUT_TRANSITION_MS / 2) - 0.5).abs() < 1e-6);
        // Opacity: fully on at both ends, fully out at the midpoint swap.
        assert!((layout_transition_opacity(0.0) - 1.0).abs() < 1e-6);
        assert!((layout_transition_opacity(1.0) - 1.0).abs() < 1e-6);
        assert!(layout_transition_opacity(0.5).abs() < 1e-6);
        assert!(layout_transition_opacity(0.25) < 1.0 && layout_transition_opacity(0.25) > 0.0);
        // Offset: resting at both ends, peaking at the midpoint swap, axis per layout.
        let (dx, dy) = layout_transition_offset(0.0, ImmersiveLayout::Vertical);
        assert!(dx.abs() < 1e-6 && dy.abs() < 1e-6);
        let (dx, dy) = layout_transition_offset(1.0, ImmersiveLayout::Horizontal);
        assert!(dx.abs() < 1e-6 && dy.abs() < 1e-6);
        let (dx, dy) = layout_transition_offset(0.5, ImmersiveLayout::Vertical);
        assert!(dx.abs() < 1e-6 && (dy.abs() - LAYOUT_TRANSITION_SLIDE_PX).abs() < 0.01);
        let (dx, dy) = layout_transition_offset(0.5, ImmersiveLayout::Horizontal);
        assert!((dx.abs() - LAYOUT_TRANSITION_SLIDE_PX).abs() < 0.01 && dy.abs() < 1e-6);
        // Exit travels the negative side, enter arrives from the positive side.
        let (_, exit_dy) = layout_transition_offset(0.25, ImmersiveLayout::Vertical);
        assert!(exit_dy < 0.0);
        let (_, enter_dy) = layout_transition_offset(0.75, ImmersiveLayout::Vertical);
        assert!(enter_dy > 0.0);
        let _ = ImmersiveLayout::Vertical;
    }

    #[test]
    fn begin_layout_transition_only_on_change_and_swaps_at_midpoint() {
        use crate::persist::prefs::ImmersiveLayout;
        let mut state = State::default();
        let now = Instant::now();
        // Same layout: no anim (settings open / redraw must not transition).
        state.begin_layout_transition(ImmersiveLayout::Vertical, ImmersiveLayout::Vertical, now);
        assert!(state.layout_transition.is_none());
        // Actual change: anim runs and needs frames.
        state.begin_layout_transition(ImmersiveLayout::Vertical, ImmersiveLayout::Horizontal, now);
        assert!(state.layout_transition.is_some());
        assert!(state.needs_frames());
        // First half renders the old orientation sliding out (up for vertical).
        let quarter = now + Duration::from_millis(LAYOUT_TRANSITION_MS / 4);
        let (layout, opacity, dx, dy) =
            state.layout_transition_visual(ImmersiveLayout::Horizontal, quarter);
        assert_eq!(layout, ImmersiveLayout::Vertical);
        assert!(opacity < 1.0 && opacity > 0.0);
        assert!(dx.abs() < 1e-6 && dy < 0.0);
        // Second half renders the new orientation sliding in (from the right).
        let three_quarter = now + Duration::from_millis(LAYOUT_TRANSITION_MS * 3 / 4);
        let (layout, opacity, dx, dy) =
            state.layout_transition_visual(ImmersiveLayout::Horizontal, three_quarter);
        assert_eq!(layout, ImmersiveLayout::Horizontal);
        assert!(opacity < 1.0 && opacity > 0.0);
        assert!(dx > 0.0 && dy.abs() < 1e-6);
        // Past the end the transition retires and the settled layout shows fully.
        let done = now + Duration::from_millis(LAYOUT_TRANSITION_MS + 16);
        state.tick_anim(done);
        assert!(state.layout_transition.is_none());
        let (layout, opacity, dx, dy) =
            state.layout_transition_visual(ImmersiveLayout::Horizontal, done);
        assert_eq!(layout, ImmersiveLayout::Horizontal);
        assert_eq!(opacity, 1.0);
        assert!(dx.abs() < 1e-6 && dy.abs() < 1e-6);
    }

    /// Reduced motion snaps every movement chase to its target while opacity
    /// clocks keep easing.
    #[test]
    fn immersive_ambient_tracks_wall_clock_not_call_count() {
        // Pins the "same speed" contract the ambient preview relies on: the
        // clock advances by capped wall-clock deltas, so report-rate pad
        // edges sharing `tick_anim` with StartFrame cannot fast-forward it.
        // Reduced motion never gates the ambient clock.
        crate::ui::shader::ambient::with_tune_test_lock(|| {
            let nominal = crate::ui::shader::ambient::ambient_tick_dt();
            let t0 = Instant::now();
            for reduced in [false, true] {
                let mut state = State {
                    reduced_motion: reduced,
                    ..State::default()
                };
                // First frame: one nominal step (matches the old open frame).
                state.tick_anim(t0);
                assert!(
                    (state.ambient_time - nominal).abs() < 1e-6,
                    "reduced={reduced}"
                );
                // One 16ms frame: exactly 16ms of ambient time.
                state.tick_anim(t0 + Duration::from_millis(16));
                assert!(
                    (state.ambient_time - nominal - 0.016).abs() < 1e-6,
                    "reduced={reduced}"
                );
                // Ten ticks with no elapsed wall-clock (edge burst / saturated
                // loop recovery) advance nothing further.
                for _ in 0..10 {
                    state.tick_anim(t0 + Duration::from_millis(16));
                }
                assert!(
                    (state.ambient_time - nominal - 0.016).abs() < 1e-6,
                    "reduced={reduced}"
                );
                // Long gaps clamp to one frame instead of jumping.
                state.tick_anim(t0 + Duration::from_secs(10));
                assert!(
                    (state.ambient_time - nominal - 0.016 - 0.032).abs() < 1e-6,
                    "reduced={reduced}"
                );
            }
        });
    }

    #[test]
    fn reduced_motion_snaps_movement_keeps_opacity() {
        use crate::persist::prefs::ImmersiveLayout;
        use crate::ui::start::mode::{
            CHROME_STATUS_EXIT_MS, CHROME_STATUS_STEP_HOLD_MS, CHROME_STATUS_SUCCESS_HOLD_MS,
        };
        let now = Instant::now();
        let mut state = State {
            reduced_motion: true,
            ..State::default()
        };
        // Controllers surface: snaps open, no travel anim (stage scale gates to 1 at render).
        assert!(state.request_dock(true, now));
        assert!(state.dock_expanded);
        assert!(state.dock_anim.is_none());
        // Layout transition never starts; a pre-existing one renders settled.
        state.begin_layout_transition(ImmersiveLayout::Vertical, ImmersiveLayout::Horizontal, now);
        assert!(state.layout_transition.is_none());
        state.layout_transition = Some(LayoutTransitionAnim {
            from: ImmersiveLayout::Vertical,
            to: ImmersiveLayout::Horizontal,
            started: now,
        });
        let (layout, opacity, dx, dy) = state
            .layout_transition_visual(ImmersiveLayout::Horizontal, now + Duration::from_millis(10));
        assert_eq!(layout, ImmersiveLayout::Horizontal);
        assert_eq!(opacity, 1.0);
        assert!(dx.abs() < 1e-6 && dy.abs() < 1e-6);
        state.layout_transition = None;
        // Position bar: slide snaps (duration 0 settles on the first frame)
        // while opacity still eases over OPACITY_MS.
        state.note_position_bar_scroll(now);
        assert!(state.position_bar_slide.is_some());
        assert!(state.position_bar_motion().is_none());
        let (slide, opacity) = state.position_bar_show(now + Duration::from_millis(16));
        assert!((slide - 1.0).abs() < 1e-6);
        assert!(opacity > 0.0 && opacity < 1.0);
        // Chrome status exit: no travel, fade kept.
        state.begin_chrome_status(ChromeStatusKind::PreparingArt, now);
        state.note_chrome_status_success(ChromeStatusKind::PreparingArt, now);
        let hold = CHROME_STATUS_SUCCESS_HOLD_MS + CHROME_STATUS_STEP_HOLD_MS;
        let t_exit = now + Duration::from_millis(hold);
        state.tick_chrome_status(t_exit);
        let mid =
            state.chrome_load_status(t_exit + Duration::from_millis(CHROME_STATUS_EXIT_MS / 2));
        assert_eq!(mid.len(), 1);
        assert!(mid[0].exit_x.abs() < 1e-6);
        assert!(mid[0].opacity > 0.0 && mid[0].opacity < 1.0);
        // Enabling mid-flight settles dock/strip chases to target.
        let mut moving = State { ..State::default() };
        assert!(moving.request_dock(true, now));
        assert!(moving.dock_anim.is_some());
        moving.sync_reduced_motion(true);
        assert!(moving.reduced_motion);
        assert!(moving.dock_expanded);
        assert!(moving.dock_anim.is_none());
        assert!(moving.dock_anim.is_none());
        assert!(moving.strip_anim.is_none());
        assert!(moving.layout_transition.is_none());
        // Disabling never creates or clears chases by itself.
        moving.sync_reduced_motion(false);
        assert!(!moving.reduced_motion);
    }

    #[test]
    fn reduced_motion_snaps_strip_and_hint_press() {
        let now = Instant::now();
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
                    disabled: false,
                    view_in_steam: None,
                    played_at_ms: None,
                })
                .collect(),
            game_selected: 3,
            reduced_motion: true,
            ..Default::default()
        };
        state.begin_strip_anim(2.0, now);
        assert!(state.strip_anim.is_none());
        assert!((state.strip_scroll(now) - 3.0).abs() < 0.01);
        assert!(state.position_section.is_none());
        // Press feedback snaps to held instead of gliding over ~100 ms.
        state.held.cross = true;
        state.tick_hint_anims(now);
        assert!((state.press_anim.cross - 1.0).abs() < 1e-6);
    }

    #[test]
    fn layout_transition_blocks_nav_while_strip_stays_live() {
        use crate::persist::prefs::ImmersiveLayout;
        let mut state = State::default();
        assert!(!state.animating());
        let now = Instant::now();
        // Transition in flight: pad nav gates on animating().
        state.begin_layout_transition(ImmersiveLayout::Vertical, ImmersiveLayout::Horizontal, now);
        assert!(state.animating());
        // Retired: nav live again.
        let done = now + Duration::from_millis(LAYOUT_TRANSITION_MS + 16);
        state.tick_anim(done);
        assert!(state.layout_transition.is_none());
        assert!(!state.animating());
        // Strip scroll stays nav-live: animating() ignores it.
        let mut strip = State {
            rows: (0..3)
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
                    disabled: false,
                    view_in_steam: None,
                    played_at_ms: None,
                })
                .collect(),
            game_selected: 1,

            ..Default::default()
        };
        strip.begin_strip_anim(0.0, now);
        assert!(strip.strip_busy());
        assert!(!strip.animating());
    }

    #[test]
    fn layout_transition_drops_keyboard_equivalent_nav_until_retire() {
        use crate::persist::prefs::ImmersiveLayout;
        let mut state = State {
            rows: title_rows(3),
            game_selected: 1,
            ..Default::default()
        };
        let now = Instant::now();
        // Settled: selection moves.
        assert!(!state.layout_transition_active());
        assert!(state.move_selection(1).is_some());
        assert_eq!(state.game_selected, 2);
        assert!(state.move_selection(-1).is_some());
        // Transition in flight: keyboard-equivalent dispatch is dropped.
        state.begin_layout_transition(ImmersiveLayout::Vertical, ImmersiveLayout::Horizontal, now);
        assert!(state.layout_transition_active());
        assert!(state.move_selection(1).is_none());
        assert!(state.move_selection(-1).is_none());
        assert_eq!(state.game_selected, 1);
        // Retired: nav live again.
        let done = now + Duration::from_millis(LAYOUT_TRANSITION_MS + 16);
        state.tick_anim(done);
        assert!(!state.layout_transition_active());
        assert!(state.move_selection(1).is_some());
        assert_eq!(state.game_selected, 2);
    }

    #[test]
    fn dock_toggle_opens_and_closes() {
        let now = Instant::now();
        let mut state = State::default();
        assert!(state.request_dock(true, now));
        assert!(state.dock_expanded);
        assert!(!state.request_dock(true, now));
        let later = now + Duration::from_millis(DOCK_ANIM_MS + 10);
        assert!(state.request_dock(false, later));
        assert!(!state.dock_expanded);
        assert!(!state.request_dock(false, later));
    }

    #[test]
    fn move_selection_follows_controllers_surface() {
        let mut state = State {
            rows: vec![test_row("a", false, None), test_row("b", false, None)],
            controllers: vec![
                controller_row_with("aa", true),
                controller_row_with("bb", true),
            ],
            ..Default::default()
        };
        // Closed: moves games.
        assert!(state.move_selection(1).is_some());
        assert_eq!(state.game_selected, 1);
        assert_eq!(state.controller_selected, 0);
        // Open: moves controllers (wraps) and leaves games alone.
        state.dock_expanded = true;
        assert!(state.move_selection(1).is_some());
        assert_eq!(state.controller_selected, 1);
        assert_eq!(state.game_selected, 1);
        assert!(state.move_selection(1).is_some());
        assert_eq!(state.controller_selected, 0);
    }

    #[test]
    fn move_selection_games_clamps_at_ends() {
        let mut state = State {
            rows: (0..3)
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
                    disabled: false,
                    view_in_steam: None,
                    played_at_ms: None,
                })
                .collect(),
            game_selected: 0,
            ..Default::default()
        };
        assert!(state.move_selection(-1).is_none());
        assert_eq!(state.game_selected, 0);
        assert!(state.move_selection(1).is_some());
        assert_eq!(state.game_selected, 1);
        state.game_selected = 2;
        assert!(state.move_selection(1).is_none());
        assert_eq!(state.game_selected, 2);
    }

    fn test_row(title: &str, disabled: bool, played_at_ms: Option<u64>) -> StartRow {
        StartRow {
            title: title.into(),
            subtitle: None,
            target: format!("t-{title}"),
            args: String::new(),
            play_key: format!("k-{title}"),
            icon: None,
            icon_source: None,
            backdrop_path: None,
            edit: None,
            skeleton: false,
            update_required: false,
            disabled,
            view_in_steam: None,
            played_at_ms,
        }
    }

    #[test]
    fn move_selection_skips_disabled_rows() {
        let mut state = State {
            rows: vec![
                test_row("a", false, None),
                test_row("b", true, None),
                test_row("c", false, None),
            ],
            game_selected: 0,
            ..Default::default()
        };
        // Down from 0 skips disabled 1, lands on 2.
        assert!(state.move_selection(1).is_some());
        assert_eq!(state.game_selected, 2);
        // Up from 2 skips disabled 1, lands on 0.
        assert!(state.move_selection(-1).is_some());
        assert_eq!(state.game_selected, 0);
        // All disabled past selection: no move.
        state.game_selected = 0;
        state.rows[2].disabled = true;
        assert!(state.move_selection(1).is_none());
        assert_eq!(state.game_selected, 0);
    }

    fn edit_check_row(play_key: &str, title: &str, in_catalog: bool) -> StartRow {
        StartRow {
            title: title.into(),
            subtitle: None,
            target: format!("t-{play_key}"),
            args: String::new(),
            play_key: play_key.into(),
            icon: None,
            icon_source: None,
            backdrop_path: None,
            edit: Some(EditRow::Steam {
                appid: 7,
                in_catalog,
            }),
            skeleton: false,
            update_required: false,
            disabled: false,
            view_in_steam: None,
            played_at_ms: None,
        }
    }

    #[test]
    fn sort_edit_rows_tiebreaks_duplicate_titles_by_play_key() {
        // `steam_by_id` iterates in hash order, so duplicate titles must not
        // reshuffle the checklist on every rebuild (toggle/scan/enter).
        let mut rows = vec![
            edit_check_row("steam:2", "Same", true),
            edit_check_row("steam:1", "same", true),
            edit_check_row("steam:3", "Other", true),
        ];
        sort_edit_rows(&mut rows);
        let keys: Vec<_> = rows.iter().map(|r| r.play_key.as_str()).collect();
        assert_eq!(keys, vec!["steam:3", "steam:1", "steam:2"]);
        // Shuffled input converges to the same order.
        let mut shuffled = vec![
            edit_check_row("steam:2", "Same", true),
            edit_check_row("steam:3", "Other", true),
            edit_check_row("steam:1", "same", true),
        ];
        sort_edit_rows(&mut shuffled);
        let reshuffled: Vec<_> = shuffled.iter().map(|r| r.play_key.as_str()).collect();
        assert_eq!(reshuffled, keys);
    }

    #[test]
    fn sort_edit_rows_by_mode_last_played_matches_browse() {
        use crate::persist::prefs::GamesSortMode;
        // Last-played checklist: newest first, then A–Z, then play_key —
        // the same order as browse `merge_sorted`.
        let mut rows = vec![
            edit_check_row("steam:1", "Bravo", true),
            edit_check_row("steam:2", "Alpha", true),
            edit_check_row("steam:3", "Charlie", true),
        ];
        rows[0].played_at_ms = Some(100);
        rows[1].played_at_ms = Some(300);
        rows[2].played_at_ms = None;
        sort_edit_rows_by_mode(&mut rows, GamesSortMode::LastPlayed);
        let keys: Vec<_> = rows.iter().map(|r| r.play_key.as_str()).collect();
        assert_eq!(keys, vec!["steam:2", "steam:1", "steam:3"]);
        // Alphabetical mode ignores played time.
        sort_edit_rows_by_mode(&mut rows, GamesSortMode::Alphabetical);
        let keys: Vec<_> = rows.iter().map(|r| r.play_key.as_str()).collect();
        assert_eq!(keys, vec!["steam:2", "steam:1", "steam:3"]);
        // Tie on played falls back to A–Z.
        rows[0].played_at_ms = Some(500);
        rows[1].played_at_ms = Some(500);
        rows[2].played_at_ms = Some(500);
        sort_edit_rows_by_mode(&mut rows, GamesSortMode::LastPlayed);
        let keys: Vec<_> = rows.iter().map(|r| r.play_key.as_str()).collect();
        assert_eq!(keys, vec!["steam:2", "steam:1", "steam:3"]);
    }

    #[test]
    fn select_game_by_play_key_or_keep_pins_across_rebuild() {
        let mut state = State {
            rows: vec![
                game_row_with("steam:1"),
                game_row_with("steam:2"),
                game_row_with("steam:3"),
            ],
            game_selected: 2,
            ..Default::default()
        };
        // Key still present (rebuilt list re-sorted around it): follow the key.
        state.select_game_by_play_key_or_keep(Some("steam:1"));
        assert_eq!(state.game_selected, 0);
        // Key gone (manual removed mid-edit): keep the clamped neighbor,
        // never yank to 0.
        state.game_selected = 1;
        state.select_game_by_play_key_or_keep(Some("steam:gone"));
        assert_eq!(state.game_selected, 1);
        // No key: no-op.
        state.select_game_by_play_key_or_keep(None);
        assert_eq!(state.game_selected, 1);
        // Empty rows: no-op, no panic.
        state.rows.clear();
        state.game_selected = 0;
        state.select_game_by_play_key_or_keep(Some("steam:1"));
        assert_eq!(state.game_selected, 0);
    }

    #[test]
    fn edit_exit_target_key_keeps_toggled_on_cursor() {
        let state = State {
            rows: vec![
                edit_check_row("steam:1", "A", true),
                edit_check_row("steam:2", "B", true),
            ],
            game_selected: 1,
            editing: true,
            ..Default::default()
        };
        assert_eq!(state.edit_exit_target_key().as_deref(), Some("steam:2"));
    }

    #[test]
    fn edit_exit_target_key_prefers_above_then_after() {
        let rows = vec![
            edit_check_row("steam:1", "A", true),
            edit_check_row("steam:2", "B", false),
            edit_check_row("steam:3", "C", true),
        ];
        // Cursor on toggled-off B: nearest toggled-on above is A.
        let state = State {
            rows: rows.clone(),
            game_selected: 1,
            editing: true,
            ..Default::default()
        };
        assert_eq!(state.edit_exit_target_key().as_deref(), Some("steam:1"));
        // Nothing above: nearest toggled-on after.
        let state = State {
            rows: vec![
                edit_check_row("steam:1", "A", false),
                edit_check_row("steam:2", "B", false),
                edit_check_row("steam:3", "C", true),
            ],
            game_selected: 0,
            editing: true,
            ..Default::default()
        };
        assert_eq!(state.edit_exit_target_key().as_deref(), Some("steam:3"));
    }

    #[test]
    fn edit_exit_target_key_none_when_nothing_toggled_on() {
        let state = State {
            rows: vec![
                edit_check_row("steam:1", "A", false),
                edit_check_row("steam:2", "B", false),
            ],
            game_selected: 0,
            editing: true,
            ..Default::default()
        };
        assert_eq!(state.edit_exit_target_key(), None);
        let empty = State {
            editing: true,
            ..Default::default()
        };
        assert_eq!(empty.edit_exit_target_key(), None);
    }

    #[test]
    fn resolve_edit_exit_selection_keeps_enabled_preferred() {
        let mut state = State {
            rows: vec![
                test_row("a", false, None),
                test_row("b", false, None),
                test_row("c", false, None),
            ],
            game_selected: 0,
            ..Default::default()
        };
        state.resolve_edit_exit_selection(Some("k-b"));
        assert_eq!(state.game_selected, 1);
    }

    #[test]
    fn resolve_edit_exit_selection_prefers_above_then_after() {
        // Preferred present but disabled (uninstalled mid-edit): above wins.
        let mut state = State {
            rows: vec![
                test_row("a", false, None),
                test_row("b", true, None),
                test_row("c", false, None),
            ],
            game_selected: 0,
            ..Default::default()
        };
        state.resolve_edit_exit_selection(Some("k-b"));
        assert_eq!(state.game_selected, 0);
        // Nothing enabled above: nearest enabled after.
        let mut state = State {
            rows: vec![
                test_row("a", true, None),
                test_row("b", true, None),
                test_row("c", false, None),
            ],
            game_selected: 2,
            ..Default::default()
        };
        state.resolve_edit_exit_selection(Some("k-a"));
        assert_eq!(state.game_selected, 2);
        // Preferred key gone entirely (toggled off on save): start from the
        // clamped index, then the same above-first rule.
        let mut state = State {
            rows: vec![
                test_row("a", false, None),
                test_row("b", true, None),
                test_row("c", false, None),
            ],
            game_selected: 1,
            ..Default::default()
        };
        state.resolve_edit_exit_selection(Some("k-gone"));
        assert_eq!(state.game_selected, 0);
    }

    #[test]
    fn resolve_edit_exit_selection_empty_and_all_disabled() {
        // No rows: index 0 so the existing empty-games view shows.
        let mut state = State {
            game_selected: 3,
            ..Default::default()
        };
        state.resolve_edit_exit_selection(Some("k-anything"));
        assert_eq!(state.game_selected, 0);
        state.resolve_edit_exit_selection(None);
        assert_eq!(state.game_selected, 0);
        // Skeleton rows stay navigable (browse move_selection parity).
        let mut skeleton = test_row("s", true, None);
        skeleton.skeleton = true;
        let mut state = State {
            rows: vec![skeleton],
            game_selected: 0,
            ..Default::default()
        };
        state.resolve_edit_exit_selection(Some("k-s"));
        assert_eq!(state.game_selected, 0);
        // All disabled, none navigable: keep the clamped index (set_rows
        // parity — the row renders dimmed and launch stays blocked).
        let mut state = State {
            rows: vec![test_row("a", true, None), test_row("b", true, None)],
            game_selected: 1,
            ..Default::default()
        };
        state.resolve_edit_exit_selection(Some("k-b"));
        assert_eq!(state.game_selected, 1);
    }

    #[test]
    fn cancel_exit_target_prefers_cursor_over_anchor() {
        let state = State {
            rows: vec![
                test_row("a", false, None),
                test_row("b", false, None),
                test_row("c", false, None),
            ],
            ..Default::default()
        };
        // Navigated mid-edit: cursor survives in the restored catalog.
        assert_eq!(
            state
                .cancel_exit_target_key(Some("k-c"), Some("k-a"))
                .as_deref(),
            Some("k-c")
        );
        // Cursor on a toggled-off / newly added game (absent from browse):
        // fall back to the pre-edit anchor.
        assert_eq!(
            state
                .cancel_exit_target_key(Some("k-gone"), Some("k-a"))
                .as_deref(),
            Some("k-a")
        );
        // Neither survives: nearest-enabled rule downstream (`None`).
        assert_eq!(
            state.cancel_exit_target_key(Some("k-gone"), Some("k-missing")),
            None
        );
        assert_eq!(state.cancel_exit_target_key(None, None), None);
    }

    #[test]
    fn cancel_keeps_navigated_cursor_instead_of_anchor() {
        // Regression: enter edit, move selection, Circle must not jump back
        // to the pre-edit game — cancel discards membership changes but keeps
        // navigation, exactly like save.
        let browse = vec![
            test_row("a", false, None),
            test_row("b", false, None),
            test_row("c", false, None),
        ];
        let mut state = State {
            rows: browse.clone(),
            game_selected: 0,
            ..Default::default()
        };
        state.capture_edit_anchor();
        let anchor = state.edit_anchor_play_key.clone();
        assert_eq!(anchor.as_deref(), Some("k-a"));
        // Checklist order differs; user navigates to another game mid-edit.
        state.editing = true;
        state.set_rows(vec![
            test_row("c", false, None),
            test_row("a", false, None),
            test_row("b", false, None),
        ]);
        state.select_game_by_play_key_or_keep(Some("k-c"));
        assert_eq!(state.game_selected, 0);
        let cursor = state.selected_game_key();
        // Cancel: catalog restored (same browse rows here), cursor survives.
        state.editing = false;
        state.edit_anchor_play_key.take();
        state.set_rows(browse);
        let target = state.cancel_exit_target_key(cursor.as_deref(), anchor.as_deref());
        assert_eq!(target.as_deref(), Some("k-c"));
        state.resolve_edit_exit_selection(target.as_deref());
        assert_eq!(state.game_selected, 2);
    }

    #[test]
    fn edit_session_leaves_sort_and_section_mode_untouched() {
        use crate::persist::prefs::GamesSortMode;
        // Regression: entering/exiting edit mode (save AND cancel, with
        // toggles in between) must not re-sort the games list by itself. The
        // stored sort always mirrors prefs — `cycle_games_sort` stays live
        // while editing and `refresh_start_rows` re-syncs on every rebuild —
        // and these State transitions (the unit-testable half) preserve it.
        for sort in [GamesSortMode::LastPlayed, GamesSortMode::Alphabetical] {
            let mut state = State {
                rows: vec![test_row("b", false, None), test_row("a", false, None)],
                game_selected: 1,
                sort_mode: sort,
                ..Default::default()
            };
            let section_before = state.games_section_mode();
            // Enter edit: checklist rebuild + cursor pin must not touch sort.
            state.editing = true;
            state.capture_edit_anchor();
            let mut checklist = vec![
                edit_check_row("steam:1", "B", true),
                edit_check_row("steam:2", "A", true),
            ];
            sort_edit_rows_by_mode(&mut checklist, sort);
            state.set_rows(checklist);
            state.select_game_by_play_key_or_keep(state.edit_anchor_play_key.clone().as_deref());
            assert_eq!(state.sort_mode, sort);
            // Mid-edit toggle rebuild: pin keeps the cursor, sort untouched.
            state.select_game_by_play_key_or_keep(Some("steam:2"));
            assert_eq!(state.sort_mode, sort);
            // Exit (save or cancel): browse rows rebuilt under the same sort,
            // selection resolved by key — sort/section identical to pre-enter.
            state.editing = false;
            state.edit_anchor_play_key.take();
            state.set_rows(vec![test_row("a", false, None), test_row("b", false, None)]);
            state.resolve_edit_exit_selection(Some("k-a"));
            assert_eq!(state.sort_mode, sort);
            assert_eq!(state.games_section_mode(), section_before);
            assert_eq!(state.game_selected, 0);
        }
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
                disabled: false,
                view_in_steam: None,
                played_at_ms: None,
            }],
            game_selected: 0,

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
        assert!((state.enter_veil_amount(now) - 1.0).abs() < 0.001);
        assert!(!state.phase_finished(now + Duration::from_millis(500)));
        // Wall time alone must not lift the veil before arming.
        let later = now + Duration::from_millis(200);
        assert!((state.enter_veil_amount(later) - 1.0).abs() < 0.001);
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
                    disabled: false,
                    view_in_steam: None,
                    played_at_ms: None,
                }],
                game_selected: 0,

                steam_scan_pending: false,
                ..Default::default()
            };
            let now = Instant::now();
            state.begin_transition_phase(TransitionPhase::EnterImmersive, now);
            state.prime_backdrop_for_enter(now);
            assert!(!state.enter_art_sync_ready());
            assert!(!state.try_arm_enter_reveal_when_art_ready(now));
            assert!(!state.enter_reveal_armed());
            assert!((state.enter_veil_amount(now) - 1.0).abs() < 0.001);

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
    fn enter_hides_list_until_art_sync() {
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
                disabled: false,
                view_in_steam: None,
                played_at_ms: None,
            }],
            game_selected: 0,

            steam_scan_pending: true,
            ..Default::default()
        };
        let now = Instant::now();
        state.begin_transition_phase(TransitionPhase::EnterImmersive, now);
        state.prime_backdrop_for_enter(now);
        // Open arms veil onto ambient immediately; list stays hidden.
        state.arm_enter_reveal(now);
        assert!(state.enter_reveal_armed());
        assert!(state.games_list_reveal_at.is_none());
        assert!((state.games_list_opacity(now) - 0.0).abs() < 0.001);
        assert!(!state.enter_art_sync_ready());
        assert!(!state.try_arm_enter_reveal_when_art_ready(now));
        assert!(state.games_list_reveal_at.is_none());
    }

    #[test]
    fn enter_art_ready_reveals_list_while_veil_armed() {
        icon_cache::with_cache_lock(|| {
            use crate::ui::start::mode::TransitionPhase;
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("sdsc-enter-list-reveal-{n}"));
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
                    disabled: false,
                    view_in_steam: None,
                    played_at_ms: None,
                }],
                game_selected: 0,

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
    fn enter_list_hold_timeout_reveals_without_art() {
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
                disabled: false,
                view_in_steam: None,
                played_at_ms: None,
            }],
            game_selected: 0,

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
    fn enter_reveal_wall_clock_midpoint_and_exact_duration() {
        use crate::ui::start::mode::{ENTER_REVEAL_MS, TransitionPhase, enter_veil_amount};
        let mut state = State::default();
        let t0 = Instant::now();
        state.begin_transition_phase(TransitionPhase::EnterImmersive, t0);
        state.arm_enter_reveal(t0);
        assert!((state.enter_veil_amount(t0) - 1.0).abs() < 0.001);

        // A 200ms stall jumps time-correctly along the curve (no per-frame
        // cap stretching the phase past its specified duration).
        let stalled = t0 + Duration::from_millis(200);
        assert!((state.enter_veil_amount(stalled) - enter_veil_amount(200)).abs() < 0.001);
        assert!(!state.phase_finished(stalled));

        // Midpoint lands at ~0.5 veil by wall clock.
        let mid = t0 + Duration::from_millis(ENTER_REVEAL_MS / 2);
        assert!((state.enter_veil_amount(mid) - 0.5).abs() < 0.02);

        // The lift lasts exactly its specified duration.
        assert!(!state.phase_finished(t0 + Duration::from_millis(ENTER_REVEAL_MS - 1)));
        let done = t0 + Duration::from_millis(ENTER_REVEAL_MS);
        assert!(state.phase_finished(done));
        assert!(state.enter_veil_amount(done) < 0.01);
    }

    #[test]
    fn enter_paints_cached_splash_opaque_under_veil() {
        icon_cache::with_cache_lock(|| {
            use crate::ui::start::mode::{ENTER_REVEAL_MS, TransitionPhase};
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
                now += Duration::from_millis(ENTER_REVEAL_MS / 8);
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
                ART_FADE_MS, ENTER_REVEAL_MS, TransitionPhase, enter_art_fade_progress,
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
                now += Duration::from_millis(ENTER_REVEAL_MS / 8);
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
                        disabled: false,
                        view_in_steam: None,
                        played_at_ms: None,
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
                        disabled: false,
                        view_in_steam: None,
                        played_at_ms: None,
                    },
                ],
                game_selected: 0,

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
                        disabled: false,
                        view_in_steam: None,
                        played_at_ms: None,
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
                        disabled: false,
                        view_in_steam: None,
                        played_at_ms: None,
                    },
                ],
                game_selected: 0,

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
                    disabled: false,
                    view_in_steam: None,
                    played_at_ms: None,
                })
                .collect(),
            game_selected: 2,
            ..Default::default()
        };
        let (_heroes, _, backdrops) = state.immersive_warm_paths();
        // ±ART_WINDOW around selection 2 → all 5 rows.
        for path in &bd {
            assert!(
                backdrops.contains(path),
                "missing window backdrop {path:?} in {backdrops:?}"
            );
        }
    }

    #[test]
    fn art_retain_set_linear_at_start_even_when_immersive() {
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
                    disabled: false,
                    view_in_steam: None,
                    played_at_ms: None,
                })
                .collect(),
            game_selected: 0,

            ..Default::default()
        };
        let set = state.art_retain_set();
        let w = icon_cache::ART_WINDOW as usize;
        assert!(set.backdrops.contains(&PathBuf::from("bd0.png")));
        assert!(set.backdrops.contains(&PathBuf::from(format!("bd{w}.png"))));
        // Linear: no wrap to the end of the catalog.
        assert!(!set.backdrops.contains(&PathBuf::from("bd29.png")));
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
                disabled: false,
                view_in_steam: None,
                played_at_ms: None,
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
        let mut state = State { ..State::default() };
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
        let mut state = State { ..State::default() };
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
        let mut state = State { ..State::default() };
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
