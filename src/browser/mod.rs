//! Multi-app lifecycle for Start: foreground app with suspended background.
//!
//! Decision record (Nov 2026): the in-Start browser renders open web with no
//! DRM requirement, so the target backend is Servo offscreen composited as an
//! iced texture. A `wry`/WebView2 child HWND was rejected for inside-Start use
//! (native child draws above the iced/wgpu canvas, breaking toasts, idle/sleep
//! veils, and overlay surfaces). `retsurf` is a standalone GPL app, not a
//! library — only its controller UX patterns (virtual cursor, link hints,
//! grid/wheel OSK, input maps) carry over. Incompatible pages fall back to the
//! system browser via [`crate::games::launch::launch_target`].
//!
//! This module stays pure (no iced, no HWND, no engine): it decides *which*
//! app is foreground and *what* the shell must do. The iced app applies the
//! [`AppEffect`] values (show/hide, focus, suspend/resume, paint control).

use std::collections::HashMap;

pub mod engine;
pub mod fetch;
pub mod keys;
#[cfg(feature = "servo-engine")]
pub mod servo_backend;

/// Stable app identity. Keys mirror the Start convention of stable play keys
/// (never row indices): games resolve via catalog `play_key`, the browser via
/// a fixed id so suspend/resume survives row reorder.
pub type AppId = String;

/// The single in-Start browser id. One browser surface is enough for the
/// spike; tabs (if ever needed) live inside the engine, not as extra apps.
pub const BROWSER_APP_ID: &str = "browser";

/// What kind of app a record is. Games launch out of Start (Start closes so a
/// topmost cover never sits above the game); the browser lives inside Start
/// as an offscreen texture, so it suspends instead of closing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppKind {
    Game { play_key: String },
    Browser { url: Option<String> },
}

/// Shell work required after a manager step. Order matters: unfocus/hide and
/// suspend the outgoing app before showing/focusing/resuming the incoming one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEffect {
    Show {
        id: AppId,
    },
    Hide {
        id: AppId,
    },
    SetFocused {
        id: AppId,
        focused: bool,
    },
    /// Stop the engine loop for this app: no `spin_event_loop`, no
    /// `paint`/`present`, media paused, caches droppable.
    Suspend {
        id: AppId,
    },
    /// Reverse of suspend: show, refocus, resume timers and frames.
    Resume {
        id: AppId,
    },
    /// Drop all engine resources for a closed app.
    Close {
        id: AppId,
    },
    /// Load a URL in the browser app (no-op for games).
    Navigate {
        id: AppId,
        url: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppRecord {
    kind: AppKind,
    suspended: bool,
}

/// A fetched reader page: the URL that was actually loaded plus the
/// extracted title and body text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchedPage {
    pub url: String,
    pub title: Option<String>,
    pub body: String,
}

/// Engine-rendered page frame, as a stable iced image handle (rebuilt per
/// frame; the atlas re-upload cost is the price of live pixels and only
/// flows while the surface is visible — the worker suspends otherwise).
#[derive(Debug, Clone)]
pub struct BrowserFrame {
    pub width: u32,
    pub height: u32,
    pub handle: iced::widget::image::Handle,
}

impl BrowserFrame {
    pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Self {
        Self {
            width,
            height,
            handle: iced::widget::image::Handle::from_rgba(width, height, rgba),
        }
    }
}

