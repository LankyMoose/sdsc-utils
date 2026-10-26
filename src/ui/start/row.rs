//! Shared strip widgets: update badge, subtitle block, controllers rings strip.

use crate::controller::dualsense::lightbar;
use crate::ui::color::BatterySpectrum;
use crate::ui::percent_ring;
use crate::ui::start::view::{StartMessage, StartSubtitle, State};
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

/// Horizontal rings-only controllers strip for the island.
///
/// Percentage circles only — no titles, no ETA. The selected pad carries an
/// accent ring so the strip reads without text.
pub(crate) fn controllers_rings_strip<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    ring_size: f32,
) -> Element<'a, StartMessage> {
    let mut strip = row![].spacing(10).align_y(Alignment::Center);
    for (i, row) in state.controllers.iter().enumerate() {
        let selected = i == state.controller_selected;
        let powering_off = row.connected
            && state
                .powering_off
                .as_ref()
                .is_some_and(|s| s == &row.serial);
        let ring_color = if state.ring_flash_white(&row.serial) {
            theme::from_rgb(lightbar::IDENTIFY_FLASH)
        } else if row.connected && !powering_off {
            theme::from_rgb(spectrum.color_at_percent(row.percent))
        } else {
            theme::DIM
        };
        let ring = percent_ring::percent_ring(row.percent, ring_color, ring_size, None, 0.0);
        let cell: Element<'a, StartMessage> = if selected {
            container(ring)
                .padding(2.0)
                .style(move |_: &Theme| container::Style {
                    border: Border {
                        color: theme::ACCENT,
                        width: 2.0,
                        radius: ((ring_size + 4.0) / 2.0).into(),
                    },
                    ..container::Style::default()
                })
                .into()
        } else {
            container(ring).padding(2.0).into()
        };
        strip = strip.push(cell);
    }
    strip.into()
}
