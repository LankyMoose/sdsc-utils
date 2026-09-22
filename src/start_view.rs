//! Start-screen carousel: Games + Controllers, DualSense / keyboard navigable.

use crate::battery::PowerState;
use crate::color::BatterySpectrum;
use crate::file_icon;
use crate::games::GameEntry;
use crate::percent_ring::{self, POPUP_SIZE};
use crate::prefs::GamesSortMode;
use crate::steam::SteamGame;
use crate::svg_icon;
use crate::theme;
use crate::window_layout;
use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, row, scrollable, space, svg, text, text_input};
use iced::{
    Alignment, Background, Border, Color, ContentFit, Element, Fill, Length, Point, Rectangle,
    Renderer, Theme,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Logical width of the start-screen window.
pub const WIDTH: f32 = 640.0;
/// Logical height of the start-screen window.
pub const HEIGHT: f32 = 500.0;

pub const SLIDE_ANIM_MS: u64 = 180;
const SLIDE_ANIM_MIN_MS: u64 = 60;
const HEADER_HEIGHT: f32 = 36.0;
/// Matches the Games footer band (face hints + padded Add shortcut button).
const FOOTER_HEIGHT: f32 = 32.0;
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
const ROW_GAP: f32 = 10.0;
const ACCENT_BAR_W: f32 = 4.0;
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
}

/// Runtime-resolved game art (Steam path or extracted shell icon).
#[derive(Debug, Clone)]
pub enum StartIcon {
    Path(PathBuf),
    Rgba {
        width: u32,
        height: u32,
        pixels: Arc<[u8]>,
    },
}

#[derive(Debug, Clone)]
pub struct StartRow {
    pub title: String,
    pub subtitle: Option<String>,
    pub target: String,
    pub args: String,
    pub play_key: String,
    pub icon: Option<StartIcon>,
    /// Set in edit mode so Cross/click toggles membership or removes a manual.
    pub edit: Option<EditRow>,
}

/// Edit-mode action for a games-list row.
#[derive(Debug, Clone)]
pub enum EditRow {
    Steam { appid: u32, in_catalog: bool },
    Manual { id: String },
}

impl StartRow {
    pub fn from_entry(entry: &GameEntry, steam_by_id: &HashMap<u32, SteamGame>) -> Self {
        match entry {
            GameEntry::Steam { appid } => {
                if let Some(game) = steam_by_id.get(appid) {
                    Self {
                        title: game.name.clone(),
                        subtitle: Some(format!("Steam · {appid}")),
                        target: crate::steam::launch_uri(*appid),
                        args: String::new(),
                        play_key: entry.play_key(),
                        icon: game.icon_path.clone().map(StartIcon::Path),
                        edit: None,
                    }
                } else {
                    Self {
                        title: format!("Steam {appid}"),
                        subtitle: Some(format!("steam://rungameid/{appid}")),
                        target: crate::steam::launch_uri(*appid),
                        args: String::new(),
                        play_key: entry.play_key(),
                        icon: None,
                        edit: None,
                    }
                }
            }
            GameEntry::Manual {
                title,
                target,
                args,
                icon,
                ..
            } => Self {
                title: title.clone(),
                subtitle: Some(target.clone()),
                target: target.clone(),
                args: args.clone(),
                play_key: entry.play_key(),
                icon: manual_icon(target, icon.as_deref()),
                edit: None,
            },
        }
    }

