//! Nearly-static atmosphere for utility windows (popup, Settings, compact Start).
//!
//! Same shader as immersive Start ([`AmbientProgram`]) but with **frozen
//! time**: the frame is a function of its inputs only, so the window redraws
//! on events like any other iced view and never requests continuous frames.
//! Full-motion ambient stays immersive-only (`notes/ui-refresh.md`, decision 4).
//!
//! No separate [`VignetteProgram`](crate::ui::shader::VignetteProgram) layer:
//! the ambient shader already darkens toward its edges, and the vignette's
//! alpha blend would tint the transparent corners of rounded windows.

use crate::ui::shader::AmbientProgram;
use iced::widget::shader;
use iced::{Element, Fill};

/// Frozen ambient phase. In `ambient.wgsl`, time only drives the accent-bloom
/// breath (`0.5 + 0.5·sin(t·1.4)`) and grain; `t = 0` holds the breath exactly
/// mid-swing and matches immersive Start's first frame (`ambient_time` starts
/// at 0), so utility windows look like a still of the same atmosphere.
pub const FROZEN_TIME: f32 = 0.0;

/// Full-bleed backdrop layer (iris fully open, no veil, no dock push), masked
/// to `corner_radius` logical px (0 = square, opaque).
pub fn backdrop<'a, Message: 'a>(corner_radius: f32) -> Element<'a, Message> {
    shader(AmbientProgram::new(FROZEN_TIME, 0.0, 0.0, 1.0).rounded(corner_radius))
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
}
