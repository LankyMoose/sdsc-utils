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
use iced::widget::{column, container, mouse_area, row, scrollable, space, stack, svg, text};
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
pub const TABS_COLS: usize = 4;
pub const TABS_ROWS: usize = 3;
pub const TABS_SLOTS: usize = TABS_COLS * TABS_ROWS;
/// One slot is always the leading plus card.
pub const TABS_PER_PAGE: usize = TABS_SLOTS - 1;
const TABS_SIDE_W: f32 = 38.0;
const TABS_ISLAND_H: f32 = 44.0;
const TABS_ISLAND_GAP: f32 = 10.0;
const CHEVRON_SIZE: f32 = 34.0;
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
    pub source: Rectangle,
    pub target: Rectangle,
    /// Tab whose page texture rides the FLIP.
    pub tab: TabId,
    pub entering: bool,
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

    pub fn rect(&self, now: Instant) -> Rectangle {
        let t = self.progress(now);
        lerp_rect(self.source, self.target, t)
    }

    pub fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= GRID_ANIM
    }
}

fn lerp_rect(a: Rectangle, b: Rectangle, t: f32) -> Rectangle {
    Rectangle::new(
        iced::Point::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t),
        Size::new(
            a.width + (b.width - a.width) * t,
            a.height + (b.height - a.height) * t,
        ),
    )
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
    /// Visual strip order frozen for this grid session. Keeps FLIP source
    /// rects stable while tab activation changes the live page.
    pub visual: Vec<TabId>,
    /// Slots page (0-based); each page starts with the plus card.
    pub page: usize,
    pub bookmark_scroll_x: f32,
    pub anim: Option<GridAnim>,
    pub anim_generation: u64,
    /// Tab to activate when a leaving transition lands.
    pub pending_tab: Option<TabId>,
    /// Explicit FLIP source (plus-card exits) instead of deriving it from the
    /// selected tab's card.
    pub source_override: Option<Rectangle>,
}