    pub fn steam_edit(game: &SteamGame, in_catalog: bool) -> Self {
        Self {
            title: game.name.clone(),
            subtitle: Some(format!("Steam · {}", game.appid)),
            target: crate::steam::launch_uri(game.appid),
            args: String::new(),
            play_key: format!("steam:{}", game.appid),
            icon: game.icon_path.clone().map(StartIcon::Path),
            edit: Some(EditRow::Steam {
                appid: game.appid,
                in_catalog,
            }),
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
        Some(Self {
            title: title.clone(),
            subtitle: Some(target.clone()),
            target: target.clone(),
            args: args.clone(),
            play_key: entry.play_key(),
            icon: manual_icon(target, icon.as_deref()),
            edit: Some(EditRow::Manual { id: id.clone() }),
        })
    }

    pub fn in_catalog(&self) -> bool {
        match &self.edit {
            Some(EditRow::Steam { in_catalog, .. }) => *in_catalog,
            Some(EditRow::Manual { .. }) => true,
            None => true,
        }
    }
}

fn manual_icon(target: &str, custom: Option<&str>) -> Option<StartIcon> {
    if let Some(path) = custom.filter(|p| !p.is_empty()) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(StartIcon::Path(path));
        }
    }
    if target.starts_with("steam://") {
        return None;
    }
    let path = PathBuf::from(target);
    // Extract a bit larger than the cell so Cover scales cleanly.
    let (width, height, pixels) = file_icon::rgba_for_path(&path, ICON_H as u32)?;
    Some(StartIcon::Rgba {
        width,
        height,
        pixels: pixels.into(),
    })
}

/// Probe whether a target path yields a shell icon (for the add-manual modal).
pub fn probe_shell_icon(target: &str) -> Option<StartIcon> {
    manual_icon(target, None)
}

/// Connected-pad row for the Controllers slide (no remember / nickname edit).
#[derive(Debug, Clone, PartialEq)]
pub struct StartControllerRow {
    pub serial: String,
    pub title: String,
    pub connection: String,
    pub state: String,
    pub percent: u8,
    pub low: bool,
    pub bluetooth: bool,
    pub eta: Option<String>,
}

#[derive(Debug, Clone)]
struct SlideAnim {
    from_x: f32,
    to: StartSlide,
    duration_ms: u64,
    started: Instant,
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
pub struct State {
    pub slide: StartSlide,
    pub game_selected: usize,
    pub rows: Vec<StartRow>,
    pub controller_selected: usize,
    pub controllers: Vec<StartControllerRow>,
    pub running_target: Option<String>,
    pub replace_confirm: Option<ReplaceConfirm>,
    pub manual_add: Option<ManualAddDraft>,
    pub triangle_progress: f32,
    pub cross_progress: f32,
    pub editing: bool,
    pub sort_mode: GamesSortMode,
    /// `play_key` of the row selected when edit mode was entered (restored on Save/Cancel).
    pub edit_anchor_play_key: Option<String>,
    /// Last known games-list scroll offset / viewport height (for keep-selection-visible).
    games_scroll_y: f32,
    games_viewport_h: f32,
    controllers_scroll_y: f32,
    controllers_viewport_h: f32,
    anim: Option<SlideAnim>,
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
            game_selected: 0,
            rows: Vec::new(),
            controller_selected: 0,
            controllers: Vec::new(),
            running_target: None,
            replace_confirm: None,
            manual_add: None,
            triangle_progress: 0.0,
            cross_progress: 0.0,
            editing: false,
            sort_mode: GamesSortMode::default(),
            edit_anchor_play_key: None,
            games_scroll_y: 0.0,
            games_viewport_h: 0.0,
            controllers_scroll_y: 0.0,
            controllers_viewport_h: 0.0,
            anim: None,
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
        self.controllers = controllers;
        if self.controllers.is_empty() {
            self.controller_selected = 0;
        } else {
            self.controller_selected = self.controller_selected.min(self.controllers.len() - 1);
        }
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
        self.replace_confirm = None;
        self.manual_add = None;
        self.triangle_progress = 0.0;
        self.cross_progress = 0.0;
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
        self.anim.is_some()
    }

    pub fn needs_frames(&self) -> bool {
        self.anim.is_some()
            || self.triangle_progress > 0.0
            || self.cross_progress > 0.0
            || self.replace_confirm.is_some()
            || self.manual_add.is_some()
    }

