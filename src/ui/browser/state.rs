//! Browser app state: page state, cursor, text keyboard, tab strip, grid, and
//! bookmarks.
//!
//! Page mode has one header row (Back / URL / Forward). Share/Create swaps the
//! page area for the tab grid (`grid.rs`); the header stays.

use std::collections::HashMap;
use std::time::Instant;

use crate::browser::cursor::CursorHandle;

/// Header row height, excluding the hairline rule below it.
pub const BROWSER_HEADER_H: f32 = 44.0;
/// Hairline under the header row.
pub const BROWSER_HEADER_RULE: f32 = 1.0;
/// Total header stack consumed above the page.
pub const BROWSER_HEADER_TOTAL: f32 = BROWSER_HEADER_H + BROWSER_HEADER_RULE;
/// Horizontal inset for header content against the card page width.
pub const BROWSER_HEADER_PAD: f32 = 12.0;
/// Small gap between the history prompts and the URL well.
pub const BROWSER_HEADER_GROUP_GAP: f32 = 8.0;
/// Fixed side slot for the Back/Forward prompt clusters. The Back cluster is
/// right-aligned and Forward left-aligned, so both stay snug to the URL while
/// equal slots keep the URL exactly centred.
pub const BROWSER_HEADER_SIDE_W: f32 = 136.0;
/// URL well width bounds.
pub const BROWSER_URL_W_MIN: f32 = 420.0;
pub const BROWSER_URL_W_MAX: f32 = 680.0;
/// URL well height; matches the shared 32px action-hint slot.
pub const BROWSER_URL_BAR_H: f32 = 32.0;
/// Symmetric vertical padding around the URL well inside the header row.
pub const BROWSER_HEADER_V_PAD: f32 = (BROWSER_HEADER_H - BROWSER_URL_BAR_H) * 0.5;
/// URL well horizontal padding.
pub const BROWSER_URL_BAR_PAD: f32 = 10.0;
/// Globe icon size inside the URL well.
pub const BROWSER_URL_ICON: f32 = 16.0;
/// Gap between items inside the URL well.
pub const BROWSER_URL_GAP: f32 = 8.0;
/// Shared action-hint slot (`face::HOLD_RING_SIZE`); kept local to the pure layout.
pub const BROWSER_HINT_SLOT: f32 = 32.0;

/// Last canonical history snapshot reported by the engine, kept alongside the
/// UI's projection so traces can expose disagreement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineHistorySnapshot {
    pub current: usize,
    pub entries: Vec<crate::browser::engine::EngineHistoryEntry>,
}

/// Per-tab page state: everything that belongs to one tab's document.
/// `BrowserPanel::page` is always the ACTIVE tab's; background tabs stash
/// theirs in `BrowserPanel::stored`. The panel derefs to this, so all
/// existing live-tab call sites (`panel.session`, `panel.cursor`, …) keep
/// working.
#[derive(Debug, Clone, Default)]
pub struct BrowserTabPage {
    pub session: crate::browser::BrowserSession,
    /// Virtual cursor (engine mode only; pad-owned). The pull-model surface
    /// advances the shared runtime; app actions read snapshots.
    pub cursor: CursorHandle,
    /// Focusable elements from the latest accessibility tree.
    pub focusables: Vec<crate::browser::cursor::Focusable>,
    /// Document URL that produced `focusables`. Replay docking rejects a
    /// tree from the pre-navigation document.
    pub focusables_url: Option<String>,
    /// Bumped on every accessibility tree. Replay waits for the tree culled
    /// around a finished cursor nudge before starting the next directional
    /// step, so the step reads the target set it will actually navigate.
    pub focusables_generation: u64,
    /// Bumped on every page title change. Replay waits for selection/scroll
    /// title reports before the next synthetic input.
    pub title_generation: u64,
    pub scroll_y: f32,
    /// Viewport scroll offset the backend translated the tree with
    /// (`EngineEvent::ScrollOffset`). Debug overlay + shot correlation.
    pub scroll_offset: (f32, f32),
    /// Document extent in page pixels (grow-only; `EngineEvent::ScrollExtent`).
    /// Drives the page scrollbar. `(0,0)` until the first snapshot.
    pub page_extent: (f32, f32),
    /// Page-driven focus target (autofocus / JS `.focus()`): UI id to dock
    /// when it next resolves in the live set. One shot per tree — a culled
    /// target never docks ("if possible").
    pub pending_focus: Option<u64>,
    /// Last click point + time, for the R3 issue-flag correlation.
    pub last_click: Option<((f32, f32), Instant)>,
    /// Last canonical engine history; tracing compares it with the UI stack.
    pub last_engine_history: Option<EngineHistorySnapshot>,
    /// Last CEF back/forward capability flag pair.
    pub last_engine_can: Option<(bool, bool)>,
    /// Page text session state published to the hosted-app contract. Set by
    /// the engine's editable-focus signal; never opens the OSK by itself.
    pub text_session_active: bool,
    /// Current engine load phase. Replay settle waits for Idle after a step
    /// that may have started a navigation.
    pub activity: crate::browser::engine::EngineActivity,
}

/// One background tab: stable id, page state, and whether a backend view
/// exists for it. Restored tabs start unloaded (no view, no load) and open
/// on first switch — the worker only ever pays for visible tabs.
#[derive(Debug, Clone)]
pub struct StoredTab {
    pub id: crate::browser::engine::TabId,
    pub page: BrowserTabPage,
    pub loaded: bool,
}

