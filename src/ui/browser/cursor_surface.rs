//! Pull-model virtual cursor surface.
//!
//! The cursor owns its own clock: it reads the latest pad snapshot, advances
//! the shared [`CursorHandle`] wall-clock state, streams quantized mouse/scroll
//! commands to CEF, and asks the canvas for the next vsync frame while moving.
//! No app-level `BrowserFrame` message is involved, so cursor motion no longer
//! rebuilds every window.

use crate::browser::cursor::CursorHandle;
use crate::browser::engine::{EngineCommand, EngineHandle, TabId};
use crate::domain::pad::InputHub;
use crate::ui::browser::BrowserMessage;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::{Element, Point, Rectangle, Renderer, Size, Theme, mouse};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Maximum age of the HID snapshot before the cursor treats it as neutral.
const SNAPSHOT_MAX_AGE: Duration = Duration::from_millis(200);

pub struct CursorSurface {
    handle: CursorHandle,
    hub: Arc<InputHub>,
    engine: Option<EngineHandle>,
    tab: TabId,
    active: bool,
    draws: bool,
}

/// Owned inputs for the cursor canvas. The app builds this per Browser view;
/// the canvas itself carries the clock.
pub struct CursorSurfaceSpec {
    pub handle: CursorHandle,
    pub hub: Arc<InputHub>,
    pub engine: Option<EngineHandle>,
    pub tab: TabId,
    pub active: bool,
    pub draws: bool,
}

pub fn cursor_surface(
    spec: CursorSurfaceSpec,
    page_w: f32,
    page_h: f32,
) -> Element<'static, BrowserMessage> {
    iced::widget::canvas(CursorSurface {
        handle: spec.handle,
        hub: spec.hub,
        engine: spec.engine,
        tab: spec.tab,
        active: spec.active,
        draws: spec.draws,
    })
    .width(page_w)
    .height(page_h)
    .into()
}

impl<Message> canvas::Program<Message> for CursorSurface {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        _event: &canvas::Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        if !self.active {
            // Surface hidden/covered: stop the analog clock so resuming does
            // not integrate the whole pause as one 100ms step.
            self.handle.reset_analog();
            return None;
        }
        // One advance per event; redraws drive the active loop below.
        let now = Instant::now();
        let sample = self
            .hub
            .latest_reading(SNAPSHOT_MAX_AGE)
            .map(|reading| reading.sample)
            .unwrap_or_default();
        let frame = {
            let mut runtime = self.handle.lock();
            runtime.advance(&sample, now)
        };

        if let Some(engine) = &self.engine {
            if let Some((x, y)) = frame.mouse {
                engine.send(EngineCommand::MouseMove {
                    tab: self.tab,
                    x: x as f32,
                    y: y as f32,
                    modifiers: keyboard_types::Modifiers::empty(),
                });
            }
            if let Some((dx, dy, (x, y))) = frame.scroll {
                engine.send(EngineCommand::ScrollBy {
                    tab: self.tab,
                    dx,
                    dy,
                    x,
                    y,
                });
            }
            if frame.reseed {
                engine.send(EngineCommand::ReseedDom { tab: self.tab });
            }
        }

        let snapshot = self.handle.snapshot();
        let moving = snapshot.vx.abs() > 0.5
            || snapshot.vy.abs() > 0.5
            || snapshot.fly_to.is_some()
            || (snapshot.dock_blend
                - if snapshot.attached.is_some() {
                    1.0
                } else {
                    0.0
                })
            .abs()
                > 0.001;
        let stick = sample.lstick_x != 0.0
            || sample.lstick_y != 0.0
            || sample.rstick_x != 0.0
            || sample.rstick_y != 0.0;
        let pending = {
            let runtime = self.handle.lock();
            runtime.force_mouse_sync
                || runtime.dom_reseed_pending
                || runtime.last_sent_mouse != Some((snapshot.x as i32, snapshot.y.max(0.0) as i32))
        };
        if moving || stick || pending {
            // Immediate request: iced/wgpu presents at vsync, so the cursor
            // targets the monitor rate instead of a hardcoded 16ms phase.
            Some(canvas::Action::request_redraw())
        } else {
            None
        }
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        if !self.draws {
            return Vec::new();
        }
        let cursor = self.handle.snapshot();
        let t = cursor.dock_blend.clamp(0.0, 1.0);
        let t = t * t * (3.0 - 2.0 * t);
        let (dx, dy) = (cursor.x - 10.0, cursor.y - 10.0);
        let wrap = cursor
            .attached_rect(&self.handle.lock().focusables)
            .or(cursor.last_dock);
        let (wx, wy, ww, wh) = match wrap {
            Some((x, y, w, h)) => (x - 2.0, y - 2.0, (w + 4.0).max(8.0), (h + 4.0).max(8.0)),
            None => (dx, dy, 20.0, 20.0),
        };
        let (ix, iy) = (dx + (wx - dx) * t, dy + (wy - dy) * t);
        let (iw, ih) = (20.0 + (ww - 20.0) * t, 20.0 + (wh - 20.0) * t);
        let radius = 10.0 + (crate::ui::theme::radius::SM - 10.0) * t;
        let mut frame = Frame::new(renderer, bounds.size());
        let path = Path::rounded_rectangle(Point::new(ix, iy), Size::new(iw, ih), radius.into());
        frame.stroke(
            &path,
            Stroke::default()
                .with_color(crate::ui::theme::ACCENT)
                .with_width(2.0),
        );
        vec![frame.into_geometry()]
    }
}
