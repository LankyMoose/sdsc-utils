//! Standalone Browser window rendering.
//!
//! Moved out of the Launcher view in pivot phase 2. The panel state lives in
//! `ui::browser::state`; this module owns the chrome/page/cursor composition
//! and the browser-facing message vocabulary.

use crate::ui::browser::state::{
    BrowserPanel, CHROME_BOOKMARK_H, CHROME_BOOKMARKS_Y, CHROME_HEADER_H, CHROME_ROW_GAP,
    CHROME_ROW_H, CHROME_STRIP_Y, CHROME_TOOLBAR_Y, ChromeTarget, cursor_targets,
};
use crate::ui::text_fit;
use crate::ui::theme;
use iced::widget::text::Wrapping;
use iced::widget::{
    button, column, container, image, row, scrollable, space, stack, text, text_input,
};
use iced::{Alignment, Element, Fill, Length};

/// Everything the standalone Browser window can ask the shell/engine to do.
#[derive(Debug, Clone)]
pub enum BrowserMessage {
    /// Circle/Esc at the root: suspend the engine and close the window.
    Close,
    UrlChanged(String),
    ToggleUrlEntry,
    Navigate,
    AutocompletePick(String),
    Back,
    Forward,
    HistoryOpen {
        back: bool,
    },
    HistoryPick(usize),
    HistoryCancel,
    OpenExternal,
    Reload,
    DefaultPage,
    TabNew,
    TabNext,
    TabSwitch(crate::browser::engine::TabId),
    TabClose(crate::browser::engine::TabId),
    BookmarkStar,
    BookmarkNameChanged(String),
    BookmarkSave,
    BookmarkRemove,
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
            Self::Close => Some("browser.close"),
            Self::UrlChanged(_) | Self::BookmarkNameChanged(_) => None,
            Self::ToggleUrlEntry => Some("browser.url.edit"),
            Self::Navigate | Self::AutocompletePick(_) => Some("browser.navigate"),
            Self::Back => Some("browser.history.back"),
            Self::Forward => Some("browser.history.forward"),
            Self::HistoryOpen { .. } => Some("browser.history.open"),
            Self::HistoryPick(_) => Some("browser.history.pick"),
            Self::HistoryCancel => Some("browser.history.cancel"),
            Self::OpenExternal => Some("browser.external"),
            Self::Reload => Some("browser.reload"),
            Self::DefaultPage => Some("browser.default_page"),
            Self::TabNew => Some("browser.tab.new"),
            Self::TabNext => Some("browser.tab.next"),
            Self::TabSwitch(_) => Some("browser.tab.switch"),
            Self::TabClose(_) => Some("browser.tab.close"),
            Self::BookmarkStar => Some("browser.bookmark.star"),
            Self::BookmarkSave => Some("browser.bookmark.save"),
            Self::BookmarkRemove => Some("browser.bookmark.remove"),
            Self::Scrolled(_) => None,
            #[cfg(debug_assertions)]
            Self::Shot(_) => None,
        }
    }
}

impl BrowserPanel {
    /// Pad action for a docked chrome target. `None` when the target needs
    /// data that vanished (removed bookmark) — the caller exits to the
    /// viewport instead.
    pub fn chrome_message(&self, target: ChromeTarget) -> Option<BrowserMessage> {
        match target {
            ChromeTarget::Tab(id) => Some(BrowserMessage::TabSwitch(id)),
            ChromeTarget::TabClose(id) => Some(BrowserMessage::TabClose(id)),
            ChromeTarget::NewTab => Some(BrowserMessage::TabNew),
            ChromeTarget::TabOverflow(_) => Some(BrowserMessage::TabNext),
            ChromeTarget::Back => Some(BrowserMessage::Back),
            ChromeTarget::Forward => Some(BrowserMessage::Forward),
            ChromeTarget::Reload => Some(BrowserMessage::Reload),
            ChromeTarget::DefaultPage => Some(BrowserMessage::DefaultPage),
            // The address field owns the keyboard; pad users get there via
            // Triangle today, cursor dock now too.
            ChromeTarget::Omnibox => Some(BrowserMessage::ToggleUrlEntry),
            ChromeTarget::Star => Some(BrowserMessage::BookmarkStar),
            ChromeTarget::Go => Some(BrowserMessage::Navigate),
            ChromeTarget::Bookmark(i) => self
                .bookmarks
                .get(i)
                .map(|b| BrowserMessage::AutocompletePick(b.url.clone())),
        }
    }
}

/// Scrollable id for the browser body (pad scroll + `on_scroll` reports).
pub fn browser_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("browser-body")
}

/// Address field id (URL-edit mode focuses it so typing lands in the box,
/// not the page).
pub fn browser_url_id() -> iced::widget::Id {
    iced::widget::Id::new("browser-url")
}

/// Pad Up/Down body scroll step.
pub const BROWSER_SCROLL_STEP: f32 = 240.0;

/// Whether the pad cursor should be drawn at all. URL entry owns the
/// keyboard, and a chrome dock is carried by the control chip; free roam
/// and flights draw everywhere on the surface, header included.
fn cursor_draws(panel: &BrowserPanel) -> bool {
    !panel.url_editing && panel.chrome_attached().is_none()
}

/// Map a unified cursor point (page px; header negative) to card pixels.
/// The surface is 1:1: the engine viewport is resized to the page area.
fn cursor_card_point(x: f32, y: f32) -> (f32, f32) {
    (x, CHROME_HEADER_H + y)
}

/// Virtual cursor overlay across the whole browser surface (header plus
/// page): a ring that morphs between the free dot and the docked wrap.
/// Position, size, and corner radius interpolate every tick
/// (smoothstepped blend), so docking animates circle to square instead of
/// popping.
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
    let targets = cursor_targets(&panel.focusables, &panel.chrome);
    let wrap = cursor.attached_rect(&targets).or(cursor.last_dock);
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
    let surface_h = CHROME_HEADER_H + page_h;
    // F10 coverage view: one thin ring per page snap target, colored by
    // kind. Draws exactly the rectangle's intersection with the card (the
    // truthful pixels); off-screen targets skip.
    let body: Element<'_, BrowserMessage> = if panel.debug_overlay {
        let mut overlay = stack![morph]
            .width(Length::Fixed(page_w))
            .height(Length::Fixed(surface_h));
        for f in &panel.focusables {
            let (bx, by) = (f.x, CHROME_HEADER_H + f.y);
            let (bw, bh) = (f.w.max(3.0), f.h.max(3.0));
            let (ox, oy) = (bx.max(0.0), by.max(0.0));
            let (ex, ey) = ((bx + bw).min(page_w), (by + bh).min(surface_h));
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
        .height(Length::Fixed(surface_h))
        .clip(true)
        .into()
}

