//! Tabs view: Share/Create opens a 4x3 card grid over the Browser card, with a
//! leading plus card, a bookmark strip, and a footer hint island.
//!
//! The transition is a FLIP over the existing per-tab `FrameSlot` texture: the
//! engine viewport is never resized, so the page content and aspect ratio stay
//! exact while it shrinks/grows between the full page and a card.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::browser::engine::TabId;
use crate::browser::frame_slot::FrameSlot;
use crate::ui::browser::view::BrowserMessage;
use crate::ui::browser::{BrowserPanel, display_url};
use crate::ui::face::{ActionHint, FaceButton, action_cluster_spaced};
use crate::ui::launcher::browser_frame::BrowserFrameProgram;
use crate::ui::{svg_icon, text_fit, theme};
use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{self, Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Renderer as _, Shell, overlay, renderer};
use iced::widget::text::Wrapping;
use iced::widget::{container, mouse_area, row, scrollable, space, stack, svg, text};
use iced::{
    Alignment, Background, Element, Event, Fill, Length, Padding, Point, Rectangle, Size,
    Transformation, Vector, mouse, window,
};

/// Enter/exit duration. Long enough to read the FLIP, short enough to stay out
/// of the way of the next input.
pub const GRID_ANIM_MS: u64 = 320;
const GRID_ANIM: Duration = Duration::from_millis(GRID_ANIM_MS);

#[cfg(debug_assertions)]
static GRID_TICK_FRAMES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
#[cfg(debug_assertions)]
static GRID_FRAME_STATS: std::sync::Mutex<GridFrameStats> = std::sync::Mutex::new(GridFrameStats {
    last: None,
    max_gap: Duration::ZERO,
});

#[cfg(debug_assertions)]
struct GridFrameStats {
    last: Option<Instant>,
    max_gap: Duration,
}

/// Debug: consume the number of presented FLIP frames + worst frame gap.
#[cfg(debug_assertions)]
pub fn take_grid_anim_frames() -> (u32, u128) {
    let max_gap = GRID_FRAME_STATS
        .lock()
        .map(|stats| stats.max_gap.as_millis())
        .unwrap_or(0);
    (
        GRID_TICK_FRAMES.swap(0, std::sync::atomic::Ordering::Relaxed),
        max_gap,
    )
}

#[cfg(debug_assertions)]
fn note_grid_frame(now: Instant) {
    if let Ok(mut stats) = GRID_FRAME_STATS.lock() {
        if let Some(last) = stats.last {
            stats.max_gap = stats.max_gap.max(now.saturating_duration_since(last));
        }
        stats.last = Some(now);
    }
}

#[cfg(debug_assertions)]
pub(crate) fn reset_grid_anim_frames() {
    GRID_TICK_FRAMES.store(0, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut stats) = GRID_FRAME_STATS.lock() {
        stats.last = None;
        stats.max_gap = Duration::ZERO;
    }
}

const GRID_PAD: f32 = 18.0;
const GRID_GAP: f32 = 14.0;
/// Compact tabs card width as a fraction of the page-mode card width.
pub const TABS_CARD_SCALE: f32 = 0.70;
/// Cards visible across the strip at the default width.
pub const TABS_VISIBLE: usize = 4;
/// Keep cards touchable on narrow stages; the strip scrolls instead.
const TABS_MIN_CARD_W: f32 = 150.0;
const TABS_ISLAND_H: f32 = 44.0;
const TABS_ISLAND_GAP: f32 = 10.0;
/// Reserved horizontal-scrollbar lane below the card row. Always present so
/// showing/hiding the scrollbar never reflows the cards or bookmark strip.
const TABS_SCROLLBAR_LANE: f32 = 9.0;
const TABS_SCROLLBAR_THICKNESS: f32 = 6.0;
pub const BOOKMARK_ROW_H: f32 = 88.0;
const BOOKMARK_GAP: f32 = 10.0;
pub const BOOKMARK_CARD_W: f32 = 168.0;
const BOOKMARK_CARD_H: f32 = 58.0;
const CARD_TITLE_H: f32 = 34.0;
/// Header enter/exit slide distance (positive = down from its resting row).
const HEADER_SLIDE_PX: f32 = 22.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GridMode {
    #[default]
    Page,
    Entering,
    Grid,
    Leaving,
}

/// Focus identity. Tab ids survive reorders and tab closes; bookmark indices
/// are stable as long as the bookmark list is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridFocus {
    Tab(TabId),
    Plus,
    Bookmark(usize),
}

#[derive(Debug, Clone, Copy)]
pub struct GridAnim {
    pub generation: u64,
    pub started: Instant,
    /// Tab whose page texture rides the FLIP.
    pub tab: TabId,
    pub entering: bool,
    /// Card the page morphs to (entering) or from (leaving).
    pub card: GridFocus,
}

impl GridAnim {
    pub fn progress(&self, now: Instant) -> f32 {
        let t = (now.saturating_duration_since(self.started).as_secs_f32()
            / GRID_ANIM.as_secs_f32())
        .clamp(0.0, 1.0);
        if self.entering {
            crate::ui::motion::ease_out_cubic(t)
        } else {
            crate::ui::motion::ease_in_out_cubic(t)
        }
    }

    pub fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= GRID_ANIM
    }
}

pub fn lerp_rect(a: Rectangle, b: Rectangle, t: f32) -> Rectangle {
    Rectangle::new(
        iced::Point::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t),
        Size::new(
            a.width + (b.width - a.width) * t,
            a.height + (b.height - a.height) * t,
        ),
    )
}

/// Card shell rect at `now`: full page card in page mode, compact in tabs,
/// and a synchronized lerp during either FLIP.
pub fn card_rect_at(
    mode: GridMode,
    anim: Option<GridAnim>,
    page: Rectangle,
    tabs: Rectangle,
    now: Instant,
) -> Rectangle {
    match mode {
        GridMode::Page => page,
        GridMode::Grid => tabs,
        GridMode::Entering => anim
            .map(|anim| lerp_rect(page, tabs, anim.progress(now)))
            .unwrap_or(tabs),
        GridMode::Leaving => anim
            .map(|anim| lerp_rect(tabs, page, anim.progress(now)))
            .unwrap_or(page),
    }
}

/// Scale + translation that maps `source` exactly onto `target`. Both rects
/// are in the same (absolute) coordinate space; scaling about the screen origin
/// would pin the page's y near the top and snap it on the final frame.
fn flip_transform(source: Rectangle, target: Rectangle) -> (f32, f32, f32) {
    let scale = target.width / source.width.max(1.0);
    (
        scale,
        target.x - source.x * scale,
        target.y - source.y * scale,
    )
}

