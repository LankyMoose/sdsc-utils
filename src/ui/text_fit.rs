//! Pixel-aware single-line truncation (End ellipsis) for browser chrome.
//!
//! Renders nothing itself: it measures with cosmic-text 0.18 (system sans,
//! matching the stock iced text stack) and returns a pre-truncated string
//! the normal iced `text` widget draws. All rendering stays on iced's own
//! pipeline — no second font/raster/DPI path to maintain.
//!
//! The helper mirrors iced's defaults (sans family, Normal weight, Auto
//! shaping: Basic for pure-ASCII, Advanced otherwise) with a small safety
//! margin absorbing shaper-version drift between the measuring 0.18 stack
//! and iced 0.14's rendering 0.15 stack. Truncation splits at char
//! boundaries by construction, so multi-byte glyphs (emoji, CJK) never tear.

use std::sync::{Mutex, OnceLock};

/// Pixels of slack kept past the measured width: absorbs sub-pixel drift
/// between the measuring stack and iced's renderer (same families, adjacent
/// shaper versions).
const SAFETY_PX: f32 = 2.0;

fn font_system() -> std::sync::MutexGuard<'static, cosmic_text::FontSystem> {
    static FONT_SYSTEM: OnceLock<Mutex<cosmic_text::FontSystem>> = OnceLock::new();
    FONT_SYSTEM
        .get_or_init(|| Mutex::new(cosmic_text::FontSystem::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Single-line laid-out width of `s` at `font_px` (system sans). Empty
/// strings measure zero; wrapping is off so multi-word strings report their
/// full run width.
pub fn text_width_px(s: &str, font_px: f32) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    let mut fs = font_system();
    let metrics = cosmic_text::Metrics::new(font_px, font_px);
    let mut buffer = cosmic_text::Buffer::new(&mut fs, metrics);
    // Mirror iced's Auto shaping: cheap Basic for pure-ASCII, Advanced
    // (font fallback, complex scripts) otherwise.
    let shaping = if s.is_ascii() {
        cosmic_text::Shaping::Basic
    } else {
        cosmic_text::Shaping::Advanced
    };
    let attrs = cosmic_text::Attrs::new().family(cosmic_text::Family::SansSerif);
    buffer.set_wrap(&mut fs, cosmic_text::Wrap::None);
    buffer.set_text(&mut fs, s, &attrs, shaping, None);
    buffer.shape_until_scroll(&mut fs, false);
    buffer
        .layout_runs()
        .map(|run| run.line_w)
        .fold(0.0f32, f32::max)
}

/// Truncate `s` (single line) to fit `max_width_px` at `font_px`, appending
/// "…" when truncated. Strings that already fit pass through untouched;
/// `max_width_px <= 0` or empty input yields "". Normal weight, system sans.
pub fn ellipsize_to_width(s: impl AsRef<str>, font_px: f32, max_width_px: f32) -> String {
    let s = s.as_ref();
    if max_width_px <= 0.0 || s.is_empty() {
        return String::new();
    }
    let budget = (max_width_px - SAFETY_PX).max(0.0);
    if text_width_px(s, font_px) <= budget {
        return s.to_string();
    }
    const ELLIPSIS: &str = "…";
    if text_width_px(ELLIPSIS, font_px) > budget {
        return String::new();
    }
    // Binary search over char count: longest prefix + "…" that fits.
    let chars: Vec<char> = s.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect::<String>() + ELLIPSIS;
        if text_width_px(&candidate, font_px) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>() + ELLIPSIS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_nonpositive_width_yield_empty() {
        assert_eq!(ellipsize_to_width("", 13.0, 400.0), "");
        assert_eq!(ellipsize_to_width("hello", 13.0, 0.0), "");
        assert_eq!(ellipsize_to_width("hello", 13.0, -10.0), "");
    }

    #[test]
    fn fitting_text_passes_through_untouched() {
        assert_eq!(ellipsize_to_width("short", 13.0, 400.0), "short");
    }

    #[test]
    fn truncation_fits_budget_and_keeps_prefix() {
        let long = "https://www.example.com/some/very/long/page/title/that/overflows";
        let out = ellipsize_to_width(long, 13.0, 120.0);
        assert!(out.ends_with('…'), "got {out:?}");
        assert!(out.len() < long.len());
        assert!(text_width_px(&out, 13.0) <= 120.0);
        let prefix = out.trim_end_matches('…');
        assert!(long.starts_with(prefix));
    }

    #[test]
    fn emoji_never_tears_and_stays_valid() {
        let s = "a🎮bcd efgh 🎮🎮🎮 tail";
        let out = ellipsize_to_width(s, 13.0, 60.0);
        assert!(out.is_char_boundary(out.len()));
        assert!(text_width_px(&out, 13.0) <= 60.0);
    }

    #[test]
    fn width_is_monotone_in_prefix_length() {
        let s = "measure me please";
        let mut prev = 0.0;
        for i in 1..=s.len() {
            if !s.is_char_boundary(i) {
                continue;
            }
            let w = text_width_px(&s[..i], 13.0);
            assert!(w >= prev, "width shrank at {i}");
            prev = w;
        }
    }
}
