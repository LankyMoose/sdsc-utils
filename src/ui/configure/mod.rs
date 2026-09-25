//! Configure window UI: sidebar tabs, notification toggles, toast
//! position picker, lightbar spectrum editor, battery analytics, and
//! system / start-screen launcher settings.

mod coverage;
mod spectrum;

use coverage::CoverageChart;
use spectrum::{BAR_HEIGHT, HUE_HEIGHT, HueBar, SV_HEIGHT, SpectrumBar, SvSquare};

#[cfg(feature = "dev-emulate")]
use crate::controller::emulate::Preset;
use crate::persist::analytics::{
    BucketDirection, ControllerAnalytics, InProgressBucket, StepCoverage, format_duration_short,
};
use crate::persist::prefs::{LOW_BATTERY_PERCENT_MAX, LOW_BATTERY_PERCENT_MIN, ToastPosition};
use crate::platform::app_meta::{DISPLAY_NAME, PKG_VERSION};
use crate::ui::color::{BatterySpectrum, hsv_to_rgb};
use crate::ui::start::gesture::{self, GestureControl};
use crate::ui::svg_icon;
use crate::ui::theme;
use iced::mouse;
use iced::widget::{
    Column, Row, button, canvas as canvas_widget, checkbox, column, container, mouse_area, row,
    scrollable, slider, space, svg, text, tooltip,
};
use iced::{Alignment, Color, Element, Fill, Length};
use std::time::Duration;

/// Logical width of the configure window.
pub const WIDTH: f32 = 420.0;
/// Logical height of the configure window.
pub const HEIGHT: f32 = 400.0;

const SIDEBAR_WIDTH: f32 = 120.0;
const CONTENT_PADDING: f32 = 10.0;
/// Gap between scrollable content and the embedded scrollbar.
const SCROLL_GAP: f32 = 8.0;
/// Inset so the scrollbar is not flush to the window frame.
const SCROLL_EDGE: f32 = 8.0;
const HEADER_HEIGHT: f32 = 32.0;
/// Content pane width: window minus sidebar, scroll gutters, and content padding.
const CONTENT_WIDTH: f32 = WIDTH - SIDEBAR_WIDTH - CONTENT_PADDING * 2.0 - SCROLL_GAP - SCROLL_EDGE;

