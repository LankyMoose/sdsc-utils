//! Fullscreen console-style presentation of the start screen.

use crate::ui::color::BatterySpectrum;
use crate::ui::percent_ring;
use crate::ui::shader::AmbientProgram;
use crate::ui::start::mode::{
    TransitionPhase, chrome_stagger, dock_panel_width, dock_stage_dim, dock_stage_scale,
    enter_aperture, enter_chrome_scale, exit_aperture, exit_chrome_scale,
};
use crate::ui::start::view::{
    StartControllerRow, StartMessage, StartRow, StartSlide, State, footer_hint,
    immersive_dock_row_hints, immersive_slide_header, manual_add_view, replace_confirm_view,
};
use crate::ui::start::vstrip::{self, CENTER_H, CENTER_W, NEIGHBORS, SELECTED_SCALE, SLOT_W};
use crate::ui::theme;
use iced::font::Weight;
use iced::widget::{Float, button, column, container, row, shader, space, stack, text};
use iced::{Alignment, Element, Fill, Font, Length, Padding};
use std::time::Instant;

const EDGE_PAD: f32 = 28.0;
const DOCK_PEEK_W: f32 = 110.0;
const DOCK_MAX_W: f32 = 360.0;
const DOCK_RING_MIN: f32 = 56.0;
/// Fixed dock row height so rings do not jump when details/hints appear.
const DOCK_ROW_H: f32 = 88.0;
const MODAL_W: f32 = 640.0;

pub fn veil_view(state: &State, now: Instant) -> Element<'_, StartMessage> {
    let dock = state.dock_progress(now);
    let program = AmbientProgram::new(state.ambient_time, dock, 1.0, 0.0);
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

/// Compact chrome blackout (`veil_amount` 0 = clear, 1 = solid black). No ambient overlay.
pub fn compact_transition_overlay(
    compact: Element<'_, StartMessage>,
    veil_amount: f32,
    exiting: bool,
) -> Element<'_, StartMessage> {
    let veil = veil_amount.clamp(0.0, 1.0);
    let scale = if exiting {
        1.0 - 0.08 * veil
    } else {
        0.92 + 0.08 * (1.0 - veil)
    };
    let scaled = container(Float::new(compact).scale(scale))
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill);
    stack![
        scaled,
        container(space())
            .width(Fill)
            .height(Fill)
            .style(theme::immersive_dim(veil)),
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
    let (aperture, chrome_scale, header_a, stage_a, footer_a, veil) =
        immersive_chrome_params(state, now);

    let program = AmbientProgram::new(state.ambient_time, dock_p, veil, aperture);

    let atmosphere = shader(program).width(Fill).height(Fill);

    let body: Element<'_, StartMessage> = if state.manual_add.is_some() {
        modal_card(manual_add_view(state))
    } else if state.replace_confirm.is_some() {
        modal_card(replace_confirm_view(state))
    } else {
        stage_with_dock(state, spectrum, now, dock_p)
    };

    let header = container(immersive_slide_header(dock_p))
        .width(Fill)
        .padding(Padding {
            top: 14.0,
            right: EDGE_PAD,
            bottom: 10.0,
            left: EDGE_PAD,
        })
        .style(theme::immersive_header_band);

    let footer_capsule = container(footer_hint(state, true))
        .padding(Padding {
            top: 10.0,
            right: 28.0,
            bottom: 10.0,
            left: 28.0,
        })
        .style(theme::immersive_footer_capsule);

    // Header + full-height body (dock reaches window bottom); footer overlays.
    let main = column![
        container(header).style(theme::immersive_dim(1.0 - header_a)),
        container(body)
            .width(Fill)
            .height(Fill)
            .style(theme::immersive_dim(1.0 - stage_a)),
    ]
    .width(Fill)
    .height(Fill);

    let footer_overlay = column![
        space().height(Fill),
        container(footer_capsule)
            .width(Fill)
            .center_x(Fill)
            .padding(Padding {
                top: 0.0,
                right: EDGE_PAD,
                bottom: 18.0,
                left: EDGE_PAD,
            })
            .style(theme::immersive_dim(1.0 - footer_a)),
    ]
    .width(Fill)
    .height(Fill);

    let chrome = stack![main, footer_overlay].width(Fill).height(Fill);

    let chrome: Element<'_, StartMessage> = if (chrome_scale - 1.0).abs() < 0.002 {
        chrome.into()
    } else {
        container(Float::new(chrome).scale(chrome_scale))
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill)
            .into()
    };

    stack![atmosphere, chrome].width(Fill).height(Fill).into()
}

