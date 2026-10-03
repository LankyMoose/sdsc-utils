//! Vertical cover-flow strip: selection locked to center with scale falloff.

use crate::ui::layout::ease_out_cubic;
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell, overlay};
use iced::mouse;
use iced::{Element, Event, Length, Rectangle, Size, Vector};
use std::time::{Duration, Instant};

/// Visible slots above/below the center selection (peek / full / selected / full / peek).
pub const NEIGHBORS: isize = 2;
pub const VISIBLE: usize = (NEIGHBORS * 2 + 1) as usize;
pub const STRIP_ANIM_MS: u64 = 200;
/// Shorter retarget when the strip is already mid-scroll (rapid pad/keyboard steps).
pub const STRIP_ANIM_CATCHUP_MS: u64 = 110;

/// Selected hero scale relative to base capsule size.
pub const SELECTED_SCALE: f32 = 1.42;
/// Scale at `|d| = 1` (continues shrinking past the first neighbor).
pub const SCALE_AT_1: f32 = 0.88;
/// Scale at `|d| = 2` (outer peek cards).
pub const SCALE_AT_2: f32 = 0.58;
/// Neighbor title + subtitle column width.
pub const TITLE_COL: f32 = 480.0;
/// Rightward arc offset per index unit of distance from center.
pub const ARC_PX: f32 = 14.0;
/// Constant edge gap between adjacent capsules (negative = slight overlap / denser stream).
const STRIDE_GAP: f32 = 12.0;
/// Fraction of the outer (`|d| = 2`) card visible at the viewport edge.
const OUTER_PEEK: f32 = 0.40;
/// Portrait aspect for hero capsules (`w / h`).
const ASPECT: f32 = 220.0 / 330.0;
const CENTER_H_MIN: f32 = 180.0;
const CENTER_H_MAX: f32 = 420.0;

/// Resolution-derived strip geometry for layout + slot art sizing.
#[derive(Debug, Clone, Copy)]
pub struct StripMetrics {
    pub center_w: f32,
    pub center_h: f32,
    pub selected_scale: f32,
    pub stride: f32,
    pub slot_w: f32,
    pub slot_h: f32,
}

impl StripMetrics {
    /// Metrics for a typical 1080p immersive cover (tests / fallbacks).
    pub fn default_1080() -> Self {
        metrics_for_height(1080.0)
    }
}

/// Center-to-center step between slots at integer distances `i` and `i + 1`.
///
/// Sized from the two capsule heights so **edge** gaps stay even as scale falls off
/// (a constant stride left too much air between `|d|=1` and `|d|=2`).
pub fn unit_stride(from_distance: f32, center_h: f32) -> f32 {
    let i = from_distance.floor().max(0.0);
    let h0 = center_h * scale_at_distance(i);
    let h1 = center_h * scale_at_distance(i + 1.0);
    0.5 * (h0 + h1) + STRIDE_GAP
}

/// Signed Y offset of a slot center from the strip midline for a fractional distance.
pub fn slot_y_offset(distance: f32, center_h: f32) -> f32 {
    let sign = if distance >= 0.0 { 1.0 } else { -1.0 };
    let ad = distance.abs();
    let mut offset = 0.0;
    let mut d = 0.0;
    while d < ad - 1e-4 {
        let step_end = (d.floor() + 1.0).min(ad);
        let span = step_end - d;
        offset += unit_stride(d, center_h) * span;
        d = step_end;
    }
    sign * offset
}

/// Derive capsule size + stride so the strip fits 3 full heroes + outer peeks in `stage_h`.
pub fn metrics_for_height(stage_h: f32) -> StripMetrics {
    let stage_h = stage_h.max(480.0);
    // half ≈ stride(0→1) + stride(1→2) + h*scale(2)*(0.5 - PEEK)
    // stride(i→i+1) = 0.5 * h * (scale(i)+scale(i+1)) + GAP
    let scale_sum = 0.5 * (SELECTED_SCALE + 2.0 * SCALE_AT_1 + SCALE_AT_2);
    let denom = scale_sum + SCALE_AT_2 * (0.5 - OUTER_PEEK);
    // Two gaps appear in the half-extent sum; STRIDE_GAP may be negative (overlap).
    let numer = (stage_h * 0.5 - 2.0 * STRIDE_GAP).max(80.0);
    let center_h = (numer / denom).clamp(CENTER_H_MIN, CENTER_H_MAX);
    let center_w = center_h * ASPECT;
    // Reference stride = first step (selected → neighbor); layout uses [`slot_y_offset`].
    let stride = unit_stride(0.0, center_h);
    StripMetrics {
        center_w,
        center_h,
        selected_scale: SELECTED_SCALE,
        stride,
        slot_w: center_w * SELECTED_SCALE + TITLE_COL,
        slot_h: center_h,
    }
}

