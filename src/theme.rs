//! Dark palette and shared widget styling for the iced windows.
//!
//! Surface colors derive from a few seeds at the top of this file — tweak
//! [`BASE_BG`] and the darken/lift amounts to retheme all windows.

use crate::color::Rgb;
use iced::theme::Palette;
use iced::widget::{button, container, space, text_input};
use iced::{Background, Border, Color, Element, Fill, Length, Theme};
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Surface seeds — change these to retheme chrome / panels / borders
// ---------------------------------------------------------------------------

/// Window chrome fill (Settings, Start, popup roots; toast card).
pub const BASE_BG: Color = rgb(20, 22, 28);
/// Mix toward black for inset content panels (start list, configure body, popup rows).
const CONTENT_DARKEN: f32 = 0.20;
/// Sidebar is between chrome and content (lighter than [`CONTENT`]).
const SIDEBAR_DARKEN: f32 = 0.05;
/// Mix toward white for raised panels / hover / hairlines.
const PANEL_LIFT: f32 = 0.055;
const PANEL_HOVER_LIFT: f32 = 0.10;
/// Hairlines / window frame: opaque stand-in for white @ 15% over [`BASE_BG`].
const LINE_LIFT: f32 = 0.15;

/// Inset content panels (configure body, popup rows, start body).
pub const CONTENT: Color = darken(BASE_BG, CONTENT_DARKEN);
/// Configure tabs rail — slightly lighter than [`CONTENT`].
pub const SIDEBAR: Color = darken(BASE_BG, SIDEBAR_DARKEN);
/// Raised interactive surfaces (chips, list surfaces, buttons).
pub const PANEL: Color = lighten(BASE_BG, PANEL_LIFT);
pub const PANEL_HOVER: Color = lighten(BASE_BG, PANEL_HOVER_LIFT);
/// Window and widget hairlines.
pub const LINE: Color = lighten(BASE_BG, LINE_LIFT);

// Ink / accent seeds (not surface levels)
pub const INK: Color = rgb(241, 243, 245);
pub const MUTED: Color = rgb(186, 193, 202);
pub const DIM: Color = rgb(128, 136, 148);

pub const ACCENT: Color = rgb(0x41, 0x41, 0xFB);
pub const DANGER: Color = rgb(0xBE, 0x00, 0x00);
pub const WARNING: Color = rgb(0xFF, 0x64, 0x00);
pub const SUCCESS: Color = rgb(0x35, 0xA0, 0x6B);

pub const RADIUS: f32 = 2.0;
pub const RADIUS_SM: f32 = 1.0;

/// Build a [`Color`] from 8-bit channels at compile time.
pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
}

/// Re-tint an existing color with a new alpha.
pub const fn alpha(color: Color, alpha: f32) -> Color {
    Color { a: alpha, ..color }
}

/// Linear blend of two colors (`t` = 0 keeps `a`, 1 keeps `b`).
pub const fn mix(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// Mix toward black.
pub const fn darken(color: Color, amount: f32) -> Color {
    mix(color, Color::BLACK, amount)
}

/// Mix toward white.
pub const fn lighten(color: Color, amount: f32) -> Color {
    mix(color, Color::WHITE, amount)
}

/// Convert a lightbar [`Rgb`] into an iced [`Color`].
pub const fn from_rgb(value: Rgb) -> Color {
    rgb(value.r, value.g, value.b)
}

/// Convert an iced [`Color`] back into a lightbar [`Rgb`].
#[allow(dead_code)]
pub fn to_rgb(color: Color) -> Rgb {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgb::new(channel(color.r), channel(color.g), channel(color.b))
}

fn base_palette() -> Palette {
    Palette {
        background: BASE_BG,
        text: INK,
        primary: ACCENT,
        success: SUCCESS,
        warning: WARNING,
        danger: DANGER,
    }
}

static APP_THEME: LazyLock<Theme> =
    LazyLock::new(|| Theme::custom("DualSense Dark", base_palette()));

static TOAST_THEME: LazyLock<Theme> = LazyLock::new(|| {
    Theme::custom(
        "DualSense Toast",
        Palette {
            background: Color::TRANSPARENT,
            ..base_palette()
        },
    )
});

/// The dark theme used by the popup and configure windows.
pub fn app_theme() -> Theme {
    APP_THEME.clone()
}

/// A theme whose base background is fully transparent, for the overlay toast window.
pub fn toast_theme() -> Theme {
    TOAST_THEME.clone()
}

// ---------------------------------------------------------------------------
// Containers
// ---------------------------------------------------------------------------

/// Width of the [`framed`] outline on each side.
pub const WINDOW_FRAME: f32 = 1.0;

/// Opaque window face (inside the 1px [`framed`] outline).
pub fn root(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BASE_BG)),
        text_color: Some(INK),
        ..container::Style::default()
    }
}

