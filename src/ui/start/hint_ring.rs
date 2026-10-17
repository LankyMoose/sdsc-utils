//! SVG face-button rings for Start hint glyphs (Idle / Pressed / Hold-progress).
//!
//! One `canvas` widget per hint glyph meant one tiny-skia layer rebuilt on
//! every frame while Start is visible (footer hints, dock hints, hero cue
//! boxes — a dozen-plus instances at 60fps). These rings are plain stroked
//! circles, so they render identically as SVG: iced hashes `Handle`
//! content for its cache id, parses + rasterizes each distinct handle once,
//! then reuses the atlas entry on every later frame, and `.opacity()` rides
//! outside the cache keys (no re-raster while fading).
//!
//! Steady-state per-frame cost is therefore one small deterministic string
//! format + hash per glyph. Animated hold arcs stay correct by quantizing
//! progress to [`PROGRESS_STEPS`] (the segment count of the canvas painter
//! this replaces) and the armed blend to [`ARMED_STEPS`): a full 0→1 sweep
//! mints a bounded set of handles no matter how many frames it spans (see
//! `hold_sweep_is_bounded`). No app-side cache is needed — and deliberately
//! none exists, so there is no shared global for tests to coordinate.

use iced::widget::svg;

use crate::ui::theme;

/// Hold-progress quantization steps (matches the canvas segment count).
pub const PROGRESS_STEPS: u32 = 48;
/// Armed-blend quantization steps for the hold arc color.
pub const ARMED_STEPS: u32 = 8;
/// Idle/hold track alpha. Raised past the old canvas 0.25: resvg
/// anti-aliasing leaves fewer fully-covered pixels on a 2px ring than the
/// tiny-skia strokes did, so 0.25 rendered visibly dimmer — one knob to tune.
pub const IDLE_TRACK_ALPHA: f32 = 0.40;

/// Which ring to draw (mirrors `RingStyle` in `view.rs` without depending on it).
#[derive(Debug, Clone, Copy)]
pub enum RingKind {
    Idle,
    Pressed,
    Hold { progress: f32, armed_t: f32 },
}

/// Pictogram box for a ring slot: the face art scaled by the idle-ring
/// ratio, snapped to whole pixels. A fractional box centers at a fractional
/// offset, and the atlas entry then resamples asymmetrically — the button
/// icon reads off-center inside its (exactly symmetric) ring on every hint.
pub fn pictogram_size(box_size: f32, pressed_scale: f32, glyph_size: f32) -> f32 {
    let half = box_size / 2.0;
    // Mirror `face_glyph_stack`: idle radius is inset so full press stays
    // in-slot; the pictogram scales by its ratio to the glyph box.
    let idle_radius = (half - 2.5) / pressed_scale;
    (glyph_size * (idle_radius / (half - 2.0))).round()
}

/// SVG handle for `kind` at `radius` in a `box_size` viewport.
///
/// Byte-deterministic: identical inputs always produce identical bytes, hence
/// the same `Handle::id`, hence one renderer cache entry. `radius` is the
/// stroke-center radius (the idle inset that keeps ×`PRESSED_SCALE` in-slot);
/// alpha is intentionally *not* baked — callers apply the widget `.opacity()`.
pub fn ring_svg(kind: RingKind, radius: f32, box_size: f32) -> svg::Handle {
    svg::Handle::from_memory(ring_string(kind, radius, box_size).into_bytes())
}

fn hex(color: iced::Color) -> String {
    fn b(v: f32) -> u8 {
        (v.clamp(0.0, 1.0) * 255.0).round() as u8
    }
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        b(color.r),
        b(color.g),
        b(color.b),
        b(color.a)
    )
}

fn ring_string(kind: RingKind, radius: f32, box_size: f32) -> String {
    let c = box_size / 2.0;
    let (track_w, track) = match kind {
        RingKind::Pressed => (2.5, theme::alpha(theme::ACCENT, 1.0)),
        RingKind::Idle | RingKind::Hold { .. } => {
            (2.0, theme::alpha(theme::MUTED, IDLE_TRACK_ALPHA))
        }
    };
    let mut out = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{box_size:.2}\" height=\"{box_size:.2}\" viewBox=\"0 0 {box_size:.2} {box_size:.2}\">\
         <circle cx=\"{c:.2}\" cy=\"{c:.2}\" r=\"{radius:.2}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{track_w:.2}\"/>",
        hex(track),
    );
    if let RingKind::Hold { progress, armed_t } = kind {
        let q = (progress.clamp(0.0, 1.0) * PROGRESS_STEPS as f32).round() as u32;
        if q > 0 {
            let qa = (armed_t.clamp(0.0, 1.0) * ARMED_STEPS as f32).round() as u32;
            let arc = hex(hold_arc_color(qa));
            if q >= PROGRESS_STEPS {
                // A full sweep has identical start/end points, which SVG
                // renders as *nothing* — emit a full circle instead, or a
                // completed hold would show only the dim track.
                out.push_str(&format!(
                    "<circle cx=\"{c:.2}\" cy=\"{c:.2}\" r=\"{radius:.2}\" fill=\"none\" stroke=\"{arc}\" stroke-width=\"2.50\"/>"
                ));
            } else {
                out.push_str(&arc_path(q, radius, c, &arc));
            }
        }
    }
    out.push_str("</svg>");
    out
}

