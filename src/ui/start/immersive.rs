//! Fullscreen console-style presentation of the start screen.

use crate::ui::color::BatterySpectrum;
use crate::ui::percent_ring;
use crate::ui::shader::AmbientProgram;
use crate::ui::start::view::{
    StartControllerRow, StartMessage, StartRow, StartSlide, State, footer_hint, manual_add_view,
    replace_confirm_view,
};
use crate::ui::start::vstrip::{self, NEIGHBORS};
use crate::ui::theme;
use iced::font::Weight;
use iced::widget::{button, column, container, row, shader, space, stack, text};
use iced::{Alignment, Element, Fill, Font, Length, Padding};
use std::time::Instant;

const PAD: f32 = 40.0;
const DOCK_MIN_W: f32 = 80.0;
const DOCK_MAX_W: f32 = 340.0;
const DOCK_RING_MIN: f32 = 44.0;
const DOCK_RING_MAX: f32 = 72.0;
const MODAL_W: f32 = 640.0;
const CAPSULE_W: f32 = 240.0;

pub fn veil_view(state: &State, now: Instant) -> Element<'_, StartMessage> {
    let dock = state.dock_progress(now);
    let program = AmbientProgram::new(state.ambient_time, dock, 1.0);
    stack![
        shader(program).width(Fill).height(Fill),
        container(space())
            .width(Fill)
            .height(Fill)
            .style(theme::root),
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

pub fn view<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    now: Instant,
) -> Element<'a, StartMessage> {
    let dock_p = state.dock_progress(now);
    let program = AmbientProgram::new(state.ambient_time, dock_p, 0.0);

    let body: Element<'_, StartMessage> = if state.manual_add.is_some() {
        modal_card(manual_add_view(state))
    } else if state.replace_confirm.is_some() {
        modal_card(replace_confirm_view(state))
    } else {
        let stage = games_stage(state, now);
        let dock = controllers_dock(state, spectrum, dock_p);
        row![
            container(stage)
                .width(Fill)
                .height(Fill)
                .padding(Padding {
                    top: 12.0,
                    right: 16.0,
                    bottom: 12.0,
                    left: PAD,
                })
                .style(theme::immersive_stage),
            dock,
        ]
        .spacing(16)
        .width(Fill)
        .height(Fill)
        .into()
    };

    let header = dock_header(dock_p);
    let hint = footer_hint(state, true);

    let chrome = column![
        container(header).width(Fill).padding(Padding {
            top: PAD * 0.45,
            right: PAD,
            bottom: 0.0,
            left: PAD,
        }),
        container(body).width(Fill).height(Fill).padding(Padding {
            top: 8.0,
            right: PAD,
            bottom: 8.0,
            left: 0.0,
        }),
        container(hint).width(Fill).padding(Padding {
            top: 0.0,
            right: PAD,
            bottom: PAD * 0.55,
            left: PAD,
        }),
    ]
    .width(Fill)
    .height(Fill);

    stack![shader(program).width(Fill).height(Fill), chrome,]
        .width(Fill)
        .height(Fill)
        .into()
}

fn dock_header(dock_progress: f32) -> Element<'static, StartMessage> {
    let games_t = dock_progress;
    let controllers_t = 1.0 - dock_progress;
    let left = row![
        text("L2")
            .size(13.0)
            .color(theme::alpha(theme::ACCENT, 0.35 + 0.65 * (1.0 - games_t))),
        text("Games")
            .size(lerp(20.0, 15.0, games_t))
            .color(lerp_color(
                theme::INK,
                theme::alpha(theme::MUTED, 0.45),
                games_t,
            )),
    ]
    .spacing(8)
    .align_y(Alignment::End);

    let right = row![
        text("Controllers")
            .size(lerp(20.0, 15.0, controllers_t))
            .color(lerp_color(
                theme::INK,
                theme::alpha(theme::MUTED, 0.45),
                controllers_t,
            )),
        text("R2").size(13.0).color(theme::alpha(
            theme::ACCENT,
            0.35 + 0.65 * (1.0 - controllers_t)
        )),
    ]
    .spacing(8)
    .align_y(Alignment::End);

    row![
        container(left).width(Fill).align_x(Alignment::Start),
        container(right).width(Fill).align_x(Alignment::End),
    ]
    .height(Length::Fixed(36.0))
    .into()
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

