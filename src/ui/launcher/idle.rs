//! Immersive inactive / sleep dim phases (pure helpers + paint alpha).

use super::mode::{ENTER_REVEAL_MS, ease_in_out_cubic, phase_progress_linear};

/// Logical immersive idle level (overlay target).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdlePhase {
    #[default]
    Active,
    Inactive,
    Sleep,
}

impl IdlePhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Inactive => "idle",
            Self::Sleep => "sleep",
        }
    }

    /// Settled overlay alpha for this phase (`inactive_dim` is 0..=1).
    pub fn target_alpha(self, inactive_dim: f32) -> f32 {
        match self {
            Self::Active => 0.0,
            Self::Inactive => inactive_dim.clamp(0.0, 1.0),
            Self::Sleep => 1.0,
        }
    }
}

/// Shared dim crossfade duration (matches enter-reveal ceremony).
pub const IDLE_DIM_MS: u64 = ENTER_REVEAL_MS;

/// Pick the idle phase for elapsed seconds since last activity.
pub fn phase_for_idle_secs(idle_secs: f32, inactive_secs: u32, sleep_secs: u32) -> IdlePhase {
    let sleep = sleep_secs as f32;
    let inactive = inactive_secs as f32;
    if idle_secs >= sleep {
        IdlePhase::Sleep
    } else if idle_secs >= inactive {
        IdlePhase::Inactive
    } else {
        IdlePhase::Active
    }
}

/// Ease-in-out alpha from `from` → `to` over [`IDLE_DIM_MS`].
pub fn dim_alpha(from: f32, to: f32, elapsed_ms: u64) -> f32 {
    let t = ease_in_out_cubic(phase_progress_linear(elapsed_ms, IDLE_DIM_MS));
    from + (to - from) * t
}

/// True when the dim crossfade has finished.
pub fn dim_anim_done(elapsed_ms: u64) -> bool {
    elapsed_ms >= IDLE_DIM_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_for_idle_secs_ordering() {
        assert_eq!(phase_for_idle_secs(0.0, 30, 180), IdlePhase::Active);
        assert_eq!(phase_for_idle_secs(29.9, 30, 180), IdlePhase::Active);
        assert_eq!(phase_for_idle_secs(30.0, 30, 180), IdlePhase::Inactive);
        assert_eq!(phase_for_idle_secs(179.9, 30, 180), IdlePhase::Inactive);
        assert_eq!(phase_for_idle_secs(180.0, 30, 180), IdlePhase::Sleep);
    }

    #[test]
    fn target_alpha_inactive_and_sleep() {
        assert_eq!(IdlePhase::Active.target_alpha(0.3), 0.0);
        assert!((IdlePhase::Inactive.target_alpha(0.3) - 0.3).abs() < f32::EPSILON);
        assert_eq!(IdlePhase::Sleep.target_alpha(0.3), 1.0);
        assert!((IdlePhase::Inactive.target_alpha(1.5) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn dim_alpha_eases_endpoints() {
        assert!((dim_alpha(0.0, 1.0, 0) - 0.0).abs() < 1e-5);
        assert!((dim_alpha(0.0, 1.0, IDLE_DIM_MS) - 1.0).abs() < 1e-5);
        let mid = dim_alpha(0.0, 1.0, IDLE_DIM_MS / 2);
        assert!((mid - 0.5).abs() < 0.02);
        assert!(dim_anim_done(IDLE_DIM_MS));
        assert!(!dim_anim_done(IDLE_DIM_MS - 1));
    }
}
