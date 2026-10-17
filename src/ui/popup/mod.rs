//! Tray-anchored controller overview, rendered by the iced daemon.

use crate::controller::dualsense::lightbar;
use crate::controller::known::KnownController;
use crate::controller::model::ControllerStatus;
use crate::ui::color::BatterySpectrum;
use crate::ui::percent_ring::{self, POPUP_SIZE};
use crate::ui::svg_icon;
use crate::ui::{chrome, motion, theme};
use iced::font::Weight;
use iced::widget::{
    Column, button, column, container, row, scrollable, space, svg, text, text_input, tooltip,
};
use iced::{Alignment, Background, Border, Color, Element, Fill, Font, Length, Shrink, Theme};
use std::time::{Duration, Instant};

/// Logical width of the popup window.
pub const WIDTH: f32 = 380.0;
/// Popup grows with row count until this fraction of the monitor height.
const MAX_HEIGHT_FRACTION: f32 = 0.5;
/// Fallback monitor height when iced has not reported one yet.
const FALLBACK_MONITOR_HEIGHT: f32 = 1080.0;

/// Window inset; row cards nest at `window radius − PADDING` (concentric).
const PADDING: f32 = 8.0;
const HEADER_HEIGHT: f32 = 44.0;
const ROW_HEIGHT: f32 = 88.0;
/// Gap between row cards (spacing does the separating — no rules).
const ROW_SPACING: f32 = 6.0;
/// Gap between the header and the first card.
const HEADER_BODY_GAP: f32 = 2.0;
/// Gap between list content and the embedded scrollbar.
const SCROLL_GAP: f32 = 6.0;
const EMPTY_HEIGHT: f32 = 132.0;
const ICON_SIZE: f32 = 17.0;
const NICKNAME_MAX_CHARS: usize = 32;
const TOOLTIP_DELAY: Duration = Duration::from_millis(350);
/// Delay between successive rows starting their entrance fade.
const ENTRANCE_STAGGER_MS: u64 = 45;
/// Rows that animate in; later rows (off-screen in a scroll) appear with the last.
const ENTRANCE_MAX_STAGGERED: usize = 6;

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
            product: controller.product.clone(),
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
    /// When the popup was revealed (drives the staggered row entrance).
    revealed_at: Option<Instant>,
}

impl State {
    /// Start the staggered row entrance (call when the window becomes visible).
    pub fn begin_entrance(&mut self, now: Instant) {
        self.revealed_at = Some(now);
    }

    /// True while rows are still fading in (keep the frame tick alive).
    pub fn entrance_active(&self, now: Instant) -> bool {
        self.revealed_at.is_some_and(|at| {
            let total = entrance_total_ms();
            (now.saturating_duration_since(at).as_millis() as u64) < total
        })
    }

    /// Entrance opacity 0..=1 for row `index` at `now` (1 when no entrance).
    fn row_appear(&self, index: usize, now: Instant) -> f32 {
        let Some(at) = self.revealed_at else {
            return 1.0;
        };
        let elapsed = now.saturating_duration_since(at).as_millis() as u64;
        entrance_appear(index, elapsed)
    }

    pub fn begin_edit(&mut self, serial: &str, current: Option<&str>) {
        self.editing_serial = Some(serial.to_string());
        self.draft = current.unwrap_or_default().to_string();
    }

