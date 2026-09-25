//! Tray-anchored controller overview, rendered by the iced daemon.

use crate::controller::dualsense::lightbar;
use crate::controller::known::KnownController;
use crate::controller::model::ControllerStatus;
use crate::ui::color::BatterySpectrum;
use crate::ui::percent_ring::{self, POPUP_SIZE};
use crate::ui::svg_icon;
use crate::ui::theme;
use iced::widget::{
    Column, button, checkbox, column, container, row, scrollable, space, svg, text, text_input,
    tooltip,
};
use iced::{Alignment, Color, Element, Fill, Length, Shrink};
use std::time::{Duration, Instant};

/// Logical width of the popup window.
pub const WIDTH: f32 = 380.0;
/// Popup grows with row count until this fraction of the monitor height.
const MAX_HEIGHT_FRACTION: f32 = 0.5;
/// Fallback monitor height when iced has not reported one yet.
const FALLBACK_MONITOR_HEIGHT: f32 = 1080.0;

const HEADER_HEIGHT: f32 = 38.0;
const ROW_HEIGHT: f32 = 88.0;
const ROW_SPACING: f32 = theme::LIST_SEPARATOR_GAP;
const PADDING: f32 = 10.0;
/// Gap between the title underline and the list (matches Start’s chrome rhythm).
const HEADER_BODY_GAP: f32 = 8.0;
/// Gap between list content and the embedded scrollbar.
const SCROLL_GAP: f32 = 8.0;
const EMPTY_HEIGHT: f32 = 56.0;
const ICON_SIZE: f32 = 18.0;
const NICKNAME_MAX_CHARS: usize = 32;
const TOOLTIP_DELAY: Duration = Duration::from_millis(350);

/// Widget id of the nickname editor, so the daemon can focus it on demand.
pub fn nickname_input_id() -> iced::widget::Id {
    iced::widget::Id::new("popup-nickname")
}

/// A single controller entry: either live, or remembered-but-disconnected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerRow {
    pub serial: String,
    pub product: String,
    pub nickname: Option<String>,
    pub connection: String,
    pub state: String,
    pub percent: u8,
    pub connected: bool,
    pub remembered: bool,
    pub remember_enabled: bool,
    pub low: bool,
    /// Optional remaining-time hint from battery analytics (`~3h 30m`).
    pub eta: Option<String>,
    pub supports_identify: bool,
    pub supports_power_off: bool,
}

impl ControllerRow {
    pub fn connected(
        controller: &ControllerStatus,
        remembered: bool,
        remember_enabled: bool,
        nickname: Option<String>,
        low_battery_percent: u8,
        eta: Option<String>,
    ) -> Self {
        Self {
            serial: controller.serial.clone(),
            product: controller.product.to_string(),
            nickname,
            connection: controller.connection.to_string(),
            state: if controller.is_low_battery(low_battery_percent) {
                "low battery".to_string()
            } else {
                controller.state.as_str().to_string()
            },
            percent: controller.percent,
            connected: true,
            remembered,
            remember_enabled,
            low: controller.is_low_battery(low_battery_percent),
            eta,
            supports_identify: controller.supports_lightbar,
            supports_power_off: controller.supports_power_off,
        }
    }

    pub fn disconnected(
        controller: &KnownController,
        nickname: Option<String>,
        eta: Option<String>,
    ) -> Self {
        Self {
            serial: controller.serial.clone(),
            product: controller.product.clone(),
            nickname,
            connection: controller.connection.clone(),
            state: "disconnected".to_string(),
            percent: controller.percent,
            connected: false,
            remembered: true,
            remember_enabled: true,
            low: false,
            eta,
            supports_identify: false,
            supports_power_off: false,
        }
    }

    pub fn display_name(&self) -> &str {
        self.nickname
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(self.product.as_str())
    }

    pub fn show_identify(&self) -> bool {
        self.connected && self.supports_identify
    }

    pub fn show_power_off(&self) -> bool {
        self.connected && self.supports_power_off && self.connection == "Bluetooth"
    }

    pub fn show_edit(&self) -> bool {
        self.remember_enabled
    }
}

/// Active Identify ring flash for one controller row.
#[derive(Debug, Clone)]
struct IdentifyFlash {
    serial: String,
    started: Instant,
}

/// Transient popup UI state owned by the daemon.
#[derive(Debug, Default)]
pub struct State {
    /// Serial of the row whose nickname is being edited.
    pub editing_serial: Option<String>,
    /// Current nickname draft.
    pub draft: String,
    /// Ring flash started with the last successful Identify.
    identify_flash: Option<IdentifyFlash>,
}

impl State {
    pub fn begin_edit(&mut self, serial: &str, current: Option<&str>) {
        self.editing_serial = Some(serial.to_string());
        self.draft = current.unwrap_or_default().to_string();
    }

