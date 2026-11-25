//! Standalone Browser add-on window rendering.
//!
//! v0 is chrome-less: the window renders the CEF page surface plus the pad
//! cursor. The panel state lives in `ui::browser::state`; this module owns the
//! page/cursor composition and the browser-facing message vocabulary. The
//! system OSK is an overlay owned by the main app, never rendered here.

use crate::browser::engine::BrowserCursor;
use crate::ui::browser::state::BrowserPanel;
use crate::ui::theme;
use iced::widget::text::Wrapping;
use iced::widget::{column, container, mouse_area, row, scrollable, space, stack, text};
use iced::{Element, Fill, Length};

/// Everything the standalone Browser window can ask the shell/engine to do.
#[derive(Debug, Clone)]
pub enum BrowserMessage {
    /// Semantic history action (L1 default).
    Back,
    /// Semantic history action (R1 default).
    Forward,
    /// Semantic reload action (Options default).
    Reload,
    /// Hardware pointer moved over the live page (page-local coordinates).
    PagePointerMoved {
        x: f32,
        y: f32,
    },
    /// Physical left button pressed on the live page.
    PagePress,
    /// Physical left button released on the live page.
    PageRelease,
    /// Pointer left the live page.
    PagePointerLeft,
    /// Hardware wheel over the live page; uses `BrowserPanel::page_mouse`.
    PageWheel {
        dx: i32,
        dy: i32,
    },
    Scrolled(f32),
    #[cfg(debug_assertions)]
    Shot(iced::window::Screenshot),
}

impl BrowserMessage {
    /// Stable semantic action name for the trace timeline. `None` means the
    /// message is high-frequency detail (typing, scroll, screenshots) and is
    /// recorded by its owning lane only when needed.
    pub fn trace_label(&self) -> Option<&'static str> {
        match self {
            Self::Back => Some("browser.history.back"),
            Self::Forward => Some("browser.history.forward"),
            Self::Reload => Some("browser.reload"),
            Self::PagePointerMoved { .. }
            | Self::PagePress
            | Self::PageRelease
            | Self::PagePointerLeft
            | Self::PageWheel { .. } => None,
            Self::Scrolled(_) => None,
            #[cfg(debug_assertions)]
            Self::Shot(_) => None,
        }
    }
}

/// Scrollable id for the browser body (pad scroll + `on_scroll` reports).
pub fn browser_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("browser-body")
}

/// Pad Up/Down body scroll step.
pub const BROWSER_SCROLL_STEP: f32 = 240.0;

/// Whether the pad cursor should be drawn at all. A physical pointer owns the
/// surface.
fn cursor_draws(panel: &BrowserPanel) -> bool {
    !panel.page_mouse_active
}

/// Map a page cursor point to card pixels. The surface is 1:1: the engine
/// viewport is resized to the page area.
fn cursor_card_point(x: f32, y: f32) -> (f32, f32) {
    (x, y)
}

/// CEF wheel deltas use the Windows 120-units-per-line convention. iced
/// reports line deltas for wheel notches and pixel deltas for precision
/// touchpads; both preserve the sign, which CEF expects positive = up/left.
pub(crate) fn scroll_delta_units(delta: iced::mouse::ScrollDelta) -> (i32, i32) {
    match delta {
        iced::mouse::ScrollDelta::Lines { x, y } => {
            ((x * 120.0).round() as i32, (y * 120.0).round() as i32)
        }
        iced::mouse::ScrollDelta::Pixels { x, y } => (x.round() as i32, y.round() as i32),
    }
}

/// Map the page cursor reported by CEF onto iced's OS cursor vocabulary.
pub(crate) fn cursor_interaction(cursor: BrowserCursor) -> iced::mouse::Interaction {
    use iced::mouse::Interaction;
    match cursor {
        BrowserCursor::Default => Interaction::None,
        BrowserCursor::Pointer => Interaction::Pointer,
        BrowserCursor::Text => Interaction::Text,
        BrowserCursor::Crosshair => Interaction::Crosshair,
        BrowserCursor::Move => Interaction::Move,
        BrowserCursor::Wait => Interaction::Wait,
        BrowserCursor::Progress => Interaction::Progress,
        BrowserCursor::Help => Interaction::Help,
        BrowserCursor::NotAllowed => Interaction::NotAllowed,
        BrowserCursor::ContextMenu => Interaction::ContextMenu,
        BrowserCursor::Copy => Interaction::Copy,
        BrowserCursor::Cell => Interaction::Cell,
        BrowserCursor::NoDrop => Interaction::NoDrop,
        BrowserCursor::Grab => Interaction::Grab,
        BrowserCursor::Grabbing => Interaction::Grabbing,
        BrowserCursor::ZoomIn => Interaction::ZoomIn,
        BrowserCursor::ZoomOut => Interaction::ZoomOut,
        BrowserCursor::ResizeHorizontal => Interaction::ResizingHorizontally,
        BrowserCursor::ResizeVertical => Interaction::ResizingVertically,
        BrowserCursor::ResizeNwSe => Interaction::ResizingDiagonallyDown,
        BrowserCursor::ResizeNeSw => Interaction::ResizingDiagonallyUp,
        BrowserCursor::ResizeColumn => Interaction::ResizingColumn,
        BrowserCursor::ResizeRow => Interaction::ResizingRow,
    }
}

