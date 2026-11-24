//! Browser app state: tabs, per-tab page state, chrome targets, cursor.
//!
//! Extracted from the Launcher view so the browser is a top-level managed app
//! (see `notes/browser-Home-pivot.md`, phase 0). UI-only: the render code
//! stays in `ui::launcher` until the standalone Browser window lands.

use crate::ui::text_fit;
use std::collections::HashMap;
use std::time::Instant;

/// Last canonical history snapshot reported by the engine, kept alongside the
/// UI's projection so traces can expose disagreement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineHistorySnapshot {
    pub current: usize,
    pub entries: Vec<crate::browser::engine::EngineHistoryEntry>,
}

/// Hold-bumper history selector (B2.4): directional dropdown over the
/// session stack. `back` selects L1/back targets, else R1/forward targets;
/// `highlight` indexes the displayed directional list (0 = closest =
/// quick-press target = default). Rows map to absolute session indices via
/// [`crate::browser::BrowserSession::back_entries`] /
/// [`crate::browser::BrowserSession::forward_entries`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistorySelector {
    pub back: bool,
    pub highlight: usize,
}

impl HistorySelector {
    /// Absolute session index under the highlight, if the list is non-empty.
    pub fn picked(&self, session: &crate::browser::BrowserSession) -> Option<usize> {
        let list = if self.back {
            session.back_entries()
        } else {
            session.forward_entries()
        };
        list.get(self.highlight.min(list.len().saturating_sub(1)))
            .or_else(|| list.first())
            .map(|(i, _)| *i)
    }

    /// Clamp the highlight after the stack moved underneath the selector.
    pub fn clamp(&mut self, len: usize) {
        if len == 0 {
            self.highlight = 0;
        } else {
            self.highlight = self.highlight.min(len - 1);
        }
    }
}

/// Per-tab page state: everything that belongs to one tab's document.
/// `BrowserPanel::page` is always the ACTIVE tab's; background tabs stash
/// theirs in `BrowserPanel::stored`. The panel derefs to this, so all
/// existing live-tab call sites (`panel.session`, `panel.cursor`, …) keep
/// working.
/// Dominant right-stick axis for one scroll gesture. Chosen when the stick
/// leaves neutral and held until it returns, so a diagonal hold cannot drift
/// sideways while the user scrolls vertically (or vice versa).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAxis {
    Vertical,
    Horizontal,
}

