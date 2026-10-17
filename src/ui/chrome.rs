//! Window chrome for utility windows (popup, Settings, compact Start).
//!
//! One decision point for the corner strategy (`notes/ui-refresh.md`):
//!
//! - **Alpha-capable backend** (Vulkan; [`wgpu_diag::alpha_composite`]): the
//!   window is transparent, the backdrop shader masks its own rounded corners
//!   ([`WINDOW_RADIUS`]) and a soft hairline traces the edge. The OS must not
//!   round (`DoNotRound`).
//! - **Opaque fallback** (DX12/GL): square backdrop, the OS rounds the HWND
//!   (`CornerPreference::Round` on Windows 11, which also draws the border).
//!
//! [`wgpu_diag::alpha_composite`]: crate::platform::wgpu_diag::alpha_composite

use crate::platform::wgpu_diag;
use crate::ui::theme;
use iced::widget::{container, stack};
use iced::{Border, Element, Fill, Theme};

/// Corner radius of utility windows when the backend composites alpha.
pub const WINDOW_RADIUS: f32 = theme::radius::LG;

/// Whether utility windows are created transparent with self-drawn corners.
pub fn transparent_windows() -> bool {
    wgpu_diag::alpha_composite()
}

/// Corner radius the window content should draw (0 when the OS rounds).
pub fn window_radius() -> f32 {
    if transparent_windows() {
        WINDOW_RADIUS
    } else {
        0.0
    }
}

/// OS corner preference matching [`transparent_windows`].
#[cfg(target_os = "windows")]
pub fn corner_preference() -> iced::window::settings::platform::CornerPreference {
    use iced::window::settings::platform::CornerPreference;
    if transparent_windows() {
        CornerPreference::DoNotRound
    } else {
        CornerPreference::Round
    }
}

/// Window edge over the desktop: a faint ink hairline at the window radius.
/// Omitted when the OS rounds (it draws its own border).
fn edge(radius: f32) -> impl Fn(&Theme) -> container::Style {
    move |_theme| container::Style {
        background: None,
        text_color: Some(theme::INK),
        border: if radius > 0.0 {
            Border {
                color: theme::alpha(theme::INK, 0.14),
                width: 1.0,
                radius: radius.into(),
            }
        } else {
            Border::default()
        },
        ..container::Style::default()
    }
}

/// Root of a utility window: nearly-static atmosphere (frozen-time ambient,
/// corner-masked when transparent) under `content`, with the window edge.
pub fn window<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let radius = window_radius();
    stack![
        crate::ui::backdrop::backdrop(radius),
        container(content)
            .width(Fill)
            .height(Fill)
            .style(edge(radius)),
    ]
    .width(Fill)
    .height(Fill)
    .into()
}
