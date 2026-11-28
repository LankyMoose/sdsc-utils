//! Remembered navigation decisions, persisted atomically to
//! `browser_prefs.json`.

use crate::browser::security::navigation::{Remembered, registrable_domain};
use crate::persist::browser_prefs::{BrowserPrefs, NavDecisionAction};
use url::Url;

pub struct DecisionStore {
    prefs: BrowserPrefs,
}

impl DecisionStore {
    pub fn load() -> Self {
        Self {
            prefs: BrowserPrefs::load(),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_prefs(prefs: BrowserPrefs) -> Self {
        Self { prefs }
    }

    pub fn lookup_url(&self, url: &str) -> Option<Remembered> {
        let host = Url::parse(url).ok()?.host_str()?.to_string();
        self.lookup_host(&host)
    }

    pub fn lookup_host(&self, host: &str) -> Option<Remembered> {
        let destination = registrable_domain(host);
        self.prefs
            .navigation_decisions
            .iter()
            .rev()
            .find(|entry| entry.destination == destination)
            .map(|entry| match entry.action {
                NavDecisionAction::Block => Remembered::Block,
                NavDecisionAction::Allow => Remembered::Allow,
            })
    }

    pub fn remember_url(&mut self, url: &str, action: NavDecisionAction) {
        let Ok(parsed) = Url::parse(url) else {
            return;
        };
        let Some(host) = parsed.host_str() else {
            return;
        };
        let destination = registrable_domain(host);
        if destination.is_empty() {
            return;
        }
        self.prefs.remember(&destination, action);
    }

    /// Remember and persist; storage failure does not block the action.
    pub fn remember_url_persisted(&mut self, url: &str, action: NavDecisionAction) {
        self.remember_url(url, action);
        self.prefs.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::browser_prefs::NavDecision;

    fn store(entries: &[(&str, NavDecisionAction)]) -> DecisionStore {
        let mut prefs = BrowserPrefs::default();
        for (destination, action) in entries {
            prefs.navigation_decisions.push(NavDecision {
                destination: (*destination).to_string(),
                action: *action,
                created_unix: 0,
            });
        }
        DecisionStore::from_prefs(prefs)
    }

    #[test]
    fn lookup_matches_registrable_domain() {
        let store = store(&[("giftdash.rest", NavDecisionAction::Block)]);
        assert_eq!(
            store.lookup_url("https://www.giftdash.rest/bonus?x=1"),
            Some(Remembered::Block)
        );
        assert_eq!(store.lookup_url("https://example.com/"), None);
    }

    #[test]
    fn allow_and_block_round_trip() {
        let mut store = store(&[]);
        store.remember_url("https://sub.example.com/a", NavDecisionAction::Allow);
        assert_eq!(
            store.lookup_url("https://other.example.com/b"),
            Some(Remembered::Allow)
        );
        store.remember_url("https://example.com/c", NavDecisionAction::Block);
        assert_eq!(
            store.lookup_url("https://example.com/d"),
            Some(Remembered::Block)
        );
    }
}
