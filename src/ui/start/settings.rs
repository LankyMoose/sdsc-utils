//! In-window Start Screen settings (Options): compact modal / immersive right drawer.

use crate::persist::prefs::{
    AUTO_OPEN_MODES, IMMERSIVE_LAYOUT_MODES, ImmersiveLayout, START_SCREEN_IDLE_SECS_STEPS,
    START_SCREEN_SLEEP_SECS_STEPS, StartAutoOpen, format_timeout_duration, min_sleep_secs_for_idle,
    secs_step_index,
};
use crate::ui::layout as window_layout;
use crate::ui::start::view::{ScrollReveal, StartMessage, State, scroll_y_to_reveal_bounds};
use crate::ui::theme;
use iced::font::Weight;
use iced::widget::text::Wrapping;
use iced::widget::{column, container, mouse_area, row, scrollable, slider, space, text, toggler};
use iced::{Alignment, Element, Fill, Font, Length, Padding};
use std::time::{Duration, Instant};

/// Immersive settings drawer content width (full height).
pub const DRAWER_W: f32 = 400.0;
/// Compact modal width (~window × 0.75).
pub const MODAL_W: f32 = 576.0;
const DRAWER_ANIM_MS: u64 = 220;
const ROW_PAD: f32 = 10.0;
const COL_GAP: f32 = 6.0;
/// Inner padding of the expanded auto-open dropdown panel.
const MENU_PANEL_PAD: f32 = 4.0;
/// Gap between scrollable content and the embedded scrollbar (matches Configure/Popup).
const SCROLL_GAP: f32 = 8.0;
/// Padding inside the settings scrollable content (must match `panel_view` list padding).
/// Kept in the same ballpark as [`REVEAL_INSET`] — just enough for the focus ring.
const CONTENT_PAD_Y: f32 = 8.0;
const CONTENT_PAD_X: f32 = 4.0;
/// Gap between the static title bar and the scrollable list.
const HEADER_LIST_GAP: f32 = 8.0;
/// Height of the spacer between settings sections (shared by `space_gap` and
/// `focus_row_bounds` so geometry can't drift from the built layout).
const SECTION_GAP_H: f32 = 8.0;
/// Spacing between options inside an expanded dropdown panel (shared by
/// `selector_menu_panel` stacking and `menu_panel_h` geometry).
const MENU_OPTION_GAP: f32 = 4.0;
/// Inner spacing between a slider label and its track (shared by `slider_row`
/// stacking and `slider_row_h` geometry).
const SLIDER_INNER_GAP: f32 = 4.0;
/// Extra inset when revealing a focused row so it is not flush to the viewport edge.
const REVEAL_INSET: f32 = 8.0;
/// If an Up-reveal target sits above this, pin scroll to 0 so section labels stay visible.
const TOP_PIN_MAX_ROW_TOP: f32 = CONTENT_PAD_Y + 48.0;
/// Compact modal max content height (matches overlay card).
const COMPACT_VIEWPORT_FALLBACK: f32 = 480.0;
const COMPACT_MODAL_MAX_H: f32 = 528.0;

/// Prefs snapshot for the Start settings panel (Enabled stays Configure-only).
#[derive(Debug, Clone)]
pub struct StartSettingsSnapshot {
    pub auto_open: StartAutoOpen,
    pub always_immersive: bool,
    pub clock_enabled: bool,
    pub immersive_layout: ImmersiveLayout,
    pub inactive_secs: u32,
    pub sleep_secs: u32,
    pub inactive_dim_percent: u8,
    pub sounds_enabled: bool,
    pub sound_volume: u8,
    pub haptics_enabled: bool,
    pub haptics_strength: u8,
}

/// Focusable rows in the Start settings list (dynamic when sounds/haptics off).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    AutoOpen,
    AlwaysImmersive,
    Clock,
    ImmersiveLayout,
    IdleSecs,
    SleepSecs,
    IdleDim,
    Sounds,
    SoundVolume,
    Haptics,
    HapticsStrength,
}

impl SettingsRow {
    /// Single row table in display order. `focusable()`, `panel_view`, and
    /// `focus_row_bounds` all derive from this: a row missing here is missing
    /// everywhere (visibly), and reordering here reorders all three together.
    /// To add a row: append/reorder here, extend `visible()` + `section_before()`
    /// / `gap_before()` + the `panel_view` build match + `kind()`/dispatch arms.
    pub const ALL: [Self; 11] = [
        Self::AutoOpen,
        Self::AlwaysImmersive,
        Self::Clock,
        Self::ImmersiveLayout,
        Self::IdleSecs,
        Self::SleepSecs,
        Self::IdleDim,
        Self::Sounds,
        Self::SoundVolume,
        Self::Haptics,
        Self::HapticsStrength,
    ];

    /// Conditional visibility shared by ALL THREE paths (focus order, panel
    /// build, bounds geometry). Listed exhaustively so a new row fails to
    /// compile until its visibility is declared.
    pub fn visible(self, snapshot: &StartSettingsSnapshot) -> bool {
        match self {
            Self::AutoOpen
            | Self::AlwaysImmersive
            | Self::Clock
            | Self::ImmersiveLayout
            | Self::IdleSecs
            | Self::SleepSecs
            | Self::IdleDim
            | Self::Sounds
            | Self::Haptics => true,
            Self::SoundVolume => snapshot.sounds_enabled,
            Self::HapticsStrength => snapshot.haptics_enabled,
        }
    }

    pub fn focusable(snapshot: &StartSettingsSnapshot) -> Vec<Self> {
        debug_assert!(
            {
                let mut seen = [false; 11];
                let mut dup = false;
                for row in Self::ALL {
                    let idx = row as usize;
                    if seen[idx] {
                        dup = true;
                    }
                    seen[idx] = true;
                }
                !dup && seen.iter().all(|&s| s)
            },
            "SettingsRow::ALL must list every variant exactly once"
        );
        Self::ALL
            .iter()
            .copied()
            .filter(|row| row.visible(snapshot))
            .collect()
    }

