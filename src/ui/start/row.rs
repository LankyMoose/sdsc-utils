//! Compact games + controller list rows (browse slide).
//!
//! Behavior-preserving split from [`crate::ui::start::view`]: row bodies live
//! here, shared face-hint helpers come from [`crate::ui::start::face`].

use crate::controller::dualsense::lightbar;
use crate::ui::color::BatterySpectrum;
use crate::ui::percent_ring::{self, POPUP_SIZE};
use crate::ui::start::face::{FaceButton, action_cluster_spaced, face_hint, face_hold_hint};
use crate::ui::start::view::{
    CONTROLLER_ROW_HEIGHT, EditRow, ICON_H, ICON_W, LaunchStatus, ROW_ACTION_SPACING, ROW_HEIGHT,
    RowHintState, StartControllerRow, StartMessage, StartRow, StartSubtitle,
};
use crate::ui::{svg_icon, theme};
use iced::font::Weight;
use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, row, space, svg, text};
use iced::{
    Alignment, Background, Border, Color, ContentFit, Element, Fill, Font, Length, Padding, Theme,
};

fn skeleton_bar(width: f32, height: f32) -> Element<'static, StartMessage> {
    container(space())
        .width(Length::Fixed(width))
        .height(Length::Fixed(height))
        .style(|_theme| container::Style {
            background: Some(Background::Color(theme::alpha(theme::MUTED, 0.22))),
            border: Border {
                radius: theme::RADIUS_SM.into(),
                ..Default::default()
            },
            ..container::Style::default()
        })
        .into()
}

/// Soft pill for pending Steam updates (compact accent; immersive grey).
pub(crate) fn update_required_badge(
    selected: bool,
    muted: bool,
    fade: f32,
    immersive: bool,
) -> Element<'static, StartMessage> {
    let fade = fade.clamp(0.0, 1.0);
    let a = if muted { 0.55 * fade } else { fade };
    let label_size = if selected { 13.0 } else { 11.0 };
    let (fill, stroke) = if immersive {
        (
            theme::alpha(theme::MUTED, 0.22 * a),
            theme::alpha(theme::MUTED, 0.40 * a),
        )
    } else {
        (
            theme::alpha(theme::ACCENT, 0.22 * a),
            theme::alpha(theme::ACCENT, 0.45 * a),
        )
    };
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

/// Compact combined status string (tests / fallbacks). UI prefers meta + badge.
#[cfg(test)]
pub(crate) fn compact_status_label(row: &StartRow) -> Option<String> {
    let show_meta = row
        .subtitle
        .as_ref()
        .is_some_and(|sub| !(row.update_required && sub.is_steam_fallback()));
    match (&row.subtitle, row.update_required, show_meta) {
        (None, false, _) => None,
        (None, true, _) | (Some(_), true, false) => Some("Update required".into()),
        (Some(StartSubtitle::Plain(text)), false, _) => Some(text.clone()),
        (Some(StartSubtitle::Plain(text)), true, true) => Some(format!("{text} · Update required")),
        (Some(StartSubtitle::Labeled(lines)), false, _) => Some(
            lines
                .iter()
                .map(|l| format!("{}: {}", l.label, l.value))
                .collect::<Vec<_>>()
                .join(" · "),
        ),
        (Some(StartSubtitle::Labeled(lines)), true, true) => Some(format!(
            "{} · Update required",
            lines
                .iter()
                .map(|l| format!("{}: {}", l.label, l.value))
                .collect::<Vec<_>>()
                .join(" · ")
        )),
    }
}

