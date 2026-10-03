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

/// Cold open / first reopen: prefer immersive when the pref is on.
pub fn on_open(prefer_immersive: bool) -> ImmersiveTransition {
    if prefer_immersive {
        ImmersiveTransition::OpenImmersive
    } else {
        ImmersiveTransition::OpenCompact
    }
}

/// Second reopen-chord press while Start is already visible.
pub fn on_reopen_chord(presentation: StartPresentation) -> ImmersiveTransition {
    match presentation {
        StartPresentation::Compact => ImmersiveTransition::Promote,
        StartPresentation::Immersive => ImmersiveTransition::Noop,
    }
}

/// Circle / Escape after modals are cleared.
pub fn on_cancel(presentation: StartPresentation) -> ImmersiveTransition {
    match presentation {
        StartPresentation::Immersive => ImmersiveTransition::Demote,
        StartPresentation::Compact => ImmersiveTransition::Close,
    }
}

/// True when the reopen chord is exactly Circle (Cancel owns that button).
pub fn chord_is_circle_only(gesture: &[crate::domain::gesture::GestureControl]) -> bool {
    matches!(gesture, [crate::domain::gesture::GestureControl::Circle])
}

/// Begin promote only from compact with no in-flight transition.
pub fn begin_promote(immersive: bool, transitioning: bool) -> Option<StartTransition> {
    if immersive || transitioning {
        None
    } else {
        Some(StartTransition::Promoting)
    }
}

/// Begin demote only from immersive with no in-flight transition.
pub fn begin_demote(immersive: bool, transitioning: bool) -> Option<StartTransition> {
    if !immersive || transitioning {
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
    fn chord_promotes_compact_only() {
        assert_eq!(
            on_reopen_chord(StartPresentation::Compact),
            ImmersiveTransition::Promote
        );
        assert_eq!(
            on_reopen_chord(StartPresentation::Immersive),
            ImmersiveTransition::Noop
        );
    }

    #[test]
    fn cancel_demotes_or_closes() {
        assert_eq!(
            on_cancel(StartPresentation::Immersive),
            ImmersiveTransition::Demote
        );
        assert_eq!(
            on_cancel(StartPresentation::Compact),
            ImmersiveTransition::Close
        );
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
}