/// Virtual cursor overlay across the page: a ring that morphs between the
/// free dot and the docked wrap. Position, size, and corner radius
/// interpolate every tick (smoothstepped blend), so docking animates circle
/// to square instead of popping.
fn browser_cursor_overlay<'a>(
    panel: &'a BrowserPanel,
    page_w: f32,
    page_h: f32,
) -> Element<'a, BrowserMessage> {
    if !cursor_draws(panel) {
        return space().width(Length::Fixed(0.0)).into();
    }
    let cursor = &panel.cursor;
    // 0 = free dot, 1 = docked wrap; smoothstepped so both ends ease.
    let t = cursor.dock_blend.clamp(0.0, 1.0);
    let t = t * t * (3.0 - 2.0 * t);
    // Dot geometry in page px.
    let (dx, dy) = (cursor.x - 10.0, cursor.y - 10.0);
    let wrap = cursor.attached_rect(&panel.focusables).or(cursor.last_dock);
    let (wx, wy, ww, wh) = match wrap {
        Some((x, y, w, h)) => (x - 2.0, y - 2.0, (w + 4.0).max(8.0), (h + 4.0).max(8.0)),
        None => (dx, dy, 20.0, 20.0),
    };
    let (ix, iy) = (dx + (wx - dx) * t, dy + (wy - dy) * t);
    let (iw, ih) = (20.0 + (ww - 20.0) * t, 20.0 + (wh - 20.0) * t);
    let (x, y) = cursor_card_point(ix, iy);
    let radius = 10.0 + (crate::ui::theme::radius::SM - 10.0) * t;
    let morph: Element<'_, BrowserMessage> = column![
        space().height(Length::Fixed(y.max(0.0))),
        row![
            space().width(Length::Fixed(x.max(0.0))),
            container(space().width(Length::Fixed(iw)).height(Length::Fixed(ih)))
                .style(move |theme| crate::ui::theme::cursor_morph(theme, radius)),
        ],
    ]
    .spacing(0)
    .into();
    // F10 coverage view: one thin ring per page snap target, colored by
    // kind. Draws exactly the rectangle's intersection with the card (the
    // truthful pixels); off-screen targets skip.
    let body: Element<'_, BrowserMessage> = if panel.debug_overlay {
        let mut overlay = stack![morph]
            .width(Length::Fixed(page_w))
            .height(Length::Fixed(page_h));
        for f in &panel.focusables {
            let (bx, by) = (f.x, f.y);
            let (bw, bh) = (f.w.max(3.0), f.h.max(3.0));
            let (ox, oy) = (bx.max(0.0), by.max(0.0));
            let (ex, ey) = ((bx + bw).min(page_w), (by + bh).min(page_h));
            if ex <= ox || ey <= oy {
                continue;
            }
            let (bw, bh) = (ex - ox, ey - oy);
            let kind = f.kind;
            overlay = overlay.push(
                column![
                    space().height(Length::Fixed(oy)),
                    row![
                        space().width(Length::Fixed(ox)),
                        container(space().width(Length::Fixed(bw)).height(Length::Fixed(bh)))
                            .style(move |theme| crate::ui::theme::debug_target(theme, kind)),
                    ],
                ]
                .spacing(0),
            );
        }
        overlay.into()
    } else {
        morph
    };
    container(body)
        .width(Length::Fixed(page_w))
        .height(Length::Fixed(page_h))
        .clip(true)
        .into()
}

