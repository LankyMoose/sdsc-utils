//! Network ad/tracker filtering via Brave's `adblock-rust`.
//!
//! The engine is built once from the pinned list snapshot, on the CEF worker
//! thread, and shared by every tab. Cosmetic filtering is intentionally out of
//! scope (Phase 3): only network rules are loaded.

use adblock::Engine;
use adblock::lists::{FilterSet, ParseOptions, RuleTypes};
use adblock::request::Request;

pub const EASYLIST: &str = include_str!("../../../assets/adblock/easylist.txt");
pub const EASYPRIVACY: &str = include_str!("../../../assets/adblock/easyprivacy.txt");
pub const UBLOCK_FILTERS: &str = include_str!("../../../assets/adblock/ublock-filters.txt");

/// Build the shared engine. Network rules only; the pinned snapshot is the
/// source of truth (no updater, no build-time download).
pub fn build_engine() -> Option<Engine> {
    let started = std::time::Instant::now();
    let mut set = FilterSet::new(true);
    for list in [EASYLIST, EASYPRIVACY, UBLOCK_FILTERS] {
        set.add_filter_list(
            list.to_string(),
            ParseOptions {
                rule_types: RuleTypes::NetworkOnly,
                ..ParseOptions::default()
            },
        );
    }
    let engine = Engine::new_with_filter_set(set);
    crate::platform::app_log::info(format!(
        "adblock: loaded EasyList/EasyPrivacy/uBO network rules in {}ms",
        started.elapsed().as_millis()
    ));
    Some(engine)
}

/// `Some(rule)` when the request should be cancelled.
pub fn blocked(engine: &Engine, url: &str, source: &str, resource_type: &str) -> Option<String> {
    let request = Request::new(url, source, resource_type, "GET").ok()?;
    let result = engine.check_network_request(&request);
    if !result.should_block() {
        return None;
    }
    Some(
        result
            .filter
            .as_ref()
            .and_then(|info| info.raw_line.clone())
            .unwrap_or_else(|| "network rule".to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_lists_block_known_ad_script() {
        let engine = build_engine().expect("bundled lists build");
        assert!(
            blocked(
                &engine,
                "https://securepubads.g.doubleclick.net/tag/js/gpt.js",
                "https://example.com/",
                "script"
            )
            .is_some()
        );
    }

    #[test]
    fn allowlisted_auth_redirect_and_same_site_are_not_blocked() {
        let engine = build_engine().expect("bundled lists build");
        assert!(
            blocked(
                &engine,
                "https://accounts.google.com/o/oauth2/auth",
                "https://example.com/",
                "document"
            )
            .is_none()
        );
        assert!(
            blocked(
                &engine,
                "https://example.com/app.js",
                "https://example.com/",
                "script"
            )
            .is_none()
        );
    }
}
