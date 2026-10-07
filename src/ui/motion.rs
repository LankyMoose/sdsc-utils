//! Shared motion vocabulary: easing curves, frame-dt cap, and standard durations.
//!
//! Immersive Start established the feel (ease-in-out veils, ease-out reveals,
//! a 32 ms per-frame cap so a UI stall cannot skip the start of a curve). Every
//! animated surface should build on these instead of re-deriving them.

use std::time::Duration;

/// Per-frame cap on animation time advance. A stalled frame (atlas upload,
/// HWND resize) advances at most this much so curves never skip their start.
pub const MAX_FRAME_DT_MS: u64 = 32;
/// [`MAX_FRAME_DT_MS`] as a [`Duration`].
pub const MAX_FRAME_DT: Duration = Duration::from_millis(MAX_FRAME_DT_MS);

/// Micro-feedback: press/hover glyph scale, hint state flips.
pub const FAST_MS: u64 = 100;
/// Standard UI transition: chip slide-in/out, drawer reveal, toast slide.
pub const BASE_MS: u64 = 280;
/// Large surface transition: art crossfades, veil-style fades.
pub const SLOW_MS: u64 = 450;

/// Linear 0..=1 fraction of `elapsed_ms / duration_ms` (zero duration = done).
pub fn progress_linear(elapsed_ms: u64, duration_ms: u64) -> f32 {
    if duration_ms == 0 {
        return 1.0;
    }
    (elapsed_ms as f32 / duration_ms as f32).clamp(0.0, 1.0)
}

/// Ease-out cubic — fast start, soft landing (reveals, slide-ins).
pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Ease-in cubic — slow start (exits).
pub fn ease_in_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t
}

/// Ease-in-out cubic — symmetric veils and dims.
pub fn ease_in_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Advance `elapsed` by `raw_dt` capped at [`MAX_FRAME_DT`].
///
/// Returns `(new_elapsed, linear progress 0..=1, raw_dt_was_capped)`.
pub fn advance_capped(
    elapsed: Duration,
    raw_dt: Duration,
    duration: Duration,
) -> (Duration, f32, bool) {
    let capped = raw_dt > MAX_FRAME_DT;
    let new_elapsed = elapsed.saturating_add(raw_dt.min(MAX_FRAME_DT));
    let progress = if duration.is_zero() {
        1.0
    } else {
        (new_elapsed.as_secs_f32() / duration.as_secs_f32()).min(1.0)
    };
    (new_elapsed, progress, capped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_hit_endpoints_and_clamp() {
        for ease in [ease_out_cubic, ease_in_cubic, ease_in_out_cubic] {
            assert_eq!(ease(0.0), 0.0);
            assert_eq!(ease(1.0), 1.0);
            assert_eq!(ease(-1.0), 0.0);
            assert_eq!(ease(2.0), 1.0);
        }
        assert!((ease_in_out_cubic(0.5) - 0.5).abs() < 1e-6);
        assert!(ease_out_cubic(0.5) > 0.5);
        assert!(ease_in_cubic(0.5) < 0.5);
    }

    #[test]
    fn linear_progress_handles_zero_duration() {
        assert_eq!(progress_linear(10, 0), 1.0);
        assert_eq!(progress_linear(50, 100), 0.5);
        assert_eq!(progress_linear(500, 100), 1.0);
    }

    #[test]
    fn advance_caps_stalled_frames() {
        let d = Duration::from_millis(BASE_MS);
        let (elapsed, _, capped) = advance_capped(Duration::ZERO, Duration::from_millis(500), d);
        assert!(capped);
        assert_eq!(elapsed, MAX_FRAME_DT);

        let (elapsed, _, capped) = advance_capped(Duration::ZERO, Duration::from_millis(16), d);
        assert!(!capped);
        assert_eq!(elapsed, Duration::from_millis(16));

        let (_, progress, _) = advance_capped(Duration::from_secs(5), MAX_FRAME_DT, d);
        assert_eq!(progress, 1.0);
        let (_, progress, _) = advance_capped(Duration::ZERO, MAX_FRAME_DT, Duration::ZERO);
        assert_eq!(progress, 1.0);
    }
}
