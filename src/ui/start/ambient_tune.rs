//! In-immersive ambient tuning panel (debug-only "tune ambience").
//!
//! Centered modal with the same ambient sliders as Settings → Diagnostics,
//! drivable by pad (Up/Down focus, Left/Right nudge, Cross confirm, Circle
//! done) and mouse (drag sliders, click buttons). Values read the live
//! process-wide tuning atomics directly; commits route through
//! [`StartMessage`](crate::ui::start::view::StartMessage) so pad and mouse
//! share one apply path in the shell.

use crate::ui::shader::ambient;
use crate::ui::start::view::{StartMessage, State, scroll_y_to_reveal_bounds};
use crate::ui::theme;
use iced::font::Weight;
use iced::widget::{column, container, mouse_area, row, scrollable, slider, stack, text};
use iced::{Alignment, Element, Fill, Font, Length};

/// Fixed row height so focus-follow scroll math stays exact.
pub const TUNE_ROW_H: f32 = 64.0;
/// Panel card width; height caps and scrolls (14 rows overflow a modal).
pub const TUNE_PANEL_W: f32 = 560.0;
pub const TUNE_PANEL_MAX_H: f32 = 560.0;
/// Scrollable widget id for the tuning list.
pub fn tune_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("start-ambient-tune-scroll")
}

/// One tunable ambient slider. Ranges/steps mirror the Diagnostics sliders
/// (same clamps as `ui::shader::ambient`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmbientTuneParam {
    PulseFreq,
    PulseMin,
    PulseMax,
    GlowHue,
    OrbSpeed,
    OrbMinX,
    OrbMaxX,
    OrbMinY,
    OrbMaxY,
    VeilFreq,
    Grain,
    TimeScale,
}

impl AmbientTuneParam {
    pub const ALL: [Self; 12] = [
        Self::PulseFreq,
        Self::PulseMin,
        Self::PulseMax,
        Self::GlowHue,
        Self::OrbSpeed,
        Self::OrbMinX,
        Self::OrbMaxX,
        Self::OrbMinY,
        Self::OrbMaxY,
        Self::VeilFreq,
        Self::Grain,
        Self::TimeScale,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::PulseFreq => "Pulse speed",
            Self::PulseMin => "Pulse min",
            Self::PulseMax => "Pulse max",
            Self::GlowHue => "Glow hue",
            Self::OrbSpeed => "Orb speed",
            Self::OrbMinX => "Orb min X",
            Self::OrbMaxX => "Orb max X",
            Self::OrbMinY => "Orb min Y",
            Self::OrbMaxY => "Orb max Y",
            Self::VeilFreq => "Veil pulse speed",
            Self::Grain => "Grain",
            Self::TimeScale => "Time scale",
        }
    }

    pub fn range(self) -> std::ops::RangeInclusive<f32> {
        match self {
            Self::PulseFreq => 0.0..=5.0,
            Self::PulseMin | Self::PulseMax => 0.0..=0.6,
            Self::GlowHue => 0.0..=360.0,
            Self::OrbSpeed => 0.0..=3.0,
            Self::OrbMinX | Self::OrbMaxX | Self::OrbMinY | Self::OrbMaxY => -1.0..=1.0,
            Self::VeilFreq => 0.0..=8.0,
            Self::Grain => 0.0..=5.0,
            Self::TimeScale => 0.0..=4.0,
        }
    }

    pub fn step(self) -> f32 {
        match self {
            Self::PulseFreq | Self::VeilFreq | Self::OrbSpeed => 0.05,
            Self::PulseMin | Self::PulseMax => 0.005,
            Self::GlowHue => 1.0,
            Self::OrbMinX | Self::OrbMaxX | Self::OrbMinY | Self::OrbMaxY => 0.01,
            Self::Grain | Self::TimeScale => 0.05,
        }
    }

    /// Live value from the process-wide tuning atomics.
    pub fn read(self) -> f32 {
        match self {
            Self::PulseFreq => ambient::pulse_freq(),
            Self::PulseMin => ambient::pulse_min(),
            Self::PulseMax => ambient::pulse_max(),
            Self::GlowHue => ambient::glow_hue(),
            Self::OrbSpeed => ambient::orb_speed(),
            Self::OrbMinX => ambient::orb_min_x(),
            Self::OrbMaxX => ambient::orb_max_x(),
            Self::OrbMinY => ambient::orb_min_y(),
            Self::OrbMaxY => ambient::orb_max_y(),
            Self::VeilFreq => ambient::veil_freq(),
            Self::Grain => ambient::grain_gain(),
            Self::TimeScale => ambient::time_scale(),
        }
    }

    pub fn value_label(self) -> String {
        let v = self.read();
        match self {
            Self::PulseFreq | Self::VeilFreq | Self::OrbSpeed => {
                if v < 0.01 {
                    "frozen".to_string()
                } else {
                    format!("{v:.2}/s")
                }
            }
            Self::PulseMin | Self::PulseMax => format!("{v:.3}"),
            Self::GlowHue => format!("{v:.0}°"),
            Self::OrbMinX | Self::OrbMaxX | Self::OrbMinY | Self::OrbMaxY => format!("{v:.2}"),
            Self::Grain => format!("{v:.2}×"),
            Self::TimeScale => {
                if v < 0.01 {
                    "frozen".to_string()
                } else {
                    format!("{v:.2}×")
                }
            }
        }
    }

    /// Pad nudge from the live value; `None` when clamped back onto the
    /// current value (at a limit). Steps/rounding never drift from the
    /// Diagnostics sliders: both commit absolute values into the same
    /// clamped atomics.
    pub fn nudge(self, delta: isize) -> Option<f32> {
        let range = self.range();
        let next = (self.read() + delta as f32 * self.step()).clamp(*range.start(), *range.end());
        (next != self.read()).then_some(next)
    }
}

