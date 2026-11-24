//! Controller-first browser internals: engine seam, CEF OSR worker, DOM
//! target query, cursor physics, frame slots, tabs/bookmarks, and URL helpers.
//!
//! Decision record (Nov 2026): the browser renders open web with no DRM
//! requirement, so the backend is CEF offscreen composited as an iced
//! texture. A `wry`/WebView2 child HWND was rejected (native child draws
//! above the iced/wgpu canvas, breaking toasts, idle/sleep veils, and overlay
//! surfaces). Chromium's real TLS/JS is why CEF replaced Servo.
//!
//! App lifecycle (which app is foreground) lives in [`crate::app::Home`], not
//! here. This module is engine/runtime plumbing plus pure protocol types.

pub mod bookmarks;
#[cfg(feature = "cef-engine")]
pub mod cef_backend;
pub mod cursor;
pub mod dom;
pub mod engine;
#[cfg(feature = "cef-engine")]
pub mod engine_install;
pub mod favicon;
pub mod frame_slot;
pub mod keys;
pub mod nav;
pub mod tabs;

/// One remembered navigation for omnibox autocomplete: URL plus the title
/// at visit time. In-memory only (session scope, never persisted).
#[derive(Debug, Clone, Default)]
pub struct HistoryEntry {
    pub url: String,
    pub title: Option<String>,
}

/// Cap on remembered navigations: oldest drops past this.
pub const MAX_HISTORY_ENTRIES: usize = 100;

/// Autocomplete rows per dropdown.
pub const MAX_SUGGESTIONS: usize = 6;

/// History-selector rows per dropdown (closest first, controller-scannable).
pub const MAX_SELECTOR_ENTRIES: usize = 8;

/// Default page: hardcoded default until the settings screen lands a
/// configurable default-page pref. The engine also boots directly onto this (no
/// about:blank document), so back/forward and the selector start clean.
pub const BROWSER_DEFAULT_URL: &str = "https://www.google.com";

/// The empty document: never a history entry (autocomplete or session).
/// The engine used to boot on it before the initial URL was configured;
/// JS can still navigate to it — it is a placeholder, not a visit.
pub const BLANK_PAGE_URL: &str = "about:blank";

/// True for URLs that must never be remembered as visits.
fn is_ignored_url(url: &str) -> bool {
    url.is_empty() || url == BLANK_PAGE_URL
}

/// Browser surface state. Pure data: the iced view owns the URL
/// draft and scroll offset, this owns load lifecycle with a generation
/// counter so late worker results are ignored after suspend/close —
/// the same stale-generation discipline as the Launcher art worker.
#[derive(Debug, Clone, Default)]
pub struct BrowserSession {
    open: bool,
    url: String,
    title: Option<String>,
    loading: bool,
    generation: u64,
    error: Option<String>,
    /// Latest engine frame for this tab. The GPU primitive uploads unseen
    /// generations into one stable texture (no per-frame image handles).
    frame: std::sync::Arc<frame_slot::FrameSlot>,
    /// Most-recent-first navigation history for autocomplete. Recorded on
    /// every load and client-side URL change; titles attach out of band.
    history: Vec<HistoryEntry>,
    /// Chronological session stack (oldest first) + live index for the
    /// hold-bumper history selector (B2.4). This is a projection of CEF's
    /// canonical navigation entries, not an inference from URL changes:
    /// `apply_engine_history` is the only mutation. `history` remains the
    /// MRU-deduped autocomplete list.
    nav_stack: Vec<HistoryEntry>,
    nav_index: Option<usize>,
}

impl BrowserSession {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open_surface(&mut self) {
        self.open = true;
    }

    /// Hide the surface and cancel any in-flight load. Cached page text is
    /// kept so reopening (resume) paints instantly — the background-suspend
    /// half of the multi-app contract.
    pub fn close_surface(&mut self) {
        self.open = false;
        self.cancel_load();
    }