/// Browser app panel. The page surface and cursor live here; load lifecycle
/// (generation-guarded suspend/resume) lives in
/// [`crate::browser::BrowserSession`]. The pad cursor roams in engine page
/// pixels (see [`crate::browser::cursor`]).
#[derive(Debug, Clone)]
pub struct BrowserPanel {
    /// Live page state: ALWAYS the active tab's. Swapped with `stored`
    /// entries on switch (see `activate_stored`). The Share grid renders the
    /// strip; background tabs stay dormant until first activation.
    pub page: BrowserTabPage,
    /// Background tabs (page state only; no chrome).
    pub stored: Vec<StoredTab>,
    /// Creation-order tab ids (active included).
    pub order: Vec<crate::browser::engine::TabId>,
    /// Active tab id. Defaults to the boot webview — matches the worker's
    /// `TabId::BOOT` with no handshake.
    pub active_tab: crate::browser::engine::TabId,
    /// Next UI-owned tab id (counts up from boot; engine popups count down
    /// from `u64::MAX` — disjoint by construction).
    pub next_tab_id: u64,
    /// Last page-local hardware/replay pointer position. Clicks and wheel
    /// events use this point; replay sets it explicitly so automation never
    /// depends on the OS cursor position.
    pub page_mouse: (f32, f32),
    /// Physical mouse is the active pointer (pad ring hides, OS cursor shows).
    pub page_mouse_active: bool,
    /// Left page button currently held (drag selection).
    pub page_mouse_down: bool,
    /// Keyboard modifiers for page mouse/drag events.
    pub page_mouse_modifiers: keyboard_types::Modifiers,
    /// Last click timestamp/point/count for double/triple-click detection.
    pub page_last_click: Option<(Instant, f32, f32, u8)>,
    /// Click count for the current press (used on release).
    pub page_click_count: u8,
    /// Cursor shape reported by CEF for the page.
    pub page_cursor: crate::browser::engine::BrowserCursor,
    /// Last debug frame-shot time (5s cadence while the overlay is on).
    pub last_shot: Option<Instant>,
    /// JS element-click mode for Button-kind dock targets (toggled from the
    /// browser card itself; off by default): `__sdsc_click` on the exact
    /// element instead of the trusted synthetic pair. Lets A/B testing
    /// untrusted-activation against trusted input per page.
    pub js_click: bool,
    /// R3 rising-edge latch for the issue flag (R3 carries no NavAction).
    pub r3_latched: bool,
    /// Live page-area size in card pixels (the engine viewport resizes to
    /// this; the cursor space matches it 1:1).
    pub viewport: (f32, f32),
    /// Browser debug overlay + shots (Diagnostics toggle / F10, mirrored
    /// from App). Never persisted.
    pub debug_overlay: bool,
    /// Chrome-style bookmarks (disk-persisted). The Share grid renders the
    /// star state and the scrollable bookmark row.
    pub bookmarks: Vec<crate::browser::bookmarks::Bookmark>,
    /// Controller hint press state for the header row.
    pub l1_held: bool,
    pub r1_held: bool,
    pub triangle_held: bool,
    /// URL well editing state. Panel-level, not per-tab: the URL belongs to
    /// the app chrome, while page state belongs to the active tab.
    pub url_editing: bool,
    pub url_draft: String,
    /// First typed character replaces the prefilled URL (select-all semantics).
    pub url_replace_on_type: bool,
    /// Share/Create tab-grid mode and FLIP transition.
    pub grid: crate::ui::browser::grid::GridState,
}

impl std::ops::Deref for BrowserPanel {
    type Target = BrowserTabPage;

    /// Live page state is always the active tab's — `panel.session`,
    /// `panel.cursor` and friends address it directly.
    fn deref(&self) -> &Self::Target {
        &self.page
    }
}

impl std::ops::DerefMut for BrowserPanel {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.page
    }
}

impl Default for BrowserPanel {
    /// Fresh panel: one implicit boot tab (matches the worker's
    /// `TabId::BOOT`); the next explicit tab takes id 2.
    fn default() -> Self {
        let page = BrowserTabPage::default();
        Self {
            page,
            stored: Vec::new(),
            order: vec![crate::browser::engine::TabId::BOOT],
            active_tab: crate::browser::engine::TabId::BOOT,
            next_tab_id: 2,
            page_mouse: (
                crate::browser::cursor::VIEWPORT_W * 0.5,
                crate::browser::cursor::VIEWPORT_H * 0.5,
            ),
            page_mouse_active: false,
            page_mouse_down: false,
            page_mouse_modifiers: keyboard_types::Modifiers::empty(),
            page_last_click: None,
            page_click_count: 1,
            page_cursor: crate::browser::engine::BrowserCursor::Default,
            last_shot: None,
            js_click: false,
            r3_latched: false,
            viewport: (
                crate::browser::cursor::VIEWPORT_W,
                crate::browser::cursor::VIEWPORT_H,
            ),
            debug_overlay: false,
            bookmarks: Vec::new(),
            l1_held: false,
            r1_held: false,
            triangle_held: false,
            url_editing: false,
            url_draft: String::new(),
            url_replace_on_type: false,
            grid: crate::ui::browser::grid::GridState::default(),
        }
    }
}

