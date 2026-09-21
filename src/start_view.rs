//! Start-screen launcher card: curated game list navigable with DualSense / keyboard.

use crate::games::GameEntry;
use crate::steam::SteamGame;
use crate::svg_icon;
use crate::theme;
use iced::widget::{button, column, container, row, scrollable, space, svg, text};
use iced::{Alignment, Element, Fill, Length, Padding};
use std::collections::HashMap;
use std::path::PathBuf;

/// Logical width of the start-screen window.
pub const WIDTH: f32 = 480.0;
/// Logical height of the start-screen window.
pub const HEIGHT: f32 = 560.0;

const HEADER_HEIGHT: f32 = 44.0;
const ROW_HEIGHT: f32 = 56.0;
const PADDING: f32 = 12.0;
const ICON_SIZE: f32 = 36.0;

#[derive(Debug, Clone)]
pub enum StartMessage {
    Launch(usize),
    MoveUp,
    MoveDown,
    Confirm,
    Close,
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

#[derive(Debug, Clone, Default)]
pub struct State {
    pub selected: usize,
    pub rows: Vec<StartRow>,
}

impl State {
    pub fn set_rows(&mut self, rows: Vec<StartRow>) {
        self.rows = rows;
        if self.rows.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.rows.len() - 1);
        }
    }

    pub fn move_selection(&mut self, delta: i32) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as i32;
        let next = (self.selected as i32 + delta).rem_euclid(len);
        self.selected = next as usize;
    }

    pub fn selected_target(&self) -> Option<&str> {
        self.rows.get(self.selected).map(|r| r.target.as_str())
    }
}

pub fn view(state: &State) -> Element<'_, StartMessage> {
    let header = row![
        text("Quick launch").size(18.0).color(theme::INK),
        space().width(Fill),
        button(
            svg(svg::Handle::from_memory(svg_icon::CLOSE_SVG.as_bytes()))
                .width(Length::Fixed(16.0))
                .height(Length::Fixed(16.0)),
        )
        .padding(6)
        .on_press(StartMessage::Close)
        .style(theme::ghost),
    ]
    .align_y(Alignment::Center)
    .height(Length::Fixed(HEADER_HEIGHT));

    let list: Element<'_, StartMessage> = if state.rows.is_empty() {
        container(
            text("No games yet — add some in Settings → Start screen.")
                .size(13.0)
                .color(theme::MUTED),
        )
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .into()
    } else {
        let items = state
            .rows
            .iter()
            .enumerate()
            .fold(column![].spacing(4).width(Fill), |col, (index, row)| {
                col.push(game_row(index, row, index == state.selected))
            });
        scrollable(items).height(Fill).width(Fill).into()
    };

    let hint = text("D-pad / sticks · Cross launch · Circle close")
        .size(11.0)
        .color(theme::DIM);

    container(
        column![
            header,
            container(space())
                .width(Fill)
                .height(Length::Fixed(1.0))
                .style(theme::configure_header_rule),
            container(list)
                .padding(Padding::from([PADDING, 0.0]))
                .width(Fill)
                .height(Fill),
            hint,
        ]
        .spacing(4)
        .padding(PADDING)
        .width(Fill)
        .height(Fill),
    )
    .width(Fill)
    .height(Fill)
    .style(theme::root)
    .into()
}

fn game_row(index: usize, row: &StartRow, selected: bool) -> Element<'_, StartMessage> {
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

    let mut titles = column![text(&row.title).size(14.0).color(theme::INK)].spacing(2);
    if let Some(sub) = row.subtitle.as_ref() {
        titles = titles.push(text(sub).size(11.0).color(theme::DIM));
    }

    button(
        row![icon, titles]
            .spacing(10)
            .align_y(Alignment::Center)
            .width(Fill),
    )
    .padding([8, 10])
    .width(Fill)
    .height(Length::Fixed(ROW_HEIGHT))
    .on_press(StartMessage::Launch(index))
    .style(theme::chip(selected))
    .into()
}