fn lerp_color(a: iced::Color, b: iced::Color, t: f32) -> iced::Color {
    let t = t.clamp(0.0, 1.0);
    iced::Color {
        r: lerp(a.r, b.r, t),
        g: lerp(a.g, b.g, t),
        b: lerp(a.b, b.b, t),
        a: lerp(a.a, b.a, t),
    }
}

fn modal_card(inner: Element<'_, StartMessage>) -> Element<'_, StartMessage> {
    container(
        container(theme::framed(inner))
            .width(Length::Fixed(MODAL_W))
            .max_height(520.0),
    )
    .width(Fill)
    .height(Fill)
    .center_x(Fill)
    .center_y(Fill)
    .into()
}

fn games_stage(state: &State, now: Instant) -> Element<'_, StartMessage> {
    if state.rows.is_empty() {
        return empty_games(state);
    }

    let selected = state.game_selected.min(state.rows.len().saturating_sub(1));
    warm_hero_neighbors(&state.rows, selected);
    let visual = state.strip_scroll(now);
    let len = state.rows.len() as isize;

    let mut items = Vec::with_capacity(vstrip::VISIBLE);
    for delta in -NEIGHBORS..=NEIGHBORS {
        let idx = (selected as isize + delta).rem_euclid(len) as usize;
        let row = &state.rows[idx];
        items.push(strip_capsule(row, idx, idx == selected, state.editing));
    }

    let strip = vstrip::vstrip(visual, selected, items)
        .width(Length::Fixed(CAPSULE_W + 48.0))
        .height(Fill);

    let row = &state.rows[selected];
    let mut details = column![text(&row.title).size(34.0).color(theme::INK).font(Font {
        weight: Weight::Bold,
        ..Font::DEFAULT
    }),]
    .spacing(8);
    if let Some(sub) = row.subtitle.as_ref() {
        details = details.push(text(sub.clone()).size(15.0).color(theme::MUTED));
    }
    if state.editing {
        let mark = if row.in_catalog() {
            "In library — Cross to remove"
        } else {
            "Not in library — Cross to add"
        };
        details = details.push(text(mark).size(14.0).color(theme::ACCENT));
    } else if state
        .running_target
        .as_ref()
        .is_some_and(|t| t == &row.target)
    {
        details = details.push(text("Playing").size(14.0).color(theme::ACCENT));
    }

    row![
        strip,
        container(details)
            .width(Fill)
            .height(Fill)
            .align_y(Alignment::Center)
            .padding(Padding {
                top: 0.0,
                right: 12.0,
                bottom: 0.0,
                left: 28.0,
            }),
    ]
    .spacing(8)
    .width(Fill)
    .height(Fill)
    .into()
}

fn empty_games(state: &State) -> Element<'_, StartMessage> {
    let copy = if state.editing {
        "No installed Steam games — add a manual shortcut."
    } else {
        "No games yet — press Triangle to edit."
    };
    container(text(copy).size(22.0).color(theme::MUTED))
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .into()
}

fn warm_hero_neighbors(rows: &[StartRow], selected: usize) {
    let len = rows.len() as isize;
    if len == 0 {
        return;
    }
    for delta in -2..=2 {
        let idx = (selected as isize + delta).rem_euclid(len) as usize;
        let _ = rows[idx].hero_icon();
    }
}

