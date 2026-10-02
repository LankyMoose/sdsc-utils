//! Pure presentation state machine for overlay toasts and deferred Start open.
//!
//! Start may open only as an effect of a legal transition (slide rest or dismiss
//! finish). [`Phase::Placing`] is event-only: `Frame` is ignored; leave only on
//! [`Event::Shown`] or [`Event::Dismiss`].

use crate::ui::layout::{TOAST_SLIDE_DURATION, advance_toast_slide};
use std::time::Duration;

/// Frame ticks while Placing before the reopen gesture is allowed again (~1s at 16ms).
/// Does not leave Placing or emit OpenStart — only lifts gesture suppress.
pub const PLACING_GESTURE_SUPPRESS_FRAMES: u32 = 60;

/// What to do after this toast reaches rest (or finishes early).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AfterToast {
    #[default]
    Nothing,
    /// Latched on the 0→1 Connected toast: open Start when safe.
    OpenStart,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    /// Remount / move / show in flight; waiting for [`Event::Shown`].
    Placing,
    SlidingIn {
        elapsed: Duration,
    },
    Resting,
    SlidingOut {
        elapsed: Duration,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Active {
    pub generation: u64,
    pub phase: Phase,
    pub after: AfterToast,
    /// Toast body for debug transition lines (e.g. "Connected").
    pub body: String,
    /// Count of Frame events while in Placing (gesture suppress budget).
    pub placing_frames: u32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum State {
    #[default]
    Idle,
    Active(Active),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Show {
        generation: u64,
        after: AfterToast,
        body: String,
    },
    Shown {
        generation: u64,
    },
    Frame {
        generation: u64,
        raw_dt: Duration,
    },
    /// User dismiss, Escape, or lifetime expiry.
    Dismiss {
        generation: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Begin remount / move-offscreen / show; app replies with [`Event::Shown`].
    PlaceShow,
    /// Move toast along the slide path (`progress` 0..=1).
    Move { progress: f32, dismissing: bool },
    /// Per-frame slide dt was capped (debug).
    SlideDtCapped {
        raw_ms: u128,
        progress: f32,
        dismissing: bool,
    },
    /// Toast presentation finished; clear message / hide or hand off.
    Finished,
    /// Open the start screen now (toast may still be Resting).
    OpenStart,
    /// Raise Settings / Start / popup above the toast (only after Resting).
    RaiseInteractive { focus: bool },
}

impl State {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active(_))
    }

    pub fn generation(&self) -> Option<u64> {
        match self {
            Self::Active(active) => Some(active.generation),
            Self::Idle => None,
        }
    }

    #[cfg(test)]
    pub fn after(&self) -> AfterToast {
        match self {
            Self::Active(active) => active.after,
            Self::Idle => AfterToast::Nothing,
        }
    }

    pub fn phase(&self) -> Option<&Phase> {
        match self {
            Self::Active(active) => Some(&active.phase),
            Self::Idle => None,
        }
    }

    /// True while 0→1 Start is latched and the toast has not reached rest yet.
    ///
    /// During Placing, suppress lifts after [`PLACING_GESTURE_SUPPRESS_FRAMES`] so a
    /// lost `Shown` cannot block the reopen gesture forever. Phase stays Placing.
    pub fn suppresses_reopen_gesture(&self) -> bool {
        match self {
            Self::Active(Active {
                after: AfterToast::OpenStart,
                phase: Phase::Placing,
                placing_frames,
                ..
            }) => *placing_frames < PLACING_GESTURE_SUPPRESS_FRAMES,
            Self::Active(Active {
                after: AfterToast::OpenStart,
                phase: Phase::SlidingIn { .. },
                ..
            }) => true,
            _ => false,
        }
    }

    pub fn is_resting(&self) -> bool {
        matches!(
            self,
            Self::Active(Active {
                phase: Phase::Resting,
                ..
            })
        )
    }

    #[cfg(test)]
    pub fn is_dismissing(&self) -> bool {
        matches!(
            self,
            Self::Active(Active {
                phase: Phase::SlidingOut { .. },
                ..
            })
        )
    }

    pub fn wants_frames(&self) -> bool {
        self.is_active()
    }

    /// Clear a latched OpenStart without finishing the toast (e.g. pads went empty).
    pub fn clear_after(&mut self) {
        if let Self::Active(active) = self {
            active.after = AfterToast::Nothing;
        }
    }
}

/// Pure stepper: window completions and frames are events; effects are commands.
pub fn step(state: State, event: Event) -> (State, Vec<Effect>) {
    match (state, event) {
        (
            _,
            Event::Show {
                generation,
                after,
                body,
            },
        ) => (
            State::Active(Active {
                generation,
                phase: Phase::Placing,
                after,
                body,
                placing_frames: 0,
            }),
            vec![Effect::PlaceShow],
        ),

        (
            State::Active(Active {
                generation,
                phase: Phase::Placing,
                after,
                body,
                placing_frames,
            }),
            Event::Shown {
                generation: shown_gen,
            },
        ) if shown_gen == generation => (
            State::Active(Active {
                generation,
                phase: Phase::SlidingIn {
                    elapsed: Duration::ZERO,
                },
                after,
                body,
                placing_frames,
            }),
            Vec::new(),
        ),

        // Placing is event-only: frames never advance or abort placement.
        // Count frames so gesture suppress can lift without emitting OpenStart.
        (
            State::Active(Active {
                generation,
                phase: Phase::Placing,
                after,
                body,
                placing_frames,
            }),
            Event::Frame {
                generation: frame_gen,
                ..
            },
        ) if frame_gen == generation => (
            State::Active(Active {
                generation,
                phase: Phase::Placing,
                after,
                body,
                placing_frames: placing_frames.saturating_add(1),
            }),
            Vec::new(),
        ),

        (
            State::Active(Active {
                generation,
                phase: Phase::SlidingIn { elapsed },
                after,
                body,
                placing_frames,
            }),
            Event::Frame {
                generation: frame_gen,
                raw_dt,
            },
        ) if frame_gen == generation => step_slide(
            generation,
            elapsed,
            raw_dt,
            after,
            body,
            placing_frames,
            false,
        ),

        (
            State::Active(Active {
                generation,
                phase: Phase::SlidingOut { elapsed },
                after,
                body,
                placing_frames,
            }),
            Event::Frame {
                generation: frame_gen,
                raw_dt,
            },
        ) if frame_gen == generation => step_slide(
            generation,
            elapsed,
            raw_dt,
            after,
            body,
            placing_frames,
            true,
        ),

        (
            State::Active(Active {
                generation,
                phase: Phase::Resting,
                after,
                body,
                placing_frames,
            }),
            Event::Frame {
                generation: frame_gen,
                ..
            },
        ) if frame_gen == generation => (
            State::Active(Active {
                generation,
                phase: Phase::Resting,
                after,
                body,
                placing_frames,
            }),
            // Do not RaiseInteractive every tick — z-order churn under multi-window
            // present correlated with device/atlas stress. Raise on settle + sync only.
            Vec::new(),
        ),

        (
            State::Active(Active {
                generation,
                phase: Phase::Placing,
                after,
                body: _,
                placing_frames: _,
            }),
            Event::Dismiss {
                generation: dismiss_gen,
            },
        ) if dismiss_gen == generation => {
            let mut effects = Vec::new();
            if after == AfterToast::OpenStart {
                effects.push(Effect::OpenStart);
            }
            effects.push(Effect::Finished);
            (State::Idle, effects)
        }

        (
            State::Active(Active {
                generation,
                phase: Phase::SlidingIn { .. } | Phase::Resting,
                after,
                body,
                placing_frames,
            }),
            Event::Dismiss {
                generation: dismiss_gen,
            },
        ) if dismiss_gen == generation => (
            State::Active(Active {
                generation,
                phase: Phase::SlidingOut {
                    elapsed: Duration::ZERO,
                },
                after,
                body,
                placing_frames,
            }),
            // First dismiss frame runs on next ToastFrame; kick one Move at 0.
            vec![Effect::Move {
                progress: 0.0,
                dismissing: true,
            }],
        ),

        // Already sliding out, wrong generation, or Idle: ignore.
        (state, _) => (state, Vec::new()),
    }
}

fn step_slide(
    generation: u64,
    elapsed: Duration,
    raw_dt: Duration,
    after: AfterToast,
    body: String,
    placing_frames: u32,
    dismissing: bool,
) -> (State, Vec<Effect>) {
    let (new_elapsed, progress, capped) =
        advance_toast_slide(elapsed, raw_dt, TOAST_SLIDE_DURATION);
    let mut effects = Vec::new();
    if capped {
        effects.push(Effect::SlideDtCapped {
            raw_ms: raw_dt.as_millis(),
            progress,
            dismissing,
        });
    }
    effects.push(Effect::Move {
        progress,
        dismissing,
    });

    if progress < 1.0 {
        let phase = if dismissing {
            Phase::SlidingOut {
                elapsed: new_elapsed,
            }
        } else {
            Phase::SlidingIn {
                elapsed: new_elapsed,
            }
        };
        return (
            State::Active(Active {
                generation,
                phase,
                after,
                body,
                placing_frames,
            }),
            effects,
        );
    }

    if dismissing {
        if after == AfterToast::OpenStart {
            effects.push(Effect::OpenStart);
        }
        effects.push(Effect::Finished);
        (State::Idle, effects)
    } else {
        // Slide-in settled: rest pose. OpenStart fires here (toast stays visible).
        let mut after = after;
        if after == AfterToast::OpenStart {
            effects.push(Effect::OpenStart);
            after = AfterToast::Nothing;
        }
        effects.push(Effect::RaiseInteractive { focus: true });
        (
            State::Active(Active {
                generation,
                phase: Phase::Resting,
                after,
                body,
                placing_frames,
            }),
            effects,
        )
    }
}

/// Debug label for a phase (transition logs).
pub fn phase_name(phase: &Phase) -> &'static str {
    match phase {
        Phase::Placing => "Placing",
        Phase::SlidingIn { .. } => "SlidingIn",
        Phase::Resting => "Resting",
        Phase::SlidingOut { .. } => "SlidingOut",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show_connect(generation: u64) -> Event {
        Event::Show {
            generation,
            after: AfterToast::OpenStart,
            body: "Connected".into(),
        }
    }

    fn settle_slide_in(mut state: State) -> (State, bool) {
        let mut opened = false;
        for _ in 0..20 {
            let (next, effects) = step(
                state,
                Event::Frame {
                    generation: 1,
                    raw_dt: Duration::from_millis(32),
                },
            );
            state = next;
            if effects.iter().any(|e| matches!(e, Effect::OpenStart)) {
                opened = true;
                break;
            }
        }
        (state, opened)
    }

    #[test]
    fn show_enters_placing_with_place_show() {
        let (state, effects) = step(State::Idle, show_connect(1));
        assert!(matches!(
            state,
            State::Active(Active {
                generation: 1,
                phase: Phase::Placing,
                after: AfterToast::OpenStart,
                placing_frames: 0,
                ..
            })
        ));
        assert_eq!(effects, vec![Effect::PlaceShow]);
    }

    #[test]
    fn huge_frame_during_placing_is_ignored() {
        let (state, _) = step(State::Idle, show_connect(1));
        let (state, effects) = step(
            state,
            Event::Frame {
                generation: 1,
                raw_dt: Duration::from_secs(10),
            },
        );
        assert!(effects.is_empty());
        assert!(matches!(state.phase(), Some(Phase::Placing)));
        assert!(state.suppresses_reopen_gesture());
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::OpenStart | Effect::Finished))
        );
        // One frame counted; still under the suppress budget.
        assert!(matches!(
            state,
            State::Active(Active {
                placing_frames: 1,
                ..
            })
        ));
    }

    #[test]
    fn placing_frame_budget_lifts_gesture_suppress_without_open_start() {
        let (mut state, _) = step(State::Idle, show_connect(1));
        for _ in 0..PLACING_GESTURE_SUPPRESS_FRAMES {
            assert!(state.suppresses_reopen_gesture());
            let (next, effects) = step(
                state,
                Event::Frame {
                    generation: 1,
                    raw_dt: Duration::from_secs(10),
                },
            );
            assert!(effects.is_empty());
            assert!(!effects.iter().any(|e| matches!(e, Effect::OpenStart)));
            state = next;
            assert!(matches!(state.phase(), Some(Phase::Placing)));
        }
        assert!(!state.suppresses_reopen_gesture());
        assert!(matches!(state.phase(), Some(Phase::Placing)));
        assert_eq!(state.after(), AfterToast::OpenStart);
    }

    #[test]
    fn shown_after_placing_budget_still_settles_and_opens_start() {
        let (mut state, _) = step(State::Idle, show_connect(1));
        for _ in 0..PLACING_GESTURE_SUPPRESS_FRAMES {
            let (next, _) = step(
                state,
                Event::Frame {
                    generation: 1,
                    raw_dt: Duration::from_millis(16),
                },
            );
            state = next;
        }
        assert!(!state.suppresses_reopen_gesture());

        let (state, _) = step(state, Event::Shown { generation: 1 });
        assert!(matches!(state.phase(), Some(Phase::SlidingIn { .. })));
        assert!(state.suppresses_reopen_gesture());

        let (state, opened) = settle_slide_in(state);
        assert!(opened, "slide-in never settled / opened Start");
        assert!(state.is_resting());
    }

    #[test]
    fn show_huge_frame_then_shown_settles_and_opens_start() {
        let (state, _) = step(State::Idle, show_connect(1));
        let (state, _) = step(
            state,
            Event::Frame {
                generation: 1,
                raw_dt: Duration::from_secs(10),
            },
        );
        assert!(matches!(state.phase(), Some(Phase::Placing)));

        let (state, _) = step(state, Event::Shown { generation: 1 });
        assert!(matches!(state.phase(), Some(Phase::SlidingIn { .. })));

        let (state, opened) = settle_slide_in(state);
        assert!(opened, "slide-in never settled / opened Start");
        assert!(state.is_resting());
    }

    #[test]
    fn slide_in_settle_opens_start_and_rests() {
        let (state, _) = step(State::Idle, show_connect(1));
        let (state, _) = step(state, Event::Shown { generation: 1 });
        assert!(state.suppresses_reopen_gesture());

        let (state, opened) = settle_slide_in(state);
        assert!(opened, "slide-in never settled / opened Start");
        assert!(state.is_resting());
        assert!(!state.suppresses_reopen_gesture());
    }

    #[test]
    fn dismiss_while_placing_finishes_with_open_start() {
        let (state, _) = step(State::Idle, show_connect(1));
        let (state, effects) = step(state, Event::Dismiss { generation: 1 });
        assert_eq!(state, State::Idle);
        assert!(effects.contains(&Effect::Finished));
        assert!(effects.contains(&Effect::OpenStart));
        let open_idx = effects.iter().position(|e| matches!(e, Effect::OpenStart));
        let fin_idx = effects.iter().position(|e| matches!(e, Effect::Finished));
        assert!(open_idx < fin_idx);
    }

    #[test]
    fn dismiss_while_resting_slides_out_without_second_open_start() {
        let (state, _) = step(State::Idle, show_connect(1));
        let (state, _) = step(state, Event::Shown { generation: 1 });
        let (state, opened) = settle_slide_in(state);
        assert!(opened);
        assert!(state.is_resting());
        assert_eq!(state.after(), AfterToast::Nothing);

        let (state, effects) = step(state, Event::Dismiss { generation: 1 });
        assert!(state.is_dismissing());
        assert!(!effects.iter().any(|e| matches!(e, Effect::OpenStart)));

        let mut state = state;
        for _ in 0..20 {
            let (next, effects) = step(
                state,
                Event::Frame {
                    generation: 1,
                    raw_dt: Duration::from_millis(32),
                },
            );
            state = next;
            if effects.iter().any(|e| matches!(e, Effect::Finished)) {
                assert!(effects.contains(&Effect::Finished));
                assert!(!effects.iter().any(|e| matches!(e, Effect::OpenStart)));
                assert_eq!(state, State::Idle);
                return;
            }
        }
        panic!("slide-out never finished");
    }

    #[test]
    fn toast_without_open_start_never_emits_open_start() {
        let (state, _) = step(
            State::Idle,
            Event::Show {
                generation: 2,
                after: AfterToast::Nothing,
                body: "Disconnected".into(),
            },
        );
        let (state, _) = step(state, Event::Shown { generation: 2 });
        let mut state = state;
        for _ in 0..20 {
            let (next, effects) = step(
                state,
                Event::Frame {
                    generation: 2,
                    raw_dt: Duration::from_millis(32),
                },
            );
            state = next;
            assert!(!effects.iter().any(|e| matches!(e, Effect::OpenStart)));
            if state.is_resting() {
                return;
            }
        }
        panic!("never rested");
    }

    #[test]
    fn wrong_generation_is_ignored() {
        let (state, _) = step(State::Idle, show_connect(1));
        let (next, effects) = step(state.clone(), Event::Shown { generation: 99 });
        assert_eq!(next, state);
        assert!(effects.is_empty());
    }
}