    /// Section label rendered immediately before this row, if any. Anchored to
    /// the row so inserting/reordering rows carries its section along.
    fn section_before(self) -> Option<&'static str> {
        match self {
            Self::AutoOpen => Some("Opening"),
            Self::AlwaysImmersive => Some("Immersive"),
            Self::Sounds => Some("Feedback"),
            Self::Clock
            | Self::ImmersiveLayout
            | Self::IdleSecs
            | Self::SleepSecs
            | Self::IdleDim
            | Self::SoundVolume
            | Self::Haptics
            | Self::HapticsStrength => None,
        }
    }

    /// Whether a section spacer precedes this row's section (i.e. this row
    /// opens the Immersive / Feedback section after the previous block).
    fn gap_before(self) -> bool {
        match self {
            Self::AlwaysImmersive | Self::Sounds => true,
            Self::AutoOpen
            | Self::Clock
            | Self::ImmersiveLayout
            | Self::IdleSecs
            | Self::SleepSecs
            | Self::IdleDim
            | Self::SoundVolume
            | Self::Haptics
            | Self::HapticsStrength => false,
        }
    }

    /// Estimated row height, sharing the same `*_row_h` fns (and through them
    /// the same constants) as the builders so geometry can't drift.
    fn height(self, ty: TypeScale) -> f32 {
        match self.kind() {
            RowKind::Toggle => toggle_row_h(ty.toggle),
            RowKind::Selector => selector_row_h(ty.helper, ty.toggle),
            RowKind::Slider => slider_row_h(ty.helper),
        }
    }

    /// Widget kind for a row, declared once; confirm/nudge dispatch over it.
    pub fn kind(self) -> RowKind {
        match self {
            Self::AutoOpen | Self::ImmersiveLayout => RowKind::Selector,
            Self::AlwaysImmersive | Self::Clock | Self::Sounds | Self::Haptics => RowKind::Toggle,
            Self::IdleSecs
            | Self::SleepSecs
            | Self::IdleDim
            | Self::SoundVolume
            | Self::HapticsStrength => RowKind::Slider,
        }
    }

    /// Current on/off for toggle rows.
    fn toggle_value(self, snapshot: &StartSettingsSnapshot) -> bool {
        match self {
            Self::AlwaysImmersive => snapshot.always_immersive,
            Self::Clock => snapshot.clock_enabled,
            Self::Sounds => snapshot.sounds_enabled,
            Self::Haptics => snapshot.haptics_enabled,
            row => unreachable!("{row:?} is not a toggle row"),
        }
    }

    /// Toggle message constructor for toggle rows.
    fn set_toggle(self, on: bool) -> StartMessage {
        match self {
            Self::AlwaysImmersive => StartMessage::SetAlwaysImmersive(on),
            Self::Clock => StartMessage::SetClock(on),
            Self::Sounds => StartMessage::SetSounds(on),
            Self::Haptics => StartMessage::SetHaptics(on),
            row => unreachable!("{row:?} is not a toggle row"),
        }
    }

    /// Dropdown-open message for selector rows (confirm path).
    fn open_menu(self) -> StartMessage {
        match self {
            Self::AutoOpen => StartMessage::ToggleAutoOpenMenu,
            Self::ImmersiveLayout => StartMessage::ToggleLayoutMenu,
            row => unreachable!("{row:?} is not a selector row"),
        }
    }

    /// Dropdown length for selector rows (option count driving highlight wrap).
    pub fn selector_count(self) -> usize {
        match self {
            Self::AutoOpen => AUTO_OPEN_MODES.len(),
            Self::ImmersiveLayout => IMMERSIVE_LAYOUT_MODES.len(),
            row => unreachable!("{row:?} is not a selector row"),
        }
    }

    /// Highlight init: index of the current value in the dropdown order.
    pub fn initial_highlight(self, snapshot: &StartSettingsSnapshot) -> usize {
        match self {
            Self::AutoOpen => AUTO_OPEN_MODES
                .iter()
                .position(|&m| m == snapshot.auto_open)
                .unwrap_or(0),
            Self::ImmersiveLayout => IMMERSIVE_LAYOUT_MODES
                .iter()
                .position(|&m| m == snapshot.immersive_layout)
                .unwrap_or(0),
            row => unreachable!("{row:?} is not a selector row"),
        }
    }

    /// Display label of the dropdown option at `index`.
    pub fn selector_label(self, index: usize) -> &'static str {
        match self {
            Self::AutoOpen => AUTO_OPEN_MODES[index].label(),
            Self::ImmersiveLayout => IMMERSIVE_LAYOUT_MODES[index].label(),
            row => unreachable!("{row:?} is not a selector row"),
        }
    }

    /// Whether the dropdown option at `index` is the committed value.
    pub fn selector_selected(self, index: usize, snapshot: &StartSettingsSnapshot) -> bool {
        match self {
            Self::AutoOpen => AUTO_OPEN_MODES
                .get(index)
                .is_some_and(|&m| m == snapshot.auto_open),
            Self::ImmersiveLayout => IMMERSIVE_LAYOUT_MODES
                .get(index)
                .is_some_and(|&m| m == snapshot.immersive_layout),
            row => unreachable!("{row:?} is not a selector row"),
        }
    }

    /// Commit message for the dropdown option at `index` (clamped, matching
    /// the old per-menu confirm paths).
    pub fn selector_select(self, index: usize) -> StartMessage {
        match self {
            Self::AutoOpen => {
                StartMessage::SetAutoOpen(AUTO_OPEN_MODES[index.min(AUTO_OPEN_MODES.len() - 1)])
            }
            Self::ImmersiveLayout => StartMessage::SetImmersiveLayout(
                IMMERSIVE_LAYOUT_MODES[index.min(IMMERSIVE_LAYOUT_MODES.len() - 1)],
            ),
            row => unreachable!("{row:?} is not a selector row"),
        }
    }

    /// Quick-cycle for selector rows without opening the menu.
    /// AutoOpen respects direction (next/prev); Layout flips either way (delta ignored).
    fn cycle_selector(self, snapshot: &StartSettingsSnapshot, delta: i8) -> StartMessage {
        match self {
            Self::AutoOpen => StartMessage::SetAutoOpen(if delta > 0 {
                snapshot.auto_open.next()
            } else {
                snapshot.auto_open.prev()
            }),
            Self::ImmersiveLayout => {
                StartMessage::SetImmersiveLayout(snapshot.immersive_layout.cycle())
            }
            row => unreachable!("{row:?} is not a selector row"),
        }
    }

    /// Step for slider rows; `None` at limits.
    fn nudge_slider(self, snapshot: &StartSettingsSnapshot, step: isize) -> Option<StartMessage> {
        match self {
            Self::IdleSecs => {
                step_in_table(snapshot.inactive_secs, START_SCREEN_IDLE_SECS_STEPS, step)
                    .map(StartMessage::SetInactiveSecs)
            }
            Self::SleepSecs => {
                let choices = sleep_choices_for(snapshot.inactive_secs);
                step_in_table(snapshot.sleep_secs, &choices, step).map(StartMessage::SetSleepSecs)
            }
            Self::IdleDim => step_percent(snapshot.inactive_dim_percent, step)
                .map(StartMessage::SetInactiveDimPercent),
            Self::SoundVolume => {
                step_percent(snapshot.sound_volume, step).map(StartMessage::SetSoundVolume)
            }
            Self::HapticsStrength => {
                step_percent(snapshot.haptics_strength, step).map(StartMessage::SetHapticsStrength)
            }
            row => unreachable!("{row:?} is not a slider row"),
        }
    }

    /// Absolute-value commit for slider rows (mouse drag): maps the widget's
    /// f32 position back onto the committed value with the same tables and
    /// rounding the panel positions use. The apply side re-clamps (idle/sleep
    /// pair, percent ranges) exactly as for pad nudges, so both origins
    /// commit identically.
    fn slider_select(self, snapshot: &StartSettingsSnapshot, value: f32) -> StartMessage {
        match self {
            Self::IdleSecs => {
                let idx = (value.round() as usize)
                    .min(START_SCREEN_IDLE_SECS_STEPS.len().saturating_sub(1));
                StartMessage::SetInactiveSecs(START_SCREEN_IDLE_SECS_STEPS[idx])
            }
            Self::SleepSecs => {
                let choices = sleep_choices_for(snapshot.inactive_secs);
                let idx = (value.round() as usize).min(choices.len().saturating_sub(1));
                StartMessage::SetSleepSecs(choices[idx])
            }
            Self::IdleDim => StartMessage::SetInactiveDimPercent(value.round() as u8),
            Self::SoundVolume => StartMessage::SetSoundVolume(value.round() as u8),
            Self::HapticsStrength => StartMessage::SetHapticsStrength(value.round() as u8),
            row => unreachable!("{row:?} is not a slider row"),
        }
    }
}

/// Widget kind driving generic confirm/nudge dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Toggle,
    Selector,
    Slider,
}

#[derive(Debug, Clone)]
pub struct SettingsAnim {
    from: f32,
    to: f32,
    duration_ms: u64,
    started: Instant,
}

/// Open/close + focus state for the Options settings surface.
#[derive(Debug, Clone, Default)]
pub struct SettingsPanel {
    pub open: bool,
    pub focus: usize,
    pub anim: Option<SettingsAnim>,
    pub scroll_y: f32,
    pub viewport_h: f32,
    /// Expanded selector dropdown: (row, highlighted option index).
    /// `None` = closed (headers show current values only). Only one dropdown
    /// is ever expanded.
    pub menu: Option<(SettingsRow, usize)>,
}

impl SettingsPanel {
    /// True while a dropdown selector menu is expanded.
    pub fn any_menu_open(&self) -> bool {
        self.menu.is_some()
    }

    /// Which selector row owns the open dropdown, if any.
    pub fn menu_row(&self) -> Option<SettingsRow> {
        self.menu.map(|(row, _)| row)
    }

    /// Collapse the dropdown selector menu.
    pub fn close_menus(&mut self) {
        self.menu = None;
    }

    /// Toggle the dropdown for a selector row. Opening inits the highlight
    /// to the current value and closes any other menu (single menu);
    /// toggling the open row closes it. Returns true when open afterwards.
    pub fn toggle_menu(&mut self, row: SettingsRow, snapshot: &StartSettingsSnapshot) -> bool {
        debug_assert_eq!(row.kind(), RowKind::Selector);
        if self.menu.is_some_and(|(open, _)| open == row) {
            self.menu = None;
            false
        } else {
            self.menu = Some((row, row.initial_highlight(snapshot)));
            true
        }
    }