#[derive(Debug, Clone)]
pub struct GridState {
    pub mode: GridMode,
    pub focus: GridFocus,
    /// Frozen strip order for this grid session. Entering a tab never
    /// reorders it.
    pub visual: Vec<TabId>,
    /// Tracked horizontal strip offset (scrollable reports it).
    pub tab_scroll_x: f32,
    pub bookmark_scroll_x: f32,
    /// Row card to restore when leaving the bookmark strip (Up).
    pub row_focus: GridFocus,
    pub anim: Option<GridAnim>,
    pub anim_generation: u64,
}

impl Default for GridState {
    fn default() -> Self {
        Self {
            mode: GridMode::Page,
            focus: GridFocus::Plus,
            visual: Vec::new(),
            tab_scroll_x: 0.0,
            bookmark_scroll_x: 0.0,
            row_focus: GridFocus::Plus,
            anim: None,
            anim_generation: 0,
        }
    }
}

impl GridState {
    pub fn is_grid(&self) -> bool {
        !matches!(self.mode, GridMode::Page)
    }

    pub fn animating(&self) -> bool {
        matches!(self.mode, GridMode::Entering | GridMode::Leaving)
    }

    pub fn focus_tab(&self) -> Option<TabId> {
        match self.focus {
            GridFocus::Tab(id) => Some(id),
            _ => None,
        }
    }

    pub fn next_generation(&mut self) -> u64 {
        self.anim_generation = self.anim_generation.wrapping_add(1);
        self.anim_generation
    }

    /// Rebuild the frozen strip newest-first. Entering a tab must not move
    /// it; the active tab only sets focus.
    pub fn refresh_visual(&mut self, active: TabId, order: &[TabId]) {
        self.visual.clear();
        // Newest first: the strip opens on the tab the user just created.
        for id in order.iter().rev() {
            if !self.visual.contains(id) {
                self.visual.push(*id);
            }
        }
        self.focus = if self.visual.contains(&active) {
            GridFocus::Tab(active)
        } else {
            GridFocus::Plus
        };
        self.tab_scroll_x = 0.0;
        self.bookmark_scroll_x = 0.0;
        self.row_focus = self.focus;
        self.anim = None;
    }
}

/// Grid focus after `closed` leaves the strip. Keeping the same index means
/// the next tab slides into the freed slot; closing the last card selects the
/// new last card. A different focused tab is untouched.
pub fn focus_after_close(visual: &[TabId], focus: GridFocus, closed: TabId) -> GridFocus {
    // The final tab is reset to the default page, not removed: keep its focus.
    if visual.iter().all(|id| *id == closed) {
        return focus;
    }
    let closed_index = visual.iter().position(|id| *id == closed);
    let remaining: Vec<TabId> = visual.iter().copied().filter(|id| *id != closed).collect();
    if focus == GridFocus::Tab(closed) {
        if remaining.is_empty() {
            GridFocus::Plus
        } else {
            let slot = closed_index.unwrap_or(0).min(remaining.len() - 1);
            GridFocus::Tab(remaining[slot])
        }
    } else if let GridFocus::Tab(id) = focus {
        if remaining.contains(&id) {
            focus
        } else {
            remaining
                .first()
                .copied()
                .map(GridFocus::Tab)
                .unwrap_or(GridFocus::Plus)
        }
    } else {
        focus
    }
}

/// One card's display data. Cloned from the panel so the view can own it
/// without borrowing per-tab page state.
#[derive(Debug, Clone)]
pub struct GridTab {
    pub id: TabId,
    pub title: String,
    pub url: String,
    pub bookmarked: bool,
    pub loaded: bool,
    pub slot: Option<Arc<FrameSlot>>,
    pub has_frame: bool,
}

/// Resolve the frozen visual order to card data. Unknown ids are skipped;
/// tabs not present in the frozen order are appended (defensive).
pub fn grid_tabs(panel: &BrowserPanel, order: &[TabId]) -> Vec<GridTab> {
    let mut ids: Vec<TabId> = order.to_vec();
    if ids.is_empty() {
        ids.push(panel.active_tab);
    }
    for id in &panel.order {
        if !ids.contains(id) {
            ids.push(*id);
        }
    }
    for tab in &panel.stored {
        if !ids.contains(&tab.id) {
            ids.push(tab.id);
        }
    }
    ids.into_iter()
        .filter_map(|id| {
            let page = panel.tab_page(id)?;
            let url = page.session.url().to_string();
            let title = page
                .session
                .title()
                .map(str::to_string)
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| display_url(&url));
            Some(GridTab {
                id,
                title,
                bookmarked: panel.is_bookmarked(&url),
                loaded: panel.tab_loaded(id),
                has_frame: page.session.has_frame(),
                slot: Some(Arc::clone(page.session.frame_slot())),
                url,
            })
        })
        .collect()
}

pub fn visual_index(tabs: &[GridTab], id: TabId) -> Option<usize> {
    tabs.iter().position(|tab| tab.id == id)
}

/// Horizontal-strip geometry, relative to the compact card interior.
#[derive(Debug, Clone, Copy)]
pub struct TabsLayout {
    pub area_w: f32,
    pub area_h: f32,
    pub row: Rectangle,
    pub bookmarks: Rectangle,
    pub island: Rectangle,
    pub card_w: f32,
    pub card_h: f32,
    pub gap: f32,
}

impl TabsLayout {
    /// Slot rect in content coordinates. Slot 0 is the plus card.
    pub fn card_rect(&self, slot: usize, scroll_x: f32) -> Rectangle {
        Rectangle::new(
            iced::Point::new(
                self.row.x - scroll_x + slot as f32 * (self.card_w + self.gap),
                self.row.y,
            ),
            Size::new(self.card_w, self.card_h),
        )
    }

    pub fn content_width(&self, count: usize) -> f32 {
        if count == 0 {
            0.0
        } else {
            count as f32 * (self.card_w + self.gap) - self.gap
        }
    }
}

/// Compact tabs card and its content area in stage coordinates.
#[derive(Debug, Clone, Copy)]
pub struct TabsCardGeometry {
    pub card: Rectangle,
    pub inner: Rectangle,
    pub layout: TabsLayout,
}

fn centered_rect(stage_w: f32, stage_h: f32, w: f32, h: f32) -> Rectangle {
    Rectangle::new(
        iced::Point::new(
            ((stage_w - w) * 0.5).max(0.0),
            ((stage_h - h) * 0.5).max(0.0),
        ),
        Size::new(w, h),
    )
}

/// Page-mode card rect (16:9, centered).
pub fn page_card_rect(stage_w: f32, stage_h: f32) -> Rectangle {
    let layout = crate::ui::browser::state::browser_layout(stage_w, stage_h);
    centered_rect(stage_w, stage_h, layout.card_w, layout.card_h)
}