fn immersive_chrome_params(state: &State, now: Instant) -> (f32, f32, f32, f32, f32, f32) {
    let progress = state.phase_progress(now);
    match state.transition_phase.map(|(p, _)| p) {
        Some(TransitionPhase::EnterImmersive) => {
            // Ambient rises first; aperture opens through the middle; chrome staggers late.
            let aperture = enter_aperture(chrome_stagger(progress, 0.12));
            let scale = enter_chrome_scale(chrome_stagger(progress, 0.22));
            let header = chrome_stagger(progress, 0.28);
            let stage = chrome_stagger(progress, 0.38);
            let footer = chrome_stagger(progress, 0.48);
            let veil = (1.0 - progress) * 0.85;
            (aperture, scale, header, stage, footer, veil)
        }
        Some(TransitionPhase::ExitImmersive) => {
            let aperture = exit_aperture(progress);
            let scale = exit_chrome_scale(progress);
            let fade = 1.0 - chrome_stagger(progress, 0.0);
            let veil = progress * 0.9;
            (aperture, scale, fade, fade, fade, veil)
        }
        _ => (1.0, 1.0, 1.0, 1.0, 1.0, 0.0),
    }
}

fn stage_backdrop_layers<'a>(state: &'a State, now: Instant) -> Vec<Element<'a, StartMessage>> {
    let selected = state.game_selected.min(state.rows.len().saturating_sub(1));
    let mut layers: Vec<Element<'_, StartMessage>> = Vec::new();
    let mut has_backdrop = false;

    if let Some((key, opacity)) = state.backdrop_outgoing_visual(now)
        && let Some(art) = state.backdrop_icon_for_key(key)
    {
        layers.push(backdrop_layer(art, opacity, 1.0));
        has_backdrop = true;
    }

    if let Some(row) = state.rows.get(selected)
        && let Some(art) = row.backdrop_icon()
        && let Some((opacity, scale, _ox, _oy)) = state.backdrop_incoming_visual(now)
        && opacity > 0.01
    {
        layers.push(backdrop_layer(art, opacity, scale));
        has_backdrop = true;
    }

    // Constant dim while any backdrop is present (not tied to incoming fade).
    if has_backdrop {
        layers.push(
            container(space())
                .width(Fill)
                .height(Fill)
                .style(theme::immersive_dim(0.58))
                .into(),
        );
    }

    layers
}

fn backdrop_layer(
    art: crate::ui::start::view::StartIcon,
    opacity: f32,
    scale: f32,
) -> Element<'static, StartMessage> {
    // Image scale/opacity only — never Float (overlay) or padding (Cover gaps).
    // Scale stays ≥ 1.0 so Cover always fills the games stage.
    let img = iced::widget::image(art.0)
        .width(Fill)
        .height(Fill)
        .content_fit(iced::ContentFit::Cover)
        .opacity(opacity.clamp(0.0, 1.0))
        .scale(scale.max(1.0));
    container(img).width(Fill).height(Fill).into()
}

fn stage_with_dock<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    now: Instant,
    dock_p: f32,
) -> Element<'a, StartMessage> {
    let scale = dock_stage_scale(dock_p);
    let dim = dock_stage_dim(dock_p);

    // Backdrop lives in the games stage (scales with dock), not the full window.
    let mut stage_layers = stage_backdrop_layers(state, now);
    stage_layers.push(games_stage(state, now));
    let stage = stack(stage_layers).width(Fill).height(Fill);

    let scaled_stage: Element<'_, StartMessage> = container(Float::new(stage).scale(scale))
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .style(theme::immersive_stage)
        .into();

    let mut layers: Vec<Element<'_, StartMessage>> = vec![scaled_stage];

    if dim > 0.01 {
        layers.push(
            container(space())
                .width(Fill)
                .height(Fill)
                .style(theme::immersive_dim(dim))
                .into(),
        );
    }

    let dock = controllers_dock_host(state, spectrum, dock_p);
    layers.push(
        row![space().width(Fill), dock]
            .width(Fill)
            .height(Fill)
            .into(),
    );

    stack(layers).width(Fill).height(Fill).into()
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
    let visual = state.strip_scroll(now);
    let len = state.rows.len() as isize;

    let mut items = Vec::with_capacity(vstrip::VISIBLE);
    for delta in -NEIGHBORS..=NEIGHBORS {
        let idx = (selected as isize + delta).rem_euclid(len) as usize;
        let row = &state.rows[idx];
        let dist = delta as f32 + (selected as f32 - visual);
        let scale = vstrip::scale_at_distance(dist);
        let fade = vstrip::opacity_at_distance(dist);
        items.push(strip_slot(
            row,
            idx,
            idx == selected,
            state.editing,
            scale,
            fade,
        ));
    }

    let strip = vstrip::vstrip(visual, selected, items)
        .width(Length::Fixed(SLOT_W))
        .height(Fill);

    // Status marks only — title/subtitle live beside each hero in the strip.
    let row = &state.rows[selected];
    let mut meta = column![].spacing(6);
    if state.editing {
        let mark = if row.in_catalog() {
            "In library — Cross to remove"
        } else {
            "Not in library — Cross to add"
        };
        meta = meta.push(text(mark).size(14.0).color(theme::ACCENT));
    } else if state
        .running_target
        .as_ref()
        .is_some_and(|t| t == &row.target)
    {
        meta = meta.push(text("Playing").size(15.0).color(theme::ACCENT));
    }

    row![
        container(strip).padding(Padding {
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
            left: EDGE_PAD,
        }),
        container(meta)
            .width(Fill)
            .height(Fill)
            .align_y(Alignment::Center)
            .padding(Padding {
                top: 0.0,
                right: EDGE_PAD + DOCK_PEEK_W,
                bottom: 0.0,
                left: 12.0,
            }),
    ]
    .spacing(0)
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