    /// Move the open menu highlight (caller wraps into `selector_count`).
    pub fn set_menu_highlight(&mut self, highlight: usize) {
        if let Some((_, slot)) = self.menu.as_mut() {
            *slot = highlight;
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn progress(&self, now: Instant) -> f32 {
        if let Some(anim) = &self.anim {
            let t = (now.saturating_duration_since(anim.started).as_secs_f32()
                / (anim.duration_ms.max(1) as f32 / 1000.0))
                .clamp(0.0, 1.0);
            let e = window_layout::ease_out_cubic(t);
            return anim.from + (anim.to - anim.from) * e;
        }
        if self.open { 1.0 } else { 0.0 }
    }

    pub fn visible(&self, now: Instant) -> bool {
        self.open || self.progress(now) > 0.001
    }

    pub fn needs_frames(&self, now: Instant) -> bool {
        self.anim.is_some() || (self.open && self.progress(now) < 0.999)
    }

    /// Request open/close. Returns true when an anim (or instant settle) was applied.
    ///
    /// While a close anim runs, [`Self::open`] stays true until [`Self::tick`] settles
    /// so pad routing remains in the settings branch.
    pub fn request(&mut self, open: bool, now: Instant, animate: bool) -> bool {
        let target = if open { 1.0 } else { 0.0 };
        if self
            .anim
            .as_ref()
            .is_some_and(|a| (a.to - target).abs() < 0.01)
        {
            return false;
        }
        if self.anim.is_none() && self.open == open {
            return false;
        }
        let from = self.progress(now);
        if open {
            self.open = true;
            self.focus = 0;
            self.scroll_y = 0.0;
            self.close_menus();
        } else {
            // Drop the expanded dropdown the moment the drawer starts leaving.
            self.close_menus();
        }
        if !animate || (from - target).abs() < 0.01 {
            self.open = open;
            self.anim = None;
            return true;
        }
        // Closing with anim: keep `open` until tick settles.
        if open {
            self.open = true;
        }
        self.anim = Some(SettingsAnim {
            from,
            to: target,
            duration_ms: DRAWER_ANIM_MS,
            started: now,
        });
        true
    }

    pub fn tick(&mut self, now: Instant) -> bool {
        let Some(anim) = self.anim.as_ref() else {
            return false;
        };
        if now.saturating_duration_since(anim.started) >= Duration::from_millis(anim.duration_ms) {
            self.open = anim.to >= 0.5;
            self.anim = None;
            false
        } else {
            true
        }
    }

    pub fn set_scroll(&mut self, y: f32, viewport_h: f32) {
        self.scroll_y = y;
        self.viewport_h = viewport_h;
    }

    pub fn clamp_focus(&mut self, snapshot: &StartSettingsSnapshot) {
        let n = SettingsRow::focusable(snapshot).len().max(1);
        if self.focus >= n {
            self.focus = n - 1;
        }
    }

    pub fn focused_row(&self, snapshot: &StartSettingsSnapshot) -> Option<SettingsRow> {
        SettingsRow::focusable(snapshot).get(self.focus).copied()
    }

    pub fn move_focus(&mut self, delta: isize, snapshot: &StartSettingsSnapshot) -> bool {
        let rows = SettingsRow::focusable(snapshot);
        if rows.is_empty() {
            return false;
        }
        let n = rows.len() as isize;
        let next = (self.focus as isize + delta).rem_euclid(n) as usize;
        if next == self.focus {
            return false;
        }
        self.focus = next;
        true
    }

    /// Absolute scroll Y to keep the focused row in view, if needed.
    pub fn focus_scroll_y(
        &self,
        snapshot: &StartSettingsSnapshot,
        immersive: bool,
        direction: ScrollReveal,
    ) -> Option<f32> {
        let (row_top, row_h) = focus_row_bounds(self.focus, snapshot, immersive, self.menu_row())?;
        let viewport_h = if self.viewport_h <= 1.0 {
            if immersive {
                720.0
            } else {
                COMPACT_VIEWPORT_FALLBACK
            }
        } else {
            self.viewport_h
        };
        // Near the top of the list (or first focusable): pin to 0 so section labels /
        // content pad are not clipped when wrapping or stepping Up from below.
        let pin_top = self.focus == 0
            || (matches!(direction, ScrollReveal::Up | ScrollReveal::Either)
                && row_top <= TOP_PIN_MAX_ROW_TOP);
        if pin_top {
            return (self.scroll_y > 0.5).then_some(0.0);
        }
        // Expand the reveal box so the focus ring is not flush to the clip edge.
        let reveal_top = (row_top - REVEAL_INSET).max(0.0);
        let reveal_h = (row_top + row_h + REVEAL_INSET) - reveal_top;
        scroll_y_to_reveal_bounds(reveal_top, reveal_h, self.scroll_y, viewport_h, direction)
    }
}

pub fn settings_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("start-settings-scroll")
}

/// Scroll direction after a focus step (including wrap).
pub fn reveal_for_focus_step(before: usize, after: usize, delta: isize) -> ScrollReveal {
    if delta > 0 && after < before {
        ScrollReveal::Up
    } else if delta < 0 && after > before {
        ScrollReveal::Down
    } else if delta < 0 {
        ScrollReveal::Up
    } else {
        ScrollReveal::Down
    }
}

/// Drawer panel width from closed → full ([`DRAWER_W`]).
pub fn drawer_width(progress: f32) -> f32 {
    progress.clamp(0.0, 1.0) * DRAWER_W
}

/// Dim alpha over chrome while the settings surface opens.
pub fn scrim_dim(progress: f32) -> f32 {
    progress.clamp(0.0, 1.0) * 0.55
}

/// Confirm on the focused row (toggle / activate button).
pub fn confirm_message(row: SettingsRow, snapshot: &StartSettingsSnapshot) -> Option<StartMessage> {
    match row.kind() {
        // Selectors open the dropdown; picking happens from the menu highlight.
        RowKind::Selector => Some(row.open_menu()),
        RowKind::Toggle => Some(row.set_toggle(!row.toggle_value(snapshot))),
        // Sliders step with Left/Right; Cross does nothing.
        RowKind::Slider => None,
    }
}

/// Left/Right on the focused row (`delta` −1 = left, +1 = right).
///
/// Sliders step; toggles set off on left and on on right (no-op if already there).
/// Auto open and Layout quick-cycle without opening the menu
/// (Confirm opens the menu for direct picking instead). With a menu open,
/// directionals move the highlight and Confirm picks / Cancel closes.
pub fn nudge_message(
    row: SettingsRow,
    snapshot: &StartSettingsSnapshot,
    delta: i8,
) -> Option<StartMessage> {
    match row.kind() {
        // Quick-cycle without opening the menu (Confirm opens it instead).
        RowKind::Selector => Some(row.cycle_selector(snapshot, delta)),
        RowKind::Toggle => {
            let want_on = delta > 0;
            if row.toggle_value(snapshot) == want_on {
                None
            } else {
                Some(row.set_toggle(want_on))
            }
        }
        RowKind::Slider => {
            let step = if delta < 0 { -1isize } else { 1isize };
            row.nudge_slider(snapshot, step)
        }
    }
}

/// Step `cur` by 5 within 0..=100; `None` when clamped back onto `cur` (at a limit).
fn step_percent(cur: u8, step: isize) -> Option<u8> {
    let next = (i16::from(cur) + step as i16 * 5).clamp(0, 100) as u8;
    (next != cur).then_some(next)
}

/// Sleep slider choices for an idle value: sleep steps at/above the idle
/// minimum, falling back to a single top step when the filter empties.
/// Shared by the panel position math and both commit mappings (pad nudge,
/// mouse drag) so position and committed value can't drift apart.
fn sleep_choices_for(inactive_secs: u32) -> Vec<u32> {
    let min_sleep = min_sleep_secs_for_idle(inactive_secs);
    let choices: Vec<u32> = START_SCREEN_SLEEP_SECS_STEPS
        .iter()
        .copied()
        .filter(|&s| s >= min_sleep)
        .collect();
    if choices.is_empty() {
        vec![*START_SCREEN_SLEEP_SECS_STEPS.last().unwrap_or(&min_sleep)]
    } else {
        choices
    }
}

/// Step `value` by `step` positions within `table`; `None` when clamped back
/// onto `value` (at a limit) or when the table is empty.
fn step_in_table(value: u32, table: &[u32], step: isize) -> Option<u32> {
    if table.is_empty() {
        return None;
    }
    let idx = secs_step_index(value, table) as isize;
    let next = table[(idx + step).clamp(0, table.len() as isize - 1) as usize];
    (next != value).then_some(next)
}

#[derive(Clone, Copy)]
struct TypeScale {
    title: f32,
    section: f32,
    body: f32,
    helper: f32,
    toggle: f32,
}

impl TypeScale {
    fn for_mode(immersive: bool) -> Self {
        if immersive {
            Self {
                title: 26.0,
                section: 15.0,
                body: 16.0,
                helper: 14.0,
                toggle: 22.0,
            }
        } else {
            Self {
                title: 18.0,
                section: 12.0,
                body: 13.0,
                helper: 12.0,
                toggle: 16.0,
            }
        }
    }
}

/// Estimated `(top, height)` of the focused row inside the scrollable content.
///
/// The open selector dropdown (`menu`) expands its panel in place, shifting
/// every row below it down (panel padding + one option row per mode).
pub fn focus_row_bounds(
    focus: usize,
    snapshot: &StartSettingsSnapshot,
    immersive: bool,
    menu: Option<SettingsRow>,
) -> Option<(f32, f32)> {
    let rows = SettingsRow::focusable(snapshot);
    let target = *rows.get(focus)?;
    debug_assert!(target.visible(snapshot));
    let ty = TypeScale::for_mode(immersive);
    // Content padding is inside the scrollable — offsets are absolute in that content.
    // Title lives in the static header (not scrolled).
    // Stacking mirrors `panel_view`: one COL_GAP between every child, heights
    // from the shared `*_row_h` fns / `menu_panel_h` / section + gap constants.
    let mut y = CONTENT_PAD_Y;
    for item in layout_items(snapshot, menu) {
        let h = match item {
            LayoutItem::Section(_) => ty.section,
            LayoutItem::Gap => SECTION_GAP_H,
            LayoutItem::Row(row) => row.height(ty),
            LayoutItem::Menu(row) => menu_panel_h(row.selector_count(), ty.body),
        };
        let top = y;
        y += h + COL_GAP;
        if item == LayoutItem::Row(target) {
            return Some((top, h));
        }
    }
    None
}

fn toggle_row_h(toggle: f32) -> f32 {
    ROW_PAD * 2.0 + toggle.max(22.0)
}

/// Fixed height of the cycle-row option viewport: one option visible.
fn cycle_viewport_h(helper: f32) -> f32 {
    helper * 1.5
}

fn cycle_row_h(helper: f32, toggle: f32) -> f32 {
    ROW_PAD * 2.0 + cycle_viewport_h(helper).max(toggle).max(22.0)
}

/// Fixed height of a dropdown selector header row (same clipped single-option
/// viewport as [`cycle_row_h`]).
fn selector_row_h(helper: f32, toggle: f32) -> f32 {
    cycle_row_h(helper, toggle)
}

/// Estimated height of one expanded dropdown option row.
fn menu_option_h(body: f32) -> f32 {
    ROW_PAD * 2.0 + (body * 1.25).max(22.0)
}

/// Estimated height of an expanded dropdown panel: padding top/bottom + one
/// option row per mode with spacing between (matches `panel_view` stacking:
/// `selector_menu_panel` uses the same [`MENU_PANEL_PAD`] and
/// [`MENU_OPTION_GAP`]).
fn menu_panel_h(option_count: usize, body: f32) -> f32 {
    let mut panel_h = MENU_PANEL_PAD * 2.0;
    for i in 0..option_count {
        if i > 0 {
            panel_h += MENU_OPTION_GAP;
        }
        panel_h += menu_option_h(body);
    }
    panel_h
}

fn slider_row_h(label: f32) -> f32 {
    // Label + gap + slider track (iced default is a bit taller than a tight 22px guess).
    ROW_PAD * 2.0 + label + SLIDER_INNER_GAP + 26.0
}

/// Expanded dropdown panel height below `row` when it owns the open menu.
/// Generic over the selector option list so bounds and rendering share it.
fn menu_panel_h_for(row: SettingsRow, menu: Option<SettingsRow>, body: f32) -> Option<f32> {
    if menu == Some(row) && row.kind() == RowKind::Selector {
        Some(menu_panel_h(row.selector_count(), body))
    } else {
        None
    }
}

/// One stacked entry in the scrollable settings list. `Row` entries are the
/// focusable rows from [`SettingsRow::ALL`]; `Section`/`Gap` are the static
/// separators anchored to the row that follows them; `Menu` is the expanded
/// dropdown panel rendered immediately after its selector row, wherever that
/// row sits in the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutItem {
    Section(&'static str),
    Gap,
    Row(SettingsRow),
    Menu(SettingsRow),
}

/// Full stacking order of the scrollable list, derived from the single row
/// table: visible rows in [`SettingsRow::ALL`] order, each with its anchored
/// section/gap prefix and its trailing dropdown panel (when that selector
/// owns the open menu). `panel_view` and `focus_row_bounds` both consume this
/// shape (widgets vs. heights) so the three orderings agree by construction.
fn layout_items(snapshot: &StartSettingsSnapshot, menu: Option<SettingsRow>) -> Vec<LayoutItem> {
    let mut items = Vec::new();
    for row in SettingsRow::ALL
        .iter()
        .copied()
        .filter(|row| row.visible(snapshot))
    {
        if row.gap_before() {
            items.push(LayoutItem::Gap);
        }
        if let Some(section) = row.section_before() {
            items.push(LayoutItem::Section(section));
        }
        items.push(LayoutItem::Row(row));
        // Dropdown panel renders after its selector row wherever it sits;
        // the shift for every row below derives from this table position.
        if menu_panel_h_for(row, menu, 0.0).is_some() {
            items.push(LayoutItem::Menu(row));
        }
    }
    debug_assert_eq!(
        items
            .iter()
            .filter_map(|item| match item {
                LayoutItem::Row(row) => Some(*row),
                _ => None,
            })
            .collect::<Vec<_>>(),
        SettingsRow::focusable(snapshot),
        "layout rows must match focusable order"
    );
    items
}

pub fn panel_view<'a>(
    state: &'a State,
    snapshot: &StartSettingsSnapshot,
    immersive: bool,
) -> Element<'a, StartMessage> {
    let focus_rows = SettingsRow::focusable(snapshot);
    let focus = state.settings.focus.min(focus_rows.len().saturating_sub(1));
    let ty = TypeScale::for_mode(immersive);

    let header = column![
        text("Settings")
            .size(ty.title)
            .font(Font {
                weight: Weight::Bold,
                ..Font::DEFAULT
            })
            .color(theme::INK),
    ]
    .spacing(10)
    .width(Fill);

    let mut items = column![].spacing(COL_GAP).width(Fill);
    // Build order derives from the single row table: visible rows in ALL
    // order, each with its anchored gap/section prefix and its trailing
    // dropdown panel (when that selector owns the open menu). Exhaustive by
    // construction: a new SettingsRow variant fails to compile here until its
    // widget is declared. Messages route through the same row constructors
    // as the pad paths — `open_menu` is what confirm yields, `set_toggle`
    // is the confirm flip, `selector_select` is the menu-nav commit, and
    // `slider_select` is the absolute-value nudge equivalent — so mouse
    // and pad commits can't drift apart.
    let idle_secs = snapshot.inactive_secs;
    let idle_idx = secs_step_index(idle_secs, START_SCREEN_IDLE_SECS_STEPS);
    let idle_max = START_SCREEN_IDLE_SECS_STEPS.len().saturating_sub(1) as f32;
    let sleep_secs = snapshot.sleep_secs;
    let sleep_choices = sleep_choices_for(idle_secs);
    let sleep_idx = secs_step_index(sleep_secs, &sleep_choices);
    let sleep_max = sleep_choices.len().saturating_sub(1) as f32;
    #[cfg(debug_assertions)]
    let mut built_rows: Vec<SettingsRow> = Vec::new();
    for row in SettingsRow::ALL
        .iter()
        .copied()
        .filter(|row| row.visible(snapshot))
    {
        if row.gap_before() {
            items = items.push(space_gap());
        }
        if let Some(section) = row.section_before() {
            items = items.push(section_label(section, ty.section));
        }
        let focused = focus_rows.get(focus) == Some(&row);
        items = items.push(match row {
            SettingsRow::AutoOpen => selector_row(
                "Auto open:",
                snapshot.auto_open.label(),
                focused,
                ty,
                SettingsRow::AutoOpen.open_menu(),
            ),
            SettingsRow::AlwaysImmersive => toggle_row(
                "Always immersive",
                SettingsRow::AlwaysImmersive.toggle_value(snapshot),
                focused,
                ty,
                |on| SettingsRow::AlwaysImmersive.set_toggle(on),
            ),
            SettingsRow::Clock => toggle_row(
                "Display clock",
                SettingsRow::Clock.toggle_value(snapshot),
                focused,
                ty,
                |on| SettingsRow::Clock.set_toggle(on),
            ),
            SettingsRow::ImmersiveLayout => selector_row(
                "Layout",
                snapshot.immersive_layout.label(),
                focused,
                ty,
                SettingsRow::ImmersiveLayout.open_menu(),
            ),
            SettingsRow::IdleSecs => {
                let snap_for_msg = snapshot.clone();
                slider_row(
                    format!("Idle after {}", format_timeout_duration(idle_secs)),
                    focused,
                    ty,
                    0.0..=idle_max,
                    idle_idx as f32,
                    move |value| SettingsRow::IdleSecs.slider_select(&snap_for_msg, value),
                )
            }
            SettingsRow::SleepSecs => {
                let snap_for_msg = snapshot.clone();
                slider_row(
                    format!("Sleep after {}", format_timeout_duration(sleep_secs)),
                    focused,
                    ty,
                    0.0..=sleep_max,
                    sleep_idx as f32,
                    move |value| SettingsRow::SleepSecs.slider_select(&snap_for_msg, value),
                )
            }
            SettingsRow::IdleDim => {
                let dim = snapshot.inactive_dim_percent;
                let snap_for_msg = snapshot.clone();
                slider_row(
                    format!("Idle dim {dim}%"),
                    focused,
                    ty,
                    0.0..=100.0,
                    f32::from(dim),
                    move |value| SettingsRow::IdleDim.slider_select(&snap_for_msg, value),
                )
            }
            SettingsRow::Sounds => toggle_row(
                "UI sounds",
                SettingsRow::Sounds.toggle_value(snapshot),
                focused,
                ty,
                |on| SettingsRow::Sounds.set_toggle(on),
            ),
            SettingsRow::SoundVolume => {
                let volume = snapshot.sound_volume;
                let snap_for_msg = snapshot.clone();
                slider_row(
                    format!("Volume {volume}%"),
                    focused,
                    ty,
                    0.0..=100.0,
                    f32::from(volume),
                    move |value| SettingsRow::SoundVolume.slider_select(&snap_for_msg, value),
                )
            }
            SettingsRow::Haptics => toggle_row(
                "Controller haptics",
                SettingsRow::Haptics.toggle_value(snapshot),
                focused,
                ty,
                |on| SettingsRow::Haptics.set_toggle(on),
            ),
            SettingsRow::HapticsStrength => {
                let strength = snapshot.haptics_strength;
                let snap_for_msg = snapshot.clone();
                slider_row(
                    format!("Strength {strength}%"),
                    focused,
                    ty,
                    0.0..=100.0,
                    f32::from(strength),
                    move |value| SettingsRow::HapticsStrength.slider_select(&snap_for_msg, value),
                )
            }
        });
        #[cfg(debug_assertions)]
        built_rows.push(row);
        // Expanded dropdown panel: every option selectable, highlight follows
        // the menu. Renders after its selector row wherever it sits, so rows
        // below shift from this table position (not a hardcoded sequence).
        if let Some(panel) = selector_menu_panel(state.settings.menu, row, snapshot, ty) {
            items = items.push(panel);
        }
    }
    #[cfg(debug_assertions)]
    debug_assert_eq!(
        built_rows, focus_rows,
        "panel build order must match focusable order"
    );

    let list = scrollable(items.padding(Padding {
        top: CONTENT_PAD_Y,
        right: CONTENT_PAD_X,
        bottom: CONTENT_PAD_Y,
        left: CONTENT_PAD_X,
    }))
    .id(settings_scroll_id())
    // Embed scrollbar with a gutter so toggles/sliders are not under the thumb.
    .spacing(SCROLL_GAP)
    .on_scroll(|viewport| {
        let y = viewport.absolute_offset().y;
        StartMessage::SettingsScrolled(y, viewport.bounds().height)
    })
    .width(Fill)
    .height(Fill);

    let pad = if immersive { 20.0 } else { 16.0 };
    container(
        column![header, list]
            .spacing(HEADER_LIST_GAP)
            .width(Fill)
            .height(Fill),
    )
    .padding(pad)
    .width(Fill)
    .height(Fill)
    .into()
}

