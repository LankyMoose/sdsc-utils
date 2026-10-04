//! Immersive games-list position bar: center-focused section labels.
//!
//! The active section stays vertically centered and a bit larger; neighbors
//! fall off in scale and opacity (same distance curves as the games strip).
//! Section changes ease over the strip scroll duration. After scroll stops,
//! the bar stays up for [`HIDE_AFTER_MS`] then fades out over [`FADE_MS`].

use iced::alignment::{Horizontal, Vertical};
use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::{Color, Element, Length, Pixels, Point, Rectangle, Renderer, Size, Theme};

use crate::ui::layout::ease_out_cubic;
use crate::ui::theme;

/// Column width for the centered label stack.
pub const WIDTH: f32 = 96.0;

/// Keep the bar fully visible this long after the last scroll / section change.
pub const HIDE_AFTER_MS: u64 = 2000;
/// Fade-out (and fade-in) duration for show/hide.
pub const FADE_MS: u64 = 200;

/// Selected label scale relative to the base text size.
pub const SELECTED_SCALE: f32 = 1.35;
/// Scale at `|d| = 1`.
pub const SCALE_AT_1: f32 = 0.88;
/// Scale at `|d| = 2`.
pub const SCALE_AT_2: f32 = 0.62;
const BASE_TEXT: f32 = 12.0;
/// Vertical stride between section centers (logical px at distance units).
const STRIDE: f32 = 26.0;
/// Draw labels whose |distance| is below this (opacity floor clips the rest).
const VISIBLE_DIST: f32 = 3.25;

const PLAYED_LABELS: &[&str] = &["today", "7 days", "1 month", "1 year", "older", "never"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionMode {
    LastPlayed,
    Alphabetical,
}

#[derive(Debug, Clone, Copy)]
pub struct BarSpec {
    pub labels: &'static [&'static str],
    /// Fractional section index: the label at this index is centered.
    pub visual: f32,
    /// Overall bar opacity (list fade × show/hide visibility).
    pub opacity: f32,
}

pub fn labels(mode: SectionMode) -> &'static [&'static str] {
    match mode {
        SectionMode::LastPlayed => PLAYED_LABELS,
        SectionMode::Alphabetical => ALPHA_LABELS,
    }
}

pub fn notch_count(mode: SectionMode) -> usize {
    labels(mode).len()
}

/// Section index for one title. Last-played buckets are mutually exclusive and
/// ordered newest-first, matching browse order.
pub fn section_index(
    mode: SectionMode,
    title: &str,
    played_at_ms: Option<u64>,
    now_ms: u64,
    utc_offset_secs: i32,
) -> usize {
    match mode {
        SectionMode::Alphabetical => alpha_index(title),
        SectionMode::LastPlayed => played_index(played_at_ms, now_ms, utc_offset_secs),
    }
}

/// Midpoint of section `section` on a unit track (kept for tests / callers).
pub fn band_center(section: usize, count: usize) -> f32 {
    if count == 0 {
        return 0.5;
    }
    let section = section.min(count - 1);
    (section as f32 + 0.5) / count as f32
}

/// Scale for a signed distance (in section units) from the visual center.
pub fn scale_at_distance(distance: f32) -> f32 {
    let d = distance.abs();
    if d <= 1.0 {
        SELECTED_SCALE + (SCALE_AT_1 - SELECTED_SCALE) * d
    } else if d <= 2.0 {
        SCALE_AT_1 + (SCALE_AT_2 - SCALE_AT_1) * (d - 1.0)
    } else {
        (SCALE_AT_2 - 0.08 * (d - 2.0)).max(0.42)
    }
}

/// Opacity for a signed distance from the visual center.
pub fn opacity_at_distance(distance: f32) -> f32 {
    let d = distance.abs();
    (1.0 - 0.35 * d).clamp(0.12, 1.0)
}