const COVERAGE_HEIGHT: f32 = 56.0;
/// Vertical distance outside the bar that arms stop removal (matches softbuffer UI).
const ICON_SIZE: f32 = 16.0;
/// Representative toast aspect for the position diagram (max width / typical height).
const TOAST_ASPECT: f32 = crate::ui::toast::view::WIDTH / crate::ui::toast::view::HEIGHT;
const POSITION_TOAST_W: f32 = 56.0;
const POSITION_TOAST_MARGIN: f32 = 8.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    System,
    StartScreen,
    Notifications,
    ToastPosition,
    Lightbar,
    Analytics,
    PadInput,
    #[cfg(feature = "dev-emulate")]
    Developer,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::StartScreen => "Start screen",
            Self::Notifications => "Notifications",
            Self::ToastPosition => "Toast position",
            Self::Lightbar => "Lightbar colors",
            Self::Analytics => "Analytics",
            Self::PadInput => "Pad input",
            #[cfg(feature = "dev-emulate")]
            Self::Developer => "Developer",
        }
    }

    fn all(show_developer: bool) -> Vec<Self> {
        #[cfg(feature = "dev-emulate")]
        {
            let mut sections = vec![
                Self::System,
                Self::StartScreen,
                Self::Notifications,
                Self::ToastPosition,
                Self::Lightbar,
                Self::Analytics,
            ];
            if show_developer {
                sections.push(Self::PadInput);
                sections.push(Self::Developer);
            }
            sections
        }
        #[cfg(not(feature = "dev-emulate"))]
        {
            let _ = show_developer;
            vec![
                Self::System,
                Self::StartScreen,
                Self::Notifications,
                Self::ToastPosition,
                Self::Lightbar,
                Self::Analytics,
            ]
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationSetting {
    Connect,
    Disconnect,
    Low,
    Charged,
}

/// Read-only snapshot of app preferences shown by the configure window.
#[derive(Debug, Clone)]
pub struct ConfigureSettings {
    pub notify_low: bool,
    pub notify_charged: bool,
    pub notify_connect: bool,
    pub notify_disconnect: bool,
    pub low_battery_percent: u8,
    pub toast_position: ToastPosition,
    pub analytics_enabled: bool,
    pub lightbar_enabled: bool,
    pub start_screen_enabled: bool,
    pub start_screen_gesture: Vec<GestureControl>,
    pub start_screen_sounds_enabled: bool,
    pub start_screen_sound_volume: u8,
    pub gesture_recording: bool,
    pub gesture_recording_live: String,
    #[cfg(windows)]
    pub autostart: bool,
    pub show_developer: bool,
}

/// Live DualSense / gamepad readings for the Pad input debug tab.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PadInputPanel {
    pub has_sample: bool,
    /// `gamepad`, `hid`, or `none`.
    pub source: String,
    pub buttons0: Option<u8>,
    pub cross: bool,
    pub circle: bool,
    pub dpad_up: bool,
    pub dpad_down: bool,
    pub stick_band: String,
    pub stick_y: f32,
    pub held: String,
}

/// Analytics tab content (owned snapshot; not `Copy` because of open sessions).
#[derive(Debug, Clone, Default)]
pub struct AnalyticsPanel {
    pub rows: Vec<AnalyticsPadRow>,
}

#[derive(Debug, Clone)]
pub struct AnalyticsPadRow {
    pub label: String,
    pub typical_charge: Option<String>,
    pub typical_play: Option<String>,
    pub in_progress: Option<InProgressBucket>,
    pub drain_coverage: Vec<StepCoverage>,
    pub charge_coverage: Vec<StepCoverage>,
}

impl AnalyticsPadRow {
    pub fn from_analytics(row: &ControllerAnalytics, label: String) -> Self {
        Self {
            label,
            typical_charge: row.typical_charge.map(format_duration_short),
            typical_play: row.typical_play.map(format_duration_short),
            in_progress: row.in_progress.clone(),
            drain_coverage: row.drain_coverage.clone(),
            charge_coverage: row.charge_coverage.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum ConfigureMessage {
    SelectSection(Section),
    SetNotification(NotificationSetting, bool),
    SetLowBatteryPercent(u8),
    SetToastPosition(ToastPosition),
    SetAnalyticsEnabled(bool),
    SetLightbarEnabled(bool),
    SetStartScreenEnabled(bool),
    SetStartScreenSounds(bool),
    SetStartScreenSoundVolume(u8),
    StartGestureRecord,
    ResetStartGesture,
    CancelGestureRecord,
    OpenDataFolder,
    #[cfg(windows)]
    SetAutostart(bool),
    SelectStop(usize),
    /// Move the currently selected stop to `percent` (selection tracks reorder).
    MoveStop(u8),
    /// Insert a stop at the clicked percent on the spectrum bar.
    AddStopAt(u8),
    RemoveStop,
    /// Remove the stop at `index` (e.g. right-click on a handle or stop row).
    RemoveStopAt(usize),
    HueChanged(f32),
    SaturationValueChanged(f32, f32),
    ResetSpectrum,
    #[cfg(feature = "dev-emulate")]
    DeveloperPreset(Preset),
    /// Begin an OS window drag (title-bar press on the undecorated window).
    DragWindow,
    Close,
}

/// Mutable UI state owned by the daemon.
#[derive(Debug)]
pub struct ConfigureState {
    pub section: Section,
    pub spectrum: BatterySpectrum,
    pub selected: usize,
    pub hue: f32,
    pub saturation: f32,
    pub value: f32,
    pub error: Option<String>,
}

impl ConfigureState {
    pub fn new(spectrum: BatterySpectrum) -> Self {
        let mut state = Self {
            section: Section::System,
            spectrum,
            selected: 0,
            hue: 0.0,
            saturation: 0.0,
            value: 0.0,
            error: None,
        };
        state.sync_hsv();
        state
    }

    fn sync_hsv(&mut self) {
        if self.spectrum.stops.is_empty() {
            return;
        }
        self.selected = self.selected.min(self.spectrum.stops.len() - 1);
        let (hue, saturation, value) = self.spectrum.stops[self.selected].color.to_hsv();
        // Keep the current hue when the color is fully desaturated, otherwise the
        // picker snaps back to red every time the user drags value to zero.
        if saturation > f32::EPSILON || value > f32::EPSILON {
            self.hue = hue;
        }
        self.saturation = saturation;
        self.value = value;
    }

    /// Adopt a spectrum applied elsewhere (e.g. loaded prefs).
    pub fn set_spectrum(&mut self, spectrum: BatterySpectrum) {
        self.spectrum = spectrum;
        self.sync_hsv();
    }

    pub fn select_section(&mut self, section: Section) {
        self.section = section;
    }

    pub fn select(&mut self, index: usize) {
        if index < self.spectrum.stops.len() {
            self.selected = index;
            self.sync_hsv();
            self.error = None;
        }
    }

    fn apply_selected_color(&mut self) -> Option<BatterySpectrum> {
        let color = hsv_to_rgb(self.hue, self.saturation, self.value);
        match self.spectrum.set_stop_color(self.selected, color) {
            Ok(()) => {
                self.error = None;
                Some(self.spectrum.clone())
            }
            Err(err) => {
                self.error = Some(err);
                None
            }
        }
    }

    pub fn set_hue(&mut self, hue: f32) -> Option<BatterySpectrum> {
        self.hue = hue.rem_euclid(360.0);
        self.apply_selected_color()
    }

    pub fn set_saturation_value(&mut self, saturation: f32, value: f32) -> Option<BatterySpectrum> {
        self.saturation = saturation.clamp(0.0, 1.0);
        self.value = value.clamp(0.0, 1.0);
        self.apply_selected_color()
    }

    pub fn move_stop(&mut self, percent: u8) -> Option<BatterySpectrum> {
        match self.spectrum.set_stop_percent(self.selected, percent) {
            Ok(new_index) => {
                self.selected = new_index;
                self.error = None;
                Some(self.spectrum.clone())
            }
            Err(err) => {
                self.error = Some(err);
                None
            }
        }
    }

    pub fn add_stop_at(&mut self, percent: u8) -> Option<BatterySpectrum> {
        match self.spectrum.add_stop_at(percent) {
            Ok(index) => {
                self.selected = index;
                self.sync_hsv();
                self.error = None;
                Some(self.spectrum.clone())
            }
            Err(err) => {
                if let Some(index) = self
                    .spectrum
                    .stops
                    .iter()
                    .position(|stop| stop.percent == percent)
                {
                    self.select(index);
                    return None;
                }
                self.error = Some(err);
                None
            }
        }
    }

    pub fn remove_selected(&mut self) -> Option<BatterySpectrum> {
        match self.spectrum.remove_stop(self.selected) {
            Ok(index) => {
                self.selected = index;
                self.sync_hsv();
                self.error = None;
                Some(self.spectrum.clone())
            }
            Err(err) => {
                self.error = Some(err);
                None
            }
        }
    }

    pub fn remove_at(&mut self, index: usize) -> Option<BatterySpectrum> {
        if index >= self.spectrum.stops.len() {
            return None;
        }
        self.selected = index;
        self.remove_selected()
    }

    pub fn reset(&mut self) -> BatterySpectrum {
        self.spectrum = BatterySpectrum::default_spectrum();
        self.selected = 0;
        self.sync_hsv();
        self.error = None;
        self.spectrum.clone()
    }
}

/// Midpoint of the widest gap between existing stops.
#[cfg(test)]
pub fn suggested_stop_percent(spectrum: &BatterySpectrum) -> u8 {
    let mut percents: Vec<u8> = spectrum.stops.iter().map(|stop| stop.percent).collect();
    percents.sort_unstable();
    percents.dedup();

    let mut best = 50u8;
    let mut widest = 0u16;
    let mut previous = 0u8;
    for (index, percent) in percents.iter().copied().enumerate() {
        let low = if index == 0 { 0 } else { previous };
        let gap = percent.saturating_sub(low) as u16;
        if gap > widest {
            widest = gap;
            best = low + (gap / 2) as u8;
        }
        previous = percent;
    }
    let tail = 100u16.saturating_sub(previous as u16);
    if tail > widest {
        best = previous + (tail / 2) as u8;
    }

    if percents.contains(&best) {
        (0..=100u8)
            .find(|candidate| !percents.contains(candidate))
            .unwrap_or(best)
    } else {
        best
    }
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

pub fn view<'a>(
    state: &'a ConfigureState,
    settings: &ConfigureSettings,
    analytics: &'a AnalyticsPanel,
    pad_input: &'a PadInputPanel,
) -> Element<'a, ConfigureMessage> {
    let title = mouse_area(
        container(text("Settings").size(16.0).color(theme::INK))
            .width(Fill)
            .height(Fill)
            .align_y(Alignment::Center),
    )
    .on_press(ConfigureMessage::DragWindow)
    .interaction(mouse::Interaction::Grab);

    let header = row![
        title,
        tooltip(
            button(
                svg(svg::Handle::from_memory(svg_icon::CLOSE_SVG.as_bytes()))
                    .width(Length::Fixed(ICON_SIZE))
                    .height(Length::Fixed(ICON_SIZE))
                    .style(|_theme, _status| svg::Style {
                        color: Some(theme::MUTED)
                    }),
            )
            .padding(4)
            .on_press(ConfigureMessage::Close)
            .style(theme::ghost),
            text("Close").size(12.0).color(theme::INK),
            tooltip::Position::Bottom,
        )
        .gap(6)
        .padding(6)
        .delay(Duration::from_millis(350))
        .style(theme::tooltip),
    ]
    .align_y(Alignment::Center)
    .spacing(6)
    .height(Length::Fixed(HEADER_HEIGHT));

    let sidebar = tab_list(state.section, settings.show_developer);
    let content = section_content(state, settings, analytics, pad_input);

    let body = row![
        container(sidebar)
            .width(Length::Fixed(SIDEBAR_WIDTH))
            .height(Fill)
            .style(theme::sidebar),
        // Embed scrollbar with gutters: content ↔ bar ↔ window edge.
        container(
            scrollable(container(content).padding(CONTENT_PADDING).width(Fill))
                .spacing(SCROLL_GAP)
                .height(Fill)
                .width(Fill),
        )
        .padding(iced::Padding::ZERO.right(SCROLL_EDGE))
        .width(Fill)
        .height(Fill),
    ]
    .spacing(0)
    .width(Fill)
    .height(Fill);

    let chrome = column![
        container(header)
            .padding([0.0, CONTENT_PADDING])
            .width(Fill),
        // Inset like Start: do not meet the window side borders, or the
        // underline + root border read as a box around only the title.
        container(
            container(space())
                .width(Fill)
                .height(Length::Fixed(1.0))
                .style(theme::configure_header_rule),
        )
        .padding([0.0, CONTENT_PADDING])
        .width(Fill),
    ]
    .spacing(0)
    .width(Fill);

    theme::framed(
        column![
            chrome,
            container(body)
                .width(Fill)
                .height(Fill)
                .style(theme::configure_body),
        ]
        .width(Fill)
        .height(Fill),
    )
}

fn tab_list<'a>(active: Section, show_developer: bool) -> Element<'a, ConfigureMessage> {
    Section::all(show_developer)
        .into_iter()
        .fold(Column::new().spacing(2).width(Fill), |list, section| {
            let selected = active == section;
            list.push(
                button(
                    row![
                        container(space())
                            .width(Length::Fixed(2.0))
                            .height(Length::Fixed(12.0))
                            .style(theme::position_rail(selected)),
                        text(section.title()).size(13.0).width(Fill),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                )
                .padding([5, 6])
                .width(Fill)
                .on_press(ConfigureMessage::SelectSection(section))
                .style(theme::tab(selected)),
            )
        })
        .into()
}

fn section_content<'a>(
    state: &'a ConfigureState,
    settings: &ConfigureSettings,
    analytics: &'a AnalyticsPanel,
    pad_input: &'a PadInputPanel,
) -> Element<'a, ConfigureMessage> {
    match state.section {
        Section::System => system_view(settings),
        Section::StartScreen => start_screen_view(settings),
        Section::Notifications => notifications_view(settings),
        Section::ToastPosition => toast_position_view(settings, theme::ACCENT),
        Section::Lightbar => lightbar_view(state, settings),
        Section::Analytics => analytics_view(settings, analytics),
        Section::PadInput => pad_input_view(pad_input),
        #[cfg(feature = "dev-emulate")]
        Section::Developer => developer_view(),
    }
}

fn system_view<'a>(settings: &ConfigureSettings) -> Element<'a, ConfigureMessage> {
    let mut items = Column::new().spacing(8).width(Fill);

    #[cfg(windows)]
    {
        items = items.push(
            checkbox(settings.autostart)
                .label("Start with Windows")
                .size(16.0)
                .text_size(13.0)
                .spacing(8)
                .on_toggle(ConfigureMessage::SetAutostart),
        );
    }

    items = items.push(
        button(text("Open data folder").size(13.0))
            .padding([6, 10])
            .on_press(ConfigureMessage::OpenDataFolder)
            .style(theme::ghost),
    );

    items = items.push(
        text(format!("{DISPLAY_NAME} {PKG_VERSION}"))
            .size(12.0)
            .color(theme::DIM),
    );

    items.into()
}

fn start_screen_view<'a>(settings: &ConfigureSettings) -> Element<'a, ConfigureMessage> {
    let mut items = Column::new().spacing(8).width(Fill);

    items = items.push(
        checkbox(settings.start_screen_enabled)
            .label("Show start screen when a controller connects")
            .size(16.0)
            .text_size(13.0)
            .spacing(8)
            .on_toggle(ConfigureMessage::SetStartScreenEnabled),
    );

    if settings.start_screen_enabled {
        items = items.push(text("Reopen gesture").size(13.0).color(theme::INK));
        items = items.push(
            text(gesture::format_gesture(&settings.start_screen_gesture))
                .size(12.0)
                .color(theme::MUTED),
        );

        if settings.gesture_recording {
            items = items.push(
                text(if settings.gesture_recording_live.is_empty() {
                    "Hold combo, then release…".to_string()
                } else {
                    format!("Holding: {}", settings.gesture_recording_live)
                })
                .size(12.0)
                .color(theme::ACCENT),
            );
            items = items.push(
                button(text("Cancel recording").size(12.0))
                    .padding([5, 8])
                    .on_press(ConfigureMessage::CancelGestureRecord)
                    .style(theme::ghost),
            );
        } else {
            items = items.push(
                row![
                    button(text("Record").size(12.0))
                        .padding([5, 8])
                        .on_press(ConfigureMessage::StartGestureRecord)
                        .style(theme::primary),
                    button(text("Reset to default").size(12.0))
                        .padding([5, 8])
                        .on_press(ConfigureMessage::ResetStartGesture)
                        .style(theme::ghost),
                ]
                .spacing(6),
            );
        }

        items = items.push(
            checkbox(settings.start_screen_sounds_enabled)
                .label("UI sounds")
                .size(16.0)
                .text_size(13.0)
                .spacing(8)
                .on_toggle(ConfigureMessage::SetStartScreenSounds),
        );

        if settings.start_screen_sounds_enabled {
            let volume = settings.start_screen_sound_volume;
            items = items.push(
                column![
                    text(format!("Volume {volume}%"))
                        .size(12.0)
                        .color(theme::MUTED),
                    slider(0.0..=100.0, f32::from(volume), |value| {
                        ConfigureMessage::SetStartScreenSoundVolume(value.round() as u8)
                    })
                    .step(5.0_f32),
                ]
                .spacing(4)
                .width(Fill),
            );
        }
    }

    items.into()
}

fn notifications_view<'a>(settings: &ConfigureSettings) -> Element<'a, ConfigureMessage> {
    let toggle = |label: &'static str, value: bool, setting: NotificationSetting| {
        checkbox(value)
            .label(label)
            .size(16.0)
            .text_size(13.0)
            .spacing(8)
            .on_toggle(move |enabled| ConfigureMessage::SetNotification(setting, enabled))
    };

    let mut items = column![
        toggle(
            "Connected",
            settings.notify_connect,
            NotificationSetting::Connect
        ),
        toggle(
            "Disconnected",
            settings.notify_disconnect,
            NotificationSetting::Disconnect
        ),
        toggle("Low battery", settings.notify_low, NotificationSetting::Low),
    ]
    .spacing(8)
    .width(Fill);

    if settings.notify_low {
        let threshold = settings.low_battery_percent;
        items = items.push(
            column![
                text(format!("At or below {threshold}%"))
                    .size(12.0)
                    .color(theme::MUTED),
                slider(
                    f32::from(LOW_BATTERY_PERCENT_MIN)..=f32::from(LOW_BATTERY_PERCENT_MAX),
                    f32::from(threshold),
                    |value| ConfigureMessage::SetLowBatteryPercent(value.round() as u8),
                )
                .step(10.0_f32),
            ]
            .spacing(4)
            .width(Fill),
        );
    }

    items = items.push(toggle(
        "Finished charging",
        settings.notify_charged,
        NotificationSetting::Charged,
    ));

    items.into()
}

fn analytics_view<'a>(
    settings: &ConfigureSettings,
    panel: &'a AnalyticsPanel,
) -> Element<'a, ConfigureMessage> {
    let mut items = Column::new().spacing(10).width(Fill);

    items = items.push(
        checkbox(settings.analytics_enabled)
            .label("Record battery analytics")
            .size(16.0)
            .text_size(13.0)
            .spacing(8)
            .on_toggle(ConfigureMessage::SetAnalyticsEnabled),
    );

    items = items.push(
        text("Local only. Full charge / full drain are estimated totals for a complete cycle. Remaining time on the tray uses your current (or last-known) percent. Mid-cycle unplug or charge does not wipe learned steps.")
            .size(12.0)
            .color(theme::DIM),
    );

    if settings.analytics_enabled {
        if panel.rows.is_empty() {
            items = items.push(
                text("Learning… play or charge through a battery step to seed estimates.")
                    .size(12.0)
                    .color(theme::MUTED),
            );
        } else {
            for row in &panel.rows {
                items = items.push(analytics_pad_card(row));
            }
        }
    }

    items.into()
}

