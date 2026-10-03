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
    Float, button, column, container, row, scrollable, space, svg, text, text_input,
};
use iced::{
    Alignment, Background, Border, Color, ContentFit, Element, Fill, Font, Length, Padding, Point,
    Rectangle, Renderer, Theme,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Logical width of the start-screen window.
pub const WIDTH: f32 = 640.0;
/// Logical height of the start-screen window.
pub const HEIGHT: f32 = 500.0;

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
const ROW_HEIGHT: f32 = 88.0;
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
    ManualAddTitle(String),
    ManualAddArgs(String),
    ManualPickIcon,
    ManualClearIcon,
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

#[derive(Debug, Clone)]
pub struct StartRow {
    pub title: String,
    pub subtitle: Option<String>,
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
}

/// Edit-mode action for a games-list row.
#[derive(Debug, Clone)]
pub enum EditRow {
    Steam { appid: u32, in_catalog: bool },
    Manual { id: String },
}

impl StartRow {
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
                        subtitle: Some(format!("Steam · {appid}")),
                        target: crate::games::steam::launch_uri(*appid),
                        args: String::new(),
                        play_key: entry.play_key(),
                        icon: steam_icon(game),
                        icon_source,
                        backdrop_path: game.backdrop_path.clone(),
                        edit: None,
                        skeleton: false,
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
                    }
                } else {
                    Self {
                        title: format!("Steam {appid}"),
                        subtitle: Some(format!("steam://rungameid/{appid}")),
                        target: crate::games::steam::launch_uri(*appid),
                        args: String::new(),
                        play_key: entry.play_key(),
                        icon: None,
                        icon_source: None,
                        backdrop_path: None,
                        edit: None,
                        skeleton: false,
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
                    subtitle: Some(target.clone()),
                    target: target.clone(),
                    args: args.clone(),
                    play_key: entry.play_key(),
                    icon: manual_icon(target, icon.as_deref()),
                    icon_source,
                    backdrop_path: None,
                    edit: None,
                    skeleton: false,
                }
            }
        }
    }

    pub fn steam_edit(game: &SteamGame, in_catalog: bool) -> Self {
        let icon_source = steam_icon_source(game);
        Self {
            title: game.name.clone(),
            subtitle: Some(format!("Steam · {}", game.appid)),
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
            subtitle: Some(target.clone()),
            target: target.clone(),
            args: args.clone(),
            play_key: entry.play_key(),
            icon: manual_icon(target, icon.as_deref()),
            icon_source,
            backdrop_path: None,
            edit: Some(EditRow::Manual { id: id.clone() }),
            skeleton: false,
        })
    }

    /// Immersive hero art — peek only (never decodes on the UI thread).
    /// Falls back to the list icon when the hero tier is still cold.
    pub fn hero_icon(&self) -> Option<StartIcon> {
        match &self.icon_source {
            Some(IconSource::File(path)) => icon_cache::hero_cached(path).map(StartIcon),
            Some(IconSource::Shell(path)) => icon_cache::hero_shell_cached(path).map(StartIcon),
            None => None,
        }
        .or_else(|| self.icon.clone())
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
        .and_then(|p| icon_cache::handle_for_path(p))
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

fn manual_icon(target: &str, custom: Option<&str>) -> Option<StartIcon> {
    if let Some(path) = custom.filter(|p| !p.is_empty()) {
        let path = PathBuf::from(path);
        if path.is_file()
            && let Some(handle) = icon_cache::handle_for_path_warn(&path)
        {
            return Some(StartIcon(handle));
        }
        // Fall through to shell icon when the custom image fails to decode.
    }
    if target.starts_with("steam://") {
        return None;
    }
    let path = PathBuf::from(target);
    icon_cache::handle_for_shell(&path).map(StartIcon)
}

/// Probe whether a target path yields a shell icon (for the add-manual modal).
pub fn probe_shell_icon(target: &str) -> Option<StartIcon> {
    manual_icon(target, None)
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

impl RowHintState {
    fn for_selected(state: &State, selected: bool) -> Self {
        if selected {
            Self {
                triangle_progress: state.triangle_progress,
                triangle_armed_t: state.triangle_armed_anim,
                cross_progress: state.cross_progress,
                cross_armed_t: state.cross_armed_anim,
                held: state.held,
                press_anim: state.press_anim,
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

#[derive(Debug, Clone)]
pub struct State {
    pub slide: StartSlide,
    /// Fullscreen console presentation (same data as compact).
    pub immersive: bool,
    /// HWND resize in flight — paint veil until settled.
    pub transition: Option<crate::ui::start::mode::StartTransition>,
    /// Cinematic phase around promote/demote (exit → resize → enter).
    pub transition_phase: Option<(crate::ui::start::mode::TransitionPhase, Instant)>,
    /// Immersive controllers dock target (expanded = Controllers focus).
    pub dock_expanded: bool,
    pub game_selected: usize,
    pub rows: Vec<StartRow>,
    pub controller_selected: usize,
    pub controllers: Vec<StartControllerRow>,
    pub running_target: Option<String>,
    pub replace_confirm: Option<ReplaceConfirm>,
    pub manual_add: Option<ManualAddDraft>,
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
    /// Outgoing backdrop during opacity crossfade: `(play_key, started)`.
    backdrop_outgoing: Option<(String, Instant)>,
    /// Ring flash started with the last successful Identify (Controllers slide).
    identify_flash: Option<IdentifyFlash>,
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
            dock_expanded: false,
            game_selected: 0,
            rows: Vec::new(),
            controller_selected: 0,
            controllers: Vec::new(),
            running_target: None,
            replace_confirm: None,
            manual_add: None,
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
        self.replace_confirm.is_some() || self.manual_add.is_some()
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
            || self.triangle_progress > 0.0
            || self.cross_progress > 0.0
            || self.replace_confirm.is_some()
            || self.manual_add.is_some()
            || self.hint_anims_need_frames()
            || self.identify_flash_active()
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
        self.transition = None;
        self.transition_phase = None;
        self.backdrop_current = None;
        self.backdrop_outgoing = None;
        self.ambient_time = 0.0;
    }

    pub fn begin_transition_phase(
        &mut self,
        phase: crate::ui::start::mode::TransitionPhase,
        now: Instant,
    ) {
        self.dock_anim = None;
        self.strip_anim = None;
        self.transition_phase = Some((phase, now));
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: start immersive transition phase={}",
            phase.label()
        ));
    }

    pub fn clear_transition_phase(&mut self) {
        self.transition_phase = None;
    }

    pub fn phase_progress(&self, now: Instant) -> f32 {
        let Some((phase, started)) = self.transition_phase else {
            return 1.0;
        };
        let elapsed = now.saturating_duration_since(started).as_millis() as u64;
        crate::ui::start::mode::phase_progress(elapsed, phase.duration_ms())
    }

    /// Linear phase fraction — use for EnterImmersive grow/chrome so ease-out cannot crush timing.
    pub fn phase_progress_linear(&self, now: Instant) -> f32 {
        let Some((phase, started)) = self.transition_phase else {
            return 1.0;
        };
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
        now.saturating_duration_since(started) >= Duration::from_millis(dur)
    }

    /// Drop crossfade/land state (demote / leave immersive without closing).
    pub fn clear_backdrop_transition(&mut self) {
        self.backdrop_current = None;
        self.backdrop_outgoing = None;
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
        if waiting {
            if let Some((_, started)) = &mut self.backdrop_current {
                *started = Some(now);
            }
        } else if !armed {
            let was = self
                .backdrop_current
                .as_ref()
                .map(|(k, _)| k.as_str())
                .unwrap_or("none");
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: backdrop ready re-arm key={key} was={was}"
            ));
            self.backdrop_current = Some((key, Some(now)));
        }
    }

    /// Visual params for the incoming backdrop: `(opacity, scale, ox, oy)`.
    pub fn backdrop_incoming_visual(&self, now: Instant) -> Option<(f32, f32, f32, f32)> {
        use crate::ui::start::mode::{
            BACKDROP_LAND_OX0, BACKDROP_LAND_OY0, BACKDROP_LAND_SCALE0, art_fade_progress,
            backdrop_land_offset, backdrop_land_progress, backdrop_land_scale,
        };
        let row = self.rows.get(self.game_selected)?;
        row.backdrop_icon()?;
        match &self.backdrop_current {
            Some((key, None)) if key == &row.play_key => Some((
                0.0,
                BACKDROP_LAND_SCALE0,
                BACKDROP_LAND_OX0,
                BACKDROP_LAND_OY0,
            )),
            Some((key, Some(started))) if key == &row.play_key => {
                let elapsed = now.saturating_duration_since(*started).as_millis() as u64;
                let opacity = art_fade_progress(elapsed);
                let land = backdrop_land_progress(elapsed);
                let scale = backdrop_land_scale(land);
                let (ox, oy) = backdrop_land_offset(land);
                Some((opacity, scale, ox, oy))
            }
            // No state, or stale key after compact nav — still paint current art.
            None | Some(_) => Some((1.0, 1.0, 0.0, 0.0)),
        }
    }

    /// Outgoing backdrop during crossfade: `(play_key, opacity)`.
    pub fn backdrop_outgoing_visual(&self, now: Instant) -> Option<(&str, f32)> {
        let (key, started) = self.backdrop_outgoing.as_ref()?;
        let elapsed = now.saturating_duration_since(*started).as_millis() as u64;
        let opacity = 1.0 - crate::ui::start::mode::art_fade_progress(elapsed);
        if opacity <= 0.01 {
            return None;
        }
        Some((key.as_str(), opacity))
    }

    /// Resolve a peek backdrop handle by play_key (for outgoing layer).
    pub fn backdrop_icon_for_key(&self, play_key: &str) -> Option<StartIcon> {
        self.rows
            .iter()
            .find(|r| r.play_key == play_key)
            .and_then(|r| r.backdrop_icon())
    }

    /// Paths to warm for the current immersive selection window.
    /// Returns `(hero_files, hero_shells, backdrops)`.
    pub fn immersive_warm_paths(&self) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
        use crate::ui::start::vstrip::{self, NEIGHBORS};
        let mut heroes = Vec::new();
        let mut shells = Vec::new();
        let mut backdrops = Vec::new();
        let len = self.rows.len();
        if len == 0 {
            return (heroes, shells, backdrops);
        }
        let selected = self.game_selected.min(len - 1);
        for delta in -NEIGHBORS..=NEIGHBORS {
            let Some(idx) = vstrip::slot_catalog_index(selected, delta, len) else {
                continue;
            };
            let row = &self.rows[idx];
            match &row.icon_source {
                Some(IconSource::File(path)) => heroes.push(path.clone()),
                Some(IconSource::Shell(path)) => shells.push(path.clone()),
                None => {}
            }
        }
        for delta in -1isize..=1 {
            let Some(idx) = vstrip::slot_catalog_index(selected, delta, len) else {
                continue;
            };
            if let Some(path) = self.rows[idx].backdrop_path.clone() {
                backdrops.push(path);
            }
        }
        (heroes, shells, backdrops)
    }

    /// Selection changed: park prior art as outgoing and wait/land the new one.
    pub fn reset_backdrop_fade(&mut self) {
        let now = Instant::now();
        if let Some((prev_key, Some(_))) = self.backdrop_current.take() {
            // Only crossfade out if we were actually showing something.
            if self.backdrop_icon_for_key(&prev_key).is_some() {
                self.backdrop_outgoing = Some((prev_key, now));
            }
        } else {
            self.backdrop_current = None;
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
        let mut busy = false;
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
        if let Some(anim) = self.strip_anim.as_ref() {
            let elapsed = now.saturating_duration_since(anim.started);
            if elapsed >= Duration::from_millis(anim.duration_ms) {
                self.strip_anim = None;
            } else {
                busy = true;
            }
        }
        if let Some((_key, started)) = &self.backdrop_current {
            if let Some(started) = started {
                let elapsed = now.saturating_duration_since(*started);
                if elapsed < Duration::from_millis(crate::ui::start::mode::BACKDROP_LAND_MS) {
                    busy = true;
                }
            } else {
                busy = true;
            }
        }
        if let Some((_, started)) = &self.backdrop_outgoing {
            let elapsed = now.saturating_duration_since(*started);
            if elapsed >= Duration::from_millis(crate::ui::start::mode::ART_FADE_MS) {
                self.backdrop_outgoing = None;
            } else {
                busy = true;
            }
        }
        if self.transition_phase.is_some() {
            busy = true;
        }
        if self.immersive || self.transition.is_some() || self.transition_phase.is_some() {
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
    let row_bottom = row_top + row_h;
    // Before the first on_scroll, fall back to a layout estimate — never force a
    // top pin on every step (that made controller nav look glued to the top).
    let viewport_h = if viewport_h <= 1.0 {
        estimated_list_viewport_h()
    } else {
        viewport_h
    };
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
                Some(row_top)
            } else {
                None
            }
        }
        ScrollReveal::Either => {
            if row_top < view_top {
                Some(row_top)
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

pub fn view<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    now: Instant,
    always_immersive: bool,
    promote_gesture: &'a [crate::domain::gesture::GestureControl],
    stage_h: f32,
) -> Element<'a, StartMessage> {
    use crate::ui::start::mode::TransitionPhase;

    if matches!(
        state.transition_phase.map(|(p, _)| p),
        Some(TransitionPhase::Resizing)
    ) || state.transition.is_some()
    {
        return crate::ui::start::immersive::veil_view(state, now);
    }

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
    let compact = compact_chrome(
        state,
        spectrum,
        now,
        always_immersive,
        promote_gesture,
        flat_hints,
    );
    crate::ui::start::immersive::compact_transition_overlay(compact, veil, false)
}

fn compact_chrome<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    now: Instant,
    always_immersive: bool,
    promote_gesture: &'a [crate::domain::gesture::GestureControl],
    flat_hints: bool,
) -> Element<'a, StartMessage> {
    let header = slide_header(state.slide_progress(now));

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
            "Launch",
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
pub(crate) fn slide_header(progress: f32) -> Element<'static, StartMessage> {
    slide_header_metrics(
        progress,
        HEADER_HEIGHT,
        TITLE_ACTIVE,
        TITLE_INACTIVE,
        CUE_SIZE,
        CUE_SLOT_W,
    )
}