    pub fn tick_anim(&mut self, now: Instant) -> bool {
        let Some(anim) = self.anim.as_ref() else {
            return false;
        };
        let elapsed = now.saturating_duration_since(anim.started);
        if elapsed >= Duration::from_millis(anim.duration_ms) {
            self.slide = anim.to;
            self.anim = None;
            return true;
        }
        true
    }

    /// Request a slide target. Interruptible: restarts from the current scroll position.
    /// No-op if already at / animating toward `to`.
    pub fn request_slide(&mut self, to: StartSlide, now: Instant) {
        if self.anim.as_ref().is_some_and(|a| a.to == to) {
            return;
        }
        if self.anim.is_none() && self.slide == to {
            return;
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
            return;
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
    fn slide_progress(&self, now: Instant) -> f32 {
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
) -> Element<'a, StartMessage> {
    let header = slide_header(state.slide_progress(now));

    let body = if state.manual_add.is_some() {
        manual_add_view(state)
    } else if state.replace_confirm.is_some() {
        replace_confirm_view(state)
    } else {
        carousel_body(state, spectrum, now)
    };

    let hint = footer_hint(state);

    container(
        column![
            header,
            container(space())
                .width(Fill)
                .height(Length::Fixed(1.0))
                .style(theme::configure_header_rule),
            container(body).width(Fill).height(Fill),
            hint,
        ]
        .spacing(10)
        .padding([PAD_Y, PADDING])
        .width(Fill)
        .height(Fill),
    )
    .width(Fill)
    .height(Fill)
    .style(theme::root)
    .into()
}

/// Header titles and L2/R2 cues interpolate with carousel progress (0 = Games, 1 = Controllers).
fn slide_header(progress: f32) -> Element<'static, StartMessage> {
    let games_t = progress;
    let controllers_t = 1.0 - progress;

    let games_label = text(StartSlide::Games.title())
        .size(lerp(TITLE_ACTIVE, TITLE_INACTIVE, games_t))
        .color(lerp_color(
            theme::INK,
            theme::alpha(theme::MUTED, 0.45),
            games_t,
        ));
    let controllers_label = text(StartSlide::Controllers.title())
        .size(lerp(TITLE_ACTIVE, TITLE_INACTIVE, controllers_t))
        .color(lerp_color(
            theme::INK,
            theme::alpha(theme::MUTED, 0.45),
            controllers_t,
        ));

    let left = row![slide_cue("L2", games_t, Alignment::Start), games_label,]
        .spacing(0)
        .align_y(Alignment::End);

    let right = row![
        controllers_label,
        slide_cue("R2", controllers_t, Alignment::End),
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
    .height(Length::Fixed(HEADER_HEIGHT))
    .into()
}

/// Width-revealed + faded L2/R2 cue (`amount` 0 = hidden, 1 = fully shown).
fn slide_cue(label: &'static str, amount: f32, align: Alignment) -> Element<'static, StartMessage> {
    let amount = amount.clamp(0.0, 1.0);
    let width = CUE_SLOT_W * amount;
    container(
        text(label)
            .size(CUE_SIZE)
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
    crate::start_carousel::carousel(
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
                face_svg(FaceButton::Square),
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
        column![].spacing(ROW_GAP).width(Fill),
        |col, (index, row)| {
            let selected = index == state.game_selected;
            let running = !state.editing
                && state
                    .running_target
                    .as_ref()
                    .is_some_and(|t| t == &row.target);
            col.push(game_row(
                index,
                row,
                selected,
                running,
                state.editing,
                if selected {
                    state.triangle_progress
                } else {
                    0.0
                },
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
        column![].spacing(ROW_GAP).width(Fill),
        |col, (index, row)| {
            let selected = index == state.controller_selected;
            col.push(controller_row(
                index,
                row,
                selected,
                spectrum,
                if selected {
                    state.triangle_progress
                } else {
                    0.0
                },
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

fn manual_add_view(state: &State) -> Element<'_, StartMessage> {
    let Some(draft) = state.manual_add.as_ref() else {
        return space().into();
    };

    let icon_el: Element<'_, StartMessage> =
        if let Some(path) = draft.icon_path.as_ref().filter(|p| p.is_file()) {
            start_icon_image(&StartIcon::Path(path.clone()))
        } else if let Some(shell) = draft.shell_icon.as_ref() {
            start_icon_image(shell)
        } else {
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
        };

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
            action_cluster(&[
                ActionHint {
                    face: Some(FaceButton::Cross),
                    text_glyph: None,
                    label: if draft.is_edit() { "Save" } else { "Add" },
                    hold: None,
                },
                ActionHint {
                    face: Some(FaceButton::Circle),
                    text_glyph: None,
                    label: "Cancel",
                    hold: None,
                },
            ]),
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
    match icon {
        StartIcon::Path(path) => {
            iced::widget::image(iced::widget::image::Handle::from_path(path.clone()))
                .width(Length::Fixed(ICON_W))
                .height(Length::Fixed(ICON_H))
                .content_fit(ContentFit::Cover)
                .into()
        }
        StartIcon::Rgba {
            width,
            height,
            pixels,
        } => iced::widget::image(iced::widget::image::Handle::from_rgba(
            *width,
            *height,
            pixels.to_vec(),
        ))
        .width(Length::Fixed(ICON_W))
        .height(Length::Fixed(ICON_H))
        .content_fit(ContentFit::Cover)
        .into(),
    }
}

fn replace_confirm_view(state: &State) -> Element<'_, StartMessage> {
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
            action_cluster(&[
                ActionHint {
                    face: Some(FaceButton::Cross),
                    text_glyph: None,
                    label: "Proceed",
                    hold: Some(state.cross_progress),
                },
                ActionHint {
                    face: Some(FaceButton::Circle),
                    text_glyph: None,
                    label: "Cancel",
                    hold: None,
                },
            ]),
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
}

struct ActionHint {
    face: Option<FaceButton>,
    text_glyph: Option<&'static str>,
    label: &'static str,
    hold: Option<f32>,
}

fn footer_hint(state: &State) -> Element<'_, StartMessage> {
    if state.manual_add.is_some() {
        let label = if state.manual_add.as_ref().is_some_and(|d| d.is_edit()) {
            "Save"
        } else {
            "Add"
        };
        return footer_band(action_cluster(&[
            ActionHint {
                face: Some(FaceButton::Cross),
                text_glyph: None,
                label,
                hold: None,
            },
            ActionHint {
                face: Some(FaceButton::Circle),
                text_glyph: None,
                label: "Cancel",
                hold: None,
            },
        ]));
    }
    if state.replace_confirm.is_some() {
        return footer_band(action_cluster(&[
            ActionHint {
                face: Some(FaceButton::Cross),
                text_glyph: None,
                label: "Proceed",
                hold: Some(state.cross_progress),
            },
            ActionHint {
                face: Some(FaceButton::Circle),
                text_glyph: None,
                label: "Cancel",
                hold: None,
            },
        ]));
    }

    let cluster: Element<'_, StartMessage> = match state.slide {
        StartSlide::Games => {
            let hints = [
                ActionHint {
                    face: Some(FaceButton::Square),
                    text_glyph: None,
                    label: if state.editing { "Save" } else { "Edit" },
                    hold: None,
                },
                ActionHint {
                    face: Some(FaceButton::Circle),
                    text_glyph: None,
                    label: if state.editing { "Cancel" } else { "Close" },
                    hold: None,
                },
            ];
            iced::widget::row![
                action_cluster(&hints),
                sort_mode_picker(state.sort_mode, state.editing),
            ]
            .spacing(28)
            .align_y(Alignment::Center)
            .into()
        }
        StartSlide::Controllers => action_cluster(&[ActionHint {
            face: Some(FaceButton::Circle),
            text_glyph: None,
            label: "Close",
            hold: None,
        }]),
    };

    let cluster = if state.editing && matches!(state.slide, StartSlide::Games) {
        iced::widget::row![
            cluster,
            button(text("Add shortcut…").size(14.0).color(theme::ACCENT))
                .padding([6, 12])
                .on_press(StartMessage::AddShortcut)
                .style(theme::ghost),
        ]
        .spacing(28)
        .align_y(Alignment::Center)
        .into()
    } else {
        cluster
    };

    footer_band(cluster)
}

fn footer_band(content: Element<'_, StartMessage>) -> Element<'_, StartMessage> {
    container(content)
        .width(Fill)
        .height(Length::Fixed(FOOTER_HEIGHT))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .into()
}

fn sort_mode_picker(mode: GamesSortMode, muted: bool) -> Element<'static, StartMessage> {
    let dim = theme::alpha(theme::MUTED, 0.45);
    let options_color = if muted { dim } else { theme::ACCENT };
    let last_played = sort_mode_label(
        "Last played",
        matches!(mode, GamesSortMode::LastPlayed),
        muted,
    );
    let alpha = sort_mode_label("A–Z", matches!(mode, GamesSortMode::Alphabetical), muted);
    iced::widget::row![
        text("Options").size(14.0).color(options_color),
        last_played,
        text("·").size(14.0).color(dim),
        alpha,
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn sort_mode_label(
    label: &'static str,
    active: bool,
    muted: bool,
) -> Element<'static, StartMessage> {
    let color = if muted {
        theme::alpha(theme::MUTED, 0.45)
    } else if active {
        theme::INK
    } else {
        theme::alpha(theme::MUTED, 0.55)
    };
    text(label).size(14.0).color(color).into()
}

fn action_cluster(hints: &[ActionHint]) -> Element<'static, StartMessage> {
    action_cluster_spaced(hints, 28.0)
}

fn action_cluster_spaced(hints: &[ActionHint], spacing: f32) -> Element<'static, StartMessage> {
    let mut row = row![].spacing(spacing).align_y(Alignment::Center);
    for hint in hints {
        row = row.push(action_hint(hint));
    }
    row.into()
}

fn action_hint(hint: &ActionHint) -> Element<'static, StartMessage> {
    let glyph: Element<'static, StartMessage> = if let Some(progress) = hint.hold {
        if let Some(face) = hint.face {
            hold_glyph(face, progress)
        } else {
            text(hint.text_glyph.unwrap_or("?"))
                .size(14.0)
                .color(theme::ACCENT)
                .into()
        }
    } else if let Some(face) = hint.face {
        face_svg(face)
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

fn face_svg(face: FaceButton) -> Element<'static, StartMessage> {
    svg(svg::Handle::from_memory(face.svg().as_bytes()))
        .width(Length::Fixed(FACE_GLYPH_SIZE))
        .height(Length::Fixed(FACE_GLYPH_SIZE))
        .into()
}

fn hold_glyph(face: FaceButton, progress: f32) -> Element<'static, StartMessage> {
    iced::widget::stack![
        hold_ring(progress),
        container(face_svg(face))
            .width(Length::Fixed(HOLD_RING_SIZE))
            .height(Length::Fixed(HOLD_RING_SIZE))
            .center_x(Fill)
            .center_y(Fill),
    ]
    .width(Length::Fixed(HOLD_RING_SIZE))
    .height(Length::Fixed(HOLD_RING_SIZE))
    .into()
}

fn hold_ring(progress: f32) -> Element<'static, StartMessage> {
    iced::widget::canvas(HoldRing { progress })
        .width(Length::Fixed(HOLD_RING_SIZE))
        .height(Length::Fixed(HOLD_RING_SIZE))
        .into()
}

struct HoldRing {
    progress: f32,
}

impl canvas::Program<StartMessage> for HoldRing {
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
        let radius = (bounds.width.min(bounds.height) / 2.0) - 2.0;
        let track = Path::circle(center, radius);
        frame.stroke(
            &track,
            Stroke::default().with_width(2.0).with_color(Color {
                a: 0.25,
                ..theme::MUTED
            }),
        );
        if self.progress > 0.01 {
            let start = -std::f32::consts::FRAC_PI_2;
            let end = start + self.progress * std::f32::consts::TAU;
            let steps = ((self.progress * 48.0).ceil() as usize).max(2);
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
                Stroke::default().with_width(2.5).with_color(theme::ACCENT),
            );
        }
        vec![frame.into_geometry()]
    }
}

fn selection_bar(selected: bool) -> Element<'static, StartMessage> {
    container(space())
        .width(Length::Fixed(ACCENT_BAR_W))
        .height(Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(if selected {
                theme::ACCENT
            } else {
                Color::TRANSPARENT
            })),
            border: Border {
                radius: 2.0.into(),
                ..Default::default()
            },
            ..container::Style::default()
        })
        .into()
}

