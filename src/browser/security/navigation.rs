//! Pure top-level navigation policy (no CEF or UI types).
//!
//! Same-site and user-gesture navigations proceed. Ungestured cross-site
//! navigations prompt, unless the destination is an OAuth/SSO host or the user
//! remembered an allow. The hard denylist always prompts (Continue is still
//! possible), and a remembered block always wins.

use url::Url;

/// OAuth/SSO redirect hosts that must not prompt.
pub const OAUTH_ALLOWLIST: &[&str] = &[
    "accounts.google.com",
    "login.microsoftonline.com",
    "login.live.com",
    "appleid.apple.com",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remembered {
    Allow,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavReason {
    CrossSite,
    Denylist,
    RememberedBlock,
    UnsupportedScheme,
}

impl NavReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::CrossSite => "cross-site navigation needs confirmation",
            Self::Denylist => "blocked redirect destination",
            Self::RememberedBlock => "remembered block",
            Self::UnsupportedScheme => "unsupported destination scheme",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::CrossSite => "cross-site",
            Self::Denylist => "denylist",
            Self::RememberedBlock => "remembered-block",
            Self::UnsupportedScheme => "unsupported-scheme",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavOutcome {
    Allow,
    /// Cancel the navigation and show the Stay/Continue card.
    Prompt(NavReason),
    /// Cancel silently (remembered block).
    Block(NavReason),
}

/// Registrable domain (eTLD+1) for a host; IPs/localhost fall back to the
/// lowercased host itself, which still compares correctly.
pub fn registrable_domain(host: &str) -> String {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host;
    }
    psl::domain_str(&host)
        .map(str::to_ascii_lowercase)
        .unwrap_or(host)
}

fn host_of(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    parsed.host_str().map(registrable_domain)
}

fn host_matches_allowlist(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    OAUTH_ALLOWLIST
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
}

/// Classify a main-frame navigation.
pub fn classify(
    source: &str,
    target: &str,
    user_gesture: bool,
    remembered: Option<Remembered>,
) -> NavOutcome {
    let Ok(url) = Url::parse(target) else {
        return NavOutcome::Block(NavReason::UnsupportedScheme);
    };
    if url.scheme() == "about" || url.scheme() == "file" {
        return NavOutcome::Allow;
    }
    if !matches!(url.scheme(), "http" | "https") {
        return NavOutcome::Block(NavReason::UnsupportedScheme);
    }
    let Some(target_host) = url.host_str() else {
        return NavOutcome::Block(NavReason::UnsupportedScheme);
    };
    let target_domain = registrable_domain(target_host);

    if remembered == Some(Remembered::Block) {
        return NavOutcome::Block(NavReason::RememberedBlock);
    }
    if crate::browser::security::denylist::is_denied(target_host) {
        return NavOutcome::Prompt(NavReason::Denylist);
    }

    let Some(source_domain) = host_of(source) else {
        // No committed source to compare against (first load).
        return NavOutcome::Allow;
    };
    if source_domain == target_domain {
        return NavOutcome::Allow;
    }
    if user_gesture || host_matches_allowlist(target_host) {
        return NavOutcome::Allow;
    }
    if remembered == Some(Remembered::Allow) {
        return NavOutcome::Allow;
    }
    NavOutcome::Prompt(NavReason::CrossSite)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_site_and_subdomains_allow() {
        assert_eq!(
            classify(
                "https://wealthyexile.com/stash",
                "https://www.wealthyexile.com/maps",
                false,
                None
            ),
            NavOutcome::Allow
        );
    }

    #[test]
    fn user_gesture_allows_cross_site() {
        assert_eq!(
            classify("https://a.test/", "https://b.test/", true, None),
            NavOutcome::Allow
        );
    }

    #[test]
    fn ungestured_cross_site_prompts() {
        assert_eq!(
            classify("https://a.test/", "https://b.test/", false, None),
            NavOutcome::Prompt(NavReason::CrossSite)
        );
    }

    #[test]
    fn denylist_wins_over_gesture_and_remembered_allow() {
        assert_eq!(
            classify(
                "https://a.test/",
                "https://giftdash.rest/bonus",
                true,
                Some(Remembered::Allow)
            ),
            NavOutcome::Prompt(NavReason::Denylist)
        );
        assert_eq!(
            classify(
                "https://a.test/",
                "https://webtransit.live/visit",
                false,
                None
            ),
            NavOutcome::Prompt(NavReason::Denylist)
        );
    }

    #[test]
    fn remembered_decisions_apply() {
        assert_eq!(
            classify(
                "https://a.test/",
                "https://b.test/",
                false,
                Some(Remembered::Block)
            ),
            NavOutcome::Block(NavReason::RememberedBlock)
        );
        assert_eq!(
            classify(
                "https://a.test/",
                "https://b.test/",
                false,
                Some(Remembered::Allow)
            ),
            NavOutcome::Allow
        );
    }

    #[test]
    fn registrable_domain_matches_etld_plus_one() {
        assert_eq!(registrable_domain("ads.example.co.uk"), "example.co.uk");
        assert_eq!(registrable_domain("sub.example.com"), "example.com");
        assert_eq!(registrable_domain("127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn oauth_allowlist_skips_prompt() {
        assert_eq!(
            classify(
                "https://a.test/",
                "https://accounts.google.com/o/oauth2/auth",
                false,
                None
            ),
            NavOutcome::Allow
        );
    }

    #[test]
    fn non_http_destination_blocks() {
        assert_eq!(
            classify("https://a.test/", "file:///C:/page.html", false, None),
            NavOutcome::Allow
        );
        assert_eq!(
            classify("https://a.test/", "javascript:alert(1)", false, None),
            NavOutcome::Block(NavReason::UnsupportedScheme)
        );
        assert_eq!(
            classify("https://a.test/", "not a url", false, None),
            NavOutcome::Block(NavReason::UnsupportedScheme)
        );
    }
}