/// Visibility after `elapsed_ms` since the last scroll: full for
/// [`HIDE_AFTER_MS`], then ease-out fade over [`FADE_MS`].
pub fn visibility_after_idle(elapsed_ms: u64) -> f32 {
    if elapsed_ms <= HIDE_AFTER_MS {
        1.0
    } else if elapsed_ms >= HIDE_AFTER_MS + FADE_MS {
        0.0
    } else {
        let t = (elapsed_ms - HIDE_AFTER_MS) as f32 / FADE_MS as f32;
        1.0 - ease_out_cubic(t.clamp(0.0, 1.0))
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Seconds to add to UTC to get local time (`local = utc + offset`).
pub fn local_utc_offset_secs() -> i32 {
    #[cfg(windows)]
    {
        use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
        const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
        const TIME_ZONE_ID_INVALID: u32 = 0xFFFF_FFFF;
        unsafe {
            let mut info = TIME_ZONE_INFORMATION::default();
            let id = GetTimeZoneInformation(&mut info);
            if id == TIME_ZONE_ID_INVALID {
                return 0;
            }
            // Windows Bias: UTC = local + bias(minutes). DaylightBias is negative.
            let mut bias = info.Bias;
            if id == TIME_ZONE_ID_DAYLIGHT {
                bias += info.DaylightBias;
            } else {
                bias += info.StandardBias;
            }
            bias.saturating_mul(-60)
        }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

pub fn view<Message: 'static>(spec: BarSpec) -> Element<'static, Message> {
    iced::widget::canvas(PositionBar { spec })
        .width(Length::Fixed(WIDTH))
        .height(Length::Fill)
        .into()
}

struct PositionBar {
    spec: BarSpec,
}

impl<Message> canvas::Program<Message> for PositionBar {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let opacity = self.spec.opacity.clamp(0.0, 1.0);
        if opacity < 0.01 || bounds.height < 8.0 || self.spec.labels.is_empty() {
            return Vec::new();
        }
        let mut frame = Frame::new(renderer, bounds.size());
        let count = self.spec.labels.len();
        let center_y = bounds.height * 0.5;
        let track_x = 14.0;
        let visual = self.spec.visual.clamp(0.0, (count - 1) as f32);

        // Short guide through the visible falloff window.
        let guide_half = STRIDE * VISIBLE_DIST;
        let guide_top = (center_y - guide_half).max(4.0);
        let guide_bot = (center_y + guide_half).min(bounds.height - 4.0);
        let track = Path::line(
            Point::new(track_x, guide_top),
            Point::new(track_x, guide_bot),
        );
        frame.stroke(
            &track,
            Stroke::default()
                .with_width(1.5)
                .with_color(theme::alpha(theme::MUTED, 0.28 * opacity)),
        );

        // Fixed center pip (active section is always under this).
        let pip = Path::rounded_rectangle(
            Point::new(track_x - 4.0, center_y - 7.0),
            Size::new(8.0, 14.0),
            4.0.into(),
        );
        frame.fill(
            &pip,
            Color {
                a: opacity,
                ..theme::ACCENT
            },
        );

        for (i, label) in self.spec.labels.iter().enumerate() {
            let dist = i as f32 - visual;
            if dist.abs() > VISIBLE_DIST {
                continue;
            }
            let scale = scale_at_distance(dist);
            let local_op = opacity_at_distance(dist) * opacity;
            if local_op < 0.02 {
                continue;
            }
            let y = center_y + dist * STRIDE;
            if y < -8.0 || y > bounds.height + 8.0 {
                continue;
            }

            let active = dist.abs() < 0.35;
            let tick_w = if active { 7.0 } else { 5.0 };
            let tick = Path::line(
                Point::new(track_x - tick_w * 0.35, y),
                Point::new(track_x + tick_w * 0.65, y),
            );
            frame.stroke(
                &tick,
                Stroke::default()
                    .with_width(if active { 2.0 } else { 1.25 })
                    .with_color(if active {
                        theme::alpha(theme::INK, 0.85 * local_op)
                    } else {
                        theme::alpha(theme::MUTED, 0.5 * local_op)
                    }),
            );

            let mut text = canvas::Text::from(*label);
            text.position = Point::new(track_x + 12.0, y);
            text.color = if active {
                theme::alpha(theme::INK, local_op)
            } else {
                theme::alpha(theme::MUTED, 0.78 * local_op)
            };
            text.size = Pixels(BASE_TEXT * scale);
            text.align_x = Horizontal::Left.into();
            text.align_y = Vertical::Center;
            frame.fill_text(text);
        }

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        _state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if cursor.is_over(bounds) {
            mouse::Interaction::Idle
        } else {
            mouse::Interaction::None
        }
    }
}

fn alpha_index(title: &str) -> usize {
    let Some(ch) = title.trim().chars().next() else {
        return 0;
    };
    if ch.is_ascii_alphabetic() {
        1 + (ch.to_ascii_uppercase() as u8 - b'A') as usize
    } else {
        0
    }
}

fn played_index(played_at_ms: Option<u64>, now_ms: u64, utc_offset_secs: i32) -> usize {
    let Some(played_at_ms) = played_at_ms.filter(|ms| *ms > 0) else {
        return 5;
    };
    let age_days = local_epoch_day(now_ms, utc_offset_secs)
        .saturating_sub(local_epoch_day(played_at_ms, utc_offset_secs));
    match age_days {
        i64::MIN..=0 => 0,
        1..=7 => 1,
        8..=30 => 2,
        31..=365 => 3,
        _ => 4,
    }
}

fn local_epoch_day(unix_ms: u64, utc_offset_secs: i32) -> i64 {
    let local_secs = (unix_ms / 1000) as i64 + i64::from(utc_offset_secs);
    local_secs.div_euclid(86_400)
}

const ALPHA_LABELS: &[&str] = &[
    "#", "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R",
    "S", "T", "U", "V", "W", "X", "Y", "Z",
];

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_MS: u64 = 86_400 * 1000;

    #[test]
    fn alpha_hash_then_letters() {
        assert_eq!(alpha_index("2048"), 0);
        assert_eq!(alpha_index("..."), 0);
        assert_eq!(alpha_index(""), 0);
        assert_eq!(alpha_index("  alpha"), 1);
        assert_eq!(alpha_index("Zelda"), 26);
        assert_eq!(notch_count(SectionMode::Alphabetical), 27);
        assert_eq!(labels(SectionMode::Alphabetical)[0], "#");
    }

    #[test]
    fn played_buckets_are_exclusive_newest_first() {
        let now = 2_000 * DAY_MS;
        let idx = |played: Option<u64>| section_index(SectionMode::LastPlayed, "x", played, now, 0);
        assert_eq!(idx(Some(now)), 0);
        assert_eq!(idx(Some(now - 3 * DAY_MS)), 1);
        assert_eq!(idx(Some(now - 7 * DAY_MS)), 1);
        assert_eq!(idx(Some(now - 8 * DAY_MS)), 2);
        assert_eq!(idx(Some(now - 30 * DAY_MS)), 2);
        assert_eq!(idx(Some(now - 31 * DAY_MS)), 3);
        assert_eq!(idx(Some(now - 365 * DAY_MS)), 3);
        assert_eq!(idx(Some(now - 366 * DAY_MS)), 4);
        assert_eq!(idx(None), 5);
        assert_eq!(idx(Some(0)), 5);
        assert_eq!(labels(SectionMode::LastPlayed).len(), 6);
    }

    #[test]
    fn thumb_sits_midway_between_notches() {
        assert!((band_center(0, 6) - 0.5 / 6.0).abs() < 1e-4);
        assert!((band_center(5, 6) - 5.5 / 6.0).abs() < 1e-4);
        assert!((band_center(0, 1) - 0.5).abs() < 1e-4);
    }

    #[test]
    fn scale_falls_off_like_strip() {
        assert!((scale_at_distance(0.0) - SELECTED_SCALE).abs() < 0.001);
        assert!((scale_at_distance(1.0) - SCALE_AT_1).abs() < 0.001);
        assert!((scale_at_distance(2.0) - SCALE_AT_2).abs() < 0.001);
        assert!(scale_at_distance(0.5) > SCALE_AT_1 && scale_at_distance(0.5) < SELECTED_SCALE);
        assert!(opacity_at_distance(0.0) > opacity_at_distance(1.0));
        assert!(opacity_at_distance(1.0) > opacity_at_distance(2.0));
    }

    #[test]
    fn visibility_holds_then_fades() {
        assert!((visibility_after_idle(0) - 1.0).abs() < 1e-5);
        assert!((visibility_after_idle(HIDE_AFTER_MS) - 1.0).abs() < 1e-5);
        assert!((visibility_after_idle(HIDE_AFTER_MS + FADE_MS) - 0.0).abs() < 1e-5);
        let mid = visibility_after_idle(HIDE_AFTER_MS + FADE_MS / 2);
        assert!(mid > 0.05 && mid < 0.95, "mid={mid}");
    }
}
