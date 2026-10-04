//! Immersive games-list position indicator.
//!
//! Section names sit on the column's vertical centerline. A vertical line is
//! drawn in the gap between one section and the next, on that same axis.
//! Nothing is drawn through a label, and there is no thumb. The active section
//! stays vertically centered. Neighbors fall off in scale and opacity, and the
//! column translates vertically over the strip-scroll duration (no X offset).
//! Show/hide slides in from the left and out to the left ([`HIDE_AFTER_MS`]
//! idle, then [`FADE_MS`]). Opacity uses the shorter [`OPACITY_MS`], so it
//! leads the slide and finishes first.

use iced::alignment::{Horizontal, Vertical};
use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::{Element, Font, Length, Pixels, Point, Rectangle, Renderer, Theme, Vector};

use crate::ui::layout::ease_out_cubic;
use crate::ui::theme;

/// Column width for the centered label stack (room for mono uppercase labels).
pub const WIDTH: f32 = 120.0;

/// Keep the bar fully visible this long after the last scroll / section change.
pub const HIDE_AFTER_MS: u64 = 2000;
/// Slide in/out duration for show/hide (translation).
pub const FADE_MS: u64 = 200;
/// Show/hide opacity. Shorter than [`FADE_MS`] so it leads and finishes first.
pub const OPACITY_MS: u64 = 120;

/// Selected label scale relative to the base text size.
pub const SELECTED_SCALE: f32 = 1.35;
/// Scale at `|d| = 1`.
pub const SCALE_AT_1: f32 = 0.88;
/// Scale at `|d| = 2`.
pub const SCALE_AT_2: f32 = 0.62;
/// Base text size (logical px) before distance scale.
const BASE_TEXT: f32 = 16.0;
/// Gap between adjacent label boxes in the scale-weighted stride.
const LABEL_GAP: f32 = 8.0;
/// Draw labels whose |distance| is below this (opacity floor clips the rest).
const VISIBLE_DIST: f32 = 3.25;
/// Fraction of the canvas height the visible notch column should span.
const BAR_HEIGHT_FRAC: f32 = 2.0 / 3.0;
/// Max width for label glyphs (clip / wrap within the column).
const LABEL_MAX_WIDTH: f32 = 96.0;
/// Space kept between a gap line and the glyphs on either side.
const LABEL_LINE_PAD: f32 = 6.0;

const PLAYED_LABELS: &[&str] = &["TODAY", "7 DAYS", "1 MONTH", "1 YEAR", "OLDER", "NEVER"];

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
    /// Overall bar opacity (list reveal only; show/hide does not fade).
    pub opacity: f32,
    /// 0 = slid off the left edge, 1 = resting. Bar and labels share this.
    pub slide: f32,
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

/// Center-to-center step between labels at integer distances `i` and `i + 1`.
///
/// Sized from the two label scales so motion packs like the games strip (sans X).
pub fn unit_stride(from_distance: f32, base_h: f32) -> f32 {
    let i = from_distance.floor().max(0.0);
    let h0 = base_h * scale_at_distance(i);
    let h1 = base_h * scale_at_distance(i + 1.0);
    0.5 * (h0 + h1) + LABEL_GAP
}

/// Signed Y offset of a label center from the bar midline for a fractional distance.
///
/// Always relative to screen vertical center so the active section stays centered,
/// including at the first and last notch.
pub fn label_y_offset(distance: f32, base_h: f32) -> f32 {
    let sign = if distance >= 0.0 { 1.0 } else { -1.0 };
    let ad = distance.abs();
    let mut offset = 0.0;
    let mut d = 0.0;
    while d < ad - 1e-4 {
        let step_end = (d.floor() + 1.0).min(ad);
        let span = step_end - d;
        offset += unit_stride(d, base_h) * span;
        d = step_end;
    }
    sign * offset
}