impl BrowserPanel {
    /// Back is available when either source agrees.
    ///
    /// CEF's capability catches redirects and same-document entries that the
    /// canonical stack has not absorbed yet; the session stack catches the
    /// brief window after a commit where CEF still reports the pre-navigation
    /// capability (observed as a falsely disabled Back hint after a search).
    pub fn can_go_back(&self) -> bool {
        self.page.last_engine_can.is_some_and(|(back, _)| back) || self.page.session.can_go_back()
    }

    /// Forward is available when either source agrees (same reasoning as
    /// [`Self::can_go_back`]).
    pub fn can_go_forward(&self) -> bool {
        self.page
            .last_engine_can
            .is_some_and(|(_, forward)| forward)
            || self.page.session.can_go_forward()
    }

    /// Match the surface to the live card page size. Returns true when the
    /// size changed (the engine must be resized to the same numbers).
    pub fn set_viewport(&mut self, w: f32, h: f32) -> bool {
        let size = (w.max(1.0), h.max(1.0));
        if self.viewport == size {
            return false;
        }
        self.viewport = size;
        self.page.cursor.set_viewport(size.0, size.1);
        true
    }

    /// Keep the live page's cursor bounds in step with the panel viewport.
    /// Page state (including the cursor) is swapped per tab and replaced on
    /// new/restored tabs, so a default page carries 800x600 bounds until this
    /// runs — the recurring "cursor trapped in a quarter of the page" bug.
    pub fn sync_live_cursor(&mut self) {
        self.page
            .cursor
            .set_viewport(self.viewport.0, self.viewport.1);
    }

    /// Replace the live page with a fresh one that already knows the live
    /// viewport. Prefer this over assigning `BrowserTabPage::default()`.
    pub fn reset_live_page(&mut self) {
        self.page = BrowserTabPage::default();
        self.sync_live_cursor();
    }

    /// Find a background tab's stored state.
    pub fn stored_mut(&mut self, id: crate::browser::engine::TabId) -> Option<&mut StoredTab> {
        self.stored.iter_mut().find(|t| t.id == id)
    }

    /// Stash the live page as a background tab under its current id. Called
    /// before the live slot is reset for a new/adopted tab or swapped with
    /// a stored one. The stashed page keeps its backing view, so it stays
    /// loaded.
    pub fn stash_live(&mut self) {
        let page = std::mem::take(&mut self.page);
        // `take` leaves a default page behind; give its cursor the live
        // bounds in case the caller has not installed the replacement yet.
        self.sync_live_cursor();
        let id = self.active_tab;
        if let Some(slot) = self.stored.iter_mut().find(|t| t.id == id) {
            slot.page = page;
            slot.loaded = true;
        } else {
            self.stored.push(StoredTab {
                id,
                page,
                loaded: true,
            });
            if !self.order.contains(&id) {
                self.order.push(id);
            }
        }
    }

    /// Load a background tab into the live slot: the live page swaps into
    /// the stored slot (order untouched), the stored page becomes live.
    /// Returns false for unknown ids.
    pub fn activate_stored(&mut self, id: crate::browser::engine::TabId) -> bool {
        if id == self.active_tab {
            return true;
        }
        let Some(pos) = self.stored.iter().position(|t| t.id == id) else {
            return false;
        };
        std::mem::swap(&mut self.page, &mut self.stored[pos].page);
        self.sync_live_cursor();
        self.stored[pos].id = self.active_tab;
        self.active_tab = id;
        true
    }

    /// Drop a background tab's stored state (the engine side closes via
    /// `CloseTab`). Returns false when the id is live or unknown.
    pub fn drop_stored(&mut self, id: crate::browser::engine::TabId) -> bool {
        if id == self.active_tab {
            return false;
        }
        let Some(pos) = self.stored.iter().position(|t| t.id == id) else {
            return false;
        };
        self.stored.remove(pos);
        self.order.retain(|o| *o != id);
        true
    }

    /// Snapshot the persisted page for `tabs.json`: URL, title, active
    /// position. Blank/empty URLs never persist (placeholders, not visits —
    /// same rule as the history stacks).
    pub fn snapshot_tabs(&self) -> crate::browser::tabs::SavedTabs {
        let mut tabs = Vec::with_capacity(self.order.len());
        let mut active = 0;
        for id in &self.order {
            if *id == self.active_tab {
                active = tabs.len();
            }
            let page = if *id == self.active_tab {
                Some(&self.page)
            } else {
                self.stored.iter().find(|t| t.id == *id).map(|t| &t.page)
            };
            let Some(page) = page else {
                continue;
            };
            let url = page.session.url();
            if url.is_empty() || url == crate::browser::BLANK_PAGE_URL {
                continue;
            }
            tabs.push(crate::browser::tabs::SavedTab {
                url: url.to_string(),
                title: page.session.title().unwrap_or_default().to_string(),
            });
        }
        tabs.truncate(crate::browser::tabs::MAX_SAVED_TABS);
        active = active.min(tabs.len().saturating_sub(1));
        crate::browser::tabs::SavedTabs { tabs, active }
    }

    /// Overwrite `tabs.json` with the current page.
    pub fn save_tabs(&self) {
        crate::browser::tabs::save(&self.snapshot_tabs());
    }

    /// Resolve any tab id to its page state (live or stored).
    pub fn tab_page(&self, id: crate::browser::engine::TabId) -> Option<&BrowserTabPage> {
        if id == self.active_tab {
            Some(&self.page)
        } else {
            self.stored
                .iter()
                .find(|tab| tab.id == id)
                .map(|tab| &tab.page)
        }
    }

