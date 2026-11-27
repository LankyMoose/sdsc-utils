//! DualSense reopen chord (fixed PS) and chord labels.

use serde::{Deserialize, Serialize};

/// Digital DualSense controls that can appear in a reopen gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GestureControl {
    Cross,
    Circle,
    Square,
    Triangle,
    L1,
    R1,
    L2,
    R2,
    L3,
    R3,
    Create,
    Options,
    Ps,
    Mute,
    Touchpad,
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
}

impl GestureControl {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cross => "Cross",
            Self::Circle => "Circle",
            Self::Square => "Square",
            Self::Triangle => "Triangle",
            Self::L1 => "L1",
            Self::R1 => "R1",
            Self::L2 => "L2",
            Self::R2 => "R2",
            Self::L3 => "L3",
            Self::R3 => "R3",
            Self::Create => "Create",
            Self::Options => "Options",
            Self::Ps => "PS",
            Self::Mute => "Mute",
            Self::Touchpad => "Touchpad",
            Self::DpadUp => "D-pad Up",
            Self::DpadDown => "D-pad Down",
            Self::DpadLeft => "D-pad Left",
            Self::DpadRight => "D-pad Right",
        }
    }
}

/// Default reopen chord: PS (Guide) button.
pub fn default_gesture() -> Vec<GestureControl> {
    vec![GestureControl::Ps]
}

/// Human-readable chord label (`L2 + R2 + L3 + R3`).
pub fn format_gesture(controls: &[GestureControl]) -> String {
    if controls.is_empty() {
        return "None".to_string();
    }
    let mut sorted = controls.to_vec();
    sorted.sort();
    sorted
        .into_iter()
        .map(GestureControl::as_str)
        .collect::<Vec<_>>()
        .join(" + ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_gesture_is_ps() {
        assert_eq!(default_gesture(), vec![GestureControl::Ps]);
    }

    #[test]
    fn format_sorts_and_joins() {
        let label = format_gesture(&[GestureControl::R3, GestureControl::L2]);
        assert_eq!(label, "L2 + R3");
        assert_eq!(format_gesture(&[]), "None");
    }
}
