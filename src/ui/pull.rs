//! Shared pull-model frame demand.
//!
//! Surfaces own their redraw cadence: a widget/shader/canvas advances its
//! wall-clock pose and asks for the next frame through this type instead of a
//! global `iced::time::every` subscription. Keeping the request vocabulary in
//! one place makes the intended cadence explicit and prevents a surface from
//! silently falling back to an app-wide timer.

use iced::advanced::Shell;
use iced::time::Instant;
use std::time::Duration;

/// One surface's next redraw request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDemand {
    /// Nothing is moving; do not request another frame.
    Idle,
    /// A state change is already visible and should present immediately.
    Now,
    /// Presentation loop: wake at the supplied instant.
    At(Instant),
}

impl FrameDemand {
    /// The shared display-rate frame interval.
    pub const FRAME: Duration = Duration::from_millis(16);

    /// Display-rate demand anchored at `now`.
    pub fn next(now: Instant) -> Self {
        Self::At(now + Self::FRAME)
    }

    /// Demand at an explicit deadline.
    pub fn at(now: Instant, interval: Duration) -> Self {
        Self::At(now + interval)
    }

    pub fn is_idle(self) -> bool {
        matches!(self, Self::Idle)
    }

    /// Apply to a custom `Widget` (via `Shell`).
    pub fn request<Message>(self, shell: &mut Shell<'_, Message>) {
        match self {
            Self::Idle => {}
            Self::Now => shell.request_redraw(),
            Self::At(at) => shell.request_redraw_at(at),
        }
    }

    /// Apply to a shader `Program`.
    pub fn into_program<Message>(self) -> Option<iced::widget::Action<Message>> {
        match self {
            Self::Idle => None,
            Self::Now => Some(iced::widget::Action::request_redraw()),
            Self::At(at) => Some(iced::widget::Action::request_redraw_at(at)),
        }
    }

    /// Apply to a canvas `Program`.
    pub fn into_canvas<Message>(self) -> Option<iced::widget::canvas::Action<Message>> {
        match self {
            Self::Idle => None,
            Self::Now => Some(iced::widget::canvas::Action::request_redraw()),
            Self::At(at) => Some(iced::widget::canvas::Action::request_redraw_at(at)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_rate_demand_advances_by_one_frame() {
        let now = Instant::now();
        assert_eq!(
            FrameDemand::next(now),
            FrameDemand::At(now + FrameDemand::FRAME)
        );
        assert!(FrameDemand::Idle.is_idle());
        assert!(!FrameDemand::Now.is_idle());
    }
}