fn analytics_pad_card<'a>(row: &'a AnalyticsPadRow) -> Element<'a, ConfigureMessage> {
    let charge = row
        .typical_charge
        .as_ref()
        .map(|d| format!("Full charge {d}"))
        .unwrap_or_else(|| "Full charge Learning…".to_string());
    let play = row
        .typical_play
        .as_ref()
        .map(|d| format!("Full drain {d}"))
        .unwrap_or_else(|| "Full drain Learning…".to_string());

    let mut col = Column::new()
        .spacing(6)
        .width(Fill)
        .push(text(&row.label).size(13.0).color(theme::INK))
        .push(
            text(format!("{charge} · {play}"))
                .size(12.0)
                .color(theme::MUTED),
        );

    if let Some(progress) = row.in_progress.as_ref() {
        let label = match progress.direction {
            BucketDirection::Drain => format!("Recording drain at {}%", progress.percent),
            BucketDirection::Charge => format!("Recording charge at {}%", progress.percent),
        };
        col = col.push(text(label).size(12.0).color(theme::DIM));
    }

    col = col
        .push(text("Play coverage").size(11.0).color(theme::DIM))
        .push(
            canvas_widget(CoverageChart {
                steps: row.drain_coverage.clone(),
                drain: true,
            })
            .width(Fill)
            .height(Length::Fixed(COVERAGE_HEIGHT)),
        )
        .push(text("Charge coverage").size(11.0).color(theme::DIM))
        .push(
            canvas_widget(CoverageChart {
                steps: row.charge_coverage.clone(),
                drain: false,
            })
            .width(Fill)
            .height(Length::Fixed(COVERAGE_HEIGHT)),
        );

    container(col)
        .padding(8)
        .width(Fill)
        .style(theme::surface)
        .into()
}