    /// True when the tab has a live engine view (live tab always does).
    pub fn tab_loaded(&self, id: crate::browser::engine::TabId) -> bool {
        if id == self.active_tab {
            true
        } else {
            self.stored
                .iter()
                .find(|tab| tab.id == id)
                .is_some_and(|tab| tab.loaded)
        }
    }

    /// Activate a stored tab. The old live page lands in the target's stored
    /// slot under the old id (see [`Self::activate_stored`]).
    pub fn activate_tab(&mut self, id: crate::browser::engine::TabId) -> bool {
        self.activate_stored(id)
    }

    /// Remove a non-live tab after activation swapped the old live page into
    /// its slot. Also drops the closed id from the creation order.
    pub fn remove_stored_tab(&mut self, id: crate::browser::engine::TabId) -> bool {
        let before_stored = self.stored.len();
        let before_order = self.order.len();
        self.stored.retain(|tab| tab.id != id);
        self.order.retain(|tab| *tab != id);
        self.stored.len() != before_stored || self.order.len() != before_order
    }

    /// Freeze the tab-grid visual order for a new session (stable creation
    /// order; the active tab only sets focus/page).
    pub fn refresh_grid_visual(&mut self) {
        self.grid
            .refresh_visual(self.active_tab, &self.order.clone());
    }

    /// Keep the frozen visual strip aligned after tab open/close.
    pub fn sync_grid_visual(&mut self) {
        if !self.grid.is_grid() {
            return;
        }
        let valid: Vec<crate::browser::engine::TabId> = self
            .order
            .iter()
            .copied()
            .filter(|id| self.tab_page(*id).is_some())
            .collect();
        self.grid.visual.retain(|id| valid.contains(id));
        // Newest first: prepend newly-created tabs. Iterating creation order
        // means the newest insert lands at index 0.
        for id in valid {
            if !self.grid.visual.contains(&id) {
                self.grid.visual.insert(0, id);
            }
        }
    }

    /// Restore every persisted tab: the saved active page becomes the live
    /// boot page (the engine preloaded its URL), the rest become dormant
    /// stored pages that open their view on first activation. Returns false
    /// when the saved strip has no usable tab.
    pub fn restore_saved_strip(&mut self, saved: &crate::browser::tabs::SavedTabs) -> bool {
        let tabs: Vec<&crate::browser::tabs::SavedTab> = saved
            .tabs
            .iter()
            .filter(|tab| !tab.url.is_empty() && tab.url != crate::browser::BLANK_PAGE_URL)
            .take(crate::browser::tabs::MAX_SAVED_TABS)
            .collect();
        self.stored.clear();
        self.order.clear();
        self.active_tab = crate::browser::engine::TabId::BOOT;
        self.next_tab_id = 2;
        self.reset_live_page();
        if tabs.is_empty() {
            self.order.push(crate::browser::engine::TabId::BOOT);
            return false;
        }
        let active = saved
            .tabs
            .get(saved.active)
            .and_then(|saved_active| tabs.iter().position(|tab| tab.url == saved_active.url))
            .unwrap_or(0)
            .min(tabs.len() - 1);
        for (index, tab) in tabs.iter().enumerate() {
            let title = (!tab.title.is_empty()).then(|| tab.title.clone());
            if index == active {
                self.page.session.begin_load(tab.url.clone());
                if title.is_some() {
                    self.page.session.note_title(title);
                }
                self.order.push(crate::browser::engine::TabId::BOOT);
            } else {
                let id = crate::browser::engine::TabId(self.next_tab_id);
                self.next_tab_id += 1;
                let mut page = BrowserTabPage::default();
                page.session.note_url(tab.url.clone());
                if title.is_some() {
                    page.session.note_title(title);
                }
                self.stored.push(StoredTab {
                    id,
                    page,
                    loaded: false,
                });
                self.order.push(id);
            }
        }
        true
    }

    /// Create a fresh live tab after stashing the current page. Returns the
    /// new id and the load generation the engine result must carry.
    pub fn create_tab(&mut self, url: String) -> (crate::browser::engine::TabId, u64) {
        self.stash_live();
        let id = crate::browser::engine::TabId(self.next_tab_id);
        self.next_tab_id += 1;
        self.active_tab = id;
        self.order.push(id);
        self.reset_live_page();
        self.page.session.open_surface();
        let generation = self.page.session.begin_load(url);
        (id, generation)
    }

    /// True when this URL has a bookmark entry.
    pub fn is_bookmarked(&self, url: &str) -> bool {
        self.bookmarks.iter().any(|bookmark| bookmark.url == url)
    }

    /// Add or remove the bookmark for `url`. Returns the new bookmarked state.
    pub fn toggle_bookmark(&mut self, url: &str, title: &str) -> bool {
        if let Some(index) = self
            .bookmarks
            .iter()
            .position(|bookmark| bookmark.url == url)
        {
            self.bookmarks.remove(index);
            crate::browser::bookmarks::save(&self.bookmarks);
            false
        } else {
            self.bookmarks.push(crate::browser::bookmarks::Bookmark {
                url: url.to_string(),
                title: title.to_string(),
            });
            crate::browser::bookmarks::save(&self.bookmarks);
            true
        }
    }