/// In-Start browser surface state. Pure data: the iced view owns the URL
/// draft and scroll offset, this owns load lifecycle with a generation
/// counter so late worker results are ignored after suspend/close —
/// the same stale-generation discipline as the Start art worker.
#[derive(Debug, Clone, Default)]
pub struct BrowserSession {
    open: bool,
    url: String,
    title: Option<String>,
    body: String,
    loading: bool,
    generation: u64,
    error: Option<String>,
    frame: Option<BrowserFrame>,
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
        self.url = url;
        self.title = None;
        self.body.clear();
        self.error = None;
        self.frame = None;
        self.loading = true;
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Apply a worker result. Returns false (ignored) when the generation
    /// no longer matches — the load was suspended or superseded.
    pub fn apply_fetched(&mut self, generation: u64, page: FetchedPage) -> bool {
        if generation != self.generation {
            return false;
        }
        self.loading = false;
        self.url = page.url;
        self.title = page.title;
        self.body = page.body;
        self.error = None;
        true
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

    pub fn body(&self) -> &str {
        &self.body
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

    /// True once a page (or a load error) is on screen — reader text, an
    /// engine frame, or an error alike.
    pub fn has_content(&self) -> bool {
        !self.body.is_empty() || self.error.is_some() || self.frame.is_some()
    }

    /// Engine path: install a fresh frame (no generation — frames are
    /// continuous paint output, staleness is handled by load gating).
    pub fn set_frame(&mut self, frame: BrowserFrame) {
        self.frame = Some(frame);
    }

    pub fn clear_frame(&mut self) {
        self.frame = None;
    }

    pub fn has_frame(&self) -> bool {
        self.frame.is_some()
    }

    pub fn frame(&self) -> Option<&BrowserFrame> {
        self.frame.as_ref()
    }

    /// Engine path: title arrived out of band.
    pub fn note_title(&mut self, title: Option<String>) {
        self.title = title;
    }

    /// Engine path: URL changed (redirects) without a new load.
    pub fn note_url(&mut self, url: String) {
        self.url = url;
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

/// Foreground + suspended-background manager. One app is active; every other
/// tracked app is hidden, unfocused, and suspended.
#[derive(Debug, Default)]
pub struct AppManager {
    apps: HashMap<AppId, AppRecord>,
    /// Recency stack (oldest first). Closing the active app resumes the tail.
    order: Vec<AppId>,
    active: Option<AppId>,
}

impl AppManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn active(&self) -> Option<&AppId> {
        self.active.as_ref()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.apps.contains_key(id)
    }

    pub fn is_suspended(&self, id: &str) -> bool {
        self.apps.get(id).is_some_and(|r| r.suspended)
    }

    pub fn len(&self) -> usize {
        self.apps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }

    /// Open (register if new) and foreground an app. Reopening the active app
    /// is a no-op; reopening a suspended app resumes it.
    pub fn open_app(&mut self, id: AppId, kind: AppKind) -> Vec<AppEffect> {
        match self.apps.get(&id) {
            Some(existing) if existing.kind == kind => self.activate(&id),
            Some(_) => {
                // Same id, new kind (e.g. game row reused for browser): drop
                // the stale record so the engine never mixes backends.
                let close = self.close_app(&id);
                let mut fx = close;
                fx.extend(self.open_app(id, kind));
                fx
            }
            None => {
                self.apps.insert(
                    id.clone(),
                    AppRecord {
                        kind,
                        suspended: true,
                    },
                );
                self.order.push(id.clone());
                self.activate(&id)
            }
        }
    }

    /// Foreground a tracked app. Unknown ids are ignored (no effect).
    pub fn activate(&mut self, id: &str) -> Vec<AppEffect> {
        if !self.apps.contains_key(id) {
            return Vec::new();
        }
        if self.active.as_deref() == Some(id) {
            return Vec::new();
        }
        let mut fx = Vec::new();
        if let Some(prev) = self.active.replace(id.to_string()) {
            fx.extend(self.suspend_one(&prev));
        }
        fx.extend(self.resume_one(id));
        // Most-recently-active sinks to the tail for close-fallback.
        if let Some(pos) = self.order.iter().position(|o| o == id) {
            let id = self.order.remove(pos);
            self.order.push(id);
        }
        fx
    }

    /// Close an app. Closing the active app resumes the most recent survivor
    /// so Start never idles on a dead surface.
    pub fn close_app(&mut self, id: &str) -> Vec<AppEffect> {
        if self.apps.remove(id).is_none() {
            return Vec::new();
        }
        self.order.retain(|o| o != id);
        let mut fx = vec![
            AppEffect::SetFocused {
                id: id.to_string(),
                focused: false,
            },
            AppEffect::Hide { id: id.to_string() },
            AppEffect::Close { id: id.to_string() },
        ];
        if self.active.as_deref() == Some(id) {
            self.active = None;
            if let Some(next) = self.order.last().cloned() {
                fx.extend(self.activate(&next));
            }
        }
        fx
    }

    /// Suspend every tracked app except the active one. Cheap idempotent
    /// backstop for paths that hide Start without routing through activate.
    pub fn suspend_background(&mut self) -> Vec<AppEffect> {
        let ids: Vec<AppId> = self
            .apps
            .keys()
            .filter(|id| self.active.as_deref() != Some(id.as_str()))
            .cloned()
            .collect();
        let mut fx = Vec::new();
        for id in ids {
            fx.extend(self.suspend_one(&id));
        }
        fx
    }

    fn suspend_one(&mut self, id: &str) -> Vec<AppEffect> {
        let Some(record) = self.apps.get_mut(id) else {
            return Vec::new();
        };
        if record.suspended {
            return Vec::new();
        }
        record.suspended = true;
        vec![
            AppEffect::SetFocused {
                id: id.to_string(),
                focused: false,
            },
            AppEffect::Hide { id: id.to_string() },
            AppEffect::Suspend { id: id.to_string() },
        ]
    }

    fn resume_one(&mut self, id: &str) -> Vec<AppEffect> {
        let Some(record) = self.apps.get_mut(id) else {
            return Vec::new();
        };
        record.suspended = false;
        vec![
            AppEffect::Show { id: id.to_string() },
            AppEffect::SetFocused {
                id: id.to_string(),
                focused: true,
            },
            AppEffect::Resume { id: id.to_string() },
        ]
    }
}

/// Convenience: open (or refocus) the single in-Start browser on a URL.
pub fn open_browser(manager: &mut AppManager, url: &str) -> Vec<AppEffect> {
    let mut fx = manager.open_app(BROWSER_APP_ID.to_string(), AppKind::Browser { url: None });
    fx.push(AppEffect::Navigate {
        id: BROWSER_APP_ID.to_string(),
        url: url.to_string(),
    });
    fx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(id: &str) -> (AppId, AppKind) {
        (
            id.to_string(),
            AppKind::Game {
                play_key: format!("steam:{id}"),
            },
        )
    }

    #[test]
    fn first_open_shows_and_resumes_without_suspend() {
        let mut m = AppManager::new();
        let (id, kind) = game("a");
        let fx = m.open_app(id.clone(), kind);
        assert_eq!(
            fx,
            vec![
                AppEffect::Show { id: id.clone() },
                AppEffect::SetFocused {
                    id: id.clone(),
                    focused: true
                },
                AppEffect::Resume { id: id.clone() },
            ]
        );
        assert_eq!(m.active(), Some(&id));
        assert!(!m.is_suspended(&id));
    }

    #[test]
    fn second_open_suspends_first() {
        let mut m = AppManager::new();
        let (a, ka) = game("a");
        let (b, kb) = game("b");
        let _ = m.open_app(a.clone(), ka);
        let fx = m.open_app(b.clone(), kb);
        assert_eq!(
            fx,
            vec![
                AppEffect::SetFocused {
                    id: a.clone(),
                    focused: false
                },
                AppEffect::Hide { id: a.clone() },
                AppEffect::Suspend { id: a.clone() },
                AppEffect::Show { id: b.clone() },
                AppEffect::SetFocused {
                    id: b.clone(),
                    focused: true
                },
                AppEffect::Resume { id: b.clone() },
            ]
        );
        assert!(m.is_suspended(&a));
        assert!(!m.is_suspended(&b));
    }

    #[test]
    fn reopen_active_is_noop() {
        let mut m = AppManager::new();
        let (a, ka) = game("a");
        let _ = m.open_app(a.clone(), ka.clone());
        assert!(m.open_app(a, ka).is_empty());
    }

    #[test]
    fn activate_unknown_is_noop() {
        let mut m = AppManager::new();
        assert!(m.activate("missing").is_empty());
    }

    #[test]
    fn close_active_resumes_most_recent_survivor() {
        let mut m = AppManager::new();
        let (a, ka) = game("a");
        let (b, kb) = game("b");
        let _ = m.open_app(a.clone(), ka);
        let _ = m.open_app(b.clone(), kb);
        // Touch a so it becomes most-recent, then foreground b again.
        let _ = m.activate(&a);
        let _ = m.activate(&b);
        let fx = m.close_app(&b);
        // Tail of fx resumes a.
        assert!(fx.contains(&AppEffect::Close { id: b.clone() }));
        assert!(fx.contains(&AppEffect::Show { id: a.clone() }));
        assert!(fx.contains(&AppEffect::Resume { id: a.clone() }));
        assert_eq!(m.active(), Some(&a));
        assert!(!m.contains(&b));
    }

    #[test]
    fn close_unknown_is_noop() {
        let mut m = AppManager::new();
        assert!(m.close_app("missing").is_empty());
    }

    #[test]
    fn suspend_background_is_idempotent_backstop() {
        let mut m = AppManager::new();
        let (a, ka) = game("a");
        let (b, kb) = game("b");
        let _ = m.open_app(a.clone(), ka);
        let _ = m.open_app(b.clone(), kb);
        // a already suspended by the b open; backstop emits nothing.
        assert!(m.suspend_background().is_empty());
    }

    #[test]
    fn open_browser_emits_navigate() {
        let mut m = AppManager::new();
        let fx = open_browser(&mut m, "https://example.com");
        assert_eq!(m.active(), Some(&BROWSER_APP_ID.to_string()));
        assert!(fx.contains(&AppEffect::Navigate {
            id: BROWSER_APP_ID.to_string(),
            url: "https://example.com".to_string(),
        }));
    }

    fn page(url: &str) -> FetchedPage {
        FetchedPage {
            url: url.to_string(),
            title: Some("T".to_string()),
            body: "body".to_string(),
        }
    }

    #[test]
    fn session_applies_matching_generation_only() {
        let mut s = BrowserSession::default();
        s.open_surface();
        let generation = s.begin_load("https://example.com".to_string());
        assert!(s.loading());
        // Stale worker result (superseded load) is ignored.
        assert!(!s.apply_fetched(generation.wrapping_add(1), page("https://stale.test")));
        assert!(s.loading());
        assert!(s.apply_fetched(generation, page("https://example.com")));
        assert!(!s.loading());
        assert_eq!(s.body(), "body");
        assert!(s.has_content());
    }

    #[test]
    fn session_close_cancels_in_flight_but_keeps_url() {
        let mut s = BrowserSession::default();
        s.open_surface();
        let generation = s.begin_load("https://example.com".to_string());
        assert!(s.apply_fetched(generation, page("https://example.com")));
        // Suspend mid-load of page 2: late result ignored, target URL kept,
        // loader parked — reopen resumes from a coherent idle state.
        let generation2 = s.begin_load("https://example.com/2".to_string());
        let _ = generation2;
        s.close_surface();
        assert!(!s.is_open());
        assert!(!s.loading());
        assert!(!s.apply_fetched(generation2, page("https://example.com/2")));
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
        s.set_frame(BrowserFrame {
            width: 800,
            height: 600,
            handle: iced::widget::image::Handle::from_rgba(800, 600, vec![0; 800 * 600 * 4]),
        });
        assert!(s.has_frame());
        assert!(s.has_content());
        let generation = s.begin_load("https://next.test".to_string());
        assert!(!s.has_frame());
        assert!(s.mark_loaded(generation));
        assert!(!s.loading());
        // Stale completion ignored.
        assert!(!s.mark_loaded(generation.wrapping_add(1)));
    }
}
