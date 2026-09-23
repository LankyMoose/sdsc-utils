//! Lightbar spectrum editor canvas programs.

use super::ConfigureMessage;
use crate::color::{BatterySpectrum, GradientStop, hsv_to_rgb};
use crate::theme;
use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry, Path};
use iced::{Color, Event, Point, Rectangle, Renderer, Size, Theme};

pub(super) const BAR_HEIGHT: f32 = 44.0;
pub(super) const SV_HEIGHT: f32 = 112.0;
pub(super) const HUE_HEIGHT: f32 = 20.0;
const HANDLE_WIDTH: f32 = 10.0;
const HIT_RADIUS: f32 = 12.0;
const STOP_REMOVE_DISTANCE: f32 = 28.0;

/// Gradient preview with draggable stop handles.
/// Left-click empty bar to add; drag a stop vertically off the bar, or right-click a
/// stop, to remove (while more than [`BatterySpectrum::MIN_STOPS`] remain).
pub(super) struct SpectrumBar {
    pub stops: Vec<GradientStop>,
    pub selected: usize,
}

#[derive(Debug, Default)]
pub(super) struct BarState {
    dragging: bool,
    remove_armed: bool,
}

impl SpectrumBar {
    fn spectrum(&self) -> BatterySpectrum {
        BatterySpectrum {
            stops: self.stops.clone(),
        }
    }

    fn percent_at(x: f32, width: f32) -> u8 {
        if width <= 0.0 {
            return 0;
        }
        ((x / width).clamp(0.0, 1.0) * 100.0).round() as u8
    }

    fn handle_x(percent: u8, width: f32) -> f32 {
        width * (percent.min(100) as f32 / 100.0)
    }

    fn hit(&self, x: f32, width: f32) -> Option<usize> {
        self.stops
            .iter()
            .enumerate()
            .map(|(index, stop)| (index, (Self::handle_x(stop.percent, width) - x).abs()))
            .filter(|(_, distance)| *distance <= HIT_RADIUS)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(index, _)| index)
    }

    fn outside_vertical_distance(y: f32, bounds: Rectangle) -> f32 {
        if y < bounds.y {
            bounds.y - y
        } else if y > bounds.y + bounds.height {
            y - (bounds.y + bounds.height)
        } else {
            0.0
        }
    }
}

impl canvas::Program<ConfigureMessage> for SpectrumBar {
    type State = BarState;

