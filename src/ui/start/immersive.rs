//! Fullscreen console-style presentation of the start screen.

use crate::persist::prefs::ImmersiveLayout;
use crate::ui::color::BatterySpectrum;
use crate::ui::percent_ring;
use crate::ui::shader::{AmbientProgram, VignetteProgram};
use crate::ui::start::mode::{TransitionPhase, dock_panel_width, dock_stage_dim, dock_stage_scale};
use crate::ui::start::translate::backdrop_art;
use crate::ui::start::view::{
    StartControllerRow, StartMessage, StartRow, StartSlide, StartSubtitle, State,
    empty_games_browse_prompt, footer_hint, game_subtitle_block, immersive_dock_row_hints,
    immersive_game_hints, immersive_game_membership_label, manual_add_view, replace_confirm_view,
    scan_status_chip, scan_status_stack, update_required_badge,
};
use crate::ui::start::vstrip::{self, NEIGHBORS, StripMetrics, StripOrientation};
use crate::ui::theme;
use iced::font::Weight;
use iced::widget::{Float, column, container, row, shader, space, stack, text};
use iced::{Alignment, Element, Fill, Font, Length, Padding, Vector};
use std::time::Instant;

const EDGE_PAD: f32 = 16.0;
/// ~1″ inset for the games strip from the left window edge (96 logical px / inch).
const STRIP_AFTER_BAR: f32 = 12.0;
/// Gap between the horizontal position rail and the footer hints island below it.
const BAR_ABOVE_FOOTER: f32 = 64.0;
/// Top spacing above the horizontal strip: minimum clock-capsule clearance.
///
/// The art-row lead ([`vstrip::horizontal_block_lead`]) stacks the centering
/// drop on top of this pad, so the visual top gap runs deeper while the rail
/// keeps its minimum gap below the titles.
const STRIP_TOP_PAD: f32 = 44.0;
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
const MODAL_W: f32 = crate::ui::start::view::WIDTH;

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

#[allow(clippy::too_many_arguments)]
pub fn view<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    now: Instant,
    always_immersive: bool,
    clock_enabled: bool,
    promote_gesture: &'a [crate::domain::gesture::GestureControl],
    stage_h: f32,
    stage_w: f32,
    settings_snapshot: &crate::ui::start::settings::StartSettingsSnapshot,
    immersive_layout: ImmersiveLayout,
) -> Element<'a, StartMessage> {
    let dock_p = state.dock_progress(now);
    let veil = transition_top_veil(state, now);
    let idle_veil = state.idle_dim_amount(now);
    let settings_p = state.settings.progress(now);
    // Any Float under a veil paints above it — flatten stage scale + hint presses.
    let flat = veil > 0.001 || idle_veil > 0.001 || settings_p > 0.001;
    // Chrome stays fully lit; one top veil handles enter/exit (no per-panel dims).
    let program = AmbientProgram::new(state.ambient_time, dock_p, 0.0, 1.0);
    let atmosphere = shader(program).width(Fill).height(Fill);

    let body: Element<'_, StartMessage> = if state.manual_add.is_some() {
        modal_card(manual_add_view(state))
    } else if state.replace_confirm.is_some() {
        modal_card(replace_confirm_view(state))
    } else {
        stage_with_dock(
            state,
            spectrum,
            now,
            dock_p,
            flat,
            stage_h,
            stage_w,
            immersive_layout,
        )
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

    let mut chrome_layers: Vec<Element<'_, StartMessage>> = vec![body, footer_overlay.into()];
    if clock_enabled {
        // Center-top clock widget (system locale time). Digits carry no descenders,
        // so the font line box holds empty descender space below the baseline and
        // the glyphs sit ~1px high: top padding runs 1px deeper to compensate
        // (optical centering; total capsule height is unchanged).
        let clock_capsule = container(
            text(crate::ui::start::clock::clock_text())
                .size(18.0)
                .color(theme::alpha(theme::INK, 0.72)),
        )
        .padding(Padding {
            top: 7.0,
            right: 16.0,
            bottom: 5.0,
            left: 16.0,
        })
        .style(theme::immersive_footer_capsule);
        let clock_overlay = column![
            container(clock_capsule)
                .width(Fill)
                .center_x(Fill)
                .padding(Padding {
                    top: 10.0,
                    right: EDGE_PAD,
                    bottom: 0.0,
                    left: EDGE_PAD,
                }),
            space().height(Fill),
        ]
        .width(Fill)
        .height(Fill);
        chrome_layers.push(clock_overlay.into());
    }
    let chrome_status = state.chrome_load_status(now);
    if !chrome_status.is_empty() {
        // Clip flush to the window bottom-right; height grows with the status rail.
        let chip_h = 28.0;
        let bottom_inset = 16.0;
        let chips: Vec<_> = chrome_status
            .iter()
            .copied()
            .map(|visual| {
                let chip = scan_status_chip(chip_h, 18.0, now, visual);
                (visual, chip)
            })
            .collect();
        let scan_overlay: Element<'_, StartMessage> = column![
            space().height(Fill),
            container(scan_status_stack(chips, chip_h, bottom_inset))
                .width(Fill)
                .padding(Padding {
                    top: 0.0,
                    right: EDGE_PAD,
                    bottom: 0.0,
                    left: 0.0,
                }),
        ]
        .width(Fill)
        .height(Fill)
        .into();
        chrome_layers.push(scan_overlay);
    }

    let chrome = stack(chrome_layers).width(Fill).height(Fill);
    let base = stack![atmosphere, chrome].width(Fill).height(Fill);

    let mut layers: Vec<Element<'_, StartMessage>> = vec![base.into()];
    // Settings above chrome; idle/sleep + ceremony veils paint above the drawer.
    if state.settings.visible(now) {
        layers.push(crate::ui::start::settings::immersive_drawer(
            state,
            settings_snapshot,
            settings_p,
        ));
    }
    // Idle/sleep wash sits above settings; ceremony veil stays on top.
    if idle_veil > 0.001 {
        layers.push(
            container(space())
                .width(Fill)
                .height(Fill)
                .style(theme::immersive_dim(idle_veil))
                .into(),
        );
    }
    if veil > 0.001 {
        layers.push(
            container(space())
                .width(Fill)
                .height(Fill)
                .style(theme::immersive_dim(veil))
                .into(),
        );
    }
    stack(layers).width(Fill).height(Fill).into()
}