fn toast_position_view<'a>(
    settings: &ConfigureSettings,
    accent: Color,
) -> Element<'a, ConfigureMessage> {
    // Match the softbuffer diagram: 16:9 “monitor” with mini toast cards at each corner/edge.
    let stage_width = CONTENT_WIDTH;
    let stage_height = stage_width * 9.0 / 16.0;
    let toast_h = (POSITION_TOAST_W / TOAST_ASPECT).max(10.0);

    let marker = |position: ToastPosition| {
        let selected = settings.toast_position == position;
        button(
            row![
                container(space())
                    .width(Length::Fixed(2.0))
                    .height(Fill)
                    .style(theme::position_rail(selected))
            ]
            .padding([2, 0])
            .height(Fill),
        )
        .padding(0)
        .width(Length::Fixed(POSITION_TOAST_W))
        .height(Length::Fixed(toast_h))
        .on_press(ConfigureMessage::SetToastPosition(position))
        .style(theme::position_marker(selected, accent))
    };

    let row_of = |left: ToastPosition, center: ToastPosition, right: ToastPosition| {
        Row::new()
            .spacing(0)
            .width(Fill)
            .align_y(Alignment::Center)
            .push(marker(left))
            .push(space().width(Fill))
            .push(marker(center))
            .push(space().width(Fill))
            .push(marker(right))
    };

    container(
        column![
            row_of(
                ToastPosition::TopLeft,
                ToastPosition::TopCenter,
                ToastPosition::TopRight,
            ),
            space().height(Fill),
            row_of(
                ToastPosition::BottomLeft,
                ToastPosition::BottomCenter,
                ToastPosition::BottomRight,
            ),
        ]
        .width(Fill)
        .height(Fill),
    )
    .padding(POSITION_TOAST_MARGIN)
    .width(Fill)
    .height(Length::Fixed(stage_height))
    .style(theme::position_stage)
    .into()
}

