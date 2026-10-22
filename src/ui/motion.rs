//! Shared motion vocabulary: easing curves, frame-dt cap, and standard durations.
//!
//! Immersive Start established the feel (ease-in-out veils, ease-out reveals,
//! a 32 ms per-frame cap so a UI stall cannot skip the start of a curve). Every
//! animated surface should build on these instead of re-deriving them.
//!
//! ## Reduced motion
//!
//! Movement (travel, slide, zoom, reflow) is gated by the effective
//! reduce-motion flag ([`reduced_motion_effective`]); opacity (fades, dims,
//! veils) is never gated and keeps its own durations. Call sites snap
//! movement — durations to 0 via [`movement_duration_ms`], offsets to 0 via
//! [`movement_offset`], scales to 1 via [`movement_scale`] — while opacity
//! clocks run unchanged. The OS probe ([`system_animations_enabled`]) reads
//! Windows' "Show animations" toggle (`SPI_GETCLIENTAREAANIMATION`); every
//! other OS has no equivalent probe and reports animations off (documented in
//! `notes/ui-refresh.md`), so `System` means reduced motion there.

use std::time::Duration;

use crate::persist::prefs::ReducedMotion;

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

/// Whether the OS chrome shows window animations.
///
/// Windows: the Settings → Accessibility → "Show animations" toggle, read via
/// `SystemParametersInfo(SPI_GETCLIENTAREAANIMATION)`. A failed probe reports
/// animations on (full motion) so a transient API error cannot freeze the UI.
///
/// Non-Windows: no equivalent probe exists; reports animations off, so the
/// `System` preference means reduced motion there (see `notes/ui-refresh.md`).
pub fn system_animations_enabled() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
        };
        use windows::core::BOOL;
        let mut enabled = BOOL(0);
        // SAFETY: `enabled` is caller-owned; `addr_of_mut!` never forms a
        // temporary reference, and the API writes at most one `BOOL`.
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_GETCLIENTAREAANIMATION,
                0,
                Some(std::ptr::addr_of_mut!(enabled).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        // A failed probe must not freeze the UI: fall back to full motion.
        ok.is_ok() && enabled.as_bool()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Effective reduce-motion gate for a [`ReducedMotion`] preference.
///
/// `On` skips movement, `Off` always animates, `System` follows the OS toggle
/// (inverted: OS animations off ⇒ reduce motion on).
pub fn reduced_motion_effective(pref: ReducedMotion) -> bool {
    match pref {
        ReducedMotion::On => true,
        ReducedMotion::Off => false,
        ReducedMotion::System => !system_animations_enabled(),
    }
}

/// Movement duration under the gate: 0 settles instantly. Opacity keeps its
/// own clock — never route a fade/dim/veil duration through this.
pub fn movement_duration_ms(normal_ms: u64, reduced: bool) -> u64 {
    if reduced { 0 } else { normal_ms }
}

/// Movement travel under the gate: 0 removes the slide/pan with no reflow.
pub fn movement_offset(px: f32, reduced: bool) -> f32 {
    if reduced { 0.0 } else { px }
}

/// Movement zoom under the gate: 1 renders unscaled.
pub fn movement_scale(scale: f32, reduced: bool) -> f32 {
    if reduced { 1.0 } else { scale }
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

    #[test]
    fn reduced_motion_effective_follows_pref_and_probe() {
        assert!(reduced_motion_effective(ReducedMotion::On));
        assert!(!reduced_motion_effective(ReducedMotion::Off));
        // System is the inverted OS probe (deterministic per machine, whatever it is).
        assert_eq!(
            reduced_motion_effective(ReducedMotion::System),
            !system_animations_enabled()
        );
    }

    #[test]
    fn movement_helpers_snap_under_gate() {
        assert_eq!(movement_duration_ms(220, false), 220);
        assert_eq!(movement_duration_ms(220, true), 0);
        assert!((movement_offset(72.0, false) - 72.0).abs() < f32::EPSILON);
        assert!((movement_offset(72.0, true) - 0.0).abs() < f32::EPSILON);
        assert!((movement_scale(0.88, false) - 0.88).abs() < f32::EPSILON);
        assert!((movement_scale(0.88, true) - 1.0).abs() < f32::EPSILON);
    }
}