/// Full-bleed blackout — enter uses capped ease-in-out; exit uses ease-out like compact.
fn transition_top_veil(state: &State, now: Instant) -> f32 {
    match state.transition_phase.map(|(p, _)| p) {
        Some(TransitionPhase::EnterImmersive) => state.enter_veil_amount(),
        Some(TransitionPhase::ExitImmersive) => state.phase_progress(now).clamp(0.0, 1.0),
        _ => 0.0,
    }
}

fn stage_backdrop_layers<'a>(state: &'a State, now: Instant) -> Vec<Element<'a, StartMessage>> {
    let selected = state.game_selected.min(state.rows.len().saturating_sub(1));
    let mut layers: Vec<Element<'_, StartMessage>> = Vec::new();
    // Dim tracks splash opacity so ambient→art does not pop a full wash.
    let mut dim_amount = 0.0_f32;

    if let Some((key, opacity, scale, ox, oy)) = state.backdrop_outgoing_visual(now)
        && let Some(art) = state.backdrop_icon_for_key(key)
    {
        // Parked pose stays frozen (scale + pan) — resetting pan looked like a navigate glitch.
        layers.push(backdrop_layer(art, opacity, scale, ox, oy));
        dim_amount = dim_amount.max(opacity);
    }

    if let Some(row) = state.rows.get(selected)
        && let Some(art) = row.backdrop_icon()
        && let Some((opacity, scale, ox, oy)) = state.backdrop_incoming_visual(now)
        && opacity > 0.01
    {
        layers.push(backdrop_layer(art, opacity, scale, ox, oy));
        dim_amount = dim_amount.max(opacity);
    }

    if dim_amount > 0.01 {
        layers.push(
            container(space())
                .width(Fill)
                .height(Fill)
                .style(theme::immersive_dim(0.58 * dim_amount))
                .into(),
        );
    }

    // Soft rectangular edge vignette over atmosphere/splash only — games strip,
    // dock, footer, settings, and status chips paint above this stack.
    layers.push(
        shader(VignetteProgram::new(0.55))
            .width(Fill)
            .height(Fill)
            .into(),
    );

    layers
}

fn backdrop_layer(
    art: crate::ui::start::view::StartIcon,
    opacity: f32,
    scale: f32,
    ox_norm: f32,
    oy_norm: f32,
) -> Element<'static, StartMessage> {
    // One-shot Cover draw: zoom then pan inside headroom, clipped to the stage.
    // (Translating an iced Image after its own bounds-clip reveals black edges.)
    backdrop_art(art.0, opacity, scale, ox_norm, oy_norm).into()
}

