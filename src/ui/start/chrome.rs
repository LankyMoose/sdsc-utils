//! Compact chrome + footer assembly for the Start screen.
//!
//! Behavior-preserving split from [`crate::ui::start::view`]: header, footer,
//! scan-veil root, modal branches, and immersive hint clusters live here.
//! Face glyph primitives come from [`crate::ui::start::face`], modal bodies
//! from [`crate::ui::start::settings`], press ownership and state from `view`.

use crate::persist::prefs::GamesSortMode;
use crate::ui::color::BatterySpectrum;
use crate::ui::start::face::{
    ActionHint, FaceButton, FacePressAnim, RingStyle, action_cluster, face_glyph, face_glyph_stack,
    face_hint, face_hold_hint, hint_text_capsule, text_glyph_capsule, text_glyph_circle,
};
use crate::ui::start::input::FaceHeld;
use crate::ui::start::list::{controllers_list, games_list};
use crate::ui::start::settings::{manual_add_view, replace_confirm_view};
use crate::ui::start::view::{
    EditRow, FOOTER_HEIGHT, HEADER_HEIGHT, HintPress, HintPressOwner, LaunchStatus, PAD_Y, PADDING,
    PANE_W, ROW_ACTION_SPACING, RowHintState, StartControllerRow, StartMessage, StartRow,
    StartSlide, State, SteamScanBannerVisual, scan_status_chip, scan_status_slide,
};
use crate::ui::{chrome, theme};
use iced::widget::{button, column, container, row, space, text};
use iced::{Alignment, Color, Element, Fill, Font, Length};
use std::time::Instant;

const IMMERSIVE_FOOTER_HEIGHT: f32 = 44.0;
/// Gap inside the secondary (left) footer cluster.
const FOOTER_SECONDARY_GAP: f32 = 14.0;
/// Immersive split footer min width so left/right packing reads clearly.
const FOOTER_SPLIT_MIN_W: f32 = 560.0;
const TITLE_ACTIVE: f32 = 20.0;
const TITLE_INACTIVE: f32 = 15.0;
const CUE_SIZE: f32 = 14.0;
/// Width reserved for an L2/R2 cue plus gap while revealed.
const CUE_SLOT_W: f32 = 30.0;

#[allow(clippy::too_many_arguments)]
pub(crate) fn compact_chrome<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    now: Instant,
    always_immersive: bool,
    promote_gesture: &'a [crate::domain::gesture::GestureControl],
    flat_hints: bool,
    steam_scan_banner: Option<SteamScanBannerVisual>,
    #[cfg(debug_assertions)] debug_bounds: bool,
) -> Element<'a, StartMessage> {
    let header = slide_header(state.slide_progress(now), steam_scan_banner, now);
    #[cfg(debug_assertions)]
    let header: Element<'_, StartMessage> = crate::ui::debug_bounds::wrap(header, debug_bounds);

    let body = if state.manual_add.is_some() {
        manual_add_view(state)
    } else if state.replace_confirm.is_some() {
        replace_confirm_view(state)
    } else {
        carousel_body(state, spectrum, now)
    };
    let body = container(body).width(Fill).height(Fill);
    #[cfg(debug_assertions)]
    let body = crate::ui::debug_bounds::wrap(body.into(), debug_bounds);

    let hint = footer_hint(state, false, always_immersive, promote_gesture, flat_hints);
    #[cfg(debug_assertions)]
    let hint: Element<'_, StartMessage> = crate::ui::debug_bounds::wrap(hint, debug_bounds);
    #[cfg(debug_assertions)]
    let diag = diag_report_bar();
    #[cfg(not(debug_assertions))]
    let diag: Element<'_, StartMessage> = space().height(Length::Fixed(0.0)).into();

    chrome::window(
        column![header, body, diag, hint,]
            .spacing(10)
            .padding([PAD_Y, PADDING])
            .width(Fill)
            .height(Fill),
    )
}

