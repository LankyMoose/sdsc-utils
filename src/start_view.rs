//! Start-screen carousel: Games + Controllers, DualSense / keyboard navigable.

use crate::battery::PowerState;
use crate::color::BatterySpectrum;
use crate::games::GameEntry;
use crate::percent_ring::{self, POPUP_SIZE};
use crate::steam::SteamGame;
use crate::svg_icon;
use crate::theme;
use crate::window_layout;
use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::widget::{button, column, container, row, scrollable, space, svg, text};
use iced::{
    Alignment, Background, Border, Color, Element, Fill, Length, Padding, Point, Rectangle,
    Renderer, Theme,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Logical width of the start-screen window.
pub const WIDTH: f32 = 640.0;
/// Logical height of the start-screen window.
pub const HEIGHT: f32 = 720.0;

pub const SLIDE_ANIM_MS: u64 = 360;
const SLIDE_ANIM_MIN_MS: u64 = 120;
const HEADER_HEIGHT: f32 = 52.0;
const ROW_HEIGHT: f32 = 88.0;
const CONTROLLER_ROW_HEIGHT: f32 = 100.0;
const PADDING: f32 = 20.0;
const ICON_SIZE: f32 = 56.0;
const HOLD_RING_SIZE: f32 = 32.0;
const FACE_GLYPH_SIZE: f32 = 18.0;
const ROW_GAP: f32 = 10.0;
const ACCENT_BAR_W: f32 = 4.0;
/// Body width inside outer padding — dual-pane strip unit.
const PANE_W: f32 = WIDTH - 2.0 * PADDING;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StartSlide {
    #[default]
    Games,
    Controllers,
}

impl StartSlide {
    pub fn title(self) -> &'static str {
        match self {
            Self::Games => "Games",
            Self::Controllers => "Controllers",
        }
    }
}

#[derive(Debug, Clone)]
pub enum StartMessage {
    Launch(usize),
    SelectController(usize),
    MoveUp,
    MoveDown,
    Confirm,
    Close,
    PrevSlide,
    NextSlide,
}

#[derive(Debug, Clone)]
pub struct StartRow {
    pub title: String,
    pub subtitle: Option<String>,
    pub target: String,
    pub icon_path: Option<PathBuf>,
}

impl StartRow {
    pub fn from_entry(entry: &GameEntry, steam_by_id: &HashMap<u32, SteamGame>) -> Self {
        match entry {
            GameEntry::Steam { appid } => {
                if let Some(game) = steam_by_id.get(appid) {
                    Self {
                        title: game.name.clone(),
                        subtitle: Some(format!("Steam · {appid}")),
                        target: crate::steam::launch_uri(*appid),
                        icon_path: game.icon_path.clone(),
                    }
                } else {
                    Self {
                        title: format!("Steam {appid}"),
                        subtitle: Some(format!("steam://rungameid/{appid}")),
                        target: crate::steam::launch_uri(*appid),
                        icon_path: None,
                    }
                }
            }
            GameEntry::Manual { title, target, .. } => Self {
                title: title.clone(),
                subtitle: Some(target.clone()),
                target: target.clone(),
                icon_path: None,
            },
        }
    }
}

/// Connected-pad row for the Controllers slide (no remember / nickname edit).
#[derive(Debug, Clone, PartialEq)]
pub struct StartControllerRow {
    pub serial: String,
    pub title: String,
    pub connection: String,
    pub state: String,
    pub percent: u8,
    pub low: bool,
    pub bluetooth: bool,
    pub eta: Option<String>,
}

#[derive(Debug, Clone)]
struct SlideAnim {
    from_x: f32,
    to: StartSlide,
    duration_ms: u64,
    started: Instant,
}

#[derive(Debug, Clone)]
pub struct ReplaceConfirm {
    pub running_title: String,
    pub next_title: String,
    pub next_index: usize,
}

#[derive(Debug, Clone)]
pub struct State {
    pub slide: StartSlide,
    pub game_selected: usize,
    pub rows: Vec<StartRow>,
    pub controller_selected: usize,
    pub controllers: Vec<StartControllerRow>,
    pub running_target: Option<String>,
    pub replace_confirm: Option<ReplaceConfirm>,
    pub triangle_progress: f32,
    pub cross_progress: f32,
    anim: Option<SlideAnim>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            slide: StartSlide::Games,
            game_selected: 0,
            rows: Vec::new(),
            controller_selected: 0,
            controllers: Vec::new(),
            running_target: None,
            replace_confirm: None,
            triangle_progress: 0.0,
            cross_progress: 0.0,
            anim: None,
        }
    }
}