    pub fn cancel(&mut self) {
        self.editing_serial = None;
        self.draft.clear();
        self.identify_flash = None;
    }

    /// Start the UI ring flash that mirrors the lightbar Identify pattern.
    pub fn begin_identify_flash(&mut self, serial: &str) {
        self.identify_flash = Some(IdentifyFlash {
            serial: serial.to_string(),
            started: Instant::now(),
        });
    }

    /// True while a ring Identify flash still has frames to draw.
    pub fn identify_flash_active(&self) -> bool {
        self.identify_flash.as_ref().is_some_and(|flash| {
            lightbar::identify_flash_is_white(flash.started, Instant::now()).is_some()
        })
    }

    /// Drop finished flash state; call from the frame subscription.
    pub fn tick_identify_flash(&mut self) {
        let finished = self.identify_flash.as_ref().is_some_and(|flash| {
            lightbar::identify_flash_is_white(flash.started, Instant::now()).is_none()
        });
        if finished {
            self.identify_flash = None;
        }
    }

    fn ring_flash_white(&self, serial: &str) -> bool {
        self.identify_flash.as_ref().is_some_and(|flash| {
            flash.serial == serial
                && lightbar::identify_flash_is_white(flash.started, Instant::now()) == Some(true)
        })
    }

    /// Finish editing and return `(serial, nickname)`; `None` clears the nickname.
    pub fn commit(&mut self) -> Option<(String, Option<String>)> {
        let serial = self.editing_serial.take()?;
        let draft = std::mem::take(&mut self.draft);
        let trimmed = draft.trim().to_string();
        Some((serial, (!trimmed.is_empty()).then_some(trimmed)))
    }

    pub fn is_editing(&self, serial: &str) -> bool {
        self.editing_serial.as_deref() == Some(serial)
    }

    pub fn is_editing_any(&self) -> bool {
        self.editing_serial.is_some()
    }
}

#[derive(Debug, Clone)]
pub enum PopupMessage {
    OpenSettings,
    Identify(String),
    PowerOff(String),
    ToggleRemember(String),
    BeginEdit(String),
    DraftChanged(String),
    CommitNickname,
    CancelEdit,
}

/// Non-list chrome: frame + padding + header + underline + gap before the list.
fn chrome_height() -> f32 {
    theme::WINDOW_FRAME * 2.0 + PADDING * 2.0 + HEADER_HEIGHT + 1.0 + HEADER_BODY_GAP
}

fn list_content_height(rows: usize) -> f32 {
    if rows == 0 {
        EMPTY_HEIGHT
    } else {
        rows as f32 * ROW_HEIGHT + rows.saturating_sub(1) as f32 * ROW_SPACING
    }
}

/// Height the popup window should be given for `rows` entries (capped at 50vh).
pub fn window_height(rows: usize, monitor_height: Option<f32>) -> f32 {
    let monitor_h = monitor_height
        .filter(|h| *h > 0.0)
        .unwrap_or(FALLBACK_MONITOR_HEIGHT);
    let chrome = chrome_height();
    let natural = chrome + list_content_height(rows);
    let max_h = (monitor_h * MAX_HEIGHT_FRACTION).max(chrome + ROW_HEIGHT);
    natural.min(max_h)
}

pub fn view<'a>(
    state: &'a State,
    rows: &'a [ControllerRow],
    spectrum: &BatterySpectrum,
) -> Element<'a, PopupMessage> {
    let header = row![
        text("Controllers").size(14.0).color(theme::INK).width(Fill),
        icon_button(
            svg_icon::SETTINGS_SVG,
            theme::MUTED,
            "Settings",
            PopupMessage::OpenSettings,
        ),
    ]
    .align_y(Alignment::Center)
    .spacing(6)
    .padding([0, 2])
    .height(Length::Fixed(HEADER_HEIGHT));

    let body: Element<'_, PopupMessage> = if rows.is_empty() {
        container(
            text("No DualSense controllers connected")
                .size(13.0)
                .color(theme::DIM),
        )
        .center_x(Fill)
        .center_y(Length::Fixed(EMPTY_HEIGHT))
        .into()
    } else {
        let list = rows
            .iter()
            .enumerate()
            .fold(Column::new().spacing(0), |list, (index, entry)| {
                let list = if index > 0 {
                    list.push(theme::list_separator())
                } else {
                    list
                };
                list.push(controller_row(state, entry, spectrum))
            })
            .width(Fill);

        // Embed scrollbar with a gutter so row actions are not flush to the thumb.
        // Outer column padding keeps the bar off the window frame.
        scrollable(list)
            .spacing(SCROLL_GAP)
            .height(Fill)
            .width(Fill)
            .into()
    };

    // Match Start: 1px LINE frame, title, underline, then content.
    let chrome = column![
        header,
        container(space())
            .width(Fill)
            .height(Length::Fixed(1.0))
            .style(theme::configure_header_rule),
    ]
    .spacing(0)
    .width(Fill);

    theme::framed(
        column![chrome, body]
            .spacing(HEADER_BODY_GAP)
            .padding(PADDING)
            .width(Fill)
            .height(Fill),
    )
}