pub(crate) fn game_row(
    _index: usize,
    row: &StartRow,
    selected: bool,
    status: Option<LaunchStatus>,
    editing: bool,
    hint: RowHintState,
) -> Element<'_, StartMessage> {
    let running = status == Some(LaunchStatus::Running);
    let launching = status == Some(LaunchStatus::Launching);
    let closing = status == Some(LaunchStatus::Closing);
    let muted = editing && !row.in_catalog();
    let disabled = row.disabled && !editing;
    let title_color = if disabled {
        theme::alpha(theme::INK, 0.35)
    } else if muted {
        theme::alpha(theme::INK, 0.45)
    } else {
        theme::INK
    };
    let sub_color = if disabled {
        theme::alpha(theme::MUTED, 0.45)
    } else if muted {
        theme::alpha(theme::MUTED, 0.55)
    } else {
        theme::MUTED
    };

    let icon_inner: Element<'_, StartMessage> = if row.skeleton {
        container(space())
            .width(Length::Fixed(ICON_W))
            .height(Length::Fixed(ICON_H))
            .style(theme::well)
            .into()
    } else {
        match row.list_icon_live() {
            Some(icon) => iced::widget::image(icon.0)
                .width(Length::Fixed(ICON_W))
                .height(Length::Fixed(ICON_H))
                .content_fit(ContentFit::Cover)
                .into(),
            None => container(
                svg(svg::Handle::from_memory(svg_icon::GAME_SVG.as_bytes()))
                    .width(Length::Fixed(ICON_W * 0.5))
                    .height(Length::Fixed(ICON_W * 0.5))
                    .style(|_theme, _status| svg::Style {
                        color: Some(theme::MUTED),
                    }),
            )
            .width(Length::Fixed(ICON_W))
            .height(Length::Fixed(ICON_H))
            .center_x(Fill)
            .center_y(Fill)
            .style(theme::well)
            .into(),
        }
    };

    let icon = container(icon_inner)
        .width(Length::Fixed(ICON_W))
        .height(Length::Fixed(ICON_H))
        .clip(true)
        .style(|_| container::Style {
            border: Border {
                radius: theme::RADIUS_SM.into(),
                ..Default::default()
            },
            ..container::Style::default()
        });

    let titles: Element<'_, StartMessage> = if row.skeleton {
        column![skeleton_bar(220.0, 14.0), skeleton_bar(160.0, 10.0),]
            .spacing(8)
            .width(Fill)
            .into()
    } else {
        let title_text = text(&row.title)
            .size(19.0)
            .color(title_color)
            .font(if selected {
                Font {
                    weight: Weight::Bold,
                    ..Font::DEFAULT
                }
            } else {
                Font::DEFAULT
            })
            .wrapping(Wrapping::None);

        // Update badge sits on the title row in compact (immersive keeps it under meta).
        let title: Element<'_, StartMessage> = if !running && !launching && row.update_required {
            row![
                title_text,
                update_required_badge(selected, muted, 1.0, false),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .width(Fill)
            .into()
        } else {
            title_text.width(Fill).into()
        };

        let status: Element<'_, StartMessage> = if closing {
            text("Closing")
                .size(13.0)
                .color(theme::MUTED)
                .wrapping(Wrapping::None)
                .width(Fill)
                .into()
        } else if launching {
            text("Launching")
                .size(13.0)
                .color(theme::ACCENT)
                .wrapping(Wrapping::None)
                .width(Fill)
                .into()
        } else if running {
            text("Running")
                .size(13.0)
                .color(theme::SUCCESS)
                .wrapping(Wrapping::None)
                .width(Fill)
                .into()
        } else if disabled {
            text("Not installed")
                .size(13.0)
                .color(theme::alpha(theme::MUTED, 0.7))
                .wrapping(Wrapping::None)
                .width(Fill)
                .into()
        } else {
            let show_meta = row
                .subtitle
                .as_ref()
                .is_some_and(|sub| !(row.update_required && sub.is_steam_fallback()));
            if show_meta && let Some(sub) = row.subtitle.as_ref() {
                game_subtitle_block(sub, sub_color, sub_color, 12.0, Alignment::Start)
            } else {
                text(" ")
                    .size(13.0)
                    .color(Color::TRANSPARENT)
                    .wrapping(Wrapping::None)
                    .width(Fill)
                    .into()
            }
        };

        column![title, status]
            .spacing(4)
            .width(Fill)
            .clip(true)
            .into()
    };

    let mut content = row![icon].spacing(14).align_y(Alignment::Center);

    if editing {
        let mark = if row.in_catalog() { "✓" } else { "○" };
        let mark_color = if row.in_catalog() {
            theme::ACCENT
        } else {
            theme::alpha(theme::MUTED, 0.55)
        };
        content = content.push(
            text(mark)
                .size(20.0)
                .color(mark_color)
                .width(Length::Fixed(22.0)),
        );
    }

    content = content.push(titles);

    if selected {
        let mut actions = Vec::new();
        if editing {
            let label = match row.edit.as_ref() {
                Some(EditRow::Manual { .. }) => "Remove",
                Some(EditRow::Steam { .. }) | None => "Toggle",
            };
            actions.push(face_hint(
                FaceButton::Cross,
                label,
                hint.held,
                hint.press_anim,
            ));
            // Square cycles sort in edit too — manual Edit is mouse-only
            // (the Edit button below).
        } else {
            if closing {
                actions.push(face_hint(
                    FaceButton::Cross,
                    "Closing…",
                    hint.held,
                    hint.press_anim,
                ));
            } else if launching {
                actions.push(face_hint(
                    FaceButton::Cross,
                    "Launching…",
                    hint.held,
                    hint.press_anim,
                ));
            } else if running {
                actions.push(face_hold_hint(
                    FaceButton::Cross,
                    "Close game",
                    hint.cross_progress,
                    hint.cross_armed_t,
                    hint.held,
                    hint.press_anim,
                ));
            } else if disabled {
                actions.push(face_hint(
                    FaceButton::Cross,
                    "Not installed",
                    hint.held,
                    hint.press_anim,
                ));
            } else {
                actions.push(face_hint(
                    FaceButton::Cross,
                    row.launch_hint_label(),
                    hint.held,
                    hint.press_anim,
                ));
            }
        }
        content = content.push(action_cluster_spaced(&actions, ROW_ACTION_SPACING, false));
        // Manual Edit is mouse-only (Square cycles sort in edit too).
        if editing && matches!(row.edit, Some(EditRow::Manual { .. })) {
            content = content.push(
                button(text("Edit").size(14.0).color(theme::ACCENT))
                    .padding([6, 12])
                    .on_press(StartMessage::EditManual)
                    .style(theme::ghost),
            );
        }
    }

    // Pad/keyboard navigate — no mouse press/hover on list rows (edit actions stay clickable).
    container(content.width(Fill).height(Length::Fixed(ROW_HEIGHT)))
        .padding([0, 12])
        .width(Fill)
        .height(Length::Fixed(ROW_HEIGHT))
        .style(theme::glass_row(selected, theme::radius::MD))
        .into()
}