/// Page surface: live engine pixels when a frame is present, reader text
/// otherwise. The engine image is a fixed size (`img_w` wide) so the
/// virtual cursor maps 1:1 with a uniform scale; pad scroll routes to the
/// engine in that mode, so no scrollable wraps the image.
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
        return iced::widget::shader(
            crate::ui::launcher::browser_frame::BrowserFrameProgram::new(
                std::sync::Arc::clone(session.frame_slot()),
                panel.active_tab.0,
            ),
        )
        .width(Length::Fixed(page_w))
        .height(Length::Fixed(page_h))
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

/// Omnibox autocomplete dropdown: in-memory history matches for the draft.
/// Shows only once the user typed something diverging from the current URL
/// (post-navigate the draft mirrors the URL — no dropdown then). Rendered
/// as a `stack!` overlay layer over the page, so appearing never shifts
/// layout. Rows are plain buttons so the pad cursor reaches them like every
/// other target; dpad-through-the-list is a later refinement.
fn browser_suggestions(panel: &BrowserPanel) -> Element<'_, BrowserMessage> {
    let draft = panel.draft.trim();
    if draft.is_empty()
        || draft == panel.session.url()
        || panel.bookmark_bubble
        || panel.history_selector.is_some()
    {
        return space().height(Length::Fixed(0.0)).into();
    }
    let matches = panel.session.suggestions(draft);
    if matches.is_empty() {
        return space().height(Length::Fixed(0.0)).into();
    }
    let mut list = column![].spacing(2);
    for m in matches {
        let title = m.title.clone().unwrap_or_else(|| m.url.clone());
        list = list.push(
            button(
                column![
                    text(text_fit::ellipsize_to_width(&title, 13.0, 420.0))
                        .size(13.0)
                        .color(theme::INK),
                    text(text_fit::ellipsize_to_width(&m.url, 11.0, 480.0))
                        .size(11.0)
                        .color(theme::MUTED),
                ]
                .spacing(0),
            )
            .padding([6, 10])
            .width(Fill)
            .on_press(BrowserMessage::AutocompletePick(m.url.clone()))
            .style(theme::ghost),
        );
    }
    container(list)
        .padding([4, 0])
        .width(Fill)
        .style(theme::menu_panel)
        .into()
}

/// Address field id for the star-bubble rename box.
pub fn bookmark_name_id() -> iced::widget::Id {
    iced::widget::Id::new("bookmark-name")
}

/// Position one header control inside its fixed-height row layer.
fn chrome_at<'a>(
    x: f32,
    w: f32,
    h: f32,
    content: Element<'a, BrowserMessage>,
) -> Element<'a, BrowserMessage> {
    row![
        space().width(Length::Fixed(x)),
        container(content)
            .width(Length::Fixed(w))
            .center_y(Length::Fixed(h)),
    ]
    .height(Length::Fixed(h))
    .into()
}

