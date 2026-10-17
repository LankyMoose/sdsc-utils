//! In-window Start Screen settings (Options): compact modal / immersive right drawer.

use crate::persist::prefs::{
    AUTO_OPEN_MODES, IMMERSIVE_LAYOUT_MODES, ImmersiveLayout, NAVBAR_MODES, NavbarVisibility,
    REDUCED_MOTION_MODES, ReducedMotion, SELECTION_MODES, START_SCREEN_IDLE_SECS_STEPS,
    START_SCREEN_SLEEP_SECS_STEPS, StartAutoOpen, StartSelectionMode, format_timeout_duration,
    min_sleep_secs_for_idle, secs_step_index,
};
use crate::ui::layout as window_layout;
use crate::ui::start::view::{ScrollReveal, StartMessage, State, scroll_y_to_reveal_bounds};
use crate::ui::theme;
use iced::font::Weight;
use iced::widget::text::Wrapping;
use iced::widget::{
    button, column, container, mouse_area, row, scrollable, slider, space, stack, text, toggler,
};
use iced::{Alignment, Element, Fill, Font, Length, Padding};
use std::time::{Duration, Instant};

/// Immersive settings drawer content width (full height).
pub const DRAWER_W: f32 = 600.0;
/// Compact modal width (~window × 0.75).
pub const MODAL_W: f32 = 576.0;
const DRAWER_ANIM_MS: u64 = 220;
const ROW_PAD: f32 = 10.0;
const COL_GAP: f32 = 6.0;
/// Inner padding of the expanded auto-open dropdown panel.
const MENU_PANEL_PAD: f32 = 4.0;
/// Gap between scrollable content and the embedded scrollbar (matches Configure/Popup).
const SCROLL_GAP: f32 = 8.0;
/// Full scrollbar lane the drawer scrollable reserves on the right while
/// overflowing: iced's default 10px thumb plus the `.spacing(SCROLL_GAP)`
/// lane set on the scrollable itself (that `spacing` call configures the
/// scrollbar, not item gaps — items use COL_GAP). Rows end at this lane, so
/// the floating panel must too: same width as the control row.
const SCROLLBAR_LANE: f32 = 10.0 + SCROLL_GAP;
/// Padding inside the settings scrollable content (must match `panel_view` list padding).
/// Kept in the same ballpark as [`REVEAL_INSET`] — just enough for the focus ring.
const CONTENT_PAD_Y: f32 = 8.0;
const CONTENT_PAD_X: f32 = 4.0;
/// Gap between the static title bar and the scrollable list.
const HEADER_LIST_GAP: f32 = 8.0;
/// Height of the spacer between settings sections (shared by `space_gap` and
/// `focus_row_bounds` so geometry can't drift from the built layout).
const SECTION_GAP_H: f32 = 16.0;
/// Inner spacing between a row label and its helper line.
const HELPER_GAP: f32 = 2.0;
/// Gap between a section caption and its hairline rule.
const SECTION_RULE_GAP: f32 = 6.0;
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
    pub selection_persist: bool,
    pub selection_mode: StartSelectionMode,
    pub always_immersive: bool,
    pub clock_enabled: bool,
    pub immersive_layout: ImmersiveLayout,
    pub navbar: NavbarVisibility,
    pub reduced_motion: ReducedMotion,
    pub inactive_secs: u32,
    pub sleep_secs: u32,
    pub inactive_dim_percent: u8,
    pub sounds_enabled: bool,
    pub sound_volume: u8,
    pub haptics_enabled: bool,
    pub haptics_strength: u8,
    pub perf_throttle_immersive: bool,
    pub perf_antialiasing: bool,
    pub perf_vsync: bool,
    /// Boot values for the renderer prefs (reload row shows while live differs).
    pub perf_aa_boot: bool,
    pub perf_vsync_boot: bool,
    /// Reload queued (row disabled, spinner shown until exit).
    pub perf_reload_queued: bool,
}

/// Focusable rows in the Start settings list (dynamic when sounds/haptics off).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    AutoOpen,
    SelectionPersist,
    SelectionMode,
    AlwaysImmersive,
    Clock,
    ImmersiveLayout,
    Navbar,
    ReducedMotion,
    IdleSecs,
    SleepSecs,
    IdleDim,
    Sounds,
    SoundVolume,
    Haptics,
    HapticsStrength,
    PerfThrottle,
    PerfAntialiasing,
    PerfVsync,
    /// Reload action, visible only while renderer prefs differ from boot.
    PerfReload,
}