    fn update(
        &self,
        state: &mut Self::State,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<ConfigureMessage>> {
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let position = cursor.position_in(bounds)?;
                if let Some(index) = self.hit(position.x, bounds.width) {
                    state.dragging = true;
                    state.remove_armed = false;
                    Some(canvas::Action::publish(ConfigureMessage::SelectStop(index)).and_capture())
                } else {
                    let percent = Self::percent_at(position.x, bounds.width);
                    Some(
                        canvas::Action::publish(ConfigureMessage::AddStopAt(percent)).and_capture(),
                    )
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                let position = cursor.position_in(bounds)?;
                let index = self.hit(position.x, bounds.width)?;
                if self.stops.len() <= BatterySpectrum::MIN_STOPS {
                    return Some(
                        canvas::Action::publish(ConfigureMessage::SelectStop(index)).and_capture(),
                    );
                }
                Some(canvas::Action::publish(ConfigureMessage::RemoveStopAt(index)).and_capture())
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.dragging => {
                let point = cursor.position()?;
                let can_remove = self.stops.len() > BatterySpectrum::MIN_STOPS;
                let remove_armed = can_remove
                    && Self::outside_vertical_distance(point.y, bounds) >= STOP_REMOVE_DISTANCE;
                state.remove_armed = remove_armed;
                if remove_armed {
                    return Some(canvas::Action::request_redraw().and_capture());
                }
                let percent = Self::percent_at(point.x - bounds.x, bounds.width);
                Some(canvas::Action::publish(ConfigureMessage::MoveStop(percent)).and_capture())
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.dragging => {
                let remove = state.remove_armed;
                state.dragging = false;
                state.remove_armed = false;
                if remove {
                    Some(canvas::Action::publish(ConfigureMessage::RemoveStop).and_capture())
                } else {
                    Some(canvas::Action::request_redraw().and_capture())
                }
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let width = bounds.width;
        let height = bounds.height;
        let spectrum = self.spectrum();

        // Draw the gradient as thin columns so it matches `color_at_percent` exactly.
        let columns = width.max(1.0).ceil() as usize;
        let step = width / columns as f32;
        for column in 0..columns {
            let x = column as f32 * step;
            let percent = Self::percent_at(x + step / 2.0, width);
            frame.fill_rectangle(
                Point::new(x, 0.0),
                Size::new(step + 1.0, height),
                theme::from_rgb(spectrum.color_at_percent(percent)),
            );
        }

        for (index, stop) in self.stops.iter().enumerate() {
            let x = Self::handle_x(stop.percent, width);
            let left = (x - HANDLE_WIDTH / 2.0).clamp(0.0, (width - HANDLE_WIDTH).max(0.0));
            let selected = index == self.selected;
            let removing = state.remove_armed && state.dragging && selected;

            let outline = Path::rectangle(
                Point::new(left - 1.0, -1.0),
                Size::new(HANDLE_WIDTH + 2.0, height + 2.0),
            );
            frame.fill(
                &outline,
                if removing {
                    theme::DANGER
                } else if selected {
                    theme::INK
                } else {
                    theme::DIM
                },
            );

            let handle =
                Path::rectangle(Point::new(left, 2.0), Size::new(HANDLE_WIDTH, height - 4.0));
            frame.fill(&handle, theme::from_rgb(stop.color));
        }

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if state.dragging {
            return if state.remove_armed {
                mouse::Interaction::NotAllowed
            } else {
                mouse::Interaction::Grabbing
            };
        }
        match cursor.position_in(bounds) {
            Some(position) if self.hit(position.x, bounds.width).is_some() => {
                mouse::Interaction::Grab
            }
            Some(_) if self.stops.len() < BatterySpectrum::MAX_STOPS => {
                mouse::Interaction::Crosshair
            }
            _ => mouse::Interaction::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Hue spectrum bar
// ---------------------------------------------------------------------------

pub(super) struct HueBar {
    pub hue: f32,
}

#[derive(Debug, Default)]
pub(super) struct HueState {
    dragging: bool,
}

impl HueBar {
    fn sample(bounds: Rectangle, cursor: mouse::Cursor) -> Option<f32> {
        let point = cursor.position()?;
        let t = ((point.x - bounds.x) / bounds.width.max(1.0)).clamp(0.0, 1.0);
        Some(t * 360.0)
    }
}

impl canvas::Program<ConfigureMessage> for HueBar {
    type State = HueState;

    fn update(
        &self,
        state: &mut Self::State,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<ConfigureMessage>> {
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if cursor.is_over(bounds) =>
            {
                state.dragging = true;
                let hue = Self::sample(bounds, cursor)?;
                Some(canvas::Action::publish(ConfigureMessage::HueChanged(hue)).and_capture())
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.dragging => {
                let hue = Self::sample(bounds, cursor)?;
                Some(canvas::Action::publish(ConfigureMessage::HueChanged(hue)).and_capture())
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.dragging => {
                state.dragging = false;
                Some(canvas::Action::request_redraw().and_capture())
            }
            _ => None,
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
        let mut frame = Frame::new(renderer, bounds.size());
        let size = bounds.size();

        let mut spectrum =
            canvas::gradient::Linear::new(Point::new(0.0, 0.0), Point::new(size.width, 0.0));
        for (index, hue) in [0.0, 60.0, 120.0, 180.0, 240.0, 300.0, 360.0]
            .into_iter()
            .enumerate()
        {
            spectrum = spectrum.add_stop(
                index as f32 / 6.0,
                theme::from_rgb(hsv_to_rgb(hue, 1.0, 1.0)),
            );
        }
        frame.fill_rectangle(Point::ORIGIN, size, spectrum);

        let x = (self.hue.rem_euclid(360.0) / 360.0).clamp(0.0, 1.0) * size.width;
        let cursor_point = Point::new(x, size.height / 2.0);
        frame.stroke(
            &Path::circle(cursor_point, 6.0),
            canvas::Stroke::default()
                .with_color(Color::WHITE)
                .with_width(2.0),
        );
        frame.stroke(
            &Path::circle(cursor_point, 7.5),
            canvas::Stroke::default()
                .with_color(theme::alpha(Color::BLACK, 0.6))
                .with_width(1.0),
        );

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if state.dragging {
            mouse::Interaction::Grabbing
        } else if cursor.is_over(bounds) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Saturation / value square
// ---------------------------------------------------------------------------

pub(super) struct SvSquare {
    pub hue: f32,
    pub saturation: f32,
    pub value: f32,
}

#[derive(Debug, Default)]
pub(super) struct SvState {
    dragging: bool,
}

impl SvSquare {
    fn sample(bounds: Rectangle, cursor: mouse::Cursor) -> Option<(f32, f32)> {
        let point = cursor.position()?;
        let x = ((point.x - bounds.x) / bounds.width.max(1.0)).clamp(0.0, 1.0);
        let y = ((point.y - bounds.y) / bounds.height.max(1.0)).clamp(0.0, 1.0);
        Some((x, 1.0 - y))
    }
}

impl canvas::Program<ConfigureMessage> for SvSquare {
    type State = SvState;

    fn update(
        &self,
        state: &mut Self::State,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<ConfigureMessage>> {
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if cursor.is_over(bounds) =>
            {
                state.dragging = true;
                let (saturation, value) = Self::sample(bounds, cursor)?;
                Some(
                    canvas::Action::publish(ConfigureMessage::SaturationValueChanged(
                        saturation, value,
                    ))
                    .and_capture(),
                )
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.dragging => {
                let (saturation, value) = Self::sample(bounds, cursor)?;
                Some(
                    canvas::Action::publish(ConfigureMessage::SaturationValueChanged(
                        saturation, value,
                    ))
                    .and_capture(),
                )
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.dragging => {
                state.dragging = false;
                Some(canvas::Action::request_redraw().and_capture())
            }
            _ => None,
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
        let mut frame = Frame::new(renderer, bounds.size());
        let size = bounds.size();
        let pure = theme::from_rgb(hsv_to_rgb(self.hue, 1.0, 1.0));

        let saturation =
            canvas::gradient::Linear::new(Point::new(0.0, 0.0), Point::new(size.width, 0.0))
                .add_stop(0.0, Color::WHITE)
                .add_stop(1.0, pure);
        frame.fill_rectangle(Point::ORIGIN, size, saturation);

        let value =
            canvas::gradient::Linear::new(Point::new(0.0, 0.0), Point::new(0.0, size.height))
                .add_stop(0.0, Color::TRANSPARENT)
                .add_stop(1.0, Color::BLACK);
        frame.fill_rectangle(Point::ORIGIN, size, value);

        let cursor_point = Point::new(
            self.saturation.clamp(0.0, 1.0) * size.width,
            (1.0 - self.value.clamp(0.0, 1.0)) * size.height,
        );
        frame.stroke(
            &Path::circle(cursor_point, 6.0),
            canvas::Stroke::default()
                .with_color(Color::WHITE)
                .with_width(2.0),
        );
        frame.stroke(
            &Path::circle(cursor_point, 7.5),
            canvas::Stroke::default()
                .with_color(theme::alpha(Color::BLACK, 0.6))
                .with_width(1.0),
        );

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        _state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if cursor.is_over(bounds) {
            mouse::Interaction::Crosshair
        } else {
            mouse::Interaction::default()
        }
    }
}