/// Page-mode card interior (header + page), centered.
pub fn page_inner_rect(stage_w: f32, stage_h: f32) -> Rectangle {
    let layout = crate::ui::browser::state::browser_layout(stage_w, stage_h);
    let card = page_card_rect(stage_w, stage_h);
    Rectangle::new(
        iced::Point::new(
            card.x + crate::ui::browser::state::BROWSER_CARD_PAD,
            card.y + crate::ui::browser::state::BROWSER_CARD_PAD,
        ),
        Size::new(
            layout.page_w,
            layout.page_h + crate::ui::browser::state::BROWSER_HEADER_TOTAL,
        ),
    )
}

/// Page body rect (below the header) in stage coordinates.
pub fn page_body_rect(stage_w: f32, stage_h: f32) -> Rectangle {
    let layout = crate::ui::browser::state::browser_layout(stage_w, stage_h);
    let inner = page_inner_rect(stage_w, stage_h);
    Rectangle::new(
        iced::Point::new(
            inner.x,
            inner.y + crate::ui::browser::state::BROWSER_HEADER_TOTAL,
        ),
        Size::new(layout.page_w, layout.page_h),
    )
}

/// Single horizontal row of cards, bookmarks, and the footer island.
pub fn tabs_layout(area_w: f32, page_aspect: f32) -> TabsLayout {
    let area_w = area_w.max(1.0);
    let row_w = (area_w - 2.0 * GRID_PAD).max(1.0);
    let fit_w = (row_w - (TABS_VISIBLE.saturating_sub(1)) as f32 * GRID_GAP) / TABS_VISIBLE as f32;
    let card_w = fit_w.max(TABS_MIN_CARD_W);
    let aspect = page_aspect.max(0.1);
    let card_h = (card_w / aspect).max(1.0);
    let area_h = GRID_PAD
        + card_h
        + TABS_SCROLLBAR_LANE
        + GRID_GAP
        + BOOKMARK_ROW_H
        + TABS_ISLAND_GAP
        + TABS_ISLAND_H
        + GRID_PAD;
    let row = Rectangle::new(
        iced::Point::new(GRID_PAD, GRID_PAD),
        Size::new(row_w, card_h + TABS_SCROLLBAR_LANE),
    );
    let bookmarks = Rectangle::new(
        iced::Point::new(GRID_PAD, GRID_PAD + card_h + TABS_SCROLLBAR_LANE + GRID_GAP),
        Size::new(row_w, BOOKMARK_ROW_H),
    );
    let island = Rectangle::new(
        iced::Point::new(GRID_PAD, area_h - GRID_PAD - TABS_ISLAND_H),
        Size::new(row_w, TABS_ISLAND_H),
    );
    TabsLayout {
        area_w,
        area_h,
        row,
        bookmarks,
        island,
        card_w,
        card_h,
        gap: GRID_GAP,
    }
}

/// Compact tabs card geometry: 30% narrower than the page card, with the
/// content driving the height. The top inset equals the side inset.
pub fn tabs_card_geometry(stage_w: f32, stage_h: f32) -> TabsCardGeometry {
    let page = crate::ui::browser::state::browser_layout(stage_w, stage_h);
    let page_aspect = page.page_w / page.page_h.max(1.0);
    let card_w = (page.card_w * TABS_CARD_SCALE).max(1.0);
    let area_w = (card_w - 2.0 * crate::ui::browser::state::BROWSER_CARD_PAD).max(1.0);
    let layout = tabs_layout(area_w, page_aspect);
    let card_h = layout.area_h + 2.0 * crate::ui::browser::state::BROWSER_CARD_PAD;
    let card = centered_rect(stage_w, stage_h, card_w, card_h);
    let inner = Rectangle::new(
        iced::Point::new(
            card.x + crate::ui::browser::state::BROWSER_CARD_PAD,
            card.y + crate::ui::browser::state::BROWSER_CARD_PAD,
        ),
        Size::new(area_w, layout.area_h),
    );
    TabsCardGeometry {
        card,
        inner,
        layout,
    }
}

/// Scroll offset that keeps `slot` fully inside the row viewport.
pub fn reveal_offset(layout: &TabsLayout, slot: usize, count: usize, current: f32) -> f32 {
    let max = (layout.content_width(count) - layout.row.width).max(0.0);
    let current = current.clamp(0.0, max);
    let x = slot as f32 * (layout.card_w + layout.gap);
    if x < current {
        x
    } else if x + layout.card_w > current + layout.row.width {
        x + layout.card_w - layout.row.width
    } else {
        current
    }
    .clamp(0.0, max)
}

pub fn tabs_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("browser-tabs-scroll")
}

pub fn bookmark_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("browser-bookmark-scroll")
}

/// Content-space x of one bookmark chip.
pub fn bookmark_x(index: usize) -> f32 {
    index as f32 * (BOOKMARK_CARD_W + BOOKMARK_GAP)
}

/// Total content width of a bookmark strip.
pub fn bookmark_content_width(count: usize) -> f32 {
    if count == 0 {
        0.0
    } else {
        count as f32 * BOOKMARK_CARD_W + (count - 1) as f32 * BOOKMARK_GAP
    }
}

/// Tabs-view root: one horizontal card strip, bookmark strip, and footer
/// island. The compact card height is exactly the strip + bookmarks + island
/// plus the same inset on every side.
pub fn page_view<'a>(
    panel: &'a BrowserPanel,
    engine_available: bool,
    area_w: f32,
    page_aspect: f32,
    fade: f32,
) -> Element<'a, BrowserMessage> {
    let fade = fade.clamp(0.0, 1.0);
    let state = &panel.grid;
    let layout = tabs_layout(area_w, page_aspect);
    let anim_tab = state.anim.map(|anim| anim.tab);
    let tabs = grid_tabs(panel, &state.visual);
    let strip = tab_strip(
        &tabs,
        &layout,
        state.focus,
        engine_available,
        fade,
        anim_tab,
    );
    let row_layer = positioned(strip, layout.row, area_w, layout.area_h);
    let bookmarks = positioned(
        bookmark_strip(panel, state, layout.bookmarks.width, fade),
        layout.bookmarks,
        area_w,
        layout.area_h,
    );
    let island = positioned(island(fade), layout.island, area_w, layout.area_h);
    let body: Element<'a, BrowserMessage> = stack(vec![row_layer, bookmarks, island])
        .width(Length::Fixed(area_w))
        .height(Length::Fixed(layout.area_h))
        .clip(true)
        .into();
    ticker(body, state.anim.is_some())
}

fn tabs_scroll_style(theme: &iced::Theme, status: scrollable::Status) -> scrollable::Style {
    let mut style = scrollable::default(theme, status);
    let hovered = matches!(
        status,
        scrollable::Status::Hovered {
            is_horizontal_scrollbar_hovered: true,
            ..
        } | scrollable::Status::Dragged {
            is_horizontal_scrollbar_dragged: true,
            ..
        }
    );
    style.horizontal_rail = scrollable::Rail {
        background: Some(iced::Background::Color(theme::alpha(theme::BASE_BG, 0.55))),
        border: iced::Border {
            radius: 3.0.into(),
            ..iced::Border::default()
        },
        scroller: scrollable::Scroller {
            background: iced::Background::Color(theme::alpha(
                if hovered { theme::INK } else { theme::MUTED },
                0.72,
            )),
            border: iced::Border {
                radius: 3.0.into(),
                ..iced::Border::default()
            },
        },
    };
    style
}