impl SettingsRow {
    /// Single row table in display order. `focusable()`, `panel_view`, and
    /// `focus_row_bounds` all derive from this: a row missing here is missing
    /// everywhere (visibly), and reordering here reorders all three together.
    /// To add a row: append/reorder here, extend `visible()` + `section_before()`
    /// / `gap_before()` + the `panel_view` build match + `kind()`/dispatch arms.
    pub const ALL: [Self; 19] = [
        Self::AutoOpen,
        Self::SelectionPersist,
        Self::SelectionMode,
        Self::AlwaysImmersive,
        Self::Clock,
        Self::ImmersiveLayout,
        Self::Navbar,
        Self::ReducedMotion,
        Self::IdleSecs,
        Self::SleepSecs,
        Self::IdleDim,
        Self::Sounds,
        Self::SoundVolume,
        Self::Haptics,
        Self::HapticsStrength,
        Self::PerfThrottle,
        Self::PerfAntialiasing,
        Self::PerfVsync,
        Self::PerfReload,
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
            | Self::Navbar
            | Self::ReducedMotion
            | Self::IdleSecs
            | Self::SleepSecs
            | Self::IdleDim
            | Self::Sounds
            | Self::Haptics
            | Self::PerfThrottle
            | Self::PerfAntialiasing
            | Self::PerfVsync => true,
            // Hidden feature: selection persistence is always off (no UI).
            // The row, its logic, and the mode gate stay wired underneath.
            Self::SelectionPersist => false,
            Self::PerfReload => crate::persist::prefs::renderer_restart_needed(
                snapshot.perf_antialiasing,
                snapshot.perf_vsync,
                snapshot.perf_aa_boot,
                snapshot.perf_vsync_boot,
            ),
            Self::SoundVolume => snapshot.sounds_enabled,
            Self::HapticsStrength => snapshot.haptics_enabled,
            Self::SelectionMode => snapshot.selection_persist,
        }
    }

    pub fn focusable(snapshot: &StartSettingsSnapshot) -> Vec<Self> {
        debug_assert!(
            {
                let mut seen = [false; 19];
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
    ///
    /// Selection restores on open, so it gets its own section immediately
    /// after Opening (Opening-adjacent, before Immersive) rather than joining
    /// either neighbor.
    fn section_before(self) -> Option<&'static str> {
        match self {
            Self::AutoOpen => Some("Opening"),
            Self::SelectionPersist => Some("Selection"),
            Self::AlwaysImmersive => Some("Immersive"),
            Self::Sounds => Some("Feedback"),
            Self::PerfThrottle => Some("Performance"),
            Self::Clock
            | Self::SelectionMode
            | Self::ImmersiveLayout
            | Self::Navbar
            | Self::ReducedMotion
            | Self::IdleSecs
            | Self::SleepSecs
            | Self::IdleDim
            | Self::SoundVolume
            | Self::Haptics
            | Self::HapticsStrength
            | Self::PerfAntialiasing
            | Self::PerfVsync
            | Self::PerfReload => None,
        }
    }

    /// Whether a section spacer precedes this row's section (i.e. this row
    /// opens the Immersive / Feedback section after the previous block).
    fn gap_before(self) -> bool {
        match self {
            Self::SelectionPersist | Self::AlwaysImmersive | Self::Sounds | Self::PerfThrottle => {
                true
            }
            Self::AutoOpen
            | Self::SelectionMode
            | Self::Clock
            | Self::ImmersiveLayout
            | Self::Navbar
            | Self::ReducedMotion
            | Self::IdleSecs
            | Self::SleepSecs
            | Self::IdleDim
            | Self::SoundVolume
            | Self::Haptics
            | Self::HapticsStrength
            | Self::PerfAntialiasing
            | Self::PerfVsync
            | Self::PerfReload => false,
        }
    }

    /// Helper line under the row label, mirroring Configure → Start screen
    /// copy so the drawer explains itself the same way Settings does.
    fn helper(self) -> Option<&'static str> {
        row_helper(self)
    }

    /// Estimated row height, sharing the same `*_row_h` fns (and through them
    /// the same constants) as the builders so geometry can't drift.
    fn height(self, ty: TypeScale) -> f32 {
        let helper = row_helper(self).is_some();
        match self.kind() {
            RowKind::Toggle => toggle_row_h(ty, helper),
            RowKind::Action => action_row_h(),
            RowKind::Selector => selector_row_h(ty, helper),
            RowKind::Slider => slider_row_h(ty, helper),
        }
    }

    /// Widget kind for a row, declared once; confirm/nudge dispatch over it.
    pub fn kind(self) -> RowKind {
        match self {
            Self::AutoOpen
            | Self::SelectionMode
            | Self::ImmersiveLayout
            | Self::Navbar
            | Self::ReducedMotion => RowKind::Selector,
            Self::SelectionPersist
            | Self::AlwaysImmersive
            | Self::Clock
            | Self::Sounds
            | Self::Haptics
            | Self::PerfThrottle
            | Self::PerfAntialiasing
            | Self::PerfVsync => RowKind::Toggle,
            Self::PerfReload => RowKind::Action,
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
            Self::SelectionPersist => snapshot.selection_persist,
            Self::AlwaysImmersive => snapshot.always_immersive,
            Self::Clock => snapshot.clock_enabled,
            Self::Sounds => snapshot.sounds_enabled,
            Self::Haptics => snapshot.haptics_enabled,
            Self::PerfThrottle => snapshot.perf_throttle_immersive,
            Self::PerfAntialiasing => snapshot.perf_antialiasing,
            Self::PerfVsync => snapshot.perf_vsync,
            row => unreachable!("{row:?} is not a toggle row"),
        }
    }

    /// Toggle message constructor for toggle rows.
    fn set_toggle(self, on: bool) -> StartMessage {
        match self {
            Self::SelectionPersist => StartMessage::SetSelectionPersist(on),
            Self::AlwaysImmersive => StartMessage::SetAlwaysImmersive(on),
            Self::Clock => StartMessage::SetClock(on),
            Self::Sounds => StartMessage::SetSounds(on),
            Self::Haptics => StartMessage::SetHaptics(on),
            Self::PerfThrottle => StartMessage::SetPerfThrottle(on),
            Self::PerfAntialiasing => StartMessage::SetPerfAntialiasing(on),
            Self::PerfVsync => StartMessage::SetPerfVsync(on),
            row => unreachable!("{row:?} is not a toggle row"),
        }
    }

    /// Dropdown-open message for selector rows (confirm path).
    fn open_menu(self) -> StartMessage {
        match self {
            Self::AutoOpen => StartMessage::ToggleAutoOpenMenu,
            Self::SelectionMode => StartMessage::ToggleSelectionModeMenu,
            Self::ImmersiveLayout => StartMessage::ToggleLayoutMenu,
            Self::Navbar => StartMessage::ToggleNavbarMenu,
            Self::ReducedMotion => StartMessage::ToggleReducedMotionMenu,
            row => unreachable!("{row:?} is not a selector row"),
        }
    }

    /// Dropdown length for selector rows (option count driving highlight wrap).
    pub fn selector_count(self) -> usize {
        match self {
            Self::AutoOpen => AUTO_OPEN_MODES.len(),
            Self::SelectionMode => SELECTION_MODES.len(),
            Self::ImmersiveLayout => IMMERSIVE_LAYOUT_MODES.len(),
            Self::Navbar => NAVBAR_MODES.len(),
            Self::ReducedMotion => REDUCED_MOTION_MODES.len(),
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
            Self::SelectionMode => SELECTION_MODES
                .iter()
                .position(|&m| m == snapshot.selection_mode)
                .unwrap_or(0),
            Self::ImmersiveLayout => IMMERSIVE_LAYOUT_MODES
                .iter()
                .position(|&m| m == snapshot.immersive_layout)
                .unwrap_or(0),
            Self::Navbar => NAVBAR_MODES
                .iter()
                .position(|&m| m == snapshot.navbar)
                .unwrap_or(0),
            Self::ReducedMotion => REDUCED_MOTION_MODES
                .iter()
                .position(|&m| m == snapshot.reduced_motion)
                .unwrap_or(0),
            row => unreachable!("{row:?} is not a selector row"),
        }
    }

    /// Display label of the dropdown option at `index`.
    pub fn selector_label(self, index: usize) -> &'static str {
        match self {
            Self::AutoOpen => AUTO_OPEN_MODES[index].label(),
            Self::SelectionMode => SELECTION_MODES[index].label(),
            Self::ImmersiveLayout => IMMERSIVE_LAYOUT_MODES[index].label(),
            Self::Navbar => NAVBAR_MODES[index].label(),
            Self::ReducedMotion => REDUCED_MOTION_MODES[index].label(),
            row => unreachable!("{row:?} is not a selector row"),
        }
    }

    /// Whether the dropdown option at `index` is the committed value.
    pub fn selector_selected(self, index: usize, snapshot: &StartSettingsSnapshot) -> bool {
        match self {
            Self::AutoOpen => AUTO_OPEN_MODES
                .get(index)
                .is_some_and(|&m| m == snapshot.auto_open),
            Self::SelectionMode => SELECTION_MODES
                .get(index)
                .is_some_and(|&m| m == snapshot.selection_mode),
            Self::ImmersiveLayout => IMMERSIVE_LAYOUT_MODES
                .get(index)
                .is_some_and(|&m| m == snapshot.immersive_layout),
            Self::Navbar => NAVBAR_MODES
                .get(index)
                .is_some_and(|&m| m == snapshot.navbar),
            Self::ReducedMotion => REDUCED_MOTION_MODES
                .get(index)
                .is_some_and(|&m| m == snapshot.reduced_motion),
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
            Self::SelectionMode => StartMessage::SetSelectionMode(
                SELECTION_MODES[index.min(SELECTION_MODES.len() - 1)],
            ),
            Self::ImmersiveLayout => StartMessage::SetImmersiveLayout(
                IMMERSIVE_LAYOUT_MODES[index.min(IMMERSIVE_LAYOUT_MODES.len() - 1)],
            ),
            Self::Navbar => {
                StartMessage::SetNavbarVisibility(NAVBAR_MODES[index.min(NAVBAR_MODES.len() - 1)])
            }
            Self::ReducedMotion => StartMessage::SetReducedMotion(
                REDUCED_MOTION_MODES[index.min(REDUCED_MOTION_MODES.len() - 1)],
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
            Self::SelectionMode => StartMessage::SetSelectionMode(if delta > 0 {
                snapshot.selection_mode.next()
            } else {
                snapshot.selection_mode.prev()
            }),
            Self::ImmersiveLayout => {
                StartMessage::SetImmersiveLayout(snapshot.immersive_layout.cycle())
            }
            Self::Navbar => StartMessage::SetNavbarVisibility(snapshot.navbar.cycle()),
            Self::ReducedMotion => StartMessage::SetReducedMotion(snapshot.reduced_motion.cycle()),
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
    /// One-shot action button (reload UI). Confirm runs it; nudge ignores it.
    Action,
}

/// Which half of the nav cue the pad path should play for a settings row.
///
/// The apply side previews the *changed* modality after saving (volume rows
/// play sound at the new volume, strength rows rumble at the new strength),
/// so the pre-flip nav cue must only play the *other* half — otherwise a
/// pad nudge double-cues (old-value nav cue + new-value apply preview) while
/// the equivalent mouse drag single-previews. Rows without an apply preview
/// keep the full cue. Keep exhaustive alongside future rows: a new feedback
/// row with an apply preview must be added to the split here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsCue {
    Full,
    HapticOnly,
    SoundOnly,
}

/// Row-aware nav-cue policy for pad/keyboard settings navigation.
pub fn settings_nav_cue(row: SettingsRow) -> SettingsCue {
    match row {
        // Apply plays sound at the new volume; nav covers haptics only.
        SettingsRow::Sounds | SettingsRow::SoundVolume => SettingsCue::HapticOnly,
        // Apply rumbles at the new strength; nav covers sound only.
        SettingsRow::Haptics | SettingsRow::HapticsStrength => SettingsCue::SoundOnly,
        SettingsRow::AutoOpen
        | SettingsRow::SelectionPersist
        | SettingsRow::SelectionMode
        | SettingsRow::AlwaysImmersive
        | SettingsRow::Clock
        | SettingsRow::ImmersiveLayout
        | SettingsRow::Navbar
        | SettingsRow::ReducedMotion
        | SettingsRow::IdleSecs
        | SettingsRow::SleepSecs
        | SettingsRow::IdleDim
        | SettingsRow::PerfThrottle
        | SettingsRow::PerfAntialiasing
        | SettingsRow::PerfVsync
        | SettingsRow::PerfReload => SettingsCue::Full,
    }
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
        let (row_top, row_h) = focus_row_bounds(self.focus, snapshot, immersive)?;
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

    /// Absolute scroll Y to reveal the trailing [`SettingsRow::PerfReload`]
    /// row after it mounts (an AA/vsync toggle dirtied the renderer prefs).
    /// Focus stays on the toggle, so this targets the reload row explicitly
    /// rather than the focused row. Returns `None` when the row is absent
    /// (prefs clean) or already fully visible.
    pub fn reload_scroll_y(
        &self,
        snapshot: &StartSettingsSnapshot,
        immersive: bool,
        direction: ScrollReveal,
    ) -> Option<f32> {
        let rows = SettingsRow::focusable(snapshot);
        let idx = rows
            .iter()
            .position(|&row| row == SettingsRow::PerfReload)?;
        let (row_top, row_h) = focus_row_bounds(idx, snapshot, immersive)?;
        let viewport_h = if self.viewport_h <= 1.0 {
            if immersive {
                720.0
            } else {
                COMPACT_VIEWPORT_FALLBACK
            }
        } else {
            self.viewport_h
        };
        // Same focus-ring inset as `focus_scroll_y`, no top pin: the reload
        // row is always trailing, so it can only ever need a downward reveal.
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
        // The reload action applies pending renderer prefs immediately.
        RowKind::Action => Some(StartMessage::ReloadUi),
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
        // Actions only run on Confirm; directionals leave them alone.
        RowKind::Action => None,
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

/// Helper line under a settings row label, mirroring Configure → Start
/// screen copy (single source for the drawer; Configure keeps its own
/// literals). `None` where Configure itself shows no helper.
fn row_helper(row: SettingsRow) -> Option<&'static str> {
    match row {
        SettingsRow::AutoOpen => Some("When a controller connects"),
        SettingsRow::SelectionPersist => Some("Keep the game highlight across restarts"),
        SettingsRow::SelectionMode => Some("Match the game, or its position"),
        SettingsRow::AlwaysImmersive => {
            Some("Open full screen; Circle closes instead of going back")
        }
        SettingsRow::Clock => None,
        SettingsRow::ImmersiveLayout => Some("Orientation of the games strip"),
        SettingsRow::Navbar => Some("Section indicator visibility"),
        SettingsRow::ReducedMotion => Some("Skip movement animations"),
        SettingsRow::IdleSecs => Some("Fullscreen dims after this long idle"),
        SettingsRow::SleepSecs => Some("Fullscreen sleeps after this long idle"),
        SettingsRow::IdleDim => Some("Brightness while idle"),
        SettingsRow::Sounds => None,
        SettingsRow::SoundVolume => None,
        SettingsRow::Haptics => None,
        SettingsRow::HapticsStrength => None,
        SettingsRow::PerfThrottle => {
            Some("Let the fullscreen Start screen rest when nothing is moving")
        }
        SettingsRow::PerfAntialiasing => Some("Smoother edges"),
        SettingsRow::PerfVsync => Some("Sync to display; off may tear"),
        SettingsRow::PerfReload => None,
    }
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
/// Dropdown panels float over the list ([`menu_overlay`]) instead of living
/// in the flow, so row bounds never move when a menu opens or closes.
/// Stacking mirrors `panel_view`: one COL_GAP between every child, heights
/// from the shared `*_row_h` fns / section + gap constants.
pub fn focus_row_bounds(
    focus: usize,
    snapshot: &StartSettingsSnapshot,
    immersive: bool,
) -> Option<(f32, f32)> {
    let rows = SettingsRow::focusable(snapshot);
    let target = *rows.get(focus)?;
    debug_assert!(target.visible(snapshot));
    let ty = TypeScale::for_mode(immersive);
    // Content padding is inside the scrollable — offsets are absolute in that content.
    // Title lives in the static header (not scrolled).
    // Stacking mirrors `panel_view`: one COL_GAP between every child, heights
    // from the shared `*_row_h` fns / section + gap constants.
    let mut y = CONTENT_PAD_Y;
    for item in layout_items(snapshot) {
        let h = match item {
            LayoutItem::Section(_) => section_block_h(ty.section),
            LayoutItem::Gap => SECTION_GAP_H,
            LayoutItem::Row(row) => row.height(ty),
        };
        let top = y;
        y += h + COL_GAP;
        if item == LayoutItem::Row(target) {
            return Some((top, h));
        }
    }
    None
}

/// Line height estimate for a text size (matches `menu_option_h` convention).
fn text_line_h(size: f32) -> f32 {
    (size * 1.25).max(22.0)
}

fn toggle_row_h(ty: TypeScale, helper: bool) -> f32 {
    let mut content = text_line_h(ty.body).max(ty.toggle);
    if helper {
        content += HELPER_GAP + ty.helper * 1.25;
    }
    ROW_PAD * 2.0 + content
}

/// Fixed inner height of the reload action button. The row (`action_row_h`)
/// and the button are both fixed, so the shared bounds geometry agrees with
/// the built widget by construction regardless of font metrics; the label
/// and spinner always fit with room to spare in both type scales.
const ACTION_BUTTON_H: f32 = 30.0;

fn action_row_h() -> f32 {
    ROW_PAD * 2.0 + ACTION_BUTTON_H
}

/// Fixed height of the cycle-row option viewport: one option visible.
fn cycle_viewport_h(helper: f32) -> f32 {
    helper * 1.5
}

fn cycle_row_h(ty: TypeScale, helper: bool) -> f32 {
    let mut content = cycle_viewport_h(ty.helper).max(ty.toggle).max(22.0);
    if helper {
        content = content.max(text_line_h(ty.body) + HELPER_GAP + ty.helper * 1.25);
    }
    ROW_PAD * 2.0 + content
}

/// Fixed height of a dropdown selector header row (same clipped single-option
/// viewport as [`cycle_row_h`]).
fn selector_row_h(ty: TypeScale, helper: bool) -> f32 {
    cycle_row_h(ty, helper)
}

/// Estimated height of one expanded dropdown option row.
fn menu_option_h(body: f32) -> f32 {
    ROW_PAD * 2.0 + (body * 1.25).max(22.0)
}

/// Estimated height of an expanded dropdown panel: padding top/bottom + one
/// option row per mode with spacing between (matches `menu_panel_h` geometry
/// used by the floating [`menu_overlay`]).
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

fn slider_row_h(ty: TypeScale, helper: bool) -> f32 {
    // Title line + gap + slider track (iced default is a bit taller than a tight 22px guess).
    let mut content = ty.helper + SLIDER_INNER_GAP + 26.0;
    if helper {
        content += HELPER_GAP + ty.helper * 1.25;
    }
    ROW_PAD * 2.0 + content
}

/// One stacked entry in the scrollable settings list. `Row` entries are the
/// focusable rows from [`SettingsRow::ALL`]; `Section`/`Gap` are the static
/// separators anchored to the row that follows them. Dropdown panels are NOT
/// layout items: they float over the list ([`menu_overlay`]) so opening one
/// never shifts rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutItem {
    Section(&'static str),
    Gap,
    Row(SettingsRow),
}

/// Full stacking order of the scrollable list, derived from the single row
/// table: visible rows in [`SettingsRow::ALL`] order, each with its anchored
/// section/gap prefix. `panel_view` and `focus_row_bounds` both consume this
/// shape (widgets vs. heights) so the three orderings agree by construction.
fn layout_items(snapshot: &StartSettingsSnapshot) -> Vec<LayoutItem> {
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

/// Right inset for the floating dropdown panel: content padding, plus the
/// scrollbar lane when the list overflows (the thumb + spacing reserve the
/// lane, so rows end short of it — the panel matches the control width).
fn menu_overlay_right_pad(content_h: f32, viewport_h: f32) -> f32 {
    CONTENT_PAD_X
        + if content_h > viewport_h {
            SCROLLBAR_LANE
        } else {
            0.0
        }
}

/// Total scrollable content height: top/bottom content padding plus one gap
/// between stacked items. Decides scrollbar visibility for the overlay inset.
fn settings_content_h(snapshot: &StartSettingsSnapshot, immersive: bool) -> f32 {
    let ty = TypeScale::for_mode(immersive);
    let items = layout_items(snapshot);
    let mut h = CONTENT_PAD_Y * 2.0;
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            h += COL_GAP;
        }
        h += match item {
            LayoutItem::Section(_) => section_block_h(ty.section),
            LayoutItem::Gap => SECTION_GAP_H,
            LayoutItem::Row(row) => row.height(ty),
        };
    }
    h
}

/// Vertical offset (from the scrollable viewport top) for the floating
/// dropdown panel of the menu-owning row.
///
/// Pure geometry so it stays unit-testable: the panel hangs below its row,
/// flips above it when it would overflow the viewport bottom and fits above,
/// and clamps into view otherwise.
fn menu_overlay_y(row_top: f32, row_h: f32, panel_h: f32, scroll_y: f32, viewport_h: f32) -> f32 {
    let row_visible_top = row_top - scroll_y;
    let below = row_visible_top + row_h;
    let fits_below = below + panel_h <= viewport_h;
    let fits_above = row_visible_top - panel_h >= 0.0;
    let y = if fits_below || !fits_above {
        below
    } else {
        row_visible_top - panel_h
    };
    y.clamp(0.0, (viewport_h - panel_h).max(0.0))
}

/// Floating dropdown panel for the open selector menu, if any.
///
/// Overlays the list at a fixed position derived from the menu row's content
/// bounds and the live scroll offset — opening a menu never shifts rows, and
/// the panel tracks its row while scrolling. `None` when no menu is open or
/// the menu row left the focusable list.
fn menu_overlay(
    state: &State,
    snapshot: &StartSettingsSnapshot,
    immersive: bool,
    #[cfg(debug_assertions)] debug_bounds: bool,
) -> Option<Element<'static, StartMessage>> {
    let (menu_row, _) = state.settings.menu?;
    let ty = TypeScale::for_mode(immersive);
    let idx = SettingsRow::focusable(snapshot)
        .iter()
        .position(|&row| row == menu_row)?;
    let (row_top, row_h) = focus_row_bounds(idx, snapshot, immersive)?;
    let panel_h = menu_panel_h(menu_row.selector_count(), ty.body);
    let viewport_h = if state.settings.viewport_h <= 1.0 {
        if immersive {
            720.0
        } else {
            COMPACT_VIEWPORT_FALLBACK
        }
    } else {
        state.settings.viewport_h
    };
    let y = menu_overlay_y(row_top, row_h, panel_h, state.settings.scroll_y, viewport_h);
    let right_pad = menu_overlay_right_pad(settings_content_h(snapshot, immersive), viewport_h);
    let panel = selector_menu_panel(state.settings.menu, menu_row, snapshot, ty)?;
    #[cfg(debug_assertions)]
    let panel = crate::ui::debug_bounds::wrap(panel, debug_bounds);
    Some(
        column![
            space::Space::new().width(Fill).height(Length::Fixed(y)),
            container(panel).width(Fill).padding(Padding {
                top: 0.0,
                right: right_pad,
                bottom: 0.0,
                left: CONTENT_PAD_X,
            }),
        ]
        .width(Fill)
        .height(Fill)
        .into(),
    )
}

pub fn panel_view<'a>(
    state: &'a State,
    snapshot: &StartSettingsSnapshot,
    immersive: bool,
    #[cfg(debug_assertions)] debug_bounds: bool,
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
        let widget: Element<'_, StartMessage> = match row {
            SettingsRow::AutoOpen => selector_row(
                "Auto open:",
                SettingsRow::AutoOpen.helper(),
                snapshot.auto_open.label(),
                focused,
                ty,
                SettingsRow::AutoOpen.open_menu(),
            ),
            SettingsRow::AlwaysImmersive => toggle_row(
                "Always immersive",
                SettingsRow::AlwaysImmersive.helper(),
                SettingsRow::AlwaysImmersive.toggle_value(snapshot),
                focused,
                ty,
                |on| SettingsRow::AlwaysImmersive.set_toggle(on),
            ),
            SettingsRow::SelectionPersist => toggle_row(
                "Remember selection",
                SettingsRow::SelectionPersist.helper(),
                SettingsRow::SelectionPersist.toggle_value(snapshot),
                focused,
                ty,
                |on| SettingsRow::SelectionPersist.set_toggle(on),
            ),
            SettingsRow::SelectionMode => selector_row(
                "Restore by",
                SettingsRow::SelectionMode.helper(),
                snapshot.selection_mode.label(),
                focused,
                ty,
                SettingsRow::SelectionMode.open_menu(),
            ),
            SettingsRow::Clock => toggle_row(
                "Display clock",
                SettingsRow::Clock.helper(),
                SettingsRow::Clock.toggle_value(snapshot),
                focused,
                ty,
                |on| SettingsRow::Clock.set_toggle(on),
            ),
            SettingsRow::ImmersiveLayout => selector_row(
                "Layout",
                SettingsRow::ImmersiveLayout.helper(),
                snapshot.immersive_layout.label(),
                focused,
                ty,
                SettingsRow::ImmersiveLayout.open_menu(),
            ),
            SettingsRow::Navbar => selector_row(
                "Navbar",
                SettingsRow::Navbar.helper(),
                snapshot.navbar.label(),
                focused,
                ty,
                SettingsRow::Navbar.open_menu(),
            ),
            SettingsRow::ReducedMotion => selector_row(
                "Reduced motion",
                SettingsRow::ReducedMotion.helper(),
                snapshot.reduced_motion.label(),
                focused,
                ty,
                SettingsRow::ReducedMotion.open_menu(),
            ),
            SettingsRow::IdleSecs => {
                let snap_for_msg = snapshot.clone();
                slider_row(
                    format!("Idle after {}", format_timeout_duration(idle_secs)),
                    SettingsRow::IdleSecs.helper(),
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
                    SettingsRow::SleepSecs.helper(),
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
                    SettingsRow::IdleDim.helper(),
                    focused,
                    ty,
                    0.0..=100.0,
                    f32::from(dim),
                    move |value| SettingsRow::IdleDim.slider_select(&snap_for_msg, value),
                )
            }
            SettingsRow::Sounds => toggle_row(
                "UI sounds",
                SettingsRow::Sounds.helper(),
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
                    SettingsRow::SoundVolume.helper(),
                    focused,
                    ty,
                    0.0..=100.0,
                    f32::from(volume),
                    move |value| SettingsRow::SoundVolume.slider_select(&snap_for_msg, value),
                )
            }
            SettingsRow::Haptics => toggle_row(
                "Controller haptics",
                SettingsRow::Haptics.helper(),
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
                    SettingsRow::HapticsStrength.helper(),
                    focused,
                    ty,
                    0.0..=100.0,
                    f32::from(strength),
                    move |value| SettingsRow::HapticsStrength.slider_select(&snap_for_msg, value),
                )
            }
            SettingsRow::PerfThrottle => toggle_row(
                "Save power when idle",
                SettingsRow::PerfThrottle.helper(),
                SettingsRow::PerfThrottle.toggle_value(snapshot),
                focused,
                ty,
                |on| SettingsRow::PerfThrottle.set_toggle(on),
            ),
            SettingsRow::PerfAntialiasing => toggle_row(
                "Antialiasing",
                SettingsRow::PerfAntialiasing.helper(),
                SettingsRow::PerfAntialiasing.toggle_value(snapshot),
                focused,
                ty,
                |on| SettingsRow::PerfAntialiasing.set_toggle(on),
            ),
            SettingsRow::PerfVsync => toggle_row(
                "Vsync",
                SettingsRow::PerfVsync.helper(),
                SettingsRow::PerfVsync.toggle_value(snapshot),
                focused,
                ty,
                |on| SettingsRow::PerfVsync.set_toggle(on),
            ),
            SettingsRow::PerfReload => action_row(
                "Restart app to apply changes",
                focused,
                ty,
                snapshot.perf_reload_queued,
            ),
        };
        #[cfg(debug_assertions)]
        built_rows.push(row);
        #[cfg(debug_assertions)]
        let widget = crate::ui::debug_bounds::wrap(widget, debug_bounds);
        items = items.push(widget);
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

    #[cfg(debug_assertions)]
    let header: Element<'_, StartMessage> =
        crate::ui::debug_bounds::wrap(header.into(), debug_bounds);
    #[cfg(debug_assertions)]
    let list = crate::ui::debug_bounds::wrap(list.into(), debug_bounds);

    // Float the open dropdown over the list at its row's position (see
    // `menu_overlay`): rows never shift when a menu opens or closes.
    let mut layer = stack![list];
    if let Some(overlay) = menu_overlay(
        state,
        snapshot,
        immersive,
        #[cfg(debug_assertions)]
        debug_bounds,
    ) {
        layer = layer.push(overlay);
    }
    let list = layer.width(Fill).height(Fill);

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
    #[cfg(debug_assertions)] debug_bounds: bool,
) -> Element<'a, StartMessage> {
    let dim = scrim_dim(progress);
    let modal = container(theme::framed(panel_view(
        state,
        snapshot,
        false,
        #[cfg(debug_assertions)]
        debug_bounds,
    )))
    .width(Length::Fixed(MODAL_W))
    .max_height(COMPACT_MODAL_MAX_H);
    #[cfg(debug_assertions)]
    let modal: Element<'_, StartMessage> =
        crate::ui::debug_bounds::wrap_fixed(modal.into(), debug_bounds);
    let card = container(modal)
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
    #[cfg(debug_assertions)] debug_bounds: bool,
) -> Element<'a, StartMessage> {
    let dim = scrim_dim(progress);
    let w = drawer_width(progress);
    let panel = container(panel_view(
        state,
        snapshot,
        true,
        #[cfg(debug_assertions)]
        debug_bounds,
    ))
    .width(Length::Fixed(DRAWER_W))
    .height(Fill)
    .style(theme::immersive_island_radius(0.0));
    #[cfg(debug_assertions)]
    let panel: Element<'_, StartMessage> =
        crate::ui::debug_bounds::wrap_fixed(panel.into(), debug_bounds);

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

