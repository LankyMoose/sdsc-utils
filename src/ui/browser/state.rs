//! Browser app state: page state, cursor, text keyboard, and dormant tab data.
//!
//! v0 is chrome-less: one page surface, no tab strip/toolbar/address/status.
//! Tab/session/bookmark backends stay available for the future contract work,
//! but the panel only ever renders and drives the live page.

use std::collections::HashMap;
use std::time::Instant;

/// Last canonical history snapshot reported by the engine, kept alongside the
/// UI's projection so traces can expose disagreement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineHistorySnapshot {
    pub current: usize,
    pub entries: Vec<crate::browser::engine::EngineHistoryEntry>,
}

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

/// Per-tab page state: everything that belongs to one tab's document.
/// `BrowserPanel::page` is always the ACTIVE tab's; background tabs stash
/// theirs in `BrowserPanel::stored`. The panel derefs to this, so all
/// existing live-tab call sites (`panel.session`, `panel.cursor`, …) keep
/// working.
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
    /// Last canonical engine history; tracing compares it with the UI stack.
    pub last_engine_history: Option<EngineHistorySnapshot>,
    /// Last CEF back/forward capability flag pair.
    pub last_engine_can: Option<(bool, bool)>,
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

/// One on-screen keyboard key. Character keys commit text; the rest are
/// editing/control actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OskKey {
    Char(char),
    Space,
    Backspace,
    Enter,
    Layer,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OskLayer {
    #[default]
    Alpha,
    Symbol,
}

const OSK_ALPHA: &[&[OskKey]] = &[
    &[
        OskKey::Char('q'),
        OskKey::Char('w'),
        OskKey::Char('e'),
        OskKey::Char('r'),
        OskKey::Char('t'),
        OskKey::Char('y'),
        OskKey::Char('u'),
        OskKey::Char('i'),
        OskKey::Char('o'),
        OskKey::Char('p'),
    ],
    &[
        OskKey::Char('a'),
        OskKey::Char('s'),
        OskKey::Char('d'),
        OskKey::Char('f'),
        OskKey::Char('g'),
        OskKey::Char('h'),
        OskKey::Char('j'),
        OskKey::Char('k'),
        OskKey::Char('l'),
    ],
    &[
        OskKey::Char('z'),
        OskKey::Char('x'),
        OskKey::Char('c'),
        OskKey::Char('v'),
        OskKey::Char('b'),
        OskKey::Char('n'),
        OskKey::Char('m'),
    ],
    &[
        OskKey::Layer,
        OskKey::Space,
        OskKey::Backspace,
        OskKey::Enter,
        OskKey::Close,
    ],
];

const OSK_SYMBOL: &[&[OskKey]] = &[
    &[
        OskKey::Char('1'),
        OskKey::Char('2'),
        OskKey::Char('3'),
        OskKey::Char('4'),
        OskKey::Char('5'),
        OskKey::Char('6'),
        OskKey::Char('7'),
        OskKey::Char('8'),
        OskKey::Char('9'),
        OskKey::Char('0'),
    ],
    &[
        OskKey::Char('-'),
        OskKey::Char('/'),
        OskKey::Char(':'),
        OskKey::Char(';'),
        OskKey::Char('('),
        OskKey::Char(')'),
        OskKey::Char('$'),
        OskKey::Char('&'),
        OskKey::Char('@'),
        OskKey::Char('"'),
    ],
    &[
        OskKey::Char('.'),
        OskKey::Char(','),
        OskKey::Char('?'),
        OskKey::Char('!'),
        OskKey::Char('\''),
        OskKey::Char('#'),
        OskKey::Char('%'),
        OskKey::Char('+'),
        OskKey::Char('='),
    ],
    &[
        OskKey::Layer,
        OskKey::Space,
        OskKey::Backspace,
        OskKey::Enter,
        OskKey::Close,
    ],
];

/// Controller-driven on-screen keyboard for page text entry.
#[derive(Debug, Clone, Default)]
pub struct OskState {
    pub open: bool,
    pub focus: usize,
    pub layer: OskLayer,
    /// Opened because a page text target gained focus. Blur closes only this
    /// kind, so a deliberately opened OSK stays put.
    pub auto_opened: bool,
}

impl OskState {
    /// Open from a page-focus event (no-op if already open).
    pub fn open_auto(&mut self) {
        if !self.open {
            self.open = true;
            self.auto_opened = true;
        }
    }

    /// Open from an explicit press. The caller's manual choice wins over
    /// focus-based lifetime from here on.
    pub fn open_manual(&mut self) {
        self.open = true;
        self.auto_opened = false;
    }

