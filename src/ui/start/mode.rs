//! Pure start-screen timing (enter ceremony) and controllers-surface focus.
//!
//! The start screen is always the fullscreen console stage; there is no
//! windowed presentation and no promote/demote morph.

#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicU32, Ordering};

/// Enter ceremony phase around opening the start window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionPhase {
    /// Top-veil lift after open (holds for splash art sync).
    EnterImmersive,
}

/// Shared duration for darken veils and the cold ambient ease.
pub const VEIL_TRANSITION_MS: u64 = 300;
/// Enter top-veil lift (ease-in-out); matches splash length so one ceremony covers both.
pub const ENTER_REVEAL_MS: u64 = crate::ui::motion::SLOW_MS;
pub const ENTER_IMMERSIVE_MS: u64 = ENTER_REVEAL_MS;
/// Max time to hold a solid enter veil waiting for selected splash + strip heroes.
pub const ENTER_ART_HOLD_MAX_MS: u64 = 3000;
/// First-launch cold load: black → ambient ease before status shows over atmosphere.
pub const COLD_AMBIENT_MS: u64 = VEIL_TRANSITION_MS;
/// Minimum wall time for a live Steam library scan/refresh so chrome spinners stay readable.
pub const STEAM_SCAN_MIN_MS: u64 = 2000;
/// Slide a chrome status chip in (below → its stack slot) / stack reflow.
pub const CHROME_STATUS_IN_MS: u64 = crate::ui::motion::BASE_MS;
/// Slide-right exit duration once the batch clears.
pub const CHROME_STATUS_EXIT_MS: u64 = crate::ui::motion::BASE_MS;
/// Hold each success checkmark before the batch may clear.
pub const CHROME_STATUS_SUCCESS_HOLD_MS: u64 = 500;
/// Extra pause after every chip is Success before staggered ExitRight.
pub const CHROME_STATUS_STEP_HOLD_MS: u64 = 180;
/// Delay between each chip starting ExitRight (bottom-first).
pub const CHROME_STATUS_STAGGER_MS: u64 = 70;
/// Horizontal travel for ExitRight (logical px). Must clear the widest status chip
/// past the window's right edge (labels like "Scanning Steam library…").
pub const CHROME_STATUS_EXIT_TRAVEL_PX: f32 = 520.0;
/// Cold: fade the games strip in once artwork is ready.
pub const GAMES_LIST_FADE_MS: u64 = ART_FADE_MS;
/// Opacity crossfade between game backdrops (selection change; not enter veil).
pub const ART_FADE_MS: u64 = crate::ui::motion::SLOW_MS;
/// Shared ken-burns period for zoom + pan (burn-in drift while splash is held).
pub const BACKDROP_ZOOM_PERIOD_MS: u64 = 30_000;
/// Mid scale between min zoom and peak (must stay > 1 for Cover pan headroom).
pub const BACKDROP_SCALE_BASE: f32 = 1.055;
/// Peak zoom excursion around [`BACKDROP_SCALE_BASE`].
pub const BACKDROP_SCALE_AMP: f32 = 0.02;
/// Floor scale while panning (Cover edge safety).
pub const BACKDROP_SCALE_MIN: f32 = 1.035;
/// Max horizontal drift as a fraction of stage width (Cover overflow is often huge).
pub const BACKDROP_PAN_STAGE_X: f32 = 0.012;
/// Max vertical drift as a fraction of stage height.
pub const BACKDROP_PAN_STAGE_Y: f32 = 0.035;
/// Never spend more than this fraction of real Cover+zoom overflow (black-edge safety).
pub const BACKDROP_PAN_OVERFLOW_CAP: f32 = 0.92;

/// Dev-only ken-burns tuning defaults (multiplier of shipped behavior).
pub const DEFAULT_KENBURNS_AMP_SCALE: f32 = 1.0;
pub const DEFAULT_KENBURNS_PAN_SCALE: f32 = 1.0;
/// Pose-clock speed multiplier (0 freezes drift).
pub const DEFAULT_KENBURNS_SPEED: f32 = 1.0;

#[cfg(debug_assertions)]
static KENBURNS_AMP_BITS: AtomicU32 = AtomicU32::new(DEFAULT_KENBURNS_AMP_SCALE.to_bits());
#[cfg(debug_assertions)]
static KENBURNS_PAN_BITS: AtomicU32 = AtomicU32::new(DEFAULT_KENBURNS_PAN_SCALE.to_bits());
#[cfg(debug_assertions)]
static KENBURNS_SPEED_BITS: AtomicU32 = AtomicU32::new(DEFAULT_KENBURNS_SPEED.to_bits());

