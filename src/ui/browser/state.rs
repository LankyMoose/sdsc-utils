//! Browser app state: page state, cursor, text keyboard, tab strip, grid, and
//! bookmarks.
//!
//! Page mode has one header row (Back / URL / Forward). Circle swaps the
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

/// One tab. Page state and engine attachment travel with the tab identity;
/// there is no separate "live" copy to swap, so idle tabs stay coherent.
#[derive(Debug, Clone)]
pub struct BrowserTab {
    pub id: crate::browser::engine::TabId,
    pub page: BrowserTabPage,
    /// The engine worker has a browser view for this tab. This is about view
    /// existence, not whether the page content has finished loading.
    pub engine_attached: bool,
}

/// Silent result of closing one tab. The caller owns the single UI cue, so the
/// close operation itself never sounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabCloseOutcome {
    /// A real close. `next` is the tab active afterwards.
    Closed { next: crate::browser::engine::TabId },
    /// The final tab: reset it to the default page instead of leaving none.
    ResetDefault,
    /// Unknown id or nothing to close.
    Rejected,
}

/// A main-frame navigation cancelled by the security policy. The card is
/// session state, never persisted; `stay_focused` defaults to Stay.
#[derive(Debug, Clone)]
pub struct BlockedNavigation {
    pub source: String,
    pub target: String,
    pub reason: String,
    pub can_continue: bool,
    pub token: Option<u64>,
    pub stay_focused: bool,
    pub remember: bool,
}

/// Browser app panel. The page surface and cursor live here; load lifecycle
/// (generation-guarded suspend/resume) lives in
/// [`crate::browser::BrowserSession`]. The pad cursor roams in engine page
/// pixels (see [`crate::browser::cursor`]).
#[derive(Debug, Clone)]
pub struct BrowserPanel {
    /// Every tab in creation order. The active one is `active_tab`; page
    /// state is never swapped between entries.
    pub tabs: Vec<BrowserTab>,
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
    /// Tabs-view mode and FLIP transition.
    pub grid: crate::ui::browser::grid::GridState,
    /// Stay/Continue card for the active tab.
    pub nav_block: Option<BlockedNavigation>,
    /// Pending cards for background tabs (per-tab, so a background redirect
    /// never paints over the active page).
    pub nav_blocks: HashMap<crate::browser::engine::TabId, BlockedNavigation>,
}

impl std::ops::Deref for BrowserPanel {
    type Target = BrowserTabPage;

    /// Page state is always the active tab's — `panel.session`,
    /// `panel.cursor` and friends address it directly.
    fn deref(&self) -> &Self::Target {
        &self.active().page
    }
}

impl std::ops::DerefMut for BrowserPanel {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.active_mut().page
    }
}

impl Default for BrowserPanel {
    /// Fresh panel: one implicit boot tab (matches the worker's
    /// `TabId::BOOT`); the next explicit tab takes id 2.
    fn default() -> Self {
        let page = BrowserTabPage::default();
        Self {
            tabs: vec![BrowserTab {
                id: crate::browser::engine::TabId::BOOT,
                page,
                engine_attached: true,
            }],
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
            nav_block: None,
            nav_blocks: HashMap::new(),
        }
    }
}

impl BrowserPanel {
    fn active_index(&self) -> usize {
        self.tabs
            .iter()
            .position(|tab| tab.id == self.active_tab)
            .unwrap_or(0)
    }

    /// The active tab record.
    pub fn active(&self) -> &BrowserTab {
        &self.tabs[self.active_index()]
    }

    /// The active tab record, mutable.
    pub fn active_mut(&mut self) -> &mut BrowserTab {
        let index = self.active_index();
        &mut self.tabs[index]
    }

    /// Active page state (same as `panel` through `Deref`), for call sites
    /// that already have a `&BrowserPanel` and cannot use field access.
    pub fn active_page(&self) -> &BrowserTabPage {
        &self.active().page
    }

