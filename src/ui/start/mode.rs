//! Pure start-screen presentation transitions (compact ↔ immersive) and dock focus.

/// Windowed launcher vs fullscreen console stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StartPresentation {
    #[default]
    Compact,
    Immersive,
}

/// Outcome of an open / chord / cancel edge for the start window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImmersiveTransition {
    OpenCompact,
    OpenImmersive,
    Promote,
    Demote,
    Close,
    Noop,
}

/// In-flight HWND resize between compact and immersive layouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartTransition {
    Promoting,
    Demoting,
}

/// Choreographed phases around the hard HWND resize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionPhase {
    /// Compact chrome fading out; still 640×500.
    ExitCompact,
    /// Veil only while HWND resizes (matches [`StartTransition`]).
    Resizing,
    /// Immersive aperture/chrome opening after promote settle.
    EnterImmersive,
    /// Immersive aperture/chrome closing before demote resize.
    ExitImmersive,
    /// Compact chrome fading in after demote settle.
    EnterCompact,
}

/// Shared duration for darken-compact / reveal-immersive / darken-immersive / reveal-compact.
pub const VEIL_TRANSITION_MS: u64 = 300;
pub const EXIT_COMPACT_MS: u64 = VEIL_TRANSITION_MS;
pub const ENTER_IMMERSIVE_MS: u64 = VEIL_TRANSITION_MS;
/// Legacy grow-segment fraction (unused by the single top-veil enter path).
pub const ENTER_GROW_END: f32 = 0.28;
pub const EXIT_IMMERSIVE_MS: u64 = VEIL_TRANSITION_MS;
pub const ENTER_COMPACT_MS: u64 = VEIL_TRANSITION_MS;
/// Opacity crossfade between game backdrops.
pub const ART_FADE_MS: u64 = 450;
/// Slow ken-burns settle (scale + center) after landing on a game.
pub const BACKDROP_LAND_MS: u64 = 10_000;
/// Ken-burns start scale (must be ≥ 1 so Cover never letterboxes).
pub const BACKDROP_LAND_SCALE0: f32 = 1.0;
/// Ken-burns end scale (slow zoom-in).
pub const BACKDROP_LAND_SCALE1: f32 = 1.08;
/// Pan offsets kept at 0 so Cover stays edge-to-edge.
pub const BACKDROP_LAND_OX0: f32 = 0.0;
pub const BACKDROP_LAND_OY0: f32 = 0.0;

impl TransitionPhase {
    pub fn duration_ms(self) -> u64 {
        match self {
            Self::ExitCompact => EXIT_COMPACT_MS,
            Self::Resizing => 0,
            Self::EnterImmersive => ENTER_IMMERSIVE_MS,
            Self::ExitImmersive => EXIT_IMMERSIVE_MS,
            Self::EnterCompact => ENTER_COMPACT_MS,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ExitCompact => "exit_compact",
            Self::Resizing => "resizing",
            Self::EnterImmersive => "enter_immersive",
            Self::ExitImmersive => "exit_immersive",
            Self::EnterCompact => "enter_compact",
        }
    }
}

/// Cold open / first reopen: immersive when always-immersive pref is on.
pub fn on_open(always_immersive: bool) -> ImmersiveTransition {
    if always_immersive {
        ImmersiveTransition::OpenImmersive
    } else {
        ImmersiveTransition::OpenCompact
    }
}

/// Reopen-chord while Start is already visible: promote compact, or demote immersive
/// when always-immersive is off (toggle). Always-immersive stays put (no compact).
pub fn on_reopen_chord(
    presentation: StartPresentation,
    always_immersive: bool,
) -> ImmersiveTransition {
    match presentation {
        StartPresentation::Compact => ImmersiveTransition::Promote,
        StartPresentation::Immersive if always_immersive => ImmersiveTransition::Noop,
        StartPresentation::Immersive => ImmersiveTransition::Demote,
    }
}

