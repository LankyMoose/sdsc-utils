//! Address-bar navigation helpers: scheme normalization and search routing.
//!
//! Extracted from the deleted reader/fetch path so the Browser app keeps one
//! funnel for URL commits and open-external.

/// Add a scheme when the user typed a bare host (`example.com` →
/// `https://example.com`). Anything already carrying `://` passes through.
pub fn normalize_url(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    }
}

/// Omnibox resolution: full URLs (and bare hosts, ports, localhost) navigate
/// directly; anything else becomes a Google search. Single funnel for
/// address-bar commits (navigate + open-external).
pub fn resolve_navigate_target(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.contains("://") {
        return trimmed.to_string();
    }
    let host_like = !trimmed.contains(char::is_whitespace)
        && (trimmed.contains('.')
            || trimmed.contains(':')
            || trimmed.eq_ignore_ascii_case("localhost"));
    if host_like {
        return format!("https://{trimmed}");
    }
    let query: String = url::form_urlencoded::byte_serialize(trimmed.as_bytes()).collect();
    format!("https://www.google.com/search?q={query}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_adds_https_to_bare_host() {
        assert_eq!(normalize_url("example.com"), "https://example.com");
        assert_eq!(
            normalize_url("  example.com/a b "),
            "https://example.com/a b"
        );
        assert_eq!(normalize_url("http://x.test/"), "http://x.test/");
        assert_eq!(normalize_url(""), "");
    }

    #[test]
    fn resolve_routes_hosts_and_searches() {
        assert_eq!(resolve_navigate_target("http://x.test/"), "http://x.test/");
        assert_eq!(
            resolve_navigate_target("example.com"),
            "https://example.com"
        );
        assert_eq!(
            resolve_navigate_target("localhost:8080/a"),
            "https://localhost:8080/a"
        );
        assert_eq!(resolve_navigate_target("localhost"), "https://localhost");
        assert_eq!(
            resolve_navigate_target("meerkats"),
            "https://www.google.com/search?q=meerkats"
        );
        assert_eq!(
            resolve_navigate_target("meerkat facts"),
            "https://www.google.com/search?q=meerkat+facts"
        );
        assert_eq!(resolve_navigate_target(""), "");
    }
}
