//! Shared navigation policy above the input mechanics.
//!
//! `NavBank`/`NavStepper` answer "what did the pad do?" (arming, deadzone,
//! repeat, diagonal dominance). This module answers the surface questions:
//! can this list move, does an edge reject, did an action run, and which single
//! cue belongs to the accepted outcome. Lists never wrap: `Wrap::Never` is the
//! only policy, so a boundary is consumed for free.

use crate::domain::pad::{NavAction, NavBank, NavReading, PadTickResult};
use std::time::Instant;

/// One list axis. A direction only drives a list on its own axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavAxis {
    Horizontal,
    Vertical,
}

/// Explicit direction, independent of the pad action vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavDir {
    Left,
    Right,
    Up,
    Down,
}

impl NavDir {
    pub fn axis(self) -> NavAxis {
        match self {
            Self::Left | Self::Right => NavAxis::Horizontal,
            Self::Up | Self::Down => NavAxis::Vertical,
        }
    }

    pub fn delta(self) -> i32 {
        match self {
            Self::Left | Self::Up => -1,
            Self::Right | Self::Down => 1,
        }
    }

    pub fn from_action(action: NavAction) -> Option<Self> {
        match action {
            NavAction::Left => Some(Self::Left),
            NavAction::Right => Some(Self::Right),
            NavAction::Up => Some(Self::Up),
            NavAction::Down => Some(Self::Down),
            _ => None,
        }
    }

    pub fn to_action(self) -> NavAction {
        match self {
            Self::Left => NavAction::Left,
            Self::Right => NavAction::Right,
            Self::Up => NavAction::Up,
            Self::Down => NavAction::Down,
        }
    }
}

/// Boundary policy. No navigation list wraps; the variant exists so a future
/// surface must opt in explicitly rather than inheriting `rem_euclid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NavWrap {
    #[default]
    Never,
}

/// Which cue an accepted outcome owns. The app maps this to its sound/haptic
/// kinds; domain code never depends on the platform audio layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavCue {
    Nav,
    Action,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveOutcome {
    Moved { cue: NavCue },
    Rejected,
}

impl MoveOutcome {
    pub fn moved(self) -> bool {
        matches!(self, Self::Moved { .. })
    }

