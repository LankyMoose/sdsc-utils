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
/// Max slots per side in horizontal mode (geometry decides the actual count).
pub const MAX_NEIGHBORS: isize = 5;
/// Max items across the strip on the widest screens.
pub const MAX_VISIBLE: usize = (MAX_NEIGHBORS * 2 + 1) as usize;
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
/// Gap between horizontal art and the title block below it.
pub const H_TEXT_GAP: f32 = 8.0;
/// Reserved height for the horizontal in-slot text (neighbor titles + meta).
///
/// Budget (unselected sizes, 1.3x line height): title 22px (28.6) + 6 +
/// two meta lines at 14px (38.4 incl. spacing 2) + 6 + update badge (~21)
/// = ~100. The selected 39px title lives in a separate full-width layer
/// below the strip (see [`H_SELECTED_TITLE_RESERVE`]), so only neighbors
/// carry this budget: [`slot_child_size_h`] weights it in with `|distance|`
/// (0 at the selection, full from `|d| >= 1`) and [`metrics_for_width`]
/// sizes the strip as the max of selected art vs neighbor art + text.
pub const H_TEXT_RESERVE: f32 = 100.0;
/// Reserved height for the full-width selected-title layer below the strip.
///
/// Budget (selected sizes, 1.3x line height): title 39px (50.7) + 6 +
/// two meta lines at 16px (43.6 incl. spacing 2) + 6 + update badge (~23)
/// = ~129.2; 132 keeps a few px of margin. Stage-width centered, so long
/// titles never collide with neighbors.
pub const H_SELECTED_TITLE_RESERVE: f32 = 132.0;
/// Gap between the horizontal strip (art + in-slot text) and the
/// full-width selected-title layer below it.
///
/// Matches [`H_TEXT_GAP`] so the selected title sits as tight to the art
/// row as neighbor in-slot titles do.
pub const H_STRIP_TO_TITLE_GAP: f32 = H_TEXT_GAP;
/// Top clearance kept above the horizontal strip (clock capsule + art-row drop).
///
/// Sizing only: the layout anchors the ART ROW (not the strip+title block)
/// mid-region via [`horizontal_block_lead`], so the shrunken capsules sit
/// ~100px lower than block-centering would place them while the title layer
/// keeps its position above the rail. Layout adds the lead on top of its own
/// clock pad, so this reserve stays conservative.
pub const H_TOP_RESERVE: f32 = 170.0;
/// Vertical room kept for the position rail + footer hints island in horizontal mode.
const H_RAIL_RESERVE: f32 = 120.0;
const H_FOOTER_RESERVE: f32 = 72.0;
/// Minimum gap from the selected-title bottom to the position rail top.
///
/// Matches the slack the old block-centering left (~18px at 1080p); the
/// art-row lead never eats into it, so the rail always clears the titles.
pub const H_TITLE_TO_BAR_MIN: f32 = 18.0;
/// Offset between the art-row visual center and the strip+title block center.
///
/// The strip holds art only for the selection, so the block below the art is
/// just the strip→title gap plus the full-width title layer
/// (`H_STRIP_TO_TITLE_GAP` + `H_SELECTED_TITLE_RESERVE`); shifting the block
/// down by up to half that stack centers the ART ROW (not the block) in the
/// region above the rail. Neighbor in-slot text (`H_TEXT_*`) lives inside the
/// strip's neighbor children and never pads the selected art bottom.
pub const H_ART_CENTER_OFFSET: f32 = (H_STRIP_TO_TITLE_GAP + H_SELECTED_TITLE_RESERVE) * 0.5;
/// Peak horizontal drift of the selected-title crossfade (logical px, in scroll direction).
pub const TITLE_DRIFT_PX: f32 = 28.0;

/// Top lead for the horizontal strip+title block.
///
/// Centers the block in `region_h` (space above the rail, after `top_pad`
/// clock clearance), then drops it by up to [`H_ART_CENTER_OFFSET`] so the
/// ART ROW lands centered instead of riding high. The drop never eats the
/// `min_bar_gap` below the titles. Pure so unit tests can pin the geometry.
pub fn horizontal_block_lead(region_h: f32, block_h: f32, top_pad: f32, min_bar_gap: f32) -> f32 {
    let slack = (region_h - top_pad - block_h - min_bar_gap).max(0.0);
    top_pad + slack * 0.5 + H_ART_CENTER_OFFSET.min(slack * 0.5)
}

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

