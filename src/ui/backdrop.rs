//! Slow-drift atmosphere for utility windows (popup, Settings, compact Start).
//!
//! Same shader as immersive Start ([`AmbientProgram`]) but with **slow time**:
//! the shell nudges a process-wide utility clock at [`UTILITY_TICK`] (≤2fps)
//! running at [`UTILITY_TIME_RATE`] of real time, so the accent-bloom breath
//! and grain crawl instead of pulsing — a barely-there drift, not immersive
//! motion. Full-motion ambient stays immersive-only (`notes/ui-refresh.md`,
//! decision 4: frozen *or ~2 fps* time, no continuous redraw from idle
//! utility windows).
//!
//! The clock only advances while a utility window is actually visible: the
//! shell subscribes to the slow tick solely when the popup, Settings, or a
//! non-immersive Start window exists, so hidden/idle windows never tick and
//! never redraw. Toasts are excluded — the toast window uses its own
//! transparent theme and never renders this backdrop.
//!
//! No separate [`VignetteProgram`](crate::ui::shader::VignetteProgram) layer:
//! the ambient shader already darkens toward its edges, and the vignette's
//! alpha blend would tint the transparent corners of rounded windows.

use crate::ui::shader::AmbientProgram;
use iced::widget::shader;
use iced::{Element, Fill};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// Starting ambient phase. In `ambient.wgsl`, time only drives the accent-bloom
/// breath (`0.5 + 0.5·sin(t·1.4)`) and grain; `t = 0` holds the breath exactly
/// mid-swing and matches immersive Start's first frame (`ambient_time` starts
/// at 0), so a freshly opened utility window looks like a still of the same
/// atmosphere before the slow drift carries it.
pub const FROZEN_TIME: f32 = 0.0;

/// Shell tick period for the utility clock (≤2fps redraws while visible).
pub const UTILITY_TICK: Duration = Duration::from_millis(500);

/// Utility ambient time runs at 1/8 real-time: one full-rate breath (~4.5s)
/// stretches to ~36s. Same amplitude as immersive, barely-there pace.
pub const UTILITY_TIME_RATE: f32 = 0.125;

/// Advance per [`UTILITY_TICK`] while a utility window is visible.
const UTILITY_DT: f32 = 0.5 * UTILITY_TIME_RATE;

static UTILITY_TIME_BITS: AtomicU32 = AtomicU32::new(FROZEN_TIME.to_bits());

/// Current slow-drift time sampled by [`backdrop`].
pub fn utility_time() -> f32 {
    f32::from_bits(UTILITY_TIME_BITS.load(Ordering::Relaxed))
}

/// Nudge the utility clock forward one [`UTILITY_TICK`] step. Called from the
/// shell's slow-tick handler; a no-op visually unless a utility window is
/// open (no tick subscription exists otherwise, so idle windows stay still).
pub fn note_utility_tick() {
    let _ = UTILITY_TIME_BITS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
        Some((f32::from_bits(bits) + UTILITY_DT).to_bits())
    });
}

/// Full-bleed backdrop layer (iris fully open, no veil, no dock push), masked
/// to `corner_radius` logical px (0 = square, opaque).
pub fn backdrop<'a, Message: 'a>(corner_radius: f32) -> Element<'a, Message> {
    shader(AmbientProgram::new(utility_time(), 0.0, 0.0, 1.0).rounded(corner_radius))
        .width(Fill)
        .height(Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_breath_sits_mid_swing() {
        // Mirrors `pulse` in ambient.wgsl at the opening phase.
        let pulse = 0.5 + 0.5 * (FROZEN_TIME * 1.4).sin();
        assert!((pulse - 0.5).abs() < 1e-6);
    }

    #[test]
    fn utility_tick_advances_one_slow_step() {
        // Fixed-step advance: deterministic regardless of wall time.
        let before = utility_time();
        note_utility_tick();
        let after = utility_time();
        assert!((after - before - UTILITY_DT).abs() < 1e-6);
    }

    #[test]
    fn utility_drift_stays_subtle() {
        // One full-rate breath (2π/1.4 ≈ 4.49 utility-seconds) must take
        // dozens of real seconds at the drift rate.
        let breath_secs = std::f32::consts::TAU / 1.4;
        let real_secs = breath_secs / UTILITY_TIME_RATE;
        assert!(real_secs > 30.0);
        // And the tick itself stays at/above a 500ms period (≤2fps).
        assert!(UTILITY_TICK.as_millis() >= 500);
    }
}
