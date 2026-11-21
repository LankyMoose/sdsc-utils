//! Blocking web fetch + readability extraction for the browser reader path.
//!
//! Phase-1 backend behind [`crate::browser::BrowserSession`]: fetch a URL off
//! the UI thread (callers use `spawn_blocking`) and reduce the HTML to title
//! plus plain text. This is the reader-mode core borrowed from the retsurf
//! playbook — no engine, no JS, a fast path for guides and docs while the
//! Servo offscreen backend is still a spike. Anything it cannot render falls
//! back to the system browser via [`crate::games::launch::launch_target`].

use std::time::Duration;

/// Network timeout for a browser fetch (reader path must fail fast, never
/// stall Start behind a hanging origin).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
/// Largest response body accepted before giving up (spike guard against
/// multi-MB dumps landing in an iced text widget).
pub const MAX_BODY_LEN: usize = 1_000_000;
/// Largest extracted text kept for display.
pub const MAX_TEXT_LEN: usize = 20_000;

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
/// directly; anything else becomes a Google search. This is the single
/// funnel for address-bar commits (navigate + open-external).
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

/// Blocking: download `url` and extract title + readable text.
/// Runs on a worker thread through `spawn_blocking`, never on the UI thread.
pub fn fetch_readable(url: &str) -> Result<crate::browser::FetchedPage, String> {
    let normalized = normalize_url(url);
    if normalized.is_empty() {
        return Err("empty URL".to_string());
    }
    let response = ureq::get(&normalized)
        .header("User-Agent", "sdsc-utils")
        .header("Accept", "text/html,*/*")
        .config()
        .timeout_global(Some(FETCH_TIMEOUT))
        .build()
        .call()
        .map_err(|err| format!("fetch failed: {err}"))?;
    let text = response
        .into_body()
        .read_to_string()
        .map_err(|err| format!("fetch failed: {err}"))?;
    if text.len() > MAX_BODY_LEN {
        return Err("response too large".to_string());
    }
    Ok(crate::browser::FetchedPage {
        url: normalized,
        title: extract_title(&text),
        body: extract_readable_text(&text, MAX_TEXT_LEN),
    })
}

/// First `<title>` in the document, trimmed. `None` when absent or blank.
pub fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let start = lower.find("<title")?;
    let content_start = lower[start..].find('>')? + start + 1;
    let end = lower[content_start..].find("</title>")? + content_start;
    let title = html[content_start..end].trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

/// Reduce HTML to plain text: drop scripts/styles/comments/tags, decode
/// common entities, collapse whitespace, truncate to `max_chars`.
pub fn extract_readable_text(html: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Comment.
            if html[i..].starts_with("<!--") {
                if let Some(end) = html[i..].find("-->") {
                    i += end + 3;
                } else {
                    break;
                }
                continue;
            }
            // Tag name (lowercased) for skip/block decisions.
            let rest = &html[i + 1..];
            let name_end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '/' || c == '!'))
                .map(|e| i + 1 + e)
                .unwrap_or(html.len());
            let name = html[i + 1..name_end.min(html.len())].to_lowercase();
            let is_close = name.starts_with('/');
            let base = name.trim_start_matches('/').to_string();
            // Skip whole script/style elements including their content.
            if (base == "script" || base == "style") && !is_close {
                if let Some(after) = find_element_end(html, i, &base) {
                    i = after;
                } else {
                    break;
                }
                continue;
            }
            // Block boundaries become line breaks.
            if !is_close
                && matches!(
                    base.as_str(),
                    "p" | "div"
                        | "br"
                        | "li"
                        | "tr"
                        | "section"
                        | "article"
                        | "header"
                        | "footer"
                        | "blockquote"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                )
            {
                out.push('\n');
            }
            if let Some(end) = html[i..].find('>') {
                i += end + 1;
            } else {
                break;
            }
        } else {
            // Text run until the next tag.
            let end = html[i..].find('<').map(|e| i + e).unwrap_or(html.len());
            decode_entities_into(&html[i..end], &mut out);
            i = end;
        }
    }
    // Collapse whitespace per line, drop blanks.
    let mut collapsed = String::new();
    for line in out.split('\n') {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            continue;
        }
        if !collapsed.is_empty() {
            collapsed.push('\n');
        }
        collapsed.push_str(&line);
    }
    if collapsed.chars().count() > max_chars {
        let kept: String = collapsed.chars().take(max_chars).collect();
        format!("{kept}…")
    } else {
        collapsed
    }
}