/// Outer ring behind [`framed`] padding — actual window outline pixels.
pub fn window_frame(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(LINE)),
        text_color: Some(INK),
        ..container::Style::default()
    }
}

/// Wrap opaque window content in a 1px [`LINE`] outline.
///
/// Uses padding over a fill instead of `Border` on the root: iced hairlines at
/// the HWND edge are easy to lose (and `BASE_BG` on a black desktop disappears).
pub fn framed<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(container(content).width(Fill).height(Fill).style(root))
        .padding(WINDOW_FRAME)
        .width(Fill)
        .height(Fill)
        .style(window_frame)
        .into()
}

/// Raised surface with a hairline border.
#[allow(dead_code)]
pub fn panel(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(PANEL)),
        text_color: Some(INK),
        border: Border {
            color: LINE,
            width: 1.0,
            radius: RADIUS.into(),
        },
        ..container::Style::default()
    }
}

/// Raised surface without a border (list rows, headers).
pub fn surface(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(PANEL)),
        text_color: Some(INK),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 0.0.into(),
        },
        ..container::Style::default()
    }
}

/// Popup controller rows: inset content fill (slightly darker than chrome).
pub fn popup_row(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(CONTENT)),
        text_color: Some(INK),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 0.0.into(),
        },
        ..container::Style::default()
    }
}

/// Compact hover hint over icon-only buttons.
pub fn tooltip(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(PANEL_HOVER)),
        text_color: Some(INK),
        border: Border {
            color: LINE,
            width: 1.0,
            radius: RADIUS_SM.into(),
        },
        ..container::Style::default()
    }
}

/// Inset content pane (configure body, start body).
pub fn content(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(CONTENT)),
        text_color: Some(INK),
        ..container::Style::default()
    }
}

/// Configure content area below the title bar.
pub fn configure_body(theme: &Theme) -> container::Style {
    content(theme)
}

/// Configure sidebar: same content tier as the body pane.
pub fn sidebar(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(SIDEBAR)),
        text_color: Some(INK),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 0.0.into(),
        },
        ..container::Style::default()
    }
}

/// Inset well used for previews and pickers.
pub fn well(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(alpha(Color::BLACK, 0.25))),
        border: Border {
            color: LINE,
            width: 1.0,
            radius: RADIUS_SM.into(),
        },
        ..container::Style::default()
    }
}

/// Dark 16:9 “monitor” backdrop for the toast-position diagram.
pub fn position_stage(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(CONTENT)),
        border: Border {
            color: LINE,
            width: 1.0,
            radius: RADIUS.into(),
        },
        ..container::Style::default()
    }
}

/// Left accent rail inside a selected position marker.
pub fn position_rail(selected: bool) -> impl Fn(&Theme) -> container::Style {
    move |_theme| container::Style {
        background: selected.then_some(Background::Color(INK)),
        border: Border {
            radius: 1.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// Hairline under a window title bar (Settings, Start, Controllers popup).
pub fn configure_header_rule(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(LINE)),
        ..container::Style::default()
    }
}

/// Horizontal inset for [`list_separator`] (same idea as the title underline).
pub const LIST_SEPARATOR_INSET: f32 = 12.0;
/// Vertical pad around the 1px rule — total gap between rows is [`LIST_SEPARATOR_GAP`].
const LIST_SEPARATOR_PAD_Y: f32 = 5.0;
/// Layout height contributed by one inter-row separator (rule + padding).
pub const LIST_SEPARATOR_GAP: f32 = 1.0 + LIST_SEPARATOR_PAD_Y * 2.0;
/// List rules sit between [`CONTENT`] and [`LINE`] so they read softer than title chrome.
const LIST_SEPARATOR_LIFT: f32 = 0.08;

fn list_separator_color() -> Color {
    lighten(BASE_BG, LIST_SEPARATOR_LIFT)
}

fn list_separator_rule(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(list_separator_color())),
        ..container::Style::default()
    }
}