/// One header row rendered from the same rects the pad cursor uses. Every
/// control keeps its natural width (the layout computed that width), so
/// the drawn cursor, the dock outline and the real widget line up. Rows
/// are fixed-height; controls center vertically inside them.
fn chrome_row_view<'a>(
    panel: &'a BrowserPanel,
    row_y: f32,
    h: f32,
    width: f32,
) -> Element<'a, BrowserMessage> {
    let session = &panel.session;
    let mut layer = stack![]
        .width(Length::Fixed(width))
        .height(Length::Fixed(h));
    for (target, f) in panel.chrome.iter().filter(|(_, f)| f.y == row_y) {
        let content: Element<'a, BrowserMessage> = match *target {
            ChromeTarget::Tab(id) => {
                let url = panel.tab_url(id);
                let icon = if url.is_empty() {
                    None
                } else {
                    panel
                        .favicons
                        .get(&crate::browser::bookmarks::origin_key(&url))
                        .cloned()
                };
                let icon_w = if icon.is_some() { 22.0 } else { 0.0 };
                let budget = (f.w - 16.0 - icon_w).max(0.0);
                let title = if budget >= 8.0 {
                    text_fit::ellipsize_to_width(panel.tab_title(id), 11.0, budget)
                } else {
                    String::new()
                };
                let mut inner = row![].spacing(6).align_y(Alignment::Center);
                if let Some(icon) = icon {
                    inner = inner.push(
                        image(icon)
                            .width(Length::Fixed(16.0))
                            .height(Length::Fixed(16.0)),
                    );
                }
                if !title.is_empty() {
                    inner = inner.push(text(title).size(11.0).color(theme::INK));
                }
                let b = button(inner).padding([4, 8]);
                if panel.chrome_attached() == Some(ChromeTarget::Tab(id)) {
                    b.on_press(BrowserMessage::TabSwitch(id))
                        .style(theme::chip(true))
                } else if id == panel.active_tab {
                    b.style(theme::primary)
                } else {
                    b.on_press(BrowserMessage::TabSwitch(id))
                        .style(theme::ghost)
                }
                .into()
            }
            ChromeTarget::TabClose(id) => {
                let b = button(text("×").size(11.0).color(theme::MUTED)).padding([4, 6]);
                if panel.chrome_attached() == Some(ChromeTarget::TabClose(id)) {
                    b.on_press(BrowserMessage::TabClose(id))
                        .style(theme::chip(true))
                } else {
                    b.on_press(BrowserMessage::TabClose(id)).style(theme::ghost)
                }
                .into()
            }
            ChromeTarget::NewTab => {
                let b = button(text("+").size(14.0)).padding([4, 10]);
                if panel.chrome_attached() == Some(ChromeTarget::NewTab) {
                    b.on_press(BrowserMessage::TabNew).style(theme::chip(true))
                } else {
                    b.on_press(BrowserMessage::TabNew).style(theme::ghost)
                }
                .into()
            }
            ChromeTarget::TabOverflow(hidden) => {
                let b = button(text(format!("+{hidden}")).size(11.0).color(theme::MUTED))
                    .padding([4, 10]);
                if panel.chrome_attached() == Some(ChromeTarget::TabOverflow(hidden)) {
                    b.on_press(BrowserMessage::TabNext).style(theme::chip(true))
                } else {
                    b.on_press(BrowserMessage::TabNext).style(theme::ghost)
                }
                .into()
            }
            ChromeTarget::Back => {
                let b = button(text("←").size(14.0)).padding([8, 12]);
                if panel.chrome_attached() == Some(ChromeTarget::Back) {
                    b.on_press(BrowserMessage::Back).style(theme::chip(true))
                } else if panel.can_go_back() {
                    b.on_press(BrowserMessage::Back).style(theme::ghost)
                } else {
                    b.style(theme::ghost)
                }
                .into()
            }
            ChromeTarget::Forward => {
                let b = button(text("→").size(14.0)).padding([8, 12]);
                if panel.chrome_attached() == Some(ChromeTarget::Forward) {
                    b.on_press(BrowserMessage::Forward).style(theme::chip(true))
                } else if panel.can_go_forward() {
                    b.on_press(BrowserMessage::Forward).style(theme::ghost)
                } else {
                    b.style(theme::ghost)
                }
                .into()
            }
            ChromeTarget::Reload => {
                let b = button(text("⟳").size(14.0)).padding([8, 12]);
                if panel.chrome_attached() == Some(ChromeTarget::Reload) {
                    b.on_press(BrowserMessage::Reload).style(theme::chip(true))
                } else {
                    b.on_press(BrowserMessage::Reload).style(theme::ghost)
                }
                .into()
            }
            ChromeTarget::DefaultPage => {
                let b = button(text("⌂").size(14.0)).padding([8, 12]);
                if panel.chrome_attached() == Some(ChromeTarget::DefaultPage) {
                    b.on_press(BrowserMessage::DefaultPage)
                        .style(theme::chip(true))
                } else {
                    b.on_press(BrowserMessage::DefaultPage).style(theme::ghost)
                }
                .into()
            }
            ChromeTarget::Omnibox => text_input("Search or enter URL", &panel.draft)
                .id(browser_url_id())
                .size(14.0)
                .padding(8)
                .on_input(BrowserMessage::UrlChanged)
                .on_submit(BrowserMessage::Navigate)
                .style(if panel.chrome_attached() == Some(ChromeTarget::Omnibox) {
                    theme::input_docked
                } else {
                    theme::input
                })
                .into(),
            ChromeTarget::Star => {
                let label = if panel.is_bookmarked(session.url()) {
                    "★"
                } else {
                    "☆"
                };
                let b = button(text(label).size(16.0)).padding([8, 10]);
                if panel.chrome_attached() == Some(ChromeTarget::Star) {
                    b.on_press(BrowserMessage::BookmarkStar)
                        .style(theme::chip(true))
                } else {
                    b.on_press(BrowserMessage::BookmarkStar).style(theme::ghost)
                }
                .into()
            }
            ChromeTarget::Go => {
                let b = button(text("Go").size(14.0)).padding([8, 14]);
                if panel.chrome_attached() == Some(ChromeTarget::Go) {
                    b.on_press(BrowserMessage::Navigate)
                        .style(theme::chip(true))
                } else {
                    b.on_press(BrowserMessage::Navigate).style(theme::primary)
                }
                .into()
            }
            ChromeTarget::Bookmark(slot) => {
                let Some(b) = panel.bookmarks.get(slot) else {
                    continue;
                };
                let mut inner = row![].spacing(6).align_y(Alignment::Center);
                let icon_key = crate::browser::bookmarks::origin_key(&b.url);
                if let Some(icon) = panel.favicons.get(&icon_key) {
                    inner = inner.push(
                        image(icon.clone())
                            .width(Length::Fixed(16.0))
                            .height(Length::Fixed(16.0)),
                    );
                }
                inner = inner.push(
                    text(text_fit::ellipsize_to_width(b.name(), 13.0, 170.0))
                        .size(13.0)
                        .color(theme::INK),
                );
                let btn = button(inner).padding([4, 10]);
                if panel.chrome_attached() == Some(ChromeTarget::Bookmark(slot)) {
                    btn.on_press(BrowserMessage::AutocompletePick(b.url.clone()))
                        .style(theme::chip(true))
                } else {
                    btn.on_press(BrowserMessage::AutocompletePick(b.url.clone()))
                        .style(theme::ghost)
                }
                .into()
            }
        };
        layer = layer.push(chrome_at(f.x, f.w, h, content));
    }
    layer.into()
}

/// Star bubble overlay (stack layer, top-right under the toolbar): rename
/// the current page's bookmark or remove it. Done applies the draft.
fn browser_bookmark_bubble(panel: &BrowserPanel) -> Element<'_, BrowserMessage> {
    if !panel.bookmark_bubble {
        return space().height(Length::Fixed(0.0)).into();
    }
    row![
        space().width(Fill),
        container(
            column![
                text("Bookmark").size(13.0).color(theme::INK),
                text_input("Name", &panel.bookmark_name_draft)
                    .id(bookmark_name_id())
                    .size(13.0)
                    .padding(6)
                    .on_input(BrowserMessage::BookmarkNameChanged)
                    .on_submit(BrowserMessage::BookmarkSave)
                    .style(theme::input),
                row![
                    button(text("Remove").size(13.0))
                        .padding([6, 12])
                        .on_press(BrowserMessage::BookmarkRemove)
                        .style(theme::ghost),
                    button(text("Done").size(13.0))
                        .padding([6, 12])
                        .on_press(BrowserMessage::BookmarkSave)
                        .style(theme::primary),
                ]
                .spacing(8),
            ]
            .spacing(6),
        )
        .padding(10)
        .width(Length::Fixed(300.0))
        .style(theme::menu_panel),
    ]
    .width(Fill)
    .into()
}

