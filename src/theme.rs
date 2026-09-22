//! Dark palette and shared widget styling for the iced windows.
//!
//! Colors mirror the hand-painted softbuffer UI so the iced port looks identical.

use crate::color::Rgb;
use iced::theme::Palette;
use iced::widget::{button, container, text_input};
use iced::{Background, Border, Color, Shadow, Theme};
use std::sync::LazyLock;

pub const BG: Color = rgb(23, 26, 33);
/// Configure content area: a step darker than the window chrome.
pub const BODY: Color = rgb(16, 18, 24);
/// Configure sidebar: near body, darker than [`PANEL`].
pub const SIDEBAR: Color = rgb(20, 23, 29);
pub const PANEL: Color = rgb(32, 37, 45);
pub const PANEL_HOVER: Color = rgb(40, 46, 56);
pub const LINE: Color = rgb(55, 63, 75);
pub const INK: Color = rgb(241, 243, 245);
pub const MUTED: Color = rgb(186, 193, 202);
pub const DIM: Color = rgb(128, 136, 148);

pub const ACCENT: Color = rgb(0x41, 0x41, 0xFB);
pub const DANGER: Color = rgb(0xBE, 0x00, 0x00);
pub const WARNING: Color = rgb(0xFF, 0x64, 0x00);
pub const SUCCESS: Color = rgb(0x35, 0xA0, 0x6B);

pub const RADIUS: f32 = 6.0;
pub const RADIUS_SM: f32 = 4.0;

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
        background: BG,
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

/// Opaque window background.
pub fn root(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG)),
        text_color: Some(INK),
        ..container::Style::default()
    }
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
            radius: RADIUS_SM.into(),
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

/// Configure content area below the title bar.
pub fn configure_body(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BODY)),
        text_color: Some(INK),
        ..container::Style::default()
    }
}

/// Configure sidebar: near-body fill with a hairline edge.
pub fn sidebar(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(SIDEBAR)),
        text_color: Some(INK),
        border: Border {
            color: LINE,
            width: 1.0,
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
        background: Some(Background::Color(rgb(15, 17, 22))),
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

/// One-pixel horizontal divider.
#[allow(dead_code)]
pub fn divider(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(LINE)),
        ..container::Style::default()
    }
}

/// Hairline under the configure title bar (slightly lighter than [`LINE`]).
pub fn configure_header_rule(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(rgb(68, 76, 88))),
        ..container::Style::default()
    }
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

/// Toast card: panel fill, accent-tinted border.
pub fn toast_card(accent: Color) -> impl Fn(&Theme) -> container::Style {
    move |_theme| container::Style {
        background: Some(Background::Color(PANEL)),
        text_color: Some(INK),
        border: Border {
            color: alpha(accent, 0.55),
            width: 1.0,
            radius: 8.0.into(),
        },
        shadow: Shadow {
            color: alpha(Color::BLACK, 0.45),
            offset: iced::Vector::new(0.0, 4.0),
            blur_radius: 12.0,
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
            radius: 2.0.into(),
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
        button_base(fill, ink, RADIUS_SM)
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
            (false, button::Status::Disabled) => (BG, DIM, alpha(LINE, 0.55)),
            (false, button::Status::Active) => (PANEL, MUTED, LINE),
        };
        button::Style {
            border: Border {
                color: border,
                width: 1.0,
                radius: RADIUS_SM.into(),
            },
            ..button_base(Some(fill), ink, RADIUS_SM)
        }
    }
}

/// Full-width start-menu row (in-game OSD feel).
pub fn menu_row(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let (fill, ink) = match (selected, status) {
            (true, button::Status::Disabled) => (Some(alpha(ACCENT, 0.18)), DIM),
            (true, _) => (Some(alpha(ACCENT, 0.22)), INK),
            (false, button::Status::Hovered) | (false, button::Status::Pressed) => {
                (Some(PANEL_HOVER), INK)
            }
            (false, button::Status::Disabled) => (Some(alpha(PANEL, 0.5)), DIM),
            (false, button::Status::Active) => (Some(PANEL), INK),
        };
        button_base(fill, ink, RADIUS)
    }
}

/// Mini toast card on the toast-position diagram.
pub fn position_marker(
    selected: bool,
    accent: Color,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let radius = 4.0;
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
                radius: radius.into(),
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
            ..button_base(Some(BG), DIM, RADIUS_SM)
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
        background: Background::Color(BG),
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
