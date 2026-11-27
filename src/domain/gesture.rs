//! DualSense reopen chord (fixed PS) and rising-edge detectors.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// Chord must stay absent this long before a sticky-hold latch clears.
///
/// Longer than one HID wake / contact bounce; shorter than a deliberate
/// release-and-press for reopen.
pub const CHORD_RELEASE_DEBOUNCE: Duration = Duration::from_millis(64);

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

/// Rising-edge detector for a held chord (superset OK).
#[derive(Debug, Clone, Default)]
pub struct GestureDetector {
    armed: bool,
}

impl GestureDetector {
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

    /// Treat the chord as already consumed so a sticky rematch does not fire
    /// until the controls are released.
    pub fn mark_armed(&mut self) {
        self.armed = true;
    }
}

/// Outcome of feeding a chord-held sample into [`ChordReleaseGate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordReleaseTick {
    /// Latch still active — do not run the rising-edge detector.
    Blocked,
    /// The release was long enough (even if the only evidence is this held
    /// sample arriving after the debounce). Callers disarm the detector first;
    /// if the current sample holds the chord, it is a fresh press.
    Cleared,
    /// No latch — rising-edge detector may run.
    Open,
}

/// After open / close, ignore the sticky chord until it has been
/// continuously absent for [`CHORD_RELEASE_DEBOUNCE`].
///
/// The gate is sample-arrival independent: the HID edge stream drops resting
/// neutral samples, so a release often has no sample at the end of the
/// debounce. A held sample that arrives after the debounce has elapsed is a
/// fresh press, not a glitch.
#[derive(Debug, Clone, Default)]
pub struct ChordReleaseGate {
    latched: bool,
    absent_since: Option<Instant>,
}

impl ChordReleaseGate {
    pub fn is_latched(&self) -> bool {
        self.latched
    }

    /// Require a debounced full release before the next rising edge.
    pub fn arm(&mut self) {
        self.latched = true;
        self.absent_since = None;
    }

    /// Clear without waiting.
    pub fn clear(&mut self) {
        self.latched = false;
        self.absent_since = None;
    }

    /// Feed whether the required chord is held on any pad this sample.
    ///
    /// When `Blocked`, callers must not call [`GestureDetector::update`] (except
    /// optionally `consume_pending_match` / `mark_armed` to keep the hold consumed).
    /// When `Cleared`, disarm the detector first; if this sample holds the
    /// chord, evaluate it as a fresh rising edge.
    ///
    /// Returns `(tick, glitch_ignored)` — `glitch_ignored` is true when the
    /// chord returns after a short absence that did not clear the latch.
    pub fn tick(&mut self, chord_held: bool, now: Instant) -> (ChordReleaseTick, bool) {
        if !self.latched {
            return (ChordReleaseTick::Open, false);
        }
        if chord_held {
            if let Some(since) = self.absent_since {
                self.absent_since = None;
                if now.saturating_duration_since(since) >= CHORD_RELEASE_DEBOUNCE {
                    // No sample arrived while the chord was absent (the HID
                    // edge stream drops rest), but the release was real and
                    // long enough: this held sample is a fresh press.
                    self.latched = false;
                    return (ChordReleaseTick::Cleared, false);
                }
                // Short bounce: still the same physical press.
                return (ChordReleaseTick::Blocked, true);
            }
            return (ChordReleaseTick::Blocked, false);
        }
        match self.absent_since {
            None => {
                self.absent_since = Some(now);
                (ChordReleaseTick::Blocked, false)
            }
            Some(since) => {
                if now.saturating_duration_since(since) >= CHORD_RELEASE_DEBOUNCE {
                    self.latched = false;
                    self.absent_since = None;
                    (ChordReleaseTick::Cleared, false)
                } else {
                    (ChordReleaseTick::Blocked, false)
                }
            }
        }
    }
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

    #[test]
    fn detector_fires_once_on_rising_edge() {
        let required = default_gesture();
        let mut detector = GestureDetector::default();
        let mut held = BTreeSet::new();
        assert!(!detector.update(&required, &held));

        held.insert(GestureControl::Cross);
        assert!(!detector.update(&required, &held));

        held.insert(GestureControl::Ps);
        assert!(detector.update(&required, &held));
        assert!(!detector.update(&required, &held));

        // Extra buttons still match (superset OK).
        held.insert(GestureControl::L2);
        assert!(!detector.update(&required, &held));

        held.remove(&GestureControl::Ps);
        assert!(!detector.update(&required, &held));
        held.insert(GestureControl::Ps);
        assert!(detector.update(&required, &held));
    }