/// Circle / Escape after modals are cleared.
///
/// When `always_immersive`, immersive cancel closes Start instead of demoting to compact.
pub fn on_cancel(presentation: StartPresentation, always_immersive: bool) -> ImmersiveTransition {
    match presentation {
        StartPresentation::Immersive if always_immersive => ImmersiveTransition::Close,
        StartPresentation::Immersive => ImmersiveTransition::Demote,
        StartPresentation::Compact => ImmersiveTransition::Close,
    }
}

/// Footer Circle label for the main (non-modal) start chrome.
pub fn cancel_circle_label(immersive: bool, always_immersive: bool, editing: bool) -> &'static str {
    if editing {
        "Cancel"
    } else if immersive && !always_immersive {
        "Back"
    } else {
        "Close"
    }
}

/// True when the reopen chord is exactly Circle (Cancel owns that button).
pub fn chord_is_circle_only(gesture: &[crate::domain::gesture::GestureControl]) -> bool {
    matches!(gesture, [crate::domain::gesture::GestureControl::Circle])
}

/// Compact footer can show an enter-immersive cue for this chord.
pub fn promote_gesture_usable(gesture: &[crate::domain::gesture::GestureControl]) -> bool {
    !gesture.is_empty() && !chord_is_circle_only(gesture)
}

/// Begin promote only from compact with no in-flight transition.
pub fn begin_promote(immersive: bool, transitioning: bool) -> Option<StartTransition> {
    if immersive || transitioning {
        None
    } else {
        Some(StartTransition::Promoting)
    }
}

/// True when demote must wait (HWND resize / already exiting). [`TransitionPhase::EnterImmersive`] is interruptible.
pub fn demote_blocked(hwnd_transition: bool, phase: Option<TransitionPhase>) -> bool {
    if hwnd_transition {
        return true;
    }
    match phase {
        None | Some(TransitionPhase::EnterImmersive) => false,
        Some(
            TransitionPhase::ExitCompact
            | TransitionPhase::Resizing
            | TransitionPhase::ExitImmersive
            | TransitionPhase::EnterCompact,
        ) => true,
    }
}

/// Begin demote from immersive when not [`demote_blocked`].
pub fn begin_demote(immersive: bool, blocked: bool) -> Option<StartTransition> {
    if !immersive || blocked {
        None
    } else {
        Some(StartTransition::Demoting)
    }
}

/// After resize settles: target `immersive` flag for the transition.
pub fn settle_immersive(transition: StartTransition) -> bool {
    matches!(transition, StartTransition::Promoting)
}

/// R2 while immersive: expand dock when collapsed.
pub fn dock_expand_target(expanded: bool) -> Option<bool> {
    if expanded { None } else { Some(true) }
}

/// L2 while immersive: collapse dock when expanded.
pub fn dock_collapse_target(expanded: bool) -> Option<bool> {
    if !expanded { None } else { Some(false) }
}

/// Stage scale while the controllers drawer opens (1 → ~12% smaller).
pub fn dock_stage_scale(progress: f32) -> f32 {
    let t = progress.clamp(0.0, 1.0);
    1.0 - 0.12 * t
}

/// Dim overlay alpha over the games stage while the drawer opens.
pub fn dock_stage_dim(progress: f32) -> f32 {
    progress.clamp(0.0, 1.0) * 0.45
}

/// Overlay drawer width from thin peek → expanded panel.
pub fn dock_panel_width(progress: f32, peek: f32, expanded: f32) -> f32 {
    let t = progress.clamp(0.0, 1.0);
    peek + (expanded - peek) * t
}

/// Ease-out cubic progress 0..=1 for elapsed/duration.
pub fn phase_progress(elapsed_ms: u64, duration_ms: u64) -> f32 {
    if duration_ms == 0 {
        return 1.0;
    }
    let t = (elapsed_ms as f32 / duration_ms as f32).clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Linear 0..=1 elapsed fraction (no easing).
pub fn phase_progress_linear(elapsed_ms: u64, duration_ms: u64) -> f32 {
    if duration_ms == 0 {
        return 1.0;
    }
    (elapsed_ms as f32 / duration_ms as f32).clamp(0.0, 1.0)
}

/// Ease-in cubic — slow start, used for enter immersive chrome reveal.
pub fn ease_in_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t
}