/// Identify / Power-off hints for an immersive dock row (mirrors compact Controllers).
/// Uses layout-stable glyphs (no Float scale) so dock row height cannot escape.
pub(crate) fn immersive_dock_row_hints(
    row: &StartControllerRow,
    state: &State,
    selected: bool,
) -> Option<Element<'static, StartMessage>> {
    if !selected || !row.connected {
        return None;
    }
    let hint = RowHintState::for_selected(state, true);
    let powering_off = state
        .powering_off
        .as_ref()
        .is_some_and(|s| s == &row.serial);
    if powering_off {
        return Some(action_cluster_dock(&[face_hint(
            FaceButton::Triangle,
            "Powering off…",
            hint.held,
            hint.press_anim,
        )]));
    }
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
    Some(action_cluster_dock(&actions))
}

/// Edit-mode membership mark for the immersive hero meta column.
pub(crate) fn immersive_game_membership_label(
    row: &StartRow,
    alpha: f32,
) -> Element<'static, StartMessage> {
    let a = alpha.clamp(0.0, 1.0);
    let in_lib = row.in_catalog();
    let mark = if in_lib { "✓" } else { "○" };
    let mark_color = if in_lib {
        theme::alpha(theme::ACCENT, a)
    } else {
        theme::alpha(theme::MUTED, 0.7 * a)
    };
    let label = if in_lib {
        "In library"
    } else {
        "Not in library"
    };
    // Match [`action_hint_dock`] label size + color.
    row![
        text(mark).size(14.0).color(mark_color),
        text(label)
            .size(13.0)
            .color(theme::alpha(theme::INK, 0.85 * a)),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

/// Launch / Close game / edit face cues beside the immersive selected hero.
///
/// Layout-stable (`action_cluster_dock`) so veil/Float cannot resize the meta column.
/// `alpha` fades the whole cluster with the strip scroll (see
/// [`State::hero_overlay_alpha`]); other callers pass 1.0.
pub(crate) fn immersive_game_hints(
    row: &StartRow,
    state: &State,
    alpha: f32,
) -> Element<'static, StartMessage> {
    let hint = RowHintState::for_selected(state, true);
    let mut actions = Vec::new();
    let status = if state.editing {
        None
    } else {
        state.launch_status(&row.target)
    };
    if state.editing {
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
    } else if status == Some(LaunchStatus::Closing) {
        actions.push(face_hint(
            FaceButton::Cross,
            "Closing…",
            hint.held,
            hint.press_anim,
        ));
    } else if status == Some(LaunchStatus::Launching) {
        actions.push(face_hint(
            FaceButton::Cross,
            "Launching…",
            hint.held,
            hint.press_anim,
        ));
    } else if row.disabled {
        actions.push(face_hint(
            FaceButton::Cross,
            "Not installed",
            hint.held,
            hint.press_anim,
        ));
    } else if status == Some(LaunchStatus::Running) {
        actions.push(face_hold_hint(
            FaceButton::Cross,
            "Close game",
            hint.cross_progress,
            hint.cross_armed_t,
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
    let a = alpha.clamp(0.0, 1.0);
    for h in actions.iter_mut() {
        h.alpha = a;
    }
    let cluster = action_cluster_dock(&actions);
    // Manual Edit is mouse-only (Square cycles sort in edit too).
    if state.editing && matches!(row.edit, Some(EditRow::Manual { .. })) {
        row![
            cluster,
            button(
                text("Edit")
                    .size(13.0)
                    .color(theme::alpha(theme::ACCENT, a))
            )
            .padding([4, 10])
            .on_press(StartMessage::EditManual)
            .style(theme::ghost),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .into()
    } else {
        cluster
    }
}

/// Mouse + global hotkey hitch markers (not pad-navigable). Stamps `HITCH_MARK` in the logs.
/// Debug builds only.
#[cfg(debug_assertions)]
fn diag_report_bar() -> Element<'static, StartMessage> {
    row![
        button(
            text("Report lightbar write failure (F7)")
                .size(11.0)
                .color(theme::MUTED),
        )
        .padding([4, 8])
        .on_press(StartMessage::ReportLightbarFailure)
        .style(theme::ghost),
        button(
            text("Report input failure (F8)")
                .size(11.0)
                .color(theme::MUTED),
        )
        .padding([4, 8])
        .on_press(StartMessage::ReportInputFailure)
        .style(theme::ghost),
    ]
    .spacing(8)
    .into()
}

/// Header titles and L2/R2 cues interpolate with carousel progress (0 = Games, 1 = Controllers).
pub(crate) fn slide_header(
    progress: f32,
    steam_scan_banner: Option<SteamScanBannerVisual>,
    now: Instant,
) -> Element<'static, StartMessage> {
    slide_header_metrics(
        progress,
        HEADER_HEIGHT,
        TITLE_ACTIVE,
        TITLE_INACTIVE,
        CUE_SIZE,
        CUE_SLOT_W,
        steam_scan_banner,
        now,
    )
}

#[allow(clippy::too_many_arguments)]
fn slide_header_metrics(
    progress: f32,
    height: f32,
    title_active: f32,
    title_inactive: f32,
    cue_size: f32,
    cue_slot_w: f32,
    steam_scan_banner: Option<SteamScanBannerVisual>,
    now: Instant,
) -> Element<'static, StartMessage> {
    let games_t = progress;
    let controllers_t = 1.0 - progress;

    let games_label = text(StartSlide::Games.title())
        .size(lerp(title_active, title_inactive, games_t))
        .color(lerp_color(
            theme::INK,
            theme::alpha(theme::MUTED, 0.45),
            games_t,
        ));
    let controllers_label = text(StartSlide::Controllers.title())
        .size(lerp(title_active, title_inactive, controllers_t))
        .color(lerp_color(
            theme::INK,
            theme::alpha(theme::MUTED, 0.45),
            controllers_t,
        ));

    let left = row![
        slide_cue("L2", games_t, Alignment::Start, cue_size, cue_slot_w),
        games_label,
    ]
    .spacing(0)
    .align_y(Alignment::End);

    let right = row![
        controllers_label,
        slide_cue("R2", controllers_t, Alignment::End, cue_size, cue_slot_w),
    ]
    .spacing(0)
    .align_y(Alignment::End);

    let center: Element<'_, StartMessage> = if let Some(visual) = steam_scan_banner {
        // Full-header clip at the underline; chip rests above it and slides the whole way.
        let chip_h = 16.0;
        let progress = (visual.stack_slot + 1.0).clamp(0.0, 1.0);
        container(scan_status_slide(
            scan_status_chip(12.0, 11.0, now, visual),
            progress,
            chip_h,
            height,
            6.0,
        ))
        .width(Fill)
        .align_x(Alignment::Center)
        .into()
    } else {
        space().width(Length::Fixed(8.0)).into()
    };

    container(
        row![
            container(left)
                .width(Fill)
                .height(Fill)
                .align_x(Alignment::Start)
                .align_y(Alignment::End),
            center,
            container(right)
                .width(Fill)
                .height(Fill)
                .align_x(Alignment::End)
                .align_y(Alignment::End),
        ]
        .height(Length::Fixed(height)),
    )
    .width(Fill)
    .height(Length::Fixed(height))
    .clip(true)
    .into()
}