    /// Close and forget why it was open.
    pub fn close(&mut self) {
        self.open = false;
        self.auto_opened = false;
    }

    /// Close only when focus opened it; a manual OSK survives blur.
    pub fn close_auto(&mut self) {
        if self.auto_opened {
            self.close();
        }
    }

    pub fn rows(&self) -> &'static [&'static [OskKey]] {
        match self.layer {
            OskLayer::Alpha => OSK_ALPHA,
            OskLayer::Symbol => OSK_SYMBOL,
        }
    }

    pub fn key_count(&self) -> usize {
        self.rows().iter().map(|row| row.len()).sum()
    }

    pub fn focus_key(&self) -> OskKey {
        self.rows()
            .iter()
            .flat_map(|row| row.iter().copied())
            .nth(self.focus)
            .unwrap_or(OskKey::Close)
    }

    pub fn move_focus(&mut self, direction: crate::browser::cursor::JumpDir) {
        use crate::browser::cursor::JumpDir;
        let rows = self.rows();
        let mut start = 0;
        let mut position = (0usize, 0usize);
        for (r, row) in rows.iter().enumerate() {
            if self.focus < start + row.len() {
                position = (r, self.focus - start);
                break;
            }
            start += row.len();
        }
        let (mut row, mut col) = position;
        match direction {
            JumpDir::Left => {
                let len = rows[row].len();
                col = if col == 0 { len - 1 } else { col - 1 };
            }
            JumpDir::Right => {
                let len = rows[row].len();
                col = (col + 1) % len;
            }
            JumpDir::Up => {
                row = row.saturating_sub(1);
                col = col.min(rows[row].len() - 1);
            }
            JumpDir::Down => {
                row = (row + 1).min(rows.len() - 1);
                col = col.min(rows[row].len() - 1);
            }
        }
        self.focus = rows.iter().take(row).map(|r| r.len()).sum::<usize>() + col;
    }

    pub fn toggle_layer(&mut self) {
        self.layer = match self.layer {
            OskLayer::Alpha => OskLayer::Symbol,
            OskLayer::Symbol => OskLayer::Alpha,
        };
        self.focus = self.focus.min(self.key_count().saturating_sub(1));
    }
}

impl OskKey {
    pub fn label(self, layer: OskLayer) -> String {
        match self {
            OskKey::Char(ch) => ch.to_string(),
            OskKey::Space => "Space".to_string(),
            OskKey::Backspace => "Bksp".to_string(),
            OskKey::Enter => "Enter".to_string(),
            OskKey::Layer => match layer {
                OskLayer::Alpha => "123".to_string(),
                OskLayer::Symbol => "abc".to_string(),
            },
            OskKey::Close => "Close".to_string(),
        }
    }
}

