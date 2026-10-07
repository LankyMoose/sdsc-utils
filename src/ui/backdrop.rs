//! Nearly-static atmosphere for utility windows (popup, Settings, compact Start).
//!
//! Same shaders as immersive Start ([`AmbientProgram`] + [`VignetteProgram`])
//! but with **frozen time**: the frame is a function of its inputs only, so the
//! window redraws on events like any other iced view and never requests
//! continuous frames. Full-motion ambient stays immersive-only
//! (`notes/ui-refresh.md`, decision 4).

use crate::ui::shader::{AmbientProgram, VignetteProgram};
use iced::widget::{shader, stack};
use iced::{Element, Fill};

/// Frozen ambient phase. In `ambient.wgsl`, time only drives the accent-bloom
/// breath (`0.5 + 0.5·sin(t·1.4)`) and grain; `t = 0` holds the breath exactly
/// mid-swing and matches immersive Start's first frame (`ambient_time` starts
/// at 0), so utility windows look like a still of the same atmosphere.
pub const FROZEN_TIME: f32 = 0.0;

/// Edge-vignette strength for utility windows. Softer than immersive's 0.55:
/// small windows have less canvas, so a strong rim reads as a dark frame.
pub const VIGNETTE_STRENGTH: f32 = 0.35;

/// Full-bleed backdrop layer: ambient atmosphere (iris fully open, no veil, no
/// dock push) under a soft edge vignette. Stack content on top of it.
pub fn backdrop<'a, Message: 'a>() -> Element<'a, Message> {
    stack![
        shader(AmbientProgram::new(FROZEN_TIME, 0.0, 0.0, 1.0))
            .width(Fill)
            .height(Fill),
        shader(VignetteProgram::new(VIGNETTE_STRENGTH))
            .width(Fill)
            .height(Fill),
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

/// `content` over [`backdrop`].
pub fn with_backdrop<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    stack![backdrop(), content.into()]
        .width(Fill)
        .height(Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_breath_sits_mid_swing() {
        // Mirrors `pulse` in ambient.wgsl.
        let pulse = 0.5 + 0.5 * (FROZEN_TIME * 1.4).sin();
        assert!((pulse - 0.5).abs() < 1e-6);
    }

    #[test]
    fn vignette_is_softer_than_immersive() {
        // Immersive stage uses 0.55; utility windows must stay subtler.
        const _: () = assert!(VIGNETTE_STRENGTH > 0.0 && VIGNETTE_STRENGTH < 0.55);
    }
}