#[allow(clippy::too_many_arguments)]
fn stage_with_dock<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    now: Instant,
    dock_p: f32,
    flat: bool,
    stage_h: f32,
    stage_w: f32,
    immersive_layout: ImmersiveLayout,
) -> Element<'a, StartMessage> {
    let scale = dock_stage_scale(dock_p);
    let dim = dock_stage_dim(dock_p);

    // Backdrop lives in the games stage (scales with dock), not the full window.
    // A layout change slides the old orientation out along its scroll axis
    // while fading it; the orientation swaps at the midpoint, then the new one
    // slides in from the opposite side while fading back in.
    let (transition_layout, transition_opacity, transition_dx, transition_dy) =
        state.layout_transition_visual(immersive_layout, now);
    let list_op = (state.games_list_opacity(now) * transition_opacity).clamp(0.0, 1.0);
    let mut stage_layers = if list_op > 0.01 {
        stage_backdrop_layers(state, now)
    } else {
        Vec::new()
    };
    if list_op > 0.01 {
        let strip: Element<'_, StartMessage> = games_stage(
            state,
            now,
            stage_h,
            stage_w,
            list_op,
            transition_layout,
            flat,
        );
        // Float paints above veils — skip the slide while flattening (same as
        // the dock scale); sub-pixel offsets skip the extra layer. No scale
        // punch: the directional travel carries the motion.
        let strip: Element<'_, StartMessage> = if !flat
            && (transition_dx.abs() >= 0.5 || transition_dy.abs() >= 0.5)
        {
            container(
                Float::new(strip)
                    .translate(move |_bounds, _viewport| Vector::new(transition_dx, transition_dy)),
            )
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill)
            .style(theme::immersive_stage)
            .into()
        } else {
            strip
        };
        stage_layers.push(strip);
    }
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