/// Byte offset just past `</tag>` starting the search at `from`.
/// ASCII case-insensitive on the tag name; advances by whole chars so all
/// returned offsets are valid `str` boundaries even on non-ASCII pages
/// (`to_lowercase` would shift byte positions and cannot be re-sliced).
fn find_element_end(html: &str, from: usize, tag: &str) -> Option<usize> {
    let bytes = html.as_bytes();
    let mut i = from;
    while i + 2 < bytes.len() {
        if bytes[i] == b'<' && bytes[i + 1] == b'/' {
            let name_start = i + 2;
            let name_end = html[name_start..]
                .find(|c: char| !c.is_ascii_alphanumeric())
                .map(|e| name_start + e)
                .unwrap_or(html.len());
            if html[name_start..name_end.min(html.len())].eq_ignore_ascii_case(tag) {
                return html[i..].find('>').map(|e| i + e + 1);
            }
        }
        let ch_len = html[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        i += ch_len;
    }
    None
}

fn decode_entities_into(input: &str, out: &mut String) {
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let end = tail.find(';').map(|e| amp + e);
        let Some(semi) = end else {
            out.push_str(tail);
            return;
        };
        let entity = &rest[amp..=semi];
        if let Some(decoded) = decode_entity(entity) {
            out.push_str(&decoded);
        } else {
            out.push_str(entity);
        }
        rest = &rest[semi + 1..];
    }
    out.push_str(rest);
}

fn decode_entity(entity: &str) -> Option<String> {
    match entity {
        "&amp;" => Some("&".to_string()),
        "&lt;" => Some("<".to_string()),
        "&gt;" => Some(">".to_string()),
        "&quot;" => Some("\"".to_string()),
        "&#39;" | "&apos;" => Some("'".to_string()),
        "&nbsp;" => Some(" ".to_string()),
        _ => {
            let inner = entity.strip_prefix('&')?.strip_suffix(';')?;
            if let Some(hex) = inner
                .strip_prefix("#x")
                .or_else(|| inner.strip_prefix("#X"))
            {
                let code = u32::from_str_radix(hex, 16).ok()?;
                Some(char::from_u32(code)?.to_string())
            } else if let Some(dec) = inner.strip_prefix('#') {
                let code = dec.parse::<u32>().ok()?;
                Some(char::from_u32(code)?.to_string())
            } else {
                None
            }
        }
    }
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
        // Full URLs and bare hosts navigate directly.
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
        // Plain words and phrases search.
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

    #[test]
    fn title_extraction_trims_and_misses_cleanly() {
        assert_eq!(
            extract_title("<html><head><TITLE> Hi </TITLE></head></html>"),
            Some("Hi".to_string())
        );
        assert_eq!(extract_title("<p>no title</p>"), None);
        assert_eq!(extract_title("<title>   </title>"), None);
    }

    #[test]
    fn text_drops_scripts_styles_and_tags() {
        let html = "<html><head><style>.x{color:red}</style><script>alert(1)</script></head>\
            <body><h1>Guide</h1><p>Step <b>one</b> &amp; two</p></body></html>";
        assert_eq!(extract_readable_text(html, 1000), "Guide\nStep one & two");
    }

    #[test]
    fn text_collapses_whitespace_and_truncates() {
        let html = "<p>  a   b\n\nc  </p>";
        // Raw source newlines survive as line breaks; runs collapse within.
        assert_eq!(extract_readable_text(html, 1000), "a b\nc");
        let long = format!("<p>{}</p>", "x".repeat(50));
        let out = extract_readable_text(&long, 10);
        assert_eq!(out.chars().count(), 11);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn entities_decode_common_and_numeric() {
        assert_eq!(
            extract_readable_text("<p>&lt;a&gt; &#65; &#x42;</p>", 100),
            "<a> A B"
        );
        assert_eq!(extract_readable_text("<p>&unknown;</p>", 100), "&unknown;");
    }

    #[test]
    fn skips_script_around_non_ascii_without_panic() {
        let html = "<p>🎮 pad</p><SCRIPT>var x = 1;</SCRIPT><p>done ✓</p>";
        assert_eq!(extract_readable_text(html, 100), "🎮 pad\ndone ✓");
    }
}
