//! Shared motion vocabulary: easing curves, frame-dt cap, and standard durations.
//!
//! Immersive Launcher established the feel (ease-in-out veils, ease-out reveals,
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

use std::time::{Duration, Instant};

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

/// Home/Browser entrance: let the card appear first; the full-screen backdrop
/// stays clear briefly, then fades in behind it.
pub const SHELL_BACKDROP_DELAY: Duration = Duration::from_millis(140);
pub const SHELL_BACKDROP_FADE: Duration = Duration::from_millis(220);

/// One wall-clock animation segment. Pose is derived from `started`; callers
/// never accumulate per-message `dt`, so a stalled loop resumes at the true
/// time instead of replaying or skipping the curve.
#[derive(Debug, Clone, Copy)]
pub struct Timeline {
    started: Instant,
    duration: Duration,
    paused_at: Option<Instant>,
    paused_total: Duration,
}

impl Timeline {
    pub fn new(started: Instant, duration: Duration) -> Self {
        Self {
            started,
            duration,
            paused_at: None,
            paused_total: Duration::ZERO,
        }
    }

    /// Open-ended timeline (only [`Self::elapsed`] is meaningful).
    pub fn since(started: Instant) -> Self {
        Self::new(started, Duration::ZERO)
    }

    pub fn started(&self) -> Instant {
        self.started
    }

    pub fn duration(&self) -> Duration {
        self.duration
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        let paused_now = self
            .paused_at
            .map(|at| now.saturating_duration_since(at))
            .unwrap_or_default();
        now.saturating_duration_since(self.started)
            .saturating_sub(self.paused_total)
            .saturating_sub(paused_now)
    }

    pub fn progress(&self, now: Instant) -> f32 {
        progress_linear(
            self.elapsed(now).as_millis() as u64,
            self.duration.as_millis() as u64,
        )
    }

    pub fn is_done(&self, now: Instant) -> bool {
        self.progress(now) >= 1.0
    }

    pub fn is_paused(&self) -> bool {
        self.paused_at.is_some()
    }

    pub fn pause(&mut self, now: Instant) {
        if self.paused_at.is_none() {
            self.paused_at = Some(now);
        }
    }

    pub fn resume(&mut self, now: Instant) {
        if let Some(at) = self.paused_at.take() {
            self.paused_total = self
                .paused_total
                .saturating_add(now.saturating_duration_since(at));
        }
    }
}

impl Default for Timeline {
    fn default() -> Self {
        Self::since(Instant::now())
    }
}

/// Backdrop alpha for a shell opened at `started`: 0 during the delay, then
/// an eased fade to `target`. Opacity is never gated by reduced motion.
pub fn shell_backdrop_alpha(
    started: std::time::Instant,
    now: std::time::Instant,
    target: f32,
) -> f32 {
    let elapsed = now.saturating_duration_since(started);
    if elapsed < SHELL_BACKDROP_DELAY {
        return 0.0;
    }
    let t = progress_linear(
        (elapsed - SHELL_BACKDROP_DELAY).as_millis() as u64,
        SHELL_BACKDROP_FADE.as_millis() as u64,
    );
    target * ease_in_out_cubic(t)
}

pub fn shell_backdrop_done(started: std::time::Instant, now: std::time::Instant) -> bool {
    now.saturating_duration_since(started) >= SHELL_BACKDROP_DELAY + SHELL_BACKDROP_FADE
}

/// One shared full-screen dim behind Home/Browser/Launcher.
///
/// All three modes draw inside one shell HWND, so this is simply the single
/// alpha timeline for that window. It stays full across mode swaps and fades
/// only when the shell hides for the desktop; no ownership handoff is needed.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ShellBackdrop {
    #[default]
    Hidden,
    Entering {
        started: Instant,
    },
    /// The dim is already partly visible and a screen is claiming it again
    /// (swap/resume). Rise to full from the current alpha; never snap.
    Rising {
        started: Instant,
        from: f32,
    },
    Full,
    Exiting {
        started: Instant,
        from: f32,
    },
}