/// Content/veil reveal after HWND settle (0 = veiled, 1 = clear).
pub fn settle_reveal(elapsed_ms: u64, duration_ms: u64) -> f32 {
    phase_progress(elapsed_ms, duration_ms)
}

/// Art fade-in progress (0 = invisible, 1 = opaque).
pub fn art_fade_progress(elapsed_ms: u64) -> f32 {
    phase_progress(elapsed_ms, ART_FADE_MS)
}

/// Backdrop land zoom progress (0 = start pose, 1 = settled).
pub fn backdrop_land_progress(elapsed_ms: u64) -> f32 {
    phase_progress(elapsed_ms, BACKDROP_LAND_MS)
}

/// Incoming backdrop scale for land progress (1.0 → 1.08 zoom-in).
pub fn backdrop_land_scale(progress: f32) -> f32 {
    let t = progress.clamp(0.0, 1.0);
    BACKDROP_LAND_SCALE0 + (BACKDROP_LAND_SCALE1 - BACKDROP_LAND_SCALE0) * t
}

/// Incoming backdrop offset (drifts to 0,0).
pub fn backdrop_land_offset(progress: f32) -> (f32, f32) {
    let t = progress.clamp(0.0, 1.0);
    let inv = 1.0 - t;
    (BACKDROP_LAND_OX0 * inv, BACKDROP_LAND_OY0 * inv)
}

/// Early EnterImmersive grow segment progress, or `None` once iris/chrome begin.
pub fn enter_grow_progress(phase_progress: f32) -> Option<f32> {
    let t = phase_progress.clamp(0.0, 1.0);
    if t < ENTER_GROW_END {
        Some((t / ENTER_GROW_END).clamp(0.0, 1.0))
    } else {
        None
    }
}

/// Remap EnterImmersive progress so iris/chrome run over the post-grow remainder.
pub fn enter_immersive_chrome_progress(phase_progress: f32) -> f32 {
    let t = phase_progress.clamp(0.0, 1.0);
    if t <= ENTER_GROW_END {
        0.0
    } else {
        ((t - ENTER_GROW_END) / (1.0 - ENTER_GROW_END).max(0.001)).clamp(0.0, 1.0)
    }
}

/// Island → fill scale during the compact-grow segment.
pub fn enter_grow_scale(grow: f32, island: f32) -> f32 {
    let g = grow.clamp(0.0, 1.0);
    let island = island.clamp(0.12, 0.95);
    island + (1.0 - island) * g
}

/// Veil during grow: starts solid, lifts mid-segment, returns near-black before iris.
pub fn enter_grow_veil(grow: f32) -> f32 {
    let g = grow.clamp(0.0, 1.0);
    if g < 0.5 {
        1.0 - 0.4 * (g * 2.0)
    } else {
        0.6 + 0.38 * ((g - 0.5) * 2.0)
    }
}

/// Promote enter: aperture 0→1 from phase progress.
pub fn enter_aperture(progress: f32) -> f32 {
    progress.clamp(0.0, 1.0)
}

/// Demote exit: aperture 1→0 from phase progress.
pub fn exit_aperture(progress: f32) -> f32 {
    1.0 - progress.clamp(0.0, 1.0)
}

/// Chrome scale during enter immersive (0.94 → 1.0).
pub fn enter_chrome_scale(progress: f32) -> f32 {
    0.94 + 0.06 * progress.clamp(0.0, 1.0)
}

/// Chrome scale during exit immersive (1.0 → 0.96).
pub fn exit_chrome_scale(progress: f32) -> f32 {
    1.0 - 0.04 * progress.clamp(0.0, 1.0)
}

