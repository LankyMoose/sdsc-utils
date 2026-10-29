//! GitHub-release update check (check + notify only; no download or apply).
//!
//! The Settings → System → Version card and a once-per-version native OS
//! notification surface [`fetch_latest`]. Both blocking calls (network fetch,
//! notification wait) run on worker threads through `spawn_blocking` in
//! `app`, never on the UI thread. Anything the network returns is
//! untrusted: tags are whitelisted, URLs are allowlisted, and the payload
//! is size-capped before parsing.

use crate::platform::app_log;
use crate::platform::app_meta::{DISPLAY_NAME, PKG_NAME, PKG_VERSION};
use crate::{AppError, AppResult};
use std::time::Duration;

/// GitHub Releases API for the newest published release.
pub const RELEASES_API_URL: &str =
    "https://api.github.com/repos/LankyMoose/sdsc-utils/releases/latest";
/// Delay after tray boot before the automatic check (lets cold start settle,
/// without making fast Settings openers wait on "Never checked").
pub const UPDATE_BOOT_DELAY: Duration = Duration::from_secs(5);
/// Spinner redraw cadence while a check is in flight.
pub const UPDATE_SPIN_TICK: Duration = Duration::from_millis(50);
/// Network timeout for the release API call.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
/// Largest release payload we will parse (the real one is a few KB).
const MAX_BODY_LEN: usize = 1_048_576;
/// Longest tag we accept for display/comparison.
const MAX_TAG_LEN: usize = 32;
/// Longest release URL we accept.
const MAX_URL_LEN: usize = 256;

/// A newer release found on GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    /// Raw `tag_name` (e.g. `v1.7.3`), sanitized for display.
    pub tag: String,
    /// Parsed `(major, minor, patch)`.
    pub version: (u64, u64, u64),
    /// Release page URL (`html_url`), validated https github.com.
    pub url: String,
}

/// Settings → System → Version card state. Owned by `App`, snapshotted into
/// `ConfigureSettings` for the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    Idle,
    Checking,
    Available(ReleaseInfo),
    UpToDate,
    Failed(String),
}

/// Fire-and-forget: show the native OS notification for a new release.
/// Windows toasts cannot report clicks back to the process (`show` returns
/// `()` there), so the body directs to Settings → System → Version, which
/// is the reliable surface. Fail-soft: a backend failure just logs, the
/// Settings card still carries the update.
pub fn show_update_notification(tag: &str) {
    if let Err(err) = notify_rust::Notification::new()
        .summary(DISPLAY_NAME)
        .body(&format!(
            "{DISPLAY_NAME} {tag} is available. Open Settings, System, Version to view it."
        ))
        .action("default", "Open Settings")
        .show()
    {
        app_log::warn(format!("update notification failed: {err}"));
    } else {
        diag_update(format!("update notify: handed to OS for {tag}"));
    }
}

/// Strict `vMAJOR.MINOR.PATCH` parse (optional single leading `v`).
/// Anything else (missing parts, extras, non-digits) is `None`.
pub fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let tag = tag
        .strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag);
    let mut parts = tag.split('.');
    let mut numbers = [0u64; 3];
    for slot in &mut numbers {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some((numbers[0], numbers[1], numbers[2]))
}

/// Whitelist a release tag for display/comparison.
/// Only short `[0-9A-Za-z._-]` strings survive; everything else is untrusted.
pub fn sanitize_tag(tag: &str) -> Option<String> {
    let tag = tag.trim();
    if tag.is_empty() || tag.len() > MAX_TAG_LEN {
        return None;
    }
    if !tag
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return None;
    }
    Some(tag.to_string())
}