impl ShellBackdrop {
    pub fn alpha(&self, now: Instant) -> f32 {
        match *self {
            Self::Hidden => 0.0,
            Self::Entering { started } => {
                shell_backdrop_alpha(started, now, crate::ui::theme::BACKDROP_ALPHA)
            }
            Self::Rising { started, from } => {
                let t = progress_linear(
                    now.saturating_duration_since(started).as_millis() as u64,
                    BASE_MS,
                );
                let target = crate::ui::theme::BACKDROP_ALPHA;
                from + (target - from) * ease_out_cubic(t)
            }
            Self::Full => crate::ui::theme::BACKDROP_ALPHA,
            Self::Exiting { started, from } => {
                let t = progress_linear(
                    now.saturating_duration_since(started).as_millis() as u64,
                    BASE_MS,
                );
                from * (1.0 - ease_in_out_cubic(t))
            }
        }
    }

    pub fn enter(&mut self, now: Instant) {
        *self = Self::Entering { started: now };
    }

    /// Claim the shared dim for a new screen without a discontinuity. Hidden
    /// starts the delayed entrance; a partial dim rises to full; full stays
    /// full. This replaces the old hold-full snap (0 → target in one update).
    pub fn raise(&mut self, now: Instant) {
        let target = crate::ui::theme::BACKDROP_ALPHA;
        let from = self.alpha(now);
        if from >= target - 0.001 {
            *self = Self::Full;
        } else if from <= 0.01 {
            self.enter(now);
        } else {
            *self = Self::Rising { started: now, from };
        }
    }

    pub fn exit(&mut self, now: Instant) {
        if matches!(self, Self::Hidden) {
            return;
        }
        let from = self.alpha(now);
        *self = Self::Exiting { started: now, from };
    }

    /// True while a frame clock is needed for this timeline.
    pub fn animating(&self, now: Instant) -> bool {
        match *self {
            Self::Hidden | Self::Full => false,
            Self::Entering { started } => !shell_backdrop_done(started, now),
            Self::Rising { started, .. } => {
                (now.saturating_duration_since(started).as_millis() as u64) < BASE_MS
            }
            Self::Exiting { started, .. } => {
                (now.saturating_duration_since(started).as_millis() as u64) < BASE_MS
            }
        }
    }