    /// Back is available when either source agrees.
    ///
    /// CEF's capability catches redirects and same-document entries that the
    /// canonical stack has not absorbed yet; the session stack catches the
    /// brief window after a commit where CEF still reports the pre-navigation
    /// capability (observed as a falsely disabled Back hint after a search).
    pub fn can_go_back(&self) -> bool {
        self.last_engine_can.is_some_and(|(back, _)| back) || self.session.can_go_back()
    }

    /// Forward is available when either source agrees (same reasoning as
    /// [`Self::can_go_back`]).
    pub fn can_go_forward(&self) -> bool {
        self.last_engine_can.is_some_and(|(_, forward)| forward) || self.session.can_go_forward()
    }

    /// Match the surface to the live card page size. Returns true when the
    /// size changed (the engine must be resized to the same numbers).
    pub fn set_viewport(&mut self, w: f32, h: f32) -> bool {
        let size = (w.max(1.0), h.max(1.0));
        if self.viewport == size {
            return false;
        }
        self.viewport = size;
        self.cursor.set_viewport(size.0, size.1);
        true
    }

    /// Keep the active page's cursor bounds in step with the panel viewport.
    /// Page state travels with its tab, so activating a dormant or restored
    /// tab re-syncs the cursor before it can trap input in the page default.
    pub fn sync_live_cursor(&mut self) {
        self.cursor.set_viewport(self.viewport.0, self.viewport.1);
    }

    /// Replace the active page with a fresh one that already knows the live
    /// viewport. Prefer this over assigning `BrowserTabPage::default()`.
    pub fn reset_live_page(&mut self) {
        self.active_mut().page = BrowserTabPage::default();
        self.sync_live_cursor();
    }

    /// Find a tab by id.
    pub fn tab_mut(&mut self, id: crate::browser::engine::TabId) -> Option<&mut BrowserTab> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    /// Foreground a tab. Page state stays in its tab record; only the active
    /// id changes, so a tab that was already attached keeps its view.
    pub fn activate_tab(&mut self, id: crate::browser::engine::TabId) -> bool {
        if !self.tabs.iter().any(|tab| tab.id == id) {
            return false;
        }
        if id == self.active_tab {
            return true;
        }
        if let Some(block) = self.nav_block.take() {
            self.nav_blocks.insert(self.active_tab, block);
        }
        self.active_tab = id;
        self.nav_block = self.nav_blocks.remove(&id);
        self.sync_live_cursor();
        true
    }

    /// Store a blocked-navigation card with its tab. Remembered/silent
    /// blocks never create a card.
    pub fn note_navigation_blocked(
        &mut self,
        tab: crate::browser::engine::TabId,
        block: BlockedNavigation,
    ) {
        if !block.can_continue {
            return;
        }
        if tab == self.active_tab {
            self.nav_block = Some(block);
        } else {
            self.nav_blocks.insert(tab, block);
        }
    }

    /// Clear the active card, keeping background cards.
    pub fn clear_nav_block(&mut self) {
        self.nav_block = None;
    }

    /// Remove a tab record (the engine side closes via `CloseTab`). If the
    /// removed tab was active, selection moves to the nearest remaining tab.
    /// The last tab is never removed; close resets it to the default page.
    pub fn remove_tab(&mut self, id: crate::browser::engine::TabId) -> bool {
        let Some(pos) = self.tabs.iter().position(|tab| tab.id == id) else {
            return false;
        };
        if self.tabs.len() <= 1 {
            return false;
        }
        self.tabs.remove(pos);
        self.nav_blocks.remove(&id);
        if self.active_tab == id {
            let next = pos.min(self.tabs.len() - 1);
            self.active_tab = self.tabs[next].id;
            self.nav_block = self.nav_blocks.remove(&self.active_tab);
            self.sync_live_cursor();
        }
        true
    }

