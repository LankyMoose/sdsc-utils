//! Shared browser security state: one instance per engine process.
//!
//! CEF callbacks are thin adapters over this core. The navigation policy is
//! pure (`navigation.rs`); this type adds the decision store, one-shot
//! Continue tokens, the emergency denylist bridge, and the shared filter
//! engine.

pub mod adblock;
pub mod decisions;
pub mod denylist;
pub mod navigation;

use crate::browser::engine::TabId;
use crate::persist::browser_prefs::NavDecisionAction;
use decisions::DecisionStore;
use navigation::{NavOutcome, NavReason, Remembered};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Upper bound on simultaneously pending Continue prompts.
const MAX_PENDING: usize = 32;

struct Pending {
    tab: TabId,
    url: String,
}

pub struct Evaluation {
    pub outcome: NavOutcome,
    pub token: Option<u64>,
}

pub struct BrowserSecurity {
    decisions: Mutex<DecisionStore>,
    filter: Option<::adblock::Engine>,
    pending: Mutex<HashMap<u64, Pending>>,
    /// Tab -> exact URL permitted once (consumed by `on_before_browse`).
    allow_once: Mutex<HashMap<u64, String>>,
    /// Tab -> next main-frame navigation is app-initiated (address bar,
    /// history, reload, tab open) and must not prompt.
    trust_next: Mutex<std::collections::HashSet<u64>>,
    next_token: AtomicU64,
    blocked_events: AtomicU64,
}

