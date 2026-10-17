//! Horizontal width reveal: child lays out at full content width; host scissors paint.

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell, overlay};
use iced::mouse;
use iced::window;
use iced::{Element, Event, Length, Rectangle, Size, Vector};

/// Host width animates (`reveal`); child always layouts at [`Self::content_width`].
pub struct WidthReveal<'a, Message, Theme = iced::Theme, Renderer = iced::Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    content_width: f32,
    reveal: f32,
    width: Length,
    height: Length,
}

pub fn width_reveal<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    content_width: f32,
    reveal: f32,
) -> WidthReveal<'a, Message, Theme, Renderer> {
    WidthReveal {
        content: content.into(),
        content_width: content_width.max(0.0),
        reveal: reveal.max(0.0),
        width: Length::Fixed(reveal.max(0.0)),
        height: Length::Fill,
    }
}

impl<'a, Message, Theme, Renderer> WidthReveal<'a, Message, Theme, Renderer> {
    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.height = height.into();
        self
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for WidthReveal<'_, Message, Theme, Renderer>
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
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&[&self.content]);
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
        let reveal = self.reveal.min(self.content_width);

        // Shrink: measure content first (Fill height would resolve to 0 with no definite parent).
        if matches!(self.height, Length::Shrink) {
            let child_limits = layout::Limits::new(
                Size::new(self.content_width, 0.0),
                Size::new(self.content_width, limits.max().height),
            );
            let child =
                self.content
                    .as_widget_mut()
                    .layout(&mut tree.children[0], renderer, &child_limits);
            let host = Size::new(reveal, child.size().height);
            return layout::Node::with_children(host, vec![child]);
        }

        let host_limits = limits.width(Length::Fixed(reveal)).height(self.height);
        let host = host_limits.resolve(Length::Fixed(reveal), self.height, Size::ZERO);

        // Force exact content width (min = max) so Fill children never shrink-to-fit.
        let child_limits = layout::Limits::new(
            Size::new(self.content_width, 0.0),
            Size::new(self.content_width, host.height),
        );
        let child =
            self.content
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, &child_limits);

        layout::Node::with_children(host, vec![child])
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
            let Some(child_layout) = layout.children().next() else {
                return;
            };
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                child_layout,
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
        let is_redraw = matches!(event, Event::Window(window::Event::RedrawRequested(_)));
        if !is_redraw && !cursor.is_over(bounds) {
            return;
        }
        let Some(child_layout) = layout.children().next() else {
            return;
        };
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            child_layout,
            cursor,
            renderer,
            clipboard,
            shell,
            &clipped,
        );
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
        let Some(child_layout) = layout.children().next() else {
            return mouse::Interaction::None;
        };
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            child_layout,
            cursor,
            &clipped,
            renderer,
        )
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
        if bounds.width <= 0.0 || bounds.height <= 0.0 {
            return;
        }
        let Some(clipped) = bounds.intersection(viewport) else {
            return;
        };
        let Some(child_layout) = layout.children().next() else {
            return;
        };

        renderer.with_layer(clipped, |renderer| {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                child_layout,
                cursor,
                &clipped,
            );
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
        let child_layout = layout.children().next()?;
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            child_layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message, Theme, Renderer> From<WidthReveal<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(value: WidthReveal<'a, Message, Theme, Renderer>) -> Self {
        Element::new(value)
    }
}