/// Strip scroll axis. `Vertical` stacks heroes top-to-bottom (default);
/// `Horizontal` runs them left-to-right for the horizontal immersive layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StripOrientation {
    #[default]
    Vertical,
    Horizontal,
}

/// Signed X offset of a slot center from the strip midline for a fractional distance.
///
/// Same stride math as [`slot_y_offset`] but for the horizontal axis.
pub fn slot_x_offset(distance: f32, center_w: f32) -> f32 {
    let sign = if distance >= 0.0 { 1.0 } else { -1.0 };
    let ad = distance.abs();
    let mut offset = 0.0;
    let mut d = 0.0;
    while d < ad - 1e-4 {
        let step_end = (d.floor() + 1.0).min(ad);
        let span = step_end - d;
        offset += unit_stride(d, center_w) * span;
        d = step_end;
    }
    sign * offset
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

/// Derive capsule size + stride so the strip fits 3 full heroes + outer peeks in `stage_w`.
///
/// Unlike [`metrics_for_height`], the capsule size comes from the **vertical**
/// room (`stage_h` minus top clearance, position rail, and footer island):
/// the selected art plus the slim in-slot text, plus the separate full-width
/// selected-title layer below the strip, must fit. The horizontal room
/// instead decides how many neighbors are shown (see [`neighbors_for_width`]),
/// so wide screens show more items instead of bigger ones.
pub fn metrics_for_width(stage_w: f32, stage_h: f32) -> StripMetrics {
    let _ = stage_w;
    let stage_h = stage_h.max(480.0);
    let avail_h = (stage_h - H_RAIL_RESERVE - H_FOOTER_RESERVE - H_TOP_RESERVE).max(240.0);
    let overhead =
        (H_TEXT_GAP + H_TEXT_RESERVE) + (H_STRIP_TO_TITLE_GAP + H_SELECTED_TITLE_RESERVE);
    let from_h = (avail_h - overhead) / SELECTED_SCALE;
    let center_h = from_h.clamp(CENTER_H_MIN, CENTER_H_MAX);
    let center_w = center_h * ASPECT;
    // Reference stride = first step (selected → neighbor); layout uses [`slot_x_offset`].
    let stride = unit_stride(0.0, center_w);
    // Strip holds the selected art with no in-slot padding so the full-width
    // title sits one text gap below the art bottoms (like neighbor in-slot
    // titles). Neighbors still need art + text; the strip takes the max so
    // small stages (where the neighbor column outgrows the selected art)
    // never clip in-slot titles.
    let selected_art_h = center_h * SELECTED_SCALE;
    let neighbor_h = center_h * SCALE_AT_1 + H_TEXT_GAP + H_TEXT_RESERVE;
    StripMetrics {
        center_w,
        center_h,
        selected_scale: SELECTED_SCALE,
        stride,
        slot_w: center_w * SELECTED_SCALE,
        slot_h: selected_art_h.max(neighbor_h),
    }
}

/// Visible neighbors per side for a horizontal strip of width `stage_w`.
///
/// Walks outward accumulating [`unit_stride`] steps; a neighbor counts when its
/// outer peek still lands inside the half-width (minus side padding). Always at
/// least 1, at most [`MAX_NEIGHBORS`].
pub fn neighbors_for_width(stage_w: f32, center_w: f32) -> isize {
    let half = (stage_w * 0.5 - 16.0).max(0.0);
    let mut extent = 0.0;
    let mut neighbors: isize = 1;
    for i in 0..MAX_NEIGHBORS {
        extent += unit_stride(i as f32, center_w);
        let outer = scale_at_distance((i + 1) as f32);
        let peek = extent + center_w * outer * (0.5 - OUTER_PEEK);
        if peek <= half {
            neighbors = i + 1;
        } else {
            break;
        }
    }
    neighbors.clamp(1, MAX_NEIGHBORS)
}

/// Scale for a signed distance (in index units) from the visual center.
///
/// Selected is enlarged; neighbors keep shrinking past `|d| = 1`.
/// Past `|d| = 2` (horizontal outer peeks only — vertical never exceeds 2)
/// cards keep shrinking to a smaller floor so they recede instead of looming.
pub fn scale_at_distance(distance: f32) -> f32 {
    let d = distance.abs();
    if d <= 1.0 {
        SELECTED_SCALE + (SCALE_AT_1 - SELECTED_SCALE) * d
    } else if d <= 2.0 {
        SCALE_AT_1 + (SCALE_AT_2 - SCALE_AT_1) * (d - 1.0)
    } else {
        (SCALE_AT_2 - 0.08 * (d - 2.0)).max(0.34)
    }
}

/// Opacity for a signed distance from the visual center.
///
/// Matches the vertical falloff through `|d| = 2`, then keeps fading so
/// horizontal outer peeks recede instead of sitting at the floor.
pub fn opacity_at_distance(distance: f32) -> f32 {
    let d = distance.abs();
    if d <= 2.0 {
        (1.0 - 0.45 * d).clamp(0.18, 1.0)
    } else {
        (0.18 - 0.05 * (d - 2.0)).clamp(0.0, 0.18)
    }
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

/// Layout box for a horizontal strip child at `distance`.
///
/// Width is fixed (selected art width) so the scroll grid stays stable while
/// art scales. The in-slot text reserve fades in with `|distance|`: the
/// selection shows art only (its 39px title lives in the separate full-width
/// layer below the strip), immediate neighbors carry the full slim in-slot
/// text (neighbor 22px titles + meta), outer peeks keep the neighbor budget
/// so the grid never shifts. Weighting by `|d|` keeps the height continuous
/// mid-scroll instead of popping 100px at the halfway point (see
/// `strip_slot_stacked`: art-only slots use zero inter-block spacing to
/// match the art-only child height exactly).
pub fn slot_child_size_h(metrics: &StripMetrics, distance: f32) -> Size {
    let scale = scale_at_distance(distance);
    let text_w = distance.abs().clamp(0.0, 1.0);
    Size::new(
        metrics.center_w * metrics.selected_scale,
        metrics.center_h * scale + (H_TEXT_GAP + H_TEXT_RESERVE) * text_w,
    )
}

/// Catalog index for a visible slot, or `None` when empty / out of range.
///
/// Linear only — Games strip does not wrap.
pub fn slot_catalog_index(selected: usize, delta: isize, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let idx = selected as isize + delta;
    if idx < 0 || idx >= len as isize {
        None
    } else {
        Some(idx as usize)
    }
}

/// Catalog index and signed distance for a strip slot anchored on `visual`.
///
/// Linear: `floor(visual) + delta` must fall in `[0, len)`. Out-of-range peeks
/// are omitted (no duplicate wrap titles, no skeleton fillers).
pub fn visual_slot(visual: f32, delta: isize, len: usize) -> Option<(usize, f32)> {
    if len == 0 {
        return None;
    }
    let base = visual.floor();
    let frac = visual - base;
    let idx = base as isize + delta;
    if idx < 0 || idx >= len as isize {
        return None;
    }
    let dist = delta as f32 - frac;
    Some((idx as usize, dist))
}

/// Eased fractional scroll between `from` and `to` indices.
pub fn strip_scroll(from: f32, to: f32, started: Instant, now: Instant, duration_ms: u64) -> f32 {
    let duration = Duration::from_millis(duration_ms.max(1));
    let t = (now.saturating_duration_since(started).as_secs_f32() / duration.as_secs_f32())
        .clamp(0.0, 1.0);
    let e = ease_out_cubic(t);
    from + (to - from) * e
}

/// Eased 0..=1 progress of the selected-title crossfade (same ease as the strip scroll).
pub fn title_crossfade_t(elapsed_ms: u64, duration_ms: u64) -> f32 {
    ease_out_cubic((elapsed_ms as f32 / duration_ms.max(1) as f32).clamp(0.0, 1.0))
}

/// (outgoing, incoming) title opacities at eased progress `t`: old fades out
/// while new fades in.
pub fn title_crossfade_opacity(t: f32) -> (f32, f32) {
    let t = t.clamp(0.0, 1.0);
    (1.0 - t, t)
}

/// (outgoing, incoming) title X drift at eased progress `t`: the incoming
/// title slides in from the scroll direction while the outgoing drifts away.
/// `dir` is -1 | 0 | 1 (0 = fade only, no drift).
pub fn title_crossfade_drift(t: f32, dir: f32) -> (f32, f32) {
    let t = t.clamp(0.0, 1.0);
    // NB: f32::signum maps +0.0 to 1.0, so zero needs an explicit guard.
    let d = if dir == 0.0 {
        0.0
    } else {
        dir.signum().clamp(-1.0, 1.0)
    };
    (-d * TITLE_DRIFT_PX * t, d * TITLE_DRIFT_PX * (1.0 - t))
}

pub struct VStrip<'a, Message, Theme = iced::Theme, Renderer = iced::Renderer> {
    metrics: StripMetrics,
    orientation: StripOrientation,
    width: Length,
    height: Length,
    /// Signed distance from visual center for each child (parallel to `items`).
    distances: Vec<f32>,
    /// Real catalog rows only (sparse; OOB peeks omitted).
    items: Vec<Element<'a, Message, Theme, Renderer>>,
}

pub fn vstrip<'a, Message, Theme, Renderer>(
    metrics: StripMetrics,
    items: impl IntoIterator<Item = (f32, Element<'a, Message, Theme, Renderer>)>,
) -> VStrip<'a, Message, Theme, Renderer> {
    let (distances, items): (Vec<f32>, Vec<_>) = items.into_iter().unzip();
    VStrip {
        metrics,
        orientation: StripOrientation::Vertical,
        width: Length::Fill,
        height: Length::Fill,
        distances,
        items,
    }
}