fn games_stage(
    state: &State,
    now: Instant,
    stage_h: f32,
    stage_w: f32,
    list_opacity: f32,
    immersive_layout: ImmersiveLayout,
    flat: bool,
) -> Element<'_, StartMessage> {
    if state.rows.is_empty() {
        return empty_games(state);
    }

    let list_opacity = list_opacity.clamp(0.0, 1.0);
    let orientation = match immersive_layout {
        ImmersiveLayout::Vertical => StripOrientation::Vertical,
        ImmersiveLayout::Horizontal => StripOrientation::Horizontal,
    };
    let metrics = match immersive_layout {
        ImmersiveLayout::Vertical => vstrip::metrics_for_height(stage_h),
        ImmersiveLayout::Horizontal => vstrip::metrics_for_width(stage_w, stage_h),
    };
    // Horizontal shows as many neighbors as fit the stage width (up to MAX);
    // vertical keeps the fixed 5-item strip.
    let neighbors = match immersive_layout {
        ImmersiveLayout::Vertical => NEIGHBORS,
        ImmersiveLayout::Horizontal => vstrip::neighbors_for_width(stage_w, metrics.center_w),
    };
    let selected = state.game_selected.min(state.rows.len().saturating_sub(1));
    let visual = state.strip_scroll(now);
    let len = state.rows.len();

    let mut items = Vec::with_capacity((neighbors * 2 + 1).max(0) as usize);
    for delta in -neighbors..=neighbors {
        let Some((idx, dist)) = vstrip::visual_slot(visual, delta, len) else {
            continue;
        };
        let scale = vstrip::scale_at_distance(dist);
        let fade = vstrip::opacity_at_distance(dist) * list_opacity;
        let row = &state.rows[idx];
        let selected_slot = idx == selected;
        // Integer offset from the selection: horizontal outer peeks (|offset|
        // >= 2) show art only so small titles can never overlap. Stable
        // across the scroll animation (unlike the fractional `dist`).
        let sel_offset = idx as isize - selected as isize;
        let overlay = if selected_slot && list_opacity > 0.85 {
            Some(hero_hint_overlay(row, state))
        } else {
            None
        };
        items.push((
            dist,
            strip_slot(
                row,
                selected_slot,
                state.editing,
                scale,
                fade,
                &metrics,
                overlay,
                orientation,
                sel_offset,
            ),
        ));
    }

    let strip = vstrip::vstrip(metrics, items).orientation(orientation);
    let strip = match immersive_layout {
        ImmersiveLayout::Vertical => strip.width(Length::Fixed(metrics.slot_w)).height(Fill),
        ImmersiveLayout::Horizontal => strip.width(Fill).height(Length::Fixed(metrics.slot_h)),
    };

    let mode = state.games_section_mode();

    let bar = crate::ui::start::position::view(crate::ui::start::position::BarSpec {
        labels: crate::ui::start::position::labels(mode),
        visual: state.position_section_visual(now),
        reveal: list_opacity,
        show_opacity: state.position_bar_opacity(now),
        slide: state.position_bar_slide(now),
        motion: state.position_bar_motion(),
        orientation,
    });

    match immersive_layout {
        ImmersiveLayout::Vertical => {
            // Titles sit beside heroes; Launch/Close/edit cues overlay the selected capsule.
            row![
                container(bar).padding(Padding {
                    top: 0.0,
                    right: 0.0,
                    bottom: 0.0,
                    left: EDGE_PAD,
                }),
                container(strip).padding(Padding {
                    top: 0.0,
                    right: 0.0,
                    bottom: 0.0,
                    left: STRIP_AFTER_BAR,
                }),
                space().width(Fill),
            ]
            .spacing(0)
            .width(Fill)
            .height(Fill)
            .into()
        }
        ImmersiveLayout::Horizontal => {
            // Art-row-first anchoring: the strip top sits at `lead` so the ART
            // ROW (not the strip+title block) centers between the screen top
            // and the rail. The selected title follows directly beneath the
            // art as part of the same top-anchored block, and the rail keeps
            // its minimum gap below the titles — still between titles and the
            // hints island (bottom chrome overlay).
            let layer_w = (stage_w - EDGE_PAD * 2.0).max(200.0);
            let title_layer =
                horizontal_title_layer(state, now, selected, list_opacity, layer_w, flat);
            let combined = column![strip, title_layer,]
                .spacing(vstrip::H_STRIP_TO_TITLE_GAP)
                .width(Fill)
                .align_x(Alignment::Center);
            let block_h =
                metrics.slot_h + vstrip::H_STRIP_TO_TITLE_GAP + vstrip::H_SELECTED_TITLE_RESERVE;
            let region_h =
                (stage_h - (crate::ui::start::position::WIDTH + BAR_ABOVE_FOOTER)).max(1.0);
            let lead = vstrip::horizontal_block_lead(
                region_h,
                block_h,
                STRIP_TOP_PAD,
                vstrip::H_TITLE_TO_BAR_MIN,
            );
            column![
                container(
                    column![
                        space().height(Length::Fixed(lead)),
                        combined,
                        space().height(Fill),
                    ]
                    .spacing(0)
                    .width(Fill)
                    .height(Fill),
                )
                .width(Fill)
                .height(Fill)
                .padding(Padding {
                    top: 0.0,
                    right: EDGE_PAD,
                    bottom: 0.0,
                    left: EDGE_PAD,
                }),
                container(bar).padding(Padding {
                    top: 0.0,
                    right: EDGE_PAD,
                    bottom: BAR_ABOVE_FOOTER,
                    left: EDGE_PAD,
                }),
            ]
            .spacing(0)
            .width(Fill)
            .height(Fill)
            .into()
        }
    }
}

/// Membership / Playing + face cues stacked on the selected hero art.
fn hero_hint_overlay<'a>(row: &'a StartRow, state: &'a State) -> Element<'a, StartMessage> {
    let running = state
        .running_target
        .as_ref()
        .is_some_and(|t| t == &row.target);
    let closing = state
        .closing_target
        .as_ref()
        .is_some_and(|t| t == &row.target);
    let mut col = column![].spacing(10).align_x(Alignment::Center);
    if state.editing {
        col = col.push(immersive_game_membership_label(row));
    } else if closing {
        col = col.push(text("Closing").size(15.0).color(theme::MUTED));
    } else if running {
        col = col.push(text("Playing").size(15.0).color(theme::ACCENT));
    }
    col = col.push(immersive_game_hints(row, state));
    col.into()
}

fn empty_games(state: &State) -> Element<'_, StartMessage> {
    let empty: Element<'_, StartMessage> = if state.editing {
        text("No installed Steam games — add a manual shortcut.")
            .size(22.0)
            .color(theme::MUTED)
            .into()
    } else {
        empty_games_browse_prompt(state, 22.0)
    };
    container(empty)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .into()
}