    pub fn done(&self, now: Instant) -> bool {
        match *self {
            Self::Hidden | Self::Full => true,
            Self::Entering { started } => shell_backdrop_done(started, now),
            Self::Rising { started, .. } => {
                now.saturating_duration_since(started).as_millis() as u64 >= BASE_MS
            }
            Self::Exiting { started, .. } => {
                now.saturating_duration_since(started).as_millis() as u64 >= BASE_MS
            }
        }
    }
}

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
    fn timeline_derives_pose_and_excludes_pauses() {
        let t0 = Instant::now();
        let mut timeline = Timeline::new(t0, Duration::from_secs(1));
        assert_eq!(timeline.progress(t0), 0.0);
        assert!((timeline.progress(t0 + Duration::from_millis(500)) - 0.5).abs() < 0.001);
        timeline.pause(t0 + Duration::from_millis(500));
        assert!((timeline.progress(t0 + Duration::from_secs(2)) - 0.5).abs() < 0.001);
        timeline.resume(t0 + Duration::from_secs(2));
        assert!((timeline.progress(t0 + Duration::from_millis(2500)) - 1.0).abs() < 0.001);
        assert!(timeline.is_done(t0 + Duration::from_secs(3)));
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

#[cfg(test)]
mod shell_fade_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn backdrop_waits_then_fades_to_target() {
        let t0 = Instant::now();
        assert_eq!(shell_backdrop_alpha(t0, t0, 0.55), 0.0);
        assert_eq!(
            shell_backdrop_alpha(
                t0,
                t0 + SHELL_BACKDROP_DELAY - Duration::from_millis(1),
                0.55
            ),
            0.0
        );
        let mid = shell_backdrop_alpha(
            t0,
            t0 + SHELL_BACKDROP_DELAY + SHELL_BACKDROP_FADE / 2,
            0.55,
        );
        assert!(mid > 0.0 && mid < 0.55, "midpoint should be partial: {mid}");
        assert!(
            (shell_backdrop_alpha(t0, t0 + SHELL_BACKDROP_DELAY + SHELL_BACKDROP_FADE, 0.55)
                - 0.55)
                .abs()
                < 0.001
        );
        assert!(shell_backdrop_done(
            t0,
            t0 + SHELL_BACKDROP_DELAY + SHELL_BACKDROP_FADE
        ));
    }

    #[test]
    fn backdrop_raise_never_snaps() {
        let t0 = Instant::now();
        let claim_at = t0 + SHELL_BACKDROP_DELAY + SHELL_BACKDROP_FADE / 2;
        let mut backdrop = ShellBackdrop::Hidden;
        backdrop.enter(t0);
        let before = backdrop.alpha(claim_at);
        assert!(before > 0.0 && before < crate::ui::theme::BACKDROP_ALPHA);
        backdrop.raise(claim_at);
        assert!(
            (backdrop.alpha(claim_at) - before).abs() < 0.0001,
            "raise must start at the current alpha, not snap to full"
        );
        let mid = backdrop.alpha(claim_at + Duration::from_millis(BASE_MS / 2));
        assert!(
            mid > before && mid < crate::ui::theme::BACKDROP_ALPHA,
            "midpoint should keep rising smoothly: {mid}"
        );
        assert!(backdrop.animating(claim_at + Duration::from_millis(BASE_MS / 2)));
        assert!(
            (backdrop.alpha(claim_at + Duration::from_millis(BASE_MS))
                - crate::ui::theme::BACKDROP_ALPHA)
                .abs()
                < 0.0001
        );
        // Hidden still starts the delayed entrance; Full stays settled.
        let mut hidden = ShellBackdrop::Hidden;
        hidden.raise(t0);
        assert!(matches!(hidden, ShellBackdrop::Entering { .. }));
        let mut full = ShellBackdrop::Full;
        full.raise(t0);
        assert_eq!(full, ShellBackdrop::Full);
    }

    #[test]
    fn shared_backdrop_holds_through_swaps_and_fades_only_for_desktop() {
        let t0 = Instant::now();
        let mut backdrop = ShellBackdrop::Hidden;
        assert_eq!(backdrop.alpha(t0), 0.0);
        backdrop.enter(t0);
        let mid = backdrop.alpha(t0 + SHELL_BACKDROP_DELAY + SHELL_BACKDROP_FADE / 2);
        assert!(mid > 0.0 && mid < crate::ui::theme::BACKDROP_ALPHA);
        assert!(backdrop.animating(t0 + SHELL_BACKDROP_DELAY));
        assert!(!backdrop.animating(t0 + SHELL_BACKDROP_DELAY + SHELL_BACKDROP_FADE));
        backdrop.raise(t0 + SHELL_BACKDROP_DELAY + SHELL_BACKDROP_FADE / 2);
        assert_eq!(
            backdrop.alpha(t0 + Duration::from_secs(10)),
            crate::ui::theme::BACKDROP_ALPHA
        );
        backdrop.exit(t0 + Duration::from_secs(10));
        let now = t0 + Duration::from_secs(10) + Duration::from_millis(BASE_MS / 2);
        let half = backdrop.alpha(now);
        assert!(half > 0.0 && half < crate::ui::theme::BACKDROP_ALPHA);
        assert!(backdrop.done(t0 + Duration::from_secs(10) + Duration::from_millis(BASE_MS)));
        assert!(!backdrop.animating(t0 + Duration::from_secs(10) + Duration::from_millis(BASE_MS)));
    }
}