/// Focusable rows in the tuning panel: one per slider, then Reset, then Done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuneRow {
    Param(AmbientTuneParam),
    Reset,
    Done,
}

impl TuneRow {
    pub const ALL: [Self; 14] = [
        Self::Param(AmbientTuneParam::PulseFreq),
        Self::Param(AmbientTuneParam::PulseMin),
        Self::Param(AmbientTuneParam::PulseMax),
        Self::Param(AmbientTuneParam::GlowHue),
        Self::Param(AmbientTuneParam::OrbSpeed),
        Self::Param(AmbientTuneParam::OrbMinX),
        Self::Param(AmbientTuneParam::OrbMaxX),
        Self::Param(AmbientTuneParam::OrbMinY),
        Self::Param(AmbientTuneParam::OrbMaxY),
        Self::Param(AmbientTuneParam::VeilFreq),
        Self::Param(AmbientTuneParam::Grain),
        Self::Param(AmbientTuneParam::TimeScale),
        Self::Reset,
        Self::Done,
    ];

    pub fn count() -> usize {
        Self::ALL.len()
    }
}

/// Scroll offset revealing the focused row (fixed heights ⇒ exact).
/// `None` when already fully visible.
pub fn tune_reveal_y(focus: usize, scroll_y: f32, viewport_h: f32) -> Option<f32> {
    let viewport_h = if viewport_h <= 1.0 { 480.0 } else { viewport_h };
    let row_top = focus as f32 * TUNE_ROW_H;
    scroll_y_to_reveal_bounds(
        row_top,
        TUNE_ROW_H,
        scroll_y,
        viewport_h,
        crate::ui::start::view::ScrollReveal::Either,
    )
}

fn focus_ring(focused: bool) -> impl Fn(&iced::Theme) -> iced::widget::container::Style {
    move |_theme| {
        if focused {
            iced::widget::container::Style {
                background: Some(iced::Background::Color(theme::alpha(theme::ACCENT, 0.12))),
                border: iced::Border {
                    color: theme::alpha(theme::ACCENT, 0.55),
                    width: 1.0,
                    radius: 8.0.into(),
                },
                ..iced::widget::container::Style::default()
            }
        } else {
            iced::widget::container::Style::default()
        }
    }
}

fn tune_slider_row(param: AmbientTuneParam, focused: bool) -> Element<'static, StartMessage> {
    let value = param.read();
    let range = param.range();
    container(
        column![
            row![
                text(param.label()).size(14.0).color(theme::INK).width(Fill),
                text(param.value_label()).size(12.0).color(theme::MUTED),
            ]
            .align_y(Alignment::Center),
            slider(range, value, move |v| {
                StartMessage::SetAmbientTune(param, v)
            })
            .step(param.step())
            .style(theme::slider),
        ]
        .spacing(4)
        .width(Fill),
    )
    .padding(8)
    .width(Fill)
    .height(Length::Fixed(TUNE_ROW_H))
    .style(focus_ring(focused))
    .into()
}

fn tune_button_row(
    label: &'static str,
    focused: bool,
    message: StartMessage,
) -> Element<'static, StartMessage> {
    container(
        mouse_area(
            container(text(label).size(14.0).color(theme::INK))
                .width(Fill)
                .height(Fill)
                .center_x(Fill)
                .center_y(Fill),
        )
        .on_press(message)
        .interaction(iced::mouse::Interaction::Pointer),
    )
    .padding(8)
    .width(Fill)
    .height(Length::Fixed(TUNE_ROW_H))
    .style(focus_ring(focused))
    .into()
}