impl Default for GridState {
    fn default() -> Self {
        Self {
            mode: GridMode::Page,
            focus: GridFocus::Plus,
            visual: Vec::new(),
            page: 0,
            bookmark_scroll_x: 0.0,
            anim: None,
            anim_generation: 0,
            pending_tab: None,
            source_override: None,
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

    /// Rebuild the frozen visual strip for a fresh grid session: active tab
    /// first (FLIP target), then creation order.
    pub fn refresh_visual(&mut self, active: TabId, order: &[TabId]) {
        self.visual.clear();
        self.visual.push(active);
        for id in order {
            if !self.visual.contains(id) {
                self.visual.push(*id);
            }
        }
        self.focus = GridFocus::Tab(active);
        self.page = 0;
        self.bookmark_scroll_x = 0.0;
        self.source_override = None;
    }

    pub fn page_count(&self) -> usize {
        page_count(self.visual.len())
    }

    pub fn clamp_page(&mut self) {
        let count = self.page_count();
        self.page = self.page.min(count.saturating_sub(1));
    }

    pub fn page_prev(&mut self) {
        self.page = self.page.saturating_sub(1);
    }

    pub fn page_next(&mut self) {
        let count = self.page_count();
        self.page = (self.page + 1).min(count.saturating_sub(1));
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

#[derive(Debug, Clone, Copy)]
pub struct GridLayout {
    pub area_w: f32,
    pub area_h: f32,
    pub grid: Rectangle,
    pub bookmarks: Rectangle,
    pub island: Rectangle,
    pub prev: Rectangle,
    pub next: Rectangle,
    pub cols: usize,
    pub gap: f32,
    pub card_w: f32,
    pub card_h: f32,
}

impl GridLayout {
    /// Slot rect inside one tabs page. Slot 0 is the plus card.
    pub fn card_rect(&self, slot: usize) -> Rectangle {
        let cols = self.cols.max(1);
        let col = slot % cols;
        let row = slot / cols;
        Rectangle::new(
            iced::Point::new(
                self.grid.x + col as f32 * (self.card_w + self.gap),
                self.grid.y + row as f32 * (self.card_h + self.gap),
            ),
            Size::new(self.card_w, self.card_h),
        )
    }

    /// Where the page surface sits in page mode, relative to the full Browser
    /// card interior (below the header row).
    pub fn page_rect(&self) -> Rectangle {
        Rectangle::new(
            iced::Point::new(0.0, crate::ui::browser::state::BROWSER_HEADER_TOTAL),
            Size::new(
                self.area_w,
                (self.area_h - crate::ui::browser::state::BROWSER_HEADER_TOTAL).max(1.0),
            ),
        )
    }
}

pub fn page_count(tab_count: usize) -> usize {
    (tab_count.max(1)).div_ceil(TABS_PER_PAGE).max(1)
}

/// Start/end indices into the frozen visual strip for one page.
pub fn page_range(tab_count: usize, page: usize) -> (usize, usize) {
    let start = page.saturating_mul(TABS_PER_PAGE).min(tab_count);
    let end = (start + TABS_PER_PAGE).min(tab_count);
    (start, end)
}

#[cfg(test)]
fn slots_on_page(tab_count: usize, page: usize) -> usize {
    let (start, end) = page_range(tab_count, page);
    1 + end.saturating_sub(start)
}

/// Tabs-view geometry: fixed 4x3 slots, plus card first, bookmark strip above
/// the footer island, and side gutters for page chevrons.
pub fn grid_layout(area_w: f32, area_h: f32) -> GridLayout {
    let area_w = area_w.max(1.0);
    let area_h = area_h.max(1.0);
    let header = crate::ui::browser::state::BROWSER_HEADER_TOTAL;
    let page_h = (area_h - header).max(1.0);
    let aspect = area_w / page_h;
    let inner_w = (area_w - 2.0 * GRID_PAD).max(1.0);
    let island = Rectangle::new(
        iced::Point::new(GRID_PAD, (area_h - GRID_PAD - TABS_ISLAND_H).max(0.0)),
        Size::new(inner_w, TABS_ISLAND_H),
    );
    let grid_x = GRID_PAD + TABS_SIDE_W;
    let grid_w = (area_w - 2.0 * (GRID_PAD + TABS_SIDE_W)).max(1.0);
    let bottom = (island.y - TABS_ISLAND_GAP).max(0.0);
    let max_grid_h = (bottom - GRID_PAD - GRID_GAP - BOOKMARK_ROW_H).max(1.0);
    let width_w = (grid_w - (TABS_COLS.saturating_sub(1)) as f32 * GRID_GAP) / TABS_COLS as f32;
    let height_w = ((max_grid_h - (TABS_ROWS.saturating_sub(1)) as f32 * GRID_GAP)
        / TABS_ROWS as f32)
        * aspect;
    let card_w = width_w.min(height_w).max(1.0);
    let card_h = (card_w / aspect).max(1.0);
    let grid_h = TABS_ROWS as f32 * card_h + (TABS_ROWS.saturating_sub(1)) as f32 * GRID_GAP;
    let group_h = grid_h + GRID_GAP + BOOKMARK_ROW_H;
    let slack = (bottom - GRID_PAD - group_h).max(0.0);
    let grid_y = GRID_PAD + slack * 0.5;
    let bookmarks = Rectangle::new(
        iced::Point::new(GRID_PAD, grid_y + grid_h + GRID_GAP),
        Size::new(inner_w, BOOKMARK_ROW_H),
    );
    let grid = Rectangle::new(iced::Point::new(grid_x, grid_y), Size::new(grid_w, grid_h));
    let center_y = grid.y + grid.height * 0.5 - CHEVRON_SIZE * 0.5;
    let prev = Rectangle::new(
        iced::Point::new(
            GRID_PAD,
            center_y.clamp(GRID_PAD, area_h - GRID_PAD - CHEVRON_SIZE),
        ),
        Size::new(CHEVRON_SIZE, CHEVRON_SIZE),
    );
    let next = Rectangle::new(
        iced::Point::new(area_w - GRID_PAD - CHEVRON_SIZE, prev.y),
        Size::new(CHEVRON_SIZE, CHEVRON_SIZE),
    );
    GridLayout {
        area_w,
        area_h,
        grid,
        bookmarks,
        island,
        prev,
        next,
        cols: TABS_COLS,
        gap: GRID_GAP,
        card_w,
        card_h,
    }
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

/// Tabs-view root: page card grid, bookmark strip, footer hint island, page
/// chevrons, and the optional FLIP overlay.
pub fn page_view<'a>(
    panel: &'a BrowserPanel,
    engine_available: bool,
    area_w: f32,
    area_h: f32,
    fade: f32,
) -> Element<'a, BrowserMessage> {
    let fade = fade.clamp(0.0, 1.0);
    let tabs = grid_tabs(panel, &panel.grid.visual);
    let layout = grid_layout(area_w, area_h);
    let state = &panel.grid;
    let anim = state.anim;
    let anim_tab = anim.map(|anim| anim.tab);
    let (start, end) = page_range(tabs.len(), state.page);
    let page_tabs = tabs.get(start..end).unwrap_or(&[]);
    let pages = page_count(tabs.len());

    // Slot 0 is ALWAYS the plus card; tabs follow in the frozen visual order.
    let mut cards: Vec<Element<'a, BrowserMessage>> = Vec::with_capacity(page_tabs.len() + 1);
    cards.push(plus_card(
        layout.card_w,
        layout.card_h,
        state.focus == GridFocus::Plus,
        fade,
    ));
    for tab in page_tabs {
        let focused = state.focus == GridFocus::Tab(tab.id);
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

    let mut card_rows: Vec<Element<'a, BrowserMessage>> = Vec::new();
    while !cards.is_empty() {
        let take = layout.cols.min(cards.len());
        let line_items: Vec<Element<'a, BrowserMessage>> = cards.drain(..take).collect();
        let mut line = row![].spacing(layout.gap);
        for card in line_items {
            line = line.push(card);
        }
        card_rows.push(line.into());
    }
    let grid_layer = positioned(
        column(card_rows).spacing(layout.gap),
        layout.grid,
        area_w,
        area_h,
    );

    let bookmarks = positioned(
        bookmark_strip(panel, state, layout.bookmarks.width, fade),
        layout.bookmarks,
        area_w,
        area_h,
    );
    let island = positioned(island(pages, fade), layout.island, area_w, area_h);

    let mut layers: Vec<Element<'a, BrowserMessage>> = vec![grid_layer, bookmarks, island];
    if pages > 1 {
        layers.push(positioned(
            page_button(
                svg_icon::CHEVRON_LEFT_SVG,
                state.page > 0,
                BrowserMessage::GridPagePrev,
                fade,
            ),
            layout.prev,
            area_w,
            area_h,
        ));
        layers.push(positioned(
            page_button(
                svg_icon::CHEVRON_RIGHT_SVG,
                state.page + 1 < pages,
                BrowserMessage::GridPageNext,
                fade,
            ),
            layout.next,
            area_w,
            area_h,
        ));
    }
    if let Some(anim) = anim {
        // The FLIP owns its own draw-time clock: the widget tree is built once
        // per view rebuild, so a rect sampled here would freeze at whatever
        // progress the last message landed on (the old 1–3 frame judder).
        layers.push(flip_page(
            transition_page(panel, engine_available, anim.tab, layout.page_rect(), fade),
            anim,
            layout.page_rect(),
            Size::new(area_w, area_h),
        ));
        // The Browser header is part of the same transition: it must not
        // vanish/appear in a single frame while the page is still moving.
        layers.push(header_curtain(
            crate::ui::browser::view::browser_header(panel, area_w, fade),
            anim,
            Size::new(area_w, area_h),
            crate::ui::browser::state::BROWSER_HEADER_TOTAL,
            fade,
        ));
    }
    let body: Element<'a, BrowserMessage> = stack(layers)
        .width(Length::Fixed(area_w))
        .height(Length::Fixed(area_h))
        .clip(true)
        .into();
    ticker(body, state.animating())
}

/// Place `content` at an absolute rect inside the tabs-view area.
fn positioned<'a>(
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

fn island<'a>(pages: usize, fade: f32) -> Element<'a, BrowserMessage> {
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
    let mut hints = vec![
        hint(None, Some(FaceButton::Cross), "Open"),
        hint(None, Some(FaceButton::Square), "Close"),
        hint(None, Some(FaceButton::Triangle), "Bookmark"),
        hint(None, Some(FaceButton::Circle), "Back"),
    ];
    if pages > 1 {
        hints.push(hint(Some("L1"), None, "Page"));
        hints.push(hint(Some("R1"), None, "Page"));
    }
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

fn page_button<'a>(
    icon: &'static str,
    enabled: bool,
    message: BrowserMessage,
    fade: f32,
) -> Element<'a, BrowserMessage> {
    let fade = fade.clamp(0.0, 1.0);
    let color = if enabled { theme::ACCENT } else { theme::DIM };
    let glyph: Element<'a, BrowserMessage> = svg(svg::Handle::from_memory(icon.as_bytes()))
        .width(Length::Fixed(16.0))
        .height(Length::Fixed(16.0))
        .opacity(if enabled { fade } else { 0.4 * fade })
        .style(move |_theme, _status| svg::Style { color: Some(color) })
        .into();
    let button = container(glyph)
        .width(Length::Fixed(CHEVRON_SIZE))
        .height(Length::Fixed(CHEVRON_SIZE))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(move |_theme| container::Style {
            background: Some(iced::Background::Color(theme::scale_alpha(
                theme::GLASS_FILL,
                fade,
            ))),
            border: iced::Border {
                color: theme::scale_alpha(
                    if enabled {
                        theme::GLASS_HAIRLINE
                    } else {
                        theme::alpha(theme::LINE, 0.2)
                    },
                    fade,
                ),
                width: 1.0,
                radius: theme::radius::PILL.into(),
            },
            ..container::Style::default()
        });
    if enabled {
        mouse_area(button)
            .on_press(message)
            .interaction(mouse::Interaction::Pointer)
            .into()
    } else {
        button.into()
    }
}

/// Full-size page texture used only for the FLIP. The widget is laid out at the
/// page rect; [`FlipPage`] scales/translates it between page and card at draw
/// time so the transition advances every presented frame.
fn transition_page<'a>(
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
    page: Rectangle,
    area: Size,
}

fn flip_page<'a>(
    content: Element<'a, BrowserMessage>,
    anim: GridAnim,
    page: Rectangle,
    area: Size,
) -> Element<'a, BrowserMessage> {
    FlipPage {
        content,
        anim,
        page,
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
        let rect = self.anim.rect(Instant::now());
        // `child_layout` and `layout` are absolute screen coordinates, while
        // `self.anim.rect` is relative to the tabs-view area. Map the child's
        // absolute page rect to the absolute animated rect; scaling around the
        // screen origin would leave the page's y pinned near the top and snap
        // it to the card on the final frame.
        let source = child_layout.bounds();
        let origin = layout.bounds().position();
        let target = Rectangle::new(
            iced::Point::new(origin.x + rect.x, origin.y + rect.y),
            rect.size(),
        );
        let (scale, tx, ty) = flip_transform(source, target);
        renderer.with_layer(layout.bounds(), |renderer| {
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
}

fn header_curtain<'a>(
    content: Element<'a, BrowserMessage>,
    anim: GridAnim,
    area: Size,
    header_h: f32,
    fade: f32,
) -> Element<'a, BrowserMessage> {
    HeaderCurtain {
        content,
        anim,
        area,
        header_h,
        fade: fade.clamp(0.0, 1.0),
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
        let appear = self.appear(Instant::now());
        if appear <= 0.001 || self.fade <= 0.001 {
            return;
        }
        let bounds = Rectangle::new(
            layout.bounds().position(),
            Size::new(self.area.width, self.header_h),
        );
        // Negative = slide up. Entering the tabs view lifts the header out of
        // its row as it fades; leaving drops it back in from above.
        let offset = header_offset(appear);
        renderer.with_layer(bounds, |renderer| {
            renderer.with_translation(Vector::new(0.0, offset), |renderer| {
                self.content.as_widget().draw(
                    &tree.children[0],
                    renderer,
                    theme,
                    style,
                    child_layout,
                    cursor,
                    viewport,
                );
                // The curtain travels with the header, so no uncovered strip
                // keeps fully-opaque content visible at the end of the slide.
                if appear < 0.999 {
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds,
                            ..renderer::Quad::default()
                        },
                        Background::Color(theme::alpha(
                            theme::lighten(theme::BASE_BG, 0.04),
                            (1.0 - appear * appear) * self.fade,
                        )),
                    );
                }
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
        theme::WARNING
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
    fn cards_keep_the_page_aspect_ratio() {
        for (w, page_h) in [(1734.0, 931.0), (1094.0, 571.0), (700.0, 400.0)] {
            let layout = grid_layout(w, page_h + crate::ui::browser::state::BROWSER_HEADER_TOTAL);
            let aspect = w / page_h;
            assert!(
                (layout.card_w / layout.card_h - aspect).abs() < 0.01,
                "card aspect must match page aspect"
            );
            assert!(layout.card_w > 0.0);
            assert!(layout.bookmarks.y > layout.grid.y + layout.grid.height);
            assert!(layout.island.y > layout.bookmarks.y);
            assert_eq!(layout.cols, 4);
        }
    }

    #[test]
    fn grid_is_four_by_three_with_plus_first() {
        let layout = grid_layout(
            1734.0,
            931.0 + crate::ui::browser::state::BROWSER_HEADER_TOTAL,
        );
        let plus = layout.card_rect(0);
        let first_tab = layout.card_rect(1);
        assert_eq!(plus.y, first_tab.y);
        assert!(plus.x < first_tab.x);
        // Fourth slot ends the row; fifth starts row two.
        assert_eq!(layout.card_rect(3).y, layout.card_rect(0).y);
        assert!(layout.card_rect(4).y > layout.card_rect(0).y);
        // Exactly 12 slots.
        assert_eq!(TABS_SLOTS, 12);
    }

    #[test]
    fn pages_hold_eleven_tabs_after_the_plus_card() {
        assert_eq!(page_count(0), 1);
        assert_eq!(page_count(11), 1);
        assert_eq!(page_count(12), 2);
        assert_eq!(page_count(23), 3);
        assert_eq!(page_range(12, 0), (0, 11));
        assert_eq!(page_range(12, 1), (11, 12));
        assert_eq!(slots_on_page(12, 0), 12);
        assert_eq!(slots_on_page(12, 1), 2);
    }

    #[test]
    fn grid_page_state_clamps_and_pages() {
        let mut state = GridState {
            visual: (0..25).map(|i| TabId(i + 1)).collect(),
            ..GridState::default()
        };
        assert_eq!(state.page_count(), 3);
        state.page_next();
        state.page_next();
        state.page_next();
        assert_eq!(state.page, 2);
        state.page_prev();
        assert_eq!(state.page, 1);
        state.visual.truncate(11);
        state.clamp_page();
        assert_eq!(state.page, 0);
    }

    #[test]
    fn transition_rect_lerps_between_page_and_card() {
        let started = Instant::now();
        let anim = GridAnim {
            generation: 1,
            started,
            source: Rectangle::new(iced::Point::new(0.0, 0.0), Size::new(1000.0, 500.0)),
            target: Rectangle::new(iced::Point::new(100.0, 100.0), Size::new(200.0, 100.0)),
            tab: TabId::BOOT,
            entering: true,
        };
        let start = anim.rect(started);
        assert_eq!((start.x, start.y, start.width), (0.0, 0.0, 1000.0));
        let end = anim.rect(started + GRID_ANIM);
        assert!((end.x - 100.0).abs() < 0.01);
        assert!((end.width - 200.0).abs() < 0.01);
        assert!(anim.done(started + GRID_ANIM));
    }

    #[test]
    fn frozen_visual_order_puts_active_first() {
        let mut panel = BrowserPanel::default();
        panel.session.begin_load("https://a.test/".to_string());
        let (new_tab, _) = panel.create_tab("https://b.test/".to_string());
        panel.refresh_grid_visual();
        assert_eq!(panel.grid.visual, vec![new_tab, TabId::BOOT]);
        assert_eq!(panel.grid.focus, GridFocus::Tab(new_tab));
        assert_eq!(grid_tabs(&panel, &panel.grid.visual).len(), 2);

        panel.activate_tab(TabId::BOOT);
        panel.sync_grid_visual();
        // The frozen order is not re-sorted mid-session.
        assert_eq!(panel.grid.visual, vec![new_tab, TabId::BOOT]);
    }
}