impl<'a, Message, Theme, Renderer> VStrip<'a, Message, Theme, Renderer> {
    pub fn orientation(mut self, orientation: StripOrientation) -> Self {
        self.orientation = orientation;
        self
    }

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

        let mut children = Vec::with_capacity(self.items.len());
        for (i, (item, child_tree)) in self
            .items
            .iter_mut()
            .zip(tree.children.iter_mut())
            .enumerate()
        {
            let dist = self.distances.get(i).copied().unwrap_or(0.0);
            let child_size = match self.orientation {
                StripOrientation::Vertical => slot_child_size(&self.metrics, dist),
                StripOrientation::Horizontal => slot_child_size_h(&self.metrics, dist),
            };
            let child_limits = layout::Limits::new(Size::ZERO, child_size);
            let mut node = item
                .as_widget_mut()
                .layout(child_tree, renderer, &child_limits);
            let pos = match self.orientation {
                StripOrientation::Vertical => {
                    // Selected stays left-aligned; neighbors drift right with distance (arc).
                    let x = arc_offset_x(dist);
                    let y = size.height * 0.5 + slot_y_offset(dist, self.metrics.center_h)
                        - child_size.height * 0.5;
                    iced::Point::new(x, y)
                }
                StripOrientation::Horizontal => {
                    // All items share a common vertical center; neighbors lean
                    // down with distance (arc), mirroring the vertical drift.
                    let x = size.width * 0.5 + slot_x_offset(dist, self.metrics.center_w)
                        - child_size.width * 0.5;
                    let y = size.height * 0.5 + arc_offset_x(dist) - child_size.height * 0.5;
                    iced::Point::new(x, y)
                }
            };
            node = node.move_to(pos);
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
            let da = self.distances.get(a).copied().unwrap_or(0.0).abs();
            let db = self.distances.get(b).copied().unwrap_or(0.0).abs();
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
        // Outer horizontal peeks keep fading past the vertical floor.
        assert!((opacity_at_distance(3.0) - 0.13).abs() < 0.001);
        assert!(opacity_at_distance(4.0) < opacity_at_distance(3.0));
        assert!(opacity_at_distance(5.0) >= 0.0);
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
    fn horizontal_metrics_come_from_vertical_room() {
        // 1080p: selected art + separate title layer fits with top clearance
        // + rail + footer reserves. The strip holds art only for the
        // selection (no in-slot padding), so the title sits one text gap
        // below the art; neighbors still need art + text and the strip takes
        // the max so small stages never clip in-slot titles.
        let h = metrics_for_width(1920.0, 1080.0);
        assert!(h.center_h < CENTER_H_MAX && h.center_h > CENTER_H_MIN);
        assert!((h.slot_w - h.center_w * SELECTED_SCALE).abs() < 0.01);
        let selected_art = h.center_h * SELECTED_SCALE;
        let neighbor_full = h.center_h * SCALE_AT_1 + H_TEXT_GAP + H_TEXT_RESERVE;
        assert!((h.slot_h - selected_art.max(neighbor_full)).abs() < 0.01);
        // 1080p: selected art dominates, so the strip is art-only.
        assert!((h.slot_h - selected_art).abs() < 0.01);
        let block_h = h.slot_h + H_STRIP_TO_TITLE_GAP + H_SELECTED_TITLE_RESERVE;
        assert!(block_h + H_RAIL_RESERVE + H_FOOTER_RESERVE <= 1080.0 + 0.01);
        // Short stage: capsules shrink instead of overflowing the rail; the
        // strip takes the neighbor max so in-slot titles never clip.
        let short = metrics_for_width(1280.0, 720.0);
        assert!(short.center_h <= h.center_h);
        assert!(short.center_h >= CENTER_H_MIN);
        let short_selected = short.center_h * SELECTED_SCALE;
        let short_neighbor = short.center_h * SCALE_AT_1 + H_TEXT_GAP + H_TEXT_RESERVE;
        assert!((short.slot_h - short_selected.max(short_neighbor)).abs() < 0.01);
        let short_block = short.slot_h + H_STRIP_TO_TITLE_GAP + H_SELECTED_TITLE_RESERVE;
        assert!(short_block + H_RAIL_RESERVE + H_FOOTER_RESERVE <= 720.0 + 0.01);
        // Tall stage: clamped, never blows up on large monitors.
        let tall = metrics_for_width(2560.0, 2160.0);
        assert!((tall.center_h - CENTER_H_MAX).abs() < 0.001);
    }

    #[test]
    fn neighbors_grow_with_stage_width() {
        let m = metrics_for_width(1920.0, 1080.0);
        let narrow = neighbors_for_width(800.0, m.center_w);
        let wide = neighbors_for_width(1920.0, m.center_w);
        let ultrawide = neighbors_for_width(2560.0, m.center_w);
        assert!((1..=2).contains(&narrow), "narrow={narrow}");
        assert!((3..=MAX_NEIGHBORS).contains(&wide), "wide={wide}");
        assert!(ultrawide >= wide, "ultra={ultrawide} wide={wide}");
        assert!(ultrawide <= MAX_NEIGHBORS);
        // Monotonic in width.
        let mut prev = 0;
        for w in [640.0, 960.0, 1280.0, 1920.0, 2560.0, 3440.0] {
            let n = neighbors_for_width(w, m.center_w);
            assert!(n >= prev, "w={w} n={n} prev={prev}");
            prev = n;
        }
    }

    #[test]
    fn horizontal_child_boxes_share_a_fixed_width() {
        let m = metrics_for_width(1920.0, 1080.0);
        let max_w = m.center_w * SELECTED_SCALE;
        let text_full = H_TEXT_GAP + H_TEXT_RESERVE;
        for d in [0.0_f32, 1.0, 2.0, 3.0, 4.0] {
            let child = slot_child_size_h(&m, d);
            assert!((child.width - max_w).abs() < 0.01, "d={d}");
            // In-slot text fades in with |d|: art-only at the selection,
            // full neighbor budget from |d| >= 1 (continuous mid-scroll).
            let w = d.abs().clamp(0.0, 1.0);
            let art_h = m.center_h * scale_at_distance(d);
            assert!(
                (child.height - (art_h + text_full * w)).abs() < 0.01,
                "d={d}"
            );
        }
        // Selection is art-only and exactly fills the art-only strip at 1080p.
        let art0 = m.center_h * SELECTED_SCALE;
        assert!((slot_child_size_h(&m, 0.0).height - art0).abs() < 0.01);
        assert!((m.slot_h - art0).abs() < 0.01);
        // Mid-scroll stays continuous (no 100px pop at the halfway point).
        let mid = slot_child_size_h(&m, 0.5).height;
        let end0 = slot_child_size_h(&m, 0.0).height;
        let end1 = slot_child_size_h(&m, 1.0).height;
        let (lo, hi) = if end0 < end1 {
            (end0, end1)
        } else {
            (end1, end0)
        };
        assert!(mid > lo && mid < hi, "mid={mid} lo={lo} hi={hi}");
    }

    #[test]
    fn outer_scale_keeps_shrinking_past_two() {
        assert!(scale_at_distance(3.0) < SCALE_AT_2);
        assert!(scale_at_distance(4.0) < scale_at_distance(3.0));
        assert!(scale_at_distance(5.0) >= 0.34);
        // Vertical range unchanged.
        assert!((scale_at_distance(0.0) - SELECTED_SCALE).abs() < 0.001);
        assert!((scale_at_distance(2.0) - SCALE_AT_2).abs() < 0.001);
    }

    #[test]
    fn slot_x_offset_mirrors_y() {
        let w = 240.0;
        assert!(slot_x_offset(0.0, w).abs() < 1e-4);
        assert!((slot_x_offset(2.0, w) - slot_y_offset(2.0, w)).abs() < 0.01);
        assert!((slot_x_offset(-1.5, w) - slot_y_offset(-1.5, w)).abs() < 0.01);
        assert_eq!(StripOrientation::default(), StripOrientation::Vertical);
    }

    #[test]
    fn metrics_clamp_extreme_heights() {
        let tiny = metrics_for_height(400.0);
        let huge = metrics_for_height(4000.0);
        assert!((tiny.center_h - CENTER_H_MIN).abs() < 0.001);
        assert!((huge.center_h - CENTER_H_MAX).abs() < 0.001);
    }

    #[test]
    fn slot_catalog_index_linear_oob() {
        assert_eq!(slot_catalog_index(0, 0, 0), None);
        assert_eq!(slot_catalog_index(0, -1, 3), None);
        assert_eq!(slot_catalog_index(0, 0, 3), Some(0));
        assert_eq!(slot_catalog_index(0, 2, 3), Some(2));
        assert_eq!(slot_catalog_index(0, 3, 3), None);
        assert_eq!(slot_catalog_index(2, 1, 3), None);
        assert_eq!(slot_catalog_index(2, -1, 3), Some(1));
    }

    #[test]
    fn visual_slot_linear_skips_oob_no_duplicates() {
        let slots: Vec<_> = (-NEIGHBORS..=NEIGHBORS)
            .filter_map(|d| visual_slot(0.0, d, 3))
            .collect();
        assert_eq!(slots.len(), 3);
        assert_eq!(slots[0], (0, 0.0));
        assert_eq!(slots[1].0, 1);
        assert_eq!(slots[2].0, 2);
        // No wrap duplicates above the first item.
        assert!(visual_slot(0.0, -1, 3).is_none());
        assert!(visual_slot(2.0, 1, 3).is_none());
        // Mid-scroll distances stay continuous for in-range rows.
        let (idx, dist) = visual_slot(1.5, 0, 3).unwrap();
        assert_eq!(idx, 1);
        assert!((dist - (-0.5)).abs() < 0.001);
        let (idx, dist) = visual_slot(1.5, 1, 3).unwrap();
        assert_eq!(idx, 2);
        assert!((dist - 0.5).abs() < 0.001);
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
    fn title_crossfade_endpoints_and_ease() {
        let (old0, new0) = title_crossfade_opacity(title_crossfade_t(0, STRIP_ANIM_MS));
        assert!((old0 - 1.0).abs() < 0.001 && new0.abs() < 0.001);
        let (old1, new1) = title_crossfade_opacity(title_crossfade_t(STRIP_ANIM_MS, STRIP_ANIM_MS));
        assert!(old1.abs() < 0.001 && (new1 - 1.0).abs() < 0.001);
        // Ease-out runs ahead of linear at midpoint (matches the strip scroll feel).
        let mid = title_crossfade_t(STRIP_ANIM_MS / 2, STRIP_ANIM_MS);
        assert!(mid > 0.5 && mid < 1.0);
        let (old_m, new_m) = title_crossfade_opacity(mid);
        assert!((old_m + new_m - 1.0).abs() < 0.001);
        assert!(new_m > 0.5);
        // Past the end clamps (tick settles, but sampling must stay sane).
        assert!((title_crossfade_t(STRIP_ANIM_MS * 4, STRIP_ANIM_MS) - 1.0).abs() < 0.001);
        let (old_c, new_c) = title_crossfade_opacity(2.0);
        assert!(old_c.abs() < 0.001 && (new_c - 1.0).abs() < 0.001);
    }

    #[test]
    fn title_drift_follows_scroll_direction() {
        let t = 0.5;
        let (old_dx, new_dx) = title_crossfade_drift(t, 1.0);
        assert!(old_dx < 0.0 && new_dx > 0.0);
        assert!((old_dx.abs() + new_dx.abs() - TITLE_DRIFT_PX).abs() < 0.01);
        let (old_dx, new_dx) = title_crossfade_drift(t, -1.0);
        assert!(old_dx > 0.0 && new_dx < 0.0);
        // Fade-only when direction is unknown.
        let (old_dx, new_dx) = title_crossfade_drift(t, 0.0);
        assert!(old_dx.abs() < 0.001 && new_dx.abs() < 0.001);
        // Incoming settles at zero drift (no resting offset); outgoing has left.
        let (old_dx, new_dx) = title_crossfade_drift(1.0, 1.0);
        assert!(old_dx.abs() > 0.0 && new_dx.abs() < 0.001);
        let (old_dx, new_dx) = title_crossfade_drift(0.0, 1.0);
        assert!(old_dx.abs() < 0.001 && (new_dx - TITLE_DRIFT_PX).abs() < 0.001);
    }

    #[test]
    fn selected_title_gap_matches_in_slot_text_gap() {
        // The full-width selected title must sit as tight to the art row as
        // neighbor in-slot titles do (strip spacing == in-slot art→text gap).
        assert!((H_STRIP_TO_TITLE_GAP - H_TEXT_GAP).abs() < 0.001);
    }

    #[test]
    fn selected_title_sits_one_gap_below_art_at_1080p() {
        // Regression: the strip→title gap constant was always 8px, but the
        // selected art bottom sat ~108px above the title top because the
        // strip container carried the neighbor in-slot text reserve while the
        // selected slot showed art only (plus centering slack when meta was
        // absent). Pin the full stack: art bottom → title text top ≈ one gap.
        let m = metrics_for_width(1920.0, 1080.0);
        let selected_art = m.center_h * SELECTED_SCALE;
        // Strip is art-only at 1080p (no in-slot padding under the selection).
        assert!(
            (m.slot_h - selected_art).abs() < 0.01,
            "slot_h={}",
            m.slot_h
        );
        assert!(
            (slot_child_size_h(&m, 0.0).height - selected_art).abs() < 0.01,
            "child0={}",
            slot_child_size_h(&m, 0.0).height
        );
        // Selected child top-aligns its art (art-only slot uses zero
        // inter-block spacing), the title layer top-aligns its text, so the
        // visual gap is exactly strip remainder + strip→title gap.
        let art_bottom_to_title_top = (m.slot_h - selected_art) + H_STRIP_TO_TITLE_GAP;
        assert!(
            (art_bottom_to_title_top - H_TEXT_GAP).abs() < 0.01,
            "gap={art_bottom_to_title_top}"
        );
        // Neighbors keep the same one-gap relationship in-slot.
        let neighbor_art = m.center_h * SCALE_AT_1;
        let neighbor_child = slot_child_size_h(&m, 1.0).height;
        assert!(
            (neighbor_child - (neighbor_art + H_TEXT_GAP + H_TEXT_RESERVE)).abs() < 0.01,
            "neighbor={neighbor_child}"
        );
        // Title reserve still fits the measured worst case (39px title + two
        // 16px meta lines + badge) so meta presence never moves the title top.
        let title_lines = 39.0 * 1.3 + 6.0 + (16.0 * 1.3 * 2.0 + 2.0) + 6.0 + 25.0;
        assert!(
            H_SELECTED_TITLE_RESERVE >= title_lines - 0.5,
            "reserve={} need={title_lines}",
            H_SELECTED_TITLE_RESERVE
        );
    }

    #[test]
    fn horizontal_block_lead_drops_art_row_without_eating_rail_gap() {
        // 1080p: the art-only strip (no in-slot padding under the selection)
        // recenters the ART ROW mid-region; the title follows one gap below
        // the art and the rail keeps at least its minimum gap.
        let m = metrics_for_width(1920.0, 1080.0);
        let block_h = m.slot_h + H_STRIP_TO_TITLE_GAP + H_SELECTED_TITLE_RESERVE;
        // Layout region above the rail: stage minus rail height + footer pad.
        let region_h = 1080.0 - 120.0 - 64.0;
        let lead = horizontal_block_lead(region_h, block_h, 44.0, H_TITLE_TO_BAR_MIN);
        // block = 470 (art-only) + 8 + 132 = 610; slack = 224; lead = 226.
        assert!((block_h - 610.0).abs() < 1.0, "block={block_h}");
        assert!((lead - 226.0).abs() < 1.0, "lead={lead}");
        // Rail gap never eaten (now roomier than the 18px minimum because the
        // shrunken block leaves real slack).
        let rail_gap = region_h - lead - block_h;
        assert!(rail_gap >= H_TITLE_TO_BAR_MIN - 0.01, "gap={rail_gap}");
        // Art bottom → title top is one text gap (title top-aligns, no slack).
        let art_h = m.center_h * m.selected_scale;
        let art_bottom = lead + art_h;
        let title_top = lead + m.slot_h + H_STRIP_TO_TITLE_GAP;
        assert!(
            ((title_top - art_bottom) - H_TEXT_GAP).abs() < 0.01,
            "title_top={title_top} art_bottom={art_bottom}"
        );
        // Art row (not block) lands mid-region.
        let art_center = lead + art_h * 0.5;
        assert!(
            (art_center - region_h * 0.5).abs() < 20.0,
            "art_center={art_center}"
        );
    }

    #[test]
    fn horizontal_block_lead_centers_art_on_tall_stages() {
        // Clamped capsules leave real slack: the full offset applies and the
        // art center lands mid-region (within top-pad/rail asymmetry).
        let m = metrics_for_width(2560.0, 2160.0);
        let block_h = m.slot_h + H_STRIP_TO_TITLE_GAP + H_SELECTED_TITLE_RESERVE;
        let region_h = 2160.0 - 120.0 - 64.0;
        let lead = horizontal_block_lead(region_h, block_h, 44.0, H_TITLE_TO_BAR_MIN);
        let art_h = m.center_h * m.selected_scale;
        assert!(
            (lead + art_h * 0.5 - region_h * 0.5).abs() < 16.0,
            "lead={lead} art_h={art_h}"
        );
        assert!(region_h - lead - block_h >= H_TITLE_TO_BAR_MIN - 0.01);
    }

    #[test]
    fn horizontal_block_lead_clamps_on_short_stages() {
        // Block taller than the region: lead floors at the clock pad (never negative).
        let lead = horizontal_block_lead(536.0, 519.6, 44.0, H_TITLE_TO_BAR_MIN);
        assert!((lead - 44.0).abs() < 0.001, "lead={lead}");
        // Degenerate regions cannot panic or go negative.
        assert!(horizontal_block_lead(0.0, 700.0, 44.0, 18.0) >= 44.0 - 0.001);
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