fn lightbar_view<'a>(
    state: &'a ConfigureState,
    settings: &ConfigureSettings,
) -> Element<'a, ConfigureMessage> {
    let mut content = Column::new().spacing(8).width(Fill);

    content = content.push(
        checkbox(settings.lightbar_enabled)
            .label("Enable lightbar")
            .size(16.0)
            .text_size(13.0)
            .spacing(8)
            .on_toggle(ConfigureMessage::SetLightbarEnabled),
    );

    if !settings.lightbar_enabled {
        content = content.push(
            text("Battery-driven lightbar colors and the low-battery pulse are paused. Identify still works.")
                .size(12.0)
                .color(theme::DIM),
        );
        return content.into();
    }

    let bar = canvas_widget(SpectrumBar {
        stops: state.spectrum.stops.clone(),
        selected: state.selected,
    })
    .width(Fill)
    .height(Length::Fixed(BAR_HEIGHT));

    let stops = state.spectrum.stops.iter().enumerate().fold(
        Column::new().spacing(4).width(Fill),
        |list, (index, stop)| {
            let selected = index == state.selected;
            list.push(
                mouse_area(
                    button(
                        row![
                            container(space())
                                .width(Length::Fixed(14.0))
                                .height(Length::Fixed(14.0))
                                .style(theme::swatch(theme::from_rgb(stop.color))),
                            text(format!("{}%", stop.percent)).size(12.0).width(Fill),
                            text(stop.color.to_hex()).size(12.0).color(theme::MUTED),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                    )
                    .padding([4, 6])
                    .width(Fill)
                    .on_press(ConfigureMessage::SelectStop(index))
                    .style(theme::chip(selected)),
                )
                .on_right_press(ConfigureMessage::RemoveStopAt(index)),
            )
        },
    );

    let sv = canvas_widget(SvSquare {
        hue: state.hue,
        saturation: state.saturation,
        value: state.value,
    })
    .width(Fill)
    .height(Length::Fixed(SV_HEIGHT));

    let hue = canvas_widget(HueBar { hue: state.hue })
        .width(Fill)
        .height(Length::Fixed(HUE_HEIGHT));

    let reset = button(text("Reset defaults").size(12.0).center().width(Fill))
        .padding([6, 4])
        .width(Fill)
        .on_press(ConfigureMessage::ResetSpectrum)
        .style(theme::chip(false));

    content = content.push(container(bar).width(Fill).style(theme::well));
    content = content.push(stops);
    content = content.push(container(sv).width(Fill).style(theme::well));
    content = content.push(container(hue).width(Fill).style(theme::well));
    content = content.push(reset);

    if let Some(error) = state.error.as_deref() {
        content = content.push(text(error).size(12.0).color(theme::WARNING));
    }

    content.into()
}

