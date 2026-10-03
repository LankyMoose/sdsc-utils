//! Vertical cover-flow strip: selection locked to center with scale falloff.

use crate::ui::layout::ease_out_cubic;
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell, overlay};
use iced::mouse;
use iced::{Element, Event, Length, Rectangle, Size, Vector};
use std::time::{Duration, Instant};

/// Visible slots above/below the center selection.
pub const NEIGHBORS: isize = 3;
pub const VISIBLE: usize = (NEIGHBORS * 2 + 1) as usize;
pub const STRIP_ANIM_MS: u64 = 200;
/// Shorter retarget when the strip is already mid-scroll (rapid pad/keyboard steps).
pub const STRIP_ANIM_CATCHUP_MS: u64 = 110;

/// Capsule art size inside each strip slot (neighbor / base size).
pub const CENTER_W: f32 = 220.0;
pub const CENTER_H: f32 = 330.0;
/// Selected hero scale relative to [`CENTER_W`] / [`CENTER_H`].
pub const SELECTED_SCALE: f32 = 1.33;
/// Neighbor title + subtitle column width.
pub const TITLE_COL: f32 = 480.0;
/// Full slot room for selected art width + title column.
pub const SLOT_W: f32 = CENTER_W * SELECTED_SCALE + TITLE_COL;
pub const SLOT_H: f32 = CENTER_H;
/// Vertical pitch: clears enlarged selected (~439h) plus ~20px gap.
pub const STRIDE: f32 = 410.0;

/// Scale for a signed distance (in index units) from the visual center.
/// Selected is ~33% larger; neighbors return to 1.0 by |d| = 1.
pub fn scale_at_distance(distance: f32) -> f32 {
    let d = distance.abs();
    if d >= 1.0 {
        1.0
    } else {
        SELECTED_SCALE + (1.0 - SELECTED_SCALE) * d
    }
}

/// Opacity for a signed distance from the visual center (next≈0.7, next+1≈0.4, …).
pub fn opacity_at_distance(distance: f32) -> f32 {
    let d = distance.abs();
    (1.0 - 0.3 * d).clamp(0.25, 1.0)
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
    width: Length,
    height: Length,
    /// Children ordered top→bottom for the visible window (length [`VISIBLE`]).
    items: Vec<Element<'a, Message, Theme, Renderer>>,
}

pub fn vstrip<'a, Message, Theme, Renderer>(
    visual_scroll: f32,
    anchor: usize,
    items: impl IntoIterator<Item = Element<'a, Message, Theme, Renderer>>,
) -> VStrip<'a, Message, Theme, Renderer> {
    VStrip {
        visual_scroll,
        anchor,
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
        let slot = Size::new(SLOT_W, SLOT_H);

        let mut children = Vec::with_capacity(self.items.len());
        for (i, (item, child_tree)) in self
            .items
            .iter_mut()
            .zip(tree.children.iter_mut())
            .enumerate()
        {
            let dist = slot_distance(i, self.anchor, self.visual_scroll);
            let scale = scale_at_distance(dist);
            let child_size = Size::new(slot.width * scale, slot.height * scale);
            let child_limits = layout::Limits::new(Size::ZERO, child_size);
            let mut node = item
                .as_widget_mut()
                .layout(child_tree, renderer, &child_limits);
            // Left-align so scale changes do not slide heroes on the X axis.
            let x = 0.0;
            let y = size.height * 0.5 + dist * STRIDE - child_size.height * 0.5;
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
        assert!((scale_at_distance(1.0) - 1.0).abs() < 0.001);
        assert!((scale_at_distance(2.0) - 1.0).abs() < 0.001);
        assert!(scale_at_distance(0.5) > 1.0 && scale_at_distance(0.5) < SELECTED_SCALE);
    }

    #[test]
    fn opacity_steps_down_by_third() {
        assert!((opacity_at_distance(0.0) - 1.0).abs() < 0.001);
        assert!((opacity_at_distance(1.0) - 0.7).abs() < 0.001);
        assert!((opacity_at_distance(2.0) - 0.4).abs() < 0.001);
    }

    #[test]
    fn stride_clears_selected_with_gap() {
        let selected_h = CENTER_H * SELECTED_SCALE;
        let neighbor_h = CENTER_H;
        let clearance = selected_h * 0.5 + neighbor_h * 0.5;
        assert!(STRIDE > clearance + 16.0);
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
}