impl ScrollAxis {
    fn other(self) -> Self {
        match self {
            Self::Vertical => Self::Horizontal,
            Self::Horizontal => Self::Vertical,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BrowserTabPage {
    pub session: crate::browser::BrowserSession,
    /// Virtual cursor (engine mode only; pad-owned).
    pub cursor: crate::browser::cursor::Cursor,
    /// Focusable elements from the latest accessibility tree.
    pub focusables: Vec<crate::browser::cursor::Focusable>,
    pub scroll_y: f32,
    /// Viewport scroll offset the backend translated the tree with
    /// (`EngineEvent::ScrollOffset`). Debug overlay + shot correlation.
    pub scroll_offset: (f32, f32),
    /// Document extent in page pixels (grow-only; `EngineEvent::ScrollExtent`).
    /// Drives the page scrollbar. `(0,0)` until the first snapshot.
    pub page_extent: (f32, f32),
    /// Last analog integrate step (None until the first pad sample).
    pub analog_at: Option<Instant>,
    /// Active right-stick scroll axis lock (None = stick at rest, unlocked).
    pub scroll_axis: Option<ScrollAxis>,
    /// Page-driven focus target (autofocus / JS `.focus()`): UI id to dock
    /// when it next resolves in the live set. One shot per tree — a culled
    /// target never docks ("if possible").
    pub pending_focus: Option<u64>,
    /// Last left-stick vector seen by the cursor tick. Autofocus docking
    /// requires the stick at rest (an active steer means the user is driving
    /// and wins over the page).
    pub last_pad_stick: (f32, f32),
    /// Last sent mouse position (quantized px), so the pad cursor streams
    /// moves to the engine only when it actually travels. Never persisted.
    pub last_sent_mouse: Option<(i32, i32)>,
    /// Last click point + time, for the R3 issue-flag correlation.
    pub last_click: Option<((f32, f32), Instant)>,
    /// Transient action hint shown in the card header ("Opening link…"):
    /// instant feedback that a click landed, so a slow-or-dead page reads
    /// as such instead of a dropped input. Superseded by Loading…/Error,
    /// expires after a few seconds.
    pub action_hint: Option<(String, Instant)>,
    /// Last canonical engine history; tracing compares it with the UI stack.
    pub last_engine_history: Option<EngineHistorySnapshot>,
    /// Last CEF back/forward capability flag pair.
    pub last_engine_can: Option<(bool, bool)>,
    /// Hold-bumper history selector overlay (B2.4). `None` = closed.
    pub history_selector: Option<HistorySelector>,
    /// Engine phase for the status strip (navigate → load → DOM boot).
    pub engine_activity: crate::browser::engine::EngineActivity,
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

/// Browser app panel. The URL draft and body scroll offset live
/// here; load lifecycle (generation-guarded suspend/resume) lives in
/// [`crate::browser::BrowserSession`]. The pad cursor roams in engine page
/// pixels (see [`crate::browser::cursor`]).
#[derive(Debug, Clone)]
pub struct BrowserPanel {
    /// Live page state: ALWAYS the active tab's. Swapped with `stored`
    /// entries on switch (see `activate_stored`).
    pub page: BrowserTabPage,
    /// Background tabs (page state only; chrome stays shared).
    pub stored: Vec<StoredTab>,
    /// Creation-order tab ids (active included): the strip renders in this
    /// order so switching never shuffles positions.
    pub order: Vec<crate::browser::engine::TabId>,
    /// Active tab id. Defaults to the boot webview — matches the worker's
    /// `TabId::BOOT` with no handshake.
    pub active_tab: crate::browser::engine::TabId,
    /// Next UI-owned tab id (counts up from boot; engine popups count down
    /// from `u64::MAX` — disjoint by construction).
    pub next_tab_id: u64,
    pub draft: String,
    /// URL-edit mode: keyboard goes to the address field (pad Triangle /
    /// Ctrl+L enters, Enter commits, Esc/Circle backs out). Cleared on
    /// navigate and close.
    pub url_editing: bool,
    /// Last page-local hardware/replay pointer position. Clicks and wheel
    /// events use this point; replay sets it explicitly so automation never
    /// depends on the OS cursor position.
    pub page_mouse: (f32, f32),
    /// Last debug frame-shot time (5s cadence while the overlay is on).
    pub last_shot: Option<Instant>,
    /// Last dock transition (None→Some) wall-clock, for settled dock shots.
    /// Dock-triggered shots wait ~300ms so the morph completes first —
    /// shooting on the dock tick only ever captures a half-morphed wrap
    /// and reads as a misplaced outline.
    pub last_dock_change: Option<Instant>,
    /// JS element-click mode for Button-kind dock targets (toggled from the
    /// browser card itself; off by default): `__sdsc_click` on the exact
    /// element instead of the trusted synthetic pair. Lets A/B testing
    /// untrusted-activation against trusted input per page.
    pub js_click: bool,
    /// R3 rising-edge latch for the issue flag (R3 carries no NavAction).
    pub r3_latched: bool,
    /// Synthetic header snap targets: the tab strip, toolbar and bookmarks
    /// as cursor-space rects above the viewport (negative y), so the pad
    /// cursor treats chrome and page as one continuous surface. Rebuilt
    /// whenever the strip or bookmark count changes; ids are stable while
    /// the set is unchanged so dock attachments survive pulls.
    pub chrome: Vec<(ChromeTarget, crate::browser::cursor::Focusable)>,
    /// Live page-area size in card pixels (the engine viewport resizes to
    /// this; the cursor space matches it 1:1).
    pub viewport: (f32, f32),
    /// A D-pad jump asked for a fresh DOM target cull around the cursor.
    /// Cleared once the jump flight lands and the engine is told to pull.
    pub dom_reseed_pending: bool,
    /// Next synthetic id (counts up from [`CHROME_ID_BASE`], far above any
    /// engine-assigned page id).
    pub chrome_seq: u64,
    /// Browser debug overlay + shots (Diagnostics toggle / F10, mirrored
    /// from App). Never persisted.
    pub debug_overlay: bool,
    /// Chrome-style bookmarks (disk-persisted). The star toggles the current
    /// page; the bar offers one-click open.
    pub bookmarks: Vec<crate::browser::bookmarks::Bookmark>,
    /// Star bubble open: rename the current page's bookmark or remove it.
    pub bookmark_bubble: bool,
    /// Bubble rename draft (applied on Done, discarded on dismiss).
    pub bookmark_name_draft: String,
    /// Favicon handles by full page URL (engine `FaviconChanged`), shared by
    /// the bookmark bar and history rows. Capped; oldest evicted first.
    pub favicons: HashMap<String, iced::widget::image::Handle>,
    pub favicon_order: Vec<String>,
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
        let mut panel = Self {
            page: BrowserTabPage::default(),
            stored: Vec::new(),
            order: vec![crate::browser::engine::TabId::BOOT],
            active_tab: crate::browser::engine::TabId::BOOT,
            next_tab_id: 2,
            draft: String::new(),
            url_editing: false,
            page_mouse: (
                crate::browser::cursor::VIEWPORT_W * 0.5,
                crate::browser::cursor::VIEWPORT_H * 0.5,
            ),
            last_shot: None,
            last_dock_change: None,
            js_click: false,
            r3_latched: false,
            chrome: Vec::new(),
            viewport: (
                crate::browser::cursor::VIEWPORT_W,
                crate::browser::cursor::VIEWPORT_H,
            ),
            dom_reseed_pending: false,
            chrome_seq: crate::browser::cursor::CHROME_ID_BASE,
            debug_overlay: false,
            bookmarks: Vec::new(),
            bookmark_bubble: false,
            bookmark_name_draft: String::new(),
            favicons: HashMap::new(),
            favicon_order: Vec::new(),
        };
        panel.rebuild_chrome();
        panel
    }
}

impl BrowserPanel {
    /// CEF's own back/forward capability wins over the projected stack:
    /// redirects and same-document entries can move CEF without the UI's
    /// local snapshot settling first.
    pub fn can_go_back(&self) -> bool {
        self.page
            .last_engine_can
            .map(|(back, _)| back)
            .unwrap_or_else(|| self.page.session.can_go_back())
    }

    pub fn can_go_forward(&self) -> bool {
        self.page
            .last_engine_can
            .map(|(_, forward)| forward)
            .unwrap_or_else(|| self.page.session.can_go_forward())
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
        self.rebuild_chrome();
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

    /// Snapshot the strip for `tabs.json`: creation-order (URL, title),
    /// active position. Blank/empty URLs never persist (placeholders, not
    /// visits — same rule as the history stacks).
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

    /// Overwrite `tabs.json` with the current strip.
    pub fn save_tabs(&self) {
        crate::browser::tabs::save(&self.snapshot_tabs());
    }

    /// Rebuild the synthetic chrome map: stable ids while the target set
    /// is unchanged (dock attachments survive pulls), fresh ids for new
    /// targets, dropped ids for gone ones (the cursor detaches via the
    /// normal retarget path). Call after strip/bookmark changes.
    pub fn rebuild_chrome(&mut self) {
        use crate::browser::cursor::{FocusKind, Focusable};
        // The header and cursor share the live page size; keep the cursor
        // bounds in step even if only the chrome map is being refreshed.
        self.sync_live_cursor();
        // Natural-width layout is the single source of truth: the rendered
        // header and the pad cursor both use these rects, so the dot can be
        // drawn truthfully in header space.
        let layout = chrome_layout(self);
        let mut fresh = Vec::new();
        for row in layout {
            for (target, x, w) in row.items {
                let id = match self.chrome.iter().find(|(t, _)| *t == target) {
                    Some((_, f)) => f.id,
                    None => {
                        let id = self.chrome_seq;
                        self.chrome_seq = self.chrome_seq.wrapping_add(1);
                        id
                    }
                };
                // Chrome slots carry no link target: hover status stays on
                // the engine phase, and activation never touches the page.
                fresh.push((
                    target,
                    Focusable {
                        id,
                        x,
                        y: row.y,
                        w,
                        h: row.h,
                        kind: FocusKind::Button,
                        url: None,
                    },
                ));
            }
        }
        self.chrome = fresh;
    }

    /// Chrome target for a cursor attachment id, if synthetic.
    pub fn chrome_target(&self, id: u64) -> Option<ChromeTarget> {
        self.chrome
            .iter()
            .find(|(_, f)| f.id == id)
            .map(|(t, _)| *t)
    }

    /// Currently docked chrome target, if the cursor sits on synthetic
    /// chrome rather than a page element.
    pub fn chrome_attached(&self) -> Option<ChromeTarget> {
        self.cursor.attached.and_then(|id| self.chrome_target(id))
    }

    /// Header target under a precise cursor point (page/chrome coordinates).
    /// Docked clicks use the attachment id; a free precise click in the
    /// header must still hit the tab/toolbar/bookmark rect it points at.
    pub fn chrome_target_at(&self, x: f32, y: f32) -> Option<ChromeTarget> {
        self.chrome
            .iter()
            .find(|(_, f)| x >= f.x && x <= f.x + f.w && y >= f.y && y <= f.y + f.h)
            .map(|(target, _)| *target)
    }

    /// Title for a tab id (live or stored): page title, else URL, else the default page.
    pub fn tab_title(&self, id: crate::browser::engine::TabId) -> String {
        let page = if id == self.active_tab {
            Some(&self.page)
        } else {
            self.stored.iter().find(|t| t.id == id).map(|t| &t.page)
        };
        match page {
            Some(p) => {
                let url = p.session.url();
                p.session
                    .title()
                    .map(str::to_string)
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| {
                        if url.is_empty() {
                            crate::browser::BROWSER_DEFAULT_URL.to_string()
                        } else {
                            url.to_string()
                        }
                    })
            }
            None => String::new(),
        }
    }

    /// URL for a tab id (live or stored), empty when the tab vanished.
    pub fn tab_url(&self, id: crate::browser::engine::TabId) -> String {
        let page = if id == self.active_tab {
            Some(&self.page)
        } else {
            self.stored.iter().find(|t| t.id == id).map(|t| &t.page)
        };
        page.map(|p| p.session.url().to_string())
            .unwrap_or_default()
    }

    pub fn toggle(&mut self) {
        if self.session.is_open() {
            self.session.close_surface();
        } else {
            self.session.open_surface();
        }
        self.url_editing = false;
    }

    pub fn close_surface(&mut self) {
        self.session.close_surface();
        self.url_editing = false;
        self.history_selector = None;
        self.pending_focus = None;
        // Drop the analog clock so reopening never applies a huge first dt.
        self.analog_at = None;
        self.scroll_axis = None;
    }

    pub fn note_scrolled(&mut self, y: f32) {
        self.scroll_y = y.max(0.0);
    }

    /// Cap on cached favicon handles: oldest evicted first.
    pub const MAX_FAVICONS: usize = 64;

    /// True when `url` is bookmarked (drives the star state).
    pub fn is_bookmarked(&self, url: &str) -> bool {
        !url.is_empty() && self.bookmarks.iter().any(|b| b.url == url)
    }

    /// Cache a favicon handle, keyed by site origin (see `origin_key`):
    /// icons are per-site, and arrivals race navigations, so full-URL keys
    /// miss their rows. Evicts the oldest past the cap.
    pub fn note_favicon(&mut self, url: String, handle: iced::widget::image::Handle) {
        if url.is_empty() {
            return;
        }
        let key = crate::browser::bookmarks::origin_key(&url);
        if !self.favicons.contains_key(&key) {
            self.favicon_order.push(key.clone());
            while self.favicon_order.len() > Self::MAX_FAVICONS {
                let oldest = self.favicon_order.remove(0);
                self.favicons.remove(&oldest);
            }
        }
        self.favicons.insert(key, handle);
        // Tab strips size themselves from the icon's presence/absence.
        self.rebuild_chrome();
    }

    /// Step the body scroll (reader text); returns the new absolute offset.
    pub fn scroll_by(&mut self, delta: f32) -> f32 {
        self.scroll_y = (self.scroll_y + delta).max(0.0);
        self.scroll_y
    }

    /// Integrate one analog pad sample: left stick moves the cursor
    /// (magnetic snap + glide when enabled), right stick scrolls. Runs every
    /// tick, including neutral ones, so friction and glide converge even
    /// with no deflection — a released stick must decay, not freeze.
    /// Returns the scroll delta in page pixels for this step; `dt` is
    /// clamped so background gaps cannot teleport either axis.
    pub fn advance_stick(
        &mut self,
        lx: f32,
        ly: f32,
        rx: f32,
        ry: f32,
        r2: bool,
        now: Instant,
    ) -> (f32, f32) {
        use crate::browser::cursor::SCROLL_SPEED;
        let dt = self
            .analog_at
            .map(|t| now.saturating_duration_since(t).as_secs_f32().min(0.1))
            .unwrap_or(0.0);
        self.analog_at = Some(now);
        self.page.cursor.turbo = r2;
        // One continuous surface: page targets plus synthetic chrome.
        let targets = cursor_targets(&self.page.focusables, &self.chrome);
        // Docking transitions are rare and are the cursor's most surprising
        // behavior (magnet centre snap, break-free kick); keep the trace in
        // release too so user-marked nudges are diagnosable.
        let before = self.page.cursor.attached;
        let (bx, by) = (self.page.cursor.x, self.page.cursor.y);
        // The scroll stick only means anything over the page surface. In
        // the header (negative y) it must not scroll the page, and it must
        // not release the dock: each deflection/neutral pair used to
        // undock/re-dock the chrome target under the point (the nudge
        // flicker) while also moving the page scrollbar.
        let in_page = self.page.cursor.x >= 0.0
            && self.page.cursor.x < self.viewport.0
            && self.page.cursor.y >= 0.0
            && self.page.cursor.y < self.viewport.1;
        let scrolling = in_page && (rx != 0.0 || ry != 0.0);
        let cursor = &mut self.page.cursor;
        cursor.advance_with(lx, ly, dt, &targets, scrolling);
        {
            let after = self.page.cursor.attached;
            if before != after {
                let describe = |id: Option<u64>| match id {
                    None => "free".to_string(),
                    Some(i) => match targets.iter().find(|o| o.id == i) {
                        Some(o) => format!(
                            "#{i} {:?} ({:.0},{:.0},{:.0}x{:.0}){}",
                            o.kind,
                            o.x,
                            o.y,
                            o.w,
                            o.h,
                            if i >= crate::browser::cursor::CHROME_ID_BASE {
                                " chrome"
                            } else {
                                ""
                            }
                        ),
                        None => format!("#{i} gone"),
                    },
                };
                crate::platform::app_log::info(format!(
                    "cursor dock {} -> {} stick=({lx:.2},{ly:.2}) rstick=({rx:.2},{ry:.2}) scrolling={} pos=({bx:.0},{by:.0})",
                    describe(before),
                    describe(after),
                    u8::from(scrolling),
                ));
            }
        }
        if in_page {
            // One axis per gesture: a slightly diagonal hold must not drift
            // sideways (the scroll-trap). The lock is chosen on the first
            // deflected sample and cleared only at neutral; if the locked
            // axis is released while the other stays deflected, adopt the
            // other so crossing over without a neutral pass never dead-sticks.
            let axis = match self.scroll_axis {
                Some(_) if rx == 0.0 && ry == 0.0 => {
                    self.scroll_axis = None;
                    None
                }
                Some(axis) => {
                    let (locked, other) = match axis {
                        ScrollAxis::Vertical => (ry, rx),
                        ScrollAxis::Horizontal => (rx, ry),
                    };
                    if locked == 0.0 && other != 0.0 {
                        let next = axis.other();
                        self.scroll_axis = Some(next);
                        Some(next)
                    } else {
                        Some(axis)
                    }
                }
                None if rx == 0.0 && ry == 0.0 => None,
                None => {
                    // Tie goes vertical: down-page is the dominant gesture.
                    let axis = if ry.abs() >= rx.abs() {
                        ScrollAxis::Vertical
                    } else {
                        ScrollAxis::Horizontal
                    };
                    self.scroll_axis = Some(axis);
                    Some(axis)
                }
            };
            let scale = if r2 { 2.0 } else { 1.0 };
            match axis {
                Some(ScrollAxis::Vertical) => (0.0, ry * SCROLL_SPEED * dt * scale),
                Some(ScrollAxis::Horizontal) => (rx * SCROLL_SPEED * dt * scale, 0.0),
                None => (0.0, 0.0),
            }
        } else {
            // The scroll stick only applies over the page surface; visiting
            // the header resets the gesture so re-entry starts clean.
            self.scroll_axis = None;
            (0.0, 0.0)
        }
    }

    /// D-pad focus step. Chrome travel is row-stepped: Left/Right stay in
    /// the row (stop at the ends), Up/Down move exactly one row with the
    /// column preserved — never skipping rows, never flying into the void.
    /// Page travel keeps the spatial cone jump.
    pub fn jump_cursor(&mut self, dir: crate::browser::cursor::JumpDir) {
        use crate::browser::cursor::JumpDir;
        // A D-pad press always asks the engine for a fresh target set around
        // the new cursor: the live DOM set is culled around the previous
        // pull center, and a jump can land where no target was collected
        // yet (the "skipped target" that works on the second pass).
        self.dom_reseed_pending = true;
        let from = (self.page.cursor.x, self.page.cursor.y);
        let was = self.page.cursor.attached;
        self.rebuild_chrome();
        // Row layout with live rects (stable indices: 0 strip, 1 toolbar,
        // 2 bookmarks; empties kept so indices never shift).
        let layout: Vec<Vec<(ChromeTarget, crate::browser::cursor::Focusable)>> = chrome_rows(self)
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .filter_map(|t| {
                        self.chrome
                            .iter()
                            .find(|(tt, _)| *tt == t)
                            .map(|(t, f)| (*t, f.clone()))
                    })
                    .collect()
            })
            .collect();
        let attached = self.page.cursor.attached;
        let in_chrome = match attached {
            Some(id) => layout
                .iter()
                .flat_map(|r| r.iter())
                .any(|(_, f)| f.id == id),
            None => self.page.cursor.y < 0.0,
        };
        // Current row: the attached slot's row, else the y-band row.
        let cur_row: Option<usize> = if in_chrome {
            attached
                .and_then(|id| {
                    layout
                        .iter()
                        .position(|r| r.iter().any(|(_, f)| f.id == id))
                })
                .or_else(|| {
                    // Row boundaries: strip ends where toolbar begins,
                    // toolbar ends where the bookmark row begins.
                    let y = self.page.cursor.y;
                    let strip_bottom = CHROME_STRIP_Y + CHROME_ROW_H;
                    let toolbar_bottom = CHROME_TOOLBAR_Y + CHROME_ROW_H;
                    if y < strip_bottom {
                        Some(0)
                    } else if y < toolbar_bottom {
                        Some(1)
                    } else {
                        Some(2)
                    }
                })
        } else {
            None
        };
        let row_items = |r: usize| -> Vec<crate::browser::cursor::Focusable> {
            layout
                .get(r)
                .map(|row| row.iter().map(|(_, f)| f.clone()).collect())
                .unwrap_or_default()
        };
        // Column-preserving pick: the slot containing the cursor's x, else
        // the one whose edge is nearest. Deliberately not cone-filtered —
        // row stepping always moves exactly one row, however far sideways
        // the nearest slot sits.
        let col_pick = |items: &[crate::browser::cursor::Focusable],
                        x: f32|
         -> Option<crate::browser::cursor::Focusable> {
            let mut best: Option<(&crate::browser::cursor::Focusable, f32)> = None;
            for f in items {
                let d = if x < f.x {
                    f.x - x
                } else if x > f.x + f.w {
                    x - (f.x + f.w)
                } else {
                    return Some(f.clone());
                };
                if best.is_none_or(|(_, bd)| d < bd) {
                    best = Some((f, d));
                }
            }
            best.map(|(f, _)| f.clone())
        };
        let x = self.page.cursor.x;
        'jump: {
            match (cur_row, dir) {
                // Header horizontals: same row only; nothing in that direction
                // means a no-op (stop at the row's ends).
                (Some(r), JumpDir::Left | JumpDir::Right) => {
                    let items = row_items(r);
                    self.page.cursor.jump_row(dir, &items);
                }
                // Header up: nearest non-empty row above, column preserved;
                // top row stays put.
                (Some(r), JumpDir::Up) => {
                    for prev in (0..r).rev() {
                        if let Some(f) = col_pick(&row_items(prev), x) {
                            self.page.cursor.jump_to(&f);
                            break 'jump;
                        }
                    }
                }
                // Header down: nearest non-empty row below, column preserved;
                // the bottom row drops spatially into the page.
                (Some(r), JumpDir::Down) => {
                    for next in r + 1..layout.len() {
                        if let Some(f) = col_pick(&row_items(next), x) {
                            self.page.cursor.jump_to(&f);
                            break 'jump;
                        }
                    }
                    let targets = cursor_targets(&self.page.focusables, &self.chrome);
                    self.page.cursor.jump(dir, &targets);
                }
                // Page horizontals never see chrome (header entry/exit is
                // Up/Down or analog) and never cone into another row: the
                // row lock stops at the row's ends.
                (None, JumpDir::Left | JumpDir::Right) => {
                    let targets = self.page.focusables.clone();
                    self.page.cursor.jump_row(dir, &targets);
                }
                // Page verticals: spatial cone. A page target above wins on
                // distance; only the topmost page row reaches chrome, and
                // the nearest chrome row wins (toolbar before strip,
                // bookmarks before toolbar when present).
                (None, _) => {
                    let targets = cursor_targets(&self.page.focusables, &self.chrome);
                    self.page.cursor.jump(dir, &targets);
                }
            }
        }
        crate::platform::app_log::info(format!(
            "cursor jump {dir:?} zone={} from=({:.0},{:.0}) dock={was:?} fly={:?} attached={:?} targets={}",
            if from.1 < 0.0 { "header" } else { "page" },
            from.0,
            from.1,
            self.page.cursor.fly_to,
            self.page.cursor.attached,
            self.page.focusables.len() + self.chrome.len(),
        ));
    }
}