/// Hold-bumper history selector (B2.4): directional dropdown over the
/// session stack (closest first, `highlight` = quick-press default until
/// the pad moves it). Current page heads the list (marked, not a target);
/// rows carry favicons + titles like the bookmark bar. Rendered as a
/// `stack!` overlay layer so appearing never shifts layout; hidden unless
/// the selector is open with entries in that direction.
fn browser_history_selector(panel: &BrowserPanel) -> Element<'_, BrowserMessage> {
    let Some(sel) = &panel.history_selector else {
        return space().height(Length::Fixed(0.0)).into();
    };
    let session = &panel.session;
    let entries = if sel.back {
        session.back_entries()
    } else {
        session.forward_entries()
    };
    if entries.is_empty() {
        return space().height(Length::Fixed(0.0)).into();
    }
    let current_title = session.title().unwrap_or_else(|| session.url()).to_string();
    let header = if sel.back { "Back" } else { "Forward" };
    let mut list = column![
        text(format!(
            "{header} — {}",
            text_fit::ellipsize_to_width(&current_title, 12.0, 460.0)
        ))
        .size(12.0)
        .color(theme::MUTED),
    ]
    .spacing(2);
    for (row, (abs, entry)) in entries.iter().enumerate() {
        let title = entry.title.clone().unwrap_or_else(|| entry.url.clone());
        let mut inner = row![].spacing(6).align_y(Alignment::Center);
        let icon_key = crate::browser::bookmarks::origin_key(&entry.url);
        if let Some(icon) = panel.favicons.get(&icon_key) {
            inner = inner.push(
                image(icon.clone())
                    .width(Length::Fixed(16.0))
                    .height(Length::Fixed(16.0)),
            );
        }
        inner = inner.push(
            column![
                text(text_fit::ellipsize_to_width(&title, 13.0, 420.0))
                    .size(13.0)
                    .color(theme::INK),
                text(text_fit::ellipsize_to_width(&entry.url, 11.0, 480.0))
                    .size(11.0)
                    .color(theme::MUTED),
            ]
            .spacing(0),
        );
        if row == sel.highlight.min(entries.len() - 1) {
            inner = inner.push(text("◀").size(13.0).color(theme::INK));
        }
        let abs = *abs;
        list = list.push(
            button(inner)
                .padding([6, 10])
                .width(Fill)
                .on_press(BrowserMessage::HistoryPick(abs))
                .style(theme::ghost),
        );
    }
    list = list.push(
        button(text("Cancel").size(13.0))
            .padding([6, 12])
            .on_press(BrowserMessage::HistoryCancel)
            .style(theme::ghost),
    );
    container(list.spacing(2))
        .padding([4, 0])
        .width(Fill)
        .style(theme::menu_panel)
        .into()
}

/// Hovered link target, if the cursor sits on a link with a URL: the
/// Chrome-style status-bar URL. Viewport-space point against the translated
/// tree; first id-order match wins (stable across pulls).
fn browser_hover_link(panel: &BrowserPanel) -> Option<String> {
    // Chrome-docked: the cursor is parked in the header; page hover is stale.
    if panel.chrome_attached().is_some() {
        return None;
    }
    let (x, y) = (panel.cursor.x, panel.cursor.y);
    panel
        .focusables
        .iter()
        .find(|o| {
            o.kind == crate::browser::cursor::FocusKind::Link
                && x >= o.x
                && x <= o.x + o.w
                && y >= o.y
                && y <= o.y + o.h
        })
        .and_then(|o| o.url.clone())
}

/// Status text: hovered-link URL wins (Chrome-style), else the live engine
/// phase, else nothing (the overlay hides — it never takes layout space).
fn browser_status_text(panel: &BrowserPanel) -> Option<String> {
    if let Some(url) = browser_hover_link(panel) {
        return Some(url);
    }
    let activity = panel.engine_activity.label();
    if activity.is_empty() {
        None
    } else {
        Some(activity.to_string())
    }
}

/// Status overlay: small black (translucent) bar pinned to the bottom of
/// the viewport, Chrome-style. Anchored opposite the cursor's half so it
/// never covers what you are pointing at (a docked cursor sits at its
/// element's center, so this also clears the wrap). Hidden entirely when
/// there is no status; as a `stack!` layer it never shifts layout.
fn browser_status_overlay(panel: &BrowserPanel) -> Element<'_, BrowserMessage> {
    let Some(status) = browser_status_text(panel) else {
        return space().height(Length::Fixed(0.0)).into();
    };
    let chip = container(
        text(text_fit::ellipsize_to_width(&status, 11.0, 480.0))
            .size(11.0)
            .color(theme::INK),
    )
    .padding([4, 10])
    .style(theme::status_overlay);
    // Cursor on the left half: bar goes right, and vice versa.
    let bar = if panel.cursor.x < crate::browser::cursor::VIEWPORT_W / 2.0 {
        row![space().width(Fill), chip]
    } else {
        row![chip, space().width(Fill)]
    };
    column![space().height(Fill), bar].into()
}

/// Browser surface: one 1:1 card containing the fixed header and
/// the page area. The caller sizes the page to 16:9; the engine viewport is
/// resized to the same numbers, so cursor coordinates are card pixels.
pub fn browser_view<'a>(
    panel: &'a BrowserPanel,
    engine_available: bool,
    page_w: f32,
    page_h: f32,
) -> Element<'a, BrowserMessage> {
    let base = column![
        chrome_row_view(panel, CHROME_STRIP_Y, CHROME_ROW_H, page_w),
        space().height(CHROME_ROW_GAP),
        chrome_row_view(panel, CHROME_TOOLBAR_Y, CHROME_ROW_H, page_w),
        space().height(CHROME_ROW_GAP),
        chrome_row_view(panel, CHROME_BOOKMARKS_Y, CHROME_BOOKMARK_H, page_w),
        space().height(CHROME_ROW_GAP),
        // Fixed-position dropdowns: the stack sizes to its base layer, so
        // suggestions, the star bubble, the history selector, and the status
        // overlay appearing never shove content down. The bubble wins over
        // suggestions (it owns the keyboard); the selector wins over both
        // (hold claims the pad until release/Cross/Circle resolves it). The
        // status bar floats bottom-left over the page, Chrome-style.
        stack![
            browser_page_view(panel, engine_available, page_w, page_h),
            browser_suggestions(panel),
            browser_bookmark_bubble(panel),
            browser_history_selector(panel),
            browser_status_overlay(panel),
        ],
    ];
    stack![base, browser_cursor_overlay(panel, page_w, page_h)]
        .width(Length::Fixed(page_w))
        .height(Length::Fixed(CHROME_HEADER_H + page_h))
        .clip(true)
        .into()
}