fn controller_row<'a>(
    state: &'a State,
    entry: &'a ControllerRow,
    spectrum: &BatterySpectrum,
) -> Element<'a, PopupMessage> {
    let accent = theme::from_rgb(spectrum.color_at_percent(entry.percent));
    let ring_color = if state.ring_flash_white(&entry.serial) {
        theme::from_rgb(lightbar::IDENTIFY_FLASH)
    } else if entry.connected {
        accent
    } else {
        theme::DIM
    };
    let ring = percent_ring::percent_ring(entry.percent, ring_color, POPUP_SIZE, entry.eta.clone());

    let name: Element<'_, PopupMessage> = if state.is_editing(&entry.serial) {
        text_input("Nickname", &state.draft)
            .id(nickname_input_id())
            .on_input(|value| {
                PopupMessage::DraftChanged(value.chars().take(NICKNAME_MAX_CHARS).collect())
            })
            .on_submit(PopupMessage::CommitNickname)
            .size(13.0)
            .padding([2, 6])
            .width(Fill)
            .style(theme::input)
            .into()
    } else {
        text(entry.display_name())
            .size(14.0)
            .color(if entry.connected {
                theme::INK
            } else {
                theme::MUTED
            })
            .width(Fill)
            .into()
    };

    let mut actions = row![].spacing(2).align_y(Alignment::Center);
    if state.is_editing(&entry.serial) {
        actions = actions.push(icon_button(
            svg_icon::CHECK_SVG,
            theme::SUCCESS,
            "Save nickname",
            PopupMessage::CommitNickname,
        ));
        actions = actions.push(icon_button(
            svg_icon::CLOSE_SVG,
            theme::MUTED,
            "Cancel",
            PopupMessage::CancelEdit,
        ));
    } else {
        if entry.show_edit() {
            actions = actions.push(icon_button(
                svg_icon::EDIT_SVG,
                theme::MUTED,
                "Edit nickname",
                PopupMessage::BeginEdit(entry.serial.clone()),
            ));
        }
        if entry.show_identify() {
            actions = actions.push(icon_button(
                svg_icon::IDENTIFY_SVG,
                theme::MUTED,
                "Identify",
                PopupMessage::Identify(entry.serial.clone()),
            ));
        }
        if entry.show_power_off() {
            actions = actions.push(icon_button(
                svg_icon::POWER_SVG,
                theme::MUTED,
                "Power off",
                PopupMessage::PowerOff(entry.serial.clone()),
            ));
        }
    }

    let meta = text(format!("{} · {}", entry.connection, entry.state))
        .size(12.0)
        .color(if entry.low {
            theme::WARNING
        } else {
            theme::DIM
        })
        .width(Fill);

    let remember: Element<'_, PopupMessage> = if entry.remember_enabled {
        let serial = entry.serial.clone();
        checkbox(entry.remembered)
            .label("Remember")
            .size(14.0)
            .text_size(12.0)
            .spacing(5)
            .on_toggle(move |_| PopupMessage::ToggleRemember(serial.clone()))
            .into()
    } else {
        space().into()
    };

    let details = column![
        row![name, actions].spacing(6).align_y(Alignment::Center),
        row![meta, remember].spacing(6).align_y(Alignment::Center),
    ]
    .spacing(5)
    .width(Fill);

    container(
        row![ring, details]
            .spacing(10)
            .align_y(Alignment::Center)
            .width(Fill),
    )
    .padding([8, 10])
    .width(Fill)
    .height(Length::Fixed(ROW_HEIGHT))
    .style(theme::popup_row)
    .into()
}

fn icon_button<'a>(
    source: &'static str,
    color: Color,
    tip: &'static str,
    message: PopupMessage,
) -> Element<'a, PopupMessage> {
    let button = button(
        svg(svg::Handle::from_memory(source.as_bytes()))
            .width(Length::Fixed(ICON_SIZE))
            .height(Length::Fixed(ICON_SIZE))
            .style(move |_theme, _status| svg::Style { color: Some(color) }),
    )
    .padding(4)
    .width(Shrink)
    .height(Shrink)
    .on_press(message)
    .style(theme::ghost);

    tooltip(
        button,
        text(tip).size(12.0).color(theme::INK),
        tooltip::Position::Top,
    )
    .gap(6)
    .padding(6)
    .delay(TOOLTIP_DELAY)
    .style(theme::tooltip)
    .into()
}