/// Browser app panel. The page surface and cursor live here; load lifecycle
/// (generation-guarded suspend/resume) lives in
/// [`crate::browser::BrowserSession`]. The pad cursor roams in engine page
/// pixels (see [`crate::browser::cursor`]).
#[derive(Debug, Clone)]
pub struct BrowserPanel {
    /// Live page state: ALWAYS the active tab's. Swapped with `stored`
    /// entries on switch (see `activate_stored`). v0 is single-tab and has
    /// no switching UI; the dormant tab data stays for the future contract.
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
    /// Last page-pointer action (mouse click or pad Cross). Auto-OSK ignores
    /// programmatic page-load autofocus unless this is recent, so opening the
    /// Browser on a search page does not immediately cover it with the
    /// keyboard.
    pub page_user_input_at: Option<Instant>,
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
    /// On-screen keyboard overlay (controller text entry).
    pub osk: OskState,
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
    /// Live page-area size in card pixels (the engine viewport resizes to
    /// this; the cursor space matches it 1:1).
    pub viewport: (f32, f32),
    /// A D-pad jump asked for a fresh DOM target cull around the cursor.
    /// Cleared once the jump flight lands and the engine is told to pull.
    pub dom_reseed_pending: bool,
    /// Browser debug overlay + shots (Diagnostics toggle / F10, mirrored
    /// from App). Never persisted.
    pub debug_overlay: bool,
    /// Chrome-style bookmarks (disk-persisted, dormant in v0). Kept as data
    /// for the future contract; no UI, bindings, or favicon chrome.
    pub bookmarks: Vec<crate::browser::bookmarks::Bookmark>,
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
            page_user_input_at: None,
            page_mouse_active: false,
            page_mouse_down: false,
            page_mouse_modifiers: keyboard_types::Modifiers::empty(),
            page_last_click: None,
            page_click_count: 1,
            page_cursor: crate::browser::engine::BrowserCursor::Default,
            last_shot: None,
            osk: OskState::default(),
            last_dock_change: None,
            js_click: false,
            r3_latched: false,
            viewport: (
                crate::browser::cursor::VIEWPORT_W,
                crate::browser::cursor::VIEWPORT_H,
            ),
            dom_reseed_pending: false,
            debug_overlay: false,
            bookmarks: Vec::new(),
        }
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
        // The old page's focused field no longer owns the text keyboard.
        self.osk.close_auto();
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
        self.analog_at = None;
        self.scroll_axis = None;
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
        // Docking transitions are rare and are the cursor's most surprising
        // behavior (magnet centre snap, break-free kick); keep the trace in
        // release too so user-marked nudges are diagnosable.
        let before = self.page.cursor.attached;
        let (bx, by) = (self.page.cursor.x, self.page.cursor.y);
        let scrolling = rx != 0.0 || ry != 0.0;
        let cursor = &mut self.page.cursor;
        cursor.advance_with(lx, ly, dt, &self.page.focusables, scrolling);
        {
            let after = self.page.cursor.attached;
            if before != after {
                let describe = |id: Option<u64>| match id {
                    None => "free".to_string(),
                    Some(i) => match self.page.focusables.iter().find(|o| o.id == i) {
                        Some(o) => format!(
                            "#{i} {:?} ({:.0},{:.0},{:.0}x{:.0})",
                            o.kind, o.x, o.y, o.w, o.h
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
    }

    /// D-pad focus step: spatial cone jump over page targets. A press always
    /// asks the engine for a fresh target set around the new cursor.
    pub fn jump_cursor(&mut self, dir: crate::browser::cursor::JumpDir) {
        let from = (self.page.cursor.x, self.page.cursor.y);
        let was = self.page.cursor.attached;
        self.dom_reseed_pending = true;
        self.page.cursor.jump(dir, &self.page.focusables);
        crate::platform::app_log::info(format!(
            "cursor jump {dir:?} from=({:.0},{:.0}) dock={was:?} fly={:?} attached={:?} targets={}",
            from.0,
            from.1,
            self.page.cursor.fly_to,
            self.page.cursor.attached,
            self.page.focusables.len(),
        ));
    }
}

/// Browser popup geometry: the card itself is 16:9, sized from the
/// available viewport height (clamped by width). The page surface fills the
/// card (no browser chrome).
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
    let page_h = (card_h - 2.0 * BROWSER_CARD_PAD).max(1.0);
    BrowserLayout {
        page_w: page_w.round(),
        page_h: page_h.round(),
        card_w: card_w.round(),
        card_h: card_h.round(),
    }
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
        panel.page.cursor.x = 150.0;
        panel.page.cursor.y = 320.0;
        panel.page.cursor.attached = Some(2);
        panel.jump_cursor(crate::browser::cursor::JumpDir::Up);
        assert!(panel.dom_reseed_pending);
        assert_eq!(panel.page.cursor.fly_to, Some((150.0, 120.0)));
    }

    #[test]
    fn osk_focus_wraps_and_layers_keep_the_cursor_in_range() {
        use crate::browser::cursor::JumpDir;
        let mut osk = OskState::default();
        assert_eq!(osk.focus_key(), OskKey::Char('q'));
        osk.move_focus(JumpDir::Left);
        assert_eq!(osk.focus_key(), OskKey::Char('p'));
        osk.move_focus(JumpDir::Down);
        assert_eq!(osk.focus_key(), OskKey::Char('l'));
        osk.move_focus(JumpDir::Down);
        assert_eq!(osk.focus_key(), OskKey::Char('m'));
        osk.move_focus(JumpDir::Down);
        assert_eq!(osk.focus_key(), OskKey::Close);
        osk.move_focus(JumpDir::Up);
        assert_eq!(osk.focus_key(), OskKey::Char('b'));
        osk.toggle_layer();
        assert_eq!(osk.layer, OskLayer::Symbol);
        assert!(osk.focus < osk.key_count());
        assert_eq!(OskKey::Layer.label(osk.layer), "abc");
    }

    #[test]
    fn auto_osk_closes_on_blur_but_manual_osk_survives_it() {
        let mut osk = OskState::default();
        osk.open_auto();
        assert!(osk.open && osk.auto_opened);
        osk.close_auto();
        assert!(!osk.open && !osk.auto_opened);

        osk.open_manual();
        assert!(osk.open && !osk.auto_opened);
        osk.close_auto();
        assert!(osk.open, "manual OSK must survive a page blur");
        osk.close();
        assert!(!osk.open && !osk.auto_opened);

        osk.open_manual();
        osk.open_auto();
        assert!(
            !osk.auto_opened,
            "auto-open must not take ownership of a manual OSK"
        );
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
}
