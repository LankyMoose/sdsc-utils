//! In-window Start Screen settings (Options): compact modal / immersive right drawer.

use crate::domain::gesture::GestureControl;
use crate::persist::prefs::{
    START_SCREEN_IDLE_SECS_STEPS, START_SCREEN_SLEEP_SECS_STEPS, format_timeout_duration,
    min_sleep_secs_for_idle, secs_step_index,
};
use crate::ui::layout as window_layout;
use crate::ui::start::view::{ScrollReveal, StartMessage, State, scroll_y_to_reveal_bounds};
use crate::ui::theme;
use iced::font::Weight;
use iced::widget::{button, column, container, row, scrollable, slider, space, text, toggler};
use iced::{Alignment, Element, Fill, Font, Length, Padding};
use std::time::{Duration, Instant};

/// Immersive settings drawer content width (full height).
pub const DRAWER_W: f32 = 400.0;
/// Compact modal width (~window × 0.75).
pub const MODAL_W: f32 = 576.0;
const DRAWER_ANIM_MS: u64 = 220;
const ROW_PAD: f32 = 10.0;
const COL_GAP: f32 = 6.0;
/// Gap between scrollable content and the embedded scrollbar (matches Configure/Popup).
const SCROLL_GAP: f32 = 8.0;
/// Padding inside the settings scrollable content (must match `panel_view` list padding).
/// Kept in the same ballpark as [`REVEAL_INSET`] — just enough for the focus ring.
const CONTENT_PAD_Y: f32 = 8.0;
const CONTENT_PAD_X: f32 = 4.0;
/// Gap between the static title bar and the scrollable list.
const HEADER_LIST_GAP: f32 = 8.0;
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
    pub usb_controllers: bool,
    pub always_immersive: bool,
    pub inactive_secs: u32,
    pub sleep_secs: u32,
    pub inactive_dim_percent: u8,
    pub gesture: Vec<GestureControl>,
    pub gesture_recording: bool,
    pub gesture_recording_live: String,
    pub sounds_enabled: bool,
    pub sound_volume: u8,
    pub haptics_enabled: bool,
    pub haptics_strength: u8,
}

/// Focusable rows in the Start settings list (dynamic when sounds/haptics off).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    UsbControllers,
    AlwaysImmersive,
    IdleSecs,
    SleepSecs,
    IdleDim,
    GestureRecord,
    GestureReset,
    Sounds,
    SoundVolume,
    Haptics,
    HapticsStrength,
}

impl SettingsRow {
    pub fn focusable(snapshot: &StartSettingsSnapshot) -> Vec<Self> {
        let mut rows = vec![
            Self::UsbControllers,
            Self::AlwaysImmersive,
            Self::IdleSecs,
            Self::SleepSecs,
            Self::IdleDim,
            Self::GestureRecord,
        ];
        if !snapshot.gesture_recording {
            rows.push(Self::GestureReset);
        }
        rows.push(Self::Sounds);
        if snapshot.sounds_enabled {
            rows.push(Self::SoundVolume);
        }
        rows.push(Self::Haptics);
        if snapshot.haptics_enabled {
            rows.push(Self::HapticsStrength);
        }
        rows
    }