impl State {
    pub fn set_rows(&mut self, rows: Vec<StartRow>) {
        self.rows = rows;
        if self.rows.is_empty() {
            self.game_selected = 0;
        } else {
            self.game_selected = self.game_selected.min(self.rows.len() - 1);
        }
    }

    pub fn set_controllers(&mut self, controllers: Vec<StartControllerRow>) {
        self.controllers = controllers;
        if self.controllers.is_empty() {
            self.controller_selected = 0;
        } else {
            self.controller_selected = self.controller_selected.min(self.controllers.len() - 1);
        }
    }

    /// Land on Games with no in-flight anim (used whenever the start screen opens).
    pub fn reset_to_games(&mut self) {
        self.slide = StartSlide::Games;
        self.anim = None;
        self.replace_confirm = None;
        self.triangle_progress = 0.0;
        self.cross_progress = 0.0;
    }

    pub fn animating(&self) -> bool {
        self.anim.is_some()
    }

    pub fn needs_frames(&self) -> bool {
        self.anim.is_some()
            || self.triangle_progress > 0.0
            || self.cross_progress > 0.0
            || self.replace_confirm.is_some()
    }

    pub fn tick_anim(&mut self, now: Instant) -> bool {
        let Some(anim) = self.anim.as_ref() else {
            return false;
        };
        let elapsed = now.saturating_duration_since(anim.started);
        if elapsed >= Duration::from_millis(anim.duration_ms) {
            self.slide = anim.to;
            self.anim = None;
            return true;
        }
        true
    }

    /// Request a slide target. Interruptible: restarts from the current scroll position.
    /// No-op if already at / animating toward `to`.
    pub fn request_slide(&mut self, to: StartSlide, now: Instant) {
        if self.anim.as_ref().is_some_and(|a| a.to == to) {
            return;
        }
        if self.anim.is_none() && self.slide == to {
            return;
        }

        let from_x = self.slide_scroll_x(now);
        let to_x = slide_x(to);
        let distance = (to_x - from_x).abs();
        if distance < 0.5 {
            self.slide = to;
            self.anim = None;
            return;
        }

        let duration_ms = ((SLIDE_ANIM_MS as f32) * (distance / PANE_W))
            .round()
            .clamp(SLIDE_ANIM_MIN_MS as f32, SLIDE_ANIM_MS as f32) as u64;

        self.replace_confirm = None;
        self.cross_progress = 0.0;
        self.anim = Some(SlideAnim {
            from_x,
            to,
            duration_ms,
            started: now,
        });
    }

    pub fn move_selection(&mut self, delta: i32) {
        if self.replace_confirm.is_some() || self.anim.is_some() {
            return;
        }
        match self.slide {
            StartSlide::Games => {
                if self.rows.is_empty() {
                    return;
                }
                let len = self.rows.len() as i32;
                self.game_selected = (self.game_selected as i32 + delta).rem_euclid(len) as usize;
            }
            StartSlide::Controllers => {
                if self.controllers.is_empty() {
                    return;
                }
                let len = self.controllers.len() as i32;
                self.controller_selected =
                    (self.controller_selected as i32 + delta).rem_euclid(len) as usize;
            }
        }
    }

    pub fn selected_controller(&self) -> Option<&StartControllerRow> {
        self.controllers.get(self.controller_selected)
    }

    /// Horizontal scroll of the dual-pane strip (0 = Games, PANE_W = Controllers).
    fn slide_scroll_x(&self, now: Instant) -> f32 {
        let Some(anim) = self.anim.as_ref() else {
            return slide_x(self.slide);
        };
        let t = now.saturating_duration_since(anim.started).as_secs_f32()
            / (anim.duration_ms as f32 / 1000.0).max(0.001);
        let eased = window_layout::ease_out_cubic(t.clamp(0.0, 1.0));
        let to_x = slide_x(anim.to);
        anim.from_x + (to_x - anim.from_x) * eased
    }

    fn header_slide(&self, now: Instant) -> StartSlide {
        if self.slide_scroll_x(now) >= PANE_W * 0.5 {
            StartSlide::Controllers
        } else {
            StartSlide::Games
        }
    }
}

fn slide_x(slide: StartSlide) -> f32 {
    match slide {
        StartSlide::Games => 0.0,
        StartSlide::Controllers => PANE_W,
    }
}

