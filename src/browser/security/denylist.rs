//! Emergency top-level navigation denylist.
//!
//! Deliberately tiny and independent of the filter-list snapshot: these hosts
//! are known same-tab ad-redirect destinations and must never load without an
//! explicit user Continue.

/// Registrable domains denied for main-frame navigation.
pub const DENIED_DOMAINS: &[&str] = &[
    "webtransit.live",
    "giftdash.rest",
    "popads.net",
    "propellerads.com",
    "onclickads.net",
    "adsterra.com",
];

/// Host/domain membership: an entry matches itself and any subdomain.
pub fn is_denied(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    DENIED_DOMAINS
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incident_hosts_and_subdomains_match() {
        assert!(is_denied("webtransit.live"));
        assert!(is_denied("ads.webtransit.live"));
        assert!(is_denied("giftdash.rest"));
        assert!(is_denied("www.giftdash.rest."));
        assert!(!is_denied("notwebtransit.live"));
        assert!(!is_denied("example.com"));
    }
}