    pub fn is_slider(self) -> bool {
        matches!(
            self,
            Self::IdleSecs
                | Self::SleepSecs
                | Self::IdleDim
                | Self::SoundVolume
                | Self::HapticsStrength
        )
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
}

impl SettingsPanel {
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
    match row {
        SettingsRow::UsbControllers => {
            Some(StartMessage::SetUsbControllers(!snapshot.usb_controllers))
        }
        SettingsRow::AlwaysImmersive => {
            Some(StartMessage::SetAlwaysImmersive(!snapshot.always_immersive))
        }
        SettingsRow::Sounds => Some(StartMessage::SetSounds(!snapshot.sounds_enabled)),
        SettingsRow::Haptics => Some(StartMessage::SetHaptics(!snapshot.haptics_enabled)),
        SettingsRow::GestureRecord => {
            if snapshot.gesture_recording {
                Some(StartMessage::CancelGestureRecord)
            } else {
                Some(StartMessage::StartGestureRecord)
            }
        }
        SettingsRow::GestureReset => Some(StartMessage::ResetStartGesture),
        SettingsRow::IdleSecs
        | SettingsRow::SleepSecs
        | SettingsRow::IdleDim
        | SettingsRow::SoundVolume
        | SettingsRow::HapticsStrength => None,
    }
}

/// Left/Right on the focused row (`delta` −1 = left, +1 = right).
///
/// Sliders step; toggles set off on left and on on right (no-op if already there).
pub fn nudge_message(
    row: SettingsRow,
    snapshot: &StartSettingsSnapshot,
    delta: i8,
) -> Option<StartMessage> {
    let step = if delta < 0 { -1isize } else { 1isize };
    let want_on = delta > 0;
    match row {
        SettingsRow::UsbControllers => {
            if snapshot.usb_controllers == want_on {
                None
            } else {
                Some(StartMessage::SetUsbControllers(want_on))
            }
        }
        SettingsRow::AlwaysImmersive => {
            if snapshot.always_immersive == want_on {
                None
            } else {
                Some(StartMessage::SetAlwaysImmersive(want_on))
            }
        }
        SettingsRow::Sounds => {
            if snapshot.sounds_enabled == want_on {
                None
            } else {
                Some(StartMessage::SetSounds(want_on))
            }
        }
        SettingsRow::Haptics => {
            if snapshot.haptics_enabled == want_on {
                None
            } else {
                Some(StartMessage::SetHaptics(want_on))
            }
        }
        SettingsRow::IdleSecs => {
            let idx =
                secs_step_index(snapshot.inactive_secs, START_SCREEN_IDLE_SECS_STEPS) as isize;
            let next = (idx + step).clamp(0, START_SCREEN_IDLE_SECS_STEPS.len() as isize - 1);
            let secs = START_SCREEN_IDLE_SECS_STEPS[next as usize];
            if secs == snapshot.inactive_secs {
                None
            } else {
                Some(StartMessage::SetInactiveSecs(secs))
            }
        }
        SettingsRow::SleepSecs => {
            let min_sleep = min_sleep_secs_for_idle(snapshot.inactive_secs);
            let choices: Vec<u32> = START_SCREEN_SLEEP_SECS_STEPS
                .iter()
                .copied()
                .filter(|&s| s >= min_sleep)
                .collect();
            if choices.is_empty() {
                return None;
            }
            let idx = secs_step_index(snapshot.sleep_secs, &choices) as isize;
            let next = (idx + step).clamp(0, choices.len() as isize - 1);
            let secs = choices[next as usize];
            if secs == snapshot.sleep_secs {
                None
            } else {
                Some(StartMessage::SetSleepSecs(secs))
            }
        }
        SettingsRow::IdleDim => {
            let cur = i16::from(snapshot.inactive_dim_percent);
            let next = (cur + step as i16 * 5).clamp(0, 100) as u8;
            if next == snapshot.inactive_dim_percent {
                None
            } else {
                Some(StartMessage::SetInactiveDimPercent(next))
            }
        }
        SettingsRow::SoundVolume => {
            let cur = i16::from(snapshot.sound_volume);
            let next = (cur + step as i16 * 5).clamp(0, 100) as u8;
            if next == snapshot.sound_volume {
                None
            } else {
                Some(StartMessage::SetSoundVolume(next))
            }
        }
        SettingsRow::HapticsStrength => {
            let cur = i16::from(snapshot.haptics_strength);
            let next = (cur + step as i16 * 5).clamp(0, 100) as u8;
            if next == snapshot.haptics_strength {
                None
            } else {
                Some(StartMessage::SetHapticsStrength(next))
            }
        }
        _ => None,
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
pub fn focus_row_bounds(
    focus: usize,
    snapshot: &StartSettingsSnapshot,
    immersive: bool,
) -> Option<(f32, f32)> {
    let rows = SettingsRow::focusable(snapshot);
    let target = *rows.get(focus)?;
    let ty = TypeScale::for_mode(immersive);
    // Content padding is inside the scrollable — offsets are absolute in that content.
    // Title lives in the static header (not scrolled).
    let mut y = CONTENT_PAD_Y;
    let mut gap = |h: f32| {
        let top = y;
        y += h + COL_GAP;
        top
    };

    // Match panel_view scroll stacking order (column spacing = COL_GAP between children).
    gap(ty.section); // Opening
    if target == SettingsRow::UsbControllers {
        return Some((gap(toggle_row_h(ty.toggle)), toggle_row_h(ty.toggle)));
    }
    gap(toggle_row_h(ty.toggle));
    gap(ty.helper); // USB helper
    gap(section_rule_h());
    gap(ty.section); // Immersive
    if target == SettingsRow::AlwaysImmersive {
        return Some((gap(toggle_row_h(ty.toggle)), toggle_row_h(ty.toggle)));
    }
    gap(toggle_row_h(ty.toggle));
    if target == SettingsRow::IdleSecs {
        return Some((gap(slider_row_h(ty.helper)), slider_row_h(ty.helper)));
    }
    gap(slider_row_h(ty.helper));
    if target == SettingsRow::SleepSecs {
        return Some((gap(slider_row_h(ty.helper)), slider_row_h(ty.helper)));
    }
    gap(slider_row_h(ty.helper));
    if target == SettingsRow::IdleDim {
        return Some((gap(slider_row_h(ty.helper)), slider_row_h(ty.helper)));
    }
    gap(slider_row_h(ty.helper));
    gap(section_rule_h());
    gap(ty.section); // Gesture
    gap(ty.helper); // chord label
    if snapshot.gesture_recording {
        gap(ty.helper); // holding…
        if target == SettingsRow::GestureRecord {
            return Some((gap(action_row_h(ty.body)), action_row_h(ty.body)));
        }
        gap(action_row_h(ty.body));
    } else {
        if target == SettingsRow::GestureRecord {
            return Some((gap(action_row_h(ty.body)), action_row_h(ty.body)));
        }
        gap(action_row_h(ty.body));
        if target == SettingsRow::GestureReset {
            return Some((gap(action_row_h(ty.body)), action_row_h(ty.body)));
        }
        gap(action_row_h(ty.body));
    }
    gap(section_rule_h());
    gap(ty.section); // Feedback
    if target == SettingsRow::Sounds {
        return Some((gap(toggle_row_h(ty.toggle)), toggle_row_h(ty.toggle)));
    }
    gap(toggle_row_h(ty.toggle));
    if snapshot.sounds_enabled {
        if target == SettingsRow::SoundVolume {
            return Some((gap(slider_row_h(ty.helper)), slider_row_h(ty.helper)));
        }
        gap(slider_row_h(ty.helper));
    }
    if target == SettingsRow::Haptics {
        return Some((gap(toggle_row_h(ty.toggle)), toggle_row_h(ty.toggle)));
    }
    gap(toggle_row_h(ty.toggle));
    if snapshot.haptics_enabled && target == SettingsRow::HapticsStrength {
        return Some((gap(slider_row_h(ty.helper)), slider_row_h(ty.helper)));
    }
    None
}

fn toggle_row_h(toggle: f32) -> f32 {
    ROW_PAD * 2.0 + toggle.max(22.0)
}

fn slider_row_h(label: f32) -> f32 {
    // Label + gap + slider track (iced default is a bit taller than a tight 22px guess).
    ROW_PAD * 2.0 + label + 4.0 + 26.0
}

fn action_row_h(body: f32) -> f32 {
    ROW_PAD * 2.0 + body + 14.0
}

fn section_rule_h() -> f32 {
    1.0 + 16.0
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
        container(space())
            .width(Fill)
            .height(Length::Fixed(1.0))
            .style(theme::configure_header_rule),
    ]
    .spacing(10)
    .width(Fill);

    let mut items = column![].spacing(COL_GAP).width(Fill);
    items = items.push(section_label("Opening", ty.section));
    items = items.push(toggle_row(
        "USB controllers",
        snapshot.usb_controllers,
        focus_rows.get(focus) == Some(&SettingsRow::UsbControllers),
        ty,
        StartMessage::SetUsbControllers,
    ));
    items = items.push(
        text("When off, only Bluetooth opens and closes the start screen.")
            .size(ty.helper)
            .color(theme::MUTED),
    );

    items = items.push(section_rule());
    items = items.push(section_label("Immersive", ty.section));
    items = items.push(toggle_row(
        "Always immersive",
        snapshot.always_immersive,
        focus_rows.get(focus) == Some(&SettingsRow::AlwaysImmersive),
        ty,
        StartMessage::SetAlwaysImmersive,
    ));

    let idle_secs = snapshot.inactive_secs;
    let idle_idx = secs_step_index(idle_secs, START_SCREEN_IDLE_SECS_STEPS);
    items = items.push(slider_row(
        format!("Idle after {}", format_timeout_duration(idle_secs)),
        focus_rows.get(focus) == Some(&SettingsRow::IdleSecs),
        ty,
        0.0..=(START_SCREEN_IDLE_SECS_STEPS.len().saturating_sub(1) as f32),
        idle_idx as f32,
        |value| {
            let idx =
                (value.round() as usize).min(START_SCREEN_IDLE_SECS_STEPS.len().saturating_sub(1));
            StartMessage::SetInactiveSecs(START_SCREEN_IDLE_SECS_STEPS[idx])
        },
    ));

    let sleep_secs = snapshot.sleep_secs;
    let min_sleep = min_sleep_secs_for_idle(idle_secs);
    let sleep_choices: Vec<u32> = START_SCREEN_SLEEP_SECS_STEPS
        .iter()
        .copied()
        .filter(|&s| s >= min_sleep)
        .collect();
    let sleep_choices = if sleep_choices.is_empty() {
        vec![*START_SCREEN_SLEEP_SECS_STEPS.last().unwrap_or(&min_sleep)]
    } else {
        sleep_choices
    };
    let sleep_idx = secs_step_index(sleep_secs, &sleep_choices);
    let sleep_max = sleep_choices.len().saturating_sub(1) as f32;
    let sleep_choices_for_msg = sleep_choices.clone();
    items = items.push(slider_row(
        format!("Sleep after {}", format_timeout_duration(sleep_secs)),
        focus_rows.get(focus) == Some(&SettingsRow::SleepSecs),
        ty,
        0.0..=sleep_max,
        sleep_idx as f32,
        move |value| {
            let idx = (value.round() as usize).min(sleep_choices_for_msg.len().saturating_sub(1));
            StartMessage::SetSleepSecs(sleep_choices_for_msg[idx])
        },
    ));

    let dim = snapshot.inactive_dim_percent;
    items = items.push(slider_row(
        format!("Idle dim {dim}%"),
        focus_rows.get(focus) == Some(&SettingsRow::IdleDim),
        ty,
        0.0..=100.0,
        f32::from(dim),
        |value| StartMessage::SetInactiveDimPercent(value.round() as u8),
    ));

    items = items.push(section_rule());
    items = items.push(section_label("Gesture", ty.section));
    items = items.push(
        text(crate::ui::start::gesture::format_gesture(&snapshot.gesture))
            .size(ty.helper)
            .color(theme::MUTED),
    );
    if snapshot.gesture_recording {
        items = items.push(
            text(if snapshot.gesture_recording_live.is_empty() {
                "Hold combo, then release…".to_string()
            } else {
                format!("Holding: {}", snapshot.gesture_recording_live)
            })
            .size(ty.helper)
            .color(theme::ACCENT),
        );
        items = items.push(action_row(
            "Cancel recording",
            focus_rows.get(focus) == Some(&SettingsRow::GestureRecord),
            ty,
            StartMessage::CancelGestureRecord,
        ));
    } else {
        items = items.push(action_row(
            "Record",
            focus_rows.get(focus) == Some(&SettingsRow::GestureRecord),
            ty,
            StartMessage::StartGestureRecord,
        ));
        items = items.push(action_row(
            "Reset to default",
            focus_rows.get(focus) == Some(&SettingsRow::GestureReset),
            ty,
            StartMessage::ResetStartGesture,
        ));
    }

    items = items.push(section_rule());
    items = items.push(section_label("Feedback", ty.section));
    items = items.push(toggle_row(
        "UI sounds",
        snapshot.sounds_enabled,
        focus_rows.get(focus) == Some(&SettingsRow::Sounds),
        ty,
        StartMessage::SetSounds,
    ));
    if snapshot.sounds_enabled {
        let volume = snapshot.sound_volume;
        items = items.push(slider_row(
            format!("Volume {volume}%"),
            focus_rows.get(focus) == Some(&SettingsRow::SoundVolume),
            ty,
            0.0..=100.0,
            f32::from(volume),
            |value| StartMessage::SetSoundVolume(value.round() as u8),
        ));
    }
    items = items.push(toggle_row(
        "Controller haptics",
        snapshot.haptics_enabled,
        focus_rows.get(focus) == Some(&SettingsRow::Haptics),
        ty,
        StartMessage::SetHaptics,
    ));
    if snapshot.haptics_enabled {
        let strength = snapshot.haptics_strength;
        items = items.push(slider_row(
            format!("Strength {strength}%"),
            focus_rows.get(focus) == Some(&SettingsRow::HapticsStrength),
            ty,
            0.0..=100.0,
            f32::from(strength),
            |value| StartMessage::SetHapticsStrength(value.round() as u8),
        ));
    }

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

    let revealed = crate::ui::start::reveal::width_reveal(panel, DRAWER_W, w).height(Fill);
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

fn section_rule() -> Element<'static, StartMessage> {
    container(
        container(space())
            .width(Fill)
            .height(Length::Fixed(1.0))
            .style(theme::configure_header_rule),
    )
    .padding(Padding {
        top: 8.0,
        right: 0.0,
        bottom: 8.0,
        left: 0.0,
    })
    .width(Fill)
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
            toggler(checked).size(ty.toggle).on_toggle(message),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding(ROW_PAD)
    .width(Fill)
    .style(focus_style(focused))
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
            slider(range, value, on_change).step(step),
        ]
        .spacing(4)
        .width(Fill),
    )
    .padding(ROW_PAD)
    .width(Fill)
    .style(focus_style(focused))
    .into()
}

fn action_row(
    label: &'static str,
    focused: bool,
    ty: TypeScale,
    message: StartMessage,
) -> Element<'static, StartMessage> {
    container(
        button(text(label).size(ty.body))
            .padding([5, 8])
            .width(Fill)
            .on_press(message)
            .style(theme::chip(focused)),
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
            usb_controllers: true,
            always_immersive: false,
            inactive_secs: 30,
            sleep_secs: 120,
            inactive_dim_percent: 50,
            gesture: crate::domain::gesture::default_gesture(),
            gesture_recording: false,
            gesture_recording_live: String::new(),
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
        assert!(nudge_message(SettingsRow::UsbControllers, &s, 1).is_none());
        let msg = nudge_message(SettingsRow::UsbControllers, &s, -1).unwrap();
        assert!(matches!(msg, StartMessage::SetUsbControllers(false)));
        assert!(nudge_message(SettingsRow::AlwaysImmersive, &s, -1).is_none());
        let msg = nudge_message(SettingsRow::AlwaysImmersive, &s, 1).unwrap();
        assert!(matches!(msg, StartMessage::SetAlwaysImmersive(true)));
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
}