/// Inset 1px hairline between list rows (start menu, controllers popup).
pub fn list_separator<'a, Message: 'a>() -> Element<'a, Message> {
    container(
        container(space())
            .width(Fill)
            .height(Length::Fixed(1.0))
            .style(list_separator_rule),
    )
    .padding([LIST_SEPARATOR_PAD_Y, LIST_SEPARATOR_INSET])
    .width(Fill)
    .into()
}

/// Solid color chip, e.g. a gradient stop swatch.
pub fn swatch(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_theme| container::Style {
        background: Some(Background::Color(color)),
        border: Border {
            color: LINE,
            width: 1.0,
            radius: RADIUS_SM.into(),
        },
        ..container::Style::default()
    }
}

/// Toast card: flat dark chrome matching popup / Settings.
pub fn toast_card(_accent: Color) -> impl Fn(&Theme) -> container::Style {
    move |_theme| container::Style {
        background: Some(Background::Color(BASE_BG)),
        text_color: Some(INK),
        border: Border {
            color: LINE,
            width: 1.0,
            radius: 0.0.into(),
        },
        ..container::Style::default()
    }
}

/// Vertical accent rail drawn down the left edge of a toast.
pub fn rail(accent: Color) -> impl Fn(&Theme) -> container::Style {
    move |_theme| container::Style {
        background: Some(Background::Color(accent)),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 0.0.into(),
        },
        ..container::Style::default()
    }
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

fn button_base(background: Option<Color>, text_color: Color, radius: f32) -> button::Style {
    button::Style {
        background: background.map(Background::Color),
        text_color,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: radius.into(),
        },
        ..button::Style::default()
    }
}

/// Transparent button that only lights up on hover. Used for icon actions.
pub fn ghost(_theme: &Theme, status: button::Status) -> button::Style {
    match status {
        button::Status::Active => button_base(None, MUTED, RADIUS_SM),
        button::Status::Hovered => button_base(Some(PANEL_HOVER), INK, RADIUS_SM),
        button::Status::Pressed => button_base(Some(LINE), INK, RADIUS_SM),
        button::Status::Disabled => button_base(None, DIM, RADIUS_SM),
    }
}

/// Vertical tab in the configure sidebar.
pub fn tab(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let (fill, ink) = match (selected, status) {
            (true, button::Status::Disabled) => (Some(alpha(ACCENT, 0.12)), DIM),
            (true, _) => (Some(alpha(ACCENT, 0.28)), INK),
            (false, button::Status::Hovered) | (false, button::Status::Pressed) => {
                (Some(PANEL_HOVER), INK)
            }
            (false, button::Status::Disabled) => (None, DIM),
            (false, button::Status::Active) => (None, MUTED),
        };
        button_base(fill, ink, 0.0)
    }
}

/// Selectable chip used by stop rows and similar compact toggles.
pub fn chip(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let (fill, ink, border) = match (selected, status) {
            (true, button::Status::Disabled) => (alpha(ACCENT, 0.12), DIM, alpha(ACCENT, 0.35)),
            (true, _) => (alpha(ACCENT, 0.28), INK, ACCENT),
            (false, button::Status::Hovered) | (false, button::Status::Pressed) => {
                (PANEL_HOVER, INK, LINE)
            }
            (false, button::Status::Disabled) => (BASE_BG, DIM, alpha(LINE, 0.55)),
            (false, button::Status::Active) => (PANEL, MUTED, LINE),
        };
        button::Style {
            border: Border {
                color: border,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..button_base(Some(fill), ink, 0.0)
        }
    }
}

