//! Visual-only scale wrapper: draw (and hit-test) a widget tree scaled
//! about its own center while its layout footprint stays exactly the child's.
//!
//! This exists because [`iced::widget::Float`] cannot do the job: its scale
//! only applies above `1.0` (anything at or below is silently ignored), and
//! while floating it drops input and repaints above every sibling. `Scale`
//! instead draws inline in tree order with the renderer's graphics
//! transform, so stacking, clipping, and input keep working.
//!
//! Used for dialog enter/exit tweens (see
//! [`crate::ui::start::overlay`]): layout reserves the full-size card for
//! the whole tween while only the paint scales, so neighbors never reflow.
//!
//! Contract:
//!
//! * `layout`, `size`, and `size_hint` delegate to the child unchanged —
//!   the footprint is identical at every scale (unit-tested below).
//! * `draw` applies a center-anchored scale via the renderer transform.
//!   At `>= 0.999` it draws undecorated (exact path, no overhead).
//! * `update` / `mouse_interaction` map the cursor through the inverse
//!   transform, so hits follow the visual. Near zero the inverse maps the
//!   cursor away, so an effectively invisible widget is unhittable.
//! * Scale clamps to `[0.01, 1.0]`: growing past full size is not supported
//!   (dialogs settle at identity), and `0.01` keeps the inverse non-singular.

use iced::advanced::widget::tree;
use iced::advanced::{Clipboard, Layout, Shell};
use iced::advanced::{layout, mouse, overlay, renderer, widget};
use iced::{Element, Event, Length, Rectangle, Size, Transformation, Vector};

/// Smallest representable scale: visually zero while keeping the graphics
/// transform invertible.
const MIN_SCALE: f32 = 0.01;
/// At or above this the widget draws (and hit-tests) undecorated.
const IDENTITY_THRESHOLD: f32 = 0.999;

pub struct Scale<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    scale: f32,
}

impl<'a, Message, Theme, Renderer> Scale<'a, Message, Theme, Renderer> {
    /// Wrap `content`, drawn scaled about its own center. Layout (and the
    /// neighbors around it) always sees the full-size child.
    pub fn new(content: impl Into<Element<'a, Message, Theme, Renderer>>, scale: f32) -> Self {
        Self {
            content: content.into(),
            scale,
        }
    }

    fn effective_scale(&self) -> f32 {
        self.scale.clamp(MIN_SCALE, 1.0)
    }

    fn transformation(&self, bounds: Rectangle) -> Transformation {
        Transformation::translate(
            bounds.x + bounds.width / 2.0,
            bounds.y + bounds.height / 2.0,
        ) * Transformation::scale(self.effective_scale())
            * Transformation::translate(
                -bounds.x - bounds.width / 2.0,
                -bounds.y - bounds.height / 2.0,
            )
    }

    fn is_identity(&self) -> bool {
        self.scale >= IDENTITY_THRESHOLD
    }
}

impl<Message, Theme, Renderer> widget::Widget<Message, Theme, Renderer>
    for Scale<'_, Message, Theme, Renderer>
where
    Renderer: renderer::Renderer,
{
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<tree::Tree> {
        self.content.as_widget().children()
    }

    fn diff(&self, tree: &mut widget::Tree) {
        self.content.as_widget().diff(tree)
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(
        &mut self,
        tree: &mut widget::Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut widget::Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if self.is_identity() {
            self.content.as_widget_mut().update(
                tree, event, layout, cursor, renderer, clipboard, shell, viewport,
            );
            return;
        }
        let transformation = self.transformation(layout.bounds());
        let inverse = transformation.inverse();
        self.content.as_widget_mut().update(
            tree,
            event,
            layout,
            cursor * inverse,
            renderer,
            clipboard,
            shell,
            &(*viewport * inverse),
        )
    }

    fn draw(
        &self,
        tree: &widget::Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        if self.is_identity() {
            self.content
                .as_widget()
                .draw(tree, renderer, theme, style, layout, cursor, viewport);
            return;
        }
        let transformation = self.transformation(layout.bounds());
        let inverse = transformation.inverse();
        renderer.with_layer(*viewport, |renderer| {
            renderer.with_transformation(transformation, |renderer| {
                self.content.as_widget().draw(
                    tree,
                    renderer,
                    theme,
                    style,
                    layout,
                    cursor * inverse,
                    &(*viewport * inverse),
                );
            });
        });
    }

    fn mouse_interaction(
        &self,
        tree: &widget::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        if self.is_identity() {
            return self
                .content
                .as_widget()
                .mouse_interaction(tree, layout, cursor, viewport, renderer);
        }
        // Outside the shrunken visual there is nothing to hover.
        let visual = layout.bounds() * self.transformation(layout.bounds());
        if !cursor.is_over(visual) {
            return mouse::Interaction::None;
        }
        let inverse = self.transformation(layout.bounds()).inverse();
        self.content.as_widget().mouse_interaction(
            tree,
            layout,
            cursor * inverse,
            &(*viewport * inverse),
            renderer,
        )
    }

    fn operate(
        &mut self,
        tree: &mut widget::Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, operation);
    }

    fn overlay<'a>(
        &'a mut self,
        state: &'a mut widget::Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        offset: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        // Child overlays (if any) keep full-size coordinates; the card's own
        // draw path is transformed, and dialog cards carry no overlay widgets.
        self.content
            .as_widget_mut()
            .overlay(state, layout, renderer, viewport, offset)
    }
}

impl<'a, Message: 'a, Theme: 'a, Renderer: renderer::Renderer + 'a>
    From<Scale<'a, Message, Theme, Renderer>> for Element<'a, Message, Theme, Renderer>
{
    fn from(scale: Scale<'a, Message, Theme, Renderer>) -> Self {
        Self::new(scale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::widget::{container, text};

    fn plain() -> Element<'static, (), iced::Theme, iced::Renderer> {
        container(text("hello")).width(Length::Fixed(420.0)).into()
    }

    #[test]
    fn scale_preserves_layout_footprint_at_any_scale() {
        let expected = plain().as_widget().size();
        assert_eq!(expected.width, Length::Fixed(420.0));
        for scale in [0.0, 0.02, 0.5, 0.999, 1.0, 2.0] {
            let scaled: Element<'static, (), iced::Theme, iced::Renderer> =
                Scale::new(container(text("hello")).width(Length::Fixed(420.0)), scale).into();
            assert_eq!(
                scaled.as_widget().size(),
                expected,
                "footprint must not move with scale {scale}"
            );
            assert_eq!(
                scaled.as_widget().size_hint(),
                plain().as_widget().size_hint()
            );
        }
    }
}