/// Scale for a signed distance (in index units) from the visual center.
///
/// Selected is enlarged; neighbors keep shrinking past `|d| = 1`.
pub fn scale_at_distance(distance: f32) -> f32 {
    let d = distance.abs();
    if d <= 1.0 {
        SELECTED_SCALE + (SCALE_AT_1 - SELECTED_SCALE) * d
    } else if d <= 2.0 {
        SCALE_AT_1 + (SCALE_AT_2 - SCALE_AT_1) * (d - 1.0)
    } else {
        (SCALE_AT_2 - 0.06 * (d - 2.0)).max(0.48)
    }
}

/// Opacity for a signed distance from the visual center.
pub fn opacity_at_distance(distance: f32) -> f32 {
    let d = distance.abs();
    (1.0 - 0.45 * d).clamp(0.18, 1.0)
}

/// Rightward X offset (logical px) for a signed strip distance.
pub fn arc_offset_x(distance: f32) -> f32 {
    ARC_PX * distance.abs()
}

/// Layout box for a strip child at `distance`.
///
/// Capsule height/width follow [`scale_at_distance`]; the title column stays
/// [`TITLE_COL`] so outer peeks (`|d| = 2`) do not force needless wraps.
pub fn slot_child_size(metrics: &StripMetrics, distance: f32) -> Size {
    let scale = scale_at_distance(distance);
    let max_art_w = metrics.center_w * metrics.selected_scale;
    let art_w = metrics.center_w * scale;
    let inset = ((max_art_w - art_w) * 0.5).max(0.0);
    Size::new(inset + art_w + TITLE_COL, metrics.center_h * scale)
}

/// Catalog index for a visible slot, or `None` for a skeleton filler.
///
/// When `len < VISIBLE`, out-of-range linear indices become dummies. With enough
/// games, slots wrap circularly so the stream stays filled with real titles.
pub fn slot_catalog_index(selected: usize, delta: isize, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let idx = selected as isize + delta;
    if len < VISIBLE {
        if idx < 0 || idx >= len as isize {
            None
        } else {
            Some(idx as usize)
        }
    } else {
        Some(idx.rem_euclid(len as isize) as usize)
    }
}

/// Shortest signed step on a circular list of length `len` from `from` → `to`.
///
/// Used so last→first animates as `+1` (and first→last as `-1`) instead of
/// lerping across the whole catalog.
pub fn shortest_circular_delta(from: f32, to: f32, len: f32) -> f32 {
    if len <= 1.0 {
        return 0.0;
    }
    let mut d = (to - from).rem_euclid(len);
    if d > len * 0.5 {
        d -= len;
    }
    d
}

/// Eased fractional scroll between `from` and `to` indices.
pub fn strip_scroll(from: f32, to: f32, started: Instant, now: Instant, duration_ms: u64) -> f32 {
    let duration = Duration::from_millis(duration_ms.max(1));
    let t = (now.saturating_duration_since(started).as_secs_f32() / duration.as_secs_f32())
        .clamp(0.0, 1.0);
    let e = ease_out_cubic(t);
    from + (to - from) * e
}

/// Signed distance for visible slot `i` (0..VISIBLE) when the strip is anchored
/// on `anchor` and the visual scroll sits at `visual_scroll`.
pub fn slot_distance(i: usize, anchor: usize, visual_scroll: f32) -> f32 {
    let slot = i as f32 - NEIGHBORS as f32;
    slot + (anchor as f32 - visual_scroll)
}

pub struct VStrip<'a, Message, Theme = iced::Theme, Renderer = iced::Renderer> {
    /// Fractional game index at the visual center.
    visual_scroll: f32,
    /// Catalog index of the center slot's game.
    anchor: usize,
    metrics: StripMetrics,
    width: Length,
    height: Length,
    /// Children ordered top→bottom for the visible window (length [`VISIBLE`]).
    items: Vec<Element<'a, Message, Theme, Renderer>>,
}

pub fn vstrip<'a, Message, Theme, Renderer>(
    visual_scroll: f32,
    anchor: usize,
    metrics: StripMetrics,
    items: impl IntoIterator<Item = Element<'a, Message, Theme, Renderer>>,
) -> VStrip<'a, Message, Theme, Renderer> {
    VStrip {
        visual_scroll,
        anchor,
        metrics,
        width: Length::Fill,
        height: Length::Fill,
        items: items.into_iter().collect(),
    }
}