    pub fn cue(self) -> Option<NavCue> {
        match self {
            Self::Moved { cue } => Some(cue),
            Self::Rejected => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOutcome {
    Accepted { cue: NavCue },
    Rejected,
}

impl ActionOutcome {
    pub fn accepted(self) -> bool {
        matches!(self, Self::Accepted { .. })
    }

    pub fn cue(self) -> Option<NavCue> {
        match self {
            Self::Accepted { cue } => Some(cue),
            Self::Rejected => None,
        }
    }
}

/// One focus list. `focus` is caller-owned state; `try_move` is the only
/// mutation and clamps at both ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavList {
    pub focus: usize,
    pub len: usize,
    pub axis: NavAxis,
    pub wrap: NavWrap,
    pub enabled: bool,
}

impl NavList {
    pub fn new(len: usize, axis: NavAxis) -> Self {
        Self {
            focus: 0,
            len,
            axis,
            wrap: NavWrap::Never,
            enabled: true,
        }
    }

    pub fn with_focus(mut self, focus: usize) -> Self {
        self.focus = focus.min(self.len.saturating_sub(1));
        self
    }

    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// Return true when `focus` moved. Empty, disabled, wrong-axis, and edge
    /// inputs are all rejected without changing state.
    pub fn try_move(&mut self, dir: NavDir) -> MoveOutcome {
        if !self.enabled || self.len == 0 || dir.axis() != self.axis {
            return MoveOutcome::Rejected;
        }
        let next = (self.focus as i32 + dir.delta()).clamp(0, self.len as i32 - 1) as usize;
        if next == self.focus {
            return MoveOutcome::Rejected;
        }
        self.focus = next;
        MoveOutcome::Moved { cue: NavCue::Nav }
    }

    /// Set focus directly (focus carry-over, pointer selection). Returns true
    /// when the index changed.
    pub fn set_focus(&mut self, index: usize) -> bool {
        let next = index.min(self.len.saturating_sub(1));
        if next == self.focus {
            return false;
        }
        self.focus = next;
        true
    }
}

/// A surface exposes its focus lists and state mutations; feedback policy
/// stays here. Surfaces never call `play_ui_cue` for movement.
pub trait NavSurface {
    fn try_move(&mut self, dir: NavDir) -> MoveOutcome;

    fn try_action(&mut self, action: NavAction) -> ActionOutcome;

    /// Optional analog scroll. Returns true when the surface consumed it.
    fn scroll(&mut self, _delta: f32) -> bool {
        false
    }
}

/// Per-pad input mechanics plus the policy entry points. The bank is an
/// implementation detail: surfaces ask the dispatcher to tick and to resolve
/// moves, then apply the returned outcome.
#[derive(Debug, Clone, Default)]
pub struct NavDispatcher {
    bank: NavBank,
}

impl NavDispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn prepare_on_open(&mut self, arm_immediately: bool) {
        self.bank.prepare_on_open(arm_immediately);
    }

    pub fn reset(&mut self) {
        self.bank.reset();
    }

    pub fn cancel_holds(&mut self) {
        self.bank.cancel_holds();
    }

    /// Menu-profile tick shared by list surfaces. Holds are inert here; the
    /// face/action paths stay on `PadActionGate` and the surface reducers.
    pub fn tick(
        &mut self,
        readings: &[NavReading],
        now: Instant,
        allow_nav_move: bool,
        horizontal_nav: bool,
    ) -> PadTickResult {
        self.bank.tick(
            readings,
            now,
            allow_nav_move,
            horizontal_nav,
            false,
            false,
            false,
            false,
            false,
        )
    }

    /// Full-profile tick for Launcher. Keeps the launcher's hold/replace/edit
    /// flags and returns the same `PadTickResult` the bank produced.
    #[allow(clippy::too_many_arguments)]
    pub fn tick_full(
        &mut self,
        readings: &[NavReading],
        now: Instant,
        allow_nav_move: bool,
        horizontal_nav: bool,
        horizontal_games_nav: bool,
        replace_confirm: bool,
        editing: bool,
        hold_cross_close: bool,
        hold_triangle_power: bool,
    ) -> PadTickResult {
        self.bank.tick(
            readings,
            now,
            allow_nav_move,
            horizontal_nav,
            horizontal_games_nav,
            replace_confirm,
            editing,
            hold_cross_close,
            hold_triangle_power,
        )
    }

    pub fn hold_progress(&self, now: Instant) -> (f32, f32) {
        self.bank.hold_progress(now)
    }

    pub fn has_hold_activity(&self) -> bool {
        self.bank.has_hold_activity()
    }

    /// Resolve a nav action against a surface. Only an accepted move carries a
    /// cue; the app has one `apply` helper for that outcome.
    pub fn dispatch_move<S: NavSurface>(
        &mut self,
        surface: &mut S,
        action: NavAction,
    ) -> MoveOutcome {
        match NavDir::from_action(action) {
            Some(dir) => surface.try_move(dir),
            None => MoveOutcome::Rejected,
        }
    }

    pub fn dispatch_action<S: NavSurface>(
        &mut self,
        surface: &mut S,
        action: NavAction,
    ) -> ActionOutcome {
        surface.try_action(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::pad::{PadId, PadSample};

    fn reading(dpad_down: bool) -> NavReading {
        NavReading {
            id: PadId("hid:test".into()),
            source: "test",
            sample: PadSample {
                dpad_down,
                ..PadSample::default()
            },
        }
    }

    #[test]
    fn lists_clamp_at_edges_and_reject_wrong_axis() {
        let mut list = NavList::new(2, NavAxis::Horizontal);
        assert_eq!(list.try_move(NavDir::Left), MoveOutcome::Rejected);
        assert_eq!(
            list.try_move(NavDir::Right),
            MoveOutcome::Moved { cue: NavCue::Nav }
        );
        assert_eq!(list.try_move(NavDir::Right), MoveOutcome::Rejected);
        assert_eq!(list.focus, 1);
        assert_eq!(list.try_move(NavDir::Down), MoveOutcome::Rejected);
        assert_eq!(
            list.try_move(NavDir::Left),
            MoveOutcome::Moved { cue: NavCue::Nav }
        );
    }

    #[test]
    fn empty_and_disabled_lists_never_move() {
        let mut empty = NavList::new(0, NavAxis::Horizontal);
        assert_eq!(empty.try_move(NavDir::Right), MoveOutcome::Rejected);
        let mut disabled = NavList::new(3, NavAxis::Horizontal).disabled();
        assert_eq!(disabled.try_move(NavDir::Right), MoveOutcome::Rejected);
        assert_eq!(disabled.focus, 0);
    }

    struct TestSurface {
        list: NavList,
        accepted: bool,
    }

    impl NavSurface for TestSurface {
        fn try_move(&mut self, dir: NavDir) -> MoveOutcome {
            self.list.try_move(dir)
        }

        fn try_action(&mut self, action: NavAction) -> ActionOutcome {
            if action == NavAction::Confirm && self.accepted {
                ActionOutcome::Accepted {
                    cue: NavCue::Action,
                }
            } else {
                ActionOutcome::Rejected
            }
        }
    }

    #[test]
    fn dispatcher_turns_actions_into_outcomes() {
        let mut dispatcher = NavDispatcher::new();
        let mut surface = TestSurface {
            list: NavList::new(2, NavAxis::Vertical),
            accepted: true,
        };
        assert_eq!(
            dispatcher.dispatch_move(&mut surface, NavAction::Down),
            MoveOutcome::Moved { cue: NavCue::Nav }
        );
        assert_eq!(
            dispatcher.dispatch_move(&mut surface, NavAction::Down),
            MoveOutcome::Rejected
        );
        assert_eq!(
            dispatcher.dispatch_action(&mut surface, NavAction::Confirm),
            ActionOutcome::Accepted {
                cue: NavCue::Action
            }
        );
        assert_eq!(
            dispatcher.dispatch_action(&mut surface, NavAction::Cancel),
            ActionOutcome::Rejected
        );
    }

    #[test]
    fn held_dpad_repeats_from_the_bank() {
        let mut dispatcher = NavDispatcher::new();
        dispatcher.prepare_on_open(true);
        let now = Instant::now();
        let down = reading(true);
        let first = dispatcher.tick(std::slice::from_ref(&down), now, true, true);
        assert_eq!(first.action, Some(NavAction::Down));
        assert_eq!(
            dispatcher
                .tick(
                    std::slice::from_ref(&down),
                    now + std::time::Duration::from_millis(500),
                    true,
                    true
                )
                .action,
            Some(NavAction::Down),
            "held input repeats from the bank clock"
        );
        let neutral = reading(false);
        let release = dispatcher.tick(
            std::slice::from_ref(&neutral),
            now + std::time::Duration::from_millis(700),
            true,
            true,
        );
        assert_eq!(release.action, None, "release does not emit a move");
    }
}
