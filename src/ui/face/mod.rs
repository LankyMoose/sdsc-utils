//! Shared face-button hint primitives (footer, rows, cards, hero cues).
//!
//! Glyph and cluster constructors live here; callers assemble them into
//! Launcher chrome, settings, and the PS Home. The widgets are message-generic,
//! so a surface never needs its own copy of the ring/label style.
//! [`ring`] holds the SVG ring renderer.

mod ring;

use crate::domain::pad::FaceHeld;
use crate::ui::{svg_icon, theme};
use iced::widget::{Float, container, row, svg, text};
use iced::{Alignment, Border, Color, Element, Fill, Length, Padding};
use ring::{RingKind, ring_svg};

const HOLD_RING_SIZE: f32 = 32.0;
const FACE_GLYPH_SIZE: f32 = 18.0;
/// Peak scale of a face glyph while its button is pressed (layout size unchanged).
const PRESSED_SCALE: f32 = 1.1;

#[derive(Clone, Copy)]
pub(crate) enum FaceButton {
    Cross,
    Circle,
    Square,
    Triangle,
}

impl FaceButton {
    fn svg(self) -> &'static str {
        match self {
            Self::Cross => svg_icon::FACE_CROSS_SVG,
            Self::Circle => svg_icon::FACE_CIRCLE_SVG,
            Self::Square => svg_icon::FACE_SQUARE_SVG,
            Self::Triangle => svg_icon::FACE_TRIANGLE_SVG,
        }
    }

    pub(crate) fn held(self, held: FaceHeld) -> bool {
        match self {
            Self::Cross => held.cross,
            Self::Circle => held.circle,
            Self::Square => held.square,
            Self::Triangle => held.triangle,
        }
    }
}

pub(crate) struct ActionHint {
    pub(crate) face: Option<FaceButton>,
    pub(crate) text_glyph: Option<&'static str>,
    pub(crate) label: &'static str,
    pub(crate) hold: Option<f32>,
    /// 0..=1 blend of hold arc toward ink once armed (animated).
    pub(crate) armed_t: f32,
    /// Regular (non-hold) action: light the ring while the face button is pressed.
    pub(crate) pressed: bool,
    /// 0..=1 animated press amount (drives layout-stable scale).
    pub(crate) press_t: f32,
    /// Overall fade multiplier (1 = opaque). The immersive selected-hero
    /// overlay animates this with the strip scroll so the black cue box
    /// fades instead of popping; every other surface leaves it at 1.
    pub(crate) alpha: f32,
}

pub(crate) fn face_hint(
    face: FaceButton,
    label: &'static str,
    held: FaceHeld,
    press_anim: FacePressAnim,
) -> ActionHint {
    ActionHint {
        face: Some(face),
        text_glyph: None,
        label,
        hold: None,
        armed_t: 0.0,
        pressed: face.held(held),
        press_t: press_anim.for_face(face),
        alpha: 1.0,
    }
}

pub(crate) fn face_hold_hint(
    face: FaceButton,
    label: &'static str,
    progress: f32,
    armed_t: f32,
    held: FaceHeld,
    press_anim: FacePressAnim,
) -> ActionHint {
    ActionHint {
        face: Some(face),
        text_glyph: None,
        label,
        hold: Some(progress),
        armed_t,
        pressed: face.held(held),
        press_t: press_anim.for_face(face),
        alpha: 1.0,
    }
}

/// Message-agnostic action hint for surfaces that track their own held state
/// (the PS Home controller cards) instead of Launcher's [`FaceHeld`] snapshot.
pub(crate) fn plain_hint(face: FaceButton, label: &'static str, pressed: bool) -> ActionHint {
    ActionHint {
        face: Some(face),
        text_glyph: None,
        label,
        hold: None,
        armed_t: 0.0,
        pressed,
        press_t: 0.0,
        alpha: 1.0,
    }
}

