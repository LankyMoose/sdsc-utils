//! Standalone ambient preview window (debug-only Diagnostics "View immersive").
//!
//! Renders the same [`AmbientProgram`](crate::ui::shader::AmbientProgram)
//! uniforms as immersive Start (`dock_progress` 0, `veil` 0, `aperture` 1)
//! on its own 60Hz clock, so Diagnostics tuning previews at the exact
//! immersive speed without opening the Start window. Utility windows stay on
//! their slow drift; this window is the tuning-speed reference.
//!
//! The clock only advances while the preview window is open (the shell
//! subscribes to [`PREVIEW_TICK`] solely then), so a closed preview never
//! ticks and never redraws.

use crate::ui::shader::{AmbientProgram, VignetteProgram};
use iced::mouse;
use iced::widget::{container, mouse_area, shader, space, stack};
use iced::{Element, Fill};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Preview window logical size.
pub const WIDTH: f32 = 960.0;
/// Preview window logical size.
pub const HEIGHT: f32 = 600.0;

/// Preview tick cadence: 60Hz, matching immersive `StartFrame`, so motion
/// runs at the exact immersive speed.
pub const PREVIEW_TICK: Duration = Duration::from_millis(16);

/// Starting phase: matches immersive Start's first frame (`ambient_time`
/// starts at 0).
const START_PHASE: f32 = 0.0;

static PREVIEW_TIME_BITS: AtomicU32 = AtomicU32::new(START_PHASE.to_bits());
static PREVIEW_LAST_TICK: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// Current preview clock (seconds of immersive-rate ambient time).
pub fn preview_time() -> f32 {
    f32::from_bits(PREVIEW_TIME_BITS.load(Ordering::Relaxed))
}

/// Advance one [`PREVIEW_TICK`] step. Same wall-clock basis as immersive
/// `tick_anim` (per-frame capped, debug `time_scale` applied), so tuned
/// numbers transfer 1:1 even when ticks hitch or share the loop.
pub fn note_preview_tick() {
    note_preview_tick_at(Instant::now());
}

fn note_preview_tick_at(now: Instant) {
    let dt = match PREVIEW_LAST_TICK.lock().map(|mut guard| guard.replace(now)) {
        Ok(previous) => match previous {
            Some(last) => crate::ui::shader::ambient::ambient_dt_for_elapsed(
                now.saturating_duration_since(last),
            ),
            // First frame after open: one nominal step.
            None => crate::ui::shader::ambient::ambient_tick_dt(),
        },
        // Poisoned (a test panicked mid-tick): fall back to nominal.
        Err(_) => crate::ui::shader::ambient::ambient_tick_dt(),
    };
    let _ = PREVIEW_TIME_BITS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
        Some((f32::from_bits(bits) + dt).to_bits())
    });
}

/// Full-bleed immersive-rate backdrop: ambient atmosphere plus the same
/// `0.55` edge vignette the immersive stage paints over it — no veil, no
/// dock push, no art, no chrome. Both layers are corner-masked like the other
/// transparent windows. The invisible top layer makes the undecorated window
/// draggable (left-press) and closable (right-press); it paints nothing.
pub fn view<'a, Message>(on_drag: Message, on_close: Message) -> Element<'a, Message>
where
    Message: Clone + 'a,
{
    let radius = crate::ui::chrome::window_radius();
    let atmosphere = shader(AmbientProgram::new(preview_time(), 0.0, 0.0, 1.0).rounded(radius))
        .width(Fill)
        .height(Fill);
    let vignette = shader(VignetteProgram::new(0.55).rounded(radius))
        .width(Fill)
        .height(Fill);
    let drag_layer = mouse_area(container(space()).width(Fill).height(Fill))
        .on_press(on_drag)
        .on_right_press(on_close)
        .interaction(mouse::Interaction::Grab);
    stack![atmosphere, vignette, drag_layer]
        .width(Fill)
        .height(Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize the preview clock tests: they share the process-wide
    /// preview globals, and parallel interleavings would corrupt the
    /// explicit-instant sequences below. Nests the tune lock inside (one
    /// order everywhere, so no deadlock).
    fn with_preview_test_lock<R>(f: impl FnOnce() -> R) -> R {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap();
        crate::ui::shader::ambient::with_tune_test_lock(f)
    }

    #[test]
    fn preview_tick_tracks_wall_clock_not_call_count() {
        // Same wall-clock basis as immersive `tick_anim`: exact 16ms steps,
        // nothing for no-elapsed bursts, capped past stalls.
        with_preview_test_lock(|| {
            let t0 = Instant::now();
            note_preview_tick_at(t0);
            let before = preview_time();
            note_preview_tick_at(t0 + Duration::from_millis(16));
            assert!((preview_time() - before - 0.016).abs() < 1e-6);
            for _ in 0..10 {
                note_preview_tick_at(t0 + Duration::from_millis(16));
            }
            assert!((preview_time() - before - 0.016).abs() < 1e-6);
            note_preview_tick_at(t0 + Duration::from_secs(10));
            assert!((preview_time() - before - 0.016 - 0.032).abs() < 1e-6);
        });
    }

    #[test]
    fn preview_tick_runs_at_immersive_cadence() {
        // Same 60Hz cadence as immersive `StartFrame`.
        assert_eq!(PREVIEW_TICK, Duration::from_millis(16));
    }

    #[test]
    fn preview_view_composes_without_panicking() {
        // Atmosphere + vignette + drag layer must compose (message wiring is
        // exercised by the shell; here we only pin the stack construction).
        #[derive(Debug, Clone)]
        enum Msg {
            Drag,
            Close,
        }
        drop(view(Msg::Drag, Msg::Close));
    }
}