fn game_row(
    index: usize,
    row: &StartRow,
    selected: bool,
    running: bool,
    editing: bool,
    triangle_progress: f32,
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

    let icon_inner: Element<'_, StartMessage> = match row.icon.as_ref() {
        Some(StartIcon::Path(path)) => {
            iced::widget::image(iced::widget::image::Handle::from_path(path.clone()))
                .width(Length::Fixed(ICON_W))
                .height(Length::Fixed(ICON_H))
                .content_fit(ContentFit::Cover)
                .into()
        }
        Some(StartIcon::Rgba {
            width,
            height,
            pixels,
        }) => iced::widget::image(iced::widget::image::Handle::from_rgba(
            *width,
            *height,
            pixels.to_vec(),
        ))
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

    let title = text(&row.title)
        .size(19.0)
        .color(title_color)
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

    let titles = column![title, subtitle].spacing(4).width(Fill).clip(true);

    let mut content = row![selection_bar(selected), icon]
        .spacing(14)
        .align_y(Alignment::Center);

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
        let mut hints = Vec::new();
        if editing {
            let label = match row.edit.as_ref() {
                Some(EditRow::Manual { .. }) => "Remove",
                Some(EditRow::Steam { .. }) | None => "Toggle",
            };
            hints.push(ActionHint {
                face: Some(FaceButton::Cross),
                text_glyph: None,
                label,
                hold: None,
            });
            if matches!(row.edit, Some(EditRow::Manual { .. })) {
                hints.push(ActionHint {
                    face: Some(FaceButton::Triangle),
                    text_glyph: None,
                    label: "Edit",
                    hold: None,
                });
            }
        } else {
            hints.push(ActionHint {
                face: Some(FaceButton::Cross),
                text_glyph: None,
                label: "Launch",
                hold: None,
            });
            if running {
                hints.push(ActionHint {
                    face: Some(FaceButton::Triangle),
                    text_glyph: None,
                    label: "Close game",
                    hold: Some(triangle_progress),
                });
            }
        }
        content = content.push(action_cluster_spaced(&hints, ROW_ACTION_SPACING));
    }

    button(content.width(Fill).height(Length::Fixed(ROW_HEIGHT)))
        .padding([0, 12])
        .width(Fill)
        .height(Length::Fixed(ROW_HEIGHT))
        .on_press(StartMessage::Launch(index))
        .style(theme::menu_row(selected))
        .into()
}