/// Full-width start-menu row (in-game OSD feel).
pub fn menu_row(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        // Desaturate toward DIM, then a lighter wash so the row stays dark.
        let select = mix(ACCENT, DIM, 0.55);
        let (fill, ink) = match (selected, status) {
            (true, button::Status::Disabled) => (Some(alpha(select, 0.12)), DIM),
            (true, _) => (Some(alpha(select, 0.16)), INK),
            (false, button::Status::Hovered) | (false, button::Status::Pressed) => {
                (Some(alpha(PANEL_HOVER, 0.85)), INK)
            }
            (false, button::Status::Disabled) => (None, DIM),
            (false, button::Status::Active) => (None, INK),
        };
        button_base(fill, ink, RADIUS_SM)
    }
}

/// Mini toast card on the toast-position diagram.
pub fn position_marker(
    selected: bool,
    accent: Color,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let (fill, border) = match (selected, status) {
            (true, _) => (accent, INK),
            (false, button::Status::Hovered) | (false, button::Status::Pressed) => {
                (PANEL_HOVER, LINE)
            }
            (false, _) => (PANEL, LINE),
        };
        button::Style {
            background: Some(Background::Color(fill)),
            text_color: INK,
            border: Border {
                color: border,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..button::Style::default()
        }
    }
}

/// Filled accent button.
pub fn primary(_theme: &Theme, status: button::Status) -> button::Style {
    match status {
        button::Status::Active => button_base(Some(ACCENT), INK, RADIUS_SM),
        button::Status::Hovered => button_base(Some(alpha(ACCENT, 0.85)), INK, RADIUS_SM),
        button::Status::Pressed => button_base(Some(alpha(ACCENT, 0.70)), INK, RADIUS_SM),
        button::Status::Disabled => button_base(Some(LINE), DIM, RADIUS_SM),
    }
}

/// Outlined destructive button.
#[allow(dead_code)]
pub fn danger(_theme: &Theme, status: button::Status) -> button::Style {
    match status {
        button::Status::Active => button::Style {
            border: Border {
                color: alpha(DANGER, 0.65),
                width: 1.0,
                radius: RADIUS_SM.into(),
            },
            ..button_base(None, INK, RADIUS_SM)
        },
        button::Status::Hovered => button::Style {
            border: Border {
                color: alpha(DANGER, 0.85),
                width: 1.0,
                radius: RADIUS_SM.into(),
            },
            ..button_base(Some(alpha(DANGER, 0.25)), INK, RADIUS_SM)
        },
        button::Status::Pressed => button::Style {
            border: Border {
                color: DANGER,
                width: 1.0,
                radius: RADIUS_SM.into(),
            },
            ..button_base(Some(alpha(DANGER, 0.40)), INK, RADIUS_SM)
        },
        button::Status::Disabled => button::Style {
            border: Border {
                color: alpha(LINE, 0.55),
                width: 1.0,
                radius: RADIUS_SM.into(),
            },
            ..button_base(Some(BASE_BG), DIM, RADIUS_SM)
        },
    }
}

/// A whole-row button that only tints on hover.
#[allow(dead_code)]
pub fn row_button(_theme: &Theme, status: button::Status) -> button::Style {
    match status {
        button::Status::Active => button_base(None, INK, RADIUS_SM),
        button::Status::Hovered | button::Status::Pressed => {
            button_base(Some(PANEL_HOVER), INK, RADIUS_SM)
        }
        button::Status::Disabled => button_base(None, DIM, RADIUS_SM),
    }
}

// ---------------------------------------------------------------------------
// Text inputs
// ---------------------------------------------------------------------------

pub fn input(_theme: &Theme, status: text_input::Status) -> text_input::Style {
    let border_color = match status {
        text_input::Status::Focused { .. } => ACCENT,
        text_input::Status::Hovered => MUTED,
        text_input::Status::Active | text_input::Status::Disabled => LINE,
    };
    text_input::Style {
        background: Background::Color(BASE_BG),
        border: Border {
            color: border_color,
            width: 1.0,
            radius: RADIUS_SM.into(),
        },
        icon: MUTED,
        placeholder: DIM,
        value: if matches!(status, text_input::Status::Disabled) {
            DIM
        } else {
            INK
        },
        selection: alpha(ACCENT, 0.45),
    }
}