fn strip_slot(
    row: &StartRow,
    index: usize,
    selected: bool,
    editing: bool,
    scale: f32,
    fade: f32,
) -> Element<'_, StartMessage> {
    let muted = editing && !row.in_catalog();
    let hero_ready = row.hero_ready();
    let capsule = strip_capsule(row, selected, muted, hero_ready, fade);

    let title_size = if selected { 30.0 } else { 22.0 };
    let title_alpha = if muted { 0.55 * fade } else { fade };
    let title_color = if selected {
        theme::alpha(theme::INK, fade)
    } else {
        theme::alpha(theme::MUTED, 0.85 * title_alpha)
    };
    let title = text(if row.skeleton {
        "…".into()
    } else {
        row.title.clone()
    })
    .size(title_size)
    .color(title_color)
    .font(Font {
        weight: if selected {
            Weight::Bold
        } else {
            Weight::Normal
        },
        ..Font::DEFAULT
    });

    let mut titles = column![title].spacing(6);
    if let Some(sub) = row.subtitle.as_ref() {
        titles = titles.push(
            text(sub.clone())
                .size(if selected { 16.0 } else { 14.0 })
                .color(theme::alpha(theme::MUTED, 0.9 * fade)),
        );
    }

    let label: Element<'_, StartMessage> = container(titles)
        .width(Fill)
        .height(Fill)
        .align_y(Alignment::Center)
        .padding(Padding {
            top: 0.0,
            right: 8.0,
            bottom: 0.0,
            left: 14.0,
        })
        .into();

    // Explicit pixel size so vstrip scale actually enlarges the hero.
    // Inset smaller art so every capsule shares the selected art centerline.
    let max_art_w = CENTER_W * SELECTED_SCALE;
    let art_w = CENTER_W * scale;
    let art_h = CENTER_H * scale;
    let inset = ((max_art_w - art_w) * 0.5).max(0.0);
    let body = row![
        space().width(Length::Fixed(inset)),
        container(capsule)
            .width(Length::Fixed(art_w))
            .height(Length::Fixed(art_h)),
        label,
    ]
    .spacing(0)
    .width(Fill)
    .height(Fill)
    .align_y(Alignment::Center);

    button(body)
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