#[allow(clippy::too_many_arguments)]
fn strip_slot<'a>(
    row: &'a StartRow,
    selected: bool,
    editing: bool,
    scale: f32,
    fade: f32,
    metrics: &StripMetrics,
    overlay: Option<Element<'a, StartMessage>>,
    orientation: StripOrientation,
    sel_offset: isize,
) -> Element<'a, StartMessage> {
    let muted = editing && !row.in_catalog();
    let disabled = row.disabled && !editing;
    let dim = if disabled { 0.35 } else { 1.0 };
    let hero_ready = row.hero_ready();
    let capsule = strip_capsule(
        row,
        selected,
        muted || disabled,
        hero_ready,
        fade * dim,
        overlay,
    );

    // Explicit pixel size so vstrip scale actually enlarges the hero.
    // Inset smaller art so every capsule shares the selected art centerline.
    let max_art_w = metrics.center_w * metrics.selected_scale;
    let art_w = metrics.center_w * scale;
    let art_h = metrics.center_h * scale;

    if orientation == StripOrientation::Horizontal {
        // Selected title lives in the full-width layer below the strip;
        // outer peeks (|offset| >= 2) show art only. Only immediate neighbors
        // keep a small in-slot title so adjacent slots can never overlap —
        // and only where the strip budgeted room for it (short stages go
        // art-only rather than clipping, see `metrics_for_width`).
        let show_text = !selected && sel_offset.abs() < 2 && metrics.h_text_budget > 0.0;
        return strip_slot_stacked(
            row, muted, fade, dim, capsule, max_art_w, art_w, art_h, show_text,
        );
    }

    let title_size = if selected { 39.0 } else { 22.0 };
    let title_alpha = if muted { 0.55 * fade } else { fade * dim };
    // Selected titles are large; pull back from pure white for less glare.
    let title_color = if selected {
        theme::alpha(theme::mix(theme::INK, theme::MUTED, 0.28), title_alpha)
    } else {
        theme::alpha(theme::MUTED, 0.85 * title_alpha)
    };
    let title_string = if row.skeleton {
        "…".into()
    } else {
        row.title.clone()
    };
    let title_weight = if selected {
        Weight::Bold
    } else {
        Weight::Normal
    };

    let title = text(title_string)
        .size(title_size)
        .color(title_color)
        .font(Font {
            weight: title_weight,
            ..Font::DEFAULT
        });

    let sub_alpha = if muted { 0.55 * fade } else { 0.9 * fade * dim };
    let mut titles = column![title].spacing(6);
    // Stacked `Played:` / `Last played:` + optional Update pill.
    // Disabled rows show "Not installed" instead of meta.
    titles = titles.push(status_block(row, selected, muted, fade, sub_alpha, None));

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

/// Meta under a game title: "Not installed", or `Played:` / `Last played:`
/// plus the Update pill. `fixed_w` centers the block in a fixed width: the art
/// width for horizontal neighbor slots (so adjacent slots cannot overlap), the
/// stage width for the full-width selected-title layer; `None` keeps the
/// vertical side-column behavior.
fn status_block<'a>(
    row: &'a StartRow,
    selected: bool,
    muted: bool,
    fade: f32,
    sub_alpha: f32,
    fixed_w: Option<f32>,
) -> Element<'a, StartMessage> {
    let width = match fixed_w {
        Some(w) => Length::Fixed(w),
        None => Fill,
    };
    let align = match fixed_w {
        Some(_) => Alignment::Center,
        None => Alignment::Start,
    };
    if row.disabled {
        let meta_size = if selected { 16.0 } else { 14.0 };
        let not_installed = text("Not installed")
            .size(meta_size)
            .color(theme::alpha(theme::MUTED, 0.7 * fade));
        if fixed_w.is_some() {
            let label: Element<'_, StartMessage> =
                not_installed.width(width).align_x(Alignment::Center).into();
            return column![label].spacing(6).width(width).align_x(align).into();
        }
        return column![not_installed]
            .spacing(6)
            .width(width)
            .align_x(align)
            .into();
    }
    let show_meta = row
        .subtitle
        .as_ref()
        .is_some_and(|sub| !(row.update_required && sub.is_steam_fallback()));
    if !(show_meta || row.update_required) {
        return space().width(width).height(Length::Fixed(0.0)).into();
    }
    let meta_size = if selected { 16.0 } else { 14.0 };
    let mut status = column![].spacing(6).width(width).align_x(align);
    if show_meta && let Some(sub) = row.subtitle.as_ref() {
        let value_color = theme::alpha(theme::MUTED, sub_alpha);
        let label_color = theme::alpha(theme::MUTED, sub_alpha * 0.8);
        // Horizontal slots center secondary text under the title; the
        // vertical side column keeps the shared left-aligned block.
        let sub_block = if fixed_w.is_some() {
            centered_game_subtitle_block(sub, value_color, label_color, meta_size)
        } else {
            game_subtitle_block(sub, value_color, label_color, meta_size)
        };
        status = status.push(sub_block);
    }
    if row.update_required {
        let badge: Element<'_, StartMessage> = update_required_badge(selected, muted, fade, true);
        if fixed_w.is_some() {
            status = status.push(container(badge).width(Fill).center_x(Fill));
        } else {
            status = status.push(badge);
        }
    }
    status.into()
}