/// Zoom-excursion multiplier. Release: always [`DEFAULT_KENBURNS_AMP_SCALE`].
pub fn kenburns_amp_scale() -> f32 {
    #[cfg(debug_assertions)]
    {
        f32::from_bits(KENBURNS_AMP_BITS.load(Ordering::Relaxed))
    }
    #[cfg(not(debug_assertions))]
    {
        DEFAULT_KENBURNS_AMP_SCALE
    }
}

/// Pan-travel multiplier (still capped by overflow headroom, so no black
/// edges). Release: always [`DEFAULT_KENBURNS_PAN_SCALE`].
pub fn kenburns_pan_scale() -> f32 {
    #[cfg(debug_assertions)]
    {
        f32::from_bits(KENBURNS_PAN_BITS.load(Ordering::Relaxed))
    }
    #[cfg(not(debug_assertions))]
    {
        DEFAULT_KENBURNS_PAN_SCALE
    }
}

/// Pose-clock speed multiplier. Release: always [`DEFAULT_KENBURNS_SPEED`].
pub fn kenburns_speed() -> f32 {
    #[cfg(debug_assertions)]
    {
        f32::from_bits(KENBURNS_SPEED_BITS.load(Ordering::Relaxed))
    }
    #[cfg(not(debug_assertions))]
    {
        DEFAULT_KENBURNS_SPEED
    }
}

/// True while any ken-burns tuning differs from the shipped defaults.
#[cfg(debug_assertions)]
pub fn kenburns_tuning_active() -> bool {
    kenburns_amp_scale() != DEFAULT_KENBURNS_AMP_SCALE
        || kenburns_pan_scale() != DEFAULT_KENBURNS_PAN_SCALE
        || kenburns_speed() != DEFAULT_KENBURNS_SPEED
}

/// Debug-only setters (Diagnostics sliders; shell-local, never persisted).
#[cfg(debug_assertions)]
pub fn set_kenburns_amp_scale(v: f32) {
    KENBURNS_AMP_BITS.store(v.clamp(0.0, 5.0).to_bits(), Ordering::Relaxed);
}
#[cfg(debug_assertions)]
pub fn set_kenburns_pan_scale(v: f32) {
    KENBURNS_PAN_BITS.store(v.clamp(0.0, 5.0).to_bits(), Ordering::Relaxed);
}
#[cfg(debug_assertions)]
pub fn set_kenburns_speed(v: f32) {
    KENBURNS_SPEED_BITS.store(v.clamp(0.0, 8.0).to_bits(), Ordering::Relaxed);
}
/// Restore shipped behavior (Diagnostics reset).
#[cfg(debug_assertions)]
pub fn reset_kenburns_tuning() {
    set_kenburns_amp_scale(DEFAULT_KENBURNS_AMP_SCALE);
    set_kenburns_pan_scale(DEFAULT_KENBURNS_PAN_SCALE);
    set_kenburns_speed(DEFAULT_KENBURNS_SPEED);
}

/// Test lock for the process-wide ken-burns globals (see
/// `notes/flaky-tests.md`). Release tests compile it to a pass-through.
#[cfg(test)]
pub(crate) fn with_pose_test_lock<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(debug_assertions)]
    {
        static POSE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = POSE_TEST_LOCK.lock().unwrap();
        reset_kenburns_tuning();
        let result = f();
        reset_kenburns_tuning();
        result
    }
    #[cfg(not(debug_assertions))]
    {
        f()
    }
}

impl TransitionPhase {
    pub fn duration_ms(self) -> u64 {
        match self {
            Self::EnterImmersive => ENTER_IMMERSIVE_MS,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::EnterImmersive => "enter_immersive",
        }
    }
}

/// Footer Circle label for the main (non-modal) start chrome.
pub fn cancel_circle_label(editing: bool) -> &'static str {
    if editing { "Cancel" } else { "Close" }
}

/// Right while immersive: expand dock when collapsed.
pub fn dock_expand_target(expanded: bool) -> Option<bool> {
    if expanded { None } else { Some(true) }
}

/// Left while immersive: collapse dock when expanded.
pub fn dock_collapse_target(expanded: bool) -> Option<bool> {
    if !expanded { None } else { Some(false) }
}

