//! Pull-model indeterminate spinner.
//!
//! Replaces the old `RendererReloadTick` / `UpdateSpin` global 60Hz
//! subscriptions: the canvas owns its own per-window redraw clock and draws
//! the sweep from wall time while mounted.

use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::{Color, Element, Point, Radians, Rectangle, Renderer, Size, Theme, mouse};
use std::time::Instant;

const PERIOD: f32 = 1.2;
const SWEEP: f32 = std::f32::consts::TAU * 0.75;

/// Fixed-size spinner element. Use inside an existing row/button.
pub fn spinner<Message: 'static>(size: f32, color: Color) -> Element<'static, Message> {
    iced::widget::canvas(SpinnerProgram { size, color })
        .width(size)
        .height(size)
        .into()
}

#[derive(Debug)]
struct SpinnerProgram {
    size: f32,
    color: Color,
}

#[derive(Debug)]
struct SpinnerState {
    epoch: Instant,
}

impl Default for SpinnerState {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl<Message> canvas::Program<Message> for SpinnerProgram {
    type State = SpinnerState;

    fn update(
        &self,
        _state: &mut Self::State,
        _event: &iced::Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        Some(canvas::Action::request_redraw_at(
            Instant::now() + std::time::Duration::from_millis(16),
        ))
    }

    fn draw(
        &self,
        state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, Size::new(self.size, self.size));
        let radius = (self.size * 0.5 - 1.5).max(1.0);
        let center = Point::new(self.size * 0.5, self.size * 0.5);
        let elapsed = state.epoch.elapsed().as_secs_f32();
        let start = Radians((elapsed / PERIOD).fract() * std::f32::consts::TAU);
        let path = Path::new(|builder| {
            builder.arc(canvas::path::Arc {
                center,
                radius,
                start_angle: start,
                end_angle: Radians(start.0 + SWEEP),
            });
        });
        frame.stroke(
            &path,
            Stroke::default()
                .with_width(2.0)
                .with_color(self.color)
                .with_line_cap(canvas::LineCap::Round),
        );
        let _ = bounds;
        vec![frame.into_geometry()]
    }
}