fn pad_input_view<'a>(panel: &'a PadInputPanel) -> Element<'a, ConfigureMessage> {
    let mut items = Column::new().spacing(8).width(Fill);

    items = items.push(
        text("Live DualSense readings for navigation debugging.")
            .size(12.0)
            .color(theme::MUTED),
    );

    if !panel.has_sample {
        items = items.push(
            text("No DualSense HID reading (disconnected or exclusive)")
                .size(13.0)
                .color(theme::DIM),
        );
        return items.into();
    }

    let source_line = match panel.buttons0 {
        Some(b0) => format!("source={}  buttons0=0x{b0:02x}", panel.source),
        None => format!("source={}", panel.source),
    };
    items = items.push(text(source_line).size(13.0).color(theme::INK));

    let bit = |label: &str, on: bool| {
        text(format!("{label}={}", u8::from(on)))
            .size(13.0)
            .color(if on { theme::INK } else { theme::MUTED })
    };

    items = items.push(
        column![
            bit("cross", panel.cross),
            bit("circle", panel.circle),
            bit("dpad_up", panel.dpad_up),
            bit("dpad_down", panel.dpad_down),
            text(format!(
                "stick={}  stick_y={:.2}",
                panel.stick_band, panel.stick_y
            ))
            .size(13.0)
            .color(theme::INK),
        ]
        .spacing(4),
    );

    let held = if panel.held.is_empty() {
        "(none)".to_string()
    } else {
        panel.held.clone()
    };
    items = items.push(
        column![
            text("Held").size(11.0).color(theme::DIM),
            text(held).size(13.0).color(theme::INK),
        ]
        .spacing(2),
    );

    items.into()
}