/// Hold arc color: ACCENT → INK over the quantized armed blend (integer
/// math so identical inputs always format identically). A fully-armed hold
/// reads white-hot, matching the documented "toward ink" intent.
fn hold_arc_color(qa: u32) -> iced::Color {
    let t = (qa.min(ARMED_STEPS) as f32 / ARMED_STEPS as f32).clamp(0.0, 1.0);
    let lerp = |a: f32, b: f32| a + (b - a) * t;
    let full = theme::ACCENT;
    let hot = theme::INK;
    iced::Color {
        r: lerp(full.r, hot.r),
        g: lerp(full.g, hot.g),
        b: lerp(full.b, hot.b),
        a: lerp(full.a, hot.a),
    }
}

/// Progress arc path for quantized step `q`: starts at 12 o'clock
/// (matching the canvas painter's `-FRAC_PI_2` start), sweeps clockwise.
fn arc_path(q: u32, radius: f32, center: f32, stroke: &str) -> String {
    let sweep = q as f32 / PROGRESS_STEPS as f32 * std::f32::consts::TAU;
    let (x0, y0) = (
        center + radius * (-std::f32::consts::FRAC_PI_2).cos(),
        center + radius * (-std::f32::consts::FRAC_PI_2).sin(),
    );
    let end = -std::f32::consts::FRAC_PI_2 + sweep;
    let (x1, y1) = (center + radius * end.cos(), center + radius * end.sin());
    let large = if sweep > std::f32::consts::PI { 1 } else { 0 };
    format!(
        "<path d=\"M {x0:.2} {y0:.2} A {radius:.2} {radius:.2} 0 {large} 1 {x1:.2} {y1:.2}\" fill=\"none\" stroke=\"{stroke}\" stroke-width=\"2.50\"/>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::time::Instant;

    const SIZE: f32 = 32.0;
    const RADIUS: f32 = (16.0 - 2.5) / 1.1;

    /// Static rings must hash to one handle id no matter how often they are
    /// rebuilt: the renderer then parses + rasterizes once and every later
    /// frame is a free atlas hit (the core of the canvas→SVG saving).
    #[test]
    fn static_rings_are_handle_stable() {
        for kind in [RingKind::Idle, RingKind::Pressed] {
            let ids: HashSet<u64> = (0..100)
                .map(|_| ring_svg(kind, RADIUS, SIZE).id())
                .collect();
            assert_eq!(ids.len(), 1, "kind={kind:?}");
        }
        assert_ne!(
            ring_svg(RingKind::Idle, RADIUS, SIZE).id(),
            ring_svg(RingKind::Pressed, RADIUS, SIZE).id()
        );
    }

    /// A full progress × armed sweep mints a bounded handle set no matter how
    /// many frames render it: raster work is O(states), not O(frames).
    #[test]
    fn hold_sweep_is_bounded() {
        let mut ids = HashSet::new();
        let mut p = 0.0;
        while p <= 1.0001 {
            let mut a = 0.0;
            while a <= 1.0001 {
                ids.insert(
                    ring_svg(
                        RingKind::Hold {
                            progress: p,
                            armed_t: a,
                        },
                        RADIUS,
                        SIZE,
                    )
                    .id(),
                );
                a += 0.005;
            }
            p += 0.001;
        }
        let cap = (PROGRESS_STEPS + 1) * (ARMED_STEPS + 1);
        assert!(ids.len() as u32 <= cap, "distinct={} cap={cap}", ids.len());
        // And the sweep actually varies (not one degenerate handle).
        assert!(ids.len() > 100, "distinct={}", ids.len());
    }

    /// Per-frame hot path is one small string build: tripwire against
    /// generation regressions (parse/rasterize happen once per handle in the
    /// renderer and are covered by the stability tests above).
    #[test]
    fn generation_throughput_tripwire() {
        let start = Instant::now();
        let mut sink = 0u64;
        for i in 0..20_000 {
            let p = (i % 49) as f32 / 48.0;
            sink = sink.wrapping_add(
                ring_svg(
                    RingKind::Hold {
                        progress: p,
                        armed_t: 0.5,
                    },
                    RADIUS,
                    SIZE,
                )
                .id(),
            );
        }
        assert!(start.elapsed().as_secs() < 1, "sink={sink}");
    }

    /// Arc geometry matches the canvas painter: 12 o'clock start, clockwise
    /// sweep, large-arc flip past half, gated below a flicker.
    #[test]
    fn arc_geometry_matches_canvas_math() {
        // q=12 → quarter sweep → ends at 3 o'clock.
        let path = arc_path(12, RADIUS, 16.0, "#ffffffff");
        assert!(path.contains("M 16.00 3.73"), "starts at top: {path}");
        assert!(
            path.contains("A 12.27 12.27 0 0 1 28.27 16.00"),
            "quarter arc: {path}"
        );
        // Half sweep and beyond set the large-arc flag.
        assert!(arc_path(24, RADIUS, 16.0, "#ffffffff").contains("0 0 1"));
        assert!(arc_path(36, RADIUS, 16.0, "#ffffffff").contains("0 1 1"));
        // Armed endpoints hit the exact theme colors (white-hot at full arm).
        assert_eq!(hex(hold_arc_color(0)), hex(theme::ACCENT));
        assert_eq!(hex(hold_arc_color(ARMED_STEPS)), hex(theme::INK));
    }

    /// Full sweeps render a complete circle, not the degenerate same-point
    /// arc (which SVG omits): a finished hold must show a full bright ring.
    #[test]
    fn full_hold_renders_complete_circle() {
        let xml = ring_string(
            RingKind::Hold {
                progress: 1.0,
                armed_t: 1.0,
            },
            RADIUS,
            SIZE,
        );
        assert!(!xml.contains("<path"), "{xml}");
        assert!(
            xml.contains(&format!(
                "<circle cx=\"16.00\" cy=\"16.00\" r=\"12.27\" fill=\"none\" stroke=\"{}\"",
                hex(theme::INK)
            )),
            "{xml}"
        );
    }

    /// Rasterize with the same resvg the app renders through and measure
    /// pixels: symmetry (centering), ink mass (completeness), peak alpha
    /// (baked lightness). Catches visual regressions no string assert can.
    mod pixels {
        use super::*;
        use resvg::tiny_skia::Pixmap;

        const SCALE: f32 = 4.0;

        fn rasterize(kind: RingKind) -> Pixmap {
            let xml = ring_string(kind, RADIUS, SIZE);
            let tree =
                resvg::usvg::Tree::from_data(xml.as_bytes(), &resvg::usvg::Options::default())
                    .expect("ring svg must parse");
            let size = (SIZE * SCALE) as u32;
            let mut pixmap = Pixmap::new(size, size).expect("pixmap");
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(SCALE, SCALE),
                &mut pixmap.as_mut(),
            );
            pixmap
        }

        fn alpha_mass(pixmap: &Pixmap, x0: u32, x1: u32) -> u32 {
            let w = pixmap.width();
            pixmap
                .pixels()
                .iter()
                .enumerate()
                .filter(|(i, _)| {
                    let x = (*i as u32) % w;
                    x >= x0 && x < x1
                })
                .map(|(_, p)| p.alpha() as u32)
                .sum()
        }

        fn total_mass(pixmap: &Pixmap) -> u32 {
            pixmap.pixels().iter().map(|p| p.alpha() as u32).sum()
        }

        /// Idle and pressed rings are left/right symmetric: the circle is
        /// horizontally centered in its box.
        #[test]
        fn rings_are_horizontally_centered() {
            for kind in [RingKind::Idle, RingKind::Pressed] {
                let pixmap = rasterize(kind);
                let w = pixmap.width();
                let left = alpha_mass(&pixmap, 0, w / 2);
                let right = alpha_mass(&pixmap, w / 2, w);
                let diff = left.abs_diff(right) as f32 / left.max(1) as f32;
                assert!(diff < 0.02, "kind={kind:?} left={left} right={right}");
                assert!(left > 0, "kind={kind:?} rendered nothing");
            }
        }

        /// A finished hold carries the same ink as the pressed full circle:
        /// the completion ring actually draws.
        #[test]
        fn full_hold_matches_pressed_ink_mass() {
            let full = rasterize(RingKind::Hold {
                progress: 1.0,
                armed_t: 1.0,
            });
            let pressed = rasterize(RingKind::Pressed);
            let (a, b) = (total_mass(&full), total_mass(&pressed));
            let diff = a.abs_diff(b) as f32 / b.max(1) as f32;
            assert!(diff < 0.10, "full={a} pressed={b}");
        }

        /// The baked idle track alpha survives rasterization: fully-covered
        /// ring pixels land near `IDLE_TRACK_ALPHA`, so the ring cannot
        /// silently darken (or lighten) again.
        #[test]
        fn idle_track_lightness_is_pinned() {
            let pixmap = rasterize(RingKind::Idle);
            let peak = pixmap.pixels().iter().map(|p| p.alpha()).max().unwrap_or(0);
            let expect = (IDLE_TRACK_ALPHA * 255.0).round() as i16;
            assert!(
                (peak as i16 - expect).abs() <= 25,
                "peak={peak} expect~{expect}"
            );
        }

        /// Composed glyph (ring + centered face pictogram, exactly like
        /// `face_glyph_stack`): pictogram ink stays concentric with the ring.
        /// A fractional pictogram box (15.78px → 8.11px offset) resamples its
        /// atlas entry asymmetrically against the exact ring, reading
        /// off-center on every hint — whole pixels keep them concentric.
        #[test]
        fn pictogram_stays_concentric_with_ring() {
            let glyph = crate::ui::start::hint_ring::pictogram_size(SIZE, 1.1, 18.0);
            // Whole pixels, exact centered offset: the regression pin itself.
            assert_eq!(glyph.fract(), 0.0, "glyph={glyph}");
            assert_eq!(((SIZE - glyph) / 2.0).fract(), 0.0);
            let gap_fixed = composed_gap(glyph);
            assert!(gap_fixed < 0.15, "gap={gap_fixed}");
            // Diagnosis pin: the legacy fractional box skewed measurably worse.
            let legacy = 18.0 * (((SIZE / 2.0) - 2.5) / 1.1 / ((SIZE / 2.0) - 2.0));
            let gap_legacy = composed_gap(legacy);
            assert!(
                gap_legacy > gap_fixed,
                "legacy={gap_legacy} fixed={gap_fixed}"
            );
        }

        /// Blue-vs-grey centroid gap (real px) for a pictogram of `glyph_box`
        /// composited at its centered offset over the idle ring.
        fn composed_gap(glyph_box: f32) -> f32 {
            use resvg::tiny_skia::Transform;
            let ring = rasterize(RingKind::Idle);
            let art = resvg::usvg::Tree::from_data(
                crate::ui::svg_icon::FACE_CIRCLE_SVG.as_bytes(),
                &resvg::usvg::Options::default(),
            )
            .expect("pictogram must parse");
            let offset = (SIZE - glyph_box) / 2.0;
            let s = glyph_box / 24.0 * SCALE;
            let t = Transform::from_row(s, 0.0, 0.0, s, offset * SCALE, offset * SCALE);
            let dim = (SIZE * SCALE) as u32;
            let mut art_px = Pixmap::new(dim, dim).expect("pixmap");
            resvg::render(&art, t, &mut art_px.as_mut());
            let (cx_blue, _) = centroid(&art_px, true);
            let (cx_grey, _) = centroid(&ring, false);
            (cx_blue - cx_grey).abs() / SCALE
        }

        /// Centroid-x (pixmap px) of blue pictogram ink (`blue_only`) or the
        /// grey ring ink, weighted by alpha.
        fn centroid(pixmap: &Pixmap, blue_only: bool) -> (f32, f32) {
            let (mut sx, mut sy, mut m) = (0.0f64, 0.0f64, 0.0f64);
            for (i, p) in pixmap.pixels().iter().enumerate() {
                let r = p.red() as f32;
                let b = p.blue() as f32;
                let is_blue = b > r + 40.0;
                if is_blue != blue_only || p.alpha() < 8 {
                    continue;
                }
                let m_add = p.alpha() as f64;
                sx += ((i as u32 % pixmap.width()) as f64) * m_add;
                sy += ((i as u32 / pixmap.width()) as f64) * m_add;
                m += m_add;
            }
            assert!(m > 0.0, "no ink class matched");
            (sx as f32 / m as f32, sy as f32 / m as f32)
        }
    }

    /// Ring viewport matches the canvas box so layout is unchanged.
    #[test]
    fn viewport_matches_canvas_box() {
        let xml = ring_string(RingKind::Idle, RADIUS, SIZE);
        assert!(xml.contains("width=\"32.00\""), "{xml}");
        assert!(xml.contains("stroke-width=\"2.00\""), "{xml}");
        let xml = ring_string(RingKind::Pressed, RADIUS, SIZE);
        assert!(xml.contains("stroke-width=\"2.50\""), "{xml}");
        // Flicker-gated: no arc element at rest.
        let xml = ring_string(
            RingKind::Hold {
                progress: 0.0,
                armed_t: 0.0,
            },
            RADIUS,
            SIZE,
        );
        assert!(!xml.contains("<path"), "{xml}");
    }
}