/// Base label height so the visible falloff window spans about 2/3 of bar height.
pub fn base_h_for_bar(bar_h: f32) -> f32 {
    let target_half = (bar_h * BAR_HEIGHT_FRAC * 0.5).max(48.0);
    // unit_stride = scale_term * base_h + LABEL_GAP, so extent = a * base_h + g.
    let e1 = label_y_offset(VISIBLE_DIST, 1.0).abs();
    let e2 = label_y_offset(VISIBLE_DIST, 2.0).abs();
    let a = (e2 - e1).max(1e-4);
    let g = e1 - a;
    ((target_half - g) / a).max(4.0)
}

/// Label slots within the visible falloff: `(index, distance, y)`.
pub fn label_positions(
    count: usize,
    visual: f32,
    center_y: f32,
    base_h: f32,
) -> Vec<(usize, f32, f32)> {
    if count == 0 {
        return Vec::new();
    }
    let visual = visual.clamp(0.0, (count - 1) as f32);
    let mut out = Vec::with_capacity(count.min(8));
    for i in 0..count {
        let dist = i as f32 - visual;
        if dist.abs() > VISIBLE_DIST {
            continue;
        }
        let y = center_y + label_y_offset(dist, base_h);
        out.push((i, dist, y));
    }
    out
}

/// Horizontal center of the label column. Gap lines and section text share it.
pub fn column_axis(width: f32) -> f32 {
    width * 0.5
}

/// How far a separator stays from a section label's center so it never crosses the glyphs.
fn label_clearance(distance: f32) -> f32 {
    0.45 * BASE_TEXT * scale_at_distance(distance) + LABEL_LINE_PAD
}

/// Vertical span `(y0, y1)` of the line between section `index` and the next,
/// relative to the column midline. `None` when there is no following section,
/// or the labels leave no gap.
pub fn gap_line(index: usize, count: usize, visual: f32, base_h: f32) -> Option<(f32, f32)> {
    if count < 2 || index + 1 >= count {
        return None;
    }
    let visual = visual.clamp(0.0, (count - 1) as f32);
    let d0 = index as f32 - visual;
    let d1 = d0 + 1.0;
    let y0 = label_y_offset(d0, base_h) + label_clearance(d0);
    let y1 = label_y_offset(d1, base_h) - label_clearance(d1);
    if y1 - y0 < 2.0 { None } else { Some((y0, y1)) }
}

/// Eased show/hide amount over `duration_ms` (slide uses [`FADE_MS`], opacity [`OPACITY_MS`]).
pub fn show_hide_amount(from: f32, to: f32, elapsed_ms: u64, duration_ms: u64) -> f32 {
    let t = (elapsed_ms as f32 / duration_ms.max(1) as f32).clamp(0.0, 1.0);
    let eased = ease_out_cubic(t);
    (from + (to - from) * eased).clamp(0.0, 1.0)
}

