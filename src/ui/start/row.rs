//! Shared strip widgets: update badge and subtitle block.

use crate::ui::start::view::{StartMessage, StartSubtitle};
use crate::ui::theme;
use iced::font::Weight;
use iced::widget::text::Wrapping;
use iced::widget::{column, container, row, text};
use iced::{Alignment, Background, Border, Color, Element, Fill, Font, Padding, Theme};

/// Soft pill for pending Steam updates.
pub(crate) fn update_required_badge(
    selected: bool,
    muted: bool,
    fade: f32,
) -> Element<'static, StartMessage> {
    let fade = fade.clamp(0.0, 1.0);
    let a = if muted { 0.55 * fade } else { fade };
    let label_size = if selected { 13.0 } else { 11.0 };
    let (fill, stroke) = (
        theme::alpha(theme::MUTED, 0.22 * a),
        theme::alpha(theme::MUTED, 0.40 * a),
    );
    container(
        text("Update required")
            .size(label_size)
            .color(theme::alpha(theme::INK, 0.92 * a))
            .font(Font {
                weight: Weight::Semibold,
                ..Font::DEFAULT
            }),
    )
    .padding(Padding {
        top: 3.0,
        right: 9.0,
        bottom: 3.0,
        left: 9.0,
    })
    .style(move |_theme: &Theme| container::Style {
        background: Some(Background::Color(fill)),
        border: Border {
            color: stroke,
            width: 1.0,
            radius: 999.0.into(),
        },
        ..container::Style::default()
    })
    .into()
}

/// Meta under a game title: plain path, or stacked `Label: value` lines.
///
/// `align` places the block inside its column: centered for horizontal
/// in-slot text, start-aligned for the vertical side column.
pub(crate) fn game_subtitle_block<'a>(
    subtitle: &'a StartSubtitle,
    value_color: Color,
    label_color: Color,
    value_size: f32,
    align: Alignment,
) -> Element<'a, StartMessage> {
    match subtitle {
        StartSubtitle::Plain(plain) => text(plain.as_str())
            .size(value_size)
            .color(value_color)
            .wrapping(Wrapping::Word)
            .width(Fill)
            .align_x(align)
            .into(),
        StartSubtitle::Labeled(lines) => {
            let mut col = column![].spacing(2).width(Fill).align_x(align);
            for line in lines {
                col = col.push(
                    row![
                        text(format!("{}:", line.label))
                            .size(value_size)
                            .color(label_color),
                        text(line.value.as_str())
                            .size(value_size)
                            .color(value_color)
                            .wrapping(Wrapping::Word),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                );
            }
            col.into()
        }
    }
}