fn slide_header_metrics(
    progress: f32,
    height: f32,
    title_active: f32,
    title_inactive: f32,
    cue_size: f32,
    cue_slot_w: f32,
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

    row![
        container(left)
            .width(Fill)
            .height(Fill)
            .align_x(Alignment::Start)
            .align_y(Alignment::End),
        container(right)
            .width(Fill)
            .height(Fill)
            .align_x(Alignment::End)
            .align_y(Alignment::End),
    ]
    .height(Length::Fixed(height))
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
            action_cluster(
                &[
                    face_hint(
                        FaceButton::Cross,
                        if draft.is_edit() { "Save" } else { "Add" },
                        state.held,
                        state.press_anim,
                    ),
                    face_hint(FaceButton::Circle, "Cancel", state.held, state.press_anim),
                ],
                false,
            ),
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
            action_cluster(
                &[
                    face_hold_hint(
                        FaceButton::Cross,
                        "Proceed",
                        state.cross_progress,
                        state.cross_armed_anim,
                        state.held,
                        state.press_anim,
                    ),
                    face_hint(FaceButton::Circle, "Cancel", state.held, state.press_anim),
                ],
                false,
            ),
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
    if state.manual_add.is_some() {
        let label = if state.manual_add.as_ref().is_some_and(|d| d.is_edit()) {
            "Save"
        } else {
            "Add"
        };
        return footer_band(
            action_cluster(
                &[
                    face_hint(FaceButton::Cross, label, state.held, state.press_anim),
                    face_hint(FaceButton::Circle, "Cancel", state.held, state.press_anim),
                ],
                flat_hints,
            ),
            immersive,
        );
    }
    if state.replace_confirm.is_some() {
        return footer_band(
            action_cluster(
                &[
                    face_hold_hint(
                        FaceButton::Cross,
                        "Proceed",
                        state.cross_progress,
                        state.cross_armed_anim,
                        state.held,
                        state.press_anim,
                    ),
                    face_hint(FaceButton::Circle, "Cancel", state.held, state.press_anim),
                ],
                flat_hints,
            ),
            immersive,
        );
    }

    let circle_label =
        crate::ui::start::mode::cancel_circle_label(immersive, always_immersive, state.editing);
    // Compact → Immersive; immersive (when not always-on) → Compact via the same chord.
    let toggle_label =
        if !state.editing && crate::ui::start::mode::promote_gesture_usable(promote_gesture) {
            if !immersive {
                Some("Immersive")
            } else if !always_immersive {
                Some("Compact")
            } else {
                None
            }
        } else {
            None
        };
    let promote_cue: Option<Element<'a, StartMessage>> = toggle_label.map(|label| {
        gesture_chord_hint(
            promote_gesture,
            label,
            state.reopen_chord_held,
            state.reopen_chord_press,
            flat_hints,
        )
    });

    let cluster: Element<'_, StartMessage> = match state.slide {
        StartSlide::Games => {
            let hints = [
                face_hint(
                    FaceButton::Triangle,
                    if state.editing { "Save" } else { "Edit" },
                    state.held,
                    state.press_anim,
                ),
                face_hint(
                    FaceButton::Circle,
                    circle_label,
                    state.held,
                    state.press_anim,
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
                    state.held,
                    state.press_anim,
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
                row.push(action_cluster(&hints, flat_hints)).into()
            }
        }
        StartSlide::Controllers => {
            let mut row = iced::widget::row![face_cycle_toggle(
                FaceButton::Square,
                state.held,
                state.press_anim,
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
            row.push(action_cluster(
                &[face_hint(
                    FaceButton::Circle,
                    circle_label,
                    state.held,
                    state.press_anim,
                )],
                flat_hints,
            ))
            .into()
        }
    };

    footer_band(cluster, immersive)
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
    let border_color = if pressed {
        theme::ACCENT
    } else {
        Color {
            a: 0.25,
            ..theme::MUTED
        }
    };
    let border_w = if pressed { 2.5 } else { 2.0 };
    let pill = container(text(glyph).size(12.0).color(theme::ACCENT))
        .padding(Padding {
            top: 0.0,
            right: 12.0,
            bottom: 0.0,
            left: 12.0,
        })
        .height(Length::Fixed(HOLD_RING_SIZE))
        .align_y(Alignment::Center)
        .style(move |_theme| container::Style {
            border: Border {
                color: border_color,
                width: border_w,
                radius: (HOLD_RING_SIZE / 2.0).into(),
            },
            ..container::Style::default()
        });
    if flat {
        pill.into()
    } else {
        let scale = 1.0 + (PRESSED_SCALE - 1.0) * press_t.clamp(0.0, 1.0);
        Float::new(pill).scale(scale).into()
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
        text(hint.text_glyph.unwrap_or("?"))
            .size(14.0)
            .color(theme::ACCENT)
            .into()
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
        match row.icon.as_ref() {
            Some(icon) => iced::widget::image(icon.0.clone())
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
        let title = text(&row.title)
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
            .wrapping(Wrapping::None)
            .width(Fill);

        let subtitle = if running {
            text("Running").size(13.0).color(theme::SUCCESS)
        } else if let Some(sub) = row.subtitle.as_ref() {
            text(sub.as_str()).size(13.0).color(sub_color)
        } else {
            text(" ").size(13.0).color(Color::TRANSPARENT)
        }
        .wrapping(Wrapping::None)
        .width(Fill);

        column![title, subtitle]
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
                    "Launch",
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
        assert_eq!(row.subtitle.as_deref(), Some("steam://rungameid/1371980"));
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
            },
        );
        let row = StartRow::from_entry(&entry, &map, true);
        assert!(!row.skeleton);
        assert_eq!(row.title, "Path of Exile");
        assert_eq!(row.subtitle.as_deref(), Some("Steam · 238960"));
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
}