/// Stage scale while the controllers surface opens (1 → ~12% smaller).
pub fn dock_stage_scale(progress: f32) -> f32 {
    let t = progress.clamp(0.0, 1.0);
    1.0 - 0.12 * t
}

/// Dim overlay alpha over the games stage while the controllers surface opens.
pub fn dock_stage_dim(progress: f32) -> f32 {
    progress.clamp(0.0, 1.0) * 0.45
}

/// Dock panel width from peek ring column to full rows panel.
pub fn dock_panel_width(progress: f32, peek: f32, expanded: f32) -> f32 {
    let t = progress.clamp(0.0, 1.0);
    peek + (expanded - peek) * t
}

/// Ease-out cubic progress 0..=1 for elapsed/duration.
pub fn phase_progress(elapsed_ms: u64, duration_ms: u64) -> f32 {
    crate::ui::motion::ease_out_cubic(phase_progress_linear(elapsed_ms, duration_ms))
}

/// Linear 0..=1 elapsed fraction (no easing).
pub fn phase_progress_linear(elapsed_ms: u64, duration_ms: u64) -> f32 {
    crate::ui::motion::progress_linear(elapsed_ms, duration_ms)
}

pub use crate::ui::motion::{ease_in_cubic, ease_in_out_cubic};

/// Content/veil reveal after HWND settle (0 = veiled, 1 = clear).
pub fn settle_reveal(elapsed_ms: u64, duration_ms: u64) -> f32 {
    phase_progress(elapsed_ms, duration_ms)
}

/// EnterImmersive top-veil alpha (1 = solid black, 0 = clear).
pub fn enter_veil_amount(elapsed_ms: u64) -> f32 {
    let t = phase_progress_linear(elapsed_ms, ENTER_REVEAL_MS);
    1.0 - ease_in_out_cubic(t)
}

/// Art fade-in progress (0 = invisible, 1 = opaque) — ease-out for game-change crossfade.
pub fn art_fade_progress(elapsed_ms: u64) -> f32 {
    phase_progress(elapsed_ms, ART_FADE_MS)
}

/// Ambient → splash after enter veil — ease-in-out so the rise is visible, not front-loaded.
pub fn enter_art_fade_progress(elapsed_ms: u64) -> f32 {
    let t = phase_progress_linear(elapsed_ms, ART_FADE_MS);
    ease_in_out_cubic(t)
}

fn backdrop_pose_theta(elapsed_ms: u64) -> f32 {
    elapsed_ms as f32 / BACKDROP_ZOOM_PERIOD_MS as f32 * std::f32::consts::TAU * kenburns_speed()
}

/// Continuous ken-burns scale (always ≥ [`BACKDROP_SCALE_MIN`]).
///
/// Starts at the zoom floor (centered pan) and eases out — `-cos` so velocity is 0 at t=0.
/// Debug builds scale the excursion via [`kenburns_amp_scale`] (1.0 = shipped).
pub fn backdrop_pose_scale(elapsed_ms: u64) -> f32 {
    let wave = -backdrop_pose_theta(elapsed_ms).cos();
    (BACKDROP_SCALE_BASE + BACKDROP_SCALE_AMP * kenburns_amp_scale() * wave).max(BACKDROP_SCALE_MIN)
}

/// Normalized pan offsets in `[-1, 1]`, locked to the zoom period.
///
/// Starts centered (`0,0`); diagonal drift via opposite-signed Y.
pub fn backdrop_pose_offset(elapsed_ms: u64) -> (f32, f32) {
    let ox = backdrop_pose_theta(elapsed_ms).sin();
    let oy = -ox;
    (ox, oy)
}

