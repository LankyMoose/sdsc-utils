//! Favicon fetch + decode.
//!
//! CEF hands us the page's advertised icon URLs; bookmarks ask for the
//! site's `/favicon.ico`. Either way the bytes are fetched off the UI thread
//! and decoded to straight-alpha RGBA for an iced image handle.
//!
//! Supported payloads: PNG/JPEG/WebP/GIF/ICO (via `image`) and SVG (via
//! `resvg`). `data:` URLs are decoded inline; HTTP(S) fetches are capped and
//! time-limited so a dead site cannot wedge the browser backend.

use std::io::Read;
use std::time::Duration;

/// Hard cap on a downloaded icon; favicons are tiny but hostile URLs exist.
const MAX_ICON_BYTES: u64 = 2 << 20;
/// Longest edge of the decoded icon. Rendered at 16px; 64 keeps high-DPI
/// displays crisp without feeding the atlas a full-resolution photo.
const MAX_ICON_PX: u32 = 64;
const FETCH_TIMEOUT: Duration = Duration::from_secs(4);

/// Fetch and decode the first working icon. `icon_urls` are CEF's advertised
/// candidates; when none work (or the list is empty) the site's conventional
/// `/favicon.ico` is tried last.
pub fn fetch_best(page_url: &str, icon_urls: &[String]) -> Option<(u32, u32, Vec<u8>)> {
    for url in icon_urls {
        if let Some(icon) = fetch(url) {
            return Some(icon);
        }
    }
    let origin = origin_url(page_url)?;
    fetch(&format!("{origin}/favicon.ico"))
}

/// Fetch and decode one icon URL (`data:` or HTTP(S)).
pub fn fetch(url: &str) -> Option<(u32, u32, Vec<u8>)> {
    let bytes = if let Some(rest) = url.strip_prefix("data:") {
        decode_data_url(rest)?
    } else {
        http_get(url)?
    };
    decode(&bytes)
}

/// Decode one icon payload to `(width, height, straight RGBA)`.
pub fn decode(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if looks_like_svg(bytes) {
        return decode_svg(bytes);
    }
    let image = image::load_from_memory(bytes).ok()?;
    let image = image.thumbnail(MAX_ICON_PX, MAX_ICON_PX);
    let rgba = image.to_rgba8();
    Some((rgba.width(), rgba.height(), rgba.into_raw()))
}

/// Site origin (`scheme://host[:port]`) for the conventional favicon path.
fn origin_url(page_url: &str) -> Option<String> {
    let url = url::Url::parse(page_url).ok()?;
    let origin = url.origin().ascii_serialization();
    if origin == "null" {
        return None;
    }
    Some(origin)
}

fn http_get(url: &str) -> Option<Vec<u8>> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let response = ureq::get(url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36",
        )
        .config()
        .timeout_global(Some(FETCH_TIMEOUT))
        .build()
        .call()
        .ok()?;
    let mut bytes = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(MAX_ICON_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    (!bytes.is_empty()).then_some(bytes)
}

fn decode_data_url(rest: &str) -> Option<Vec<u8>> {
    let (meta, payload) = rest.split_once(',')?;
    if meta.ends_with(";base64") {
        base64_decode(payload)
    } else {
        percent_decode(payload)
    }
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    std::str::from_utf8(head).is_ok() && head.windows(4).any(|w| w.eq_ignore_ascii_case(b"<svg"))
}

fn decode_svg(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let tree = resvg::usvg::Tree::from_data(bytes, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let longest = size.width().max(size.height()).max(1.0);
    let scale = MAX_ICON_PX as f32 / longest;
    let width = (size.width() * scale).round().max(1.0) as u32;
    let height = (size.height() * scale).round().max(1.0) as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let mut rgba = pixmap.take();
    unpremultiply(&mut rgba);
    Some((width, height, rgba))
}

/// `tiny-skia` pixmaps are premultiplied; iced image handles are not.
fn unpremultiply(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        let alpha = px[3] as u32;
        if alpha == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        } else if alpha < 255 {
            px[0] = (px[0] as u32 * 255 / alpha).min(255) as u8;
            px[1] = (px[1] as u32 * 255 / alpha).min(255) as u8;
            px[2] = (px[2] as u32 * 255 / alpha).min(255) as u8;
        }
    }
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u8;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\r' | b'\n' | b' ' | b'\t' => continue,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn percent_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex(*bytes.get(i + 1)?)?;
            let lo = hex(*bytes.get(i + 2)?)?;
            out.push(hi * 16 + lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1x1 transparent PNG.
    const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    #[test]
    fn decodes_base64_data_url_png() {
        let bytes = decode_data_url(&format!("image/png;base64,{PNG_B64}")).unwrap();
        assert!(bytes.starts_with(b"\x89PNG"));
        let (w, h, rgba) = decode(&bytes).unwrap();
        // Tiny ICO/PNG sources are upscaled to the 64px cap (crisp on
        // high-DPI displays); larger sources are downscaled.
        assert_eq!((w, h), (64, 64));
        assert_eq!(rgba.len(), 64 * 64 * 4);
    }

    #[test]
    fn decodes_percent_encoded_data_url() {
        let bytes = decode_data_url("image/svg+xml,%3Csvg%3E%3C/svg%3E").unwrap();
        assert_eq!(bytes, b"<svg></svg>");
    }

    #[test]
    fn renders_svg_icons() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><rect width="16" height="16" fill="#336699"/></svg>"##;
        let (w, h, rgba) = decode(svg).unwrap();
        assert_eq!((w, h), (64, 64));
        assert_eq!(rgba.len(), 64 * 64 * 4);
        assert_eq!(&rgba[0..3], &[0x33, 0x66, 0x99]);
    }

    #[test]
    fn base64_ignores_whitespace_and_padding() {
        assert_eq!(base64_decode("aGVs\nbG8=").unwrap(), b"hello");
    }

    #[test]
    fn origin_excludes_path_and_fragment() {
        assert_eq!(
            origin_url("https://example.com/some/page?q=1#frag").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(origin_url("file:///tmp/page.html"), None);
    }
}