/// Page surface: live engine pixels when a frame is present, text otherwise.
/// The engine image is a fixed size (`page_w`) so the virtual cursor maps 1:1;
/// pad scroll routes to the engine in that mode, so no scrollable wraps the
/// image.
pub fn browser_page_view(
    panel: &BrowserPanel,
    engine_available: bool,
    page_w: f32,
    page_h: f32,
) -> Element<'_, BrowserMessage> {
    let session = &panel.session;
    if session.has_frame() {
        // One stable GPU texture per tab, uploaded on unseen generations
        // only. Never mint a per-frame image handle: the atlas cache misses
        // by construction and the page blinks at large viewports.
        let page = iced::widget::shader(
            crate::ui::launcher::browser_frame::BrowserFrameProgram::new(
                std::sync::Arc::clone(session.frame_slot()),
                panel.active_tab.0,
            ),
        )
        .width(Length::Fixed(page_w))
        .height(Length::Fixed(page_h));
        // Hardware pointer path: page-local coordinates go straight to the
        // engine. Replay injects the same handlers with explicit points, so
        // automation never needs the OS cursor.
        return mouse_area(page)
            .on_move(|point| BrowserMessage::PagePointerMoved {
                x: point.x,
                y: point.y,
            })
            .on_press(BrowserMessage::PagePress)
            .on_release(BrowserMessage::PageRelease)
            .on_exit(BrowserMessage::PagePointerLeft)
            .on_scroll(|delta| {
                let (dx, dy) = scroll_delta_units(delta);
                BrowserMessage::PageWheel { dx, dy }
            })
            .interaction(cursor_interaction(panel.page_cursor))
            .into();
    }
    let body_text = match session.error() {
        Some(err) => format!("Page couldn't load: {err}"),
        None if !session.loading() && !session.has_content() => {
            if engine_available {
                "Rendering…".to_string()
            } else {
                "Browser engine not installed. Install it from Home.".to_string()
            }
        }
        None => String::new(),
    };
    scrollable(
        text(body_text)
            .size(13.0)
            .color(theme::INK)
            .wrapping(Wrapping::Word)
            .width(Fill),
    )
    .id(browser_scroll_id())
    .on_scroll(|viewport| BrowserMessage::Scrolled(viewport.absolute_offset().y))
    .height(Length::Fill)
    .into()
}

/// Browser surface: one 1:1 page card. The caller sizes the page to 16:9;
/// the engine viewport is resized to the same numbers, so cursor coordinates
/// are card pixels.
pub fn browser_view<'a>(
    panel: &'a BrowserPanel,
    engine_available: bool,
    page_w: f32,
    page_h: f32,
) -> Element<'a, BrowserMessage> {
    stack![
        browser_page_view(panel, engine_available, page_w, page_h),
        browser_cursor_overlay(panel, page_w, page_h),
    ]
    .width(Length::Fixed(page_w))
    .height(Length::Fixed(page_h))
    .clip(true)
    .into()
}

/// Standalone Browser card geometry: 16:9, height-driven, with a 50px
/// breather per side (window height - 100). The page fills the card.
pub fn fullscreen_layout(stage_w: f32, stage_h: f32) -> (f32, f32) {
    let layout = crate::ui::browser::state::browser_layout(stage_w, stage_h);
    (layout.page_w, layout.page_h)
}