/// Staggered chrome opacity: header leads, footer lags slightly.
pub fn chrome_stagger(progress: f32, lag: f32) -> f32 {
    ((progress - lag) / (1.0 - lag).max(0.001)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::gesture::GestureControl;

    #[test]
    fn open_follows_immersive_pref() {
        assert_eq!(on_open(false), ImmersiveTransition::OpenCompact);
        assert_eq!(on_open(true), ImmersiveTransition::OpenImmersive);
    }

    #[test]
    fn chord_toggles_when_not_always_immersive() {
        assert_eq!(
            on_reopen_chord(StartPresentation::Compact, false),
            ImmersiveTransition::Promote
        );
        assert_eq!(
            on_reopen_chord(StartPresentation::Immersive, false),
            ImmersiveTransition::Demote
        );
        assert_eq!(
            on_reopen_chord(StartPresentation::Immersive, true),
            ImmersiveTransition::Noop
        );
        assert_eq!(
            on_reopen_chord(StartPresentation::Compact, true),
            ImmersiveTransition::Promote
        );
    }

    #[test]
    fn cancel_demotes_or_closes() {
        assert_eq!(
            on_cancel(StartPresentation::Immersive, false),
            ImmersiveTransition::Demote
        );
        assert_eq!(
            on_cancel(StartPresentation::Immersive, true),
            ImmersiveTransition::Close
        );
        assert_eq!(
            on_cancel(StartPresentation::Compact, false),
            ImmersiveTransition::Close
        );
        assert_eq!(
            on_cancel(StartPresentation::Compact, true),
            ImmersiveTransition::Close
        );
        assert_eq!(cancel_circle_label(true, false, false), "Back");
        assert_eq!(cancel_circle_label(true, true, false), "Close");
        assert_eq!(cancel_circle_label(false, false, false), "Close");
        assert_eq!(cancel_circle_label(true, false, true), "Cancel");
        assert!(promote_gesture_usable(&[GestureControl::Ps]));
        assert!(!promote_gesture_usable(&[]));
        assert!(!promote_gesture_usable(&[GestureControl::Circle]));
    }

    #[test]
    fn circle_only_chord_detected() {
        assert!(chord_is_circle_only(&[GestureControl::Circle]));
        assert!(!chord_is_circle_only(&[GestureControl::Ps]));
        assert!(!chord_is_circle_only(&[
            GestureControl::Circle,
            GestureControl::Ps
        ]));
        assert!(!chord_is_circle_only(&[]));
    }

    #[test]
    fn promote_demote_settle_ordering() {
        assert_eq!(
            begin_promote(false, false),
            Some(StartTransition::Promoting)
        );
        assert_eq!(begin_promote(true, false), None);
        assert_eq!(begin_promote(false, true), None);
        assert_eq!(begin_demote(true, false), Some(StartTransition::Demoting));
        assert_eq!(begin_demote(false, false), None);
        assert_eq!(begin_demote(true, true), None);
        assert!(!demote_blocked(false, None));
        assert!(!demote_blocked(
            false,
            Some(TransitionPhase::EnterImmersive)
        ));
        assert!(demote_blocked(true, Some(TransitionPhase::EnterImmersive)));
        assert!(demote_blocked(false, Some(TransitionPhase::ExitImmersive)));
        assert!(demote_blocked(false, Some(TransitionPhase::Resizing)));
        assert!(settle_immersive(StartTransition::Promoting));
        assert!(!settle_immersive(StartTransition::Demoting));
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
        assert!((dock_panel_width(0.0, 72.0, 340.0) - 72.0).abs() < 0.001);
        assert!((dock_panel_width(1.0, 72.0, 340.0) - 340.0).abs() < 0.001);
    }

    #[test]
    fn settle_reveal_eases_out() {
        assert!((settle_reveal(0, 200) - 0.0).abs() < 0.001);
        assert!(settle_reveal(100, 200) > 0.5);
        assert!((settle_reveal(200, 200) - 1.0).abs() < 0.001);
        assert!((settle_reveal(999, 200) - 1.0).abs() < 0.001);
    }

    #[test]
    fn aperture_and_chrome_choreography() {
        assert!((enter_aperture(0.0) - 0.0).abs() < 0.001);
        assert!((enter_aperture(1.0) - 1.0).abs() < 0.001);
        assert!((exit_aperture(0.0) - 1.0).abs() < 0.001);
        assert!((exit_aperture(1.0) - 0.0).abs() < 0.001);
        assert!((enter_chrome_scale(0.0) - 0.94).abs() < 0.001);
        assert!((enter_chrome_scale(1.0) - 1.0).abs() < 0.001);
        assert!(chrome_stagger(0.2, 0.15) < chrome_stagger(0.8, 0.15));
        assert!((art_fade_progress(0) - 0.0).abs() < 0.001);
        assert!((art_fade_progress(ART_FADE_MS) - 1.0).abs() < 0.001);
        assert!(enter_grow_progress(0.0).is_some());
        assert!(enter_grow_progress(ENTER_GROW_END).is_none());
        assert!((enter_immersive_chrome_progress(ENTER_GROW_END) - 0.0).abs() < 0.001);
        assert!((enter_immersive_chrome_progress(1.0) - 1.0).abs() < 0.001);
        assert!((enter_grow_scale(0.0, 0.4) - 0.4).abs() < 0.001);
        assert!((enter_grow_scale(1.0, 0.4) - 1.0).abs() < 0.001);
        assert!((enter_grow_veil(0.0) - 1.0).abs() < 0.001);
        assert!(enter_grow_veil(0.5) < enter_grow_veil(0.0));
        assert!(enter_grow_veil(1.0) > enter_grow_veil(0.5));
    }

    #[test]
    fn backdrop_land_zoom_helpers() {
        assert!((backdrop_land_scale(0.0) - BACKDROP_LAND_SCALE0).abs() < 0.001);
        assert!((backdrop_land_scale(1.0) - BACKDROP_LAND_SCALE1).abs() < 0.001);
        const {
            assert!(BACKDROP_LAND_SCALE0 >= 1.0);
        }
        let (x0, y0) = backdrop_land_offset(0.0);
        assert!((x0 - BACKDROP_LAND_OX0).abs() < 0.001);
        assert!((y0 - BACKDROP_LAND_OY0).abs() < 0.001);
        let (x1, y1) = backdrop_land_offset(1.0);
        assert!(x1.abs() < 0.001 && y1.abs() < 0.001);
        assert!((backdrop_land_progress(0) - 0.0).abs() < 0.001);
        assert!((backdrop_land_progress(BACKDROP_LAND_MS) - 1.0).abs() < 0.001);
        assert!(backdrop_land_progress(BACKDROP_LAND_MS / 2) > 0.5);
    }

    #[test]
    fn phase_durations() {
        assert_eq!(TransitionPhase::ExitCompact.duration_ms(), EXIT_COMPACT_MS);
        assert_eq!(
            TransitionPhase::EnterImmersive.duration_ms(),
            ENTER_IMMERSIVE_MS
        );
        assert_eq!(
            TransitionPhase::ExitImmersive.duration_ms(),
            EXIT_IMMERSIVE_MS
        );
        assert_eq!(
            TransitionPhase::EnterCompact.duration_ms(),
            ENTER_COMPACT_MS
        );
        assert_eq!(ENTER_IMMERSIVE_MS, VEIL_TRANSITION_MS);
        assert_eq!(EXIT_IMMERSIVE_MS, VEIL_TRANSITION_MS);
        assert_eq!(ENTER_COMPACT_MS, VEIL_TRANSITION_MS);
        assert!((ease_in_cubic(0.0) - 0.0).abs() < 0.001);
        assert!((ease_in_cubic(1.0) - 1.0).abs() < 0.001);
        assert!(ease_in_cubic(0.5) < 0.5);
        assert!((phase_progress_linear(0, 1000) - 0.0).abs() < 0.001);
        assert!((phase_progress_linear(500, 1000) - 0.5).abs() < 0.001);
        assert!((phase_progress_linear(1000, 1000) - 1.0).abs() < 0.001);
    }
}