/// Estimated height of a section header block (caption + gap + 1px hairline),
/// sharing [`SECTION_RULE_GAP`] with the built widget so geometry agrees.
fn section_block_h(section: f32) -> f32 {
    section + SECTION_RULE_GAP + 1.0
}

/// Section header: DIM semibold caption (Configure group-caption language)
/// over a LINE hairline, so sections read as groups instead of loose rows.
fn section_block(label: &'static str, size: f32) -> Element<'static, StartMessage> {
    column![
        text(label)
            .size(size)
            .font(Font {
                weight: Weight::Semibold,
                ..Font::DEFAULT
            })
            .color(theme::DIM),
        container(space().height(Length::Fixed(1.0)))
            .width(Fill)
            .style(theme::configure_header_rule),
    ]
    .spacing(SECTION_RULE_GAP)
    .width(Fill)
    .into()
}

fn section_label(label: &'static str, size: f32) -> Element<'static, StartMessage> {
    section_block(label, size)
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
    helper: Option<&'static str>,
    checked: bool,
    focused: bool,
    ty: TypeScale,
    message: fn(bool) -> StartMessage,
) -> Element<'static, StartMessage> {
    let mut label_col = column![text(label).size(ty.body).color(theme::INK).width(Fill),]
        .spacing(HELPER_GAP)
        .width(Fill);
    if let Some(helper) = helper {
        label_col = label_col.push(
            text(helper)
                .size(ty.helper)
                .color(theme::DIM)
                .wrapping(Wrapping::None),
        );
    }
    container(
        row![
            label_col,
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

/// Full-width reload action. Row and button heights are fixed
/// ([`action_row_h`]) so the shared bounds geometry agrees with the built
/// widget by construction. Content is left-aligned like every other row, and
/// the idle label and the queued ("Restarting…" + spinner) state share the
/// same box, so swapping states never moves layout. Disabled once queued so
/// a second press cannot queue another restart.
fn action_row(
    label: &'static str,
    focused: bool,
    ty: TypeScale,
    queued: bool,
) -> Element<'static, StartMessage> {
    let content: Element<'_, StartMessage> = if queued {
        container(
            row![
                text("Restarting…").size(ty.body),
                crate::ui::start::view::scan_spinner(14.0, Instant::now(), 1.0),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .width(Fill)
        .center_x(Fill)
        .into()
    } else {
        text(label)
            .size(ty.body)
            .width(Fill)
            .align_x(Alignment::Center)
            .into()
    };
    container(
        button(content)
            .width(Fill)
            .height(Length::Fixed(ACTION_BUTTON_H))
            .padding([4, 12])
            .on_press_maybe((!queued).then_some(StartMessage::ReloadUi))
            .style(theme::primary),
    )
    .padding(ROW_PAD)
    .width(Fill)
    .height(Length::Fixed(action_row_h()))
    .align_y(Alignment::Center)
    .style(focus_style(focused))
    .into()
}

/// Dropdown selector header: label left, current option in a fixed-height
/// clipped viewport right so only the selected option displays.
/// Click (or pad Cross) toggles the menu; `toggle` opens/closes it.
fn selector_row(
    label: &'static str,
    helper: Option<&'static str>,
    value_label: &'static str,
    focused: bool,
    ty: TypeScale,
    toggle: StartMessage,
) -> Element<'static, StartMessage> {
    // Fixed-height clipped viewport: only the selected option displays.
    let viewport_h = cycle_viewport_h(ty.helper);
    let mut label_col = column![text(label).size(ty.body).color(theme::INK).width(Fill),]
        .spacing(HELPER_GAP)
        .width(Fill);
    if let Some(helper) = helper {
        label_col = label_col.push(
            text(helper)
                .size(ty.helper)
                .color(theme::DIM)
                .wrapping(Wrapping::None),
        );
    }
    mouse_area(
        container(
            row![
                label_col,
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
            .style(theme::menu_panel)
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
    helper: Option<&'static str>,
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
    let mut content = column![text(label).size(ty.helper).color(theme::MUTED),]
        .spacing(SLIDER_INNER_GAP)
        .width(Fill);
    if let Some(helper) = helper {
        content = content.push(
            text(helper)
                .size(ty.helper)
                .color(theme::DIM)
                .wrapping(Wrapping::None),
        );
    }
    content = content.push(
        slider(range, value, on_change)
            .step(step)
            .style(theme::slider),
    );
    container(content)
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
            selection_persist: true,
            selection_mode: StartSelectionMode::ByGame,
            always_immersive: false,
            clock_enabled: true,
            immersive_layout: ImmersiveLayout::Vertical,
            navbar: NavbarVisibility::WhenNavigating,
            reduced_motion: ReducedMotion::System,
            inactive_secs: 30,
            sleep_secs: 120,
            inactive_dim_percent: 50,
            sounds_enabled: sounds,
            sound_volume: 60,
            haptics_enabled: haptics,
            haptics_strength: 60,
            perf_throttle_immersive: false,
            perf_antialiasing: true,
            perf_vsync: true,
            perf_aa_boot: true,
            perf_vsync_boot: true,
            perf_reload_queued: false,
        }
    }

    #[test]
    fn focusable_omits_volume_when_sounds_off() {
        let rows = SettingsRow::focusable(&snap(false, true));
        assert!(!rows.contains(&SettingsRow::SoundVolume));
        assert!(rows.contains(&SettingsRow::HapticsStrength));
    }

    #[test]
    #[cfg(debug_assertions)]
    fn panel_view_builds_with_bounds_on_and_off() {
        // The bounds overlay must compose over the drawer rows in both
        // states (including an open dropdown menu) without panicking.
        let snapshot = snap(true, true);
        let state = State::default();
        drop(panel_view(&state, &snapshot, false, false));
        drop(panel_view(&state, &snapshot, false, true));
        drop(panel_view(&state, &snapshot, true, true));
        let mut menu_open = State::default();
        menu_open.settings.menu = Some((SettingsRow::AutoOpen, 0));
        drop(panel_view(&menu_open, &snapshot, true, true));
        drop(compact_overlay(&state, &snapshot, 1.0, true));
        drop(immersive_drawer(&state, &snapshot, 1.0, true));
        // Dirty + queued renderer prefs: the reload action row (both states)
        // must build without panicking.
        let mut dirty = snapshot.clone();
        dirty.perf_vsync = false;
        drop(panel_view(&state, &dirty, false, false));
        drop(panel_view(&state, &dirty, true, true));
        dirty.perf_reload_queued = true;
        drop(panel_view(&state, &dirty, false, false));
        drop(panel_view(&state, &dirty, true, true));
    }

    #[test]
    fn focusable_lists_selectors_in_order() {
        let rows = SettingsRow::focusable(&snap(true, true));
        assert_eq!(
            rows,
            vec![
                SettingsRow::AutoOpen,
                // SelectionPersist is a hidden row (never focusable).
                SettingsRow::SelectionMode,
                SettingsRow::AlwaysImmersive,
                SettingsRow::Clock,
                SettingsRow::ImmersiveLayout,
                SettingsRow::Navbar,
                SettingsRow::ReducedMotion,
                SettingsRow::IdleSecs,
                SettingsRow::SleepSecs,
                SettingsRow::IdleDim,
                SettingsRow::Sounds,
                SettingsRow::SoundVolume,
                SettingsRow::Haptics,
                SettingsRow::HapticsStrength,
                SettingsRow::PerfThrottle,
                SettingsRow::PerfAntialiasing,
                SettingsRow::PerfVsync,
            ]
        );
    }

    #[test]
    fn selection_rows_follow_the_hidden_persist_flag() {
        // SelectionPersist is always hidden (no UI); SelectionMode renders
        // only while the hidden flag is on.
        let mut s = snap(true, true);
        assert!(SettingsRow::focusable(&s).contains(&SettingsRow::SelectionMode));
        assert!(!SettingsRow::focusable(&s).contains(&SettingsRow::SelectionPersist));
        s.selection_persist = false;
        let rows = SettingsRow::focusable(&s);
        assert!(!rows.contains(&SettingsRow::SelectionMode));
        assert!(!rows.contains(&SettingsRow::SelectionPersist));
    }

    #[test]
    fn selection_mode_confirm_opens_menu_nudge_cycles() {
        let s = snap(true, true);
        let msg = confirm_message(SettingsRow::SelectionMode, &s).unwrap();
        assert!(matches!(msg, StartMessage::ToggleSelectionModeMenu));
        for delta in [1, -1] {
            let msg = nudge_message(SettingsRow::SelectionMode, &s, delta).unwrap();
            assert!(matches!(
                msg,
                StartMessage::SetSelectionMode(StartSelectionMode::ByPosition)
            ));
        }
        let msg = confirm_message(SettingsRow::SelectionPersist, &s).unwrap();
        assert!(matches!(msg, StartMessage::SetSelectionPersist(false)));
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
        for row in [
            SettingsRow::AutoOpen,
            SettingsRow::ImmersiveLayout,
            SettingsRow::Navbar,
            SettingsRow::ReducedMotion,
        ] {
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
        for navbar in NAVBAR_MODES {
            let mut v = snap(true, true);
            v.navbar = navbar;
            let row = SettingsRow::Navbar;
            let h = row.initial_highlight(&v);
            assert!(row.selector_selected(h, &v));
            assert!(
                matches!(row.selector_select(h), StartMessage::SetNavbarVisibility(n) if n == navbar)
            );
        }
        for motion in REDUCED_MOTION_MODES {
            let mut v = snap(true, true);
            v.reduced_motion = motion;
            let row = SettingsRow::ReducedMotion;
            let h = row.initial_highlight(&v);
            assert!(row.selector_selected(h, &v));
            assert!(
                matches!(row.selector_select(h), StartMessage::SetReducedMotion(m) if m == motion)
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
        let (y0, _) = focus_row_bounds(0, &s, false).unwrap();
        let last = SettingsRow::focusable(&s).len() - 1;
        let (y_last, _) = focus_row_bounds(last, &s, false).unwrap();
        assert!(y_last > y0);
        assert!(y0 >= CONTENT_PAD_Y);
    }

    #[test]
    fn menu_overlay_y_hangs_below_flips_above_and_clamps() {
        // Plenty of room: below the row.
        assert!((menu_overlay_y(100.0, 40.0, 120.0, 0.0, 600.0) - 140.0).abs() < f32::EPSILON);
        // Row near the bottom, room above: flips above the row.
        assert!((menu_overlay_y(500.0, 40.0, 120.0, 0.0, 600.0) - 380.0).abs() < f32::EPSILON);
        // No room either way: clamps into view.
        assert!((menu_overlay_y(500.0, 40.0, 900.0, 0.0, 600.0) - 0.0).abs() < f32::EPSILON);
        // Scrolled list: position tracks the visible row top.
        assert!((menu_overlay_y(300.0, 40.0, 120.0, 200.0, 600.0) - 140.0).abs() < f32::EPSILON);
        // Row scrolled past the top: clamps to the viewport top.
        assert!((menu_overlay_y(50.0, 40.0, 120.0, 200.0, 600.0) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn menu_overlay_right_pad_clears_the_scrollbar_only_when_overflowing() {
        // Short list, no scrollbar: symmetric content padding.
        assert!((menu_overlay_right_pad(200.0, 600.0) - CONTENT_PAD_X).abs() < f32::EPSILON);
        // Overflowing list: thumb + spacing reserve the lane, so the
        // panel stops short of it exactly like rows do.
        assert!(
            (menu_overlay_right_pad(900.0, 600.0) - (CONTENT_PAD_X + SCROLLBAR_LANE)).abs()
                < f32::EPSILON
        );
    }

    #[test]
    fn settings_content_h_covers_rows_gaps_and_pads() {
        let s = snap(true, true);
        let ty = TypeScale::for_mode(false);
        let rows_h: f32 = SettingsRow::focusable(&s)
            .iter()
            .map(|row| row.height(ty))
            .sum();
        let h = settings_content_h(&s, false);
        assert!(h > rows_h, "pads + gaps add height");
        assert!(
            h < rows_h + 400.0,
            "content height stays in a sane band, got {h} for rows {rows_h}"
        );
    }

    #[test]
    fn focus_row_bounds_ignores_open_menus() {
        // Panels float: row bounds are identical with any menu open.
        let s = snap(true, true);
        let focusable = SettingsRow::focusable(&s);
        let idle = focusable
            .iter()
            .position(|&r| r == SettingsRow::IdleSecs)
            .unwrap();
        let (y, h) = focus_row_bounds(idle, &s, false).unwrap();
        assert!(y > 0.0 && h > 0.0);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn menu_overlay_tracks_open_menu_only() {
        let s = snap(true, true);
        let mut state = State::default();
        // Closed: no overlay.
        assert!(menu_overlay(&state, &s, false, false).is_none());
        // Open on a visible row: overlay present.
        state.settings.menu = Some((SettingsRow::AutoOpen, 0));
        assert!(menu_overlay(&state, &s, false, false).is_some());
        // Open on a row that left the focusable list: no overlay.
        let mut hidden = s.clone();
        hidden.selection_persist = false;
        state.settings.menu = Some((SettingsRow::SelectionMode, 0));
        assert!(menu_overlay(&state, &hidden, false, false).is_none());
    }

    /// Triple agreement: focusable order == layout (panel build) order ==
    /// bounds geometry order, for every sounds/haptics visibility combo.
    /// Every focusable row must resolve `Some` bounds, with strictly
    /// increasing tops matching focus order. Dropdown panels float, so row
    /// geometry never depends on menu state.
    #[test]
    fn triple_agreement_focusable_panel_bounds() {
        // The single table lists every variant exactly once, in display order.
        assert_eq!(SettingsRow::ALL.len(), 19);
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
                // Layout row order never depends on menu state: dropdown
                // panels float over the list instead of living in the flow.
                for menu in [
                    None,
                    Some(SettingsRow::AutoOpen),
                    Some(SettingsRow::SelectionMode),
                    Some(SettingsRow::ImmersiveLayout),
                    Some(SettingsRow::Navbar),
                    Some(SettingsRow::ReducedMotion),
                ] {
                    let layout_rows: Vec<SettingsRow> = layout_items(&s)
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
                                focus_row_bounds(focus, &s, immersive).unwrap_or_else(
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
        let (row_top, row_h) = focus_row_bounds(panel.focus, &s, false).unwrap();
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
    fn reload_scroll_reveals_trailing_row_only_when_dirty() {
        let clean = snap(true, true);
        assert!(
            !SettingsRow::focusable(&clean).contains(&SettingsRow::PerfReload),
            "reload row hidden while prefs match boot"
        );
        // Focus stays on the toggle that dirtied the prefs (PerfVsync here).
        let toggle_focus = SettingsRow::focusable(&clean)
            .iter()
            .position(|&row| row == SettingsRow::PerfVsync)
            .unwrap();
        let panel = SettingsPanel {
            open: true,
            focus: toggle_focus,
            viewport_h: 200.0,
            scroll_y: 0.0,
            ..SettingsPanel::default()
        };
        assert!(
            panel
                .reload_scroll_y(&clean, false, ScrollReveal::Either)
                .is_none(),
            "clean prefs: no reload row, no scroll"
        );
        let mut dirty = clean.clone();
        dirty.perf_vsync = false;
        let rows = SettingsRow::focusable(&dirty);
        assert_eq!(
            rows.last(),
            Some(&SettingsRow::PerfReload),
            "reload row mounts trailing when dirty"
        );
        for immersive in [false, true] {
            let y = panel
                .reload_scroll_y(&dirty, immersive, ScrollReveal::Either)
                .expect("trailing reload row should need a reveal from the top");
            let (row_top, row_h) = focus_row_bounds(rows.len() - 1, &dirty, immersive).unwrap();
            let expected = (row_top + row_h + REVEAL_INSET - 200.0).max(0.0);
            assert!(
                (y - expected).abs() < 0.5,
                "immersive={immersive} y={y} expected={expected}"
            );
            let mut settled = panel.clone();
            settled.scroll_y = y;
            assert!(
                settled
                    .reload_scroll_y(&dirty, immersive, ScrollReveal::Either)
                    .is_none(),
                "immersive={immersive}: settled reveal is a no-op"
            );
        }
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
            confirm_message(SettingsRow::SelectionMode, &base),
            Some(StartMessage::ToggleSelectionModeMenu)
        ));
        assert!(matches!(
            confirm_message(SettingsRow::ImmersiveLayout, &base),
            Some(StartMessage::ToggleLayoutMenu)
        ));
        assert!(matches!(
            confirm_message(SettingsRow::Navbar, &base),
            Some(StartMessage::ToggleNavbarMenu)
        ));
        assert!(matches!(
            confirm_message(SettingsRow::ReducedMotion, &base),
            Some(StartMessage::ToggleReducedMotionMenu)
        ));
        // Toggles always flip, from either state.
        for on in [false, true] {
            let mut s = base.clone();
            s.selection_persist = on;
            assert!(
                matches!(
                    confirm_message(SettingsRow::SelectionPersist, &s),
                    Some(StartMessage::SetSelectionPersist(v)) if v == !on
                ),
                "SelectionPersist confirm from {on}"
            );
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
            s = base.clone();
            s.perf_throttle_immersive = on;
            assert!(
                matches!(
                    confirm_message(SettingsRow::PerfThrottle, &s),
                    Some(StartMessage::SetPerfThrottle(v)) if v == !on
                ),
                "PerfThrottle confirm from {on}"
            );
            s = base.clone();
            s.perf_antialiasing = on;
            assert!(
                matches!(
                    confirm_message(SettingsRow::PerfAntialiasing, &s),
                    Some(StartMessage::SetPerfAntialiasing(v)) if v == !on
                ),
                "PerfAntialiasing confirm from {on}"
            );
            s = base.clone();
            s.perf_vsync = on;
            assert!(
                matches!(
                    confirm_message(SettingsRow::PerfVsync, &s),
                    Some(StartMessage::SetPerfVsync(v)) if v == !on
                ),
                "PerfVsync confirm from {on}"
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

    /// The reload action appears only while renderer prefs differ from boot
    /// (last row), confirms into the reload message, and ignores nudges.
    #[test]
    fn perf_reload_row_shows_only_when_dirty() {
        let clean = snap(true, true);
        let rows = SettingsRow::focusable(&clean);
        assert!(!rows.contains(&SettingsRow::PerfReload));
        assert!(!SettingsRow::PerfReload.visible(&clean));

        let mut dirty = clean.clone();
        dirty.perf_vsync = false;
        assert!(SettingsRow::PerfReload.visible(&dirty));
        let rows = SettingsRow::focusable(&dirty);
        assert_eq!(rows.last(), Some(&SettingsRow::PerfReload));
        assert!(matches!(
            confirm_message(SettingsRow::PerfReload, &dirty),
            Some(StartMessage::ReloadUi)
        ));
        assert!(nudge_message(SettingsRow::PerfReload, &dirty, 1).is_none());
        assert!(nudge_message(SettingsRow::PerfReload, &dirty, -1).is_none());

        // Flipped back to boot: gone again.
        dirty.perf_vsync = true;
        assert!(!SettingsRow::PerfReload.visible(&dirty));
    }

    /// Pinned nudge matrix for the toggle rows: left = off, right = on,
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
        for (navbar, cycled) in [
            (NavbarVisibility::WhenNavigating, NavbarVisibility::AlwaysOn),
            (NavbarVisibility::AlwaysOn, NavbarVisibility::WhenNavigating),
        ] {
            let mut s = base.clone();
            s.navbar = navbar;
            for delta in [1, -1] {
                assert!(
                    matches!(
                        nudge_message(SettingsRow::Navbar, &s, delta),
                        Some(StartMessage::SetNavbarVisibility(v)) if v == cycled
                    ),
                    "Navbar {navbar:?} delta {delta}"
                );
            }
        }
        for (motion, cycled) in [
            (ReducedMotion::System, ReducedMotion::On),
            (ReducedMotion::On, ReducedMotion::Off),
            (ReducedMotion::Off, ReducedMotion::System),
        ] {
            let mut s = base.clone();
            s.reduced_motion = motion;
            for delta in [1, -1] {
                assert!(
                    matches!(
                        nudge_message(SettingsRow::ReducedMotion, &s, delta),
                        Some(StartMessage::SetReducedMotion(v)) if v == cycled
                    ),
                    "ReducedMotion {motion:?} delta {delta}"
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
        for row in [
            SettingsRow::AutoOpen,
            SettingsRow::ImmersiveLayout,
            SettingsRow::Navbar,
            SettingsRow::ReducedMotion,
        ] {
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
            (SettingsRow::PerfThrottle, s.perf_throttle_immersive),
            (SettingsRow::PerfAntialiasing, s.perf_antialiasing),
            (SettingsRow::PerfVsync, s.perf_vsync),
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

    /// Nav-cue split: sound rows cue haptic-only (apply previews sound),
    /// haptics rows cue sound-only (apply previews haptics), all other rows
    /// keep the full cue. New feedback rows with apply previews must extend
    /// this table.
    #[test]
    fn nav_cue_split_covers_all_rows() {
        for row in SettingsRow::ALL {
            match row {
                SettingsRow::Sounds | SettingsRow::SoundVolume => assert_eq!(
                    settings_nav_cue(row),
                    SettingsCue::HapticOnly,
                    "{row:?} apply previews sound, nav must be haptic-only"
                ),
                SettingsRow::Haptics | SettingsRow::HapticsStrength => assert_eq!(
                    settings_nav_cue(row),
                    SettingsCue::SoundOnly,
                    "{row:?} apply previews haptics, nav must be sound-only"
                ),
                _ => assert_eq!(
                    settings_nav_cue(row),
                    SettingsCue::Full,
                    "{row:?} has no apply preview, nav must be full"
                ),
            }
        }
    }
}
