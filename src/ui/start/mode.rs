//! Pure start-screen presentation transitions (compact ↔ immersive).

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
}