pub(crate) fn controller_row<'a>(
    _index: usize,
    row: &'a StartControllerRow,
    selected: bool,
    powering_off: bool,
    spectrum: &BatterySpectrum,
    ring_flash_white: bool,
    hint: RowHintState,
) -> Element<'a, StartMessage> {
    let ring_color = if ring_flash_white {
        theme::from_rgb(lightbar::IDENTIFY_FLASH)
    } else if row.connected && !powering_off {
        theme::from_rgb(spectrum.color_at_percent(row.percent))
    } else {
        theme::DIM
    };
    let ring = percent_ring::percent_ring(
        row.percent,
        ring_color,
        POPUP_SIZE * 0.95,
        row.eta.clone(),
        1.0,
    );

    let meta_color = if row.low {
        theme::WARNING
    } else {
        theme::MUTED
    };
    let meta_text = if powering_off {
        "Powering off".to_string()
    } else {
        format!("{} · {}", row.connection, row.state)
    };
    let meta_text_color = if powering_off {
        theme::MUTED
    } else {
        meta_color
    };
    let title_color = if row.connected && !powering_off {
        theme::INK
    } else {
        theme::MUTED
    };
    let titles = column![
        text(&row.title)
            .size(19.0)
            .color(title_color)
            .font(if selected {
                Font {
                    weight: Weight::Bold,
                    ..Font::DEFAULT
                }
            } else {
                Font::DEFAULT
            }),
        text(meta_text).size(13.0).color(meta_text_color),
    ]
    .spacing(4)
    .width(Fill)
    .clip(true);

    let mut content = row![ring, titles].spacing(14).align_y(Alignment::Center);

    if selected && row.connected {
        // A pad on its way out offers no actions: Identify and Power off are
        // both dead ends (the worker drops the pad any moment).
        if powering_off {
            content = content.push(action_cluster_spaced(
                &[face_hint(
                    FaceButton::Triangle,
                    "Powering off…",
                    hint.held,
                    hint.press_anim,
                )],
                ROW_ACTION_SPACING,
                false,
            ));
        } else {
            let mut actions = vec![face_hint(
                FaceButton::Cross,
                "Identify",
                hint.held,
                hint.press_anim,
            )];
            if row.show_power_off() {
                actions.push(face_hold_hint(
                    FaceButton::Triangle,
                    "Power off",
                    hint.triangle_progress,
                    hint.triangle_armed_t,
                    hint.held,
                    hint.press_anim,
                ));
            }
            content = content.push(action_cluster_spaced(&actions, ROW_ACTION_SPACING, false));
        }
    }

    container(
        content
            .width(Fill)
            .height(Length::Fixed(CONTROLLER_ROW_HEIGHT)),
    )
    .padding([0, 12])
    .width(Fill)
    .height(Length::Fixed(CONTROLLER_ROW_HEIGHT))
    .style(theme::glass_row(selected, theme::radius::MD))
    .into()
}
