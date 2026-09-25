//! Analytics coverage chart canvas program.

use super::ConfigureMessage;
use crate::persist::analytics::StepCoverage;
use crate::ui::theme;
use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry};
use iced::{Color, Point, Rectangle, Renderer, Size, Theme};

pub(super) struct CoverageChart {
    pub steps: Vec<StepCoverage>,
    pub drain: bool,
}

impl canvas::Program<ConfigureMessage> for CoverageChart {
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
        let pad = 6.0;
        let width = (bounds.width - pad * 2.0).max(1.0);
        let height = (bounds.height - pad * 2.0).max(1.0);

        frame.fill_rectangle(
            Point::new(0.0, 0.0),
            Size::new(bounds.width, bounds.height),
            theme::PANEL,
        );

        if self.steps.is_empty() {
            return vec![frame.into_geometry()];
        }

        let filled = theme::ACCENT;
        let charge_filled = theme::SUCCESS;
        let gap = theme::LINE;
        let speculative_drain = Color {
            a: 0.35,
            ..theme::ACCENT
        };
        let speculative_charge = Color {
            a: 0.35,
            ..theme::SUCCESS
        };

        let n = self.steps.len() as f32;
        let seg_w = width / n;
        for (i, step) in self.steps.iter().enumerate() {
            let x = pad + i as f32 * seg_w;
            let color = if step.typical_ms.is_some() {
                if self.drain { filled } else { charge_filled }
            } else if step.speculative {
                if self.drain {
                    speculative_drain
                } else {
                    speculative_charge
                }
            } else {
                gap
            };
            frame.fill_rectangle(
                Point::new(x + 1.0, pad + height * 0.25),
                Size::new((seg_w - 2.0).max(1.0), height * 0.5),
                color,
            );
        }

        vec![frame.into_geometry()]
    }
}