/// Compact: dim scrim + centered modal over full chrome.
pub fn compact_overlay<'a>(
    state: &'a State,
    snapshot: &StartSettingsSnapshot,
    progress: f32,
) -> Element<'a, StartMessage> {
    let dim = scrim_dim(progress);
    let card = container(
        container(theme::framed(panel_view(state, snapshot, false)))
            .width(Length::Fixed(MODAL_W))
            .max_height(COMPACT_MODAL_MAX_H),
    )
    .width(Fill)
    .height(Fill)
    .center_x(Fill)
    .center_y(Fill);

    stack_scrim(dim, card)
}

/// Immersive: full-height right drawer over chrome (square corners).
pub fn immersive_drawer<'a>(
    state: &'a State,
    snapshot: &StartSettingsSnapshot,
    progress: f32,
) -> Element<'a, StartMessage> {
    let dim = scrim_dim(progress);
    let w = drawer_width(progress);
    let panel = container(panel_view(state, snapshot, true))
        .width(Length::Fixed(DRAWER_W))
        .height(Fill)
        .style(theme::immersive_island_radius(0.0));

    let revealed = crate::ui::reveal::width_reveal(panel, DRAWER_W, w).height(Fill);
    let drawer = row![space().width(Fill), revealed].width(Fill).height(Fill);

    stack_scrim(dim, drawer)
}

