//! Debug automations must not react to the real keyboard or mouse.
//!
//! `SDSC_DEBUG_INPUT=0` already drops pad edges. This wrapper extends that
//! contract to every iced window while an automated/quiet session is active:
//! pointer, touch, and keyboard events are swallowed before any widget sees
//! them. Synthetic replay input bypasses the widget tree and calls the same
//! app handlers directly, so emulation keeps working with the lock on.

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell, overlay};
use iced::mouse;
use iced::{Element, Event, Rectangle, Size, Vector};

pub fn input_lock<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
) -> Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: renderer::Renderer + 'a,
{
    Element::new(InputLock {
        content: content.into(),
    })
}

/// True for the event kinds an automated session must never accept.
fn is_input_event(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(_) | Event::Touch(_) | Event::Keyboard(_)
    )
}

struct InputLock<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for InputLock<'_, Message, Theme, Renderer>
where
    Renderer: renderer::Renderer,
{
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }

    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }

    fn size(&self) -> Size<iced::Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<iced::Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, operation);
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
        if is_input_event(event) {
            // Keep clicks/wheel inside the locked window from activating a
            // backdrop or shell target behind it.
            let captures = matches!(
                event,
                Event::Mouse(mouse::Event::ButtonPressed(_))
                    | Event::Mouse(mouse::Event::WheelScrolled { .. })
                    | Event::Touch(iced::touch::Event::FingerPressed { .. })
            );
            if captures && cursor.is_over(layout.bounds()) {
                shell.capture_event();
            }
            return;
        }

        self.content.as_widget_mut().update(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
        );
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Idle
        } else {
            mouse::Interaction::None
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        // No hover state: widgets that consult the cursor directly must not
        // paint as if a real pointer were over them.
        self.content.as_widget().draw(
            tree,
            renderer,
            theme,
            style,
            layout,
            mouse::Cursor::Unavailable,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(tree, layout, renderer, viewport, translation)
    }
}

impl<'a, Message, Theme, Renderer> From<InputLock<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: renderer::Renderer + 'a,
{
    fn from(lock: InputLock<'a, Message, Theme, Renderer>) -> Self {
        Element::new(lock)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::Point;
    use iced::keyboard;

    #[test]
    fn input_kinds_are_locked_window_events_are_not() {
        assert!(is_input_event(&Event::Mouse(mouse::Event::CursorMoved {
            position: Point::ORIGIN,
        })));
        assert!(is_input_event(&Event::Mouse(mouse::Event::ButtonPressed(
            mouse::Button::Left,
        ))));
        assert!(is_input_event(&Event::Keyboard(
            keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                physical_key: keyboard::key::Physical::Unidentified(
                    keyboard::key::NativeCode::Unidentified,
                ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            }
        )));
        assert!(!is_input_event(&Event::Window(
            iced::window::Event::Unfocused
        )));
    }
}