/// Convert normalized pan into pixels from the drawn Cover rect vs stage.
///
/// Travel is staged in **stage fractions** (so wide Cover overflow cannot make X
/// race), then clamped to available overflow so the clip never shows empty edges.
pub fn backdrop_pose_pixels(
    ox_norm: f32,
    oy_norm: f32,
    drawn_w: f32,
    drawn_h: f32,
    stage_w: f32,
    stage_h: f32,
) -> (f32, f32) {
    let avail_x = (drawn_w - stage_w).max(0.0) * 0.5 * BACKDROP_PAN_OVERFLOW_CAP;
    let avail_y = (drawn_h - stage_h).max(0.0) * 0.5 * BACKDROP_PAN_OVERFLOW_CAP;
    // Debug builds scale travel via [`kenburns_pan_scale`] (1.0 = shipped);
    // the overflow cap still applies, so boosted pan can never show edges.
    let headroom_x = (stage_w * BACKDROP_PAN_STAGE_X * kenburns_pan_scale()).min(avail_x);
    let headroom_y = (stage_h * BACKDROP_PAN_STAGE_Y * kenburns_pan_scale()).min(avail_y);
    (
        ox_norm.clamp(-1.0, 1.0) * headroom_x,
        oy_norm.clamp(-1.0, 1.0) * headroom_y,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_label_is_close_unless_editing() {
        assert_eq!(cancel_circle_label(false), "Close");
        assert_eq!(cancel_circle_label(true), "Cancel");
    }

    #[test]
    fn dock_expand_collapse_targets() {
        assert_eq!(dock_expand_target(false), Some(true));
        assert_eq!(dock_expand_target(true), None);
        assert_eq!(dock_collapse_target(true), Some(false));
        assert_eq!(dock_collapse_target(false), None);
    }

    #[test]
    fn dock_overlay_scale_dim_width() {
        assert!((dock_stage_scale(0.0) - 1.0).abs() < 0.001);
        assert!((dock_stage_scale(1.0) - 0.88).abs() < 0.001);
        assert!((dock_stage_dim(0.0) - 0.0).abs() < 0.001);
        assert!((dock_stage_dim(1.0) - 0.45).abs() < 0.001);
    }

    #[test]
    fn settle_reveal_eases_out() {
        assert!((settle_reveal(0, 200) - 0.0).abs() < 0.001);
        assert!(settle_reveal(100, 200) > 0.5);
        assert!((settle_reveal(200, 200) - 1.0).abs() < 0.001);
        assert!((settle_reveal(999, 200) - 1.0).abs() < 0.001);
    }

    #[test]
    fn art_fades_use_eased_curves() {
        assert!((art_fade_progress(0) - 0.0).abs() < 0.001);
        assert!((art_fade_progress(ART_FADE_MS) - 1.0).abs() < 0.001);
        assert!((enter_art_fade_progress(0) - 0.0).abs() < 0.001);
        assert!((enter_art_fade_progress(ART_FADE_MS) - 1.0).abs() < 0.001);
        assert!((enter_art_fade_progress(ART_FADE_MS / 2) - 0.5).abs() < 0.001);
        // Ease-out is already ~0.875 at midpoint — enter splash must stay near half.
        assert!(art_fade_progress(ART_FADE_MS / 2) > 0.8);
    }

    #[test]
    fn backdrop_pose_helpers() {
        with_pose_test_lock(|| {
            backdrop_pose_helpers_inner();
        });
    }

    fn backdrop_pose_helpers_inner() {
        const {
            assert!(BACKDROP_SCALE_MIN >= 1.0);
            assert!(BACKDROP_SCALE_BASE - BACKDROP_SCALE_AMP <= BACKDROP_SCALE_MIN + 0.001);
        }
        // Starts at zoom floor, pan centered, rest (d(scale)/dt = 0 via -cos).
        let s0 = backdrop_pose_scale(0);
        let floor = (BACKDROP_SCALE_BASE - BACKDROP_SCALE_AMP).max(BACKDROP_SCALE_MIN);
        assert!((s0 - floor).abs() < 0.001);
        let (x0, y0) = backdrop_pose_offset(0);
        assert!(x0.abs() < 0.001 && y0.abs() < 0.001);
        // Quarter turn: pan at +X / −Y peak, zoom at mid base.
        let mid = BACKDROP_ZOOM_PERIOD_MS / 4;
        let (x1, y1) = backdrop_pose_offset(mid);
        assert!(x1 > 0.9);
        assert!((y1 + x1).abs() < 0.001);
        assert!((backdrop_pose_scale(mid) - BACKDROP_SCALE_BASE).abs() < 0.001);
        // Half turn: zoom peak, pan back through center.
        let half = BACKDROP_ZOOM_PERIOD_MS / 2;
        let peak = BACKDROP_SCALE_BASE + BACKDROP_SCALE_AMP;
        assert!((backdrop_pose_scale(half) - peak).abs() < 0.001);
        let (x2, y2) = backdrop_pose_offset(half);
        assert!(x2.abs() < 0.001 && y2.abs() < 0.001);
        // Modest overflow: stage targets win (not raw overflow %).
        let (px, py) = backdrop_pose_pixels(1.0, -1.0, 220.0, 110.0, 200.0, 100.0);
        let expect_x = (200.0 * BACKDROP_PAN_STAGE_X).min(10.0 * BACKDROP_PAN_OVERFLOW_CAP);
        let expect_y = (100.0 * BACKDROP_PAN_STAGE_Y).min(5.0 * BACKDROP_PAN_OVERFLOW_CAP);
        assert!((px - expect_x).abs() < 0.001);
        assert!((py + expect_y).abs() < 0.001);
        // Huge Cover X overflow must not amplify horizontal travel.
        let (px_wide, _) = backdrop_pose_pixels(1.0, 0.0, 800.0, 110.0, 200.0, 100.0);
        assert!((px_wide - 200.0 * BACKDROP_PAN_STAGE_X).abs() < 0.001);
        const {
            assert!(BACKDROP_PAN_STAGE_X < BACKDROP_PAN_STAGE_Y);
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn kenburns_tuning_scales_pose_without_breaking_safety() {
        with_pose_test_lock(|| {
            assert!(!kenburns_tuning_active());
            // Amp 0 parks zoom at base; the min floor still holds.
            set_kenburns_amp_scale(0.0);
            assert!(kenburns_tuning_active());
            let half = BACKDROP_ZOOM_PERIOD_MS / 2;
            assert!((backdrop_pose_scale(half) - BACKDROP_SCALE_BASE).abs() < 0.001);
            assert!(backdrop_pose_scale(half) >= BACKDROP_SCALE_MIN);
            // Amp 5x widens the excursion around the same base.
            set_kenburns_amp_scale(5.0);
            let peak = BACKDROP_SCALE_BASE + BACKDROP_SCALE_AMP * 5.0;
            assert!((backdrop_pose_scale(half) - peak).abs() < 0.001);
            // Pan 0 freezes travel; boosted pan still respects overflow caps.
            set_kenburns_pan_scale(0.0);
            assert_eq!(
                backdrop_pose_pixels(1.0, -1.0, 220.0, 110.0, 200.0, 100.0),
                (0.0, 0.0)
            );
            set_kenburns_pan_scale(5.0);
            let (px, _) = backdrop_pose_pixels(1.0, 0.0, 800.0, 110.0, 200.0, 100.0);
            let avail = (800.0 - 200.0) * 0.5 * BACKDROP_PAN_OVERFLOW_CAP;
            assert!(px <= avail + 0.001);
            assert!(px > 200.0 * BACKDROP_PAN_STAGE_X);
            // Speed 0 freezes the pose clock; 2x reaches the quarter turn early.
            set_kenburns_speed(0.0);
            assert_eq!(backdrop_pose_offset(half), backdrop_pose_offset(0));
            set_kenburns_speed(2.0);
            let (x1, _) = backdrop_pose_offset(BACKDROP_ZOOM_PERIOD_MS / 8);
            assert!(x1 > 0.9);
        });
    }

    #[test]
    fn phase_durations() {
        assert_eq!(
            TransitionPhase::EnterImmersive.duration_ms(),
            ENTER_IMMERSIVE_MS
        );
        assert_eq!(ENTER_IMMERSIVE_MS, ENTER_REVEAL_MS);
        assert_eq!(ENTER_REVEAL_MS, ART_FADE_MS);
        assert!((ease_in_cubic(0.0) - 0.0).abs() < 0.001);
        assert!((ease_in_cubic(1.0) - 1.0).abs() < 0.001);
        assert!(ease_in_cubic(0.5) < 0.5);
        assert!((ease_in_out_cubic(0.0) - 0.0).abs() < 0.001);
        assert!((ease_in_out_cubic(1.0) - 1.0).abs() < 0.001);
        assert!((ease_in_out_cubic(0.5) - 0.5).abs() < 0.001);
        assert!((enter_veil_amount(0) - 1.0).abs() < 0.001);
        assert!((enter_veil_amount(ENTER_REVEAL_MS / 2) - 0.5).abs() < 0.001);
        assert!((enter_veil_amount(ENTER_REVEAL_MS) - 0.0).abs() < 0.001);
        // Ease-out would already be ~0.875 at the midpoint — enter must stay near half.
        assert!(phase_progress(ENTER_REVEAL_MS / 2, ENTER_REVEAL_MS) > 0.8);
        assert!((phase_progress_linear(0, 1000) - 0.0).abs() < 0.001);
        assert!((phase_progress_linear(500, 1000) - 0.5).abs() < 0.001);
        assert!((phase_progress_linear(1000, 1000) - 1.0).abs() < 0.001);
    }
}