/// Pad-navigable chrome target: every header control the pad cursor docks
/// onto as if it were in-viewport page chrome (same magnet, same
/// break-free physics — the header rows live just above y=0 in unified
/// cursor space).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeTarget {
    Tab(crate::browser::engine::TabId),
    TabClose(crate::browser::engine::TabId),
    NewTab,
    /// Hidden-tab count; activation cycles to the next tab.
    TabOverflow(usize),
    Back,
    Forward,
    Reload,
    DefaultPage,
    Omnibox,
    Star,
    Go,
    Bookmark(usize),
}

/// Natural-width header layout in unified page pixels. The rendered
/// header and the pad cursor share these rects, so the cursor dot can be
/// drawn over header controls at its true position. Widths mirror the
/// widgets' font sizes and paddings; the omnibox takes the remainder.
pub struct ChromeRowLayout {
    pub y: f32,
    pub h: f32,
    /// `(target, x, width)` in unified space, left to right.
    pub items: Vec<(ChromeTarget, f32, f32)>,
}

fn label_width(label: &str, size: f32, pad_x: f32) -> f32 {
    text_fit::text_width_px(label, size) + pad_x * 2.0
}

pub fn chrome_layout(panel: &BrowserPanel) -> Vec<ChromeRowLayout> {
    let viewport_w = panel.viewport.0;
    let mut rows = Vec::with_capacity(3);

    // Tab strip: favicon + ellipsized title + close per tab, fitted to the
    // width. Titles shrink evenly first; when even minimum titles cannot
    // fit, a window around the active tab is shown plus an overflow chip
    // that cycles to the next tab (so no tab is unreachable).
    const TAB_PAD: f32 = 16.0; // button padding [4, 8]
    const CLOSE_PAD: f32 = 12.0; // close padding [4, 6]
    const ICON_W: f32 = 22.0; // 16px favicon + 6px spacing
    const TAB_GAP: f32 = 4.0;
    // Minimum title budget per tab (~120px tab + close). Below this the
    // strip switches to the overflow chip instead of collapsing titles to a
    // few characters.
    const TITLE_MIN: f32 = 72.0;
    // Give a lone/few tabs real room. The old fixed 140px cap clipped normal
    // page titles after a handful of characters on wide screens.
    let title_max = (viewport_w * 0.55).clamp(200.0, 640.0);
    let close_w = text_fit::text_width_px("×", 11.0) + CLOSE_PAD;
    let new_w = text_fit::text_width_px("+", 14.0) + 20.0;
    let mut plans = Vec::with_capacity(panel.order.len());
    for id in &panel.order {
        let url = panel.tab_url(*id);
        let icon_w = if !url.is_empty()
            && panel
                .favicons
                .contains_key(&crate::browser::bookmarks::origin_key(&url))
        {
            ICON_W
        } else {
            0.0
        };
        let title = text_fit::ellipsize_to_width(panel.tab_title(*id), 11.0, title_max);
        plans.push((*id, text_fit::text_width_px(&title, 11.0), icon_w));
    }
    let count = plans.len();
    let gaps_all = TAB_GAP * count as f32;
    let fixed_all: f32 = plans
        .iter()
        .map(|(_, _, icon)| TAB_PAD + icon + close_w)
        .sum();
    let titles_all: f32 = plans.iter().map(|(_, title, _)| title).sum();
    let (start, end, title_budget, hidden) = if count == 0 {
        (0, 0, 0.0, 0)
    } else if fixed_all + titles_all + gaps_all + new_w <= viewport_w {
        (0, count, f32::INFINITY, 0)
    } else {
        let per = (viewport_w - fixed_all - gaps_all - new_w) / count as f32;
        if per >= TITLE_MIN {
            (0, count, per, 0)
        } else {
            // Reserve the widest chip and the new-tab button, then fit as
            // many minimum-title tabs as possible.
            let chip_max = label_width("+99", 11.0, 10.0);
            let group_min = TAB_PAD + ICON_W + close_w + TITLE_MIN;
            let room = viewport_w - new_w - chip_max - TAB_GAP;
            let fit = (room / (group_min + TAB_GAP)).floor().max(1.0) as usize;
            let fit = fit.min(count.saturating_sub(1)).max(1);
            let active = panel
                .order
                .iter()
                .position(|id| *id == panel.active_tab)
                .unwrap_or(0);
            let start = if active < fit {
                0
            } else {
                (active + 1 - fit).min(count - fit)
            };
            (start, start + fit, TITLE_MIN, count - fit)
        }
    };
    let mut strip = Vec::with_capacity((end - start) * 2 + 2);
    let mut x = 0.0;
    for (id, title_w, icon_w) in plans.iter().take(end).skip(start) {
        let title_w = title_w.min(title_budget);
        // A few px of slack so the render-side ellipsis safety margin does not
        // shave the last character off a title that already fits naturally.
        let tab_w = TAB_PAD + icon_w + title_w + 6.0;
        strip.push((ChromeTarget::Tab(*id), x, tab_w));
        strip.push((ChromeTarget::TabClose(*id), x + tab_w, close_w));
        x += tab_w + close_w + TAB_GAP;
    }
    if hidden > 0 {
        let label = format!("+{hidden}");
        let chip_w = label_width(&label, 11.0, 10.0);
        strip.push((ChromeTarget::TabOverflow(hidden), x, chip_w));
        x += chip_w + TAB_GAP;
    }
    if x + new_w <= viewport_w {
        strip.push((ChromeTarget::NewTab, x, new_w));
    }
    rows.push(ChromeRowLayout {
        y: CHROME_STRIP_Y,
        h: CHROME_ROW_H,
        items: strip,
    });

    // Toolbar: fixed controls, the address field absorbs the remainder.
    let gaps = 6.0 * 8.0;
    let back = label_width("←", 14.0, 12.0);
    let forward = label_width("→", 14.0, 12.0);
    let reload = label_width("⟳", 14.0, 12.0);
    let default_page = label_width("⌂", 14.0, 12.0);
    let star = label_width("★", 16.0, 10.0).max(label_width("☆", 16.0, 10.0));
    let go = label_width("Go", 14.0, 14.0);
    let omni =
        (viewport_w - (back + forward + reload + default_page + star + go + gaps)).max(120.0);
    let mut bar = Vec::with_capacity(7);
    let mut x = 0.0;
    for (target, w) in [
        (ChromeTarget::Back, back),
        (ChromeTarget::Forward, forward),
        (ChromeTarget::Reload, reload),
        (ChromeTarget::DefaultPage, default_page),
        (ChromeTarget::Omnibox, omni),
        (ChromeTarget::Star, star),
        (ChromeTarget::Go, go),
    ] {
        bar.push((target, x, w));
        x += w + 8.0;
    }
    rows.push(ChromeRowLayout {
        y: CHROME_TOOLBAR_Y,
        h: CHROME_ROW_H,
        items: bar,
    });

    // One bookmark row, natural chip widths, clipped at the viewport edge.
    let mut marks = Vec::new();
    let mut x = 0.0;
    for (i, b) in panel
        .bookmarks
        .iter()
        .take(CHROME_MAX_BOOKMARKS)
        .enumerate()
    {
        let icon = panel
            .favicons
            .contains_key(&crate::browser::bookmarks::origin_key(&b.url));
        let name = text_fit::ellipsize_to_width(b.name(), 13.0, 170.0);
        let w = 20.0 + text_fit::text_width_px(&name, 13.0) + if icon { 22.0 } else { 0.0 };
        if x + w > viewport_w {
            break;
        }
        marks.push((ChromeTarget::Bookmark(i), x, w));
        x += w + 6.0;
    }
    rows.push(ChromeRowLayout {
        y: CHROME_BOOKMARKS_Y,
        h: CHROME_BOOKMARK_H,
        items: marks,
    });

    rows
}