fn strip_capsule(
    row: &StartRow,
    index: usize,
    selected: bool,
    editing: bool,
) -> Element<'_, StartMessage> {
    let muted = editing && !row.in_catalog();
    let inner: Element<'_, StartMessage> = if row.skeleton {
        container(space())
            .width(Fill)
            .height(Fill)
            .style(theme::well)
            .into()
    } else {
        match row.hero_icon() {
            Some(icon) => iced::widget::image(icon.0)
                .width(Fill)
                .height(Fill)
                .content_fit(iced::ContentFit::Cover)
                .into(),
            None => container(
                text(row.title.chars().next().unwrap_or('?').to_string())
                    .size(if selected { 56.0 } else { 36.0 })
                    .color(if muted {
                        theme::alpha(theme::MUTED, 0.5)
                    } else {
                        theme::MUTED
                    }),
            )
            .width(Fill)
            .height(Fill)
            .style(theme::well)
            .center_x(Fill)
            .center_y(Fill)
            .into(),
        }
    };

    let border = if selected {
        theme::alpha(theme::ACCENT, 0.85)
    } else {
        theme::alpha(theme::LINE, 0.5)
    };

    let card = container(inner)
        .width(Fill)
        .height(Fill)
        .style(move |_theme: &iced::Theme| iced::widget::container::Style {
            background: Some(iced::Background::Color(theme::CONTENT)),
            border: iced::Border {
                color: border,
                width: if selected { 2.0 } else { 1.0 },
                radius: theme::RADIUS.into(),
            },
            ..iced::widget::container::Style::default()
        });

    button(card)
        .padding(0)
        .width(Fill)
        .height(Fill)
        .on_press(if selected {
            StartMessage::Launch(index)
        } else {
            StartMessage::SelectGame(index)
        })
        .style(theme::ghost)
        .into()
}

fn controllers_dock<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    dock_progress: f32,
) -> Element<'a, StartMessage> {
    let width = lerp(DOCK_MIN_W, DOCK_MAX_W, dock_progress);
    let expanded = dock_progress > 0.45;

    let connected: Vec<&StartControllerRow> =
        state.controllers.iter().filter(|c| c.connected).collect();
    let list: Vec<&StartControllerRow> = if expanded {
        state.controllers.iter().collect()
    } else {
        connected
    };

    let mut items = column![].spacing(10).width(Fill);
    if list.is_empty() {
        items = items.push(
            text(if expanded { "No controllers" } else { "—" })
                .size(12.0)
                .color(theme::DIM),
        );
    } else {
        for (i, row) in state.controllers.iter().enumerate() {
            if !expanded && !row.connected {
                continue;
            }
            let selected =
                i == state.controller_selected && matches!(state.slide, StartSlide::Controllers);
            items = items.push(dock_row(
                i,
                row,
                spectrum,
                selected,
                expanded,
                state.ring_flash_white(&row.serial),
                dock_progress,
            ));
        }
    }

    container(container(items).width(Fill).height(Fill).padding(Padding {
        top: 16.0,
        right: 12.0,
        bottom: 16.0,
        left: 12.0,
    }))
    .width(Length::Fixed(width))
    .height(Fill)
    .style(theme::immersive_dock)
    .into()
}

fn dock_row<'a>(
    index: usize,
    row: &'a StartControllerRow,
    spectrum: &BatterySpectrum,
    selected: bool,
    expanded: bool,
    flash_white: bool,
    dock_progress: f32,
) -> Element<'a, StartMessage> {
    let ring_color = if flash_white {
        theme::from_rgb(crate::controller::dualsense::lightbar::IDENTIFY_FLASH)
    } else if row.connected {
        theme::from_rgb(spectrum.color_at_percent(row.percent))
    } else {
        theme::DIM
    };
    let ring_size = lerp(DOCK_RING_MIN, DOCK_RING_MAX, dock_progress);
    let ring = percent_ring::percent_ring(row.percent, ring_color, ring_size, row.eta.clone());

    let content: Element<'_, StartMessage> = if expanded {
        let title_color = if row.connected {
            theme::INK
        } else {
            theme::MUTED
        };
        let meta = if row.low {
            theme::WARNING
        } else {
            theme::MUTED
        };
        row![
            ring,
            column![
                text(&row.title).size(16.0).color(title_color).font(Font {
                    weight: if selected {
                        Weight::Bold
                    } else {
                        Weight::Normal
                    },
                    ..Font::DEFAULT
                }),
                text(format!("{} · {}", row.connection, row.state))
                    .size(12.0)
                    .color(meta),
            ]
            .spacing(4)
            .width(Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .into()
    } else {
        container(ring).center_x(Fill).into()
    };

    button(content)
        .padding(if expanded { [8, 8] } else { [4, 4] })
        .width(Fill)
        .on_press(StartMessage::SelectController(index))
        .style(theme::menu_row(selected))
        .into()
}
