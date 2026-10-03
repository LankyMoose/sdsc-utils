//! Fullscreen console-style presentation of the start screen.

use crate::ui::color::BatterySpectrum;
use crate::ui::percent_ring;
use crate::ui::start::view::{
    StartControllerRow, StartMessage, StartRow, StartSlide, State, footer_hint, manual_add_view,
    replace_confirm_view, slide_header,
};
use crate::ui::theme;
use iced::font::Weight;
use iced::widget::{button, column, container, row, space, text};
use iced::{Alignment, Element, Fill, Font, Length, Padding};
use std::time::Instant;

const PAD: f32 = 48.0;
const HERO_W: f32 = 200.0;
const HERO_H: f32 = 300.0;
const RAIL_W: f32 = 320.0;
const RAIL_ROW_H: f32 = 40.0;
const CONTROLLER_RING: f32 = 160.0;
const MODAL_W: f32 = 640.0;
const RAIL_NEIGHBORS: isize = 4;

pub fn view<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    now: Instant,
) -> Element<'a, StartMessage> {
    let header = slide_header(state.slide_progress(now));

    let body: Element<'_, StartMessage> = if state.manual_add.is_some() {
        modal_card(manual_add_view(state))
    } else if state.replace_confirm.is_some() {
        modal_card(replace_confirm_view(state))
    } else {
        match state.slide {
            StartSlide::Games => games_stage(state),
            StartSlide::Controllers => controllers_stage(state, spectrum),
        }
    };

    let hint = footer_hint(state, true);

    container(
        column![
            container(header).width(Fill).padding(Padding {
                top: PAD * 0.5,
                right: PAD,
                bottom: 0.0,
                left: PAD,
            }),
            container(body).width(Fill).height(Fill).padding(Padding {
                top: 16.0,
                right: PAD,
                bottom: 16.0,
                left: PAD,
            }),
            container(hint).width(Fill).padding(Padding {
                top: 0.0,
                right: PAD,
                bottom: PAD * 0.6,
                left: PAD,
            }),
        ]
        .width(Fill)
        .height(Fill),
    )
    .width(Fill)
    .height(Fill)
    .style(theme::root)
    .into()
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

fn games_stage(state: &State) -> Element<'_, StartMessage> {
    if state.rows.is_empty() {
        return empty_games(state);
    }

    let selected = state.game_selected.min(state.rows.len().saturating_sub(1));
    let row = &state.rows[selected];
    // Warm hero art for the selection and its immediate neighbors.
    warm_hero_neighbors(&state.rows, selected);

    let hero = hero_capsule(row);
    let title = text(&row.title).size(36.0).color(theme::INK).font(Font {
        weight: Weight::Bold,
        ..Font::DEFAULT
    });
    let subtitle = row
        .subtitle
        .as_ref()
        .map(|s| text(s.clone()).size(16.0).color(theme::MUTED));

    let mut details = column![hero, space().height(Length::Fixed(20.0)), title].spacing(0);
    if let Some(sub) = subtitle {
        details = details.push(space().height(Length::Fixed(8.0)));
        details = details.push(sub);
    }
    if state.editing {
        let mark = if row.in_catalog() {
            "In library — Cross to remove"
        } else {
            "Not in library — Cross to add"
        };
        details = details.push(space().height(Length::Fixed(12.0)));
        details = details.push(text(mark).size(14.0).color(theme::ACCENT));
    } else if state
        .running_target
        .as_ref()
        .is_some_and(|t| t == &row.target)
    {
        details = details.push(space().height(Length::Fixed(12.0)));
        details = details.push(text("Playing").size(14.0).color(theme::ACCENT));
    }

    let featured = container(details)
        .width(Fill)
        .height(Fill)
        .align_y(Alignment::Center)
        .align_x(Alignment::Start);

    row![featured, title_rail(&state.rows, selected, state.editing)]
        .spacing(40)
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
    for delta in -1..=1 {
        let idx = (selected as isize + delta).rem_euclid(len) as usize;
        let _ = rows[idx].hero_icon();
    }
}

fn hero_capsule(row: &StartRow) -> Element<'_, StartMessage> {
    let inner: Element<'_, StartMessage> = if row.skeleton {
        container(space())
            .width(Length::Fixed(HERO_W))
            .height(Length::Fixed(HERO_H))
            .style(theme::well)
            .into()
    } else {
        match row.hero_icon() {
            Some(icon) => iced::widget::image(icon.0)
                .width(Length::Fixed(HERO_W))
                .height(Length::Fixed(HERO_H))
                .content_fit(iced::ContentFit::Cover)
                .into(),
            None => container(
                text(row.title.chars().next().unwrap_or('?').to_string())
                    .size(64.0)
                    .color(theme::MUTED),
            )
            .width(Length::Fixed(HERO_W))
            .height(Length::Fixed(HERO_H))
            .style(theme::well)
            .center_x(Fill)
            .center_y(Fill)
            .into(),
        }
    };
    container(inner)
        .width(Length::Fixed(HERO_W))
        .height(Length::Fixed(HERO_H))
        .into()
}