/// Standalone Browser window root. Fills the window with the page surface
/// (opaque background; no Launcher stage behind it).
pub fn root_view(
    panel: &BrowserPanel,
    engine_available: bool,
    stage_w: f32,
    stage_h: f32,
    backdrop_alpha: f32,
) -> Element<'_, BrowserMessage> {
    let (page_w, page_h) = fullscreen_layout(stage_w, stage_h);
    let layout = crate::ui::browser::state::browser_layout(stage_w, stage_h);
    let card = container(browser_view(panel, engine_available, page_w, page_h))
        .padding(crate::ui::browser::state::BROWSER_CARD_PAD)
        .style(theme::menu_panel)
        .width(Length::Fixed(layout.card_w))
        .height(Length::Fixed(layout.card_h));
    let transparent = crate::ui::chrome::transparent_windows();
    let root: Element<'_, BrowserMessage> = container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .style(move |_theme| container::Style {
            // Alpha-capable backends dim the whole screen behind the card;
            // opaque backends keep the flat dark backdrop.
            background: Some(iced::Background::Color(if transparent {
                theme::alpha(iced::Color::BLACK, backdrop_alpha)
            } else {
                theme::BASE_BG
            })),
            text_color: Some(theme::INK),
            ..container::Style::default()
        })
        .into();
    // Controller mode owns the sole pointer (pad ring); a real mouse shows the
    // OS cursor with CEF's hover shape instead. Automated sessions never hide
    // the host cursor.
    let surface: Element<'_, BrowserMessage> = crate::ui::launcher::cursor_hide::force_cursor(
        root,
        !panel.page_mouse_active && !crate::platform::debug_quiet::input_dropped(),
    )
    .into();
    let card_x = (stage_w - layout.card_w) * 0.5;
    let card_y = (stage_h - layout.card_h) * 0.5;
    let (mouse_x, mouse_y) = panel.page_mouse;
    let cursor = iced::Rectangle::new(
        iced::Point::new(
            card_x + crate::ui::browser::state::BROWSER_CARD_PAD + mouse_x,
            card_y + crate::ui::browser::state::BROWSER_CARD_PAD + mouse_y,
        ),
        iced::Size::new(1.0, 1.0),
    );
    let ime_enabled = !crate::platform::debug_quiet::input_dropped() && panel.session.has_frame();
    crate::ui::input_method::ime_bridge(surface, ime_enabled, cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::browser::state::{BROWSER_CARD_PAD, BROWSER_OUTER_MARGIN, browser_layout};

    #[test]
    fn browser_layout_is_16_9_and_height_driven() {
        let l = browser_layout(1920.0, 1080.0);
        assert!(
            (l.card_w / l.card_h - 16.0 / 9.0).abs() < 0.01,
            "card must be 16:9 ({:.1}x{:.1})",
            l.card_w,
            l.card_h
        );
        assert_eq!(l.card_h, 980.0, "50px top/bottom breather (height - 100)");
        assert_eq!(l.card_w, l.page_w + 2.0 * BROWSER_CARD_PAD);
        assert_eq!(l.card_h, l.page_h + 2.0 * BROWSER_CARD_PAD);
        assert!(l.page_w > 1600.0, "height-driven width");
        // Narrow stage: width clamps and the card keeps 16:9.
        let n = browser_layout(800.0, 1200.0);
        assert!(n.card_w <= 800.0 - 2.0 * BROWSER_OUTER_MARGIN + 0.01);
        assert!((n.card_w / n.card_h - 16.0 / 9.0).abs() < 0.01);
    }

    #[test]
    fn page_replacement_keeps_live_cursor_bounds() {
        let mut panel = BrowserPanel::default();
        panel.set_viewport(1734.0, 860.0);
        panel.reset_live_page();
        assert_eq!(
            (panel.page.cursor.view_w, panel.page.cursor.view_h),
            (1734.0, 860.0)
        );

        // A stored tab's cursor may predate the live viewport; activating it
        // must re-sync rather than clamp to the page's 800x600 default.
        let id = crate::browser::engine::TabId(2);
        panel.order.push(id);
        panel.stored.push(crate::ui::browser::StoredTab {
            id,
            page: crate::ui::browser::BrowserTabPage::default(),
            loaded: false,
        });
        assert!(panel.activate_stored(id));
        assert_eq!(
            (panel.page.cursor.view_w, panel.page.cursor.view_h),
            (1734.0, 860.0)
        );

        // `stash_live` leaves a default placeholder page behind.
        panel.stash_live();
        assert_eq!(
            (panel.page.cursor.view_w, panel.page.cursor.view_h),
            (1734.0, 860.0)
        );
    }

    #[test]
    fn wheel_lines_use_the_cef_120_unit_scale() {
        assert_eq!(
            scroll_delta_units(iced::mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 }),
            (0, -120)
        );
        assert_eq!(
            scroll_delta_units(iced::mouse::ScrollDelta::Lines { x: 1.0, y: 0.0 }),
            (120, 0)
        );
        assert_eq!(
            scroll_delta_units(iced::mouse::ScrollDelta::Pixels { x: 0.0, y: -12.4 }),
            (0, -12)
        );
    }

    #[test]
    fn page_cursor_shapes_map_to_os_interactions() {
        use iced::mouse::Interaction;
        assert_eq!(
            cursor_interaction(BrowserCursor::Default),
            Interaction::None
        );
        assert_eq!(
            cursor_interaction(BrowserCursor::Pointer),
            Interaction::Pointer
        );
        assert_eq!(cursor_interaction(BrowserCursor::Text), Interaction::Text);
        assert_eq!(
            cursor_interaction(BrowserCursor::ResizeNwSe),
            Interaction::ResizingDiagonallyDown
        );
        assert_eq!(
            cursor_interaction(BrowserCursor::ResizeNeSw),
            Interaction::ResizingDiagonallyUp
        );
    }

    #[test]
    fn cursor_overlay_is_page_space_only() {
        let mut panel = BrowserPanel::default();
        panel.cursor.y = 0.0;
        assert!(cursor_draws(&panel), "free page cursor is drawn");
        panel.page_mouse_active = true;
        assert!(!cursor_draws(&panel), "physical mouse hides the pad ring");
        panel.page_mouse_active = false;
        assert!(cursor_draws(&panel));
        assert_eq!(cursor_card_point(400.0, 0.0), (400.0, 0.0));
        assert_eq!(cursor_card_point(400.0, 600.0), (400.0, 600.0));
        assert_eq!(BROWSER_SCROLL_STEP, 240.0);
    }
}