#[cfg(feature = "dev-emulate")]
fn developer_view<'a>() -> Element<'a, ConfigureMessage> {
    let mut list = Column::new().spacing(4).width(Fill);

    list = list.push(text("Controllers").size(12.0).color(theme::DIM));
    for preset in [
        Preset::Discharging50,
        Preset::LowBattery,
        Preset::Charging,
        Preset::FullyCharged,
        Preset::ChargeCompleteStep,
        Preset::TwoPads,
        Preset::Clear,
    ] {
        list = list.push(dev_preset_button(preset));
    }

    list = list.push(space().height(Length::Fixed(6.0)));
    list = list.push(text("Battery analytics").size(12.0).color(theme::DIM));
    list = list.push(
        text("Enable is toggled on automatically. Seed shows est. in the ring; other steps walk a timed cycle.")
            .size(11.0)
            .color(theme::DIM),
    );
    for preset in [
        Preset::AnalyticsSeedEstimates,
        Preset::AnalyticsPlugEmpty,
        Preset::AnalyticsChargeAdvance,
        Preset::AnalyticsUnplugFull,
        Preset::AnalyticsDrainAdvance,
        Preset::AnalyticsPause,
        Preset::AnalyticsResume,
        Preset::AnalyticsPlugMidDrain,
    ] {
        list = list.push(dev_preset_button(preset));
    }

    list.into()
}