/// Width-revealed + faded L2/R2 cue (`amount` 0 = hidden, 1 = fully shown).
fn slide_cue(
    label: &'static str,
    amount: f32,
    align: Alignment,
    cue_size: f32,
    cue_slot_w: f32,
) -> Element<'static, StartMessage> {
    let amount = amount.clamp(0.0, 1.0);
    let width = cue_slot_w * amount;
    container(
        text(label)
            .size(cue_size)
            .color(theme::alpha(theme::ACCENT, amount)),
    )
    .width(Length::Fixed(width))
    .align_x(align)
    .clip(true)
    .into()
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color {
        r: lerp(a.r, b.r, t),
        g: lerp(a.g, b.g, t),
        b: lerp(a.b, b.b, t),
        a: lerp(a.a, b.a, t),
    }
}

fn carousel_body<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    now: Instant,
) -> Element<'a, StartMessage> {
    let scroll = state.slide_scroll_x(now);
    crate::ui::start::carousel::carousel(
        scroll,
        PANE_W,
        games_list(state),
        controllers_list(state, spectrum),
    )
    .width(Length::Fixed(PANE_W))
    .height(Fill)
    .into()
}

/// Empty browse Games list: "press [△] to edit" with the standard ringed face glyph.
pub(crate) fn empty_games_browse_prompt(
    state: &State,
    text_size: f32,
) -> Element<'static, StartMessage> {
    let press = HintPress::for_owner(state, HintPressOwner::Browse);
    let face = FaceButton::Triangle;
    let style = if face.held(press.held) {
        RingStyle::Pressed
    } else {
        RingStyle::Idle
    };
    // Flat: body prompt must not Float-scale (same stacking constraint as dock glyphs).
    let glyph = face_glyph(face, style, press.press_anim.for_face(face), true);
    row![
        text("No games yet - press")
            .size(text_size)
            .color(theme::MUTED),
        glyph,
        text("to edit.").size(text_size).color(theme::MUTED),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

pub(crate) fn footer_hint<'a>(
    state: &'a State,
    immersive: bool,
    always_immersive: bool,
    promote_gesture: &'a [crate::domain::gesture::GestureControl],
    flat_hints: bool,
) -> Element<'a, StartMessage> {
    // Settings keeps its own footer inside the panel — main chrome footer stays browse/modals.
    if state.manual_add.is_some() {
        let press = HintPress::for_owner(state, HintPressOwner::ManualAdd);
        let label = if state.manual_add.as_ref().is_some_and(|d| d.is_edit()) {
            "Save"
        } else {
            "Add"
        };
        return footer_band(
            action_cluster(
                &[
                    face_hint(FaceButton::Cross, label, press.held, press.press_anim),
                    face_hint(FaceButton::Circle, "Cancel", press.held, press.press_anim),
                ],
                flat_hints,
            ),
            immersive,
            false,
        );
    }
    if state.replace_confirm.is_some() {
        let press = HintPress::for_owner(state, HintPressOwner::ReplaceConfirm);
        return footer_band(
            action_cluster(
                &[
                    face_hold_hint(
                        FaceButton::Cross,
                        "Proceed",
                        press.cross_progress,
                        press.cross_armed_t,
                        press.held,
                        press.press_anim,
                    ),
                    face_hint(FaceButton::Circle, "Cancel", press.held, press.press_anim),
                ],
                flat_hints,
            ),
            immersive,
            false,
        );
    }

    let press = HintPress::for_owner(state, HintPressOwner::Browse);
    let circle_label =
        crate::ui::start::mode::cancel_circle_label(immersive, always_immersive, state.editing);
    // Compact footer: reopen chord → Immersive. Immersive demote is Circle (Back) when
    // always-immersive is off — do not also advertise Compact on the reopen chord.
    let toggle_label = if !state.editing
        && !immersive
        && crate::ui::start::mode::promote_gesture_usable(promote_gesture)
    {
        Some("Immersive")
    } else {
        None
    };
    let promote_cue: Option<Element<'a, StartMessage>> = toggle_label.map(|label| {
        gesture_chord_hint(
            promote_gesture,
            label,
            press.reopen_held,
            press.reopen_press,
            flat_hints,
        )
    });

    // Split layout: secondary (view / Immersive / Settings) left, primary actions right.
    let (secondary, actions): (Element<'a, StartMessage>, Element<'a, StartMessage>) =
        match state.slide {
            StartSlide::Games => {
                let hints = [
                    face_hint(
                        FaceButton::Triangle,
                        if state.editing { "Save" } else { "Edit" },
                        press.held,
                        press.press_anim,
                    ),
                    face_hint(
                        FaceButton::Circle,
                        circle_label,
                        press.held,
                        press.press_anim,
                    ),
                ];
                let actions = action_cluster(&hints, flat_hints);
                if state.editing {
                    (
                        footer_secondary_row(
                            Some(face_cycle_toggle(
                                FaceButton::Square,
                                press.held,
                                press.press_anim,
                                &[
                                    (
                                        "Last played",
                                        matches!(state.sort_mode, GamesSortMode::LastPlayed),
                                    ),
                                    (
                                        "A–Z",
                                        matches!(state.sort_mode, GamesSortMode::Alphabetical),
                                    ),
                                ],
                                flat_hints,
                            )),
                            Some(
                                button(text("Add shortcut…").size(14.0).color(theme::ACCENT))
                                    .padding([6, 12])
                                    .on_press(StartMessage::AddShortcut)
                                    .style(theme::ghost)
                                    .into(),
                            ),
                            None,
                        ),
                        actions,
                    )
                } else {
                    (
                        footer_secondary_row(
                            Some(face_cycle_toggle(
                                FaceButton::Square,
                                press.held,
                                press.press_anim,
                                &[
                                    (
                                        "Last played",
                                        matches!(state.sort_mode, GamesSortMode::LastPlayed),
                                    ),
                                    (
                                        "A–Z",
                                        matches!(state.sort_mode, GamesSortMode::Alphabetical),
                                    ),
                                ],
                                flat_hints,
                            )),
                            promote_cue,
                            Some(options_hint_el(
                                "Settings",
                                press.held,
                                press.press_anim,
                                flat_hints,
                            )),
                        ),
                        actions,
                    )
                }
            }
            StartSlide::Controllers => (
                footer_secondary_row(
                    Some(face_cycle_toggle(
                        FaceButton::Square,
                        press.held,
                        press.press_anim,
                        &[
                            ("Connected", !state.show_all_controllers),
                            ("All", state.show_all_controllers),
                        ],
                        flat_hints,
                    )),
                    promote_cue,
                    Some(options_hint_el(
                        "Settings",
                        press.held,
                        press.press_anim,
                        flat_hints,
                    )),
                ),
                action_cluster(
                    &[face_hint(
                        FaceButton::Circle,
                        circle_label,
                        press.held,
                        press.press_anim,
                    )],
                    flat_hints,
                ),
            ),
        };

    footer_band(compose_footer_split(secondary, actions), immersive, true)
}