    #[test]
    fn mark_armed_skips_current_match() {
        let required = default_gesture();
        let mut detector = GestureDetector::default();
        detector.mark_armed();
        let held: BTreeSet<_> = [GestureControl::Ps].into_iter().collect();
        assert!(!detector.update(&required, &held));
        let empty = BTreeSet::new();
        assert!(!detector.update(&required, &empty));
        assert!(detector.update(&required, &held));
    }

    #[test]
    fn empty_gesture_never_fires() {
        let mut detector = GestureDetector::default();
        let held: BTreeSet<_> = [GestureControl::L2].into_iter().collect();
        assert!(!detector.update(&[], &held));
    }

    #[test]
    fn release_gate_steady_hold_stays_blocked() {
        let mut gate = ChordReleaseGate::default();
        let t0 = Instant::now();
        gate.arm();
        for ms in [0u64, 10, 50, 100] {
            let (tick, glitch) = gate.tick(true, t0 + Duration::from_millis(ms));
            assert_eq!(tick, ChordReleaseTick::Blocked);
            assert!(!glitch);
        }
        assert!(gate.is_latched());
    }

    #[test]
    fn release_gate_one_sample_gap_then_rehold_stays_blocked() {
        let mut gate = ChordReleaseGate::default();
        let mut detector = GestureDetector::default();
        let required = default_gesture();
        let held: BTreeSet<_> = [GestureControl::Ps].into_iter().collect();
        let t0 = Instant::now();

        gate.arm();
        detector.mark_armed();
        assert_eq!(
            gate.tick(true, t0).0,
            ChordReleaseTick::Blocked,
            "opening hold"
        );
        // One-sample gap (e.g. 4ms HID wake).
        assert_eq!(
            gate.tick(false, t0 + Duration::from_millis(4)).0,
            ChordReleaseTick::Blocked
        );
        let (tick, glitch) = gate.tick(true, t0 + Duration::from_millis(8));
        assert_eq!(tick, ChordReleaseTick::Blocked);
        assert!(glitch, "short gap should report glitch ignored");
        assert!(
            !detector.update(&required, &held),
            "same press must not fire after glitch"
        );
        assert!(gate.is_latched());
    }

    #[test]
    fn release_gate_long_gap_then_rehold_is_fresh_press() {
        let mut gate = ChordReleaseGate::default();
        let mut detector = GestureDetector::default();
        let required = default_gesture();
        let held: BTreeSet<_> = [GestureControl::Ps].into_iter().collect();
        let empty = BTreeSet::new();
        let t0 = Instant::now();

        gate.arm();
        detector.mark_armed();
        assert_eq!(gate.tick(true, t0).0, ChordReleaseTick::Blocked);
        // Release seen once, then no samples while the pad rests (the HID edge
        // stream drops neutral samples). The next held sample arrives after the
        // debounce window and must count as a new press.
        assert_eq!(
            gate.tick(false, t0 + Duration::from_millis(1)).0,
            ChordReleaseTick::Blocked
        );
        let (tick, glitch) =
            gate.tick(true, t0 + Duration::from_millis(1) + CHORD_RELEASE_DEBOUNCE);
        assert_eq!(tick, ChordReleaseTick::Cleared);
        assert!(!glitch);
        assert!(!gate.is_latched());
        // Caller pattern: disarm, then evaluate the held sample.
        assert!(!detector.update(&required, &empty));
        assert!(detector.update(&required, &held));
    }

    #[test]
    fn release_gate_debounce_then_new_hold_fires() {
        let mut gate = ChordReleaseGate::default();
        let mut detector = GestureDetector::default();
        let required = default_gesture();
        let held: BTreeSet<_> = [GestureControl::Ps].into_iter().collect();
        let empty = BTreeSet::new();
        let t0 = Instant::now();

        gate.arm();
        detector.mark_armed();
        assert_eq!(gate.tick(true, t0).0, ChordReleaseTick::Blocked);

        // Launcher absent.
        assert_eq!(
            gate.tick(false, t0 + Duration::from_millis(1)).0,
            ChordReleaseTick::Blocked
        );
        // Continuous absence past debounce.
        let (tick, _) = gate.tick(
            false,
            t0 + Duration::from_millis(1) + CHORD_RELEASE_DEBOUNCE,
        );
        assert_eq!(tick, ChordReleaseTick::Cleared);
        assert!(!gate.is_latched());
        // Cleared sample disarms the detector.
        assert!(!detector.update(&required, &empty));
        // Fresh press fires.
        assert!(detector.update(&required, &held));
        assert_eq!(
            gate.tick(true, t0 + Duration::from_millis(200)).0,
            ChordReleaseTick::Open
        );
    }
}