fn tab_strip<'a>(
    tabs: &[GridTab],
    layout: &TabsLayout,
    focus: GridFocus,
    engine_available: bool,
    fade: f32,
    anim_tab: Option<TabId>,
) -> Element<'a, BrowserMessage> {
    // Slot 0 is ALWAYS the plus card; tabs follow in the frozen visual order.
    let mut cards: Vec<Element<'a, BrowserMessage>> = Vec::with_capacity(tabs.len() + 1);
    cards.push(plus_card(
        layout.card_w,
        layout.card_h,
        focus == GridFocus::Plus,
        fade,
    ));
    for tab in tabs {
        let focused = focus == GridFocus::Tab(tab.id);
        // The FLIP overlay owns this tab's texture; omit the card instance to
        // avoid two primitives sharing one tab's per-id uniform block.
        let show_frame = anim_tab != Some(tab.id);
        cards.push(tab_card(
            tab,
            layout.card_w,
            layout.card_h,
            focused,
            show_frame,
            engine_available,
            fade,
        ));
    }
    let content = row(cards)
        .spacing(layout.gap)
        .height(Length::Fixed(layout.card_h));
    scrollable(content)
        .id(tabs_scroll_id())
        .direction(scrollable::Direction::Horizontal(
            scrollable::Scrollbar::new()
                .width(TABS_SCROLLBAR_THICKNESS)
                .scroller_width(TABS_SCROLLBAR_THICKNESS)
                .margin(1.0),
        ))
        .style(tabs_scroll_style)
        .on_scroll(|viewport| BrowserMessage::TabsScrolled(viewport.absolute_offset().x))
        .width(Length::Fixed(layout.row.width))
        .height(Length::Fixed(layout.row.height))
        .into()
}

/// Place `content` at an absolute rect inside the tabs-view area.
pub fn positioned<'a>(
    content: impl Into<Element<'a, BrowserMessage>>,
    rect: Rectangle,
    area_w: f32,
    area_h: f32,
) -> Element<'a, BrowserMessage> {
    container(content.into())
        .width(Length::Fixed(area_w))
        .height(Length::Fixed(area_h))
        .padding(Padding {
            top: rect.y.max(0.0),
            right: 0.0,
            bottom: 0.0,
            left: rect.x.max(0.0),
        })
        .align_x(Alignment::Start)
        .align_y(Alignment::Start)
        .into()
}

fn island<'a>(fade: f32) -> Element<'a, BrowserMessage> {
    let fade = fade.clamp(0.0, 1.0);
    let hint =
        |glyph: Option<&'static str>, face: Option<FaceButton>, label: &'static str| ActionHint {
            face,
            text_glyph: glyph,
            label,
            hold: None,
            armed_t: 0.0,
            pressed: false,
            press_t: 0.0,
            alpha: fade,
        };
    let hints = vec![
        hint(None, Some(FaceButton::Cross), "Open"),
        hint(None, Some(FaceButton::Square), "Close"),
        hint(None, Some(FaceButton::Triangle), "Bookmark"),
        hint(None, Some(FaceButton::Circle), "Back"),
    ];
    let cluster = action_cluster_spaced(&hints, 20.0, true);
    let pill = container(cluster)
        .padding(Padding {
            top: 6.0,
            right: 18.0,
            bottom: 6.0,
            left: 18.0,
        })
        .style(theme::immersive_footer_pill_fade(fade));
    container(pill)
        .width(Fill)
        .height(Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .into()
}

pub fn transition_page<'a>(
    panel: &BrowserPanel,
    engine_available: bool,
    tab: TabId,
    page: Rectangle,
    fade: f32,
) -> Element<'a, BrowserMessage> {
    let page_ref = panel.tab_page(tab);
    let slot = page_ref
        .map(|page| Arc::clone(page.session.frame_slot()))
        .unwrap_or_else(|| Arc::clone(panel.session.frame_slot()));
    let inner: Element<'a, BrowserMessage> = if slot.has_frame() {
        iced::widget::shader(BrowserFrameProgram::new(slot, tab.0).opacity(fade.clamp(0.0, 1.0)))
            .width(Length::Fixed(page.width))
            .height(Length::Fixed(page.height))
            .into()
    } else {
        let label = if engine_available {
            "Loading…"
        } else {
            "Browser engine not installed"
        };
        container(
            text(label)
                .size(theme::type_scale::META)
                .color(theme::scale_alpha(theme::MUTED, fade))
                .wrapping(Wrapping::Word),
        )
        .width(Length::Fixed(page.width))
        .height(Length::Fixed(page.height))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(move |_theme| container::Style {
            background: Some(iced::Background::Color(theme::scale_alpha(
                theme::BASE_BG,
                fade,
            ))),
            ..container::Style::default()
        })
        .into()
    };
    container(inner)
        .width(Length::Fixed(page.width))
        .height(Length::Fixed(page.height))
        .style(theme::menu_panel_fade(fade))
        .into()
}

/// Draw-time FLIP wrapper. `content` is laid out once at the page rect and the
/// renderer transforms it to the current animated rect on every draw. Because
/// the rect is sampled inside `draw` (not when the view is built), the
/// transition gets a new pose for every presented frame.
struct FlipPage<'a> {
    content: Element<'a, BrowserMessage>,
    anim: GridAnim,
    /// Where the page content is laid out (stage coordinates).
    page: Rectangle,
    /// FLIP endpoints in stage coordinates; the shell around them resizes on
    /// the same clock.
    source: Rectangle,
    target: Rectangle,
    /// Shell endpoints for the draw-time clip around the moving page.
    mode: GridMode,
    page_card: Rectangle,
    tabs_card: Rectangle,
    area: Size,
}

#[allow(clippy::too_many_arguments)]
pub fn flip_page<'a>(
    content: Element<'a, BrowserMessage>,
    anim: GridAnim,
    page: Rectangle,
    source: Rectangle,
    target: Rectangle,
    mode: GridMode,
    page_card: Rectangle,
    tabs_card: Rectangle,
    area: Size,
) -> Element<'a, BrowserMessage> {
    FlipPage {
        content,
        anim,
        page,
        source,
        target,
        mode,
        page_card,
        tabs_card,
        area,
    }
    .into()
}