impl Default for BrowserSecurity {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserSecurity {
    pub fn new() -> Self {
        let decisions = DecisionStore::load();
        let filter = adblock::build_engine();
        Self {
            decisions: Mutex::new(decisions),
            filter,
            pending: Mutex::new(HashMap::new()),
            allow_once: Mutex::new(HashMap::new()),
            trust_next: Mutex::new(std::collections::HashSet::new()),
            next_token: AtomicU64::new(1),
            blocked_events: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    fn for_test(decisions: DecisionStore) -> Self {
        Self {
            decisions: Mutex::new(decisions),
            filter: None,
            pending: Mutex::new(HashMap::new()),
            allow_once: Mutex::new(HashMap::new()),
            trust_next: Mutex::new(std::collections::HashSet::new()),
            next_token: AtomicU64::new(1),
            blocked_events: AtomicU64::new(0),
        }
    }

    /// Classify a main-frame navigation for `tab`.
    pub fn evaluate(
        &self,
        tab: TabId,
        source: &str,
        target: &str,
        user_gesture: bool,
    ) -> Evaluation {
        {
            let mut allow_once = self
                .allow_once
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if allow_once.get(&tab.0).is_some_and(|url| url == target) {
                allow_once.remove(&tab.0);
                return Evaluation {
                    outcome: NavOutcome::Allow,
                    token: None,
                };
            }
        }
        let trusted = self
            .trust_next
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&tab.0);
        let remembered = self
            .decisions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .lookup_url(target);
        let outcome = match navigation::classify(source, target, user_gesture, remembered) {
            // An app-initiated navigation (typed URL, tab open, history,
            // reload) is deliberate. The hard denylist still prompts.
            NavOutcome::Prompt(NavReason::CrossSite) if trusted => NavOutcome::Allow,
            outcome => outcome,
        };
        let token = match outcome {
            NavOutcome::Prompt(_) => {
                let token = self.next_token.fetch_add(1, Ordering::Relaxed);
                let mut pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if pending.len() >= MAX_PENDING
                    && let Some(oldest) = pending.keys().next().copied()
                {
                    pending.remove(&oldest);
                }
                pending.insert(
                    token,
                    Pending {
                        tab,
                        url: target.to_string(),
                    },
                );
                Some(token)
            }
            _ => None,
        };
        Evaluation { outcome, token }
    }

    /// Mark the next main-frame navigation for `tab` as app-initiated
    /// (address bar, tab open, history, reload). Redirect hops are evaluated
    /// separately and still prompt.
    pub fn trust_next_navigation(&self, tab: TabId) {
        self.trust_next
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(tab.0);
    }

    /// Accept a Continue for the exact pending prompt. The next
    /// `on_before_browse` for this tab/URL is allowed once.
    pub fn authorize_continue(&self, tab: TabId, url: &str, token: u64) -> bool {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let matches = pending
            .get(&token)
            .is_some_and(|entry| entry.tab == tab && entry.url == url);
        if !matches {
            return false;
        }
        pending.remove(&token);
        self.allow_once
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(tab.0, url.to_string());
        true
    }

    pub fn remember_block(&self, url: &str) {
        self.decisions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remember_url_persisted(url, NavDecisionAction::Block);
    }

    pub fn remember_allow(&self, url: &str) {
        self.decisions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remember_url_persisted(url, NavDecisionAction::Allow);
    }

    /// Subresource check: denylist first, then the filter engine.
    /// Returns the rule source label when the request must be cancelled.
    pub fn blocked_resource(&self, url: &str, source: &str, resource_type: &str) -> Option<String> {
        if let Ok(parsed) = url::Url::parse(url)
            && parsed.host_str().is_some_and(denylist::is_denied)
        {
            return Some("denylist".to_string());
        }
        let engine = self.filter.as_ref()?;
        adblock::blocked(engine, url, source, resource_type)
    }

    /// Trace roughly one in 16 blocked resources, plus the first few.
    pub fn should_trace_blocked(&self) -> bool {
        let count = self.blocked_events.fetch_add(1, Ordering::Relaxed);
        count < 8 || count.is_multiple_of(16)
    }

    /// Saved-tab restore filter: hard-denied destinations and remembered
    /// blocks never come back into the strip.
    pub fn drop_on_restore(url: &str) -> bool {
        Self::drop_with(&DecisionStore::load(), url)
    }

    fn drop_with(decisions: &DecisionStore, url: &str) -> bool {
        if let Ok(parsed) = url::Url::parse(url) {
            if parsed.host_str().is_some_and(denylist::is_denied) {
                return true;
            }
            return decisions.lookup_url(url) == Some(Remembered::Block);
        }
        true
    }

    /// Classify a navigation exactly as the CEF callback would, for the
    /// blocked card's reason copy.
    pub fn reason_key(reason: NavReason) -> &'static str {
        reason.key()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::browser_prefs::{BrowserPrefs, NavDecision};

    fn security(entries: &[(&str, NavDecisionAction)]) -> BrowserSecurity {
        let mut prefs = BrowserPrefs::default();
        for (destination, action) in entries {
            prefs.navigation_decisions.push(NavDecision {
                destination: (*destination).to_string(),
                action: *action,
                created_unix: 0,
            });
        }
        BrowserSecurity::for_test(DecisionStore::from_prefs(prefs))
    }

    #[test]
    fn prompt_continue_is_one_shot() {
        let security = security(&[]);
        let eval = security.evaluate(TabId::BOOT, "https://a.test/", "https://b.test/", false);
        assert_eq!(eval.outcome, NavOutcome::Prompt(NavReason::CrossSite));
        let token = eval.token.expect("prompt token");
        assert!(security.authorize_continue(TabId::BOOT, "https://b.test/", token));
        assert_eq!(
            security
                .evaluate(TabId::BOOT, "https://a.test/", "https://b.test/", false)
                .outcome,
            NavOutcome::Allow
        );
        // Consumed: a second navigation prompts again.
        assert_eq!(
            security
                .evaluate(TabId::BOOT, "https://a.test/", "https://b.test/", false)
                .outcome,
            NavOutcome::Prompt(NavReason::CrossSite)
        );
    }

    #[test]
    fn app_initiated_navigation_is_trusted_once() {
        let security = security(&[]);
        security.trust_next_navigation(TabId::BOOT);
        assert_eq!(
            security
                .evaluate(TabId::BOOT, "https://a.test/", "https://b.test/", false)
                .outcome,
            NavOutcome::Allow
        );
        assert_eq!(
            security
                .evaluate(TabId::BOOT, "https://a.test/", "https://b.test/", false)
                .outcome,
            NavOutcome::Prompt(NavReason::CrossSite)
        );
    }

    #[test]
    fn remembered_block_is_silent_and_survives() {
        let security = security(&[("b.test", NavDecisionAction::Block)]);
        assert_eq!(
            security
                .evaluate(TabId::BOOT, "https://a.test/", "https://b.test/", true)
                .outcome,
            NavOutcome::Block(NavReason::RememberedBlock)
        );
        let decisions = security
            .decisions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(BrowserSecurity::drop_with(&decisions, "https://b.test/x"));
    }

    #[test]
    fn hard_denylist_prompts_and_ids_are_not_restored() {
        let security = security(&[]);
        assert_eq!(
            security
                .evaluate(
                    TabId::BOOT,
                    "https://a.test/",
                    "https://giftdash.rest/bonus",
                    true
                )
                .outcome,
            NavOutcome::Prompt(NavReason::Denylist)
        );
        assert!(BrowserSecurity::drop_on_restore(
            "https://webtransit.live/visit"
        ));
        assert!(!BrowserSecurity::drop_on_restore("https://example.com/"));
    }
}