fn strip_capsule(
    row: &StartRow,
    selected: bool,
    muted: bool,
    hero_ready: bool,
    fade: f32,
) -> Element<'_, StartMessage> {
    let fade = fade.clamp(0.0, 1.0);
    let inner: Element<'_, StartMessage> = if row.skeleton {
        container(space())
            .width(Fill)
            .height(Fill)
            .style(theme::well)
            .into()
    } else {
        match row.hero_icon() {
            Some(icon) if hero_ready => iced::widget::image(icon.0)
                .width(Fill)
                .height(Fill)
                .content_fit(iced::ContentFit::Cover)
                .opacity(fade)
                .into(),
            Some(icon) => iced::widget::image(icon.0)
                .width(Fill)
                .height(Fill)
                .content_fit(iced::ContentFit::Cover)
                .opacity(0.55 * fade)
                .into(),
            None => container(
                text(row.title.chars().next().unwrap_or('?').to_string())
                    .size(if selected { 56.0 } else { 36.0 })
                    .color(if muted {
                        theme::alpha(theme::MUTED, 0.5 * fade)
                    } else {
                        theme::alpha(theme::MUTED, fade)
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
        theme::alpha(theme::ACCENT, 0.85 * fade)
    } else {
        theme::alpha(theme::LINE, 0.45 * fade)
    };

    container(inner)
        .width(Fill)
        .height(Fill)
        .style(move |_theme: &iced::Theme| iced::widget::container::Style {
            background: Some(iced::Background::Color(theme::CONTENT)),
            border: iced::Border {
                color: border,
                width: if selected { 2.0 } else { 1.0 },
                radius: 6.0.into(),
            },
            ..iced::widget::container::Style::default()
        })
        .into()
}

/// Fixed peek rings + clipped fixed-width details (reveal on expand, no text reflow).
fn controllers_dock_host<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    dock_progress: f32,
) -> Element<'a, StartMessage> {
    let visible_w = dock_panel_width(dock_progress, DOCK_PEEK_W, DOCK_MAX_W);
    let details_full = DOCK_MAX_W - DOCK_PEEK_W;
    let details_reveal = (visible_w - DOCK_PEEK_W).max(0.0);

    let mut rings = column![].spacing(12).width(Length::Fixed(DOCK_PEEK_W));
    let mut details = column![].spacing(12).width(Length::Fixed(details_full));

    if state.controllers.is_empty() {
        rings = rings.push(
            container(text("—").size(13.0).color(theme::DIM))
                .width(Length::Fixed(DOCK_PEEK_W))
                .height(Length::Fixed(DOCK_ROW_H))
                .center_x(Fill)
                .center_y(Fill),
        );
        details = details.push(
            container(text("No controllers").size(13.0).color(theme::DIM))
                .width(Length::Fixed(details_full))
                .height(Length::Fixed(DOCK_ROW_H))
                .center_y(Fill)
                .padding(Padding {
                    top: 0.0,
                    right: 16.0,
                    bottom: 0.0,
                    left: 4.0,
                }),
        );
    } else {
        for (i, row) in state.controllers.iter().enumerate() {
            let selected =
                i == state.controller_selected && matches!(state.slide, StartSlide::Controllers);
            rings = rings.push(dock_ring_cell(
                i,
                row,
                spectrum,
                selected,
                state.ring_flash_white(&row.serial),
            ));
            details = details.push(dock_details_cell(i, row, selected, state, details_full));
        }
    }

    let pad = Padding {
        top: 20.0,
        right: 0.0,
        bottom: 72.0,
        left: 0.0,
    };
    let rings_col = container(rings).padding(pad).height(Fill);
    let details_inner = container(details)
        .width(Length::Fixed(details_full))
        .padding(pad)
        .height(Fill);
    let details_clip = container(details_inner)
        .width(Length::Fixed(details_reveal))
        .height(Fill)
        .clip(true);

    container(row![rings_col, details_clip].width(Fill).height(Fill))
        .width(Length::Fixed(visible_w))
        .height(Fill)
        .style(theme::immersive_dock)
        .into()
}

fn dock_ring_cell<'a>(
    index: usize,
    row: &'a StartControllerRow,
    spectrum: &BatterySpectrum,
    selected: bool,
    flash_white: bool,
) -> Element<'a, StartMessage> {
    let ring_color = if flash_white {
        theme::from_rgb(crate::controller::dualsense::lightbar::IDENTIFY_FLASH)
    } else if row.connected {
        theme::from_rgb(spectrum.color_at_percent(row.percent))
    } else {
        theme::DIM
    };
    let ring = percent_ring::percent_ring(row.percent, ring_color, DOCK_RING_MIN, row.eta.clone());
    button(
        container(ring)
            .width(Length::Fixed(DOCK_PEEK_W))
            .height(Length::Fixed(DOCK_ROW_H))
            .center_x(Fill)
            .center_y(Fill),
    )
    .padding(0)
    .width(Length::Fixed(DOCK_PEEK_W))
    .height(Length::Fixed(DOCK_ROW_H))
    .on_press(StartMessage::SelectController(index))
    .style(theme::menu_row(selected))
    .into()
}

fn dock_details_cell<'a>(
    index: usize,
    row: &'a StartControllerRow,
    selected: bool,
    state: &State,
    details_w: f32,
) -> Element<'a, StartMessage> {
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
    let mut col = column![
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
    .width(Fill);

    if let Some(hints) = immersive_dock_row_hints(row, state, selected) {
        col = col.push(hints);
    }

    button(
        container(col)
            .width(Length::Fixed(details_w))
            .height(Length::Fixed(DOCK_ROW_H))
            .center_y(Fill)
            .clip(true)
            .padding(Padding {
                top: 0.0,
                right: 16.0,
                bottom: 0.0,
                left: 4.0,
            }),
    )
    .padding(0)
    .width(Length::Fixed(details_w))
    .height(Length::Fixed(DOCK_ROW_H))
    .on_press(StartMessage::SelectController(index))
    .style(theme::menu_row(selected))
    .into()
}