#[cfg(feature = "dev-emulate")]
fn dev_preset_button<'a>(preset: Preset) -> Element<'a, ConfigureMessage> {
    button(text(preset.menu_label()).size(12.0).width(Fill))
        .padding([5, 8])
        .width(Fill)
        .on_press(ConfigureMessage::DeveloperPreset(preset))
        .style(theme::row_button)
        .into()
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggested_percent_lands_in_the_widest_gap() {
        let spectrum = BatterySpectrum::default_spectrum();
        let percent = suggested_stop_percent(&spectrum);
        assert!(percent > 0 && percent < 100);
        assert!(!spectrum.stops.iter().any(|stop| stop.percent == percent));
    }

    #[test]
    fn state_tracks_selection_after_reorder() {
        let mut state = ConfigureState::new(BatterySpectrum::default_spectrum());
        state.select(1);
        let color = state.spectrum.stops[1].color;
        assert!(state.move_stop(5).is_some());
        assert_eq!(state.spectrum.stops[state.selected].color, color);
    }

    #[test]
    fn removing_below_the_minimum_reports_an_error() {
        let mut state = ConfigureState::new(BatterySpectrum::default_spectrum());
        assert!(state.remove_selected().is_some());
        assert!(state.remove_selected().is_none());
        assert!(state.error.is_some());
    }
}