fn footer_secondary_row<'a>(
    view: Option<Element<'a, StartMessage>>,
    promote: Option<Element<'a, StartMessage>>,
    settings: Option<Element<'a, StartMessage>>,
) -> Element<'a, StartMessage> {
    let mut row = iced::widget::row![]
        .spacing(FOOTER_SECONDARY_GAP)
        .align_y(Alignment::Center);
    if let Some(view) = view {
        row = row.push(view);
    }
    if let Some(cue) = promote {
        row = row.push(cue);
    }
    if let Some(settings) = settings {
        row = row.push(settings);
    }
    row.into()
}

fn compose_footer_split<'a>(
    secondary: Element<'a, StartMessage>,
    actions: Element<'a, StartMessage>,
) -> Element<'a, StartMessage> {
    iced::widget::row![
        container(secondary)
            .width(Fill)
            .align_x(Alignment::Start)
            .align_y(Alignment::Center),
        container(actions)
            .align_x(Alignment::End)
            .align_y(Alignment::Center),
    ]
    .width(Fill)
    .align_y(Alignment::Center)
    .into()
}

fn options_hint_el(
    label: &'static str,
    held: FaceHeld,
    press_anim: FacePressAnim,
    flat: bool,
) -> Element<'static, StartMessage> {
    let glyph = start_options_capsule(held.options, press_anim.options, flat);
    row![glyph, text(label).size(14.0).color(theme::MUTED)]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
}