/// Centered variant of [`game_subtitle_block`] for the horizontal strip:
/// `Played:` / `Last played:` lines sit centered under the title instead of
/// left-aligned. Only used on the `fixed_w` (centered) path; vertical
/// side-column callers keep the shared left-aligned block untouched.
fn centered_game_subtitle_block<'a>(
    subtitle: &'a StartSubtitle,
    value_color: iced::Color,
    label_color: iced::Color,
    value_size: f32,
) -> Element<'a, StartMessage> {
    match subtitle {
        StartSubtitle::Plain(plain) => text(plain.as_str())
            .size(value_size)
            .color(value_color)
            .wrapping(iced::widget::text::Wrapping::Word)
            .width(Fill)
            .align_x(Alignment::Center)
            .into(),
        StartSubtitle::Labeled(lines) => {
            let mut col = column![].spacing(2).width(Fill).align_x(Alignment::Center);
            for line in lines {
                col = col.push(
                    row![
                        text(format!("{}:", line.label))
                            .size(value_size)
                            .color(label_color),
                        text(line.value.as_str())
                            .size(value_size)
                            .color(value_color)
                            .wrapping(iced::widget::text::Wrapping::Word),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                );
            }
            col.into()
        }
    }
}

/// Horizontal item: art on top with a small centered title + meta below it.
/// Only immediate neighbors show text; the selected title lives in a separate
/// full-width layer and outer peeks show art only. Neighbor titles stay bound
/// to the art width, which is always narrower than the slot spacing (edge gap
/// `STRIDE_GAP` to spare), so adjacent slots can never overlap. Single-line,
/// clipped; Launch/Close/edit cues still overlay the capsule via `strip_capsule`.
#[allow(clippy::too_many_arguments)]
fn strip_slot_stacked<'a>(
    row: &'a StartRow,
    muted: bool,
    fade: f32,
    dim: f32,
    capsule: Element<'a, StartMessage>,
    max_art_w: f32,
    art_w: f32,
    art_h: f32,
    show_text: bool,
) -> Element<'a, StartMessage> {
    let art_w = art_w.max(1.0);
    let label: Element<'_, StartMessage> = if show_text {
        let title_string = if row.skeleton {
            "…".into()
        } else {
            row.title.clone()
        };
        let title_alpha = if muted { 0.55 * fade } else { fade * dim };
        let title = text(title_string)
            .size(22.0)
            .color(theme::alpha(theme::MUTED, 0.85 * title_alpha))
            .font(Font {
                weight: Weight::Normal,
                ..Font::DEFAULT
            })
            .width(Length::Fixed(art_w))
            .align_x(Alignment::Center)
            .wrapping(iced::widget::text::Wrapping::None);
        let title: Element<'_, StartMessage> = container(title)
            .width(Length::Fixed(art_w))
            .clip(true)
            .into();

        let sub_alpha = if muted { 0.55 * fade } else { 0.9 * fade * dim };
        let text_block = column![
            title,
            status_block(row, false, muted, fade, sub_alpha, Some(art_w))
        ]
        .spacing(6)
        .width(Length::Fixed(art_w))
        .align_x(Alignment::Center);
        // Center the (possibly narrower) text under the art within the fixed slot.
        container(text_block)
            .width(Length::Fixed(max_art_w))
            .center_x(Length::Fixed(max_art_w))
            .into()
    } else {
        // Art only: selected text lives in the full-width layer, outer peeks
        // stay quiet. Zero-height (no inter-block spacing) so the art bottom
        // meets the strip bottom and the title layer below sits one text gap
        // away — matching neighbor in-slot titles. Geometry matches
        // `slot_child_size_h` at `|d| = 0` (art only, zero text weight).
        container(space())
            .width(Length::Fixed(max_art_w))
            .height(Length::Shrink)
            .into()
    };

    let art_cell = container(capsule)
        .width(Length::Fixed(art_w))
        .height(Length::Fixed(art_h));
    // Fixed-width cell so every slot shares the same horizontal centerline.
    let art = container(art_cell)
        .width(Length::Fixed(max_art_w))
        .center_x(Length::Fixed(max_art_w));

    // Art-only slots carry no text gap (their child box is art-only too);
    // text slots keep the art→text gap so neighbor titles sit one gap below
    // the art like the selected title does below the strip.
    let body_gap = if show_text { vstrip::H_TEXT_GAP } else { 0.0 };
    let body = column![art, label]
        .spacing(body_gap)
        .width(Length::Fixed(max_art_w))
        .height(Fill)
        .align_x(Alignment::Center);

    // Pad/keyboard navigate the strip — presentational slot (no mouse press/hover).
    container(body).width(Fill).height(Fill).into()
}