/// Centered tuning modal over the live ambient background. No dim wash: the
/// panel tunes the atmosphere itself, so it must stay WYSIWYG.
pub fn tuning_overlay(state: &State) -> Element<'_, StartMessage> {
    let focus = state
        .ambient_tune_focus
        .min(TuneRow::count().saturating_sub(1));
    let mut items = column![].spacing(4).width(Fill);
    for (i, row) in TuneRow::ALL.iter().copied().enumerate() {
        let focused = i == focus;
        items = items.push(match row {
            TuneRow::Param(param) => tune_slider_row(param, focused),
            TuneRow::Reset => {
                tune_button_row("Reset ambience", focused, StartMessage::ResetAmbientTuning)
            }
            TuneRow::Done => tune_button_row("Done", focused, StartMessage::CloseAmbientTuning),
        });
    }
    let list = scrollable(items)
        .id(tune_scroll_id())
        .on_scroll(|viewport| {
            StartMessage::AmbientTuneScrolled(
                viewport.absolute_offset().y,
                viewport.bounds().height,
            )
        })
        .width(Fill)
        .height(Fill)
        .style(theme::scrollbar);

    let card = container(theme::framed(
        column![
            text("Tune ambience")
                .size(20.0)
                .font(Font {
                    weight: Weight::Bold,
                    ..Font::DEFAULT
                })
                .color(theme::INK),
            text("Up/Down select · Left/Right adjust · R3 hide UI · Circle done")
                .size(12.0)
                .color(theme::DIM),
            list,
        ]
        .spacing(8)
        .width(Fill)
        .height(Fill),
    ))
    .padding(16)
    .width(Length::Fixed(TUNE_PANEL_W))
    .max_height(TUNE_PANEL_MAX_H);

    container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .into()
}

/// Stack the tuning modal over immersive chrome (same layer shape as the
/// settings drawer so the stage never remounts while it opens/closes).
pub fn tuning_layer<'a>(
    chrome: Element<'a, StartMessage>,
    state: &'a State,
) -> Element<'a, StartMessage> {
    if state.ambient_tune_open {
        stack![chrome, tuning_overlay(state)]
            .width(Fill)
            .height(Fill)
            .into()
    } else {
        chrome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tune_rows_have_unique_labels() {
        // Labels double as the pad-focus readout; duplicates would be ambiguous.
        let mut labels: Vec<&str> = AmbientTuneParam::ALL.iter().map(|p| p.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), AmbientTuneParam::ALL.len());
        assert_eq!(TuneRow::count(), AmbientTuneParam::ALL.len() + 2);
    }

    #[test]
    fn nudge_stays_in_range_and_reports_limits() {
        ambient::with_tune_test_lock(|| {
            for param in AmbientTuneParam::ALL {
                let range = param.range();
                for _ in 0..10_000 {
                    let Some(next) = param.nudge(1) else {
                        break;
                    };
                    assert!(
                        next >= *range.start() && next <= *range.end(),
                        "{param:?} escaped {range:?} with {next}"
                    );
                    // Drive the live value forward so the walk terminates.
                    apply_for_test(param, next);
                }
                // Pinned at the ceiling: further nudges are no-ops.
                assert_eq!(param.nudge(1), None);
                assert_eq!(param.nudge(-1).is_some(), param.read() > *range.start());
            }
        });
    }

    #[cfg(debug_assertions)]
    fn apply_for_test(param: AmbientTuneParam, value: f32) {
        match param {
            AmbientTuneParam::PulseFreq => ambient::set_pulse_freq(value),
            AmbientTuneParam::PulseMin => ambient::set_pulse_min(value),
            AmbientTuneParam::PulseMax => ambient::set_pulse_max(value),
            AmbientTuneParam::GlowHue => ambient::set_glow_hue(value),
            AmbientTuneParam::OrbSpeed => ambient::set_orb_speed(value),
            AmbientTuneParam::OrbMinX => ambient::set_orb_min_x(value),
            AmbientTuneParam::OrbMaxX => ambient::set_orb_max_x(value),
            AmbientTuneParam::OrbMinY => ambient::set_orb_min_y(value),
            AmbientTuneParam::OrbMaxY => ambient::set_orb_max_y(value),
            AmbientTuneParam::VeilFreq => ambient::set_veil_freq(value),
            AmbientTuneParam::Grain => ambient::set_grain_gain(value),
            AmbientTuneParam::TimeScale => ambient::set_time_scale(value),
        }
    }

    #[test]
    fn reveal_math_pins_focused_row() {
        // Fixed heights: row tops land exactly on multiples of TUNE_ROW_H.
        assert_eq!(tune_reveal_y(0, 0.0, 480.0), None);
        // Second row below a one-row viewport must scroll by exactly one row.
        assert_eq!(tune_reveal_y(1, 0.0, TUNE_ROW_H), Some(TUNE_ROW_H));
        // Far focus below a short viewport reveals its bottom edge.
        assert_eq!(
            tune_reveal_y(5, 0.0, 200.0),
            Some(5.0 * TUNE_ROW_H + TUNE_ROW_H - 200.0)
        );
    }

    #[test]
    fn tune_panel_composes_with_live_tuning() {
        use iced::widget::space;
        ambient::with_tune_test_lock(|| {
            ambient::set_pulse_freq(0.5);
            ambient::set_orb_speed(1.2);
            let state = State::default();
            drop(tuning_overlay(&state));
            drop(tuning_layer(
                space().width(Fill).height(Fill).into(),
                &state,
            ));
        });
    }
}
