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
}