    /// Remove the bookmark at `index` (bookmark-row Triangle). Returns true
    /// when one was removed.
    pub fn remove_bookmark(&mut self, index: usize) -> bool {
        if index >= self.bookmarks.len() {
            return false;
        }
        self.bookmarks.remove(index);
        crate::browser::bookmarks::save(&self.bookmarks);
        true
    }

    pub fn toggle(&mut self) {
        if self.session.is_open() {
            self.session.close_surface();
        } else {
            self.session.open_surface();
        }
    }

    pub fn close_surface(&mut self) {
        self.session.close_surface();
        self.pending_focus = None;
        // Drop the analog clock so reopening never applies a huge first dt.
        self.cursor.reset_analog();
        self.cancel_url_edit();
        self.l1_held = false;
        self.r1_held = false;
        self.triangle_held = false;
    }

    /// Begin editing the URL. The current page URL is prefilled; the first
    /// typed character replaces it so the controller keyboard does not need
    /// select-all.
    pub fn start_url_edit(&mut self) {
        self.url_editing = true;
        self.url_draft = self.page.session.url().to_string();
        self.url_replace_on_type = true;
    }

    /// Clear URL editing state without touching the OSK (the caller owns that).
    pub fn cancel_url_edit(&mut self) {
        self.url_editing = false;
        self.url_draft.clear();
        self.url_replace_on_type = false;
    }

    /// Append committed text to the URL draft, replacing the prefilled URL on
    /// the first edit.
    pub fn url_edit_insert(&mut self, text: &str) {
        if !self.url_editing {
            return;
        }
        if self.url_replace_on_type {
            self.url_draft.clear();
            self.url_replace_on_type = false;
        }
        self.url_draft.push_str(text);
    }

    /// Backspace in the URL draft. Backspace on the prefilled selection clears
    /// it, matching a text field with all text selected.
    pub fn url_edit_backspace(&mut self) {
        if !self.url_editing {
            return;
        }
        if self.url_replace_on_type {
            self.url_draft.clear();
            self.url_replace_on_type = false;
            return;
        }
        self.url_draft.pop();
    }

    pub fn note_scrolled(&mut self, y: f32) {
        self.scroll_y = y.max(0.0);
    }

    /// Step the body scroll (reader/non-engine text); returns the new absolute
    /// offset.
    pub fn scroll_by(&mut self, delta: f32) -> f32 {
        self.scroll_y = (self.scroll_y + delta).max(0.0);
        self.scroll_y
    }

    /// Integrate one analog pad sample through the shared cursor runtime.
    ///
    /// Production uses the pull-model cursor surface; this wrapper remains for
    /// tests and for callers that need a deterministic single step.
    pub fn advance_stick(
        &mut self,
        lx: f32,
        ly: f32,
        rx: f32,
        ry: f32,
        r2: bool,
        now: Instant,
    ) -> (f32, f32) {
        let mut sample = crate::domain::pad::PadSample {
            lstick_x: lx,
            lstick_y: ly,
            rstick_x: rx,
            rstick_y: ry,
            r2,
            ..Default::default()
        };
        sample.stick_x = lx;
        sample.stick_y = ly;
        let mut rt = self.page.cursor.lock();
        rt.focusables = self.page.focusables.clone();
        rt.advance(&sample, now)
            .scroll
            .map(|(dx, dy, _)| (dx, dy))
            .unwrap_or((0.0, 0.0))
    }

    /// D-pad focus step: spatial cone jump over page targets. A press always
    /// asks the engine for a fresh target set around the new cursor.
    pub fn jump_cursor(&mut self, dir: crate::browser::cursor::JumpDir) {
        let before = self.page.cursor.snapshot();
        self.page
            .cursor
            .set_focusables(self.page.focusables.clone());
        self.page.cursor.jump(dir);
        let after = self.page.cursor.snapshot();
        crate::platform::app_log::info(format!(
            "cursor jump {dir:?} from=({:.0},{:.0}) dock={:?} fly={:?} attached={:?} targets={}",
            before.x,
            before.y,
            before.attached,
            after.fly_to,
            after.attached,
            self.page.focusables.len(),
        ));
    }
}

/// Browser popup geometry: the card itself is 16:9, sized from the
/// available viewport height (clamped by width). The header row sits above
/// the page surface inside the card.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrowserLayout {
    pub page_w: f32,
    pub page_h: f32,
    pub card_w: f32,
    pub card_h: f32,
}

pub fn browser_layout(stage_w: f32, stage_h: f32) -> BrowserLayout {
    let avail_w = (stage_w - 2.0 * BROWSER_OUTER_MARGIN).max(320.0);
    let avail_h = (stage_h - 2.0 * BROWSER_OUTER_MARGIN).max(240.0);
    let mut card_h = avail_h;
    let mut card_w = card_h * 16.0 / 9.0;
    if card_w > avail_w {
        card_w = avail_w;
        card_h = card_w * 9.0 / 16.0;
    }
    let page_w = (card_w - 2.0 * BROWSER_CARD_PAD).max(1.0);
    let page_h = (card_h - 2.0 * BROWSER_CARD_PAD - BROWSER_HEADER_TOTAL).max(1.0);
    BrowserLayout {
        page_w: page_w.round(),
        page_h: page_h.round(),
        card_w: card_w.round(),
        card_h: card_h.round(),
    }
}

/// Horizontal split for the centred header group.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrowserHeaderLayout {
    /// Equal side slots for the Back/Forward clusters.
    pub side_w: f32,
    /// URL well width.
    pub url_w: f32,
}

