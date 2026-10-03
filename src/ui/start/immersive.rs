//! Fullscreen console-style presentation of the start screen.

use crate::ui::color::BatterySpectrum;
use crate::ui::percent_ring;
use crate::ui::shader::AmbientProgram;
use crate::ui::start::mode::{TransitionPhase, dock_panel_width, dock_stage_dim, dock_stage_scale};
use crate::ui::start::view::{
    StartControllerRow, StartMessage, StartRow, StartSlide, State, footer_hint,
    immersive_dock_row_hints, manual_add_view, replace_confirm_view,
};
use crate::ui::start::vstrip::{self, CENTER_H, CENTER_W, NEIGHBORS, SELECTED_SCALE, SLOT_W};
use crate::ui::theme;
use iced::font::Weight;
use iced::widget::{Float, column, container, row, shader, space, stack, text};
use iced::{Alignment, Element, Fill, Font, Length, Padding};
use std::time::Instant;

const EDGE_PAD: f32 = 16.0;
/// Minimum gap from ring outer edge → peek column edge (also sizes peek width for max ring).
const DOCK_RING_INSET: f32 = 6.0;
const DOCK_RING_MIN: f32 = 52.0;
const DOCK_RING_MAX: f32 = 68.0;
/// Peek column = ring cell only (sized for the largest ring + inset).
const DOCK_PEEK_W: f32 = DOCK_RING_MAX + DOCK_RING_INSET * 2.0;
/// Wide enough for larger type + inline Identify/Power-off on one row.
const DOCK_MAX_W: f32 = 560.0;
/// Expanded row height (peek rows hug the ring).
const DOCK_ROW_H: f32 = 80.0;
/// Expanded list gap (peek gap matches island pad — see [`dock_peek_inset`]).
const DOCK_ROW_GAP: f32 = 8.0;
/// Reserved trailing width for dock action hints (avoids select flicker).
const DOCK_HINT_COL_W: f32 = 210.0;
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
///
/// Do **not** wrap chrome in [`Float`] here: iced paints Floats above later stack
/// siblings (including any veil), so footer/face-hint Floats would sit on top of the fade.
/// Compact enter/exit is dim-only; scale is omitted for that reason.
pub fn compact_transition_overlay(
    compact: Element<'_, StartMessage>,
    veil_amount: f32,
    _exiting: bool,
) -> Element<'_, StartMessage> {
    let veil = veil_amount.clamp(0.0, 1.0);
    stack![
        compact,
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
    always_immersive: bool,
    promote_gesture: &'a [crate::domain::gesture::GestureControl],
) -> Element<'a, StartMessage> {
    let dock_p = state.dock_progress(now);
    let veil = transition_top_veil(state, now);
    // Any Float under the veil paints above it — flatten stage scale + hint presses.
    let flat = veil > 0.001;
    // Chrome stays fully lit; one top veil handles enter/exit (no per-panel dims).
    let program = AmbientProgram::new(state.ambient_time, dock_p, 0.0, 1.0);
    let atmosphere = shader(program).width(Fill).height(Fill);

    let body: Element<'_, StartMessage> = if state.manual_add.is_some() {
        modal_card(manual_add_view(state))
    } else if state.replace_confirm.is_some() {
        modal_card(replace_confirm_view(state))
    } else {
        stage_with_dock(state, spectrum, now, dock_p, flat)
    };

    let footer_capsule = container(footer_hint(
        state,
        true,
        always_immersive,
        promote_gesture,
        flat,
    ))
    .padding(Padding {
        top: 6.0,
        right: 16.0,
        bottom: 6.0,
        left: 16.0,
    })
    .style(theme::immersive_footer_capsule);

    // Full-bleed stage + dock; footer hints overlay the bottom.
    let footer_overlay = column![
        space().height(Fill),
        container(footer_capsule)
            .width(Fill)
            .center_x(Fill)
            .padding(Padding {
                top: 0.0,
                right: EDGE_PAD,
                bottom: 10.0,
                left: EDGE_PAD,
            }),
    ]
    .width(Fill)
    .height(Fill);

    let chrome = stack![body, footer_overlay].width(Fill).height(Fill);
    let base = stack![atmosphere, chrome].width(Fill).height(Fill);

    if flat {
        stack![
            base,
            container(space())
                .width(Fill)
                .height(Fill)
                .style(theme::immersive_dim(veil)),
        ]
        .width(Fill)
        .height(Fill)
        .into()
    } else {
        base.into()
    }
}