/// Full-width selected title below the horizontal art row, with a selection
/// crossfade: the outgoing title fades out while the incoming fades in (plus
/// a slight drift in scroll direction) over the strip clock.
///
/// Both opacities scale `fade` — which already carries the layout-transition
/// multiplier — so transition + crossfade compose instead of conflicting. Drift
/// skips while flattening (Floats paint above veils).
fn horizontal_title_layer(
    state: &State,
    now: Instant,
    selected: usize,
    fade: f32,
    layer_w: f32,
    flat: bool,
) -> Element<'_, StartMessage> {
    let Some(row) = state.rows.get(selected) else {
        return container(space())
            .width(Fill)
            .height(Length::Fixed(vstrip::H_SELECTED_TITLE_RESERVE))
            .into();
    };
    let Some((prev_idx, old_op, new_op, old_dx, new_dx)) = state.title_crossfade_visual(now) else {
        return horizontal_selected_title(row, state.editing, fade, layer_w);
    };
    let Some(prev_row) = state.rows.get(prev_idx) else {
        return horizontal_selected_title(row, state.editing, fade, layer_w);
    };
    let old_layer = title_drift(
        horizontal_selected_title(prev_row, state.editing, fade * old_op, layer_w),
        old_dx,
        flat,
    );
    let new_layer = title_drift(
        horizontal_selected_title(row, state.editing, fade * new_op, layer_w),
        new_dx,
        flat,
    );
    // Same fixed reserve either way, so the strip never shifts mid-crossfade.
    stack![old_layer, new_layer].width(Fill).into()
}

/// Horizontal nudge without layout impact; skipped while flattening or when
/// the offset is negligible.
fn title_drift<'a>(
    layer: Element<'a, StartMessage>,
    dx: f32,
    flat: bool,
) -> Element<'a, StartMessage> {
    if flat || dx.abs() < 0.5 {
        return layer;
    }
    Float::new(layer)
        .translate(move |_bounds, _viewport| Vector::new(dx, 0.0))
        .into()
}

/// Full-width selected title below the horizontal art row.
///
/// Stage-width centered, so even the longest 39px title fits without clipping
/// or neighbor collisions. Selected meta (Played / Last played + update badge)
/// joins the title here, centered on screen rather than the art width.
fn horizontal_selected_title<'a>(
    row: &'a StartRow,
    editing: bool,
    fade: f32,
    layer_w: f32,
) -> Element<'a, StartMessage> {
    let layer_w = layer_w.max(1.0);
    let muted = editing && !row.in_catalog();
    let disabled = row.disabled && !editing;
    let dim = if disabled { 0.35 } else { 1.0 };
    let fade = fade.clamp(0.0, 1.0);
    let title_string = if row.skeleton {
        "…".into()
    } else {
        row.title.clone()
    };
    let title_alpha = if muted { 0.55 * fade } else { fade * dim };
    let title = text(title_string)
        .size(39.0)
        .color(theme::alpha(
            theme::mix(theme::INK, theme::MUTED, 0.28),
            title_alpha,
        ))
        .font(Font {
            weight: Weight::Bold,
            ..Font::DEFAULT
        })
        .width(Length::Fixed(layer_w))
        .align_x(Alignment::Center)
        .wrapping(iced::widget::text::Wrapping::None);
    let title: Element<'_, StartMessage> = container(title)
        .width(Length::Fixed(layer_w))
        .clip(true)
        .into();

    let sub_alpha = if muted { 0.55 * fade } else { 0.9 * fade * dim };
    let meta = status_block(row, true, muted, fade, sub_alpha, Some(layer_w));
    let block = column![title, meta]
        .spacing(6)
        .width(Length::Fixed(layer_w))
        .align_x(Alignment::Center);
    // Fixed height keeps the strip + title block stable while navigating
    // (different meta presence must not move the title): content top-aligns
    // (no centering slack even when meta/badge are absent) so the title text
    // top sits one text gap below the art bottoms.
    container(block)
        .width(Fill)
        .height(Length::Fixed(vstrip::H_SELECTED_TITLE_RESERVE))
        .align_y(Alignment::Start)
        .center_x(Fill)
        .into()
}

