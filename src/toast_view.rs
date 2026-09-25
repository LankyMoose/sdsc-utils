//! Overlay toast card rendered by the iced daemon.

use crate::percent_ring::{self, TOAST_SIZE};
use crate::svg_icon;
use crate::theme;
use crate::toast::{ToastMessage, ToastTrailing};
use iced::widget::{column, container, mouse_area, row, space, svg, text};
use iced::{Alignment, Element, Fill, Length, Shrink};

/// Logical width of the toast window.
pub const WIDTH: f32 = 360.0;
/// Logical height of the toast window.
pub const HEIGHT: f32 = 100.0;
/// Gap kept between the toast and the screen edge.
pub const MARGIN: f32 = 16.0;

const RAIL_WIDTH: f32 = 3.0;
const PADDING: f32 = 12.0;
const HEADING_SIZE: f32 = 14.0;
const BODY_SIZE: f32 = 13.0;
/// Compact leading icon for crash-restart toasts (not the percent-ring slot).
const BUG_ICON_SIZE: f32 = 36.0;

/// Renders the toast card. Clicking anywhere on it emits `on_dismiss`.
///
/// `generation` changes the layout width by 1px so iced/wgpu cannot keep the
/// previous toast's surface when messages are chained on one window.
pub fn view<'a, Message>(
    message: &'a ToastMessage,
    generation: u64,
    on_dismiss: Message,
) -> Element<'a, Message>
where
    Message: Clone + 'a,
{
    let accent = theme::from_rgb(message.accent);
    let width = WIDTH
        + if generation.is_multiple_of(2) {
            0.0
        } else {
            1.0
        };

    let rail = container(space())
        .width(Length::Fixed(RAIL_WIDTH))
        .height(Fill)
        .style(theme::rail(accent));

    let body = column![
        text(message.heading.as_str())
            .size(HEADING_SIZE)
            .color(theme::INK)
            .width(Fill),
        text(message.body.as_str())
            .size(BODY_SIZE)
            .color(theme::MUTED)
            .width(Fill),
    ]
    .spacing(4)
    .width(Fill);

    let content: Element<'_, Message> = match &message.trailing {
        ToastTrailing::Percent { percent, eta } => row![
            rail,
            body,
            percent_ring::percent_ring(*percent, accent, TOAST_SIZE, eta.clone())
        ]
        .spacing(PADDING)
        .align_y(Alignment::Center)
        .height(Fill)
        .into(),
        ToastTrailing::Bug => {
            let icon = svg(svg::Handle::from_memory(svg_icon::BUG_SVG.as_bytes()))
                .width(Length::Fixed(BUG_ICON_SIZE))
                .height(Length::Fixed(BUG_ICON_SIZE))
                .style(move |_theme, _status| svg::Style {
                    color: Some(accent),
                });
            row![rail, icon, body]
                .spacing(PADDING)
                .align_y(Alignment::Center)
                .height(Fill)
                .into()
        }
    };

    let card = container(content)
        .padding(PADDING)
        .width(Fill)
        .height(Fill)
        .style(theme::toast_card(accent));

    mouse_area(
        container(card)
            .width(Length::Fixed(width))
            .height(Length::Fixed(HEIGHT)),
    )
    .on_press(on_dismiss)
    .interaction(iced::mouse::Interaction::Pointer)
    .into()
}

/// Renders nothing when the toast window is open but idle between messages.
pub fn empty<'a, Message: 'a>() -> Element<'a, Message> {
    container(space()).width(Shrink).height(Shrink).into()
}
