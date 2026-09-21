//! Configurable DualSense chord used to reopen the start screen.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

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

/// Default reopen chord: L2 + R2 + L3 + R3.
pub fn default_gesture() -> Vec<GestureControl> {
    vec![
        GestureControl::L2,
        GestureControl::R2,
        GestureControl::L3,
        GestureControl::R3,
    ]
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

/// Rising-edge detector for a held chord (superset OK).
#[derive(Debug, Clone, Default)]
pub struct GestureDetector {
    armed: bool,
}

impl GestureDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true once when every required control is held, then stays silent
    /// until at least one required control is released.
    pub fn update(&mut self, required: &[GestureControl], held: &BTreeSet<GestureControl>) -> bool {
        if required.is_empty() {
            self.armed = false;
            return false;
        }
        let matched = required.iter().all(|c| held.contains(c));
        if matched {
            if !self.armed {
                self.armed = true;
                return true;
            }
            false
        } else {
            self.armed = false;
            false
        }
    }

    pub fn reset(&mut self) {
        self.armed = false;
    }
}

/// Peak-set recorder for Settings → Record gesture.
#[derive(Debug, Clone, Default)]
pub struct GestureRecorder {
    active: bool,
    saw_input: bool,
    peak: BTreeSet<GestureControl>,
}

impl GestureRecorder {
    pub fn start(&mut self) {
        self.active = true;
        self.saw_input = false;
        self.peak.clear();
    }

    pub fn cancel(&mut self) {
        self.active = false;
        self.saw_input = false;
        self.peak.clear();
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn peak(&self) -> &BTreeSet<GestureControl> {
        &self.peak
    }

    /// Feed the current held set. When the user releases after holding something,
    /// returns the peak simultaneous set (at least one control).
    pub fn update(&mut self, held: &BTreeSet<GestureControl>) -> Option<Vec<GestureControl>> {
        if !self.active {
            return None;
        }
        if !held.is_empty() {
            self.saw_input = true;
            for control in held {
                self.peak.insert(*control);
            }
            return None;
        }
        if self.saw_input && !self.peak.is_empty() {
            let result: Vec<_> = self.peak.iter().copied().collect();
            self.cancel();
            Some(result)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_gesture_is_triggers_and_sticks() {
        assert_eq!(
            default_gesture(),
            vec![
                GestureControl::L2,
                GestureControl::R2,
                GestureControl::L3,
                GestureControl::R3,
            ]
        );
    }

    #[test]
    fn format_sorts_and_joins() {
        let label = format_gesture(&[GestureControl::R3, GestureControl::L2]);
        assert_eq!(label, "L2 + R3");
        assert_eq!(format_gesture(&[]), "None");
    }

    #[test]
    fn detector_fires_once_on_rising_edge() {
        let required = default_gesture();
        let mut detector = GestureDetector::new();
        let mut held = BTreeSet::new();
        assert!(!detector.update(&required, &held));

        held.insert(GestureControl::L2);
        held.insert(GestureControl::R2);
        assert!(!detector.update(&required, &held));

        held.insert(GestureControl::L3);
        held.insert(GestureControl::R3);
        assert!(detector.update(&required, &held));
        assert!(!detector.update(&required, &held));

        // Extra buttons still match (superset OK).
        held.insert(GestureControl::Cross);
        assert!(!detector.update(&required, &held));

        held.remove(&GestureControl::L3);
        assert!(!detector.update(&required, &held));
        held.insert(GestureControl::L3);
        assert!(detector.update(&required, &held));
    }

    #[test]
    fn empty_gesture_never_fires() {
        let mut detector = GestureDetector::new();
        let held: BTreeSet<_> = [GestureControl::L2].into_iter().collect();
        assert!(!detector.update(&[], &held));
    }

    #[test]
    fn recorder_keeps_peak_across_staggered_release() {
        let mut recorder = GestureRecorder::default();
        recorder.start();
        let mut held = BTreeSet::new();
        held.insert(GestureControl::L2);
        held.insert(GestureControl::R2);
        assert!(recorder.update(&held).is_none());
        held.insert(GestureControl::L3);
        held.insert(GestureControl::R3);
        assert!(recorder.update(&held).is_none());
        held.remove(&GestureControl::R3);
        assert!(recorder.update(&held).is_none());
        held.clear();
        let peak = recorder.update(&held).expect("finished");
        assert_eq!(
            peak.into_iter().collect::<BTreeSet<_>>(),
            [
                GestureControl::L2,
                GestureControl::R2,
                GestureControl::L3,
                GestureControl::R3,
            ]
            .into_iter()
            .collect()
        );
        assert!(!recorder.is_active());
    }
}
