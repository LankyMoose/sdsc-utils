//! Overlay toast card rendered by the iced daemon.

use crate::ui::percent_ring::{self, TOAST_SIZE};
use crate::ui::svg_icon;
use crate::ui::theme;
use crate::ui::toast::{ToastMessage, ToastTrailing};
use iced::advanced::Renderer as _;
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{self, Tree, Widget, tree};
use iced::widget::{column, container, mouse_area, row, space, svg, text};
use iced::{Alignment, Element, Fill, Length, Shrink, Vector, mouse};

/// Logical width of the toast window.
pub const WIDTH: f32 = 360.0;
/// Logical height of the toast window.
pub const HEIGHT: f32 = 100.0;
/// Gap kept between the toast and the screen edge.
pub const MARGIN: f32 = 16.0;
/// Fixed toast HWND height: covers both slide extremes (rest and fully out).
pub const WINDOW_HEIGHT: f32 = HEIGHT * 2.0 + MARGIN;

const RAIL_WIDTH: f32 = 3.0;
const PADDING: f32 = 12.0;
const HEADING_SIZE: f32 = 14.0;
const BODY_SIZE: f32 = 13.0;
/// Compact leading icon for crash-restart toasts (not the percent-ring slot).
const BUG_ICON_SIZE: f32 = 36.0;

/// Renders the toast card. Clicking anywhere on it emits `on_dismiss`.
///
/// `generation` changes the layout width by 1px so iced/wgpu cannot keep the
/// previous toast's surface when messages are chained on one window.
/// `opacity` (0..=1) is the card's true opacity: the fill fades linearly to
/// it, while text/rail/ring saturate at full strength above the glass point.
/// `offset_y` is the wall-clock slide pose drawn inside the fixed window
/// (`0.0` once resting); the HWND never moves per frame.
pub fn view<'a, Message>(
    message: &'a ToastMessage,
    generation: u64,
    on_dismiss: Message,
    opacity: f32,
    offset_y: f32,
) -> Element<'a, Message>
where
    Message: Clone + 'a,
{
    let opacity = opacity.clamp(0.0, 1.0);
    let content = theme::toast_content_opacity(opacity);
    let accent = theme::from_rgb(message.accent);
    let width = WIDTH
        + if generation.is_multiple_of(2) {
            0.0
        } else {
            1.0
        };

    let rail = container(space())
        .width(Length::Fixed(RAIL_WIDTH))
        .height(Fill)
        .style(theme::rail(accent, opacity));

    let body = column![
        text(message.heading.as_str())
            .size(HEADING_SIZE)
            .color(theme::scale_alpha(theme::INK, content))
            .width(Fill),
        text(message.body.as_str())
            .size(BODY_SIZE)
            .color(theme::scale_alpha(theme::MUTED, content))
            .width(Fill),
    ]
    .spacing(4)
    .width(Fill);

    let content: Element<'_, Message> = match &message.trailing {
        ToastTrailing::Percent { percent, eta } => row![
            rail,
            body,
            percent_ring::percent_ring(*percent, accent, TOAST_SIZE, eta.clone(), 1.0, opacity)
        ]
        .spacing(PADDING)
        .align_y(Alignment::Center)
        .height(Fill)
        .into(),
        trailing @ (ToastTrailing::Bug | ToastTrailing::Check) => {
            let source = if matches!(trailing, ToastTrailing::Check) {
                svg_icon::CHECK_SVG
            } else {
                svg_icon::BUG_SVG
            };
            let icon = svg(svg::Handle::from_memory(source.as_bytes()))
                .width(Length::Fixed(BUG_ICON_SIZE))
                .height(Length::Fixed(BUG_ICON_SIZE))
                .style(move |_theme, _status| svg::Style {
                    color: Some(theme::scale_alpha(accent, content)),
                });
            row![rail, icon, body]
                .spacing(PADDING)
                .align_y(Alignment::Center)
                .height(Fill)
                .into()
        }
    };

    let card = container(content)
        .padding(PADDING)
        .width(Fill)
        .height(Fill)
        .style(theme::toast_card(opacity));

    let card = mouse_area(
        container(card)
            .width(Length::Fixed(width))
            .height(Length::Fixed(HEIGHT)),
    )
    .on_press(on_dismiss)
    .interaction(iced::mouse::Interaction::Pointer);
    Offset {
        content: card.into(),
        offset_y,
    }
    .into()
}

/// Draws its child translated inside a fixed window/ancestor.
struct Offset<'a, Message> {
    content: Element<'a, Message>,
    offset_y: f32,
}

impl Offset<'_, ()> {
    fn child_cursor(offset_y: f32, cursor: mouse::Cursor) -> mouse::Cursor {
        cursor
            .position()
            .map(|point| mouse::Cursor::Available(iced::Point::new(point.x, point.y - offset_y)))
            .unwrap_or(cursor)
    }
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for Offset<'_, Message> {
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
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> iced::Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &iced::Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn iced::advanced::Clipboard,
        shell: &mut iced::advanced::Shell<'_, Message>,
        viewport: &iced::Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            Offset::child_cursor(self.offset_y, cursor),
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &iced::Rectangle,
    ) {
        renderer.with_translation(Vector::new(0.0, self.offset_y), |renderer| {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                layout,
                Offset::child_cursor(self.offset_y, cursor),
                viewport,
            );
        });
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &iced::Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
}

impl<'a, Message: 'a> From<Offset<'a, Message>> for Element<'a, Message> {
    fn from(value: Offset<'a, Message>) -> Self {
        Element::new(value)
    }
}

/// Renders nothing when the toast window is open but idle between messages.
pub fn empty<'a, Message: 'a>() -> Element<'a, Message> {
    container(space()).width(Shrink).height(Shrink).into()
}
