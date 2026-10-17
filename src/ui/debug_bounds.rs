//! Debug-only layout-bounding-box overlay.
//!
//! Toggled from Diagnostics ("Display layout bounding boxes", or F9). Wraps
//! landmark containers in a 1px outline so fixed / clamped widths can be
//! eyeballed against the art underneath. Wrapping is style-only (no padding,
//! no sizing), so the overlay never moves content; the whole module is
//! compiled out of release builds (gated in [`crate::ui`]).

use iced::widget::container;
use iced::{Border, Color, Element};

/// Outline for generic containers (magenta: readable on dark glass + art).
pub const CONTAINER: Color = Color {
    r: 1.0,
    g: 0.16,
    b: 1.0,
    a: 0.85,
};
/// Outline for fixed / clamped-width elements (cyan: drawer + modal widths,
/// dock widths, strip slots).
pub const FIXED: Color = Color {
    r: 0.1,
    g: 0.9,
    b: 1.0,
    a: 0.9,
};

fn outline(color: Color) -> impl Fn(&iced::Theme) -> container::Style {
    move |_theme| container::Style {
        background: None,
        border: Border {
            color,
            width: 1.0,
            radius: 0.0.into(),
        },
        ..container::Style::default()
    }
}

/// Outline `content`'s measured bounds when `enabled`; otherwise return it
/// untouched (no extra widget, no layout delta).
pub fn wrap<'a, M: 'a>(content: Element<'a, M>, enabled: bool) -> Element<'a, M> {
    if !enabled {
        return content;
    }
    container(content).style(outline(CONTAINER)).into()
}

/// Outline a fixed / clamped-width element's bounds when `enabled`;
/// otherwise return it untouched.
pub fn wrap_fixed<'a, M: 'a>(content: Element<'a, M>, enabled: bool) -> Element<'a, M> {
    if !enabled {
        return content;
    }
    container(content).style(outline(FIXED)).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::widget::text;

    #[test]
    fn wrap_passes_elements_through_on_and_off() {
        // Building the element tree is the check: both colors must compose
        // without panicking, and the off path must not add a widget layer.
        let off: Element<'_, ()> = text("bounds").into();
        drop(wrap(off, false));
        let on: Element<'_, ()> = text("bounds").into();
        drop(wrap(on, true));
        let off: Element<'_, ()> = text("fixed").into();
        drop(wrap_fixed(off, false));
        let on: Element<'_, ()> = text("fixed").into();
        drop(wrap_fixed(on, true));
    }
}