impl<'a, Message, Theme, Renderer> VStrip<'a, Message, Theme, Renderer> {
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.height = height.into();
        self
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for VStrip<'_, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::stateless()
    }

    fn state(&self) -> tree::State {
        tree::State::None
    }

    fn children(&self) -> Vec<Tree> {
        self.items.iter().map(Tree::new).collect()
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(self.items.as_slice());
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: self.height,
        }
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let limits = limits.width(self.width).height(self.height);
        let size = limits.resolve(self.width, self.height, Size::ZERO);
        let center_h = self.metrics.center_h;

        let mut children = Vec::with_capacity(self.items.len());
        for (i, (item, child_tree)) in self
            .items
            .iter_mut()
            .zip(tree.children.iter_mut())
            .enumerate()
        {
            let dist = slot_distance(i, self.anchor, self.visual_scroll);
            let child_size = slot_child_size(&self.metrics, dist);
            let child_limits = layout::Limits::new(Size::ZERO, child_size);
            let mut node = item
                .as_widget_mut()
                .layout(child_tree, renderer, &child_limits);
            // Selected stays left-aligned; neighbors drift right with distance (arc).
            let x = arc_offset_x(dist);
            let y = size.height * 0.5 + slot_y_offset(dist, center_h) - child_size.height * 0.5;
            node = node.move_to(iced::Point::new(x, y));
            children.push(node);
        }

        layout::Node::with_children(size, children)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn iced::advanced::widget::Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            self.items
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
                .for_each(|((child, state), child_layout)| {
                    child
                        .as_widget_mut()
                        .operate(state, child_layout, renderer, operation);
                });
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        if !cursor.is_over(bounds) {
            return;
        }
        for ((child, state), child_layout) in self
            .items
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            child.as_widget_mut().update(
                state,
                event,
                child_layout,
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
            if shell.is_event_captured() {
                return;
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.items
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .map(|((child, state), child_layout)| {
                child
                    .as_widget()
                    .mouse_interaction(state, child_layout, cursor, viewport, renderer)
            })
            .max()
            .unwrap_or_default()
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let Some(clipped) = bounds.intersection(viewport) else {
            return;
        };

        let child_layouts: Vec<_> = layout.children().collect();
        let mut order: Vec<usize> = (0..self.items.len()).collect();
        order.sort_by(|&a, &b| {
            let da = slot_distance(a, self.anchor, self.visual_scroll).abs();
            let db = slot_distance(b, self.anchor, self.visual_scroll).abs();
            db.partial_cmp(&da).unwrap_or(std::cmp::Ordering::Equal)
        });

        renderer.with_layer(clipped, |renderer| {
            for i in order {
                let Some(child_layout) = child_layouts.get(i).copied() else {
                    continue;
                };
                self.items[i].as_widget().draw(
                    &tree.children[i],
                    renderer,
                    theme,
                    style,
                    child_layout,
                    cursor,
                    &clipped,
                );
            }
        });
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        overlay::from_children(
            &mut self.items,
            tree,
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message, Theme, Renderer> From<VStrip<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(value: VStrip<'a, Message, Theme, Renderer>) -> Self {
        Element::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_falls_off_with_distance() {
        assert!((scale_at_distance(0.0) - SELECTED_SCALE).abs() < 0.001);
        assert!((scale_at_distance(1.0) - SCALE_AT_1).abs() < 0.001);
        assert!((scale_at_distance(2.0) - SCALE_AT_2).abs() < 0.001);
        assert!(scale_at_distance(0.5) > SCALE_AT_1 && scale_at_distance(0.5) < SELECTED_SCALE);
        assert!(scale_at_distance(1.5) > SCALE_AT_2 && scale_at_distance(1.5) < SCALE_AT_1);
    }

    #[test]
    fn opacity_steps_down_steeply() {
        assert!((opacity_at_distance(0.0) - 1.0).abs() < 0.001);
        assert!((opacity_at_distance(1.0) - 0.55).abs() < 0.001);
        assert!((opacity_at_distance(2.0) - 0.18).abs() < 0.001);
        assert!((opacity_at_distance(3.0) - 0.18).abs() < 0.001);
    }

    #[test]
    fn arc_grows_with_distance() {
        assert!((arc_offset_x(0.0)).abs() < 0.001);
        assert!((arc_offset_x(1.0) - ARC_PX).abs() < 0.001);
        assert!((arc_offset_x(-2.0) - 2.0 * ARC_PX).abs() < 0.001);
    }

    #[test]
    fn metrics_stride_matches_even_edge_gap() {
        let m = metrics_for_height(1080.0);
        let selected_h = m.center_h * m.selected_scale;
        let neighbor_h = m.center_h * SCALE_AT_1;
        let clearance = selected_h * 0.5 + neighbor_h * 0.5;
        assert!((m.stride - (clearance + STRIDE_GAP)).abs() < 0.01);
        assert!(m.center_h >= CENTER_H_MIN && m.center_h <= CENTER_H_MAX);
    }

    #[test]
    fn neighbor_edge_gaps_stay_even() {
        let h = 240.0;
        let s01 = unit_stride(0.0, h);
        let s12 = unit_stride(1.0, h);
        let gap01 = s01 - 0.5 * h * (SELECTED_SCALE + SCALE_AT_1);
        let gap12 = s12 - 0.5 * h * (SCALE_AT_1 + SCALE_AT_2);
        // Same edge gap at every step (constant STRIDE_GAP).
        assert!((gap01 - STRIDE_GAP).abs() < 0.01);
        assert!((gap12 - STRIDE_GAP).abs() < 0.01);
        // Outer step is shorter than selected→neighbor (smaller cards).
        assert!(s12 < s01);
        assert!((slot_y_offset(2.0, h) - (s01 + s12)).abs() < 0.01);
        assert!((slot_y_offset(-1.5, h) + (s01 + 0.5 * s12)).abs() < 0.01);
    }

    #[test]
    fn metrics_clamp_extreme_heights() {
        let tiny = metrics_for_height(400.0);
        let huge = metrics_for_height(4000.0);
        assert!((tiny.center_h - CENTER_H_MIN).abs() < 0.001);
        assert!((huge.center_h - CENTER_H_MAX).abs() < 0.001);
    }

    #[test]
    fn slot_catalog_index_dummies_only_when_short() {
        // Short catalog: linear + dummy OOB.
        assert_eq!(slot_catalog_index(0, -1, 3), None);
        assert_eq!(slot_catalog_index(0, 0, 3), Some(0));
        assert_eq!(slot_catalog_index(0, 2, 3), Some(2));
        assert_eq!(slot_catalog_index(0, 3, 3), None);
        assert_eq!(slot_catalog_index(2, 1, 3), None);
        assert_eq!(slot_catalog_index(0, 0, 0), None);
        // Enough games: circular fill (no dummies).
        assert_eq!(slot_catalog_index(0, -1, 5), Some(4));
        assert_eq!(slot_catalog_index(4, 1, 5), Some(0));
        assert_eq!(slot_catalog_index(2, 2, 5), Some(4));
    }

    #[test]
    fn shortest_circular_wraps_forward_and_back() {
        assert!((shortest_circular_delta(4.0, 0.0, 5.0) - 1.0).abs() < 0.001);
        assert!((shortest_circular_delta(0.0, 4.0, 5.0) - (-1.0)).abs() < 0.001);
        assert!((shortest_circular_delta(2.0, 3.0, 5.0) - 1.0).abs() < 0.001);
        assert!((shortest_circular_delta(3.0, 1.0, 5.0) - (-2.0)).abs() < 0.001);
    }

    #[test]
    fn strip_scroll_eases_to_target() {
        let start = Instant::now();
        let mid = start + Duration::from_millis(STRIP_ANIM_MS / 2);
        let end = start + Duration::from_millis(STRIP_ANIM_MS);
        let s0 = strip_scroll(0.0, 2.0, start, start, STRIP_ANIM_MS);
        let s1 = strip_scroll(0.0, 2.0, start, mid, STRIP_ANIM_MS);
        let s2 = strip_scroll(0.0, 2.0, start, end, STRIP_ANIM_MS);
        assert!((s0 - 0.0).abs() < 0.001);
        assert!(s1 > 0.0 && s1 < 2.0);
        assert!((s2 - 2.0).abs() < 0.001);
        // Ease-out is ahead of linear at midpoint.
        assert!(s1 > 1.0);
    }

    #[test]
    fn slot_distance_centers_anchor() {
        assert!((slot_distance(NEIGHBORS as usize, 5, 5.0)).abs() < 0.001);
        assert!((slot_distance(NEIGHBORS as usize + 1, 5, 5.0) - 1.0).abs() < 0.001);
        // Mid-scroll toward next: center slot sits slightly above visual center.
        assert!((slot_distance(NEIGHBORS as usize, 5, 4.5) - 0.5).abs() < 0.001);
    }

    #[test]
    fn slot_child_keeps_full_title_col() {
        let m = metrics_for_height(1080.0);
        let d2 = slot_child_size(&m, 2.0);
        let scale = SCALE_AT_2;
        let max_art_w = m.center_w * m.selected_scale;
        let art_w = m.center_w * scale;
        let inset = ((max_art_w - art_w) * 0.5).max(0.0);
        let title_w = d2.width - inset - art_w;
        assert!((title_w - TITLE_COL).abs() < 0.01);
        assert!(d2.width > m.slot_w * SCALE_AT_2 + 1.0);
        assert!((slot_child_size(&m, 0.0).width - m.slot_w).abs() < 0.01);
        for d in [0.0_f32, 1.0, 2.0] {
            let row = slot_child_size(&m, d);
            assert!(arc_offset_x(d) + row.width <= m.slot_w + 0.01);
        }
    }
}