    /// Background suspend without hiding: cancels in-flight work, keeps the
    /// cached page for an instant resume.
    pub fn cancel_load(&mut self) {
        if self.loading {
            self.loading = false;
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// Begin loading `url`. Returns the generation the worker result must
    /// carry to be applied. Drops any engine frame (stale pixels never stand
    /// in for the new page).
    pub fn begin_load(&mut self, url: String) -> u64 {
        self.url = url.clone();
        self.title = None;
        self.error = None;
        self.frame.clear();
        self.loading = true;
        self.generation = self.generation.wrapping_add(1);
        self.record_visit(url);
        self.generation
    }

    /// Begin a CEF-native reload. Unlike `begin_load`, this keeps the live
    /// frame and title; the page repaints when CEF commits the reload. It
    /// does not add a history entry or a new MRU visit.
    pub fn begin_reload(&mut self) -> u64 {
        self.error = None;
        self.loading = true;
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    pub fn apply_failed(&mut self, generation: u64, error: String) -> bool {
        if generation != self.generation {
            return false;
        }
        self.loading = false;
        self.error = Some(error);
        true
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn loading(&self) -> bool {
        self.loading
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// True once a page (or a load error) is on screen.
    pub fn has_content(&self) -> bool {
        self.error.is_some() || self.frame.has_frame()
    }

    /// Engine path: install the latest RGBA frame. `FrameSlot::write`
    /// rejects mismatched buffers and bumps the generation the GPU
    /// primitive watches.
    pub fn write_frame(&mut self, width: u32, height: u32, rgba: Vec<u8>) {
        self.frame.write(width, height, rgba);
    }

    pub fn clear_frame(&mut self) {
        self.frame.clear();
    }

    pub fn has_frame(&self) -> bool {
        self.frame.has_frame()
    }

    /// Slot for the stable-texture shader widget.
    pub fn frame_slot(&self) -> &std::sync::Arc<frame_slot::FrameSlot> {
        &self.frame
    }

    /// Replace the placeholder slot with the worker-owned one (boot tab or
    /// an engine-created popup). The old slot drops; its texture entry is
    /// trimmed by the render pipeline.
    pub fn set_frame_slot(&mut self, slot: std::sync::Arc<frame_slot::FrameSlot>) {
        self.frame = slot;
    }

    /// Engine path: title arrived out of band. Attaches to the most recent
    /// history entry (titles trail their document; a stale late title may
    /// misattach until the next one lands — harmless for suggestions) and
    /// to the live session entry when the URLs match.
    pub fn note_title(&mut self, title: Option<String>) {
        self.title = title.clone();
        if let Some(entry) = self.history.first_mut() {
            entry.title = title.clone();
        }
        if let Some(i) = self.nav_index
            && let Some(entry) = self.nav_stack.get_mut(i)
            && Self::same_doc(&entry.url, &self.url)
        {
            entry.title = title;
        }
    }

    /// Engine path: URL changed (redirects) without a new load. This only
    /// updates the live URL and MRU autocomplete; session history waits for
    /// CEF's canonical `HistoryChanged` snapshot.
    pub fn note_url(&mut self, url: String) {
        self.url = url.clone();
        self.record_visit(url);
    }

    /// Remember a navigation (most-recent-first, deduped, capped).
    /// Revisits move to the front; empty URLs never record.
    pub fn record_visit(&mut self, url: String) {
        if is_ignored_url(&url) {
            return;
        }
        self.history.retain(|e| e.url != url);
        self.history.insert(0, HistoryEntry { url, title: None });
        self.history.truncate(MAX_HISTORY_ENTRIES);
    }

    /// Fragment-stripped document identity: same-document traffic
    /// (fragments) must not fork the session stack.
    fn same_doc(a: &str, b: &str) -> bool {
        fn strip(s: &str) -> &str {
            s.split('#').next().unwrap_or(s)
        }
        !a.is_empty() && !b.is_empty() && strip(a) == strip(b)
    }

    /// Overwrite the session stack with CEF's canonical navigation entries.
    /// Titles are per entry (CEF reports them), so duplicate URLs no longer
    /// share a title. The projection keeps every entry CEF reports; the
    /// selector caps only what it displays.
    pub fn apply_engine_history(
        &mut self,
        entries: &[crate::browser::engine::EngineHistoryEntry],
        current: usize,
    ) {
        let before_current = self.nav_index;
        self.nav_stack = entries
            .iter()
            .map(|entry| HistoryEntry {
                url: entry.url.clone(),
                title: entry.title.clone().filter(|title| !title.is_empty()),
            })
            .collect();
        self.nav_index = if self.nav_stack.is_empty() {
            None
        } else {
            Some(current.min(self.nav_stack.len() - 1))
        };
        crate::platform::trace::event(crate::platform::trace::Lane::Browser, "history.canonical")
            .field("requested", current)
            .field("before", before_current)
            .field("after", self.nav_index)
            .field("len", self.nav_stack.len())
            .emit();
    }

    /// Live session index (None when the stack is empty).
    pub fn nav_current(&self) -> Option<usize> {
        self.nav_index
    }

    pub fn nav_len(&self) -> usize {
        self.nav_stack.len()
    }

    /// Full chronological session stack (oldest first). Tracing reads this
    /// to record the exact history state a selector index refers to.
    pub fn nav_entries(&self) -> &[HistoryEntry] {
        &self.nav_stack
    }

    pub fn can_go_back(&self) -> bool {
        self.nav_index
            .is_some_and(|i| i > 0 && !self.nav_stack.is_empty())
    }

    pub fn can_go_forward(&self) -> bool {
        self.nav_index.is_some_and(|i| i + 1 < self.nav_stack.len())
    }

    /// Back targets, closest first: `(absolute_index, entry)`. Capped so the
    /// selector dropdown stays controller-scannable.
    pub fn back_entries(&self) -> Vec<(usize, HistoryEntry)> {
        let Some(cur) = self.nav_index else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for i in (0..cur.min(self.nav_stack.len())).rev() {
            out.push((i, self.nav_stack[i].clone()));
            if out.len() >= MAX_SELECTOR_ENTRIES {
                break;
            }
        }
        out
    }

    /// Forward targets, closest first: `(absolute_index, entry)`.
    pub fn forward_entries(&self) -> Vec<(usize, HistoryEntry)> {
        let Some(cur) = self.nav_index else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for i in (cur + 1)..self.nav_stack.len() {
            out.push((i, self.nav_stack[i].clone()));
            if out.len() >= MAX_SELECTOR_ENTRIES {
                break;
            }
        }
        out
    }

    pub fn nav_entry(&self, index: usize) -> Option<&HistoryEntry> {
        self.nav_stack.get(index)
    }

    /// Autocomplete candidates for `draft`: case-insensitive substring over
    /// URL + title, prefix hits first, recency order within ranks, capped.
    /// Empty drafts match nothing (Chrome shows recents on focus; v1 shows
    /// the dropdown only once the user typed).
    pub fn suggestions(&self, draft: &str) -> Vec<HistoryEntry> {
        let needle = draft.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        // Two ranks over the whole history (prefix hits anywhere beat
        // substring hits everywhere); recency order within ranks.
        let mut prefix = Vec::new();
        let mut rest = Vec::new();
        for entry in &self.history {
            let url_lc = entry.url.to_lowercase();
            let title_lc = entry.title.as_deref().unwrap_or("").to_lowercase();
            if !(url_lc.contains(&needle) || title_lc.contains(&needle)) {
                continue;
            }
            if url_lc.starts_with(&needle) || title_lc.starts_with(&needle) {
                prefix.push(entry.clone());
            } else {
                rest.push(entry.clone());
            }
        }
        prefix.extend(rest);
        prefix.truncate(MAX_SUGGESTIONS);
        prefix
    }

    /// Engine path: load completed. Returns false (ignored) when a newer
    /// load has since begun.
    pub fn mark_loaded(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.loading = false;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_dedupes_revisits_to_front() {
        let mut s = BrowserSession::default();
        s.record_visit("https://a.test/".to_string());
        s.record_visit("https://b.test/".to_string());
        s.record_visit("https://a.test/".to_string());
        let urls: Vec<&str> = s.history.iter().map(|e| e.url.as_str()).collect();
        assert_eq!(urls, vec!["https://a.test/", "https://b.test/"]);
        // Empty URLs never record.
        s.record_visit(String::new());
        assert_eq!(s.history.len(), 2);
    }

    #[test]
    fn history_attaches_out_of_band_titles() {
        let mut s = BrowserSession::default();
        s.record_visit("https://a.test/".to_string());
        s.note_title(Some("A page".to_string()));
        assert_eq!(s.history[0].title.as_deref(), Some("A page"));
    }

    #[test]
    fn suggestions_rank_prefix_first_then_recency() {
        let mut s = BrowserSession::default();
        // Recency order (oldest first here): sub hit, then prefix hit.
        s.record_visit("https://x.test/deep-meerkat-page".to_string());
        s.record_visit("https://meerkat.test/".to_string());
        let got: Vec<String> = s
            .suggestions("meerkat")
            .iter()
            .map(|e| e.url.clone())
            .collect();
        // Prefix hit wins despite being newer-or-not; recency within ranks.
        assert_eq!(
            got,
            vec![
                "https://meerkat.test/".to_string(),
                "https://x.test/deep-meerkat-page".to_string()
            ]
        );
        // Title hits count too; empty draft matches nothing.
        s.note_title(Some("Meerkats are great".to_string()));
        assert_eq!(s.suggestions("great").len(), 1);
        assert!(s.suggestions("").is_empty());
        assert!(s.suggestions("zzz").is_empty());
    }

    #[test]
    fn session_close_cancels_in_flight_but_keeps_url() {
        let mut s = BrowserSession::default();
        s.open_surface();
        let _ = s.begin_load("https://example.com".to_string());
        // Suspend mid-load of page 2: the target URL is kept so reopening
        // lands on the user's intent; the engine has no in-flight load.
        let _ = s.begin_load("https://example.com/2".to_string());
        s.close_surface();
        assert!(!s.is_open());
        assert!(!s.loading());
        assert_eq!(s.url(), "https://example.com/2");
    }

    #[test]
    fn session_failure_surfaces_error() {
        let mut s = BrowserSession::default();
        s.open_surface();
        let generation = s.begin_load("https://down.test".to_string());
        assert!(s.apply_failed(generation, "fetch failed: dns".to_string()));
        assert!(!s.loading());
        assert_eq!(s.error(), Some("fetch failed: dns"));
        assert!(s.has_content());
    }

    #[test]
    fn engine_frame_counts_as_content_and_clears_on_navigate() {
        let mut s = BrowserSession::default();
        s.open_surface();
        assert!(!s.has_content());
        s.write_frame(800, 600, vec![0; 800 * 600 * 4]);
        assert!(s.has_frame());
        assert!(s.has_content());
        let generation = s.begin_load("https://next.test".to_string());
        assert!(!s.has_frame());
        assert!(s.mark_loaded(generation));
        assert!(!s.loading());
        // Stale completion ignored.
        assert!(!s.mark_loaded(generation.wrapping_add(1)));
    }

    fn entry(url: &str, title: Option<&str>) -> crate::browser::engine::EngineHistoryEntry {
        crate::browser::engine::EngineHistoryEntry {
            url: url.to_string(),
            display_url: url.to_string(),
            original_url: url.to_string(),
            title: title.map(str::to_string),
            has_post_data: false,
        }
    }

    #[test]
    fn blank_page_never_records() {
        // The engine boots straight onto the default page now, but JS can still land
        // on about:blank - a placeholder, never a visit, in either stack.
        let mut s = BrowserSession::default();
        s.begin_load("https://a.test/".to_string());
        s.record_visit(BLANK_PAGE_URL.to_string());
        s.note_url(BLANK_PAGE_URL.to_string());
        // URL changes no longer synthesize history; CEF owns that stack.
        assert_eq!(s.nav_len(), 0);
        assert_eq!(s.nav_current(), None);
        assert_eq!(s.url(), BLANK_PAGE_URL);
        assert!(s.history.iter().all(|e| e.url != BLANK_PAGE_URL));
        assert!(!s.can_go_back());
    }

    #[test]
    fn note_url_does_not_mutate_engine_history() {
        let mut s = BrowserSession::default();
        let entries = vec![
            entry("https://a.test/", Some("A")),
            entry("https://b.test/", Some("B")),
        ];
        s.apply_engine_history(&entries, 0);
        s.note_url("https://a.test/next".to_string());
        assert_eq!(s.nav_len(), 2);
        assert_eq!(s.nav_current(), Some(0));
        assert_eq!(
            s.nav_entry(0).map(|e| e.url.as_str()),
            Some("https://a.test/")
        );
        assert_eq!(
            s.nav_entry(1).map(|e| e.url.as_str()),
            Some("https://b.test/")
        );
    }

    #[test]
    fn engine_history_projects_entries_and_current() {
        let mut s = BrowserSession::default();
        assert!(s.nav_current().is_none());
        assert!(!s.can_go_back());
        assert!(!s.can_go_forward());
        let entries = vec![
            entry("https://a.test/", Some("A")),
            entry("https://b.test/", Some("B")),
            entry("https://c.test/", Some("C")),
        ];
        s.apply_engine_history(&entries, 2);
        assert_eq!(s.nav_current(), Some(2));
        assert!(s.can_go_back());
        assert!(!s.can_go_forward());
        let back: Vec<usize> = s.back_entries().iter().map(|(i, _)| *i).collect();
        assert_eq!(back, vec![1, 0]);
        assert!(s.forward_entries().is_empty());
    }

    #[test]
    fn engine_history_projection_preserves_forward_tail() {
        let mut s = BrowserSession::default();
        let entries: Vec<_> = ["a", "b", "c", "d"]
            .into_iter()
            .map(|host| entry(&format!("https://{host}.test/"), None))
            .collect();
        s.apply_engine_history(&entries, 3);
        s.apply_engine_history(&entries, 0);
        assert_eq!(s.nav_current(), Some(0));
        assert_eq!(s.nav_len(), 4);
        assert_eq!(s.forward_entries().len(), 3);
        s.apply_engine_history(&entries, 3);
        assert_eq!(s.nav_current(), Some(3));
        assert_eq!(s.nav_len(), 4);
        assert!(s.forward_entries().is_empty());
    }

    #[test]
    fn engine_history_current_moves_without_rewriting_entries() {
        let mut s = BrowserSession::default();
        let entries = vec![
            entry("https://a.test/", None),
            entry("https://b.test/", None),
            entry("https://c.test/", None),
        ];
        s.apply_engine_history(&entries, 2);
        s.apply_engine_history(&entries, 1);
        assert_eq!(s.nav_current(), Some(1));
        assert_eq!(s.nav_len(), 3);
        assert_eq!(
            s.nav_entry(0).map(|e| e.url.as_str()),
            Some("https://a.test/")
        );
        assert_eq!(
            s.nav_entry(1).map(|e| e.url.as_str()),
            Some("https://b.test/")
        );
        assert_eq!(
            s.nav_entry(2).map(|e| e.url.as_str()),
            Some("https://c.test/")
        );
        assert!(s.can_go_back());
        assert!(s.can_go_forward());
        s.apply_engine_history(&entries, 2);
        assert_eq!(s.nav_current(), Some(2));
        assert!(!s.can_go_forward());
    }

    #[test]
    fn engine_history_titles_are_per_entry() {
        let mut s = BrowserSession::default();
        let entries = vec![
            entry("https://a.test/", Some("First visit")),
            entry("https://a.test/", Some("Second visit")),
        ];
        s.apply_engine_history(&entries, 1);
        assert_eq!(s.nav_len(), 2);
        assert_eq!(
            s.nav_entry(0).and_then(|e| e.title.as_deref()),
            Some("First visit")
        );
        assert_eq!(
            s.nav_entry(1).and_then(|e| e.title.as_deref()),
            Some("Second visit")
        );
        // Empty engine history clears the stack (no entries -> no selector).
        s.apply_engine_history(&[], 0);
        assert!(s.nav_current().is_none());
        assert!(s.back_entries().is_empty());
    }

    #[test]
    fn engine_history_clamps_current_and_keeps_full_stack() {
        let mut s = BrowserSession::default();
        let entries: Vec<_> = (0..101)
            .map(|index| entry(&format!("https://{index}.test/"), None))
            .collect();
        s.apply_engine_history(&entries, 999);
        assert_eq!(s.nav_len(), 101);
        assert_eq!(s.nav_current(), Some(100));
    }

    #[test]
    fn reload_keeps_history_and_bumps_generation() {
        let mut s = BrowserSession::default();
        let entries = vec![
            entry("https://a.test/", Some("A")),
            entry("https://b.test/", Some("B")),
        ];
        s.apply_engine_history(&entries, 1);
        let before = s.generation();
        let generation = s.begin_reload();
        assert_eq!(generation, before.wrapping_add(1));
        assert!(s.loading());
        assert_eq!(s.nav_len(), 2);
        assert_eq!(s.nav_current(), Some(1));
        assert!(s.mark_loaded(generation));
        assert!(!s.loading());
    }
}