/// Message-agnostic hold hint for surfaces with their own progress clock.
pub(crate) fn hold_hint(
    face: FaceButton,
    label: &'static str,
    progress: f32,
    armed_t: f32,
    pressed: bool,
) -> ActionHint {
    ActionHint {
        face: Some(face),
        text_glyph: None,
        label,
        hold: Some(progress),
        armed_t,
        pressed,
        press_t: 0.0,
        alpha: 1.0,
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FacePressAnim {
    pub(crate) cross: f32,
    pub(crate) circle: f32,
    pub(crate) square: f32,
    pub(crate) triangle: f32,
    pub(crate) options: f32,
    pub(crate) create: f32,
}

impl FacePressAnim {
    pub(crate) fn for_face(self, face: FaceButton) -> f32 {
        match face {
            FaceButton::Cross => self.cross,
            FaceButton::Circle => self.circle,
            FaceButton::Square => self.square,
            FaceButton::Triangle => self.triangle,
        }
    }
}

pub(crate) fn action_cluster<Message: 'static>(
    hints: &[ActionHint],
    flat: bool,
) -> Element<'static, Message> {
    action_cluster_spaced(hints, 28.0, flat)
}

pub(crate) fn action_cluster_spaced<Message: 'static>(
    hints: &[ActionHint],
    spacing: f32,
    flat: bool,
) -> Element<'static, Message> {
    let mut row = row![].spacing(spacing).align_y(Alignment::Center);
    for hint in hints {
        row = row.push(render_action_hint(hint, flat));
    }
    row.into()
}

fn render_action_hint<Message: 'static>(
    hint: &ActionHint,
    flat: bool,
) -> Element<'static, Message> {
    let glyph: Element<'static, Message> = if let Some(face) = hint.face {
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
        face_glyph(face, style, hint.press_t, flat, hint.alpha)
    } else {
        text_glyph_circle(
            hint.text_glyph.unwrap_or("?"),
            hint.pressed,
            hint.press_t,
            flat,
            hint.alpha,
        )
    };

    row![
        glyph,
        text(hint.label)
            .size(14.0)
            .color(theme::scale_alpha(theme::MUTED, hint.alpha))
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

pub(crate) fn text_glyph_circle<Message: 'static>(
    glyph: &'static str,
    pressed: bool,
    press_t: f32,
    flat: bool,
    alpha: f32,
) -> Element<'static, Message> {
    let half = HOLD_RING_SIZE / 2.0;
    let max_radius = half - 2.5;
    let idle_radius = max_radius / PRESSED_SCALE;
    let style = if pressed {
        RingStyle::Pressed
    } else {
        RingStyle::Idle
    };
    let stack = iced::widget::stack![
        ring_element(style, idle_radius, alpha),
        container(
            text(glyph)
                .size(12.0)
                .color(theme::scale_alpha(theme::ACCENT, alpha))
        )
        .width(Length::Fixed(HOLD_RING_SIZE))
        .height(Length::Fixed(HOLD_RING_SIZE))
        .center_x(Fill)
        .center_y(Fill),
    ]
    .width(Length::Fixed(HOLD_RING_SIZE))
    .height(Length::Fixed(HOLD_RING_SIZE));
    if flat {
        stack.into()
    } else {
        let scale = 1.0 + (PRESSED_SCALE - 1.0) * press_t.clamp(0.0, 1.0);
        Float::new(stack).scale(scale).into()
    }
}

/// Pill glyph centered in a [`HOLD_RING_SIZE`] slot so height matches face rings.
///
/// `optical_top` adds extra top padding inside the pill (monospace caps often sit high).
#[allow(clippy::too_many_arguments)]
pub(crate) fn hint_text_capsule<Message: 'static>(
    label: text::Text<'static>,
    pad_x: f32,
    optical_top: f32,
    pressed: bool,
    press_t: f32,
    flat: bool,
    alpha: f32,
) -> Element<'static, Message> {
    hint_element_capsule(label, pad_x, optical_top, pressed, press_t, flat, alpha)
}