/// URL well + equal side slots for the available header content width.
///
/// The well is clamped to 420–680px. Side slots keep the URL exactly centred;
/// on narrow stages they shrink so the group still fits.
pub fn browser_header_layout(content_w: f32) -> BrowserHeaderLayout {
    let content_w = content_w.max(1.0);
    let available = content_w - 2.0 * BROWSER_HEADER_GROUP_GAP - 2.0 * BROWSER_HEADER_SIDE_W;
    let url_w = available.clamp(BROWSER_URL_W_MIN, BROWSER_URL_W_MAX);
    let available_side = (content_w - 2.0 * BROWSER_HEADER_GROUP_GAP - url_w).max(0.0) * 0.5;
    let side_w = BROWSER_HEADER_SIDE_W.min(available_side);
    BrowserHeaderLayout { side_w, url_w }
}

/// URL text budget inside the well: subtract padding, globe, gaps, and the
/// right-anchored triangle hint slot. Extra pixels keep text off the border.
pub fn browser_url_text_budget(url_w: f32, editing: bool) -> f32 {
    let mut reserved = 2.0 * BROWSER_URL_BAR_PAD + BROWSER_URL_ICON + BROWSER_URL_GAP + 2.0;
    if !editing {
        reserved += BROWSER_URL_GAP + BROWSER_HINT_SLOT;
    }
    (url_w - reserved).max(0.0)
}

/// Human-facing URL in the header: drop the scheme and turn the blank/default
/// document into a stable label. The full URL is preserved for editing.
pub fn display_url(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() || trimmed == crate::browser::BLANK_PAGE_URL {
        return "Start page".to_string();
    }
    trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed)
        .to_string()
}

/// Outer breather against the screen edge: 50px per side (height - 100).
pub const BROWSER_OUTER_MARGIN: f32 = 50.0;
/// Card padding around the browser surface.
pub const BROWSER_CARD_PAD: f32 = 4.0;

/// Favicon handle cache cap kept for the dormant bookmark data (no v0 UI).
pub const MAX_FAVICONS: usize = 64;