/// DualSense Options → START capsule (visual height matches face-ring idle diameter).
fn start_options_capsule(
    pressed: bool,
    press_t: f32,
    flat: bool,
) -> Element<'static, StartMessage> {
    let label = text("START")
        .size(12.0)
        .font(Font::MONOSPACE)
        .color(theme::ACCENT);
    // Monospace caps sit high in the em box — nudge down for optical center.
    hint_text_capsule(label, 10.0, 1.0, pressed, press_t, flat)
}

/// Reopen-chord cue: short single control → circle; else one capsule with joined text.
fn gesture_chord_hint(
    controls: &[crate::domain::gesture::GestureControl],
    label: &'static str,
    pressed: bool,
    press_t: f32,
    flat: bool,
) -> Element<'static, StartMessage> {
    let glyph = if gesture_uses_circle(controls) {
        text_glyph_circle(controls[0].as_str(), pressed, press_t, flat)
    } else {
        text_glyph_capsule(
            crate::domain::gesture::format_gesture(controls),
            pressed,
            press_t,
            flat,
        )
    };
    row![glyph, text(label).size(14.0).color(theme::MUTED)]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
}

/// Circle for a single control whose label is at most 2 characters (e.g. PS, L3).
fn gesture_uses_circle(controls: &[crate::domain::gesture::GestureControl]) -> bool {
    matches!(controls, [c] if c.as_str().chars().count() <= 2)
}

