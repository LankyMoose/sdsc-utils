//! Pull-model wrapper for the system OSK.
//!
//! The keyboard layout is still built by [`super::view`]; this widget owns the
//! per-window analog poll. It reads the shared [`OskInputSlot`] at redraw time,
//! runs the shared [`DualNav`]/[`NavBank`], and publishes only discrete
//! step/commit messages. No app message is produced per pad sample.

use super::{Dir, OskMessage, OskState, view};
use crate::domain::pad::{
    DualNav, NavAction, NavBank, NavHalf, NavPhase, NavReading, OskInputSlot, dpad_only,
};
use crate::persist::prefs::OskMode;
use iced::advanced::Shell;
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{self, Tree, Widget, tree};
use iced::{Element, Event, Rectangle, Size, mouse};
use std::sync::Arc;
use std::time::Instant;

/// Wrap the OSK view in the pull-model analog widget.
pub fn surface<'a>(
    state: &'a OskState,
    slot: Arc<OskInputSlot>,
    epoch: u64,
) -> Element<'a, OskMessage> {
    OskSurface {
        content: view(state),
        slot,
        open: state.open,
        mode: state.mode,
        epoch,
    }
    .into()
}

pub struct OskSurface<'a> {
    content: Element<'a, OskMessage>,
    slot: Arc<OskInputSlot>,
    open: bool,
    mode: OskMode,
    epoch: u64,
}

#[derive(Default)]
pub struct SurfaceState {
    last_seq: u64,
    was_open: bool,
    mode: Option<OskMode>,
    dual: DualNav,
    grid: NavBank,
    /// Last poll saw a non-neutral pad; keep polling even if the slot seq
    /// stalls so repeat cadence still fires from the widget clock.
    last_active: bool,
    epoch: Option<u64>,
}

impl SurfaceState {
    fn reset(&mut self, mode: OskMode) {
        self.dual.prepare_on_open(false);
        self.grid.prepare_on_open(false);
        self.last_seq = 0;
        self.last_active = false;
        self.mode = Some(mode);
        self.epoch = None;
    }
}

impl Widget<OskMessage, iced::Theme, iced::Renderer> for OskSurface<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<SurfaceState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(SurfaceState::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<iced::Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn iced::advanced::Clipboard,
        shell: &mut Shell<'_, OskMessage>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<SurfaceState>();
        if self.open {
            if !state.was_open || state.mode != Some(self.mode) || state.epoch != Some(self.epoch) {
                state.reset(self.mode);
                state.epoch = Some(self.epoch);
            }
            state.was_open = true;
            let (seq, readings) = self.slot.read();
            let changed = seq != state.last_seq;
            if changed {
                state.last_seq = seq;
            }
            if changed || state.last_active {
                state.last_active = readings
                    .iter()
                    .any(|reading| !crate::domain::pad::sample_nav_resting(&reading.sample));
                self.poll(state, &readings, shell);
            }
        } else if state.was_open {
            state.was_open = false;
            state.mode = None;
            state.last_seq = 0;
            state.last_active = false;
            state.epoch = None;
            state.dual.reset();
            state.grid.reset();
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
}

impl OskSurface<'_> {
    fn poll(
        &self,
        state: &mut SurfaceState,
        readings: &[NavReading],
        shell: &mut Shell<'_, OskMessage>,
    ) {
        if readings.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut emitted = false;
        match self.mode {
            OskMode::Dual => {
                let tick = state.dual.tick(readings, now);
                for step in tick.steps {
                    if step.step.phase == NavPhase::Release {
                        continue;
                    }
                    emitted = true;
                    shell.publish(OskMessage::Step {
                        pad: step.pad,
                        half: step.half,
                        direction: step.step.direction,
                        phase: step.step.phase,
                    });
                }
                for edge in tick.triggers {
                    emitted = true;
                    shell.publish(OskMessage::Commit {
                        pad: edge.pad,
                        half: edge.half,
                    });
                }
            }
            OskMode::Grid => {
                let masked: Vec<NavReading> = readings
                    .iter()
                    .cloned()
                    .map(|mut reading| {
                        reading.sample = dpad_only(&reading.sample);
                        reading
                    })
                    .collect();
                let tick = state
                    .grid
                    .tick(&masked, now, true, true, false, false, false, false, false);
                if let Some(action) = tick.action
                    && let Some(pad) = tick.action_pad
                    && matches!(
                        action,
                        NavAction::Up | NavAction::Down | NavAction::Left | NavAction::Right
                    )
                {
                    emitted = true;
                    shell.publish(OskMessage::GridStep {
                        pad,
                        direction: action,
                    });
                }
            }
        }
        // Own the poll cadence while a stick/trigger is non-neutral; stop at
        // rest so a static keyboard never repaints. The pad edge stream is
        // change-gated, so this keeps repeat cadence alive without per-sample
        // app messages.
        if emitted
            || readings
                .iter()
                .any(|reading| !crate::domain::pad::sample_nav_resting(&reading.sample))
        {
            crate::ui::pull::FrameDemand::next(Instant::now()).request(shell);
        }
        if self.slot.has_replay() {
            // Drain synthetic replay frames even when the next queued frame is
            // neutral (press/release sequences are queued back-to-back).
            crate::ui::pull::FrameDemand::Now.request(shell);
        }
    }
}

impl<'a> From<OskSurface<'a>> for Element<'a, OskMessage> {
    fn from(value: OskSurface<'a>) -> Self {
        Element::new(value)
    }
}

/// DQ half -> existing layout direction helper used by tests.
pub fn dir_for(direction: NavAction) -> Option<Dir> {
    match direction {
        NavAction::Up => Some(Dir::Up),
        NavAction::Down => Some(Dir::Down),
        NavAction::Left => Some(Dir::Left),
        NavAction::Right => Some(Dir::Right),
        _ => None,
    }
}

/// Test/helper: current half for a shared nav step.
pub fn half_label(half: NavHalf) -> &'static str {
    match half {
        NavHalf::Left => "left",
        NavHalf::Right => "right",
    }
}