fn title_rail(rows: &[StartRow], selected: usize, editing: bool) -> Element<'_, StartMessage> {
    let len = rows.len() as isize;
    let mut items = column![].spacing(4).width(Length::Fixed(RAIL_W));

    if len == 0 {
        return space().into();
    }
    for delta in -RAIL_NEIGHBORS..=RAIL_NEIGHBORS {
        let idx = (selected as isize + delta).rem_euclid(len) as usize;
        let row = &rows[idx];
        let is_sel = idx == selected;
        let muted = editing && !row.in_catalog();
        let label = if row.skeleton {
            "…".to_string()
        } else if row.title.is_empty() {
            "Untitled".to_string()
        } else {
            row.title.clone()
        };
        let color = if is_sel {
            theme::INK
        } else if muted {
            theme::alpha(theme::MUTED, 0.4)
        } else {
            theme::alpha(theme::MUTED, 0.75)
        };
        let size = if is_sel { 20.0 } else { 15.0 };
        let font = if is_sel {
            Font {
                weight: Weight::Bold,
                ..Font::DEFAULT
            }
        } else {
            Font::DEFAULT
        };
        items = items.push(
            button(
                text(label)
                    .size(size)
                    .color(color)
                    .font(font)
                    .wrapping(iced::widget::text::Wrapping::Word),
            )
            .padding([8, 12])
            .width(Fill)
            .height(Length::Fixed(RAIL_ROW_H))
            .on_press(StartMessage::Launch(idx))
            .style(theme::menu_row(is_sel)),
        );
    }

    container(items)
        .width(Length::Fixed(RAIL_W))
        .height(Fill)
        .align_y(Alignment::Center)
        .into()
}

fn controllers_stage<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
) -> Element<'a, StartMessage> {
    if state.controllers.is_empty() {
        return container(
            text("No controllers — connect a DualSense to begin.")
                .size(22.0)
                .color(theme::MUTED),
        )
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .into();
    }

    let selected = state
        .controller_selected
        .min(state.controllers.len().saturating_sub(1));
    let row = &state.controllers[selected];
    let featured = featured_controller(row, spectrum, state.ring_flash_white(&row.serial));
    let rail = controller_rail(&state.controllers, selected);

    row![
        container(featured)
            .width(Fill)
            .height(Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center),
        rail,
    ]
    .spacing(40)
    .width(Fill)
    .height(Fill)
    .into()
}

fn featured_controller<'a>(
    row: &'a StartControllerRow,
    spectrum: &BatterySpectrum,
    flash_white: bool,
) -> Element<'a, StartMessage> {
    let ring_color = if flash_white {
        theme::from_rgb(crate::controller::dualsense::lightbar::IDENTIFY_FLASH)
    } else if row.connected {
        theme::from_rgb(spectrum.color_at_percent(row.percent))
    } else {
        theme::DIM
    };
    let ring =
        percent_ring::percent_ring(row.percent, ring_color, CONTROLLER_RING, row.eta.clone());
    let title_color = if row.connected {
        theme::INK
    } else {
        theme::MUTED
    };
    let meta_color = if row.low {
        theme::WARNING
    } else {
        theme::MUTED
    };

    column![
        ring,
        space().height(Length::Fixed(24.0)),
        text(&row.title).size(32.0).color(title_color).font(Font {
            weight: Weight::Bold,
            ..Font::DEFAULT
        }),
        space().height(Length::Fixed(8.0)),
        text(format!("{} · {}", row.connection, row.state))
            .size(16.0)
            .color(meta_color),
    ]
    .align_x(Alignment::Center)
    .into()
}

fn controller_rail(
    controllers: &[StartControllerRow],
    selected: usize,
) -> Element<'_, StartMessage> {
    let len = controllers.len() as isize;
    if len == 0 {
        return space().into();
    }
    let mut items = column![].spacing(4).width(Length::Fixed(RAIL_W));
    for delta in -RAIL_NEIGHBORS..=RAIL_NEIGHBORS {
        let idx = (selected as isize + delta).rem_euclid(len) as usize;
        let row = &controllers[idx];
        let is_sel = idx == selected;
        let color = if is_sel {
            theme::INK
        } else if row.connected {
            theme::alpha(theme::MUTED, 0.75)
        } else {
            theme::alpha(theme::MUTED, 0.4)
        };
        items = items.push(
            button(
                text(format!("{}  {}%", row.title, row.percent))
                    .size(if is_sel { 18.0 } else { 14.0 })
                    .color(color)
                    .font(if is_sel {
                        Font {
                            weight: Weight::Bold,
                            ..Font::DEFAULT
                        }
                    } else {
                        Font::DEFAULT
                    }),
            )
            .padding([8, 12])
            .width(Fill)
            .height(Length::Fixed(RAIL_ROW_H))
            .on_press(StartMessage::SelectController(idx))
            .style(theme::menu_row(is_sel)),
        );
    }
    container(items)
        .width(Length::Fixed(RAIL_W))
        .height(Fill)
        .align_y(Alignment::Center)
        .into()
}