pub fn view<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    now: Instant,
) -> Element<'a, StartMessage> {
    let shown = state.header_slide(now);
    let header = slide_header(shown);

    let body = if state.replace_confirm.is_some() {
        replace_confirm_view(state)
    } else {
        carousel_body(state, spectrum, now)
    };

    let hint = footer_hint(state);

    container(
        column![
            header,
            container(space())
                .width(Fill)
                .height(Length::Fixed(1.0))
                .style(theme::configure_header_rule),
            container(body).width(Fill).height(Fill),
            hint,
        ]
        .spacing(10)
        .padding(PADDING)
        .width(Fill)
        .height(Fill),
    )
    .width(Fill)
    .height(Fill)
    .style(theme::root)
    .into()
}

/// Three equal columns: left cue | centered title | right cue.
fn slide_header(slide: StartSlide) -> Element<'static, StartMessage> {
    let left: Element<'static, StartMessage> = match slide {
        StartSlide::Controllers => row![
            text("Games").size(14.0).color(theme::MUTED),
            text("L2").size(14.0).color(theme::ACCENT),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into(),
        StartSlide::Games => space().into(),
    };
    let right: Element<'static, StartMessage> = match slide {
        StartSlide::Games => row![
            text("R2").size(14.0).color(theme::ACCENT),
            text("Controllers").size(14.0).color(theme::MUTED),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into(),
        StartSlide::Controllers => space().into(),
    };

    row![
        container(left)
            .width(Fill)
            .align_x(Alignment::Start)
            .center_y(Fill),
        container(text(slide.title()).size(30.0).color(theme::INK))
            .width(Fill)
            .center_x(Fill)
            .center_y(Fill),
        container(right)
            .width(Fill)
            .align_x(Alignment::End)
            .center_y(Fill),
    ]
    .align_y(Alignment::Center)
    .height(Length::Fixed(HEADER_HEIGHT))
    .into()
}

fn carousel_body<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
    now: Instant,
) -> Element<'a, StartMessage> {
    let scroll = state.slide_scroll_x(now);
    let strip = row![
        container(games_list(state))
            .width(Length::Fixed(PANE_W))
            .height(Fill),
        container(controllers_list(state, spectrum))
            .width(Length::Fixed(PANE_W))
            .height(Fill),
    ]
    .width(Length::Fixed(PANE_W * 2.0))
    .height(Fill);

    container(strip)
        .width(Length::Fixed(PANE_W))
        .height(Fill)
        .clip(true)
        .padding(Padding {
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
            left: -scroll,
        })
        .into()
}

fn games_list(state: &State) -> Element<'_, StartMessage> {
    if state.rows.is_empty() {
        return container(
            text("No games yet — add some in Settings → Start screen.")
                .size(15.0)
                .color(theme::MUTED),
        )
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .into();
    }
    let items = state.rows.iter().enumerate().fold(
        column![].spacing(ROW_GAP).width(Fill),
        |col, (index, row)| {
            let running = state
                .running_target
                .as_ref()
                .is_some_and(|t| t == &row.target);
            col.push(game_row(index, row, index == state.game_selected, running))
        },
    );
    scrollable(items).height(Fill).width(Fill).into()
}

fn controllers_list<'a>(state: &'a State, spectrum: &BatterySpectrum) -> Element<'a, StartMessage> {
    if state.controllers.is_empty() {
        return container(
            text("No DualSense connected")
                .size(15.0)
                .color(theme::MUTED),
        )
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .into();
    }
    let items = state.controllers.iter().enumerate().fold(
        column![].spacing(ROW_GAP).width(Fill),
        |col, (index, row)| {
            col.push(controller_row(
                index,
                row,
                index == state.controller_selected,
                spectrum,
            ))
        },
    );
    scrollable(items).height(Fill).width(Fill).into()
}

fn replace_confirm_view(state: &State) -> Element<'_, StartMessage> {
    let Some(confirm) = state.replace_confirm.as_ref() else {
        return space().into();
    };
    container(
        column![
            text("Game already running").size(20.0).color(theme::INK),
            text(format!(
                "Close {} and launch {}?",
                confirm.running_title, confirm.next_title
            ))
            .size(14.0)
            .color(theme::MUTED),
            space().height(8),
            action_cluster(&[
                ActionHint {
                    face: FaceButton::Cross,
                    label: "Proceed",
                    hold: Some(state.cross_progress),
                    hold_prefix: true,
                },
                ActionHint {
                    face: FaceButton::Circle,
                    label: "Cancel",
                    hold: None,
                    hold_prefix: false,
                },
            ]),
        ]
        .spacing(12)
        .align_x(Alignment::Center),
    )
    .width(Fill)
    .height(Fill)
    .center_x(Fill)
    .center_y(Fill)
    .into()
}

