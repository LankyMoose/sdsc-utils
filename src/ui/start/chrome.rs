//! Start-screen footer + controllers-surface assembly.
//!
//! Split from [`crate::ui::start::view`]: footer hints, the shared
//! controllers-surface footer, and immersive hint clusters live here.
//! Face glyph primitives come from [`crate::ui::start::face`], modal bodies
//! from [`crate::ui::start::settings`], press ownership and state from `view`.

use crate::persist::prefs::GamesSortMode;
use crate::ui::start::face::{
    ActionHint, FaceButton, FacePressAnim, RingStyle, action_cluster, face_glyph, face_glyph_stack,
    face_hint, face_hold_hint, hint_text_capsule,
};
use crate::ui::start::input::FaceHeld;
use crate::ui::start::view::{
    EditRow, HintPress, HintPressOwner, LaunchStatus, ROW_ACTION_SPACING, RowHintState,
    StartControllerRow, StartMessage, StartRow, State,
};
use crate::ui::theme;
use iced::widget::{button, container, row, text};
use iced::{Alignment, Element, Fill, Font, Length};

const IMMERSIVE_FOOTER_HEIGHT: f32 = 44.0;
/// Gap inside the secondary (left) footer cluster.
const FOOTER_SECONDARY_GAP: f32 = 14.0;
/// Immersive split footer min width so left/right packing reads clearly.
const FOOTER_SPLIT_MIN_W: f32 = 680.0;

/// Identify / Power-off hints for the selected controllers row.
/// Uses layout-stable glyphs (no Float scale) so dock row height cannot escape.
pub(crate) fn immersive_dock_row_hints(
    row: &StartControllerRow,
    state: &State,
    selected: bool,
) -> Option<Element<'static, StartMessage>> {
    if !selected || !row.connected {
        return None;
    }
    let hint = RowHintState::for_selected(state, true, HintPressOwner::Dock);
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
    let hint = RowHintState::for_selected(state, true, HintPressOwner::Browse);
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

/// Mouse hitch markers (not pad-navigable). Stamps `HITCH_MARK` in the logs.
/// Debug builds only. Rendered above the footer capsule.
#[cfg(debug_assertions)]
pub(crate) fn diag_report_bar() -> Element<'static, StartMessage> {
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

pub(crate) fn footer_hint<'a>(state: &'a State, flat_hints: bool) -> Element<'a, StartMessage> {
    // Settings keeps its own footer inside the panel. Dialog cards own
    // their hint rows (manual form and replace confirm both render live
    // hints inside the card), so the island never mirrors modal state:
    // there are deliberately no dialog arms here, and ownership forces
    // idle press visuals behind either dim.
    let press = HintPress::for_owner(state, HintPressOwner::Browse);
    // Circle closes (Cancel while editing) regardless of the dock state.
    let circle_label = crate::ui::start::mode::cancel_circle_label(state.editing);

    // Split layout: secondary (sort / controllers / Settings) left,
    // primary actions right.
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
    let (secondary, actions): (Element<'a, StartMessage>, Element<'a, StartMessage>) =
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
            // Left: Edit + sort. Right: Select + Start + Close.
            (
                row![
                    action_cluster(
                        &[face_hint(
                            FaceButton::Triangle,
                            "Edit",
                            press.held,
                            press.press_anim,
                        )],
                        flat_hints,
                    ),
                    face_cycle_toggle(
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
                    ),
                ]
                .spacing(FOOTER_SECONDARY_GAP)
                .align_y(Alignment::Center)
                .into(),
                row![
                    select_toggle_hint("Controllers", press.held, press.press_anim, flat_hints,),
                    options_hint_el("Settings", press.held, press.press_anim, flat_hints,),
                    action_cluster(
                        &[face_hint(
                            FaceButton::Circle,
                            circle_label,
                            press.held,
                            press.press_anim,
                        )],
                        flat_hints,
                    ),
                ]
                .spacing(FOOTER_SECONDARY_GAP)
                .align_y(Alignment::Center)
                .into(),
            )
        };

    footer_band(compose_footer_split(secondary, actions), true)
}

fn footer_secondary_row<'a>(
    view: Option<Element<'a, StartMessage>>,
    controllers: Option<Element<'a, StartMessage>>,
    settings: Option<Element<'a, StartMessage>>,
) -> Element<'a, StartMessage> {
    let mut row = iced::widget::row![]
        .spacing(FOOTER_SECONDARY_GAP)
        .align_y(Alignment::Center);
    if let Some(view) = view {
        row = row.push(view);
    }
    if let Some(cue) = controllers {
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

/// Create (Select) toggle cue with label (footer use). The island uses
/// the icon-only [`select_capsule`].
fn select_toggle_hint(
    label: &'static str,
    held: FaceHeld,
    press_anim: FacePressAnim,
    flat: bool,
) -> Element<'static, StartMessage> {
    let glyph = select_capsule(held.create, press_anim.create, flat);
    row![glyph, text(label).size(14.0).color(theme::MUTED)]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
}

/// DualSense Create → SELECT capsule (icon-only use in the island/header;
/// same metrics as the START capsule so the pair reads as opposites).
pub(crate) fn select_capsule(
    pressed: bool,
    press_t: f32,
    flat: bool,
) -> Element<'static, StartMessage> {
    let label = text("SELECT")
        .size(12.0)
        .font(Font::MONOSPACE)
        .color(theme::ACCENT);
    // Monospace caps sit high in the em box — nudge down for optical center.
    hint_text_capsule(label, 10.0, 1.0, pressed, press_t, flat)
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

fn footer_band(content: Element<'_, StartMessage>, expand: bool) -> Element<'_, StartMessage> {
    // Modals: hug the action cluster. Browse split: widen so left/right pack.
    let band = container(content)
        .height(Length::Fixed(IMMERSIVE_FOOTER_HEIGHT))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center);
    if expand {
        band.width(Length::Fixed(FOOTER_SPLIT_MIN_W)).into()
    } else {
        band.into()
    }
}

/// Square glyph + segmented labels; pad Square cycles (presentational — no mouse).
pub(crate) fn face_cycle_toggle(
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