pub fn visibility_after_idle(elapsed_ms: u64) -> f32 {
    if elapsed_ms <= HIDE_AFTER_MS {
        1.0
    } else {
        show_hide_amount(1.0, 0.0, elapsed_ms - HIDE_AFTER_MS, OPACITY_MS)
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
        let slide = self.spec.slide.clamp(0.0, 1.0);
        if opacity < 0.01 || slide < 0.01 || bounds.height < 8.0 || self.spec.labels.is_empty() {
            return Vec::new();
        }
        let mut frame = Frame::new(renderer, bounds.size());
        // Clip is the canvas bounds. Shift the whole column (gap lines and labels)
        // so show/hide travels horizontally off the left edge.
        frame.translate(Vector::new((slide - 1.0) * bounds.width, 0.0));

        let count = self.spec.labels.len();
        let center_y = bounds.height * 0.5;
        let axis_x = column_axis(bounds.width);
        let visual = self.spec.visual.clamp(0.0, (count - 1) as f32);
        let base_h = base_h_for_bar(bounds.height);
        let placed = label_positions(count, visual, center_y, base_h);

        // One vertical line in the gap between each pair of sections, on the
        // same axis as the labels. No line through a label, and no thumb.
        for i in 0..count.saturating_sub(1) {
            let Some((rel0, rel1)) = gap_line(i, count, visual, base_h) else {
                continue;
            };
            let y0 = center_y + rel0;
            let y1 = center_y + rel1;
            if y1 < -8.0 || y0 > bounds.height + 8.0 {
                continue;
            }
            let mid_dist = (i as f32 + 0.5) - visual;
            let local_op = opacity_at_distance(mid_dist) * opacity;
            if local_op < 0.02 {
                continue;
            }
            let line = Path::line(Point::new(axis_x, y0), Point::new(axis_x, y1));
            frame.stroke(
                &line,
                Stroke::default()
                    .with_width(1.5)
                    .with_color(theme::alpha(theme::MUTED, 0.55 * local_op)),
            );
        }

        for (i, dist, y) in placed {
            if y < -12.0 || y > bounds.height + 12.0 {
                continue;
            }
            let label = self.spec.labels[i];
            let scale = scale_at_distance(dist);
            let local_op = opacity_at_distance(dist) * opacity;
            if local_op < 0.02 {
                continue;
            }
            let active = dist.abs() < 0.35;

            let mut text = canvas::Text::from(label);
            text.content = label.to_ascii_uppercase();
            // Same vertical axis as the gap lines, centered on the column.
            text.position = Point::new(axis_x, y);
            text.color = if active {
                theme::alpha(theme::INK, local_op)
            } else {
                theme::alpha(theme::MUTED, 0.78 * local_op)
            };
            text.size = Pixels(BASE_TEXT * scale);
            text.font = Font::MONOSPACE;
            text.max_width = LABEL_MAX_WIDTH;
            text.align_x = Horizontal::Center.into();
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
        assert_eq!(labels(SectionMode::LastPlayed)[0], "TODAY");
    }

    #[test]
    fn section_midpoint_is_band_center() {
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
        assert!((visibility_after_idle(HIDE_AFTER_MS + OPACITY_MS) - 0.0).abs() < 1e-5);
        let mid = visibility_after_idle(HIDE_AFTER_MS + OPACITY_MS / 2);
        assert!(mid > 0.05 && mid < 0.95, "mid={mid}");
    }

    #[test]
    fn opacity_leads_the_slide() {
        const { assert!(OPACITY_MS < FADE_MS) };
        let half = OPACITY_MS / 2;
        let op_left = show_hide_amount(1.0, 0.0, half, OPACITY_MS);
        let slide_left = show_hide_amount(1.0, 0.0, half, FADE_MS);
        assert!(
            op_left < slide_left - 0.05,
            "op={op_left} slide={slide_left}"
        );
        assert!(show_hide_amount(1.0, 0.0, OPACITY_MS, OPACITY_MS).abs() < 1e-4);
        assert!(show_hide_amount(1.0, 0.0, OPACITY_MS, FADE_MS) > 0.02);
    }

    #[test]
    fn label_scroll_is_antisymmetric_and_grows() {
        let h = 20.0;
        assert!(label_y_offset(0.0, h).abs() < 1e-4);
        assert!((label_y_offset(1.0, h) + label_y_offset(-1.0, h)).abs() < 0.01);
        assert!(label_y_offset(2.0, h).abs() > label_y_offset(1.0, h).abs());
        // Mid-scroll (fractional) stays continuous.
        let a = label_y_offset(0.5, h);
        let b = label_y_offset(1.0, h);
        assert!(a > 0.0 && a < b);
    }

    #[test]
    fn base_h_targets_two_thirds_extent() {
        let bar_h = 900.0;
        let base = base_h_for_bar(bar_h);
        let half = label_y_offset(VISIBLE_DIST, base).abs();
        let target = bar_h * BAR_HEIGHT_FRAC * 0.5;
        assert!((half - target).abs() < 1.0, "half={half} target={target}");
    }

    #[test]
    fn labels_stop_at_the_active_end() {
        let count = 6;
        let center = 450.0;
        let base = base_h_for_bar(900.0);
        let top = label_positions(count, 0.0, center, base);
        let y_top_min = top.iter().map(|p| p.2).fold(f32::MAX, f32::min);
        let y_top_max = top.iter().map(|p| p.2).fold(f32::MIN, f32::max);
        assert!(y_top_min >= center - 0.5, "top extends up {y_top_min}");
        assert!(y_top_max > center + 40.0);

        let bot = label_positions(count, (count - 1) as f32, center, base);
        let y_bot_min = bot.iter().map(|p| p.2).fold(f32::MAX, f32::min);
        let y_bot_max = bot.iter().map(|p| p.2).fold(f32::MIN, f32::max);
        assert!(y_bot_max <= center + 0.5, "bottom extends down {y_bot_max}");
        assert!(y_bot_min < center - 40.0);

        let mid = label_positions(count, 2.5, center, base);
        let y_mid_min = mid.iter().map(|p| p.2).fold(f32::MAX, f32::min);
        let y_mid_max = mid.iter().map(|p| p.2).fold(f32::MIN, f32::max);
        let span = y_mid_max - y_mid_min;
        assert!(span > (y_top_max - y_top_min) + 20.0, "mid span {span}");
    }

    #[test]
    fn fractional_section_moves_labels() {
        let base = 20.0;
        let a = label_positions(6, 0.0, 100.0, base);
        let b = label_positions(6, 0.5, 100.0, base);
        let y0 = a.iter().find(|p| p.0 == 0).unwrap().2;
        let y0b = b.iter().find(|p| p.0 == 0).unwrap().2;
        let y1 = a.iter().find(|p| p.0 == 1).unwrap().2;
        let y1b = b.iter().find(|p| p.0 == 1).unwrap().2;
        assert!(y0b < y0 - 1.0, "label 0 should slide up");
        assert!(y1b < y1 - 1.0, "label 1 should slide up");
    }

    #[test]
    fn lines_sit_in_the_gaps_between_sections() {
        let count = 6usize;
        let base = base_h_for_bar(900.0);
        let visual = 2.0f32;
        assert!(
            gap_line(0, 1, 0.0, base).is_none(),
            "no line without a neighbor"
        );
        assert!(gap_line(count - 1, count, visual, base).is_none());

        let mut prev_end = f32::MIN;
        for i in 0..count - 1 {
            let (y0, y1) = gap_line(i, count, visual, base).expect("gap");
            let c0 = label_y_offset(i as f32 - visual, base);
            let c1 = label_y_offset((i + 1) as f32 - visual, base);
            assert!(y0 > c0 + 1.0, "line {i} crosses the upper label");
            assert!(y1 < c1 - 1.0, "line {i} crosses the lower label");
            assert!(y1 > y0, "line {i} inverted");
            assert!(y0 >= prev_end - 0.01, "line {i} overlaps the previous gap");
            prev_end = y1;
        }

        // At the top of the list the first line starts below the first label.
        // At the bottom the last line ends above the last label.
        let (top0, _) = gap_line(0, count, 0.0, base).unwrap();
        assert!(top0 > label_y_offset(0.0, base));
        let (_, bot1) = gap_line(count - 2, count, (count - 1) as f32, base).unwrap();
        assert!(bot1 < label_y_offset(0.0, base));

        let (a0, _) = gap_line(0, count, 0.0, base).unwrap();
        let (b0, _) = gap_line(0, count, 0.5, base).unwrap();
        assert!(b0 < a0 - 1.0, "gap line should scroll with the sections");
    }

    #[test]
    fn lines_and_labels_share_column_center() {
        assert!((column_axis(WIDTH) - WIDTH * 0.5).abs() < 1e-4);
        assert!((column_axis(120.0) - 60.0).abs() < 1e-4);
    }
}
