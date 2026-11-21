//! Managed-app hub: which controller-first apps exist, which one is active,
//! and the shell effects a transition requires.
//!
//! Phase 0 keeps this pure bookkeeping. The topmost PS hub window, app catalog,
//! and Start registration land in phase 1; the standalone Browser window in
//! phase 2. Start never appears in `active` yet because it is the shell that
//! hosts the hub, not a managed sibling — phase 1 makes that explicit.

use std::collections::HashSet;

/// Stable app identity. Never index rows by order; these ids survive UI
/// reflows and persist into future catalog entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppId {
    Start,
    Browser,
}

/// Shell work required after a hub transition. Order matters: unfocus/hide
/// and suspend the outgoing app before showing/focusing/resuming the
/// incoming one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEffect {
    Show(AppId),
    Hide(AppId),
    SetFocused {
        id: AppId,
        focused: bool,
    },
    /// Stop the app's engine loop: no event spin, no paint/present, media
    /// paused, caches droppable.
    Suspend(AppId),
    /// Reverse of suspend: show, refocus, resume timers and frames.
    Resume(AppId),
    /// Drop all engine resources for a closed app.
    Close(AppId),
}

/// Installed apps plus foreground/suspend bookkeeping.
#[derive(Debug)]
pub struct AppHub {
    installed: HashSet<AppId>,
    active: Option<AppId>,
    suspended: HashSet<AppId>,
}

impl Default for AppHub {
    fn default() -> Self {
        Self::new()
    }
}

// Phase 1 wires install/catalog/suspend-background through the hub window;
// the pure API lands here first so phase 0 stays behavior-preserving.
#[allow(dead_code)]
impl AppHub {
    /// Start is always present (it hosts the hub). Browser is always present
    /// in phase 0 because the reader backend still works without an engine;
    /// the tile-level install affordance arrives with the engine DLC.
    pub fn new() -> Self {
        let installed = HashSet::from([AppId::Start, AppId::Browser]);
        Self {
            installed,
            active: None,
            suspended: HashSet::new(),
        }
    }

    pub fn is_installed(&self, id: AppId) -> bool {
        self.installed.contains(&id)
    }

    pub fn active(&self) -> Option<AppId> {
        self.active
    }

    pub fn is_suspended(&self, id: AppId) -> bool {
        self.suspended.contains(&id)
    }

    /// Register an app (idempotent). Used by the catalog/DLC path later.
    pub fn install(&mut self, id: AppId) {
        self.installed.insert(id);
    }

    /// Foreground an installed app. Reopening the active app is a no-op;
    /// reopening a suspended app resumes it. Unknown/uninstalled ids are
    /// ignored (no effect).
    pub fn open(&mut self, id: AppId) -> Vec<AppEffect> {
        if !self.installed.contains(&id) {
            return Vec::new();
        }
        if self.active == Some(id) {
            return Vec::new();
        }
        let mut fx = Vec::new();
        if let Some(prev) = self.active {
            fx.extend(self.suspend_one(prev));
        }
        self.active = Some(id);
        self.suspended.remove(&id);
        fx.push(AppEffect::Show(id));
        fx.push(AppEffect::SetFocused { id, focused: true });
        fx.push(AppEffect::Resume(id));
        fx
    }

    /// Close an app. Closing the active app leaves no foreground app; the
    /// caller decides whether to return to Start or the hub.
    pub fn close(&mut self, id: AppId) -> Vec<AppEffect> {
        if !self.installed.remove(&id) {
            return Vec::new();
        }
        self.suspended.remove(&id);
        let fx = vec![
            AppEffect::SetFocused { id, focused: false },
            AppEffect::Hide(id),
            AppEffect::Close(id),
        ];
        if self.active == Some(id) {
            self.active = None;
        }
        fx
    }

    /// Suspend every installed app except the active one. Idempotent backstop
    /// for paths that hide the shell without routing through `open`.
    pub fn suspend_background(&mut self) -> Vec<AppEffect> {
        let ids: Vec<AppId> = self
            .installed
            .iter()
            .copied()
            .filter(|id| self.active != Some(*id))
            .collect();
        ids.into_iter()
            .flat_map(|id| self.suspend_one(id))
            .collect()
    }

    fn suspend_one(&mut self, id: AppId) -> Vec<AppEffect> {
        if !self.suspended.insert(id) {
            return Vec::new();
        }
        vec![
            AppEffect::SetFocused { id, focused: false },
            AppEffect::Hide(id),
            AppEffect::Suspend(id),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_phase0_apps_are_installed() {
        let hub = AppHub::new();
        assert!(hub.is_installed(AppId::Start));
        assert!(hub.is_installed(AppId::Browser));
        assert_eq!(hub.active(), None);
    }

    #[test]
    fn first_open_shows_and_resumes() {
        let mut hub = AppHub::new();
        let fx = hub.open(AppId::Browser);
        assert_eq!(
            fx,
            vec![
                AppEffect::Show(AppId::Browser),
                AppEffect::SetFocused {
                    id: AppId::Browser,
                    focused: true
                },
                AppEffect::Resume(AppId::Browser),
            ]
        );
        assert_eq!(hub.active(), Some(AppId::Browser));
        assert!(!hub.is_suspended(AppId::Browser));
    }

    #[test]
    fn opening_another_suspends_the_first() {
        let mut hub = AppHub::new();
        let _ = hub.open(AppId::Browser);
        let fx = hub.open(AppId::Start);
        assert_eq!(
            fx,
            vec![
                AppEffect::SetFocused {
                    id: AppId::Browser,
                    focused: false
                },
                AppEffect::Hide(AppId::Browser),
                AppEffect::Suspend(AppId::Browser),
                AppEffect::Show(AppId::Start),
                AppEffect::SetFocused {
                    id: AppId::Start,
                    focused: true
                },
                AppEffect::Resume(AppId::Start),
            ]
        );
        assert!(hub.is_suspended(AppId::Browser));
        assert!(!hub.is_suspended(AppId::Start));
        assert_eq!(hub.active(), Some(AppId::Start));
    }

    #[test]
    fn reopen_active_is_noop() {
        let mut hub = AppHub::new();
        let _ = hub.open(AppId::Browser);
        assert!(hub.open(AppId::Browser).is_empty());
    }

    #[test]
    fn close_active_clears_foreground_and_emits_close() {
        let mut hub = AppHub::new();
        let _ = hub.open(AppId::Browser);
        let fx = hub.close(AppId::Browser);
        assert_eq!(
            fx,
            vec![
                AppEffect::SetFocused {
                    id: AppId::Browser,
                    focused: false
                },
                AppEffect::Hide(AppId::Browser),
                AppEffect::Close(AppId::Browser),
            ]
        );
        assert_eq!(hub.active(), None);
        assert!(!hub.is_installed(AppId::Browser));
        assert!(hub.close(AppId::Browser).is_empty());
    }

    #[test]
    fn suspend_background_is_idempotent() {
        let mut hub = AppHub::new();
        let _ = hub.open(AppId::Browser);
        let fx = hub.suspend_background();
        assert_eq!(
            fx,
            vec![
                AppEffect::SetFocused {
                    id: AppId::Start,
                    focused: false
                },
                AppEffect::Hide(AppId::Start),
                AppEffect::Suspend(AppId::Start),
            ]
        );
        assert!(hub.suspend_background().is_empty());
    }
}