    pub fn cancel(&mut self) {
        self.editing_serial = None;
        self.draft.clear();
        self.identify_flash = None;
        self.revealed_at = None;
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

/// Total entrance length: last staggered row's delay + one fade.
fn entrance_total_ms() -> u64 {
    (ENTRANCE_MAX_STAGGERED as u64 - 1) * ENTRANCE_STAGGER_MS + motion::BASE_MS
}

/// Ease-out fade for row `index`, `elapsed_ms` after reveal.
fn entrance_appear(index: usize, elapsed_ms: u64) -> f32 {
    let delay = index.min(ENTRANCE_MAX_STAGGERED - 1) as u64 * ENTRANCE_STAGGER_MS;
    motion::ease_out_cubic(motion::progress_linear(
        elapsed_ms.saturating_sub(delay),
        motion::BASE_MS,
    ))
}

/// Non-list chrome: padding + header + gap before the list.
fn chrome_height() -> f32 {
    PADDING * 2.0 + HEADER_HEIGHT + HEADER_BODY_GAP
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

/// Row card corner radius: concentric with the window corner.
fn card_radius() -> f32 {
    (chrome::window_radius() - PADDING).max(theme::radius::SM)
}

/// Scale `color`'s alpha by the entrance `appear` factor.
fn fade(color: Color, appear: f32) -> Color {
    theme::alpha(color, color.a * appear.clamp(0.0, 1.0))
}

pub fn view<'a>(
    state: &'a State,
    rows: &'a [ControllerRow],
    spectrum: &BatterySpectrum,
    powering_off: Option<&str>,
) -> Element<'a, PopupMessage> {
    let now = Instant::now();
    let connected = rows.iter().filter(|r| r.connected).count();
    let subtitle = match (rows.len(), connected) {
        (0, _) => String::new(),
        (_, 0) => "none connected".to_string(),
        (_, n) => format!("{n} connected"),
    };

    let header = row![
        text("Controllers")
            .size(theme::type_scale::TITLE)
            .font(Font {
                weight: Weight::Semibold,
                ..Font::DEFAULT
            })
            .color(theme::INK),
        text(subtitle)
            .size(theme::type_scale::META)
            .color(theme::DIM),
        space().width(Fill),
        icon_button(
            svg_icon::SETTINGS_SVG,
            theme::MUTED,
            "Settings",
            PopupMessage::OpenSettings,
        ),
    ]
    .align_y(Alignment::Center)
    .spacing(10)
    .padding([0, 6])
    .height(Length::Fixed(HEADER_HEIGHT));

    let body: Element<'_, PopupMessage> = if rows.is_empty() {
        empty_state(state.row_appear(0, now))
    } else {
        let list = rows
            .iter()
            .enumerate()
            .fold(
                Column::new().spacing(ROW_SPACING),
                |list, (index, entry)| {
                    list.push(controller_row(
                        state,
                        entry,
                        spectrum,
                        entry.connected && powering_off.is_some_and(|s| s == entry.serial),
                        state.row_appear(index, now),
                    ))
                },
            )
            .width(Fill);

        scrollable(list)
            .spacing(SCROLL_GAP)
            .style(theme::scrollbar)
            .height(Fill)
            .width(Fill)
            .into()
    };

    chrome::window(
        column![header, body]
            .spacing(HEADER_BODY_GAP)
            .padding(PADDING)
            .width(Fill)
            .height(Fill),
    )
}

fn empty_state<'a>(appear: f32) -> Element<'a, PopupMessage> {
    let icon = svg(svg::Handle::from_memory(svg_icon::DUALSENSE_SVG.as_bytes()))
        .width(Length::Fixed(44.0))
        .height(Length::Fixed(44.0))
        .style(move |_theme, _status| svg::Style {
            color: Some(fade(theme::DIM, appear)),
        });
    container(
        column![
            icon,
            text("No controllers yet")
                .size(theme::type_scale::BODY)
                .color(fade(theme::MUTED, appear)),
            text("Turn on a DualSense or plug it in with USB.")
                .size(12.0)
                .color(fade(theme::DIM, appear)),
        ]
        .spacing(6)
        .align_x(Alignment::Center),
    )
    .center_x(Fill)
    .center_y(Length::Fixed(EMPTY_HEIGHT))
    .style(card_style(appear))
    .into()
}

/// Glass row card, faded by the entrance `appear` factor.
fn card_style(appear: f32) -> impl Fn(&Theme) -> container::Style {
    let radius = card_radius();
    move |_theme| container::Style {
        background: Some(Background::Color(fade(theme::GLASS_FILL, appear))),
        text_color: Some(theme::INK),
        border: Border {
            color: fade(theme::GLASS_HAIRLINE, appear),
            width: 1.0,
            radius: radius.into(),
        },
        ..container::Style::default()
    }
}

