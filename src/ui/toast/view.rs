//! Overlay toast card rendered by the iced daemon.

use crate::ui::percent_ring::{self, TOAST_SIZE};
use crate::ui::svg_icon;
use crate::ui::theme;
use crate::ui::toast::{ToastMessage, ToastTrailing};
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
/// `opacity` (0..=1) is the card's true opacity: the fill fades linearly to
/// it, while text/rail/ring saturate at full strength above the glass point.
pub fn view<'a, Message>(
    message: &'a ToastMessage,
    generation: u64,
    on_dismiss: Message,
    opacity: f32,
) -> Element<'a, Message>
where
    Message: Clone + 'a,
{
    let opacity = opacity.clamp(0.0, 1.0);
    let content = theme::toast_content_opacity(opacity);
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
        .style(theme::rail(accent, opacity));

    let body = column![
        text(message.heading.as_str())
            .size(HEADING_SIZE)
            .color(theme::scale_alpha(theme::INK, content))
            .width(Fill),
        text(message.body.as_str())
            .size(BODY_SIZE)
            .color(theme::scale_alpha(theme::MUTED, content))
            .width(Fill),
    ]
    .spacing(4)
    .width(Fill);

    let content: Element<'_, Message> = match &message.trailing {
        ToastTrailing::Percent { percent, eta } => row![
            rail,
            body,
            percent_ring::percent_ring(*percent, accent, TOAST_SIZE, eta.clone(), 1.0, opacity)
        ]
        .spacing(PADDING)
        .align_y(Alignment::Center)
        .height(Fill)
        .into(),
        trailing @ (ToastTrailing::Bug | ToastTrailing::Check) => {
            let source = if matches!(trailing, ToastTrailing::Check) {
                svg_icon::CHECK_SVG
            } else {
                svg_icon::BUG_SVG
            };
            let icon = svg(svg::Handle::from_memory(source.as_bytes()))
                .width(Length::Fixed(BUG_ICON_SIZE))
                .height(Length::Fixed(BUG_ICON_SIZE))
                .style(move |_theme, _status| svg::Style {
                    color: Some(theme::scale_alpha(accent, content)),
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
        .style(theme::toast_card(opacity));

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