/// True only when `latest_tag` parses and is strictly newer than
/// `current_tag`. Malformed input is never newer (no downgrade/rollback).
pub fn is_newer(latest_tag: &str, current_tag: &str) -> bool {
    match (checked_version(latest_tag), checked_version(current_tag)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

fn checked_version(tag: &str) -> Option<(u64, u64, u64)> {
    parse_version(&sanitize_tag(tag)?)
}

/// Allowlist a release page URL: `https://github.com/…` only.
fn validate_release_url(url: &str) -> Option<String> {
    let url = url.trim();
    if url.len() > MAX_URL_LEN {
        return None;
    }
    let rest = url.strip_prefix("https://")?;
    if rest.split('/').next() != Some("github.com") {
        return None;
    }
    Some(url.to_string())
}

/// Blocking: fetch the latest release from GitHub. Run through
/// `spawn_blocking`, never on the UI thread.
pub fn fetch_latest() -> AppResult<ReleaseInfo> {
    // Debug-only: hold the Checking/spinner state briefly so the
    // in-flight UI is observable (real fetches finish in ~100ms).
    // Compiled out of release builds entirely.
    #[cfg(debug_assertions)]
    std::thread::sleep(Duration::from_secs(2));
    let started = std::time::Instant::now();
    diag_update(format!(
        "update check: requesting {}",
        url_host(RELEASES_API_URL)
    ));
    let request = ureq::get(RELEASES_API_URL)
        .header("User-Agent", format!("{PKG_NAME}/{PKG_VERSION}"))
        .header("Accept", "application/vnd.github+json")
        .config()
        .timeout_global(Some(FETCH_TIMEOUT))
        .build();
    let response = request
        .call()
        .map_err(|err| AppError::Platform(format!("update check failed: {err}")))?;
    let text = response
        .into_body()
        .read_to_string()
        .map_err(|err| AppError::Platform(format!("update check failed: {err}")))?;
    if text.len() > MAX_BODY_LEN {
        return Err(AppError::Platform(
            "update check failed: oversize response".into(),
        ));
    }
    let info = release_from_json(&text)?;
    diag_update(format!(
        "update check: got tag {} in {}ms",
        info.tag,
        started.elapsed().as_millis(),
    ));
    Ok(info)
}

/// Debug-only step logging for the update check (never in release builds;
/// failures still `warn` there). Mirrors `session::notify::diag_notify`.
pub(crate) fn diag_update(message: impl AsRef<str>) {
    #[cfg(debug_assertions)]
    app_log::info(message);
    #[cfg(not(debug_assertions))]
    let _ = message;
}

/// Host (and port) only, for log lines — never query strings.
fn url_host(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("unknown host")
}

fn release_from_json(body: &str) -> AppResult<ReleaseInfo> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|err| AppError::Platform(format!("update check failed: bad response ({err})")))?;
    let tag_raw = value.get("tag_name").and_then(|v| v.as_str()).unwrap_or("");
    let url_raw = value.get("html_url").and_then(|v| v.as_str()).unwrap_or("");
    let tag = sanitize_tag(tag_raw)
        .ok_or_else(|| AppError::Platform("update check failed: bad release tag".into()))?;
    let version = parse_version(&tag)
        .ok_or_else(|| AppError::Platform("update check failed: bad release tag".into()))?;
    let url = validate_release_url(url_raw)
        .ok_or_else(|| AppError::Platform("update check failed: bad release link".into()))?;
    Ok(ReleaseInfo { tag, version, url })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_accepts_tag_shapes() {
        assert_eq!(parse_version("v1.7.3"), Some((1, 7, 3)));
        assert_eq!(parse_version("1.7.3"), Some((1, 7, 3)));
        assert_eq!(parse_version("V10.0.0"), Some((10, 0, 0)));
        assert!(parse_version(PKG_VERSION).is_some());
    }

    #[test]
    fn parse_version_rejects_non_triples() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("v1.7"), None);
        assert_eq!(parse_version("1.7.3.4"), None);
        assert_eq!(parse_version("v1.7.x"), None);
        assert_eq!(parse_version("v1.7.+3"), None);
        assert_eq!(parse_version("v 1.7.3"), None);
        assert_eq!(parse_version("v1..3"), None);
    }

    #[test]
    fn sanitize_tag_whitelists_display_text() {
        assert_eq!(sanitize_tag("v1.7.3").as_deref(), Some("v1.7.3"));
        assert_eq!(sanitize_tag("  v1.7.3\n").as_deref(), Some("v1.7.3"));
        assert_eq!(sanitize_tag(""), None);
        assert_eq!(sanitize_tag("v1.7.3; rm -rf /"), None);
        assert_eq!(sanitize_tag("v1.7.3<script>"), None);
        assert_eq!(sanitize_tag("v1.7/3"), None);
        assert_eq!(sanitize_tag(&"v".repeat(64)), None);
    }

    #[test]
    fn is_newer_needs_strictly_newer_triple() {
        assert!(is_newer("v1.7.3", "1.7.2"));
        assert!(is_newer("v2.0.0", "1.9.9"));
        assert!(!is_newer("v1.7.2", "1.7.2"));
        assert!(!is_newer("v1.7.1", "1.7.2"));
        assert!(!is_newer("garbage", "1.7.2"));
        assert!(!is_newer("v1.7.3", "garbage"));
    }

    #[test]
    fn release_from_json_accepts_github_shape() {
        let body = r#"{"tag_name":"v1.7.3","html_url":"https://github.com/LankyMoose/sdsc-utils/releases/tag/v1.7.3"}"#;
        let info = release_from_json(body).unwrap();
        assert_eq!(info.tag, "v1.7.3");
        assert_eq!(info.version, (1, 7, 3));
        assert!(info.url.ends_with("/releases/tag/v1.7.3"));
    }

    #[test]
    fn release_from_json_rejects_untrusted_shapes() {
        // Missing tag.
        assert!(
            release_from_json(
                r#"{"html_url":"https://github.com/LankyMoose/sdsc-utils/releases/tag/v1.7.3"}"#
            )
            .is_err()
        );
        // Non-semver tag.
        assert!(
            release_from_json(r#"{"tag_name":"nightly","html_url":"https://github.com/LankyMoose/sdsc-utils/releases/tag/nightly"}"#)
                .is_err()
        );
        // Plain-http link.
        assert!(
            release_from_json(r#"{"tag_name":"v1.7.3","html_url":"http://github.com/LankyMoose/sdsc-utils/releases/tag/v1.7.3"}"#)
                .is_err()
        );
        // Off-host link.
        assert!(
            release_from_json(r#"{"tag_name":"v1.7.3","html_url":"https://example.com/evil.exe"}"#)
                .is_err()
        );
        // Not JSON at all.
        assert!(release_from_json("not json").is_err());
    }
}