impl<'a> Widget<BrowserMessage, iced::Theme, iced::Renderer> for FlipPage<'a> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::stateless()
    }

    fn state(&self) -> tree::State {
        tree::State::None
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        Size::new(
            Length::Fixed(self.area.width),
            Length::Fixed(self.area.height),
        )
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        _limits: &layout::Limits,
    ) -> layout::Node {
        let child_limits = layout::Limits::new(self.page.size(), self.page.size());
        let child =
            self.content
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, &child_limits);
        layout::Node::with_children(self.area, vec![child.move_to(self.page.position())])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        let Some(child_layout) = layout.children().next() else {
            return;
        };
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            child_layout,
            renderer,
            operation,
        );
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, BrowserMessage>,
        viewport: &Rectangle,
    ) {
        let Some(child_layout) = layout.children().next() else {
            return;
        };
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            child_layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let Some(child_layout) = layout.children().next() else {
            return;
        };
        let now = Instant::now();
        let t = self.anim.progress(now);
        let current = lerp_rect(self.source, self.target, t);
        let clip = card_rect_at(
            self.mode,
            Some(self.anim),
            self.page_card,
            self.tabs_card,
            now,
        );
        // Map the child's laid-out page rect onto the current animated rect.
        // Both are stage-local, so the y offset interpolates with the card.
        let (scale, tx, ty) = flip_transform(child_layout.bounds(), current);
        renderer.with_layer(clip, |renderer| {
            renderer.with_transformation(Transformation::translate(tx, ty), |renderer| {
                renderer.with_transformation(Transformation::scale(scale), |renderer| {
                    self.content.as_widget().draw(
                        &tree.children[0],
                        renderer,
                        theme,
                        style,
                        child_layout,
                        cursor,
                        viewport,
                    );
                });
            });
        });
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        // Input is parked for the whole FLIP; do not expose transient hit
        // rectangles while the page is mid-flight.
        mouse::Interaction::None
    }

    fn overlay<'b>(
        &'b mut self,
        _tree: &'b mut Tree,
        _layout: Layout<'b>,
        _renderer: &iced::Renderer,
        _viewport: &Rectangle,
        _translation: Vector,
    ) -> Option<overlay::Element<'b, BrowserMessage, iced::Theme, iced::Renderer>> {
        None
    }
}

impl<'a> From<FlipPage<'a>> for Element<'a, BrowserMessage> {
    fn from(value: FlipPage<'a>) -> Self {
        Element::new(value)
    }
}

/// Draw-time header enter/exit curtain. The header slides up into place and
/// fades in while the page grows on a Leaving transition; entering reverses it
/// (slide down + fade out) on the same clock as the page shrink.
///
/// iced has no group-opacity primitive, so the fade is composited as a
/// card-background quad at `1 - appear` over the header. The header area sits
/// above every grid card, so the curtain never masks grid content.
struct HeaderCurtain<'a> {
    content: Element<'a, BrowserMessage>,
    anim: GridAnim,
    area: Size,
    header_h: f32,
    fade: f32,
    mode: GridMode,
    page_card: Rectangle,
    tabs_card: Rectangle,
}

#[allow(clippy::too_many_arguments)]
pub fn header_curtain<'a>(
    content: Element<'a, BrowserMessage>,
    anim: GridAnim,
    area: Size,
    header_h: f32,
    fade: f32,
    mode: GridMode,
    page_card: Rectangle,
    tabs_card: Rectangle,
) -> Element<'a, BrowserMessage> {
    HeaderCurtain {
        content,
        anim,
        area,
        header_h,
        fade: fade.clamp(0.0, 1.0),
        mode,
        page_card,
        tabs_card,
    }
    .into()
}

/// Entering the tabs view lifts the header out of its row as it fades; leaving
/// drops it back in from above. 0 = fully hidden, 1 = resting row.
fn header_offset(appear: f32) -> f32 {
    (appear.clamp(0.0, 1.0) - 1.0) * HEADER_SLIDE_PX
}

impl HeaderCurtain<'_> {
    fn appear(&self, now: Instant) -> f32 {
        let progress = self.anim.progress(now);
        if self.anim.entering {
            1.0 - progress
        } else {
            progress
        }
    }
}

impl<'a> Widget<BrowserMessage, iced::Theme, iced::Renderer> for HeaderCurtain<'a> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::stateless()
    }

    fn state(&self) -> tree::State {
        tree::State::None
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        Size::new(
            Length::Fixed(self.area.width),
            Length::Fixed(self.area.height),
        )
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        _limits: &layout::Limits,
    ) -> layout::Node {
        let child_limits = layout::Limits::new(
            Size::new(self.area.width, self.header_h),
            Size::new(self.area.width, self.header_h),
        );
        let child =
            self.content
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, &child_limits);
        layout::Node::with_children(self.area, vec![child.move_to(Point::ORIGIN)])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        let Some(child_layout) = layout.children().next() else {
            return;
        };
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            child_layout,
            renderer,
            operation,
        );
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let Some(child_layout) = layout.children().next() else {
            return;
        };
        let now = Instant::now();
        let appear = self.appear(now);
        if appear <= 0.001 || self.fade <= 0.001 {
            return;
        }
        let card = card_rect_at(
            self.mode,
            Some(self.anim),
            self.page_card,
            self.tabs_card,
            now,
        );
        let pad = crate::ui::browser::state::BROWSER_CARD_PAD;
        let target_w = (card.width - 2.0 * pad).max(1.0);
        let target = Rectangle::new(
            iced::Point::new(card.x + pad, card.y + pad + header_offset(appear)),
            Size::new(
                target_w,
                self.header_h * target_w / self.area.width.max(1.0),
            ),
        );
        let source = child_layout.bounds();
        let (scale, tx, ty) = flip_transform(source, target);
        // Scale the whole header with the card instead of clipping it: nested
        // widget layers would ignore a parent clip.
        renderer.with_transformation(Transformation::translate(tx, ty), |renderer| {
            renderer.with_transformation(Transformation::scale(scale), |renderer| {
                self.content.as_widget().draw(
                    &tree.children[0],
                    renderer,
                    theme,
                    style,
                    child_layout,
                    cursor,
                    viewport,
                );
            });
        });
        if appear < 0.999 {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: target,
                    ..renderer::Quad::default()
                },
                Background::Color(theme::alpha(
                    theme::lighten(theme::BASE_BG, 0.04),
                    (1.0 - appear * appear) * self.fade,
                )),
            );
        }
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        mouse::Interaction::None
    }

    fn overlay<'b>(
        &'b mut self,
        _tree: &'b mut Tree,
        _layout: Layout<'b>,
        _renderer: &iced::Renderer,
        _viewport: &Rectangle,
        _translation: Vector,
    ) -> Option<overlay::Element<'b, BrowserMessage, iced::Theme, iced::Renderer>> {
        None
    }
}

impl<'a> From<HeaderCurtain<'a>> for Element<'a, BrowserMessage> {
    fn from(value: HeaderCurtain<'a>) -> Self {
        Element::new(value)
    }
}