/// Header rows for pad navigation, targets only (rects live in
/// [`BrowserPanel::chrome`], built by [`BrowserPanel::rebuild_chrome`]).
pub fn chrome_rows(panel: &BrowserPanel) -> Vec<Vec<ChromeTarget>> {
    chrome_layout(panel)
        .into_iter()
        .map(|row| row.items.into_iter().map(|(t, _, _)| t).collect())
        .collect()
}

/// Unified-space geometry (page px): header rows above the viewport.
pub const CHROME_STRIP_Y: f32 = -112.0;
pub const CHROME_TOOLBAR_Y: f32 = -72.0;
pub const CHROME_BOOKMARKS_Y: f32 = -32.0;
pub const CHROME_ROW_H: f32 = 36.0;
pub const CHROME_BOOKMARK_H: f32 = 28.0;
/// Gap between header rows.
pub const CHROME_ROW_GAP: f32 = 4.0;
/// Total header height above the page: three rows plus three gaps. Matches
/// `CURSOR_MIN_Y` exactly.
pub const CHROME_HEADER_H: f32 = CHROME_ROW_H
    + CHROME_ROW_GAP
    + CHROME_ROW_H
    + CHROME_ROW_GAP
    + CHROME_BOOKMARK_H
    + CHROME_ROW_GAP;