#[derive(Clone, Copy)]
enum FaceButton {
    Cross,
    Circle,
    Triangle,
}

impl FaceButton {
    fn svg(self) -> &'static str {
        match self {
            Self::Cross => svg_icon::FACE_CROSS_SVG,
            Self::Circle => svg_icon::FACE_CIRCLE_SVG,
            Self::Triangle => svg_icon::FACE_TRIANGLE_SVG,
        }
    }
}

struct ActionHint {
    face: FaceButton,
    label: &'static str,
    hold: Option<f32>,
    hold_prefix: bool,
}

fn footer_hint(state: &State) -> Element<'_, StartMessage> {
    if state.replace_confirm.is_some() {
        return container(action_cluster(&[
            ActionHint {
                face: FaceButton::Cross,
                label: "Proceed",
                hold: Some(state.cross_progress),
                hold_prefix: true,
            },
            ActionHint {
                face: FaceButton::Circle,
                label: "Cancel",
                hold: None,
                hold_prefix: false,
            },
        ]))
        .width(Fill)
        .center_x(Fill)
        .into();
    }

    let hints: Vec<ActionHint> = match state.slide {
        StartSlide::Games => {
            let mut v = vec![
                ActionHint {
                    face: FaceButton::Cross,
                    label: "Launch",
                    hold: None,
                    hold_prefix: false,
                },
                ActionHint {
                    face: FaceButton::Circle,
                    label: "Close",
                    hold: None,
                    hold_prefix: false,
                },
            ];
            if state.running_target.is_some() {
                v.push(ActionHint {
                    face: FaceButton::Triangle,
                    label: "Close game",
                    hold: Some(state.triangle_progress),
                    hold_prefix: true,
                });
            }
            v
        }
        StartSlide::Controllers => vec![
            ActionHint {
                face: FaceButton::Cross,
                label: "Identify",
                hold: None,
                hold_prefix: false,
            },
            ActionHint {
                face: FaceButton::Triangle,
                label: "Power off",
                hold: Some(state.triangle_progress),
                hold_prefix: true,
            },
            ActionHint {
                face: FaceButton::Circle,
                label: "Close",
                hold: None,
                hold_prefix: false,
            },
        ],
    };
    container(action_cluster(&hints))
        .width(Fill)
        .center_x(Fill)
        .into()
}

fn action_cluster(hints: &[ActionHint]) -> Element<'static, StartMessage> {
    let mut row = row![].spacing(28).align_y(Alignment::Center);
    for hint in hints {
        row = row.push(action_hint(hint));
    }
    row.into()
}

fn action_hint(hint: &ActionHint) -> Element<'static, StartMessage> {
    let glyph: Element<'static, StartMessage> = if let Some(progress) = hint.hold {
        hold_glyph(hint.face, progress)
    } else {
        face_svg(hint.face)
    };

    let mut parts = row![].spacing(8).align_y(Alignment::Center);
    if hint.hold_prefix && hint.hold.is_some() {
        parts = parts.push(text("Hold").size(14.0).color(theme::MUTED));
    }
    parts = parts.push(glyph);
    parts = parts.push(text(hint.label).size(14.0).color(theme::MUTED));
    parts.into()
}

fn face_svg(face: FaceButton) -> Element<'static, StartMessage> {
    svg(svg::Handle::from_memory(face.svg().as_bytes()))
        .width(Length::Fixed(FACE_GLYPH_SIZE))
        .height(Length::Fixed(FACE_GLYPH_SIZE))
        .into()
}

fn hold_glyph(face: FaceButton, progress: f32) -> Element<'static, StartMessage> {
    iced::widget::stack![
        hold_ring(progress),
        container(face_svg(face))
            .width(Length::Fixed(HOLD_RING_SIZE))
            .height(Length::Fixed(HOLD_RING_SIZE))
            .center_x(Fill)
            .center_y(Fill),
    ]
    .width(Length::Fixed(HOLD_RING_SIZE))
    .height(Length::Fixed(HOLD_RING_SIZE))
    .into()
}

fn hold_ring(progress: f32) -> Element<'static, StartMessage> {
    iced::widget::canvas(HoldRing { progress })
        .width(Length::Fixed(HOLD_RING_SIZE))
        .height(Length::Fixed(HOLD_RING_SIZE))
        .into()
}

struct HoldRing {
    progress: f32,
}