fn tab_card<'a>(
    tab: &GridTab,
    card_w: f32,
    card_h: f32,
    focused: bool,
    show_frame: bool,
    engine_available: bool,
    fade: f32,
) -> Element<'a, BrowserMessage> {
    let fade = fade.clamp(0.0, 1.0);
    let surface: Element<'a, BrowserMessage> = if show_frame
        && tab.has_frame
        && let Some(slot) = &tab.slot
    {
        iced::widget::shader(BrowserFrameProgram::new(Arc::clone(slot), tab.id.0).opacity(fade))
            .width(Length::Fixed(card_w))
            .height(Length::Fixed(card_h))
            .into()
    } else {
        let label = if !tab.loaded {
            display_url(&tab.url)
        } else if engine_available {
            tab.title.clone()
        } else {
            "Engine not installed".to_string()
        };
        container(
            text(text_fit::ellipsize_to_width(
                &label,
                theme::type_scale::META,
                card_w - 24.0,
            ))
            .size(theme::type_scale::META)
            .color(theme::scale_alpha(theme::DIM, fade))
            .wrapping(Wrapping::Word),
        )
        .width(Length::Fixed(card_w))
        .height(Length::Fixed(card_h))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(move |_theme| container::Style {
            background: Some(iced::Background::Color(theme::scale_alpha(
                theme::darken(theme::BASE_BG, 0.18),
                fade,
            ))),
            ..container::Style::default()
        })
        .into()
    };

    let star = if tab.bookmarked {
        svg_icon::STAR_FILLED_SVG
    } else {
        svg_icon::STAR_SVG
    };
    let star_color = if tab.bookmarked {
        theme::GOLD
    } else {
        theme::MUTED
    };
    let star: Element<'a, BrowserMessage> = svg(svg::Handle::from_memory(star.as_bytes()))
        .width(Length::Fixed(18.0))
        .height(Length::Fixed(18.0))
        .opacity(fade)
        .style(move |_theme, _status| svg::Style {
            color: Some(star_color),
        })
        .into();
    let star_badge = container(star)
        .padding(3)
        .style(move |_theme| container::Style {
            background: Some(iced::Background::Color(theme::scale_alpha(
                theme::alpha(theme::BASE_BG, 0.55),
                fade,
            ))),
            border: iced::Border {
                radius: theme::radius::PILL.into(),
                ..iced::Border::default()
            },
            ..container::Style::default()
        });
    let star_layer = container(star_badge)
        .width(Length::Fixed(card_w))
        .height(Length::Fixed(card_h))
        .padding(Padding {
            top: 8.0,
            right: 10.0,
            bottom: 0.0,
            left: 0.0,
        })
        .align_x(Alignment::End)
        .align_y(Alignment::Start);
    let focus_layer = container(space())
        .width(Length::Fixed(card_w))
        .height(Length::Fixed(card_h))
        .style(move |_theme| container::Style {
            border: iced::Border {
                color: theme::scale_alpha(if focused { theme::ACCENT } else { theme::LINE }, fade),
                width: if focused { 2.5 } else { 1.0 },
                radius: theme::radius::MD.into(),
            },
            ..container::Style::default()
        });
    let title_bar = container(
        text(text_fit::ellipsize_to_width(
            &tab.title,
            theme::type_scale::CAPTION,
            card_w - 20.0,
        ))
        .size(theme::type_scale::CAPTION)
        .color(theme::scale_alpha(theme::INK, fade))
        .wrapping(Wrapping::None),
    )
    .width(Length::Fixed(card_w))
    .height(Length::Fixed(CARD_TITLE_H))
    .padding(Padding {
        top: 0.0,
        right: 10.0,
        bottom: 0.0,
        left: 10.0,
    })
    .align_y(Alignment::Center)
    .style(move |_theme| container::Style {
        background: Some(iced::Background::Color(theme::scale_alpha(
            theme::alpha(theme::BASE_BG, 0.82),
            fade,
        ))),
        border: iced::Border {
            radius: iced::border::Radius {
                top_left: 0.0,
                top_right: 0.0,
                bottom_right: theme::radius::MD,
                bottom_left: theme::radius::MD,
            },
            ..iced::Border::default()
        },
        ..container::Style::default()
    });
    let title_layer = container(title_bar)
        .width(Length::Fixed(card_w))
        .height(Length::Fixed(card_h))
        .align_x(Alignment::Start)
        .align_y(Alignment::End);

    let body: Element<'a, BrowserMessage> = stack![surface, star_layer, title_layer, focus_layer]
        .width(Length::Fixed(card_w))
        .height(Length::Fixed(card_h))
        .clip(true)
        .into();
    let card = container(body)
        .width(Length::Fixed(card_w))
        .height(Length::Fixed(card_h))
        .style(move |_theme| grid_card_style(focused, fade, _theme));
    mouse_area(card)
        .on_press(BrowserMessage::GridActivate(tab.id))
        .interaction(mouse::Interaction::Pointer)
        .into()
}

fn plus_card<'a>(
    card_w: f32,
    card_h: f32,
    focused: bool,
    fade: f32,
) -> Element<'a, BrowserMessage> {
    let fade = fade.clamp(0.0, 1.0);
    let glyph: Element<'a, BrowserMessage> =
        svg(svg::Handle::from_memory(svg_icon::PLUS_SVG.as_bytes()))
            .width(Length::Fixed(34.0))
            .height(Length::Fixed(34.0))
            .opacity(fade)
            .style(|_theme, _status| svg::Style {
                color: Some(theme::ACCENT),
            })
            .into();
    let body = container(glyph)
        .width(Length::Fixed(card_w))
        .height(Length::Fixed(card_h))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(move |_theme| grid_card_style(focused, fade, _theme));
    mouse_area(body)
        .on_press(BrowserMessage::GridOpenPlus)
        .interaction(mouse::Interaction::Pointer)
        .into()
}

fn grid_card_style(focused: bool, fade: f32, _theme: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(iced::Background::Color(theme::scale_alpha(
            if focused {
                theme::PANEL_HOVER
            } else {
                theme::PANEL
            },
            fade,
        ))),
        border: iced::Border {
            color: theme::scale_alpha(if focused { theme::ACCENT } else { theme::LINE }, fade),
            width: if focused { 2.5 } else { 1.0 },
            radius: theme::radius::MD.into(),
        },
        ..container::Style::default()
    }
}

