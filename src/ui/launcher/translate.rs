//! Full-bleed splash art with ken-burns scale + pan in one draw (outer clip only).

use crate::ui::launcher::mode::backdrop_pose_pixels;
use iced::advanced::image::{self as adv_image, FilterMethod};
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{Tree, Widget, tree};
use iced::widget::image::Handle;
use iced::{ContentFit, Element, Length, Radians, Rectangle, Size};
use iced::{Point, mouse};

/// Cover-fit splash that zooms then pans inside the zoom headroom (no black edges).
pub struct BackdropArt<Message, Theme = iced::Theme, Renderer = iced::Renderer> {
    handle: Handle,
    opacity: f32,
    scale: f32,
    ox_norm: f32,
    oy_norm: f32,
    /// Pull-model ken-burns: draw-time pose from `started + base_ms`.
    animate: bool,
    reduced_motion: bool,
    started: iced::time::Instant,
    base_ms: u64,
    width: Length,
    height: Length,
    _message: std::marker::PhantomData<Message>,
    _theme: std::marker::PhantomData<Theme>,
    _renderer: std::marker::PhantomData<Renderer>,
}

pub fn backdrop_art<Message, Theme, Renderer>(
    handle: Handle,
    opacity: f32,
    scale: f32,
    ox_norm: f32,
    oy_norm: f32,
) -> BackdropArt<Message, Theme, Renderer> {
    BackdropArt {
        handle,
        opacity: opacity.clamp(0.0, 1.0),
        scale: scale.max(1.0),
        ox_norm,
        oy_norm,
        animate: false,
        reduced_motion: false,
        started: iced::time::Instant::now(),
        base_ms: 0,
        width: Length::Fill,
        height: Length::Fill,
        _message: std::marker::PhantomData,
        _theme: std::marker::PhantomData,
        _renderer: std::marker::PhantomData,
    }
}

/// Incoming splash with a wall-clock ken-burns pose. `base_ms` is the elapsed
/// pose at view build; the widget adds its own elapsed time and requests
/// per-window redraws while motion is live.
#[allow(clippy::too_many_arguments)]
pub fn backdrop_art_animated<Message, Theme, Renderer>(
    handle: Handle,
    opacity: f32,
    scale: f32,
    ox_norm: f32,
    oy_norm: f32,
    reduced_motion: bool,
    started: iced::time::Instant,
    base_ms: u64,
) -> BackdropArt<Message, Theme, Renderer> {
    BackdropArt {
        handle,
        opacity: opacity.clamp(0.0, 1.0),
        scale: scale.max(1.0),
        ox_norm,
        oy_norm,
        animate: true,
        reduced_motion,
        started,
        base_ms,
        width: Length::Fill,
        height: Length::Fill,
        _message: std::marker::PhantomData,
        _theme: std::marker::PhantomData,
        _renderer: std::marker::PhantomData,
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for BackdropArt<Message, Theme, Renderer>
where
    Renderer: adv_image::Renderer<Handle = Handle>,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::stateless()
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: self.height,
        }
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.resolve(self.width, self.height, Size::ZERO))
    }

    fn update(
        &mut self,
        _tree: &mut Tree,
        _event: &iced::Event,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn iced::advanced::Clipboard,
        shell: &mut iced::advanced::Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        if self.animate && !self.reduced_motion {
            shell.request_redraw_at(
                iced::time::Instant::now() + iced::time::Duration::from_millis(16),
            );
        }
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else {
            return;
        };
        if clip.width <= 0.0 || clip.height <= 0.0 {
            return;
        }

        let natural = renderer
            .measure_image(&self.handle)
            .map(|s| Size::new(s.width as f32, s.height as f32))
            .unwrap_or(Size::new(1.0, 1.0));
        let (scale, ox_norm, oy_norm) = if self.animate && !self.reduced_motion {
            use crate::ui::launcher::mode::{backdrop_pose_offset, backdrop_pose_scale};
            use crate::ui::motion::{movement_offset, movement_scale};
            let elapsed = self.base_ms + self.started.elapsed().as_millis() as u64;
            let (ox, oy) = backdrop_pose_offset(elapsed);
            (
                movement_scale(backdrop_pose_scale(elapsed), false).max(1.0),
                movement_offset(ox, false),
                movement_offset(oy, false),
            )
        } else {
            (self.scale, self.ox_norm, self.oy_norm)
        };
        let fitted = ContentFit::Cover.fit(natural, bounds.size());
        let drawn = Size::new(fitted.width * scale, fitted.height * scale);
        // Pan within the Cover+zoom overflow only — clip is the stage rect.
        let (px, py) = backdrop_pose_pixels(
            ox_norm,
            oy_norm,
            drawn.width,
            drawn.height,
            bounds.width,
            bounds.height,
        );
        let drawing_bounds = Rectangle::new(
            Point::new(
                bounds.center_x() - drawn.width * 0.5 + px,
                bounds.center_y() - drawn.height * 0.5 + py,
            ),
            drawn,
        );

        renderer.draw_image(
            adv_image::Image {
                handle: self.handle.clone(),
                border_radius: 0.0.into(),
                filter_method: FilterMethod::default(),
                rotation: Radians(0.0),
                opacity: self.opacity,
                snap: true,
            },
            drawing_bounds,
            clip,
        );
    }
}

impl<'a, Message, Theme, Renderer> From<BackdropArt<Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: adv_image::Renderer<Handle = Handle> + 'a,
{
    fn from(value: BackdropArt<Message, Theme, Renderer>) -> Self {
        Element::new(value)
    }
}