/// Dormant favicon handles by origin; retained so bookmark data can regain
/// icons without touching the engine event path.
pub type FaviconCache = HashMap<String, iced::widget::image::Handle>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn panel() -> BrowserPanel {
        let panel = BrowserPanel {
            viewport: (900.0, 700.0),
            ..BrowserPanel::default()
        };
        panel.page.cursor.lock().cursor.x = 450.0;
        panel.page.cursor.lock().cursor.y = 350.0;
        panel
    }

    fn step(panel: &mut BrowserPanel, rx: f32, ry: f32, r2: bool, at: Instant) -> (f32, f32) {
        panel.advance_stick(0.0, 0.0, rx, ry, r2, at)
    }

    fn engine_entry(url: &str) -> crate::browser::engine::EngineHistoryEntry {
        crate::browser::engine::EngineHistoryEntry {
            url: url.to_string(),
            display_url: url.to_string(),
            original_url: url.to_string(),
            title: None,
            has_post_data: false,
        }
    }

    #[test]
    fn history_hint_uses_either_the_stack_or_engine_capability() {
        let mut panel = panel();
        // CEF briefly reports no capability after a commit while the canonical
        // stack already has the new entry: the hint must stay enabled.
        panel.page.last_engine_can = Some((false, false));
        panel.session.apply_engine_history(
            &[
                engine_entry("https://a.test/"),
                engine_entry("https://b.test/"),
            ],
            1,
        );
        assert!(panel.can_go_back());
        assert!(!panel.can_go_forward());

        // Engine capability can also lead the stack (redirects/same-document).
        panel.session = crate::browser::BrowserSession::default();
        panel.page.last_engine_can = Some((false, true));
        assert!(!panel.can_go_back());
        assert!(panel.can_go_forward());

        // Both sources agree there is nowhere to go.
        panel.page.last_engine_can = Some((false, false));
        assert!(!panel.can_go_back());
        assert!(!panel.can_go_forward());
    }

    #[test]
    fn scroll_locks_to_dominant_vertical_axis() {
        let mut panel = panel();
        let t0 = Instant::now();
        step(&mut panel, 0.0, 0.0, false, t0);
        let at = t0 + Duration::from_millis(100);
        let (dx, dy) = step(&mut panel, 0.4, -0.8, false, at);
        assert_eq!(dx, 0.0);
        assert!(dy < 0.0);
        // Same gesture, now deflected mostly sideways: still vertical.
        let at = at + Duration::from_millis(100);
        let (dx, dy) = step(&mut panel, 0.9, -0.2, false, at);
        assert_eq!(dx, 0.0);
        assert!(dy < 0.0);
    }

    #[test]
    fn scroll_locks_to_dominant_horizontal_axis() {
        let mut panel = panel();
        let t0 = Instant::now();
        step(&mut panel, 0.0, 0.0, false, t0);
        let at = t0 + Duration::from_millis(100);
        let (dx, dy) = step(&mut panel, 0.9, -0.2, false, at);
        assert!(dx > 0.0);
        assert_eq!(dy, 0.0);
    }

    #[test]
    fn neutral_release_unlocks_axis() {
        let mut panel = panel();
        let t0 = Instant::now();
        step(&mut panel, 0.0, 0.0, false, t0);
        let at = t0 + Duration::from_millis(100);
        let (dx, dy) = step(&mut panel, 0.2, -0.9, false, at);
        assert_eq!(dx, 0.0);
        assert!(dy < 0.0);
        let at = at + Duration::from_millis(100);
        step(&mut panel, 0.0, 0.0, false, at);
        assert_eq!(panel.cursor.lock().scroll_axis, None);
        let at = at + Duration::from_millis(100);
        let (dx, dy) = step(&mut panel, 0.9, -0.2, false, at);
        assert!(dx > 0.0);
        assert_eq!(dy, 0.0);
    }

    #[test]
    fn releasing_locked_axis_adopts_the_other() {
        let mut panel = panel();
        let t0 = Instant::now();
        step(&mut panel, 0.0, 0.0, false, t0);
        let at = t0 + Duration::from_millis(100);
        step(&mut panel, 0.2, -0.9, false, at);
        let at = at + Duration::from_millis(100);
        let (dx, dy) = step(&mut panel, 0.9, 0.0, false, at);
        assert!(dx > 0.0);
        assert_eq!(dy, 0.0);
    }

    #[test]
    fn turbo_doubles_scroll_speed() {
        let mut panel = panel();
        let t0 = Instant::now();
        step(&mut panel, 0.0, 0.0, false, t0);
        let at = t0 + Duration::from_millis(100);
        let (_, normal) = step(&mut panel, 0.0, -0.5, false, at);
        let at = at + Duration::from_millis(100);
        step(&mut panel, 0.0, 0.0, false, at);
        let at = at + Duration::from_millis(100);
        let (_, turbo) = step(&mut panel, 0.0, -0.5, true, at);
        assert!((turbo - normal * 2.0).abs() < 0.01);
    }

    #[test]
    fn dpad_jump_steps_through_page_targets() {
        use crate::browser::cursor::{FocusKind, Focusable};
        let mut panel = panel();
        panel.page.focusables = vec![
            Focusable {
                id: 1,
                x: 100.0,
                y: 100.0,
                w: 100.0,
                h: 40.0,
                kind: FocusKind::Link,
                url: None,
            },
            Focusable {
                id: 2,
                x: 100.0,
                y: 300.0,
                w: 100.0,
                h: 40.0,
                kind: FocusKind::Link,
                url: None,
            },
        ];
        panel.page.cursor.lock().cursor.x = 150.0;
        panel.page.cursor.lock().cursor.y = 320.0;
        panel.page.cursor.lock().cursor.attached = Some(2);
        panel.jump_cursor(crate::browser::cursor::JumpDir::Up);
        assert!(panel.cursor.lock().dom_reseed_pending);
        assert_eq!(panel.cursor.snapshot().fly_to, Some((150.0, 120.0)));
    }

    #[test]
    fn snapshot_persists_the_live_page_only() {
        let mut panel = BrowserPanel::default();
        panel.session.begin_load("https://a.test/".to_string());
        panel.session.note_title(Some("A".to_string()));
        let snap = panel.snapshot_tabs();
        assert_eq!(snap.tabs.len(), 1);
        assert_eq!(snap.tabs[0].url, "https://a.test/");
        assert_eq!(snap.tabs[0].title, "A");
        assert_eq!(snap.active, 0);
        // Blank/empty pages never persist.
        panel
            .session
            .note_url(crate::browser::BLANK_PAGE_URL.to_string());
        assert!(panel.snapshot_tabs().tabs.is_empty());
    }

    #[test]
    fn store_activation_round_trips_page_state() {
        let mut panel = BrowserPanel::default();
        assert_eq!(panel.active_tab, crate::browser::engine::TabId::BOOT);
        panel.session.begin_load("https://a.test/".to_string());
        panel.stash_live();
        assert_eq!(panel.stored.len(), 1);
        assert_eq!(panel.stored[0].id, crate::browser::engine::TabId::BOOT);
        assert_eq!(panel.stored[0].page.session.url(), "https://a.test/");
        assert!(panel.stored[0].loaded);
        let second = crate::browser::engine::TabId(panel.next_tab_id);
        panel.next_tab_id += 1;
        panel.active_tab = second;
        panel.order.push(second);
        panel.session.begin_load("https://b.test/".to_string());
        assert!(panel.activate_stored(crate::browser::engine::TabId::BOOT));
        assert_eq!(panel.active_tab, crate::browser::engine::TabId::BOOT);
        assert_eq!(panel.session.url(), "https://a.test/");
        assert_eq!(panel.stored.len(), 1);
        assert_eq!(panel.stored[0].id, second);
        assert_eq!(panel.stored[0].page.session.url(), "https://b.test/");
        assert!(!panel.activate_stored(crate::browser::engine::TabId(999)));
        assert!(!panel.drop_stored(crate::browser::engine::TabId(999)));
        assert!(panel.drop_stored(second));
        assert!(panel.stored.is_empty());
        assert!(!panel.drop_stored(crate::browser::engine::TabId::BOOT));
    }

    #[test]
    fn restored_strip_keeps_active_live_and_others_dormant() {
        let saved = crate::browser::tabs::SavedTabs {
            tabs: vec![
                crate::browser::tabs::SavedTab {
                    url: "https://a.test/".into(),
                    title: "A".into(),
                },
                crate::browser::tabs::SavedTab {
                    url: "https://b.test/".into(),
                    title: "B".into(),
                },
                crate::browser::tabs::SavedTab {
                    url: "https://c.test/".into(),
                    title: "C".into(),
                },
            ],
            active: 1,
        };
        let mut panel = BrowserPanel::default();
        assert!(panel.restore_saved_strip(&saved));
        assert_eq!(panel.active_tab, crate::browser::engine::TabId::BOOT);
        assert_eq!(panel.session.url(), "https://b.test/");
        assert_eq!(panel.session.title(), Some("B"));
        assert_eq!(panel.order.len(), 3);
        assert_eq!(panel.stored.len(), 2);
        assert!(panel.stored.iter().all(|tab| !tab.loaded));
        let urls: Vec<&str> = panel
            .order
            .iter()
            .filter_map(|id| panel.tab_page(*id).map(|page| page.session.url()))
            .collect();
        assert_eq!(
            urls,
            vec!["https://a.test/", "https://b.test/", "https://c.test/"]
        );
    }

    #[test]
    fn create_tab_stashes_live_and_foregrounds_new_page() {
        let mut panel = BrowserPanel::default();
        panel.session.begin_load("https://a.test/".to_string());
        let (id, generation) = panel.create_tab("https://b.test/".to_string());
        assert_ne!(id, crate::browser::engine::TabId::BOOT);
        assert_eq!(panel.active_tab, id);
        assert_eq!(panel.session.url(), "https://b.test/");
        assert_eq!(panel.session.generation(), generation);
        assert_eq!(panel.order, vec![crate::browser::engine::TabId::BOOT, id]);
        assert_eq!(panel.stored.len(), 1);
        assert_eq!(panel.stored[0].page.session.url(), "https://a.test/");
        assert!(panel.tab_loaded(crate::browser::engine::TabId::BOOT));
    }

    #[test]
    fn bookmark_match_is_exact_url() {
        let panel = BrowserPanel {
            bookmarks: vec![crate::browser::bookmarks::Bookmark {
                url: "https://a.test/".into(),
                title: "A".into(),
            }],
            ..BrowserPanel::default()
        };
        assert!(panel.is_bookmarked("https://a.test/"));
        assert!(!panel.is_bookmarked("https://a.test/next"));
        assert!(!panel.is_bookmarked(""));
    }

    #[test]
    fn header_sits_inside_the_16_9_card() {
        let l = browser_layout(1920.0, 1080.0);
        assert_eq!(l.card_h, 980.0);
        assert_eq!(
            l.page_h + 2.0 * BROWSER_CARD_PAD + BROWSER_HEADER_TOTAL,
            l.card_h
        );
        assert_eq!(l.page_w + 2.0 * BROWSER_CARD_PAD, l.card_w);
    }

    #[test]
    fn url_width_is_clamped_and_side_slots_keep_it_centred() {
        let wide = browser_header_layout(1710.0);
        assert_eq!(wide.url_w, BROWSER_URL_W_MAX);
        assert_eq!(wide.side_w, BROWSER_HEADER_SIDE_W);
        let wide_group = wide.side_w * 2.0 + wide.url_w + 2.0 * BROWSER_HEADER_GROUP_GAP;
        assert_eq!(wide_group, 968.0);
        assert!(
            wide_group < 1710.0,
            "group stays centred with air either side"
        );

        let narrow = browser_header_layout(668.0);
        assert_eq!(narrow.url_w, BROWSER_URL_W_MIN);
        assert!(narrow.side_w < BROWSER_HEADER_SIDE_W);
        assert_eq!(
            narrow.side_w * 2.0 + narrow.url_w + 2.0 * BROWSER_HEADER_GROUP_GAP,
            668.0
        );
    }

    #[test]
    fn header_vertical_padding_is_symmetric() {
        assert_eq!(
            BROWSER_HEADER_H - BROWSER_URL_BAR_H,
            2.0 * BROWSER_HEADER_V_PAD
        );
    }

    #[test]
    fn url_budget_reserves_the_triangle_slot_when_idle() {
        let idle = browser_url_text_budget(600.0, false);
        let editing = browser_url_text_budget(600.0, true);
        assert_eq!(
            editing - idle,
            BROWSER_URL_GAP + BROWSER_HINT_SLOT,
            "editing hides the triangle and reclaims its slot"
        );
        assert!(idle > 0.0);
    }

    #[test]
    fn display_url_strips_scheme_and_labels_blank() {
        assert_eq!(display_url("https://example.com/a"), "example.com/a");
        assert_eq!(display_url("http://example.com/a"), "example.com/a");
        assert_eq!(display_url("example.com/a"), "example.com/a");
        assert_eq!(display_url(""), "Start page");
        assert_eq!(display_url(crate::browser::BLANK_PAGE_URL), "Start page");
    }

    #[test]
    fn url_edit_replaces_prefill_then_backspaces() {
        let mut panel = BrowserPanel::default();
        panel.session.begin_load("https://a.test/".to_string());
        panel.start_url_edit();
        assert_eq!(panel.url_draft, "https://a.test/");
        assert!(panel.url_replace_on_type);
        panel.url_edit_insert("example.com");
        assert_eq!(panel.url_draft, "example.com");
        panel.url_edit_insert("/path");
        assert_eq!(panel.url_draft, "example.com/path");
        panel.url_edit_backspace();
        assert_eq!(panel.url_draft, "example.com/pat");
        panel.cancel_url_edit();
        assert!(!panel.url_editing);
        assert!(panel.url_draft.is_empty());
    }
}