/// Shared capsule chrome for text and multi-glyph pills.
///
/// Important: use [`Alignment::Center`], never `center_y(Fill)` — Fill expands the
/// capsule to the parent height (settings footer became a tall vertical pill).
fn hint_element_capsule<Message: 'static>(
    content: impl Into<Element<'static, Message>>,
    pad_x: f32,
    optical_top: f32,
    pressed: bool,
    press_t: f32,
    flat: bool,
    alpha: f32,
) -> Element<'static, Message> {
    let border_color = theme::scale_alpha(
        if pressed {
            theme::ACCENT
        } else {
            Color {
                a: 0.25,
                ..theme::MUTED
            }
        },
        alpha,
    );
    let border_w = if pressed { 2.5 } else { 2.0 };
    let pill_h = hint_ring_idle_diameter();
    let pill = container(content)
        .padding(Padding {
            top: optical_top,
            right: pad_x,
            bottom: 0.0,
            left: pad_x,
        })
        .height(Length::Fixed(pill_h))
        .align_y(Alignment::Center)
        .style(move |_theme| container::Style {
            border: Border {
                color: border_color,
                width: border_w,
                radius: (pill_h / 2.0).into(),
            },
            ..container::Style::default()
        });
    // Same outer slot as [`text_glyph_circle`] / face rings so labels share a baseline.
    let slot = container(pill)
        .height(Length::Fixed(HOLD_RING_SIZE))
        .align_y(Alignment::Center);
    if flat {
        slot.into()
    } else {
        let scale = 1.0 + (PRESSED_SCALE - 1.0) * press_t.clamp(0.0, 1.0);
        Float::new(slot).scale(scale).into()
    }
}

/// Visible idle diameter of face / text rings (inset so ×[`PRESSED_SCALE`] stays in-slot).
fn hint_ring_idle_diameter() -> f32 {
    let half = HOLD_RING_SIZE / 2.0;
    let max_radius = half - 2.5;
    (max_radius / PRESSED_SCALE) * 2.0
}

fn face_svg<Message: 'static>(
    face: FaceButton,
    size: f32,
    alpha: f32,
) -> Element<'static, Message> {
    svg(svg::Handle::from_memory(face.svg().as_bytes()))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .opacity(alpha.clamp(0.0, 1.0))
        .into()
}

#[derive(Clone, Copy)]
pub(crate) enum RingStyle {
    Idle,
    Pressed,
    Hold { progress: f32, armed_t: f32 },
}

pub(crate) fn face_glyph<Message: 'static>(
    face: FaceButton,
    style: RingStyle,
    press_t: f32,
    flat: bool,
    alpha: f32,
) -> Element<'static, Message> {
    let stack = face_glyph_stack(face, style, alpha);
    if flat {
        return stack;
    }
    let press_t = press_t.clamp(0.0, 1.0);
    let scale = 1.0 + (PRESSED_SCALE - 1.0) * press_t;
    // Idle radius is inset so full press (× PRESSED_SCALE) + stroke stays inside the slot.
    // Geometry stays fixed; Float scales the stack so SVG/canvas are not re-rasterized each frame.
    Float::new(stack).scale(scale).into()
}

pub(crate) fn face_glyph_stack<Message: 'static>(
    face: FaceButton,
    style: RingStyle,
    alpha: f32,
) -> Element<'static, Message> {
    // Leave room for ~2.5px stroke: idle radius is inset so full press stays
    // in-slot. Pictogram box comes snapped to whole pixels (see helper).
    let idle_radius = (HOLD_RING_SIZE / 2.0 - 2.5) / PRESSED_SCALE;
    let glyph_size = ring::pictogram_size(HOLD_RING_SIZE, PRESSED_SCALE, FACE_GLYPH_SIZE);
    iced::widget::stack![
        ring_element(style, idle_radius, alpha),
        container(face_svg(face, glyph_size, alpha))
            .width(Length::Fixed(HOLD_RING_SIZE))
            .height(Length::Fixed(HOLD_RING_SIZE))
            .center_x(Fill)
            .center_y(Fill),
    ]
    .width(Length::Fixed(HOLD_RING_SIZE))
    .height(Length::Fixed(HOLD_RING_SIZE))
    .into()
}

/// Face-button ring as SVG instead of `canvas` (see `hint_ring`): identical
/// stroked circle + hold arc, but static states rasterize once and every
/// later frame is an atlas hit instead of a rebuilt tiny-skia layer.
fn ring_element<Message: 'static>(
    style: RingStyle,
    radius: f32,
    alpha: f32,
) -> Element<'static, Message> {
    let kind = match style {
        RingStyle::Pressed => RingKind::Pressed,
        RingStyle::Idle => RingKind::Idle,
        RingStyle::Hold { progress, armed_t } => RingKind::Hold { progress, armed_t },
    };
    svg(ring_svg(kind, radius, HOLD_RING_SIZE))
        .width(Length::Fixed(HOLD_RING_SIZE))
        .height(Length::Fixed(HOLD_RING_SIZE))
        .opacity(alpha.clamp(0.0, 1.0))
        .into()
}
