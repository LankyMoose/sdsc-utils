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
/// Delay after tray boot before the automatic check (lets cold start settle).
pub const UPDATE_BOOT_DELAY: Duration = Duration::from_secs(10);
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

/// Debug-only mock of the GitHub `releases/latest` endpoint (std only, no
/// dependencies). Serves `sdsc-utils --mock-releases` and the `fetch_from_url`
/// tests below: every response sleeps first so the checking spinner and the
/// once-per-version notify path are exercisable locally.
///
/// Query knobs on `GET /releases/latest` (all optional):
/// - `tag`: reported `tag_name` (default `v9.9.9`, always newer in dev);
/// - `status`: HTTP status (default 200; e.g. 500 exercises the Failed card);
/// - `delay_ms`: artificial delay before responding (default 300).
#[cfg(debug_assertions)]
pub mod mock {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    /// Default `--mock-releases` port (loopback only).
    pub const DEFAULT_PORT: u16 = 8751;
    /// Artificial delay before every mock response.
    pub const DEFAULT_DELAY_MS: u64 = 300;
    /// Default reported tag (newer than any shipped version).
    const DEFAULT_TAG: &str = "v9.9.9";
    /// Largest request line we parse (dev tool, but stay bounded).
    const MAX_REQUEST_LINE: usize = 8192;

    struct Query {
        tag: String,
        status: u16,
        delay_ms: u64,
    }

    /// Blocking: serve mock releases on 127.0.0.1 until the process exits.
    /// Entry point for `sdsc-utils --mock-releases [port]`.
    pub fn serve(port: u16) -> std::io::Result<()> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        println!(
            "mock releases at http://127.0.0.1:{port}/releases/latest[?tag=v9.9.9&status=200&delay_ms=300]"
        );
        println!("run the app with SDSC_UPDATE_URL set to that address");
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => handle_connection(stream),
                Err(err) => eprintln!("mock releases: connection failed: {err}"),
            }
        }
        Ok(())
    }

    /// Test helper: serve exactly `requests` connections on an ephemeral
    /// loopback port, then exit. Returns the port and join handle (join it so
    /// no server thread outlives the test).
    pub fn spawn_test_server(requests: usize) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock server");
        let port = listener.local_addr().expect("mock addr").port();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming().take(requests).flatten() {
                handle_connection(stream);
            }
        });
        (port, handle)
    }

    fn handle_connection(mut stream: TcpStream) {
        let request_line = {
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            // Request line plus header drain; anything unreadable ends it.
            if reader.read_line(&mut line).is_err() {
                return;
            }
            loop {
                let mut header = String::new();
                match reader.read_line(&mut header) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if header.trim().is_empty() => break,
                    Ok(_) => {}
                }
            }
            line
        };
        if request_line.len() > MAX_REQUEST_LINE {
            return;
        }
        let query = parse_query(&request_line);
        std::thread::sleep(Duration::from_millis(query.delay_ms));
        let (status, body) = mock_response(&query, &request_line);
        let response = format!(
            "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            reason_phrase(status),
            body.len(),
        );
        let _ = stream.write_all(response.as_bytes());
    }

    fn parse_query(request_line: &str) -> Query {
        let mut query = Query {
            tag: DEFAULT_TAG.to_string(),
            status: 200,
            delay_ms: DEFAULT_DELAY_MS,
        };
        let path = request_line.split_whitespace().nth(1).unwrap_or("");
        let params = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        for pair in params.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match key {
                "tag" if !value.is_empty() => query.tag = value.to_string(),
                "status" => {
                    if let Ok(status) = value.parse::<u16>() {
                        query.status = status;
                    }
                }
                "delay_ms" => {
                    if let Ok(delay) = value.parse::<u64>() {
                        query.delay_ms = delay;
                    }
                }
                _ => {}
            }
        }
        query
    }

    /// Only `GET /releases/latest` is served; anything else is a 404.
    /// `?status=` overrides the status for the endpoint itself.
    fn mock_response(query: &Query, request_line: &str) -> (u16, String) {
        let mut parts = request_line.split_whitespace();
        let is_get = parts.next() == Some("GET");
        let is_endpoint = parts
            .next()
            .is_some_and(|path| path.split('?').next() == Some("/releases/latest"));
        if !is_get || !is_endpoint {
            return (404, r#"{"message":"mock: unknown endpoint"}"#.to_string());
        }
        if query.status != 200 {
            return (query.status, r#"{"message":"mock error"}"#.to_string());
        }
        let body = format!(
            "{{\"tag_name\":\"{}\",\"html_url\":\"https://github.com/LankyMoose/sdsc-utils/releases/tag/{}\"}}",
            query.tag, query.tag,
        );
        (200, body)
    }

    fn reason_phrase(status: u16) -> &'static str {
        match status {
            200 => "OK",
            403 => "Forbidden",
            404 => "Not Found",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            _ => "Error",
        }
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
///
/// Debug builds honor `SDSC_UPDATE_URL` (a full `releases/latest` URL, served
/// by `sdsc-utils --mock-releases`) so the check is testable without hitting
/// GitHub. Release builds always use [`RELEASES_API_URL`].
pub fn fetch_latest() -> AppResult<ReleaseInfo> {
    fetch_from_url(&releases_url())
}

fn releases_url() -> String {
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("SDSC_UPDATE_URL")
        && !url.trim().is_empty()
    {
        return url;
    }
    RELEASES_API_URL.to_string()
}

fn fetch_from_url(url: &str) -> AppResult<ReleaseInfo> {
    let started = std::time::Instant::now();
    diag_update(format!("update check: requesting {}", url_host(url)));
    let request = ureq::get(url)
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

    /// Each mock test binds its own ephemeral loopback port (no shared
    /// state), so parallel test threads cannot interfere.
    #[cfg(debug_assertions)]
    #[test]
    fn fetch_from_url_serves_mock_release() {
        let (port, handle) = mock::spawn_test_server(1);
        let info = fetch_from_url(&format!("http://127.0.0.1:{port}/releases/latest")).unwrap();
        handle.join().expect("mock server");
        assert_eq!(info.tag, "v9.9.9");
        assert_eq!(info.version, (9, 9, 9));
        assert!(is_newer(&info.tag, PKG_VERSION));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn fetch_from_url_reflects_tag_and_status() {
        let (port, handle) = mock::spawn_test_server(2);
        let base = format!("http://127.0.0.1:{port}/releases/latest");
        let info = fetch_from_url(&format!("{base}?tag=v1.0.0")).unwrap();
        assert_eq!(info.tag, "v1.0.0");
        assert!(!is_newer(&info.tag, PKG_VERSION));
        assert!(fetch_from_url(&format!("{base}?status=500")).is_err());
        handle.join().expect("mock server");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn fetch_from_url_rejects_unknown_endpoint() {
        let (port, handle) = mock::spawn_test_server(1);
        assert!(fetch_from_url(&format!("http://127.0.0.1:{port}/nope")).is_err());
        handle.join().expect("mock server");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn mock_response_carries_the_artificial_delay() {
        // Lower bound only: `sleep` never returns early, scheduling only adds.
        let started = std::time::Instant::now();
        let (port, handle) = mock::spawn_test_server(1);
        let _ = fetch_from_url(&format!(
            "http://127.0.0.1:{port}/releases/latest?delay_ms=300"
        ));
        handle.join().expect("mock server");
        assert!(started.elapsed() >= Duration::from_millis(200));
    }
}