/// Standalone Browser card geometry: 16:9, height-driven, with a 50px
/// breather per side (window height - 100). The fixed tab/toolbar/bookmark
/// header is inside the card; only the page area is returned.
pub fn fullscreen_layout(stage_w: f32, stage_h: f32) -> (f32, f32) {
    let layout = crate::ui::browser::state::browser_layout(stage_w, stage_h);
    (layout.page_w, layout.page_h)
}

/// Standalone Browser window root. Fills the window with the browser surface
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
    container(card)
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
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::engine::TabId;
    use crate::ui::browser::state::{
        BROWSER_CARD_PAD, BROWSER_OUTER_MARGIN, BrowserTabPage, StoredTab, browser_layout,
        chrome_layout,
    };
    use std::time::{Duration, Instant};

    fn docked_panel_on(target: ChromeTarget) -> BrowserPanel {
        let mut panel = BrowserPanel::default();
        panel.rebuild_chrome();
        let (_, f) = panel
            .chrome
            .iter()
            .find(|(t, _)| *t == target)
            .expect("chrome slot exists");
        panel.page.cursor.x = f.x + f.w / 2.0;
        panel.page.cursor.y = f.y + f.h / 2.0;
        panel.page.cursor.attached = Some(f.id);
        panel
    }

    /// Tick neutral samples until the current jump flight lands, so the
    /// post-jump dock state is observable (docking is arrival-based).
    fn fly_until_dock(panel: &mut BrowserPanel) {
        let mut now = Instant::now();
        for _ in 0..240 {
            now += Duration::from_millis(16);
            panel.advance_stick(0.0, 0.0, 0.0, 0.0, false, now);
            if panel.page.cursor.fly_to.is_none() {
                break;
            }
        }
        assert!(
            panel.page.cursor.fly_to.is_none(),
            "flight never arrived (cursor stuck at ({:.0},{:.0}))",
            panel.page.cursor.x,
            panel.page.cursor.y
        );
    }

    #[test]
    fn page_left_never_enters_the_header() {
        // R3 trace: Left from a wide page link at y=339 flew to the Back
        // button at y=-54 because the 60-degree cone accepted it by 3px.
        // Page horizontals must never see chrome; header entry is Up/Down
        // (or the analog stick).
        use crate::browser::cursor::{FocusKind, Focusable};
        let mut panel = BrowserPanel::default();
        panel.page.focusables = vec![Focusable {
            id: 2,
            x: 32.0,
            y: 309.0,
            w: 851.0,
            h: 60.0,
            kind: FocusKind::Link,
            url: None,
        }];
        panel.rebuild_chrome();
        panel.page.cursor.x = 458.0;
        panel.page.cursor.y = 339.0;
        panel.page.cursor.attached = Some(2);
        panel.jump_cursor(crate::browser::cursor::JumpDir::Left);
        assert!(panel.chrome_attached().is_none());
        assert!(panel.page.cursor.fly_to.is_none());
        assert_eq!(
            panel.page.cursor.attached,
            Some(2),
            "row end keeps the dock"
        );
    }

    #[test]
    fn page_right_prefers_the_same_row() {
        // A same-row link must win over a farther tile that merely shares
        // the row band, and neither may pull the point off its row.
        use crate::browser::cursor::{FocusKind, Focusable};
        let mut panel = BrowserPanel::default();
        panel.page.focusables = vec![
            Focusable {
                id: 2,
                x: 100.0,
                y: 300.0,
                w: 100.0,
                h: 40.0,
                kind: FocusKind::Link,
                url: None,
            },
            Focusable {
                id: 3,
                x: 300.0,
                y: 300.0,
                w: 100.0,
                h: 40.0,
                kind: FocusKind::Link,
                url: None,
            },
            Focusable {
                id: 4,
                x: 500.0,
                y: 50.0,
                w: 100.0,
                h: 360.0,
                kind: FocusKind::Link,
                url: None,
            },
        ];
        panel.rebuild_chrome();
        panel.page.cursor.x = 150.0;
        panel.page.cursor.y = 320.0;
        panel.page.cursor.attached = Some(2);
        panel.jump_cursor(crate::browser::cursor::JumpDir::Right);
        fly_until_dock(&mut panel);
        assert_eq!(panel.page.cursor.attached, Some(3));
        assert_eq!(panel.page.cursor.y, 320.0, "row y preserved");
    }

    #[test]
    fn header_left_stays_in_row() {
        // Left from the omnibox must land on the adjacent toolbar button,
        // never a strip slot two rows up.
        let mut panel = docked_panel_on(ChromeTarget::Omnibox);
        panel.jump_cursor(crate::browser::cursor::JumpDir::Left);
        fly_until_dock(&mut panel);
        assert_eq!(
            panel.chrome_attached(),
            Some(ChromeTarget::DefaultPage),
            "left of omnibox is the default-page button, same row"
        );
    }

    #[test]
    fn header_up_down_moves_one_row() {
        // Toolbar -> strip -> toolbar with the column preserved; never a
        // two-row skip in either direction.
        let mut panel = docked_panel_on(ChromeTarget::Omnibox);
        panel.jump_cursor(crate::browser::cursor::JumpDir::Up);
        fly_until_dock(&mut panel);
        let up = panel.chrome_attached().expect("docked in strip row");
        let (_, f) = panel
            .chrome
            .iter()
            .find(|(t, _)| *t == up)
            .expect("strip slot");
        assert_eq!(f.y, CHROME_STRIP_Y, "one row up, not skipped");
        panel.jump_cursor(crate::browser::cursor::JumpDir::Down);
        fly_until_dock(&mut panel);
        let back = panel.chrome_attached().expect("docked in toolbar row");
        let (_, f) = panel
            .chrome
            .iter()
            .find(|(t, _)| *t == back)
            .expect("toolbar slot");
        assert_eq!(f.y, CHROME_TOOLBAR_Y, "one row down, not skipped");
    }

    #[test]
    fn page_up_lands_on_toolbar_never_strip() {
        // Up from the page enters the header at the lowest row (toolbar),
        // never vaulting straight to the tab strip.
        let mut panel = BrowserPanel::default();
        panel.rebuild_chrome();
        panel.page.cursor.x = 400.0;
        panel.page.cursor.y = 300.0;
        panel.jump_cursor(crate::browser::cursor::JumpDir::Up);
        fly_until_dock(&mut panel);
        let got = panel.chrome_attached().expect("docked in header");
        let (_, f) = panel
            .chrome
            .iter()
            .find(|(t, _)| *t == got)
            .expect("header slot");
        assert_eq!(f.y, CHROME_TOOLBAR_Y);
        // Off-column entry still moves a row: a far-right point must land
        // on the toolbar's right edge, not refuse the cone and stay put.
        panel.page.cursor.attached = None;
        panel.page.cursor.x = 770.0;
        panel.page.cursor.y = 300.0;
        panel.jump_cursor(crate::browser::cursor::JumpDir::Up);
        fly_until_dock(&mut panel);
        assert_eq!(
            panel.chrome_attached(),
            Some(ChromeTarget::Go),
            "column preserved even when far off the omnibox"
        );
    }

    #[test]
    fn page_up_steps_to_page_target_before_header() {
        // Up on a populated page walks page elements first; chrome is only
        // reachable once the topmost page row has nothing above it.
        use crate::browser::cursor::{FocusKind, Focusable};
        let mut panel = BrowserPanel::default();
        panel.rebuild_chrome();
        panel.page.focusables = vec![Focusable {
            id: 2,
            x: 300.0,
            y: 80.0,
            w: 200.0,
            h: 40.0,
            kind: FocusKind::Link,
            url: None,
        }];
        panel.page.cursor.x = 400.0;
        panel.page.cursor.y = 300.0;
        panel.jump_cursor(crate::browser::cursor::JumpDir::Up);
        fly_until_dock(&mut panel);
        assert_eq!(panel.page.cursor.attached, Some(2), "page link above wins");
        assert_eq!(panel.chrome_attached(), None, "did not vault into chrome");
        // One more Up from the top page row reaches the toolbar.
        panel.jump_cursor(crate::browser::cursor::JumpDir::Up);
        fly_until_dock(&mut panel);
        let got = panel.chrome_attached().expect("docked in header");
        let (_, f) = panel
            .chrome
            .iter()
            .find(|(t, _)| *t == got)
            .expect("header slot");
        assert_eq!(f.y, CHROME_TOOLBAR_Y);
    }

    #[test]
    fn chrome_layout_matches_cursor_rects() {
        // Rendering and cursor physics must share one geometry: the
        // rebuilt chrome rects are exactly what chrome_layout produced.
        let mut panel = BrowserPanel::default();
        panel.rebuild_chrome();
        let layout = chrome_layout(&panel);
        let flat: Vec<_> = layout
            .iter()
            .flat_map(|r| r.items.iter().map(move |(t, x, w)| (*t, r.y, *x, *w)))
            .collect();
        assert_eq!(panel.chrome.len(), flat.len());
        for (i, (target, f)) in panel.chrome.iter().enumerate() {
            let (lt, ly, lx, lw) = flat[i];
            assert_eq!(*target, lt);
            assert_eq!(f.y, ly);
            assert!((f.x - lx).abs() < 0.01, "x {i}: {} vs {lx}", f.x);
            assert!((f.w - lw).abs() < 0.01, "w {i}: {} vs {lw}", f.w);
        }
        // Toolbar: natural controls plus the flexing omnibox fill 800px.
        let bar = &layout[1].items;
        let last = bar.last().expect("toolbar items");
        assert!(
            (last.1 + last.2 - crate::browser::cursor::VIEWPORT_W).abs() < 0.5,
            "toolbar must end at the viewport edge"
        );
        // A tab and its close button are adjacent, no gap.
        let tab = layout[0]
            .items
            .iter()
            .find(|(t, _, _)| matches!(t, ChromeTarget::Tab(_)))
            .expect("tab slot");
        let close = layout[0]
            .items
            .iter()
            .find(|(t, _, _)| matches!(t, ChromeTarget::TabClose(_)))
            .expect("close slot");
        assert!((tab.1 + tab.2 - close.1).abs() < 0.5);
        // Header rows sit in the negative cursor bands at their fixed ys.
        assert_eq!(layout[0].y, CHROME_STRIP_Y);
        assert_eq!(layout[1].y, CHROME_TOOLBAR_Y);
        assert_eq!(layout[2].y, CHROME_BOOKMARKS_Y);
        assert_eq!(CHROME_HEADER_H, 112.0);
    }

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
        assert_eq!(
            l.card_h,
            l.page_h + CHROME_HEADER_H + 2.0 * BROWSER_CARD_PAD
        );
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
        let id = TabId(2);
        panel.order.push(id);
        panel.stored.push(StoredTab {
            id,
            page: BrowserTabPage::default(),
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
    fn crowded_tabs_keep_a_minimum_width() {
        let mut panel = BrowserPanel::default();
        panel.set_viewport(900.0, 500.0);
        let title = "A very long page title that needs the overflow chip";
        panel
            .page
            .session
            .begin_load("https://example.com/".to_string());
        panel.page.session.note_title(Some(title.to_string()));
        panel.order.clear();
        panel.order.push(TabId(10));
        panel.active_tab = TabId(10);
        for i in 1..12 {
            let id = TabId(10 + i);
            let mut page = BrowserTabPage::default();
            page.session.note_url(format!("https://example.com/{i}"));
            page.session.note_title(Some(title.to_string()));
            panel.stored.push(StoredTab {
                id,
                page,
                loaded: false,
            });
            panel.order.push(id);
        }
        panel.rebuild_chrome();
        let visible = panel
            .chrome
            .iter()
            .filter(|(t, _)| matches!(t, ChromeTarget::Tab(_)))
            .count();
        assert!(visible > 0 && visible < 12, "strip should overflow");
        for (target, f) in &panel.chrome {
            if matches!(target, ChromeTarget::Tab(_)) {
                // 16px tab padding + 72px title budget + 6px slack.
                assert!(f.w >= 94.0, "tab {target:?} collapsed to {:.0}px", f.w);
            }
        }
        assert!(
            panel
                .chrome
                .iter()
                .any(|(t, _)| matches!(t, ChromeTarget::TabOverflow(_))),
            "crowded strip needs an overflow chip"
        );
    }

    #[test]
    fn one_long_tab_gets_a_wide_title() {
        use crate::browser::engine::TabId;
        let mut panel = BrowserPanel::default();
        panel.set_viewport(1920.0, 968.0);
        panel
            .page
            .session
            .begin_load("https://example.com/".to_string());
        panel.page.session.note_title(Some(
            "A very long page title that must not collapse to four characters".to_string(),
        ));
        panel.rebuild_chrome();
        let f = panel
            .chrome
            .iter()
            .find(|(t, _)| matches!(t, ChromeTarget::Tab(TabId::BOOT)))
            .map(|(_, f)| f)
            .expect("tab slot");
        assert!(
            f.w > 260.0,
            "long lone tab should get real width, got {:.0}",
            f.w
        );
    }

    #[test]
    fn precise_header_point_hits_the_rect() {
        let mut panel = BrowserPanel::default();
        panel.set_viewport(1920.0, 968.0);
        panel.rebuild_chrome();
        // Tab, tab-close, and toolbar controls all live in the negative
        // header band; a magnet-off precise click must hit each rect.
        for target in [
            ChromeTarget::Tab(TabId::BOOT),
            ChromeTarget::TabClose(TabId::BOOT),
            ChromeTarget::Back,
            ChromeTarget::Omnibox,
        ] {
            let f = panel
                .chrome
                .iter()
                .find(|(t, _)| *t == target)
                .map(|(_, f)| f.clone())
                .unwrap_or_else(|| panic!("missing chrome slot {target:?}"));
            assert!(
                f.y + f.h <= 0.0,
                "{target:?} must sit in the header band, got y={}..{}",
                f.y,
                f.y + f.h
            );
            assert_eq!(
                panel.chrome_target_at(f.x + f.w / 2.0, f.y + f.h / 2.0),
                Some(target)
            );
        }
        assert_eq!(panel.chrome_target_at(-1.0, -80.0), None);
    }

    #[test]
    fn tab_favicon_widens_the_slot() {
        let mut panel = BrowserPanel::default();
        panel.page.session.begin_load("https://a.test/".to_string());
        panel.rebuild_chrome();
        let tab_width = |panel: &BrowserPanel| {
            panel
                .chrome
                .iter()
                .find_map(|(t, f)| matches!(t, ChromeTarget::Tab(TabId::BOOT)).then_some(f.w))
                .expect("tab slot")
        };
        let before = tab_width(&panel);
        panel.note_favicon(
            "https://a.test/".to_string(),
            iced::widget::image::Handle::from_rgba(1, 1, vec![0, 0, 0, 255]),
        );
        let after = tab_width(&panel);
        assert!(
            (after - before - 22.0).abs() < 0.5,
            "favicon must add 16px + 6px spacing ({before} -> {after})"
        );
    }

    #[test]
    fn tab_strip_windows_and_counts_overflow() {
        let mut panel = BrowserPanel::default();
        // Twenty tabs with long titles cannot fit; the strip must window
        // around the active tab and expose the rest through the chip.
        panel.order.clear();
        panel.stored.clear();
        for i in 0..20u64 {
            let id = TabId(100 + i);
            let mut page = BrowserTabPage::default();
            page.session
                .begin_load(format!("https://site-{i}.test/a-long-page-title"));
            panel.order.push(id);
            panel.stored.push(StoredTab {
                id,
                page,
                loaded: false,
            });
        }
        panel.active_tab = TabId(115);
        panel.rebuild_chrome();
        let layout = chrome_layout(&panel);
        let strip = &layout[0].items;
        for (target, x, w) in strip {
            assert!(
                *x >= 0.0 && x + w <= crate::browser::cursor::VIEWPORT_W + 0.01,
                "{target:?} spills past the strip"
            );
        }
        let overflow = strip
            .iter()
            .find_map(|(t, _, _)| match t {
                ChromeTarget::TabOverflow(hidden) => Some(*hidden),
                _ => None,
            })
            .expect("overflow chip");
        assert!(
            strip
                .iter()
                .any(|(t, _, _)| *t == ChromeTarget::Tab(TabId(115))),
            "the active tab must stay in the visible window"
        );
        assert!(strip.iter().any(|(t, _, _)| *t == ChromeTarget::NewTab));
        let shown = strip
            .iter()
            .filter(|(t, _, _)| matches!(t, ChromeTarget::Tab(_)))
            .count();
        assert_eq!(shown + overflow, 20, "all tabs are accounted for");
        let laid_out: usize = layout.iter().map(|row| row.items.len()).sum();
        assert_eq!(panel.chrome.len(), laid_out);
    }

    #[test]
    fn header_cursor_is_drawn_in_unified_space() {
        // Header roam and header-bound flights are visible now: the overlay
        // spans the fixed header plus the page, so the dot is never drawn
        // clamped to the page edge.
        let mut panel = BrowserPanel::default();
        panel.page.cursor.y = -54.0;
        assert!(cursor_draws(&panel), "free header roam is drawn");
        panel.page.cursor.fly_to = Some((400.0, -94.0));
        assert!(cursor_draws(&panel), "header-to-header flight is drawn");
        panel.page.cursor.fly_to = Some((400.0, 200.0));
        assert!(cursor_draws(&panel), "header-to-page flight is drawn");
        // URL entry owns the cursor; chrome docks use the control chip.
        panel.url_editing = true;
        assert!(!cursor_draws(&panel));
        panel.url_editing = false;
        panel.page.cursor.attached = Some(crate::browser::cursor::CHROME_ID_BASE);
        assert!(!cursor_draws(&panel));

        // Unified -> card mapping: the surface is 1:1 (dynamic viewport).
        assert_eq!(cursor_card_point(400.0, -112.0), (400.0, 0.0));
        assert_eq!(cursor_card_point(400.0, -94.0), (400.0, 18.0));
        assert_eq!(cursor_card_point(400.0, -18.0), (400.0, 94.0));
        assert_eq!(cursor_card_point(400.0, 0.0), (400.0, 112.0));
        assert_eq!(cursor_card_point(400.0, 600.0), (400.0, 712.0));
    }

    #[test]
    fn tab_stash_activate_round_trips_live_page() {
        let mut panel = BrowserPanel::default();
        assert_eq!(panel.active_tab, TabId::BOOT);
        assert_eq!(panel.order, vec![TabId::BOOT]);
        // Live page belongs to boot: visiting a URL then stashing keeps it.
        panel.session.begin_load("https://a.test/".to_string());
        panel.stash_live();
        assert_eq!(panel.stored.len(), 1);
        assert_eq!(panel.stored[0].id, TabId::BOOT);
        assert_eq!(panel.stored[0].page.session.url(), "https://a.test/");
        // Stashed live pages keep their backing view.
        assert!(panel.stored[0].loaded);
        assert_eq!(panel.order, vec![TabId::BOOT]);
        // New tab id takes the live slot; switching back restores the page.
        let second = TabId(panel.next_tab_id);
        panel.next_tab_id += 1;
        panel.active_tab = second;
        panel.order.push(second);
        panel.session.begin_load("https://b.test/".to_string());
        assert!(panel.activate_stored(TabId::BOOT));
        assert_eq!(panel.active_tab, TabId::BOOT);
        assert_eq!(panel.session.url(), "https://a.test/");
        assert_eq!(panel.stored.len(), 1);
        assert_eq!(panel.stored[0].id, second);
        assert_eq!(panel.stored[0].page.session.url(), "https://b.test/");
        // Order never shuffles on switch.
        assert_eq!(panel.order, vec![TabId::BOOT, second]);
        // Unknown ids refuse.
        assert!(!panel.activate_stored(TabId(999)));
        assert!(!panel.drop_stored(TabId(999)));
        // Dropping the background tab leaves live untouched.
        assert!(panel.drop_stored(second));
        assert!(panel.stored.is_empty());
        assert_eq!(panel.active_tab, TabId::BOOT);
        assert!(!panel.drop_stored(TabId::BOOT));
    }

    #[test]
    fn chrome_ids_stable_while_set_unchanged() {
        let mut panel = BrowserPanel::default();
        panel.rebuild_chrome();
        let first: Vec<(ChromeTarget, u64)> =
            panel.chrome.iter().map(|(t, f)| (*t, f.id)).collect();
        assert!(!first.is_empty());
        // Same set, new bookmark-less rebuild: ids persist.
        panel.rebuild_chrome();
        let second: Vec<(ChromeTarget, u64)> =
            panel.chrome.iter().map(|(t, f)| (*t, f.id)).collect();
        assert_eq!(first, second);
        // Rects live above the viewport, within the roam band.
        for (_, f) in &panel.chrome {
            assert!(f.y < 0.0);
            assert!(f.y >= crate::browser::cursor::CURSOR_MIN_Y);
        }
        // New tab shifts slots: same targets keep their ids (dock
        // attachments survive), the new tab gets a fresh one.
        panel.order.push(TabId(9));
        panel.rebuild_chrome();
        for (t, id) in &first {
            assert_eq!(panel.chrome_target(*id), Some(*t));
        }
        let tab_id = panel
            .chrome
            .iter()
            .find(|(t, _)| *t == ChromeTarget::Tab(TabId(9)))
            .map(|(_, f)| f.id);
        assert!(tab_id.is_some());
        assert!(panel.chrome_target(tab_id.unwrap()) == Some(ChromeTarget::Tab(TabId(9))));
        // Unknown id resolves to nothing (engine page ids are small).
        assert_eq!(panel.chrome_target(42), None);
    }

    #[test]
    fn chrome_message_maps_targets_to_start_messages() {
        let panel = BrowserPanel::default();
        assert!(matches!(
            panel.chrome_message(ChromeTarget::NewTab),
            Some(BrowserMessage::TabNew)
        ));
        assert!(matches!(
            panel.chrome_message(ChromeTarget::Tab(TabId::BOOT)),
            Some(BrowserMessage::TabSwitch(id)) if id == TabId::BOOT
        ));
        assert!(matches!(
            panel.chrome_message(ChromeTarget::TabOverflow(3)),
            Some(BrowserMessage::TabNext)
        ));
        assert!(matches!(
            panel.chrome_message(ChromeTarget::Omnibox),
            Some(BrowserMessage::ToggleUrlEntry)
        ));
        // Unknown bookmark index: no message (caller exits to viewport).
        assert!(panel.chrome_message(ChromeTarget::Bookmark(0)).is_none());
    }

    #[test]
    fn tab_strip_snapshot_round_trips_order_and_active() {
        let mut panel = BrowserPanel {
            order: vec![TabId::BOOT, TabId(2), TabId(3)],
            active_tab: TabId(2),
            ..BrowserPanel::default()
        };
        panel.page.session.begin_load("https://b.test/".to_string());
        panel.page.session.note_title(Some("B".to_string()));
        panel.stored.push(StoredTab {
            id: TabId::BOOT,
            loaded: true,
            page: {
                let mut p = BrowserTabPage::default();
                p.session.begin_load("https://a.test/".to_string());
                p
            },
        });
        panel.stored.push(StoredTab {
            id: TabId(3),
            // Unloaded (restored, never foregrounded) tabs snapshot the
            // same as loaded ones.
            loaded: false,
            page: {
                let mut p = BrowserTabPage::default();
                p.session.begin_load("https://c.test/".to_string());
                p.session.note_title(Some("C".to_string()));
                p
            },
        });
        let snap = panel.snapshot_tabs();
        assert_eq!(
            snap.tabs.iter().map(|t| t.url.as_str()).collect::<Vec<_>>(),
            vec!["https://a.test/", "https://b.test/", "https://c.test/"]
        );
        assert_eq!(snap.tabs[1].title, "B");
        assert_eq!(snap.active, 1);
        // Blank/empty pages never persist.
        panel
            .page
            .session
            .note_url(crate::browser::BLANK_PAGE_URL.to_string());
        let snap = panel.snapshot_tabs();
        assert_eq!(snap.tabs.len(), 2);
    }

    #[test]
    fn tab_title_falls_back_to_url_then_home() {
        let mut panel = BrowserPanel::default();
        // Fresh boot tab: the default page.
        assert_eq!(
            panel.tab_title(TabId::BOOT),
            crate::browser::BROWSER_DEFAULT_URL
        );
        panel.session.begin_load("https://a.test/page".to_string());
        assert_eq!(panel.tab_title(TabId::BOOT), "https://a.test/page");
        panel.session.note_title(Some("A page".to_string()));
        assert_eq!(panel.tab_title(TabId::BOOT), "A page");
        // Unknown id: empty (no strip slot renders it).
        assert!(panel.tab_title(TabId(999)).is_empty());
    }
}