fn strip_capsule<'a>(
    row: &'a StartRow,
    selected: bool,
    muted: bool,
    hero_ready: bool,
    fade: f32,
    overlay: Option<Element<'a, StartMessage>>,
) -> Element<'a, StartMessage> {
    // Subtle radius: image uses border_radius; card border matches for the ring.
    const HERO_RADIUS: f32 = 3.0;
    let fade = fade.clamp(0.0, 1.0);
    let art_fade = if muted { 0.55 * fade } else { fade };
    let art: Element<'_, StartMessage> = if row.skeleton {
        container(space()).width(Fill).height(Fill).into()
    } else {
        match row.hero_icon() {
            Some(icon) if hero_ready => iced::widget::image(icon.0)
                .width(Fill)
                .height(Fill)
                .content_fit(iced::ContentFit::Cover)
                .opacity(art_fade)
                .border_radius(HERO_RADIUS)
                .into(),
            Some(icon) => iced::widget::image(icon.0)
                .width(Fill)
                .height(Fill)
                .content_fit(iced::ContentFit::Cover)
                .opacity(0.55 * art_fade)
                .border_radius(HERO_RADIUS)
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
    let border_w = if selected { 2.0 } else { 1.0 };
    // No fill: the slot fade rides on the art (see `art_fade`), so an
    // opaque card behind it would read as a grey plate fading in instead
    // of the art dissolving into the backdrop. The border already fades.
    let frame = move |_theme: &iced::Theme| iced::widget::container::Style {
        background: None,
        border: iced::Border {
            color: border,
            width: border_w,
            radius: HERO_RADIUS.into(),
        },
        ..iced::widget::container::Style::default()
    };

    if let Some(hints) = overlay {
        let footer = container(hints)
            .width(Fill)
            .center_x(Fill)
            .padding(Padding {
                top: 10.0,
                right: 12.0,
                bottom: 10.0,
                left: 12.0,
            })
            .style(|_theme: &iced::Theme| iced::widget::container::Style {
                background: Some(iced::Background::Color(theme::alpha(
                    iced::Color::BLACK,
                    0.82,
                ))),
                border: iced::Border {
                    radius: iced::border::Radius {
                        top_left: 0.0,
                        top_right: 0.0,
                        bottom_right: HERO_RADIUS,
                        bottom_left: HERO_RADIUS,
                    },
                    ..Default::default()
                },
                ..iced::widget::container::Style::default()
            });
        let cues = column![space().height(Fill), footer,]
            .width(Fill)
            .height(Fill);
        if muted {
            // Edit mode, not in library: keep the full selection ring visible.
            let body = stack![container(art).width(Fill).height(Fill), cues,]
                .width(Fill)
                .height(Fill);
            container(body).width(Fill).height(Fill).style(frame).into()
        } else {
            // Play / in-library: footer covers the bottom accent edge.
            let framed_art = container(art).width(Fill).height(Fill).style(frame);
            stack![framed_art, cues].width(Fill).height(Fill).into()
        }
    } else {
        container(art).width(Fill).height(Fill).style(frame).into()
    }
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
        crate::ui::reveal::width_reveal(full, DOCK_MAX_W, visible_w).height(Length::Shrink);

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
    let powering_off = row.connected
        && state
            .powering_off
            .as_ref()
            .is_some_and(|s| s == &row.serial);
    let ring_color = if flash_white {
        theme::from_rgb(crate::controller::dualsense::lightbar::IDENTIFY_FLASH)
    } else if row.connected && !powering_off {
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

    let title_color = if row.connected && !powering_off {
        theme::INK
    } else {
        theme::MUTED
    };
    let meta = if powering_off {
        theme::MUTED
    } else if row.low {
        theme::WARNING
    } else {
        theme::MUTED
    };
    let meta_text = if powering_off {
        "Powering off".to_string()
    } else {
        format!("{} · {}", row.connection, row.state)
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
        text(meta_text).size(13.0).color(meta),
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