/// Full-bleed blackout — same ease-out curve as compact ExitCompact / EnterCompact.
fn transition_top_veil(state: &State, now: Instant) -> f32 {
    let t = state.phase_progress(now).clamp(0.0, 1.0);
    match state.transition_phase.map(|(p, _)| p) {
        Some(TransitionPhase::EnterImmersive) => 1.0 - t,
        Some(TransitionPhase::ExitImmersive) => t,
        _ => 0.0,
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
    flat: bool,
) -> Element<'a, StartMessage> {
    let scale = dock_stage_scale(dock_p);
    let dim = dock_stage_dim(dock_p);

    // Backdrop lives in the games stage (scales with dock), not the full window.
    let mut stage_layers = stage_backdrop_layers(state, now);
    stage_layers.push(games_stage(state, now));
    let stage = stack(stage_layers).width(Fill).height(Fill);

    // Float paints above the enter/exit veil — skip scale while flattening.
    let scaled_stage: Element<'_, StartMessage> = if flat || (scale - 1.0).abs() < 0.001 {
        container(stage)
            .width(Fill)
            .height(Fill)
            .style(theme::immersive_stage)
            .into()
    } else {
        container(Float::new(stage).scale(scale))
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill)
            .style(theme::immersive_stage)
            .into()
    };

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
    // Top-right floating capsule — same inset as footer (EDGE_PAD / 10).
    let dock_overlay = row![
        space().width(Fill),
        column![dock, space().height(Fill),]
            .height(Fill)
            .padding(Padding {
                top: 10.0,
                right: EDGE_PAD,
                bottom: 0.0,
                left: 0.0,
            }),
    ]
    .width(Fill)
    .height(Fill);
    layers.push(dock_overlay.into());

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
    _index: usize,
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

    // Pad/keyboard navigate the strip — presentational slot (no mouse press/hover).
    container(body).width(Fill).height(Fill).into()
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

/// Unified rows at full dock width; [`width_reveal`] scissors peek→expanded (no column desync).
fn controllers_dock_host<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    dock_progress: f32,
) -> Element<'a, StartMessage> {
    let t = dock_progress.clamp(0.0, 1.0);
    let row_h = dock_row_h(dock_progress);
    // Reveal only the list — peek width is exactly the ring column.
    let visible_w = dock_panel_width(dock_progress, DOCK_PEEK_W, DOCK_MAX_W);
    let details_w = DOCK_MAX_W - DOCK_PEEK_W;

    // Island pad stays at the peek inset (concentric with rings) through expand.
    let pad = dock_peek_inset();
    let pad_y = pad;
    // Side pad only while expanded (peek stays flush so reveal width == ring column).
    let pad_x = pad * t;
    // Peek gap matches island pad; expanded uses the list gap.
    let row_gap = pad + (DOCK_ROW_GAP - pad) * t;
    // Peek: concentric with rings (radius = peek_w/2). Expand: pad + row radius so corners nest.
    let peek_radius = DOCK_PEEK_W * 0.5;
    let expanded_island = pad + theme::IMMERSIVE_DOCK_ROW_RADIUS;
    let island_radius = peek_radius + (expanded_island - peek_radius) * t;
    // Nested rounded-rect: inner = outer − pad (never the same as the island).
    let row_radius = (island_radius - pad).max(0.0);

    let mut list = column![].spacing(row_gap).width(Length::Fixed(DOCK_MAX_W));
    if state.controllers.is_empty() {
        list = list.push(dock_empty_row(details_w, row_h));
    } else {
        for (i, row) in state.controllers.iter().enumerate() {
            let selected =
                i == state.controller_selected && matches!(state.slide, StartSlide::Controllers);
            list = list.push(dock_controller_row(
                row,
                spectrum,
                selected,
                state.ring_flash_white(&row.serial),
                details_w,
                dock_progress,
                row_h,
                row_radius,
                state,
            ));
        }
    }

    let full = container(list)
        .padding(Padding {
            top: pad_y,
            right: 0.0,
            bottom: pad_y,
            left: 0.0,
        })
        .width(Length::Fixed(DOCK_MAX_W));

    // Shrink so the floating island sizes to its rows (Fill collapses to 0 off a Fill parent).
    let revealed =
        crate::ui::start::reveal::width_reveal(full, DOCK_MAX_W, visible_w).height(Length::Shrink);

    container(revealed)
        .padding(Padding {
            top: 0.0,
            right: pad_x,
            bottom: 0.0,
            left: pad_x,
        })
        .style(theme::immersive_island_radius(island_radius))
        .clip(true)
        .into()
}