impl canvas::Program<StartMessage> for HoldRing {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let center = Point::new(bounds.width / 2.0, bounds.height / 2.0);
        let radius = (bounds.width.min(bounds.height) / 2.0) - 2.0;
        let track = Path::circle(center, radius);
        frame.stroke(
            &track,
            Stroke::default().with_width(2.0).with_color(Color {
                a: 0.25,
                ..theme::MUTED
            }),
        );
        if self.progress > 0.01 {
            let start = -std::f32::consts::FRAC_PI_2;
            let end = start + self.progress * std::f32::consts::TAU;
            let steps = ((self.progress * 48.0).ceil() as usize).max(2);
            let arc = Path::new(|builder| {
                for i in 0..=steps {
                    let t = i as f32 / steps as f32;
                    let a = start + (end - start) * t;
                    let p = Point::new(center.x + radius * a.cos(), center.y + radius * a.sin());
                    if i == 0 {
                        builder.move_to(p);
                    } else {
                        builder.line_to(p);
                    }
                }
            });
            frame.stroke(
                &arc,
                Stroke::default().with_width(2.5).with_color(theme::ACCENT),
            );
        }
        vec![frame.into_geometry()]
    }
}

fn selection_bar(selected: bool) -> Element<'static, StartMessage> {
    container(space())
        .width(Length::Fixed(ACCENT_BAR_W))
        .height(Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(if selected {
                theme::ACCENT
            } else {
                Color::TRANSPARENT
            })),
            border: Border {
                radius: 2.0.into(),
                ..Default::default()
            },
            ..container::Style::default()
        })
        .into()
}

fn game_row(
    index: usize,
    row: &StartRow,
    selected: bool,
    running: bool,
) -> Element<'_, StartMessage> {
    let icon: Element<'_, StartMessage> = if let Some(path) = row.icon_path.as_ref() {
        iced::widget::image(iced::widget::image::Handle::from_path(path.clone()))
            .width(Length::Fixed(ICON_SIZE))
            .height(Length::Fixed(ICON_SIZE))
            .into()
    } else {
        container(
            svg(svg::Handle::from_memory(svg_icon::DUALSENSE_SVG.as_bytes()))
                .width(Length::Fixed(ICON_SIZE * 0.7))
                .height(Length::Fixed(ICON_SIZE * 0.7)),
        )
        .width(Length::Fixed(ICON_SIZE))
        .height(Length::Fixed(ICON_SIZE))
        .center_x(Fill)
        .center_y(Fill)
        .style(theme::well)
        .into()
    };

    let mut titles = column![text(&row.title).size(19.0).color(theme::INK)].spacing(4);
    if running {
        titles = titles.push(text("Running").size(13.0).color(theme::SUCCESS));
    } else if let Some(sub) = row.subtitle.as_ref() {
        titles = titles.push(text(sub).size(13.0).color(theme::MUTED));
    }

    button(
        row![selection_bar(selected), icon, titles]
            .spacing(14)
            .align_y(Alignment::Center)
            .width(Fill)
            .height(Length::Fixed(ROW_HEIGHT)),
    )
    .padding([0, 12])
    .width(Fill)
    .height(Length::Fixed(ROW_HEIGHT))
    .on_press(StartMessage::Launch(index))
    .style(theme::menu_row(selected))
    .into()
}

fn controller_row<'a>(
    index: usize,
    row: &'a StartControllerRow,
    selected: bool,
    spectrum: &BatterySpectrum,
) -> Element<'a, StartMessage> {
    let ring_color = theme::from_rgb(spectrum.color_at_percent(row.percent));
    let ring =
        percent_ring::percent_ring(row.percent, ring_color, POPUP_SIZE * 0.95, row.eta.clone());

    let meta_color = if row.low {
        theme::WARNING
    } else {
        theme::MUTED
    };
    let titles = column![
        text(&row.title).size(19.0).color(theme::INK),
        text(format!("{} · {}", row.connection, row.state))
            .size(13.0)
            .color(meta_color),
    ]
    .spacing(4);

    button(
        row![selection_bar(selected), ring, titles]
            .spacing(14)
            .align_y(Alignment::Center)
            .width(Fill)
            .height(Length::Fixed(CONTROLLER_ROW_HEIGHT)),
    )
    .padding([0, 12])
    .width(Fill)
    .height(Length::Fixed(CONTROLLER_ROW_HEIGHT))
    .on_press(StartMessage::SelectController(index))
    .style(theme::menu_row(selected))
    .into()
}

pub fn controller_title(product: &str, nickname: Option<&str>) -> String {
    nickname
        .filter(|n| !n.is_empty())
        .unwrap_or(product)
        .to_string()
}

pub fn power_state_label(state: PowerState) -> &'static str {
    state.as_str()
}