fn controller_row<'a>(
    state: &'a State,
    entry: &'a ControllerRow,
    spectrum: &BatterySpectrum,
    powering_off: bool,
    appear: f32,
) -> Element<'a, PopupMessage> {
    let live = entry.connected && !powering_off;
    let accent = theme::from_rgb(spectrum.color_at_percent(entry.percent));
    let ring_color = if state.ring_flash_white(&entry.serial) {
        theme::from_rgb(lightbar::IDENTIFY_FLASH)
    } else if live {
        accent
    } else {
        theme::DIM
    };
    let ring = percent_ring::percent_ring(
        entry.percent,
        fade(ring_color, appear),
        POPUP_SIZE,
        entry.eta.clone(),
        1.0,
    );

    let name: Element<'_, PopupMessage> = if state.is_editing(&entry.serial) {
        text_input("Nickname", &state.draft)
            .id(nickname_input_id())
            .on_input(|value| {
                PopupMessage::DraftChanged(value.chars().take(NICKNAME_MAX_CHARS).collect())
            })
            .on_submit(PopupMessage::CommitNickname)
            .size(14.0)
            .padding([4, 8])
            .width(Fill)
            .style(theme::input)
            .into()
    } else {
        text(entry.display_name())
            .size(16.0)
            .font(Font {
                weight: if live {
                    Weight::Semibold
                } else {
                    Weight::Normal
                },
                ..Font::DEFAULT
            })
            .color(fade(if live { theme::INK } else { theme::MUTED }, appear))
            .width(Fill)
            .into()
    };

    let actions = row_actions(state, entry, powering_off);

    let (meta_text, meta_color) = if powering_off {
        ("Powering off".to_string(), theme::MUTED)
    } else {
        (
            format!("{} · {}", entry.connection, entry.state),
            if entry.low {
                theme::WARNING
            } else {
                theme::DIM
            },
        )
    };
    let meta = text(meta_text)
        .size(theme::type_scale::META)
        .color(fade(meta_color, appear))
        .width(Fill);

    let details = column![
        row![name, actions].spacing(6).align_y(Alignment::Center),
        meta
    ]
    .spacing(4)
    .width(Fill);

    container(
        row![ring, details]
            .spacing(14)
            .align_y(Alignment::Center)
            .width(Fill),
    )
    .padding([8, 10])
    .width(Fill)
    .height(Length::Fixed(ROW_HEIGHT))
    .style(card_style(appear))
    .into()
}

/// Trailing icon actions: pin (remember), edit, identify, power off — or
/// save / cancel while editing the nickname.
fn row_actions<'a>(
    state: &State,
    entry: &'a ControllerRow,
    powering_off: bool,
) -> Element<'a, PopupMessage> {
    let mut actions = row![].spacing(0).align_y(Alignment::Center);
    if state.is_editing(&entry.serial) {
        return actions
            .push(icon_button(
                svg_icon::CHECK_SVG,
                theme::SUCCESS,
                "Save nickname",
                PopupMessage::CommitNickname,
            ))
            .push(icon_button(
                svg_icon::CLOSE_SVG,
                theme::MUTED,
                "Cancel",
                PopupMessage::CancelEdit,
            ))
            .into();
    }
    if entry.remember_enabled {
        actions = actions.push(icon_button(
            svg_icon::PIN_SVG,
            if entry.remembered {
                theme::ACCENT
            } else {
                theme::alpha(theme::MUTED, 0.55)
            },
            if entry.remembered {
                "Remembered: stays listed when disconnected"
            } else {
                "Remember this controller"
            },
            PopupMessage::ToggleRemember(entry.serial.clone()),
        ));
    }
    if entry.show_edit() {
        actions = actions.push(icon_button(
            svg_icon::EDIT_SVG,
            theme::MUTED,
            "Edit nickname",
            PopupMessage::BeginEdit(entry.serial.clone()),
        ));
    }
    // A pad on its way out offers no actions (Identify and Power off are
    // both dead ends).
    if !powering_off {
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
    actions.into()
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
    .padding(6)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entrance_staggers_then_settles() {
        assert_eq!(entrance_appear(0, 0), 0.0);
        assert!(entrance_appear(0, 100) > entrance_appear(1, 100));
        let total = entrance_total_ms();
        for index in 0..10 {
            assert_eq!(entrance_appear(index, total), 1.0);
        }
    }

    #[test]
    fn window_height_fits_rows_and_caps() {
        let one = window_height(1, Some(1080.0));
        let two = window_height(2, Some(1080.0));
        assert!((two - one - (ROW_HEIGHT + ROW_SPACING)).abs() < 0.01);
        assert!(window_height(50, Some(1080.0)) <= 540.0);
        assert!(window_height(0, None) > chrome_height());
    }
}