fn footer_band(
    content: Element<'_, StartMessage>,
    immersive: bool,
    expand_immersive: bool,
) -> Element<'_, StartMessage> {
    let height = if immersive {
        IMMERSIVE_FOOTER_HEIGHT
    } else {
        FOOTER_HEIGHT
    };
    // Immersive modals: hug the action cluster. Browse split: widen so left/right pack.
    // Compact: fill the footer band.
    let band = container(content)
        .height(Length::Fixed(height))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center);
    if immersive {
        if expand_immersive {
            band.width(Length::Fixed(FOOTER_SPLIT_MIN_W)).into()
        } else {
            band.into()
        }
    } else {
        band.width(Fill).into()
    }
}

/// Square glyph + segmented labels; pad Square cycles (presentational — no mouse).
fn face_cycle_toggle(
    face: FaceButton,
    held: FaceHeld,
    press_anim: FacePressAnim,
    options: &[(&'static str, bool)],
    flat: bool,
) -> Element<'static, StartMessage> {
    let dim = theme::alpha(theme::MUTED, 0.45);
    let glyph = face_glyph(
        face,
        if face.held(held) {
            RingStyle::Pressed
        } else {
            RingStyle::Idle
        },
        press_anim.for_face(face),
        flat,
    );
    let mut labels = row![].spacing(8).align_y(Alignment::Center);
    labels = labels.push(glyph);
    for (i, (label, active)) in options.iter().copied().enumerate() {
        if i > 0 {
            labels = labels.push(text("·").size(14.0).color(dim));
        }
        labels = labels.push(toggle_option_label(label, active));
    }
    labels.into()
}

fn toggle_option_label(label: &'static str, active: bool) -> Element<'static, StartMessage> {
    let color = if active {
        theme::INK
    } else {
        theme::alpha(theme::MUTED, 0.55)
    };
    text(label).size(14.0).color(color).into()
}

/// Dock row cluster — no Float scale (keeps fixed row height).
fn action_cluster_dock(hints: &[ActionHint]) -> Element<'static, StartMessage> {
    let mut row = row![]
        .spacing(ROW_ACTION_SPACING)
        .align_y(Alignment::Center);
    for hint in hints {
        row = row.push(action_hint_dock(hint));
    }
    row.into()
}

fn action_hint_dock(hint: &ActionHint) -> Element<'static, StartMessage> {
    let a = hint.alpha.clamp(0.0, 1.0);
    let glyph: Element<'static, StartMessage> = if let Some(face) = hint.face {
        let style = if let Some(progress) = hint.hold {
            RingStyle::Hold {
                progress,
                armed_t: hint.armed_t,
            }
        } else if hint.pressed {
            RingStyle::Pressed
        } else {
            RingStyle::Idle
        };
        face_glyph_dock(face, style, a)
    } else {
        text(hint.text_glyph.unwrap_or("?"))
            .size(14.0)
            .color(theme::alpha(theme::ACCENT, a))
            .into()
    };

    // Brighter than footer MUTED — dock/meta cues sit over translucent backdrop wash.
    row![
        glyph,
        text(hint.label)
            .size(13.0)
            .color(theme::alpha(theme::INK, 0.85 * a)),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

/// Dock-safe glyph: same art, no Float (scaled Float becomes an overlay and breaks row height).
fn face_glyph_dock(
    face: FaceButton,
    style: RingStyle,
    alpha: f32,
) -> Element<'static, StartMessage> {
    face_glyph_stack(face, style, alpha)
}