    /// Snapshot the persisted page for `tabs.json`: URL, title, active
    /// position. Blank/empty URLs never persist (placeholders, not visits —
    /// same rule as the history stacks).
    pub fn snapshot_tabs(&self) -> crate::browser::tabs::SavedTabs {
        let mut tabs = Vec::with_capacity(self.tabs.len());
        let mut active = 0;
        for tab in &self.tabs {
            let url = tab.page.session.url();
            if url.is_empty() || url == crate::browser::BLANK_PAGE_URL {
                continue;
            }
            if tab.id == self.active_tab {
                active = tabs.len();
            }
            tabs.push(crate::browser::tabs::SavedTab {
                url: url.to_string(),
                title: tab.page.session.title().unwrap_or_default().to_string(),
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

    /// Decide what closing `id` would do without mutating anything. The close
    /// action is a silent state transition; this is the outcome it reports.
    pub fn tab_close_outcome(&self, id: crate::browser::engine::TabId) -> TabCloseOutcome {
        if self.tab_page(id).is_none() {
            return TabCloseOutcome::Rejected;
        }
        if id != self.active_tab {
            return TabCloseOutcome::Closed {
                next: self.active_tab,
            };
        }
        self.tabs
            .iter()
            .map(|tab| tab.id)
            .find(|tab| *tab != id)
            .map_or(TabCloseOutcome::ResetDefault, |next| {
                TabCloseOutcome::Closed { next }
            })
    }

    /// Resolve any tab id to its page state.
    pub fn tab_page(&self, id: crate::browser::engine::TabId) -> Option<&BrowserTabPage> {
        self.tabs
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| &tab.page)
    }

    /// True when the engine worker has a view for this tab.
    pub fn tab_loaded(&self, id: crate::browser::engine::TabId) -> bool {
        self.tabs
            .iter()
            .find(|tab| tab.id == id)
            .is_some_and(|tab| tab.engine_attached)
    }

    /// Freeze the tab-grid visual order for a new session (stable creation
    /// order; the active tab only sets focus/page).
    pub fn refresh_grid_visual(&mut self) {
        let order: Vec<_> = self.tabs.iter().map(|tab| tab.id).collect();
        self.grid.refresh_visual(self.active_tab, &order);
    }

    /// Keep the frozen visual strip aligned after tab open/close.
    pub fn sync_grid_visual(&mut self) {
        if !self.grid.is_grid() {
            return;
        }
        let valid: Vec<crate::browser::engine::TabId> =
            self.tabs.iter().map(|tab| tab.id).collect();
        self.grid.visual.retain(|id| valid.contains(id));
        // Newest first: prepend newly-created tabs. Iterating creation order
        // means the newest insert lands at index 0.
        for id in valid {
            if !self.grid.visual.contains(&id) {
                self.grid.visual.insert(0, id);
            }
        }
    }

    /// Restore every persisted tab. The saved active page is the boot view
    /// (the engine preloaded its URL); the others start detached and open on
    /// first activation. Returns false when the saved strip has no usable tab.
    pub fn restore_saved_strip(&mut self, saved: &crate::browser::tabs::SavedTabs) -> bool {
        let tabs: Vec<&crate::browser::tabs::SavedTab> = saved
            .tabs
            .iter()
            .filter(|tab| !tab.url.is_empty() && tab.url != crate::browser::BLANK_PAGE_URL)
            .take(crate::browser::tabs::MAX_SAVED_TABS)
            .collect();
        self.tabs.clear();
        self.active_tab = crate::browser::engine::TabId::BOOT;
        self.next_tab_id = 2;
        self.nav_block = None;
        self.nav_blocks.clear();
        if tabs.is_empty() {
            self.tabs.push(BrowserTab {
                id: crate::browser::engine::TabId::BOOT,
                page: BrowserTabPage::default(),
                engine_attached: true,
            });
            self.sync_live_cursor();
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
                let mut page = BrowserTabPage::default();
                page.session.begin_load(tab.url.clone());
                if title.is_some() {
                    page.session.note_title(title);
                }
                self.tabs.push(BrowserTab {
                    id: crate::browser::engine::TabId::BOOT,
                    page,
                    engine_attached: true,
                });
            } else {
                let id = crate::browser::engine::TabId(self.next_tab_id);
                self.next_tab_id += 1;
                let mut page = BrowserTabPage::default();
                page.session.note_url(tab.url.clone());
                if title.is_some() {
                    page.session.note_title(title);
                }
                self.tabs.push(BrowserTab {
                    id,
                    page,
                    engine_attached: false,
                });
            }
        }
        self.sync_live_cursor();
        true
    }

    /// Create a fresh foreground tab. Returns the new id and the load
    /// generation the engine result must carry. The caller opens the engine
    /// view and marks the tab attached.
    pub fn create_tab(&mut self, url: String) -> (crate::browser::engine::TabId, u64) {
        self.nav_block = None;
        let id = crate::browser::engine::TabId(self.next_tab_id);
        self.next_tab_id += 1;
        let mut page = BrowserTabPage::default();
        page.cursor.set_viewport(self.viewport.0, self.viewport.1);
        page.session.open_surface();
        let generation = page.session.begin_load(url);
        self.tabs.push(BrowserTab {
            id,
            page,
            engine_attached: false,
        });
        self.active_tab = id;
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
        self.url_draft = self.session.url().to_string();
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
        let mut rt = self.cursor.lock();
        rt.focusables = self.focusables.clone();
        rt.advance(&sample, now)
            .scroll
            .map(|(dx, dy, _)| (dx, dy))
            .unwrap_or((0.0, 0.0))
    }

    /// D-pad focus step: spatial cone jump over page targets. A press always
    /// asks the engine for a fresh target set around the new cursor.
    pub fn jump_cursor(&mut self, dir: crate::browser::cursor::JumpDir) {
        let before = self.cursor.snapshot();
        self.cursor.set_focusables(self.focusables.clone());
        self.cursor.jump(dir);
        let after = self.cursor.snapshot();
        crate::platform::app_log::info(format!(
            "cursor jump {dir:?} from=({:.0},{:.0}) dock={:?} fly={:?} attached={:?} targets={}",
            before.x,
            before.y,
            before.attached,
            after.fly_to,
            after.attached,
            self.focusables.len(),
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
        panel.cursor.lock().cursor.x = 450.0;
        panel.cursor.lock().cursor.y = 350.0;
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
        panel.last_engine_can = Some((false, false));
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
        panel.last_engine_can = Some((false, true));
        assert!(!panel.can_go_back());
        assert!(panel.can_go_forward());

        // Both sources agree there is nowhere to go.
        panel.last_engine_can = Some((false, false));
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
        panel.focusables = vec![
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
        panel.cursor.lock().cursor.x = 150.0;
        panel.cursor.lock().cursor.y = 320.0;
        panel.cursor.lock().cursor.attached = Some(2);
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
    fn tab_switch_keeps_page_state_and_engine_attachment() {
        let mut panel = BrowserPanel::default();
        assert_eq!(panel.active_tab, crate::browser::engine::TabId::BOOT);
        panel.session.begin_load("https://a.test/".to_string());
        let (second, _) = panel.create_tab("https://b.test/".to_string());
        assert_eq!(panel.tabs.len(), 2);
        assert_eq!(panel.tabs[0].page.session.url(), "https://a.test/");
        assert!(panel.tabs[0].engine_attached);
        assert_eq!(panel.tabs[1].page.session.url(), "https://b.test/");
        assert!(!panel.tabs[1].engine_attached);

        assert!(panel.activate_tab(crate::browser::engine::TabId::BOOT));
        assert_eq!(panel.active_tab, crate::browser::engine::TabId::BOOT);
        assert_eq!(panel.session.url(), "https://a.test/");
        assert_eq!(panel.tabs[1].id, second);
        assert_eq!(panel.tabs[1].page.session.url(), "https://b.test/");
        assert!(!panel.activate_tab(crate::browser::engine::TabId(999)));
        assert!(!panel.remove_tab(crate::browser::engine::TabId(999)));

        // Engine attachment follows the tab identity, not the old live slot.
        panel.tab_mut(second).unwrap().engine_attached = true;
        assert!(panel.activate_tab(second));
        assert!(panel.activate_tab(crate::browser::engine::TabId::BOOT));
        assert!(panel.tab_loaded(second));
        assert!(panel.remove_tab(second));
        assert!(!panel.remove_tab(crate::browser::engine::TabId::BOOT));
    }

    #[test]
    fn restored_strip_keeps_active_attached_and_others_detached() {
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
        assert_eq!(panel.tabs.len(), 3);
        assert!(panel.active().engine_attached);
        assert!(
            panel
                .tabs
                .iter()
                .filter(|tab| tab.id != crate::browser::engine::TabId::BOOT)
                .all(|tab| !tab.engine_attached)
        );
        let urls: Vec<&str> = panel
            .tabs
            .iter()
            .map(|tab| tab.page.session.url())
            .collect();
        assert_eq!(
            urls,
            vec!["https://a.test/", "https://b.test/", "https://c.test/"]
        );
    }

    #[test]
    fn create_tab_preserves_the_previous_page() {
        let mut panel = BrowserPanel::default();
        panel.session.begin_load("https://a.test/".to_string());
        let (id, generation) = panel.create_tab("https://b.test/".to_string());
        assert_ne!(id, crate::browser::engine::TabId::BOOT);
        assert_eq!(panel.active_tab, id);
        assert_eq!(panel.session.url(), "https://b.test/");
        assert_eq!(panel.session.generation(), generation);
        assert_eq!(
            panel.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            vec![crate::browser::engine::TabId::BOOT, id]
        );
        assert_eq!(panel.tabs[0].page.session.url(), "https://a.test/");
        assert!(!panel.tab_loaded(id));
        assert!(panel.tab_loaded(crate::browser::engine::TabId::BOOT));
    }

    #[test]
    fn closing_a_dormant_tab_does_not_detach_its_neighbor() {
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
            ],
            active: 0,
        };
        let mut panel = BrowserPanel::default();
        assert!(panel.restore_saved_strip(&saved));
        let second = panel.tabs[1].id;
        assert!(!panel.tab_loaded(second));

        // First activation opens the engine view and marks it attached.
        assert!(panel.activate_tab(second));
        panel.tab_mut(second).unwrap().engine_attached = true;

        // Switch back, then close the tab that was dormant. The neighbor's
        // attachment must survive the switch; it must not be reopened.
        assert!(panel.activate_tab(crate::browser::engine::TabId::BOOT));
        assert!(panel.tab_loaded(crate::browser::engine::TabId::BOOT));
        assert!(panel.remove_tab(second));
        assert_eq!(panel.active_tab, crate::browser::engine::TabId::BOOT);
        assert!(panel.tab_loaded(crate::browser::engine::TabId::BOOT));
    }

    #[test]
    fn nav_block_is_per_tab_and_silent_blocks_never_paint() {
        let mut panel = panel();
        let block = BlockedNavigation {
            source: "https://a.test/".into(),
            target: "https://b.test/".into(),
            reason: "cross-site navigation needs confirmation".into(),
            can_continue: true,
            token: Some(7),
            stay_focused: true,
            remember: false,
        };
        panel.note_navigation_blocked(crate::browser::engine::TabId(2), block.clone());
        assert!(panel.nav_block.is_none());
        assert!(
            panel
                .nav_blocks
                .contains_key(&crate::browser::engine::TabId(2))
        );

        let mut silent = block;
        silent.can_continue = false;
        panel.note_navigation_blocked(crate::browser::engine::TabId::BOOT, silent);
        assert!(panel.nav_block.is_none());
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