fn dock_empty_row<'a>(details_w: f32, row_h: f32) -> Element<'a, StartMessage> {
    container(
        row![
            container(text("—").size(13.0).color(theme::DIM))
                .width(Length::Fixed(DOCK_PEEK_W))
                .height(Length::Fixed(row_h))
                .center_x(Fill)
                .center_y(Fill),
            container(text("No controllers").size(13.0).color(theme::DIM))
                .width(Length::Fixed(details_w))
                .height(Length::Fixed(row_h))
                .center_y(Fill)
                .padding(Padding {
                    top: 0.0,
                    right: 16.0,
                    bottom: 0.0,
                    left: 4.0,
                }),
        ]
        .width(Length::Fixed(DOCK_MAX_W))
        .height(Length::Fixed(row_h))
        .align_y(Alignment::Center),
    )
    .width(Length::Fixed(DOCK_MAX_W))
    .height(Length::Fixed(row_h))
    .clip(true)
    .into()
}

fn dock_ring_size(dock_progress: f32) -> f32 {
    let t = dock_progress.clamp(0.0, 1.0);
    DOCK_RING_MIN + (DOCK_RING_MAX - DOCK_RING_MIN) * t
}

/// Horizontal inset from peek column edge to the peek-sized ring (== island pad / row gap).
fn dock_peek_inset() -> f32 {
    (DOCK_PEEK_W - DOCK_RING_MIN).max(0.0) * 0.5
}

/// Peek rows hug the ring; expanded rows grow for title + meta.
fn dock_row_h(dock_progress: f32) -> f32 {
    let t = dock_progress.clamp(0.0, 1.0);
    let ring = dock_ring_size(dock_progress);
    ring + (DOCK_ROW_H - ring) * t
}

/// ETA stays hidden in peek; fades in through the latter part of expand.
fn dock_eta_reveal(dock_progress: f32) -> f32 {
    ((dock_progress.clamp(0.0, 1.0) - 0.2) / 0.8).clamp(0.0, 1.0)
}

#[allow(clippy::too_many_arguments)]
fn dock_controller_row<'a>(
    row: &'a StartControllerRow,
    spectrum: &BatterySpectrum,
    selected: bool,
    flash_white: bool,
    details_w: f32,
    dock_progress: f32,
    row_h: f32,
    row_radius: f32,
    state: &State,
) -> Element<'a, StartMessage> {
    let ring_color = if flash_white {
        theme::from_rgb(crate::controller::dualsense::lightbar::IDENTIFY_FLASH)
    } else if row.connected {
        theme::from_rgb(spectrum.color_at_percent(row.percent))
    } else {
        theme::DIM
    };
    let ring = percent_ring::percent_ring(
        row.percent,
        ring_color,
        dock_ring_size(dock_progress),
        row.eta.clone(),
        dock_eta_reveal(dock_progress),
    );
    let ring_cell = container(ring)
        .width(Length::Fixed(DOCK_PEEK_W))
        .height(Length::Fixed(row_h))
        .center_x(Fill)
        .center_y(Fill);

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
    let titles = column![
        text(&row.title).size(18.0).color(title_color).font(Font {
            weight: if selected {
                Weight::Bold
            } else {
                Weight::Normal
            },
            ..Font::DEFAULT
        }),
        text(format!("{} · {}", row.connection, row.state))
            .size(13.0)
            .color(meta),
    ]
    .spacing(3)
    .width(Fill);

    // Inline hints; always reserve trailing width so select does not shift titles.
    let hints: Element<'_, StartMessage> =
        if let Some(hints) = immersive_dock_row_hints(row, state, selected) {
            container(hints)
                .width(Length::Fixed(DOCK_HINT_COL_W))
                .height(Length::Fixed(row_h))
                .center_y(Fill)
                .align_x(Alignment::End)
                .clip(true)
                .into()
        } else {
            space()
                .width(Length::Fixed(DOCK_HINT_COL_W))
                .height(Length::Fixed(row_h))
                .into()
        };

    let pad_l = 4.0;
    let pad_r = 16.0;
    let gap = 8.0;
    let inner_w = (details_w - pad_l - pad_r).max(DOCK_HINT_COL_W + 80.0);
    let titles_w = (inner_w - DOCK_HINT_COL_W - gap).max(80.0);
    let details_cell = container(
        row![
            container(titles)
                .width(Length::Fixed(titles_w))
                .height(Length::Fixed(row_h))
                .center_y(Fill),
            hints,
        ]
        .spacing(gap)
        .width(Length::Fixed(inner_w))
        .height(Length::Fixed(row_h))
        .align_y(Alignment::Center),
    )
    .width(Length::Fixed(details_w))
    .height(Length::Fixed(row_h))
    .padding(Padding {
        top: 0.0,
        right: pad_r,
        bottom: 0.0,
        left: pad_l,
    });

    container(
        row![ring_cell, details_cell]
            .width(Length::Fixed(DOCK_MAX_W))
            .height(Length::Fixed(row_h))
            .align_y(Alignment::Center),
    )
    .width(Length::Fixed(DOCK_MAX_W))
    .height(Length::Fixed(row_h))
    .clip(true)
    .style(theme::immersive_dock_row_surface(selected, row_radius))
    .into()
}