fn controller_row<'a>(
    index: usize,
    row: &'a StartControllerRow,
    selected: bool,
    spectrum: &BatterySpectrum,
    triangle_progress: f32,
) -> Element<'a, StartMessage> {
    let ring_color = theme::from_rgb(spectrum.color_at_percent(row.percent));
    let ring =
        percent_ring::percent_ring(row.percent, ring_color, POPUP_SIZE * 0.95, row.eta.clone());

    let meta_color = if row.low {
        theme::WARNING
    } else {
        theme::MUTED
    };
    let titles = column![
        text(&row.title).size(19.0).color(theme::INK),
        text(format!("{} · {}", row.connection, row.state))
            .size(13.0)
            .color(meta_color),
    ]
    .spacing(4)
    .width(Fill)
    .clip(true);

    let mut content = row![selection_bar(selected), ring, titles]
        .spacing(14)
        .align_y(Alignment::Center);

    if selected {
        let mut hints = vec![ActionHint {
            face: Some(FaceButton::Cross),
            text_glyph: None,
            label: "Identify",
            hold: None,
        }];
        if row.bluetooth {
            hints.push(ActionHint {
                face: Some(FaceButton::Triangle),
                text_glyph: None,
                label: "Power off",
                hold: Some(triangle_progress),
            });
        }
        content = content.push(action_cluster_spaced(&hints, ROW_ACTION_SPACING));
    }

    button(
        content
            .width(Fill)
            .height(Length::Fixed(CONTROLLER_ROW_HEIGHT)),
    )
    .padding([0, 12])
    .width(Fill)
    .height(Length::Fixed(CONTROLLER_ROW_HEIGHT))
    .on_press(StartMessage::SelectController(index))
    .style(theme::menu_row(selected))
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

    #[test]
    fn scroll_y_reveals_below_and_above_viewport() {
        // Row 5 sits at y=490 with stride 98; viewport 300 starting at 0 → need scroll (Either/Down).
        let y = scroll_y_to_reveal(5, 20, 88.0, 10.0, 0.0, 300.0, ScrollReveal::Either).unwrap();
        assert!((y - (5.0 * 98.0 + 88.0 - 300.0)).abs() < 0.1);

        // Already visible near top with estimated/real viewport — no scroll.
        assert!(scroll_y_to_reveal(1, 20, 88.0, 10.0, 0.0, 300.0, ScrollReveal::Either).is_none());
        // Unknown viewport uses estimate; early rows should still not force a top pin.
        assert!(scroll_y_to_reveal(1, 20, 88.0, 10.0, 0.0, 0.0, ScrollReveal::Down).is_none());

        // Scrolled past selection → scroll back up to row top.
        let y = scroll_y_to_reveal(2, 20, 88.0, 10.0, 400.0, 300.0, ScrollReveal::Either).unwrap();
        assert!((y - 2.0 * 98.0).abs() < 0.1);
    }

    #[test]
    fn scroll_y_leading_edge_only() {
        // Row above viewport: Down must not scroll; Up must pin to top.
        assert!(scroll_y_to_reveal(2, 20, 88.0, 10.0, 400.0, 300.0, ScrollReveal::Down).is_none());
        let y = scroll_y_to_reveal(2, 20, 88.0, 10.0, 400.0, 300.0, ScrollReveal::Up).unwrap();
        assert!((y - 2.0 * 98.0).abs() < 0.1);

        // Row below viewport: Up must not scroll; Down must pin to bottom.
        assert!(scroll_y_to_reveal(5, 20, 88.0, 10.0, 0.0, 300.0, ScrollReveal::Up).is_none());
        let y = scroll_y_to_reveal(5, 20, 88.0, 10.0, 0.0, 300.0, ScrollReveal::Down).unwrap();
        assert!((y - (5.0 * 98.0 + 88.0 - 300.0)).abs() < 0.1);
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
        assert!(scroll_y_center(1, 20, 88.0, 10.0, 0.0, 300.0, false).is_none());
        let y = scroll_y_center(5, 20, 88.0, 10.0, 0.0, 300.0, false).unwrap();
        let expected = 5.0 * 98.0 - (300.0 - 88.0) / 2.0;
        assert!((y - expected).abs() < 0.1);
        // Force recenters even when already visible.
        let y = scroll_y_center(1, 20, 88.0, 10.0, 0.0, 300.0, true).unwrap();
        let expected = (1.0_f32 * 98.0 - (300.0 - 88.0) / 2.0).max(0.0);
        assert!((y - expected).abs() < 0.1);
    }
}
