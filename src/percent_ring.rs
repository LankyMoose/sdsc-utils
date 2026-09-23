//! Circular battery-percent outline used by overlay toasts and the controller popup.

use crate::theme;
use iced::mouse;
use iced::widget::canvas as canvas_widget;
use iced::widget::canvas::{self, Frame, Geometry, Path};
use iced::widget::{column, container, stack, text};
use iced::{
    Alignment, Color, Degrees, Element, Fill, Length, Padding, Pixels, Radians, Rectangle,
    Renderer, Theme,
};

/// Reference size used by overlay toasts; stroke and type scale from this.
pub const TOAST_SIZE: f32 = 76.0;
/// Size that fits a controller-popup row.
pub const POPUP_SIZE: f32 = 72.0;

const REF_SIZE: f32 = 60.0;
const REF_STROKE: f32 = 3.5;
const REF_TEXT: f32 = 20.0;
const REF_TEXT_FULL: f32 = 16.0;
const REF_ETA_TEXT: f32 = 11.0;
/// When ETA is present, shrink the percent a bit so the pair fits inside the ring.
const ETA_PERCENT_SCALE: f32 = 0.78;
/// Gap between percent and ETA as a fraction of ETA size.
const ETA_GAP_FRAC: f32 = 0.12;
/// Nudge the two-line block slightly up so ETA isn't tight against the lower arc.
const ETA_BLOCK_NUDGE_Y: f32 = 0.04;

/// Draws a circular outline filled clockwise to `percent`, with the value centered inside.
/// When `eta` is set (e.g. `~3h 30m`), it sits under the percent inside the ring.
///
/// Labels use iced `text` widgets (not canvas `fill_text`) so glyph color stays uniform.
pub fn percent_ring<'a, Message: 'a>(
    percent: u8,
    color: Color,
    size: f32,
    eta: Option<String>,
) -> Element<'a, Message> {
    let percent = percent.min(100);
    let has_eta = eta.is_some();
    let percent_size = text_size(size, percent, has_eta);
    let eta_size = eta_text_size(size);

    let ring = canvas_widget(PercentRing {
        percent,
        color,
        size,
    })
    .width(Length::Fixed(size))
    .height(Length::Fixed(size));

    // Absolute line height = font size so layout boxes match glyph height (default ~1.3
    // relative leading made the percent line tall and shoved ETA into the lower arc).
    let percent_label = text(format!("{percent}%"))
        .size(percent_size)
        .line_height(Pixels(percent_size))
        .color(theme::INK);

    let labels: Element<'a, Message> = if let Some(eta) = eta {
        let gap = eta_size * ETA_GAP_FRAC;
        column![
            percent_label,
            text(eta)
                .size(eta_size)
                .line_height(Pixels(eta_size))
                .color(theme::MUTED),
        ]
        .spacing(gap)
        .align_x(Alignment::Center)
        .into()
    } else {
        percent_label.into()
    };

    let nudge = if has_eta {
        size * ETA_BLOCK_NUDGE_Y
    } else {
        0.0
    };
    let labels = container(labels)
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .center_x(Fill)
        .center_y(Fill)
        .padding(Padding::ZERO.bottom(nudge));

    stack![ring, labels]
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .into()
}

fn text_size(ring_size: f32, percent: u8, has_eta: bool) -> f32 {
    let base = if percent >= 100 {
        REF_TEXT_FULL
    } else {
        REF_TEXT
    };
    let scale = if has_eta { ETA_PERCENT_SCALE } else { 1.0 };
    base * (ring_size / REF_SIZE) * scale
}

fn eta_text_size(ring_size: f32) -> f32 {
    REF_ETA_TEXT * (ring_size / REF_SIZE)
}

#[derive(Clone)]
struct PercentRing {
    percent: u8,
    color: Color,
    size: f32,
}

impl PercentRing {
    fn stroke_width(&self) -> f32 {
        REF_STROKE * (self.size / REF_SIZE)
    }
}

impl<Message> canvas::Program<Message> for PercentRing {
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
        let center = frame.center();
        let stroke_width = self.stroke_width();
        let radius = (bounds.width.min(bounds.height) - stroke_width) / 2.0;
        if radius <= 0.0 {
            return vec![frame.into_geometry()];
        }

        let percent = self.percent.min(100);
        let track = canvas::Stroke::default()
            .with_color(theme::alpha(self.color, 0.28))
            .with_width(stroke_width)
            .with_line_cap(canvas::LineCap::Round);
        frame.stroke(&Path::circle(center, radius), track);

        let fill = canvas::Stroke::default()
            .with_color(self.color)
            .with_width(stroke_width)
            .with_line_cap(canvas::LineCap::Round);
        match filled_end_angle(percent) {
            None => {}
            Some(_) if percent >= 100 => {
                frame.stroke(&Path::circle(center, radius), fill);
            }
            Some(end) => {
                let start = ring_start_angle();
                let arc = Path::new(|builder| {
                    builder.arc(canvas::path::Arc {
                        center,
                        radius,
                        start_angle: start,
                        end_angle: end,
                    });
                });
                frame.stroke(&arc, fill);
            }
        }

        vec![frame.into_geometry()]
    }
}

fn ring_start_angle() -> Radians {
    Radians::from(Degrees(-90.0))
}

/// Clockwise end angle of the filled outline, starting at 12 o'clock.
/// `None` when there is no fill (0%).
fn filled_end_angle(percent: u8) -> Option<Radians> {
    let percent = percent.min(100);
    if percent == 0 {
        return None;
    }
    Some(ring_start_angle() + Degrees(360.0 * f32::from(percent) / 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filled_end_angle_skips_empty() {
        assert!(filled_end_angle(0).is_none());
    }

    #[test]
    fn filled_end_angle_quarter_is_noon_plus_90_degrees() {
        let end = filled_end_angle(25).expect("25% should draw an arc");
        let expected = ring_start_angle() + Degrees(90.0);
        assert!((f32::from(end) - f32::from(expected)).abs() < f32::EPSILON);
    }

    #[test]
    fn filled_end_angle_full_covers_a_turn() {
        let end = filled_end_angle(100).expect("100% should close the ring");
        let expected = ring_start_angle() + Degrees(360.0);
        assert!((f32::from(end) - f32::from(expected)).abs() < f32::EPSILON);
        assert!(filled_end_angle(140).is_some());
    }
}