fn bookmark_strip<'a>(
    panel: &'a BrowserPanel,
    state: &GridState,
    width: f32,
    fade: f32,
) -> Element<'a, BrowserMessage> {
    let fade = fade.clamp(0.0, 1.0);
    let mut chips: Vec<Element<'a, BrowserMessage>> = Vec::new();
    for (index, bookmark) in panel.bookmarks.iter().enumerate() {
        let focused = state.focus == GridFocus::Bookmark(index);
        let name = text_fit::ellipsize_to_width(
            bookmark.name(),
            theme::type_scale::CAPTION,
            BOOKMARK_CARD_W - 36.0,
        );
        let content = row![
            svg(svg::Handle::from_memory(svg_icon::GLOBE_SVG.as_bytes()))
                .width(Length::Fixed(16.0))
                .height(Length::Fixed(16.0))
                .opacity(fade)
                .style(|_theme, _status| svg::Style {
                    color: Some(theme::MUTED),
                }),
            text(name)
                .size(theme::type_scale::CAPTION)
                .color(theme::scale_alpha(theme::INK, fade))
                .wrapping(Wrapping::None),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let chip = container(content)
            .width(Length::Fixed(BOOKMARK_CARD_W))
            .height(Length::Fixed(BOOKMARK_CARD_H))
            .padding(Padding {
                top: 0.0,
                right: 10.0,
                bottom: 0.0,
                left: 10.0,
            })
            .align_y(Alignment::Center)
            .style(move |_theme| container::Style {
                background: Some(iced::Background::Color(theme::scale_alpha(
                    if focused {
                        theme::PANEL_HOVER
                    } else {
                        theme::PANEL
                    },
                    fade,
                ))),
                border: iced::Border {
                    color: theme::scale_alpha(
                        if focused { theme::ACCENT } else { theme::LINE },
                        fade,
                    ),
                    width: if focused { 2.5 } else { 1.0 },
                    radius: theme::radius::SM.into(),
                },
                ..container::Style::default()
            });
        chips.push(
            mouse_area(chip)
                .on_press(BrowserMessage::GridOpenBookmark(index))
                .interaction(mouse::Interaction::Pointer)
                .into(),
        );
    }
    if chips.is_empty() {
        chips.push(
            container(
                text("No bookmarks yet")
                    .size(theme::type_scale::CAPTION)
                    .color(theme::scale_alpha(theme::DIM, fade)),
            )
            .height(Length::Fixed(BOOKMARK_CARD_H))
            .align_y(Alignment::Center)
            .into(),
        );
    }
    let strip = row(chips)
        .spacing(BOOKMARK_GAP)
        .width(Fill)
        .height(Fill)
        .align_y(Alignment::Center);
    scrollable(strip)
        .id(bookmark_scroll_id())
        .direction(scrollable::Direction::Horizontal(
            scrollable::Scrollbar::hidden(),
        ))
        .on_scroll(|viewport| BrowserMessage::BookmarksScrolled(viewport.absolute_offset().x))
        .width(Length::Fixed(width))
        .height(Length::Fixed(BOOKMARK_ROW_H))
        .into()
}

/// Draw-time card shell: the panel background/border/shadow follows the same
/// clock as the FLIP, without rebuilding the view each frame.
struct CardShell {
    mode: GridMode,
    anim: Option<GridAnim>,
    page: Rectangle,
    tabs: Rectangle,
    fade: f32,
    area: Size,
}

pub fn card_shell(
    mode: GridMode,
    anim: Option<GridAnim>,
    page: Rectangle,
    tabs: Rectangle,
    fade: f32,
    area: Size,
) -> Element<'static, BrowserMessage> {
    CardShell {
        mode,
        anim,
        page,
        tabs,
        fade,
        area,
    }
    .into()
}

impl Widget<BrowserMessage, iced::Theme, iced::Renderer> for CardShell {
    fn tag(&self) -> tree::Tag {
        tree::Tag::stateless()
    }

    fn state(&self) -> tree::State {
        tree::State::None
    }

    fn children(&self) -> Vec<Tree> {
        Vec::new()
    }

    fn diff(&self, _tree: &mut Tree) {}

    fn size(&self) -> Size<Length> {
        Size::new(
            Length::Fixed(self.area.width),
            Length::Fixed(self.area.height),
        )
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        _limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(self.area)
    }

    fn update(
        &mut self,
        _tree: &mut Tree,
        _event: &Event,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn Clipboard,
        _shell: &mut Shell<'_, BrowserMessage>,
        _viewport: &Rectangle,
    ) {
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        _style: &renderer::Style,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let rect = card_rect_at(self.mode, self.anim, self.page, self.tabs, Instant::now());
        if rect.width < 1.0 || rect.height < 1.0 {
            return;
        }
        let style = theme::menu_panel_fade(self.fade)(theme);
        let Some(background) = style.background else {
            return;
        };
        renderer.fill_quad(
            renderer::Quad {
                bounds: rect,
                border: style.border,
                shadow: style.shadow,
                snap: true,
            },
            background,
        );
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        mouse::Interaction::None
    }

    fn overlay<'b>(
        &'b mut self,
        _tree: &'b mut Tree,
        _layout: Layout<'b>,
        _renderer: &iced::Renderer,
        _viewport: &Rectangle,
        _translation: Vector,
    ) -> Option<overlay::Element<'b, BrowserMessage, iced::Theme, iced::Renderer>> {
        None
    }
}

impl From<CardShell> for Element<'static, BrowserMessage> {
    fn from(value: CardShell) -> Self {
        Element::new(value)
    }
}

/// App-level ticker wrapper: while a transition is live, ask for the next
/// display frame after each present. No global subscription, no other windows
/// rebuilt.
fn ticker<'a>(content: Element<'a, BrowserMessage>, active: bool) -> Element<'a, BrowserMessage> {
    GridTicker { content, active }.into()
}

struct GridTicker<'a> {
    content: Element<'a, BrowserMessage>,
    active: bool,
}

