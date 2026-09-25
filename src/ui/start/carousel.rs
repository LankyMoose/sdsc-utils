//! Horizontal two-pane carousel: translate only, clipped to list bounds.

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell, overlay};
use iced::mouse;
use iced::window;
use iced::{Element, Event, Length, Rectangle, Size, Transformation, Vector};

/// Dual-pane carousel. Both panes layout at full size; `scroll` translates them
/// horizontally. Drawing and hit-testing clip to the carousel bounds (not the window).
pub struct Carousel<'a, Message, Theme = iced::Theme, Renderer = iced::Renderer> {
    scroll: f32,
    pane_w: f32,
    width: Length,
    height: Length,
    first: Element<'a, Message, Theme, Renderer>,
    second: Element<'a, Message, Theme, Renderer>,
}

pub fn carousel<'a, Message, Theme, Renderer>(
    scroll: f32,
    pane_w: f32,
    first: impl Into<Element<'a, Message, Theme, Renderer>>,
    second: impl Into<Element<'a, Message, Theme, Renderer>>,
) -> Carousel<'a, Message, Theme, Renderer> {
    Carousel {
        scroll,
        pane_w,
        width: Length::Fill,
        height: Length::Fill,
        first: first.into(),
        second: second.into(),
    }
}

impl<'a, Message, Theme, Renderer> Carousel<'a, Message, Theme, Renderer> {
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.height = height.into();
        self
    }

    fn offsets(&self) -> (f32, f32) {
        (-self.scroll, self.pane_w - self.scroll)
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for Carousel<'_, Message, Theme, Renderer>
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
        vec![Tree::new(&self.first), Tree::new(&self.second)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&[&self.first, &self.second]);
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
        let child_limits = layout::Limits::new(Size::ZERO, size);

        let first =
            self.first
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, &child_limits);
        let second =
            self.second
                .as_widget_mut()
                .layout(&mut tree.children[1], renderer, &child_limits);

        layout::Node::with_children(size, vec![first, second])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            let mut children = layout.children();
            let Some(first_layout) = children.next() else {
                return;
            };
            let Some(second_layout) = children.next() else {
                return;
            };
            self.first.as_widget_mut().operate(
                &mut tree.children[0],
                first_layout,
                renderer,
                operation,
            );
            self.second.as_widget_mut().operate(
                &mut tree.children[1],
                second_layout,
                renderer,
                operation,
            );
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
        let Some(clipped) = bounds.intersection(viewport) else {
            return;
        };
        // Redraw must reach scrollables so on_scroll can report viewport size even when
        // the cursor is elsewhere (controller / keyboard navigation).
        let is_redraw = matches!(event, Event::Window(window::Event::RedrawRequested(_)));
        if !is_redraw && !cursor.is_over(bounds) {
            return;
        }

        let (tx0, tx1) = self.offsets();
        let mut children = layout.children();
        let Some(first_layout) = children.next() else {
            return;
        };
        let Some(second_layout) = children.next() else {
            return;
        };

        let [first_state, second_state] = tree.children.as_mut_slice() else {
            return;
        };

        // Front-most first: second pane, then first. On redraw, visit visible panes
        // regardless of cursor so scrollables can notify viewport.
        let panes = [
            (&mut self.second, second_state, second_layout, tx1),
            (&mut self.first, first_state, first_layout, tx0),
        ];
        for (child, state, child_layout, tx) in panes {
            let drawn = translate_rect(child_layout.bounds(), tx);
            let hit = match drawn.intersection(&bounds) {
                Some(r) if r.width > 0.0 && r.height > 0.0 => r,
                _ => continue,
            };
            if !is_redraw && !cursor.is_over(hit) {
                continue;
            }
            let transform = Transformation::translate(tx, 0.0);
            let inverse = transform.inverse();
            child.as_widget_mut().update(
                state,
                event,
                child_layout,
                cursor * inverse,
                renderer,
                clipboard,
                shell,
                &(clipped * inverse),
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
        let bounds = layout.bounds();
        if !cursor.is_over(bounds) {
            return mouse::Interaction::None;
        }
        let Some(clipped) = bounds.intersection(viewport) else {
            return mouse::Interaction::None;
        };

        let (tx0, tx1) = self.offsets();
        let mut children = layout.children();
        let Some(first_layout) = children.next() else {
            return mouse::Interaction::None;
        };
        let Some(second_layout) = children.next() else {
            return mouse::Interaction::None;
        };

        for (child, state, child_layout, tx) in [
            (&self.second, &tree.children[1], second_layout, tx1),
            (&self.first, &tree.children[0], first_layout, tx0),
        ] {
            let drawn = translate_rect(child_layout.bounds(), tx);
            let hit = match drawn.intersection(&bounds) {
                Some(r) if r.width > 0.0 && r.height > 0.0 => r,
                _ => continue,
            };
            if !cursor.is_over(hit) {
                continue;
            }
            let transform = Transformation::translate(tx, 0.0);
            let inverse = transform.inverse();
            let interaction = child.as_widget().mouse_interaction(
                state,
                child_layout,
                cursor * inverse,
                &(clipped * inverse),
                renderer,
            );
            if interaction != mouse::Interaction::None {
                return interaction;
            }
        }
        mouse::Interaction::None
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

        let (tx0, tx1) = self.offsets();
        let mut children = layout.children();
        let Some(first_layout) = children.next() else {
            return;
        };
        let Some(second_layout) = children.next() else {
            return;
        };

        renderer.with_layer(clipped, |renderer| {
            for (child, state, child_layout, tx) in [
                (&self.first, &tree.children[0], first_layout, tx0),
                (&self.second, &tree.children[1], second_layout, tx1),
            ] {
                let transform = Transformation::translate(tx, 0.0);
                let inverse = transform.inverse();
                renderer.with_transformation(transform, |renderer| {
                    child.as_widget().draw(
                        state,
                        renderer,
                        theme,
                        style,
                        child_layout,
                        cursor * inverse,
                        &(clipped * inverse),
                    );
                });
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
        let (tx0, tx1) = self.offsets();
        let mut children = layout.children();
        let first_layout = children.next()?;
        let second_layout = children.next()?;

        let [first_state, second_state] = tree.children.as_mut_slice() else {
            return None;
        };

        let first = self.first.as_widget_mut().overlay(
            first_state,
            first_layout,
            renderer,
            viewport,
            translation + Vector::new(tx0, 0.0),
        );
        let second = self.second.as_widget_mut().overlay(
            second_state,
            second_layout,
            renderer,
            viewport,
            translation + Vector::new(tx1, 0.0),
        );

        match (first, second) {
            (None, None) => None,
            (Some(a), None) | (None, Some(a)) => Some(a),
            (Some(a), Some(b)) => Some(overlay::Element::new(Box::new(
                overlay::Group::with_children(vec![a, b]),
            ))),
        }
    }
}

impl<'a, Message, Theme, Renderer> From<Carousel<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(value: Carousel<'a, Message, Theme, Renderer>) -> Self {
        Element::new(value)
    }
}

fn translate_rect(rect: Rectangle, tx: f32) -> Rectangle {
    Rectangle {
        x: rect.x + tx,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}