/// Outer breather against the screen edge: 50px per side (height - 100).
pub const BROWSER_OUTER_MARGIN: f32 = 50.0;
/// Card padding around the browser surface.
pub const BROWSER_CARD_PAD: f32 = 4.0;

/// Browser popup geometry: the card itself is 16:9, sized from the
/// available viewport height (clamped by width). The fixed header takes
/// the top of the card; the page area is the rest.
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
    let page_h = (card_h - 2.0 * BROWSER_CARD_PAD - CHROME_HEADER_H).max(1.0);
    BrowserLayout {
        page_w: page_w.round(),
        page_h: page_h.round(),
        card_w: card_w.round(),
        card_h: card_h.round(),
    }
}
/// Cap on bookmark chrome slots (overflow stays mouse-reachable; magnet
/// slots must stay wide enough to arrive at).
pub const CHROME_MAX_BOOKMARKS: usize = 12;

/// Cursor physics space: the page's focusables plus the synthetic header
/// targets — one continuous magnet/jump/click surface.
pub fn cursor_targets(
    page: &[crate::browser::cursor::Focusable],
    chrome: &[(ChromeTarget, crate::browser::cursor::Focusable)],
) -> Vec<crate::browser::cursor::Focusable> {
    let mut out = Vec::with_capacity(page.len() + chrome.len());
    out.extend_from_slice(page);
    out.extend(chrome.iter().map(|(_, f)| f.clone()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn panel() -> BrowserPanel {
        let mut panel = BrowserPanel {
            viewport: (900.0, 700.0),
            ..BrowserPanel::default()
        };
        panel.page.cursor.x = 450.0;
        panel.page.cursor.y = 350.0;
        panel
    }

    fn step(panel: &mut BrowserPanel, rx: f32, ry: f32, r2: bool, at: Instant) -> (f32, f32) {
        panel.advance_stick(0.0, 0.0, rx, ry, r2, at)
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
        assert_eq!(panel.scroll_axis, None);
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
    fn header_resets_axis_lock() {
        let mut panel = panel();
        let t0 = Instant::now();
        step(&mut panel, 0.0, 0.0, false, t0);
        let at = t0 + Duration::from_millis(100);
        step(&mut panel, 0.2, -0.9, false, at);
        assert_eq!(panel.scroll_axis, Some(ScrollAxis::Vertical));
        panel.page.cursor.y = -5.0;
        let at = at + Duration::from_millis(100);
        let (dx, dy) = step(&mut panel, 0.2, -0.9, false, at);
        assert_eq!((dx, dy), (0.0, 0.0));
        assert_eq!(panel.scroll_axis, None);
    }
}