fn stack_scrim<'a>(
    dim: f32,
    content: impl Into<Element<'a, StartMessage>>,
) -> Element<'a, StartMessage> {
    use iced::widget::stack;
    let mut layers: Vec<Element<'_, StartMessage>> = Vec::new();
    if dim > 0.001 {
        layers.push(
            container(space())
                .width(Fill)
                .height(Fill)
                .style(theme::immersive_dim(dim))
                .into(),
        );
    }
    layers.push(content.into());
    stack(layers).width(Fill).height(Fill).into()
}

fn space_gap() -> Element<'static, StartMessage> {
    space()
        .height(Length::Fixed(SECTION_GAP_H))
        .width(Fill)
        .into()
}

fn section_label(label: &'static str, size: f32) -> Element<'static, StartMessage> {
    text(label)
        .size(size)
        .font(Font {
            weight: Weight::Semibold,
            ..Font::DEFAULT
        })
        .color(theme::MUTED)
        .into()
}

fn focus_style(focused: bool) -> impl Fn(&iced::Theme) -> container::Style {
    move |_theme| {
        if focused {
            container::Style {
                background: Some(iced::Background::Color(theme::alpha(theme::ACCENT, 0.12))),
                border: iced::Border {
                    color: theme::alpha(theme::ACCENT, 0.55),
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

fn toggle_row(
    label: &'static str,
    checked: bool,
    focused: bool,
    ty: TypeScale,
    message: fn(bool) -> StartMessage,
) -> Element<'static, StartMessage> {
    container(
        row![
            text(label).size(ty.body).color(theme::INK).width(Fill),
            toggler(checked)
                .size(ty.toggle)
                .on_toggle(message)
                .style(theme::toggle),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding(ROW_PAD)
    .width(Fill)
    .style(focus_style(focused))
    .into()
}

/// Dropdown selector header: label left, current option in a fixed-height
/// clipped viewport right so only the selected option displays.
/// Click (or pad Cross) toggles the menu; `toggle` opens/closes it.
fn selector_row(
    label: &'static str,
    value_label: &'static str,
    focused: bool,
    ty: TypeScale,
    toggle: StartMessage,
) -> Element<'static, StartMessage> {
    // Fixed-height clipped viewport: only the selected option displays.
    let viewport_h = cycle_viewport_h(ty.helper);
    mouse_area(
        container(
            row![
                text(label).size(ty.body).color(theme::INK),
                container(
                    container(
                        text(value_label)
                            .size(ty.helper)
                            .color(theme::INK)
                            .wrapping(Wrapping::None),
                    )
                    .width(Fill)
                    .align_x(Alignment::End),
                )
                .width(Fill)
                .height(Length::Fixed(viewport_h))
                .align_y(Alignment::Center)
                .clip(true),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        )
        .padding(ROW_PAD)
        .width(Fill)
        .style(focus_style(focused)),
    )
    .on_press(toggle)
    .interaction(iced::mouse::Interaction::Pointer)
    .into()
}

/// Expanded dropdown panel for a selector row: one option row per mode, check
/// mark tracking the committed value and focus ring tracking the menu
/// highlight. `None` when `menu` is closed or owned by another row.
fn selector_menu_panel(
    menu: Option<(SettingsRow, usize)>,
    row: SettingsRow,
    snapshot: &StartSettingsSnapshot,
    ty: TypeScale,
) -> Option<Element<'static, StartMessage>> {
    let highlight = match menu {
        Some((open, h)) if open == row => h,
        _ => return None,
    };
    let mut options = column![].spacing(MENU_OPTION_GAP).width(Fill);
    for i in 0..row.selector_count() {
        options = options.push(option_row(
            row.selector_label(i),
            row.selector_selected(i, snapshot),
            i == highlight,
            ty,
            row.selector_select(i),
        ));
    }
    Some(
        container(options)
            .padding(MENU_PANEL_PAD)
            .width(Fill)
            .style(theme::modal_card)
            .into(),
    )
}

/// One expanded dropdown option: check mark tracks the committed value,
/// focus ring tracks the menu highlight. Click selects immediately.
fn option_row(
    label: &'static str,
    selected: bool,
    highlighted: bool,
    ty: TypeScale,
    select: StartMessage,
) -> Element<'static, StartMessage> {
    let (mark, mark_color) = if selected {
        ("✓", theme::ACCENT)
    } else {
        ("○", theme::alpha(theme::MUTED, 0.55))
    };
    mouse_area(
        container(
            row![
                text(mark)
                    .size(ty.body)
                    .color(mark_color)
                    .width(Length::Fixed(22.0)),
                text(label).size(ty.body).color(theme::INK).width(Fill),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding(ROW_PAD)
        .width(Fill)
        .style(focus_style(highlighted)),
    )
    .on_press(select)
    .interaction(iced::mouse::Interaction::Pointer)
    .into()
}

fn slider_row(
    label: String,
    focused: bool,
    ty: TypeScale,
    range: std::ops::RangeInclusive<f32>,
    value: f32,
    on_change: impl Fn(f32) -> StartMessage + 'static,
) -> Element<'static, StartMessage> {
    let step = if (*range.end() - *range.start()) <= 20.0 {
        1.0_f32
    } else {
        5.0_f32
    };
    container(
        column![
            text(label).size(ty.helper).color(theme::MUTED),
            slider(range, value, on_change)
                .step(step)
                .style(theme::slider),
        ]
        .spacing(SLIDER_INNER_GAP)
        .width(Fill),
    )
    .padding(ROW_PAD)
    .width(Fill)
    .style(focus_style(focused))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(sounds: bool, haptics: bool) -> StartSettingsSnapshot {
        StartSettingsSnapshot {
            auto_open: StartAutoOpen::Any,
            always_immersive: false,
            clock_enabled: true,
            immersive_layout: ImmersiveLayout::Vertical,
            inactive_secs: 30,
            sleep_secs: 120,
            inactive_dim_percent: 50,
            sounds_enabled: sounds,
            sound_volume: 60,
            haptics_enabled: haptics,
            haptics_strength: 60,
        }
    }

    #[test]
    fn focusable_omits_volume_when_sounds_off() {
        let rows = SettingsRow::focusable(&snap(false, true));
        assert!(!rows.contains(&SettingsRow::SoundVolume));
        assert!(rows.contains(&SettingsRow::HapticsStrength));
    }

    #[test]
    fn focusable_lists_selectors_in_order() {
        let rows = SettingsRow::focusable(&snap(true, true));
        assert_eq!(
            rows,
            vec![
                SettingsRow::AutoOpen,
                SettingsRow::AlwaysImmersive,
                SettingsRow::Clock,
                SettingsRow::ImmersiveLayout,
                SettingsRow::IdleSecs,
                SettingsRow::SleepSecs,
                SettingsRow::IdleDim,
                SettingsRow::Sounds,
                SettingsRow::SoundVolume,
                SettingsRow::Haptics,
                SettingsRow::HapticsStrength,
            ]
        );
    }

    #[test]
    fn auto_open_confirm_opens_menu_nudge_quick_cycles() {
        let s = snap(true, true);
        // Confirm (Cross) expands the dropdown instead of changing the value.
        let msg = confirm_message(SettingsRow::AutoOpen, &s).unwrap();
        assert!(matches!(msg, StartMessage::ToggleAutoOpenMenu));
        // Left/Right still quick-cycle without opening the menu.
        // Snapshot starts at Any: right wraps to Never, left steps to Bluetooth.
        let msg = nudge_message(SettingsRow::AutoOpen, &s, 1).unwrap();
        assert!(matches!(
            msg,
            StartMessage::SetAutoOpen(StartAutoOpen::Never)
        ));
        let msg = nudge_message(SettingsRow::AutoOpen, &s, -1).unwrap();
        assert!(matches!(
            msg,
            StartMessage::SetAutoOpen(StartAutoOpen::Bluetooth)
        ));
    }

    #[test]
    fn layout_confirm_opens_menu_nudge_quick_cycles() {
        let s = snap(true, true);
        // Confirm (Cross) expands the dropdown instead of changing the value.
        let msg = confirm_message(SettingsRow::ImmersiveLayout, &s).unwrap();
        assert!(matches!(msg, StartMessage::ToggleLayoutMenu));
        // Left/Right quick-cycle without opening the menu (only two modes,
        // so either direction flips Vertical -> Horizontal).
        for delta in [1, -1] {
            let msg = nudge_message(SettingsRow::ImmersiveLayout, &s, delta).unwrap();
            assert!(matches!(
                msg,
                StartMessage::SetImmersiveLayout(ImmersiveLayout::Horizontal)
            ));
        }
        let mut h = s.clone();
        h.immersive_layout = ImmersiveLayout::Horizontal;
        for delta in [1, -1] {
            let msg = nudge_message(SettingsRow::ImmersiveLayout, &h, delta).unwrap();
            assert!(matches!(
                msg,
                StartMessage::SetImmersiveLayout(ImmersiveLayout::Vertical)
            ));
        }
    }

    #[test]
    fn selector_menus_open_exclusively() {
        let s = snap(true, true);
        let mut panel = SettingsPanel::default();
        assert!(!panel.any_menu_open());
        // Opening inits the highlight to the current value.
        assert!(panel.toggle_menu(SettingsRow::AutoOpen, &s));
        assert!(panel.any_menu_open());
        assert_eq!(panel.menu_row(), Some(SettingsRow::AutoOpen));
        assert_eq!(
            panel.menu,
            Some((
                SettingsRow::AutoOpen,
                SettingsRow::AutoOpen.initial_highlight(&s)
            ))
        );
        // Opening the second menu closes the first: both can never be open.
        assert!(panel.toggle_menu(SettingsRow::ImmersiveLayout, &s));
        assert!(panel.any_menu_open());
        assert_eq!(panel.menu_row(), Some(SettingsRow::ImmersiveLayout));
        // Toggling the open row closes it.
        assert!(!panel.toggle_menu(SettingsRow::ImmersiveLayout, &s));
        assert!(!panel.any_menu_open());
        panel.close_menus();
        assert!(!panel.any_menu_open());
    }

    #[test]
    fn selector_highlight_wraps_generically() {
        let s = snap(true, true);
        for row in [SettingsRow::AutoOpen, SettingsRow::ImmersiveLayout] {
            let count = row.selector_count() as isize;
            assert!(count >= 2);
            // Wrap-around in both directions from the ends.
            assert_eq!((0isize - 1).rem_euclid(count), count - 1);
            assert_eq!((count - 1 + 1).rem_euclid(count), 0);
            // Highlight init tracks the committed value.
            assert!(row.selector_selected(row.initial_highlight(&s), &s));
        }
        // Every committed value round-trips through init + selected + select.
        for mode in AUTO_OPEN_MODES {
            let mut v = snap(true, true);
            v.auto_open = mode;
            let row = SettingsRow::AutoOpen;
            let h = row.initial_highlight(&v);
            assert!(row.selector_selected(h, &v));
            assert!(matches!(row.selector_select(h), StartMessage::SetAutoOpen(m) if m == mode));
        }
        for layout in IMMERSIVE_LAYOUT_MODES {
            let mut v = snap(true, true);
            v.immersive_layout = layout;
            let row = SettingsRow::ImmersiveLayout;
            let h = row.initial_highlight(&v);
            assert!(row.selector_selected(h, &v));
            assert!(
                matches!(row.selector_select(h), StartMessage::SetImmersiveLayout(l) if l == layout)
            );
        }
    }

    #[test]
    fn nudge_idle_steps() {
        let s = snap(true, true);
        let msg = nudge_message(SettingsRow::IdleSecs, &s, 1).unwrap();
        assert!(matches!(msg, StartMessage::SetInactiveSecs(60)));
        let msg = nudge_message(SettingsRow::IdleDim, &s, -1).unwrap();
        assert!(matches!(msg, StartMessage::SetInactiveDimPercent(45)));
    }

    #[test]
    fn nudge_toggles_left_off_right_on() {
        let s = snap(true, true);
        assert!(nudge_message(SettingsRow::Clock, &s, 1).is_none());
        assert!(nudge_message(SettingsRow::AlwaysImmersive, &s, -1).is_none());
        let msg = nudge_message(SettingsRow::AlwaysImmersive, &s, 1).unwrap();
        assert!(matches!(msg, StartMessage::SetAlwaysImmersive(true)));
        let msg = nudge_message(SettingsRow::Clock, &s, -1).unwrap();
        assert!(matches!(msg, StartMessage::SetClock(false)));
        let msg = confirm_message(SettingsRow::Clock, &s).unwrap();
        assert!(matches!(msg, StartMessage::SetClock(false)));
    }

    #[test]
    fn drawer_width_and_scrim() {
        assert!((drawer_width(0.0) - 0.0).abs() < f32::EPSILON);
        assert!((drawer_width(1.0) - DRAWER_W).abs() < f32::EPSILON);
        assert!((scrim_dim(1.0) - 0.55).abs() < f32::EPSILON);
    }

    #[test]
    fn idle_sleep_clamp_via_nudge_helpers() {
        let (inactive, sleep) = crate::persist::prefs::clamp_start_screen_idle_timeouts(120, 60);
        assert_eq!((inactive, sleep), (120, 180));
        let _ = crate::persist::prefs::clamp_start_screen_inactive_dim_percent(255);
    }

    #[test]
    fn focus_row_bounds_increases_with_focus() {
        let s = snap(true, true);
        let (y0, _) = focus_row_bounds(0, &s, false, None).unwrap();
        let last = SettingsRow::focusable(&s).len() - 1;
        let (y_last, _) = focus_row_bounds(last, &s, false, None).unwrap();
        assert!(y_last > y0);
        assert!(y0 >= CONTENT_PAD_Y);
    }

    #[test]
    fn focus_row_bounds_grows_with_expanded_auto_open_menu() {
        let s = snap(true, true);
        // AutoOpen is focus 0; with the menu open everything below shifts
        // down by one option row per mode.
        let (closed_top, _) = focus_row_bounds(0, &s, false, None).unwrap();
        let (open_top, _) = focus_row_bounds(0, &s, false, Some(SettingsRow::AutoOpen)).unwrap();
        assert!((closed_top - open_top).abs() < f32::EPSILON);
        let always = SettingsRow::focusable(&s)
            .iter()
            .position(|&r| r == SettingsRow::AlwaysImmersive)
            .unwrap();
        let (closed_y, _) = focus_row_bounds(always, &s, false, None).unwrap();
        let (open_y, _) = focus_row_bounds(always, &s, false, Some(SettingsRow::AutoOpen)).unwrap();
        // Expanded panel: padding top/bottom + one option row per mode with
        // spacing between, slotted into the column stacking.
        let expected =
            menu_panel_h(AUTO_OPEN_MODES.len(), TypeScale::for_mode(false).body) + COL_GAP;
        assert!((open_y - closed_y - expected).abs() < 0.5);
    }

    #[test]
    fn focus_row_bounds_grows_with_expanded_layout_menu() {
        let s = snap(true, true);
        // Layout menu expands below the ImmersiveLayout row: rows above it
        // (e.g. AutoOpen) do not move, rows below it (e.g. IdleSecs) shift.
        let (closed_top, _) = focus_row_bounds(0, &s, false, None).unwrap();
        let (open_top, _) =
            focus_row_bounds(0, &s, false, Some(SettingsRow::ImmersiveLayout)).unwrap();
        assert!((closed_top - open_top).abs() < f32::EPSILON);
        let layout = SettingsRow::focusable(&s)
            .iter()
            .position(|&r| r == SettingsRow::ImmersiveLayout)
            .unwrap();
        let (closed_layout, _) = focus_row_bounds(layout, &s, false, None).unwrap();
        let (open_layout, _) =
            focus_row_bounds(layout, &s, false, Some(SettingsRow::ImmersiveLayout)).unwrap();
        assert!((closed_layout - open_layout).abs() < f32::EPSILON);
        let idle = SettingsRow::focusable(&s)
            .iter()
            .position(|&r| r == SettingsRow::IdleSecs)
            .unwrap();
        let (closed_y, _) = focus_row_bounds(idle, &s, false, None).unwrap();
        let (open_y, _) =
            focus_row_bounds(idle, &s, false, Some(SettingsRow::ImmersiveLayout)).unwrap();
        let expected = menu_panel_h(
            IMMERSIVE_LAYOUT_MODES.len(),
            TypeScale::for_mode(false).body,
        ) + COL_GAP;
        assert!((open_y - closed_y - expected).abs() < 0.5);
    }

    /// Triple agreement: focusable order == layout (panel build) order ==
    /// bounds geometry order, for every sounds/haptics visibility combo and
    /// every menu state. Every focusable row must resolve `Some` bounds, with
    /// strictly increasing tops matching focus order.
    #[test]
    fn triple_agreement_focusable_panel_bounds() {
        // The single table lists every variant exactly once, in display order.
        assert_eq!(SettingsRow::ALL.len(), 11);
        {
            let mut sorted = SettingsRow::ALL;
            sorted.sort_by_key(|row| *row as usize);
            for (i, row) in sorted.iter().enumerate() {
                assert_eq!(*row as usize, i);
            }
        }
        for sounds in [false, true] {
            for haptics in [false, true] {
                let s = snap(sounds, haptics);
                // Unified visibility predicate: exactly the conditional rows gate.
                assert_eq!(
                    SettingsRow::SoundVolume.visible(&s),
                    sounds,
                    "sounds={sounds} haptics={haptics}"
                );
                assert_eq!(
                    SettingsRow::HapticsStrength.visible(&s),
                    haptics,
                    "sounds={sounds} haptics={haptics}"
                );
                let focusable = SettingsRow::focusable(&s);
                assert_eq!(
                    focusable,
                    SettingsRow::ALL
                        .iter()
                        .copied()
                        .filter(|row| row.visible(&s))
                        .collect::<Vec<_>>(),
                    "focusable must be the visible table projection (sounds={sounds} haptics={haptics})"
                );
                // Menu open/closed per selector: layout row order never changes
                // (the panel inserts after its selector wherever it sits).
                for menu in [
                    None,
                    Some(SettingsRow::AutoOpen),
                    Some(SettingsRow::ImmersiveLayout),
                ] {
                    let layout_rows: Vec<SettingsRow> = layout_items(&s, menu)
                        .iter()
                        .filter_map(|item| match item {
                            LayoutItem::Row(row) => Some(*row),
                            _ => None,
                        })
                        .collect();
                    assert_eq!(
                        layout_rows, focusable,
                        "layout rows must match focusable (sounds={sounds} haptics={haptics} menu={menu:?})"
                    );
                    for immersive in [false, true] {
                        let mut prev_top: Option<f32> = None;
                        for (focus, row) in focusable.iter().enumerate() {
                            let (top, h) =
                                focus_row_bounds(focus, &s, immersive, menu).unwrap_or_else(
                                    || {
                                        panic!(
                                            "every focusable row resolves bounds (row={row:?} sounds={sounds} haptics={haptics} menu={menu:?} immersive={immersive})"
                                        )
                                    },
                                );
                            assert!(top >= CONTENT_PAD_Y - f32::EPSILON);
                            assert!(h > 0.0);
                            if let Some(prev) = prev_top {
                                assert!(
                                    top > prev,
                                    "bounds tops follow focus order (row={row:?} top={top} prev={prev} menu={menu:?})"
                                );
                            }
                            prev_top = Some(top);
                            // Row height matches the shared per-kind fn.
                            let ty = TypeScale::for_mode(immersive);
                            assert!(
                                (h - row.height(ty)).abs() < f32::EPSILON,
                                "bounds height uses the shared row height (row={row:?})"
                            );
                        }
                        // Menu-shift derives from the table position: rows at or
                        // above the menu owner don't move; rows below shift by
                        // exactly panel + one column gap.
                        if let Some(menu_row) = menu {
                            let ty = TypeScale::for_mode(immersive);
                            let expected =
                                menu_panel_h(menu_row.selector_count(), ty.body) + COL_GAP;
                            let owner_idx =
                                focusable.iter().position(|&row| row == menu_row).unwrap();
                            for (focus, _) in focusable.iter().enumerate() {
                                let (closed, _) =
                                    focus_row_bounds(focus, &s, immersive, None).unwrap();
                                let (open, _) =
                                    focus_row_bounds(focus, &s, immersive, menu).unwrap();
                                if focus <= owner_idx {
                                    assert!(
                                        (open - closed).abs() < f32::EPSILON,
                                        "rows at/above the menu don't move (focus={focus} menu={menu:?})"
                                    );
                                } else {
                                    assert!(
                                        (open - closed - expected).abs() < 0.5,
                                        "rows below shift by panel+gap (focus={focus} menu={menu:?} shift={} expected={expected})",
                                        open - closed
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn focus_scroll_reveals_last_row_with_inset() {
        let s = snap(true, true);
        let mut panel = SettingsPanel {
            open: true,
            focus: SettingsRow::focusable(&s).len() - 1,
            viewport_h: 200.0,
            scroll_y: 0.0,
            ..SettingsPanel::default()
        };
        let (row_top, row_h) = focus_row_bounds(panel.focus, &s, false, None).unwrap();
        let y = panel
            .focus_scroll_y(&s, false, ScrollReveal::Down)
            .expect("last row should need a reveal from the top");
        // Pinned so row bottom + reveal inset sits on the viewport bottom.
        let expected = (row_top + row_h + REVEAL_INSET - 200.0).max(0.0);
        assert!((y - expected).abs() < 0.5, "y={y} expected={expected}");
        panel.scroll_y = y;
        assert!(
            panel
                .focus_scroll_y(&s, false, ScrollReveal::Down)
                .is_none()
        );
    }

    #[test]
    fn focus_scroll_pins_top_when_returning_to_first_row() {
        let s = snap(true, true);
        let panel = SettingsPanel {
            open: true,
            focus: 0,
            viewport_h: 200.0,
            scroll_y: 180.0,
            ..SettingsPanel::default()
        };
        let y = panel
            .focus_scroll_y(&s, false, ScrollReveal::Up)
            .expect("first row should pin scroll to top");
        assert!((y - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn reveal_for_focus_step_wrap() {
        assert!(matches!(reveal_for_focus_step(9, 0, 1), ScrollReveal::Up));
        assert!(matches!(
            reveal_for_focus_step(0, 9, -1),
            ScrollReveal::Down
        ));
    }

    /// Pinned confirm matrix: every row, toggles in both states.
    /// Locks behavior before the settings-dispatch refactor (must pass unchanged after).
    #[test]
    fn pin_confirm_matrix_all_rows() {
        let base = snap(true, true);
        // Selectors open their dropdown menu instead of changing the value.
        assert!(matches!(
            confirm_message(SettingsRow::AutoOpen, &base),
            Some(StartMessage::ToggleAutoOpenMenu)
        ));
        assert!(matches!(
            confirm_message(SettingsRow::ImmersiveLayout, &base),
            Some(StartMessage::ToggleLayoutMenu)
        ));
        // Toggles always flip, from either state.
        for on in [false, true] {
            let mut s = base.clone();
            s.always_immersive = on;
            assert!(
                matches!(
                    confirm_message(SettingsRow::AlwaysImmersive, &s),
                    Some(StartMessage::SetAlwaysImmersive(v)) if v == !on
                ),
                "AlwaysImmersive confirm from {on}"
            );
            s = base.clone();
            s.clock_enabled = on;
            assert!(
                matches!(
                    confirm_message(SettingsRow::Clock, &s),
                    Some(StartMessage::SetClock(v)) if v == !on
                ),
                "Clock confirm from {on}"
            );
            s = base.clone();
            s.sounds_enabled = on;
            assert!(
                matches!(
                    confirm_message(SettingsRow::Sounds, &s),
                    Some(StartMessage::SetSounds(v)) if v == !on
                ),
                "Sounds confirm from {on}"
            );
            s = base.clone();
            s.haptics_enabled = on;
            assert!(
                matches!(
                    confirm_message(SettingsRow::Haptics, &s),
                    Some(StartMessage::SetHaptics(v)) if v == !on
                ),
                "Haptics confirm from {on}"
            );
        }
        // Sliders never confirm.
        for row in [
            SettingsRow::IdleSecs,
            SettingsRow::SleepSecs,
            SettingsRow::IdleDim,
            SettingsRow::SoundVolume,
            SettingsRow::HapticsStrength,
        ] {
            assert!(
                confirm_message(row, &base).is_none(),
                "confirm {row:?} should be None"
            );
        }
    }

    /// Pinned nudge matrix for the four toggle rows: left = off, right = on,
    /// silent None when already at the requested state. Both states × both deltas.
    #[test]
    fn pin_nudge_toggles_both_directions() {
        let base = snap(true, true);
        for on in [false, true] {
            let mut s = base.clone();
            s.always_immersive = on;
            if on {
                assert!(nudge_message(SettingsRow::AlwaysImmersive, &s, 1).is_none());
                assert!(matches!(
                    nudge_message(SettingsRow::AlwaysImmersive, &s, -1),
                    Some(StartMessage::SetAlwaysImmersive(false))
                ));
            } else {
                assert!(matches!(
                    nudge_message(SettingsRow::AlwaysImmersive, &s, 1),
                    Some(StartMessage::SetAlwaysImmersive(true))
                ));
                assert!(nudge_message(SettingsRow::AlwaysImmersive, &s, -1).is_none());
            }
            s = base.clone();
            s.clock_enabled = on;
            if on {
                assert!(nudge_message(SettingsRow::Clock, &s, 1).is_none());
                assert!(matches!(
                    nudge_message(SettingsRow::Clock, &s, -1),
                    Some(StartMessage::SetClock(false))
                ));
            } else {
                assert!(matches!(
                    nudge_message(SettingsRow::Clock, &s, 1),
                    Some(StartMessage::SetClock(true))
                ));
                assert!(nudge_message(SettingsRow::Clock, &s, -1).is_none());
            }
            s = base.clone();
            s.sounds_enabled = on;
            if on {
                assert!(nudge_message(SettingsRow::Sounds, &s, 1).is_none());
                assert!(matches!(
                    nudge_message(SettingsRow::Sounds, &s, -1),
                    Some(StartMessage::SetSounds(false))
                ));
            } else {
                assert!(matches!(
                    nudge_message(SettingsRow::Sounds, &s, 1),
                    Some(StartMessage::SetSounds(true))
                ));
                assert!(nudge_message(SettingsRow::Sounds, &s, -1).is_none());
            }
            s = base.clone();
            s.haptics_enabled = on;
            if on {
                assert!(nudge_message(SettingsRow::Haptics, &s, 1).is_none());
                assert!(matches!(
                    nudge_message(SettingsRow::Haptics, &s, -1),
                    Some(StartMessage::SetHaptics(false))
                ));
            } else {
                assert!(matches!(
                    nudge_message(SettingsRow::Haptics, &s, 1),
                    Some(StartMessage::SetHaptics(true))
                ));
                assert!(nudge_message(SettingsRow::Haptics, &s, -1).is_none());
            }
        }
    }

    /// Pinned nudge matrix for the two selector rows, every value × both deltas.
    /// Layout ignores delta (cycle); AutoOpen respects it (next/prev).
    #[test]
    fn pin_nudge_selectors_all_values() {
        let base = snap(true, true);
        for (mode, right, left) in [
            (
                StartAutoOpen::Never,
                StartAutoOpen::Bluetooth,
                StartAutoOpen::Any,
            ),
            (
                StartAutoOpen::Bluetooth,
                StartAutoOpen::Any,
                StartAutoOpen::Never,
            ),
            (
                StartAutoOpen::Any,
                StartAutoOpen::Never,
                StartAutoOpen::Bluetooth,
            ),
        ] {
            let mut s = base.clone();
            s.auto_open = mode;
            assert!(
                matches!(
                    nudge_message(SettingsRow::AutoOpen, &s, 1),
                    Some(StartMessage::SetAutoOpen(v)) if v == right
                ),
                "AutoOpen {mode:?} right"
            );
            assert!(
                matches!(
                    nudge_message(SettingsRow::AutoOpen, &s, -1),
                    Some(StartMessage::SetAutoOpen(v)) if v == left
                ),
                "AutoOpen {mode:?} left"
            );
        }
        for (layout, cycled) in [
            (ImmersiveLayout::Vertical, ImmersiveLayout::Horizontal),
            (ImmersiveLayout::Horizontal, ImmersiveLayout::Vertical),
        ] {
            let mut s = base.clone();
            s.immersive_layout = layout;
            for delta in [1, -1] {
                assert!(
                    matches!(
                        nudge_message(SettingsRow::ImmersiveLayout, &s, delta),
                        Some(StartMessage::SetImmersiveLayout(v)) if v == cycled
                    ),
                    "Layout {layout:?} delta {delta}"
                );
            }
        }
    }

    /// Pinned nudge matrix for the five slider rows: limits stick (None),
    /// mid steps both directions.
    #[test]
    fn pin_nudge_sliders_limits_and_mid() {
        let base = snap(true, true);
        // IdleSecs steps: [5, 15, 30, 60, 120, 180, 300, 600]; base inactive = 30.
        assert!(matches!(
            nudge_message(SettingsRow::IdleSecs, &base, 1),
            Some(StartMessage::SetInactiveSecs(60))
        ));
        assert!(matches!(
            nudge_message(SettingsRow::IdleSecs, &base, -1),
            Some(StartMessage::SetInactiveSecs(15))
        ));
        let mut s = base.clone();
        s.inactive_secs = 5;
        assert!(nudge_message(SettingsRow::IdleSecs, &s, -1).is_none());
        assert!(matches!(
            nudge_message(SettingsRow::IdleSecs, &s, 1),
            Some(StartMessage::SetInactiveSecs(15))
        ));
        s.inactive_secs = 600;
        assert!(nudge_message(SettingsRow::IdleSecs, &s, 1).is_none());
        assert!(matches!(
            nudge_message(SettingsRow::IdleSecs, &s, -1),
            Some(StartMessage::SetInactiveSecs(300))
        ));
        // SleepSecs with inactive = 30: min sleep 60, full choice table.
        // Base sleep = 120.
        assert!(matches!(
            nudge_message(SettingsRow::SleepSecs, &base, 1),
            Some(StartMessage::SetSleepSecs(180))
        ));
        assert!(matches!(
            nudge_message(SettingsRow::SleepSecs, &base, -1),
            Some(StartMessage::SetSleepSecs(60))
        ));
        s = base.clone();
        s.sleep_secs = 60;
        assert!(nudge_message(SettingsRow::SleepSecs, &s, -1).is_none());
        assert!(matches!(
            nudge_message(SettingsRow::SleepSecs, &s, 1),
            Some(StartMessage::SetSleepSecs(120))
        ));
        s.sleep_secs = 3600;
        assert!(nudge_message(SettingsRow::SleepSecs, &s, 1).is_none());
        assert!(matches!(
            nudge_message(SettingsRow::SleepSecs, &s, -1),
            Some(StartMessage::SetSleepSecs(3300))
        ));
        // SleepSecs constrained by a large idle (inactive 600 -> min sleep 900).
        s = base.clone();
        s.inactive_secs = 600;
        s.sleep_secs = 900;
        assert!(nudge_message(SettingsRow::SleepSecs, &s, -1).is_none());
        assert!(matches!(
            nudge_message(SettingsRow::SleepSecs, &s, 1),
            Some(StartMessage::SetSleepSecs(1200))
        ));
        // Percent sliders step by 5: 0 / 50 / 100 × left/right.
        let mut s = base.clone();
        s.inactive_dim_percent = 0;
        assert!(nudge_message(SettingsRow::IdleDim, &s, -1).is_none());
        assert!(matches!(
            nudge_message(SettingsRow::IdleDim, &s, 1),
            Some(StartMessage::SetInactiveDimPercent(5))
        ));
        s.inactive_dim_percent = 50;
        assert!(matches!(
            nudge_message(SettingsRow::IdleDim, &s, -1),
            Some(StartMessage::SetInactiveDimPercent(45))
        ));
        assert!(matches!(
            nudge_message(SettingsRow::IdleDim, &s, 1),
            Some(StartMessage::SetInactiveDimPercent(55))
        ));
        s.inactive_dim_percent = 100;
        assert!(matches!(
            nudge_message(SettingsRow::IdleDim, &s, -1),
            Some(StartMessage::SetInactiveDimPercent(95))
        ));
        assert!(nudge_message(SettingsRow::IdleDim, &s, 1).is_none());
        s = base.clone();
        s.sound_volume = 0;
        assert!(nudge_message(SettingsRow::SoundVolume, &s, -1).is_none());
        assert!(matches!(
            nudge_message(SettingsRow::SoundVolume, &s, 1),
            Some(StartMessage::SetSoundVolume(5))
        ));
        s.sound_volume = 50;
        assert!(matches!(
            nudge_message(SettingsRow::SoundVolume, &s, -1),
            Some(StartMessage::SetSoundVolume(45))
        ));
        assert!(matches!(
            nudge_message(SettingsRow::SoundVolume, &s, 1),
            Some(StartMessage::SetSoundVolume(55))
        ));
        s.sound_volume = 100;
        assert!(matches!(
            nudge_message(SettingsRow::SoundVolume, &s, -1),
            Some(StartMessage::SetSoundVolume(95))
        ));
        assert!(nudge_message(SettingsRow::SoundVolume, &s, 1).is_none());
        s = base.clone();
        s.haptics_strength = 0;
        assert!(nudge_message(SettingsRow::HapticsStrength, &s, -1).is_none());
        assert!(matches!(
            nudge_message(SettingsRow::HapticsStrength, &s, 1),
            Some(StartMessage::SetHapticsStrength(5))
        ));
        s.haptics_strength = 50;
        assert!(matches!(
            nudge_message(SettingsRow::HapticsStrength, &s, -1),
            Some(StartMessage::SetHapticsStrength(45))
        ));
        assert!(matches!(
            nudge_message(SettingsRow::HapticsStrength, &s, 1),
            Some(StartMessage::SetHapticsStrength(55))
        ));
        s.haptics_strength = 100;
        assert!(matches!(
            nudge_message(SettingsRow::HapticsStrength, &s, -1),
            Some(StartMessage::SetHapticsStrength(95))
        ));
        assert!(nudge_message(SettingsRow::HapticsStrength, &s, 1).is_none());
    }

    /// Mouse/pad parity: selector headers and toggler writes go through the
    /// same row constructors the pad confirm path delegates to, so the two
    /// origins can't emit divergent messages.
    #[test]
    fn mouse_builders_use_confirm_constructors() {
        let s = snap(true, true);
        for row in [SettingsRow::AutoOpen, SettingsRow::ImmersiveLayout] {
            let header = row.open_menu();
            let confirm = confirm_message(row, &s).expect("selector confirm opens the menu");
            assert_eq!(
                std::mem::discriminant(&header),
                std::mem::discriminant(&confirm),
                "{row:?} header must emit the confirm-path open message"
            );
        }
        for (row, on) in [
            (SettingsRow::AlwaysImmersive, s.always_immersive),
            (SettingsRow::Clock, s.clock_enabled),
            (SettingsRow::Sounds, s.sounds_enabled),
            (SettingsRow::Haptics, s.haptics_enabled),
        ] {
            // The toggler reads the same value confirm flips...
            assert_eq!(row.toggle_value(&s), on, "{row:?} toggler read");
            // ...and writes the same message confirm yields.
            let write = row.set_toggle(!on);
            let confirm = confirm_message(row, &s).expect("toggle confirm flips");
            assert_eq!(
                std::mem::discriminant(&write),
                std::mem::discriminant(&confirm),
                "{row:?} toggler must emit the confirm-path set message"
            );
        }
    }

    /// Mouse/pad parity: the slider drag mapping (`slider_select`) commits
    /// the same values the panel positions display, at every step position.
    #[test]
    fn slider_select_matches_panel_positions() {
        let s = snap(true, true);
        for (i, &secs) in START_SCREEN_IDLE_SECS_STEPS.iter().enumerate() {
            assert!(
                matches!(
                    SettingsRow::IdleSecs.slider_select(&s, i as f32),
                    StartMessage::SetInactiveSecs(v) if v == secs
                ),
                "idle position {i}"
            );
        }
        let choices = sleep_choices_for(s.inactive_secs);
        assert!(choices.len() >= 2);
        for (i, &secs) in choices.iter().enumerate() {
            assert!(
                matches!(
                    SettingsRow::SleepSecs.slider_select(&s, i as f32),
                    StartMessage::SetSleepSecs(v) if v == secs
                ),
                "sleep position {i}"
            );
        }
        assert!(matches!(
            SettingsRow::IdleDim.slider_select(&s, 47.4),
            StartMessage::SetInactiveDimPercent(47)
        ));
        assert!(matches!(
            SettingsRow::SoundVolume.slider_select(&s, 55.5),
            StartMessage::SetSoundVolume(56)
        ));
        assert!(matches!(
            SettingsRow::HapticsStrength.slider_select(&s, 5.0),
            StartMessage::SetHapticsStrength(5)
        ));
        // Out-of-range drag positions clamp onto the tables (never panic),
        // matching the position math's `.min(len - 1)`.
        let last_idle = *START_SCREEN_IDLE_SECS_STEPS.last().unwrap();
        assert!(matches!(
            SettingsRow::IdleSecs.slider_select(&s, 999.0),
            StartMessage::SetInactiveSecs(v) if v == last_idle
        ));
        let last_sleep = *choices.last().unwrap();
        assert!(matches!(
            SettingsRow::SleepSecs.slider_select(&s, 999.0),
            StartMessage::SetSleepSecs(v) if v == last_sleep
        ));
    }
}