impl Widget<BrowserMessage, iced::Theme, iced::Renderer> for GridTicker<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::stateless()
    }

    fn state(&self) -> tree::State {
        tree::State::None
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &iced::advanced::layout::Limits,
    ) -> iced::advanced::layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: iced::advanced::layout::Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn iced::advanced::Clipboard,
        shell: &mut iced::advanced::Shell<'_, BrowserMessage>,
        viewport: &Rectangle,
    ) {
        if self.active && matches!(event, Event::Window(window::Event::RedrawRequested(_))) {
            #[cfg(debug_assertions)]
            {
                GRID_TICK_FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                note_grid_frame(Instant::now());
            }
            // Present immediately on the next vsync. A fixed +16ms request
            // lands just past the refresh often enough to alternate 16/32ms
            // frames, which reads as a choppy FLIP.
            crate::ui::pull::FrameDemand::Now.request(shell);
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &iced::advanced::renderer::Style,
        layout: iced::advanced::layout::Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: iced::advanced::layout::Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: iced::advanced::layout::Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
}

impl<'a> From<GridTicker<'a>> for Element<'a, BrowserMessage> {
    fn from(value: GridTicker<'a>) -> Self {
        Element::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_slides_up_while_fading_out() {
        assert_eq!(header_offset(1.0), 0.0);
        assert!(header_offset(0.0) < 0.0, "hidden header sits above its row");
        assert!(header_offset(0.5) < header_offset(1.0));
        assert!((header_offset(0.0) + HEADER_SLIDE_PX).abs() < 0.01);
    }

    #[test]
    fn flip_transform_maps_the_absolute_page_onto_the_card() {
        let source = Rectangle::new(Point::new(93.0, 99.0), Size::new(1734.0, 927.0));
        let target = Rectangle::new(
            Point::new(558.0, 131.0),
            Size::new(395.0, 927.0 * 395.0 / 1734.0),
        );
        let (scale, tx, ty) = flip_transform(source, target);
        assert!((source.x * scale + tx - target.x).abs() < 0.01, "x maps");
        assert!((source.y * scale + ty - target.y).abs() < 0.01, "y maps");
        assert!((source.width * scale - target.width).abs() < 0.01, "width");
        assert!(
            (source.height * scale - target.height).abs() < 0.01,
            "height"
        );
        // t = 0 identity: same absolute rect in and out.
        let (scale, tx, ty) = flip_transform(source, source);
        assert!((scale - 1.0).abs() < f32::EPSILON);
        assert!(tx.abs() < 0.01 && ty.abs() < 0.01);
    }

    #[test]
    fn tabs_strip_is_one_row_with_plus_first() {
        let layout = tabs_layout(1206.0, 1734.0 / 931.0);
        let plus = layout.card_rect(0, 0.0);
        let first = layout.card_rect(1, 0.0);
        assert_eq!(plus.y, first.y, "single horizontal row");
        assert!(plus.x < first.x, "plus card is first");
        assert!(
            (layout.card_w / layout.card_h - 1734.0 / 931.0).abs() < 0.01,
            "cards keep the page aspect"
        );
        assert!(layout.bookmarks.y > layout.row.y + layout.row.height);
        assert!(layout.island.y > layout.bookmarks.y);
        assert!(
            (layout.area_h
                - (GRID_PAD
                    + layout.card_h
                    + TABS_SCROLLBAR_LANE
                    + GRID_GAP
                    + BOOKMARK_ROW_H
                    + TABS_ISLAND_GAP
                    + TABS_ISLAND_H
                    + GRID_PAD))
                .abs()
                < 0.01,
            "content drives the card height"
        );
    }

    #[test]
    fn compact_card_matches_the_page_inset_on_every_side() {
        let page = page_card_rect(1920.0, 1080.0);
        let geometry = tabs_card_geometry(1920.0, 1080.0);
        assert!(
            (geometry.card.width / page.width - TABS_CARD_SCALE).abs() < 0.01,
            "tabs card is 30% narrower"
        );
        assert!(geometry.card.height < page.height);
        assert!(
            (geometry.inner.x - geometry.card.x - crate::ui::browser::state::BROWSER_CARD_PAD)
                .abs()
                < 0.01
        );
        assert!(
            (geometry.layout.row.x - GRID_PAD).abs() < 0.01,
            "left inset"
        );
        assert!(
            (geometry.layout.row.y - GRID_PAD).abs() < 0.01,
            "top inset equals the sides"
        );
    }

    #[test]
    fn reveal_offset_keeps_the_card_fully_visible() {
        let layout = tabs_layout(1206.0, 1734.0 / 931.0);
        let count = 25;
        assert_eq!(reveal_offset(&layout, 0, count, 0.0), 0.0);
        let far = reveal_offset(&layout, 20, count, 0.0);
        let right = 20.0 * (layout.card_w + layout.gap) + layout.card_w;
        assert!((far - (right - layout.row.width)).abs() < 0.01);
        let max = layout.content_width(count) - layout.row.width;
        assert!(far <= max);
        // Scrolling back to the first card returns to zero, not a negative x.
        assert_eq!(reveal_offset(&layout, 0, count, far), 0.0);
    }

    #[test]
    fn card_shell_lerps_between_page_and_tabs() {
        let page = Rectangle::new(iced::Point::new(50.0, 50.0), Size::new(1800.0, 1000.0));
        let tabs = Rectangle::new(iced::Point::new(400.0, 300.0), Size::new(1200.0, 340.0));
        let now = Instant::now();
        let anim = GridAnim {
            generation: 1,
            started: now,
            tab: TabId::BOOT,
            entering: true,
            card: GridFocus::Tab(TabId::BOOT),
        };
        assert_eq!(
            card_rect_at(GridMode::Page, Some(anim), page, tabs, now).width,
            page.width
        );
        let mid = card_rect_at(
            GridMode::Entering,
            Some(anim),
            page,
            tabs,
            now + GRID_ANIM / 2,
        );
        assert!(mid.width < page.width && mid.width > tabs.width);
        assert_eq!(
            card_rect_at(GridMode::Grid, Some(anim), page, tabs, now).width,
            tabs.width
        );
    }

    #[test]
    fn closing_a_tab_keeps_the_selected_index() {
        let ids = [TabId(1), TabId(2), TabId(3), TabId(4)];
        assert_eq!(
            focus_after_close(&ids, GridFocus::Tab(TabId(2)), TabId(2)),
            GridFocus::Tab(TabId(3)),
            "next tab keeps the slot"
        );
        assert_eq!(
            focus_after_close(&ids, GridFocus::Tab(TabId(4)), TabId(4)),
            GridFocus::Tab(TabId(3)),
            "closing the last card selects the new last card"
        );
        assert_eq!(
            focus_after_close(&ids, GridFocus::Tab(TabId(1)), TabId(4)),
            GridFocus::Tab(TabId(1)),
            "closing another tab leaves focus alone"
        );
        assert_eq!(
            focus_after_close(&[TabId(1)], GridFocus::Tab(TabId(1)), TabId(1)),
            GridFocus::Tab(TabId(1)),
            "the last-tab reset keeps the tab"
        );
        assert_eq!(
            focus_after_close(&ids, GridFocus::Plus, TabId(2)),
            GridFocus::Plus
        );
    }

    #[test]
    fn frozen_visual_order_is_stable_when_a_tab_is_entered() {
        let mut panel = BrowserPanel::default();
        panel.session.begin_load("https://a.test/".to_string());
        let (new_tab, _) = panel.create_tab("https://b.test/".to_string());
        panel.refresh_grid_visual();
        assert_eq!(panel.grid.visual, vec![new_tab, TabId::BOOT]);
        assert_eq!(panel.grid.focus, GridFocus::Tab(new_tab));
        assert_eq!(grid_tabs(&panel, &panel.grid.visual).len(), 2);

        panel.activate_tab(TabId::BOOT);
        panel.sync_grid_visual();
        // Entering a tab never moves it to the front of the grid.
        assert_eq!(panel.grid.visual, vec![new_tab, TabId::BOOT]);
    }
}
