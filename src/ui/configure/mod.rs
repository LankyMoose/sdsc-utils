//! Configure window UI: sidebar tabs, notification toggles, toast
//! position picker, lightbar spectrum editor, battery analytics, and
//! system / start-screen launcher settings.

pub mod changelog;
mod coverage;
mod spectrum;

use coverage::CoverageChart;
use spectrum::{BAR_HEIGHT, HUE_HEIGHT, HueBar, SV_HEIGHT, SpectrumBar, SvSquare};

#[cfg(debug_assertions)]
#[cfg(debug_assertions)]
use crate::controller::emulate::{self, EmulatedPad};
#[cfg(debug_assertions)]
use crate::controller::model::{Connection, PowerState};
use crate::games::steam::LAST_KNOWN_COMPATIBLE_STEAM_VERSION;
use crate::persist::analytics::{
    BucketDirection, ControllerAnalytics, InProgressBucket, StepCoverage, format_duration_short,
};
use crate::persist::prefs::{
    AUTO_OPEN_MODES, IMMERSIVE_LAYOUT_MODES, ImmersiveLayout, LOW_BATTERY_PERCENT_MAX,
    LOW_BATTERY_PERCENT_MIN, StartAutoOpen, ToastPosition,
};
use crate::platform::app_meta::{DISPLAY_NAME, PKG_VERSION};
use crate::ui::color::{BatterySpectrum, hsv_to_rgb};
use crate::ui::svg_icon;
use crate::ui::{chrome, theme};
use iced::font::Weight;
use iced::mouse;
use iced::widget::{
    Column, Row, button, canvas as canvas_widget, column, container, hover, mouse_area, pick_list,
    row, scrollable, slider, space, stack, svg, text, toggler, tooltip,
};
use iced::{Alignment, Color, Element, Fill, Font, Length};
use std::path::PathBuf;
use std::time::Duration;

/// Logical width of the configure window.
pub const WIDTH: f32 = 720.0;
/// Logical height of the configure window.
pub const HEIGHT: f32 = 520.0;

/// Window inset around the sidebar island (concentric with the window corner).
const WINDOW_PAD: f32 = 8.0;
const SIDEBAR_WIDTH: f32 = 188.0;
/// Gap between the sidebar island and the content column.
const SIDEBAR_GAP: f32 = 8.0;
const CONTENT_PADDING: f32 = 16.0;
/// Gap between scrollable content and the embedded scrollbar.
const SCROLL_GAP: f32 = 8.0;
/// Inset so the scrollbar is not flush to the window frame.
const SCROLL_EDGE: f32 = 6.0;
const HEADER_HEIGHT: f32 = 52.0;
/// Inner padding of a settings group card.
const GROUP_PAD: f32 = 14.0;
/// Content pane width: window minus inset, sidebar, gutters, and padding.
const CONTENT_WIDTH: f32 = WIDTH
    - WINDOW_PAD * 2.0
    - SIDEBAR_WIDTH
    - SIDEBAR_GAP
    - CONTENT_PADDING * 2.0
    - SCROLL_GAP
    - SCROLL_EDGE;

const COVERAGE_HEIGHT: f32 = 56.0;
/// Vertical distance outside the bar that arms stop removal (matches softbuffer UI).
const ICON_SIZE: f32 = 16.0;
/// Footer store / GitHub marks — slightly larger so the Store logo stays readable.
const FOOTER_ICON_SIZE: f32 = 20.0;
/// Representative toast aspect for the position diagram (max width / typical height).
const TOAST_ASPECT: f32 = crate::ui::toast::view::WIDTH / crate::ui::toast::view::HEIGHT;
const POSITION_TOAST_W: f32 = 56.0;
const POSITION_TOAST_MARGIN: f32 = 8.0;

const GITHUB_URL: &str = "https://github.com/LankyMoose/sdsc-utils";
const MICROSOFT_STORE_URL: &str =
    "https://apps.microsoft.com/store/detail/9NDMHS9RKPP0?cid=in-app-link";

/// Scrollable widget id for the Configure content pane.
///
/// Single owner so `on_configure_message` / `debug_open` section changes can
/// `scroll_to` top through one place. Only one content scrollable is mounted
/// at a time (System vs. other sections share the id via the `scroll` closure).
pub fn content_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("configure-content-scroll")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    System,
    StartScreen,
    Notifications,
    Lightbar,
    Analytics,
    /// Live controller readings. Only listed in the sidebar for debug builds
    /// (see [`Section::all`]), but the variant exists in both so the pad-input
    /// view stays reachable from code and tests.
    PadInput,
    /// Debug-only emulated controllers.
    #[cfg(debug_assertions)]
    Emulators,
    /// Debug-only renderer diagnostics (window stress test).
    #[cfg(debug_assertions)]
    Diagnostics,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::StartScreen => "Start screen",
            Self::Notifications => "Notifications",
            Self::Lightbar => "Lightbar colors",
            Self::Analytics => "Analytics",
            Self::PadInput => "Pad input",
            #[cfg(debug_assertions)]
            Self::Emulators => "Controllers",
            #[cfg(debug_assertions)]
            Self::Diagnostics => "Diagnostics",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Self::System => "Startup and app data",
            Self::StartScreen => "Game launcher you open from your controller",
            Self::Notifications => "Which controller events show a toast",
            Self::Lightbar => "Lightbar color as the battery drains",
            Self::Analytics => "Learned charge and play times",
            Self::PadInput => "Live controller readings",
            #[cfg(debug_assertions)]
            Self::Emulators => "Emulated controllers",
            #[cfg(debug_assertions)]
            Self::Diagnostics => "Renderer stress testing",
        }
    }

    /// Compiled out of release builds; the sidebar labels the group.
    fn is_debug_only(self) -> bool {
        #[cfg(debug_assertions)]
        {
            matches!(self, Self::PadInput | Self::Emulators | Self::Diagnostics)
        }
        #[cfg(not(debug_assertions))]
        {
            let _ = self;
            false
        }
    }

    /// Sidebar sections. `PadInput` / `Emulators` / `Diagnostics` are
    /// debug-only and compiled out of release builds entirely.
    fn all() -> Vec<Self> {
        #[cfg(debug_assertions)]
        {
            vec![
                Self::System,
                Self::StartScreen,
                Self::Notifications,
                Self::Lightbar,
                Self::Analytics,
                Self::PadInput,
                Self::Emulators,
                Self::Diagnostics,
            ]
        }
        #[cfg(not(debug_assertions))]
        {
            vec![
                Self::System,
                Self::StartScreen,
                Self::Notifications,
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
    pub start_screen_auto_open: StartAutoOpen,
    pub start_screen_always_immersive: bool,
    pub start_screen_immersive_layout: ImmersiveLayout,
    pub start_screen_clock_enabled: bool,
    pub start_screen_inactive_secs: u32,
    pub start_screen_sleep_secs: u32,
    pub start_screen_inactive_dim_percent: u8,
    pub start_screen_sounds_enabled: bool,
    pub start_screen_sound_volume: u8,
    pub start_screen_haptics_enabled: bool,
    pub start_screen_haptics_strength: u8,
    #[cfg(windows)]
    pub autostart: bool,
    pub extra_steam_paths: Vec<PathBuf>,
    pub steam_path_error: Option<String>,
}

/// Edit applied to the emulated fleet (Controllers debug tab).
///
/// Every variant carries the pad's serial so an action can never land on the
/// wrong pad after the list shifts.
#[cfg(debug_assertions)]
#[derive(Debug, Clone, PartialEq)]
pub enum EmulatorCommand {
    /// Spawn a pad on the lowest free slot, remembered but disconnected.
    AddPad,
    RemovePad(String),
    /// Live drag preview — repaints the tab only.
    ///
    /// Publication is debounced by the daemon (publishing is what lets the
    /// normal preference-driven reactions fire), so this never spams per
    /// frame; a spinner marks the wait.
    PreviewPercent(String, u8),
    /// Move the pad to a link — USB or Bluetooth. The charge state follows:
    /// see [`crate::controller::emulate::state_for`].
    SetConnection(String, Connection),
    /// Plug in / unplug — runs the real connect or disconnect path.
    SetConnected(String, bool),
    /// Keep the pad across restarts (the popup's remember pin). A pad that is
    /// both unplugged and forgotten is dropped from the list.
    SetRemembered(String, bool),
    /// Seed typical charge/play samples so ETA estimates exist.
    SeedAnalytics(String),
    /// Credit active time and walk one step up the charge cycle.
    StepCharge(String),
    /// Credit active time and walk one step down the drain cycle.
    StepDrain(String),
}

/// A battery level previewed but not yet published (`None` when settled).
#[cfg(debug_assertions)]
#[derive(Debug, Clone, PartialEq)]
pub struct PendingEmulatorChange {
    pub serial: String,
    pub percent: u8,
    /// Frame counter driving the pending spinner.
    pub tick: u32,
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
    pub dpad_left: bool,
    pub dpad_right: bool,
    pub stick_band: String,
    pub stick_x: f32,
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
    SetStartScreenAutoOpen(StartAutoOpen),
    SetStartScreenAlwaysImmersive(bool),
    SetImmersiveLayout(ImmersiveLayout),
    SetStartScreenClock(bool),
    SetStartScreenInactiveSecs(u32),
    SetStartScreenSleepSecs(u32),
    SetStartScreenInactiveDimPercent(u8),
    SetStartScreenSounds(bool),
    SetStartScreenSoundVolume(u8),
    SetStartScreenHaptics(bool),
    SetStartScreenHapticsStrength(u8),
    OpenDataFolder,
    OpenExternalLink(&'static str),
    OpenChangelog,
    CloseChangelog,
    AddSteamPath,
    RemoveSteamPath(usize),
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
    #[cfg(debug_assertions)]
    Emulator(EmulatorCommand),
    /// Serial of a row whose controls are expanded.
    /// Expand a pad row.
    #[cfg(debug_assertions)]
    ToggleEmulator(String),
    /// Debug-only: start the renderer window stress test (Diagnostics section).
    #[cfg(debug_assertions)]
    RunWindowStress,
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
    pub show_changelog: bool,
    pub steam_path_error: Option<String>,
    /// Serials of expanded pad rows (Controllers tab), keyed so rows stay
    /// independently expandable as pads come and go.
    #[cfg(debug_assertions)]
    pub expanded_emulators: std::collections::HashSet<String>,
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
            show_changelog: false,
            steam_path_error: None,
            #[cfg(debug_assertions)]
            expanded_emulators: std::collections::HashSet::new(),
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
    #[cfg(debug_assertions)] emulators: &'a [EmulatedPad],
    #[cfg(debug_assertions)] pending: &[PendingEmulatorChange],
) -> Element<'a, ConfigureMessage> {
    // Section heading doubles as the drag handle for the undecorated window.
    let heading = mouse_area(
        container(
            column![
                text(state.section.title())
                    .size(theme::type_scale::TITLE)
                    .font(semibold())
                    .color(theme::INK),
                text(state.section.subtitle())
                    .size(theme::type_scale::META)
                    .color(theme::DIM),
            ]
            .spacing(2),
        )
        .width(Fill)
        .height(Fill)
        .align_y(Alignment::Center),
    )
    .on_press(ConfigureMessage::DragWindow)
    .interaction(mouse::Interaction::Grab);

    let close = tooltip(
        button(
            svg(svg::Handle::from_memory(svg_icon::CLOSE_SVG.as_bytes()))
                .width(Length::Fixed(ICON_SIZE))
                .height(Length::Fixed(ICON_SIZE))
                .style(|_theme, _status| svg::Style {
                    color: Some(theme::MUTED),
                }),
        )
        .padding(7)
        .on_press(ConfigureMessage::Close)
        .style(theme::ghost),
        text("Close").size(12.0).color(theme::INK),
        tooltip::Position::Bottom,
    )
    .gap(6)
    .padding(6)
    .delay(Duration::from_millis(350))
    .style(theme::tooltip);

    let header = row![heading, close]
        .align_y(Alignment::Center)
        .spacing(6)
        .height(Length::Fixed(HEADER_HEIGHT));

    // Hairline separating the tab-pane header from its content (LINE tier).
    let header_rule: Element<'_, ConfigureMessage> = container(space())
        .width(Fill)
        .height(Length::Fixed(1.0))
        .style(theme::configure_header_rule)
        .into();

    let scroll = |content: Element<'a, ConfigureMessage>| {
        scrollable(
            container(content)
                .padding(iced::Padding {
                    top: 4.0,
                    right: CONTENT_PADDING,
                    bottom: CONTENT_PADDING,
                    left: CONTENT_PADDING,
                })
                .width(Fill),
        )
        .id(content_scroll_id())
        .spacing(SCROLL_GAP)
        .style(theme::scrollbar)
        .height(Fill)
        .width(Fill)
    };

    let content_pane: Element<'_, ConfigureMessage> = match state.section {
        Section::System => column![
            scroll(system_settings_view(settings)),
            container(system_footer())
                .padding(iced::Padding {
                    top: 0.0,
                    right: CONTENT_PADDING,
                    bottom: 12.0,
                    left: CONTENT_PADDING,
                })
                .width(Fill),
        ]
        .width(Fill)
        .height(Fill)
        .into(),
        _ => {
            #[cfg(debug_assertions)]
            {
                scroll(section_content(
                    state, settings, analytics, pad_input, emulators, pending,
                ))
                .into()
            }
            #[cfg(not(debug_assertions))]
            {
                scroll(section_content(state, settings, analytics, pad_input)).into()
            }
        }
    };

    let content = column![
        container(column![header, header_rule].spacing(8))
            .padding(iced::Padding {
                top: 0.0,
                right: CONTENT_PADDING,
                bottom: 10.0,
                left: CONTENT_PADDING,
            })
            .width(Fill),
        // Embed scrollbar with a gutter so it does not hug the window edge.
        container(content_pane)
            .padding(iced::Padding::ZERO.right(SCROLL_EDGE))
            .width(Fill)
            .height(Fill),
    ]
    .width(Fill)
    .height(Fill);

    let sidebar = container(sidebar(state.section))
        .width(Length::Fixed(SIDEBAR_WIDTH))
        .height(Fill)
        .style(theme::glass(sidebar_radius()));

    let window = chrome::window(
        row![sidebar, content]
            .spacing(SIDEBAR_GAP)
            .padding(WINDOW_PAD)
            .width(Fill)
            .height(Fill),
    );
    if !state.show_changelog {
        return window;
    }
    // Modal overlay: the backdrop mouse_area swallows every click outside the
    // card (closing the modal) so nothing reaches the Settings window below.
    // It also reports an `Idle` interaction so the stack levitates the cursor
    // for lower layers: press-capture alone only blocks clicks, while hover /
    // tooltip state is driven by cursor position and would otherwise still
    // reach the buttons underneath.
    // The card sits in an idempotent `OpenChangelog` mouse_area that captures
    // card-background clicks before they can fall through to the backdrop
    // (which would otherwise close the modal from inside the card).
    stack![
        window,
        mouse_area(
            container(space())
                .width(Fill)
                .height(Fill)
                .style(theme::immersive_dim(0.55)),
        )
        .on_press(ConfigureMessage::CloseChangelog)
        .on_right_press(ConfigureMessage::CloseChangelog)
        .interaction(mouse::Interaction::Idle),
        container(
            mouse_area(
                container(theme::framed(changelog::changelog_view()))
                    .width(Length::Fixed(WIDTH - 80.0))
                    .max_height(HEIGHT - 60.0),
            )
            .on_press(ConfigureMessage::OpenChangelog)
            .on_right_press(ConfigureMessage::OpenChangelog),
        )
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill),
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

fn semibold() -> Font {
    Font {
        weight: Weight::Semibold,
        ..Font::DEFAULT
    }
}

/// Sidebar island radius: concentric with the window corner.
fn sidebar_radius() -> f32 {
    (chrome::window_radius() - WINDOW_PAD).max(theme::radius::SM)
}

/// Floating glass rail: app title (drag handle) over pill tabs.
fn sidebar<'a>(active: Section) -> Element<'a, ConfigureMessage> {
    let title = mouse_area(
        container(
            row![
                svg(svg::Handle::from_memory(svg_icon::DUALSENSE_SVG.as_bytes()))
                    .width(Length::Fixed(20.0))
                    .height(Length::Fixed(20.0))
                    .style(|_theme, _status| svg::Style {
                        color: Some(theme::INK),
                    }),
                text("Settings")
                    .size(theme::type_scale::BODY + 1.0)
                    .font(semibold())
                    .color(theme::INK),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .padding([0, 10])
        .width(Fill)
        .height(Length::Fixed(HEADER_HEIGHT - 4.0))
        .align_y(Alignment::Center),
    )
    .on_press(ConfigureMessage::DragWindow)
    .interaction(mouse::Interaction::Grab);

    // Debug-only tabs are grouped under one caption so a debug build never reads
    // as if the tools ship to users.
    let mut labeled = false;
    let tabs =
        Section::all()
            .into_iter()
            .fold(Column::new().spacing(2).width(Fill), |list, section| {
                let list = if section.is_debug_only() && !labeled {
                    #[cfg(debug_assertions)]
                    {
                        list.push(debug_group_label())
                    }
                    #[cfg(not(debug_assertions))]
                    {
                        list
                    }
                } else {
                    list
                };
                labeled |= section.is_debug_only();
                let selected = active == section;
                list.push(
                    button(
                        row![
                            container(space())
                                .width(Length::Fixed(3.0))
                                .height(Length::Fixed(14.0))
                                .style(theme::nav_marker(selected)),
                            text(section.title())
                                .size(theme::type_scale::BODY)
                                .font(if selected { semibold() } else { Font::DEFAULT }),
                        ]
                        .spacing(10)
                        .align_y(Alignment::Center),
                    )
                    .padding([8, 10])
                    .width(Fill)
                    .on_press(ConfigureMessage::SelectSection(section))
                    .style(theme::nav_item(selected)),
                )
            });

    column![title, tabs]
        .spacing(4)
        .padding(6)
        .width(Fill)
        .into()
}

/// Caption separating the shipped tabs from the debug-only tools below it.
#[cfg(debug_assertions)]
fn debug_group_label<'a>() -> Element<'a, ConfigureMessage> {
    container(
        text("Developer mode")
            .size(theme::type_scale::CAPTION)
            .font(semibold())
            .color(theme::DIM),
    )
    .padding(iced::Padding {
        top: 10.0,
        right: 0.0,
        bottom: 4.0,
        left: 10.0,
    })
    .width(Fill)
    .into()
}

// ---------------------------------------------------------------------------
// Settings rows & groups
// ---------------------------------------------------------------------------

/// Glass card holding related rows, with an optional caption above it.
fn group<'a>(
    caption: Option<String>,
    rows: Vec<Element<'a, ConfigureMessage>>,
) -> Element<'a, ConfigureMessage> {
    let card = container(Column::with_children(rows).spacing(14).width(Fill))
        .padding(GROUP_PAD)
        .width(Fill)
        .style(theme::glass(theme::radius::MD));
    match caption {
        Some(caption) => column![
            container(
                text(caption)
                    .size(theme::type_scale::CAPTION + 1.0)
                    .font(semibold())
                    .color(theme::DIM),
            )
            .padding([0, 4]),
            card,
        ]
        .spacing(8)
        .width(Fill)
        .into(),
        None => card.into(),
    }
}

/// Label (+ optional helper line) on the left, pill toggle on the right.
fn toggle_row<'a>(
    label: &'static str,
    helper: Option<&'static str>,
    value: bool,
    on_toggle: impl Fn(bool) -> ConfigureMessage + 'a,
) -> Element<'a, ConfigureMessage> {
    let mut labels = column![text(label).size(theme::type_scale::BODY).color(theme::INK)]
        .spacing(2)
        .width(Fill);
    if let Some(helper) = helper {
        labels = labels.push(text(helper).size(12.0).color(theme::DIM));
    }
    row![
        labels,
        toggler(value)
            .size(20.0)
            .on_toggle(on_toggle)
            .style(theme::toggle),
    ]
    .spacing(16)
    .align_y(Alignment::Center)
    .width(Fill)
    .into()
}

/// Label with its live value on the right, slider underneath.
fn slider_row<'a>(
    label: &'static str,
    value_label: String,
    slider: impl Into<Element<'a, ConfigureMessage>>,
) -> Element<'a, ConfigureMessage> {
    column![
        row![
            text(label)
                .size(theme::type_scale::BODY)
                .color(theme::INK)
                .width(Fill),
            text(value_label)
                .size(theme::type_scale::META)
                .color(theme::MUTED),
        ]
        .align_y(Alignment::Center),
        slider.into(),
    ]
    .spacing(8)
    .width(Fill)
    .into()
}

/// Secondary action button (ink wash, rounded).
fn action_button<'a>(
    label: &'static str,
    message: ConfigureMessage,
) -> Element<'a, ConfigureMessage> {
    button(text(label).size(13.0))
        .padding([7, 14])
        .on_press(message)
        .style(theme::secondary)
        .into()
}

/// Dim explanatory paragraph.
fn note<'a>(body: impl text::IntoFragment<'a>) -> Element<'a, ConfigureMessage> {
    text(body).size(12.0).color(theme::DIM).into()
}

fn section_content<'a>(
    state: &'a ConfigureState,
    settings: &ConfigureSettings,
    analytics: &'a AnalyticsPanel,
    pad_input: &'a PadInputPanel,
    #[cfg(debug_assertions)] emulators: &'a [EmulatedPad],
    #[cfg(debug_assertions)] pending: &[PendingEmulatorChange],
) -> Element<'a, ConfigureMessage> {
    match state.section {
        Section::System => system_settings_view(settings),
        Section::StartScreen => start_screen_view(settings),
        Section::Notifications => notifications_view(settings, theme::ACCENT),
        Section::Lightbar => lightbar_view(state, settings),
        Section::Analytics => analytics_view(settings, analytics),
        Section::PadInput => pad_input_view(pad_input),
        #[cfg(debug_assertions)]
        Section::Emulators => emulators_view(state, emulators, pending),
        #[cfg(debug_assertions)]
        Section::Diagnostics => diagnostics_view(),
    }
}

#[cfg(debug_assertions)]
fn diagnostics_view<'a>() -> Element<'a, ConfigureMessage> {
    group(
        Some("Renderer stress test".to_string()),
        vec![
            note(
                "Runs rapid toast + cold Start + configure + edit churn (the wgpu atlas-crash shape) for about a minute to shake out renderer crashes. Takes over window open/close while running — don't drive the UI at the same time, then check app.log for PANIC lines.",
            ),
            button(text("Run window stress test").size(13.0))
                .padding([7, 14])
                .on_press(ConfigureMessage::RunWindowStress)
                .style(theme::primary)
                .into(),
        ],
    )
}

fn system_settings_view<'a>(settings: &ConfigureSettings) -> Element<'a, ConfigureMessage> {
    #[cfg(windows)]
    let startup: Vec<Element<'a, ConfigureMessage>> = vec![toggle_row(
        "Start with Windows",
        Some("Run quietly in the tray when you sign in"),
        settings.autostart,
        ConfigureMessage::SetAutostart,
    )];
    #[cfg(not(windows))]
    let startup: Vec<Element<'a, ConfigureMessage>> = {
        let _ = settings;
        Vec::new()
    };

    let data = group(
        Some("Data".to_string()),
        vec![
            note("Preferences, remembered controllers, analytics, and logs stay on this PC."),
            action_button("Open data folder", ConfigureMessage::OpenDataFolder),
            action_button("View changelog", ConfigureMessage::OpenChangelog),
        ],
    );

    if startup.is_empty() {
        data
    } else {
        column![group(Some("Startup".to_string()), startup), data]
            .spacing(18)
            .width(Fill)
            .into()
    }
}

fn system_footer<'a>() -> Element<'a, ConfigureMessage> {
    let links = row![
        footer_link_button(
            svg_icon::GITHUB_SVG,
            // Match Microsoft Store bag main fill (`#F2F2F2`).
            Some(theme::rgb(0xF2, 0xF2, 0xF2)),
            "GitHub",
            GITHUB_URL,
        ),
        footer_link_button(
            svg_icon::MICROSOFT_STORE_SVG,
            None,
            "Microsoft Store",
            MICROSOFT_STORE_URL,
        ),
    ]
    .spacing(4)
    .align_y(Alignment::Center);

    let version = text(format!("{DISPLAY_NAME} {PKG_VERSION}"))
        .size(12.0)
        .color(theme::DIM);

    Row::new()
        .spacing(0)
        .width(Fill)
        .align_y(Alignment::Center)
        .push(links)
        .push(space().width(Fill))
        .push(version)
        .into()
}

fn footer_link_button<'a>(
    source: &'static str,
    tint: Option<Color>,
    tip: &'static str,
    url: &'static str,
) -> Element<'a, ConfigureMessage> {
    let icon = |opacity: f32| {
        svg(svg::Handle::from_memory(source.as_bytes()))
            .width(Length::Fixed(FOOTER_ICON_SIZE))
            .height(Length::Fixed(FOOTER_ICON_SIZE))
            .opacity(opacity)
            .style(move |_theme, _status| svg::Style { color: tint })
    };
    // Dim when idle; full-opacity twin paints on top while hovered.
    let icon = hover(icon(0.8), icon(1.0));

    tooltip(
        button(icon)
            .padding(4)
            .on_press(ConfigureMessage::OpenExternalLink(url))
            .style(theme::ghost),
        text(tip).size(12.0).color(theme::INK),
        tooltip::Position::Top,
    )
    .gap(6)
    .padding(6)
    .delay(Duration::from_millis(350))
    .style(theme::tooltip)
    .into()
}

impl std::fmt::Display for ImmersiveLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

fn start_screen_view<'a>(settings: &ConfigureSettings) -> Element<'a, ConfigureMessage> {
    let mut groups = Column::new().spacing(18).width(Fill);

    let mut opening = vec![toggle_row(
        "Enable start screen",
        Some("A controller-friendly launcher for your games"),
        settings.start_screen_enabled,
        ConfigureMessage::SetStartScreenEnabled,
    )];
    if settings.start_screen_enabled {
        opening.push(
            row![
                column![
                    text("Auto open")
                        .size(theme::type_scale::BODY)
                        .color(theme::INK),
                    text("When a controller connects")
                        .size(12.0)
                        .color(theme::DIM),
                ]
                .spacing(2)
                .width(Fill),
                pick_list(
                    AUTO_OPEN_MODES,
                    Some(settings.start_screen_auto_open),
                    ConfigureMessage::SetStartScreenAutoOpen,
                )
                .text_size(13.0)
                .padding([7, 10])
                .width(Length::Fixed(280.0))
                .style(theme::pick_list)
                .menu_style(theme::pick_list_menu),
            ]
            .spacing(16)
            .align_y(Alignment::Center)
            .into(),
        );
    }
    groups = groups.push(group(Some("Opening".to_string()), opening));

    if settings.start_screen_enabled {
        let mut immersive = vec![
            toggle_row(
                "Always immersive",
                Some("Open full screen; Circle closes instead of going back"),
                settings.start_screen_always_immersive,
                ConfigureMessage::SetStartScreenAlwaysImmersive,
            ),
            toggle_row(
                "Display clock",
                None,
                settings.start_screen_clock_enabled,
                ConfigureMessage::SetStartScreenClock,
            ),
            row![
                column![
                    text("Layout")
                        .size(theme::type_scale::BODY)
                        .color(theme::INK),
                    text("Orientation of the games strip")
                        .size(12.0)
                        .color(theme::DIM),
                ]
                .spacing(2)
                .width(Fill),
                pick_list(
                    IMMERSIVE_LAYOUT_MODES,
                    Some(settings.start_screen_immersive_layout),
                    ConfigureMessage::SetImmersiveLayout,
                )
                .text_size(13.0)
                .padding([7, 10])
                .width(Length::Fixed(200.0))
                .style(theme::pick_list)
                .menu_style(theme::pick_list_menu),
            ]
            .spacing(16)
            .align_y(Alignment::Center)
            .into(),
        ];

        let idle_secs = settings.start_screen_inactive_secs;
        let idle_steps = crate::persist::prefs::START_SCREEN_IDLE_SECS_STEPS;
        let idle_idx = crate::persist::prefs::secs_step_index(idle_secs, idle_steps);
        immersive.push(slider_row(
            "Idle after",
            crate::persist::prefs::format_timeout_duration(idle_secs),
            slider(
                0.0..=(idle_steps.len().saturating_sub(1) as f32),
                idle_idx as f32,
                |value| {
                    let idx = (value.round() as usize).min(idle_steps.len().saturating_sub(1));
                    ConfigureMessage::SetStartScreenInactiveSecs(idle_steps[idx])
                },
            )
            .step(1.0_f32)
            .style(theme::slider),
        ));

        let sleep_secs = settings.start_screen_sleep_secs;
        let sleep_steps = crate::persist::prefs::START_SCREEN_SLEEP_SECS_STEPS;
        let min_sleep = crate::persist::prefs::min_sleep_secs_for_idle(idle_secs);
        let sleep_choices: Vec<u32> = sleep_steps
            .iter()
            .copied()
            .filter(|&s| s >= min_sleep)
            .collect();
        let sleep_choices = if sleep_choices.is_empty() {
            vec![*sleep_steps.last().unwrap_or(&min_sleep)]
        } else {
            sleep_choices
        };
        let sleep_idx = crate::persist::prefs::secs_step_index(sleep_secs, &sleep_choices);
        immersive.push(slider_row(
            "Sleep after",
            crate::persist::prefs::format_timeout_duration(sleep_secs),
            slider(
                0.0..=(sleep_choices.len().saturating_sub(1) as f32),
                sleep_idx as f32,
                move |value| {
                    let idx = (value.round() as usize).min(sleep_choices.len().saturating_sub(1));
                    ConfigureMessage::SetStartScreenSleepSecs(sleep_choices[idx])
                },
            )
            .step(1.0_f32)
            .style(theme::slider),
        ));

        let dim = settings.start_screen_inactive_dim_percent;
        immersive.push(slider_row(
            "Idle dim",
            format!("{dim}%"),
            slider(0.0..=100.0, f32::from(dim), |value| {
                ConfigureMessage::SetStartScreenInactiveDimPercent(value.round() as u8)
            })
            .step(5.0_f32)
            .style(theme::slider),
        ));
        groups = groups.push(group(Some("Immersive".to_string()), immersive));

        let mut feedback = vec![toggle_row(
            "UI sounds",
            None,
            settings.start_screen_sounds_enabled,
            ConfigureMessage::SetStartScreenSounds,
        )];
        if settings.start_screen_sounds_enabled {
            let volume = settings.start_screen_sound_volume;
            feedback.push(slider_row(
                "Volume",
                format!("{volume}%"),
                slider(0.0..=100.0, f32::from(volume), |value| {
                    ConfigureMessage::SetStartScreenSoundVolume(value.round() as u8)
                })
                .step(5.0_f32)
                .style(theme::slider),
            ));
        }
        feedback.push(toggle_row(
            "Controller haptics",
            None,
            settings.start_screen_haptics_enabled,
            ConfigureMessage::SetStartScreenHaptics,
        ));
        if settings.start_screen_haptics_enabled {
            let strength = settings.start_screen_haptics_strength;
            feedback.push(slider_row(
                "Strength",
                format!("{strength}%"),
                slider(0.0..=100.0, f32::from(strength), |value| {
                    ConfigureMessage::SetStartScreenHapticsStrength(value.round() as u8)
                })
                .step(5.0_f32)
                .style(theme::slider),
            ));
        }
        groups = groups.push(group(Some("Feedback".to_string()), feedback));
    }

    groups = groups.push(
        container(note(format!(
            "Last-known compatible Steam version: {LAST_KNOWN_COMPATIBLE_STEAM_VERSION}"
        )))
        .padding([0, 4]),
    );

    let mut library_rows: Vec<Element<'_, ConfigureMessage>> = settings
        .extra_steam_paths
        .iter()
        .enumerate()
        .map(|(i, path)| {
            row![
                text(path.display().to_string())
                    .size(13.0)
                    .width(Fill)
                    .color(theme::INK),
                button(text("Remove").size(12.0))
                    .padding([4, 10])
                    .on_press(ConfigureMessage::RemoveSteamPath(i))
                    .style(theme::ghost),
            ]
            .align_y(Alignment::Center)
            .spacing(8)
            .into()
        })
        .collect();
    if let Some(err) = &settings.steam_path_error {
        library_rows.push(note(err.clone()));
    }
    library_rows.push(action_button("Add folder…", ConfigureMessage::AddSteamPath));
    groups = groups.push(group(
        Some("Extra Steam libraries".to_string()),
        library_rows,
    ));

    groups.into()
}

fn any_notification_enabled(settings: &ConfigureSettings) -> bool {
    settings.notify_connect
        || settings.notify_disconnect
        || settings.notify_low
        || settings.notify_charged
}

fn notifications_view<'a>(
    settings: &ConfigureSettings,
    accent: Color,
) -> Element<'a, ConfigureMessage> {
    let toggle =
        |label: &'static str, helper: &'static str, value: bool, setting: NotificationSetting| {
            toggle_row(label, Some(helper), value, move |enabled| {
                ConfigureMessage::SetNotification(setting, enabled)
            })
        };

    let connection = group(
        Some("Connection".to_string()),
        vec![
            toggle(
                "Connected",
                "When a controller connects",
                settings.notify_connect,
                NotificationSetting::Connect,
            ),
            toggle(
                "Disconnected",
                "When a controller disconnects or turns off",
                settings.notify_disconnect,
                NotificationSetting::Disconnect,
            ),
        ],
    );

    let mut battery_rows = vec![toggle(
        "Low battery",
        "When a controller drops to your threshold",
        settings.notify_low,
        NotificationSetting::Low,
    )];
    if settings.notify_low {
        let threshold = settings.low_battery_percent;
        battery_rows.push(slider_row(
            "Threshold",
            format!("{threshold}% or below"),
            slider(
                f32::from(LOW_BATTERY_PERCENT_MIN)..=f32::from(LOW_BATTERY_PERCENT_MAX),
                f32::from(threshold),
                |value| ConfigureMessage::SetLowBatteryPercent(value.round() as u8),
            )
            .step(10.0_f32)
            .style(theme::slider),
        ));
    }
    battery_rows.push(toggle(
        "Finished charging",
        "When a charging controller reaches full",
        settings.notify_charged,
        NotificationSetting::Charged,
    ));

    let mut items = column![connection, group(Some("Battery".to_string()), battery_rows)]
        .spacing(18)
        .width(Fill);
    if any_notification_enabled(settings) {
        items = items.push(group(
            Some("Toast position".to_string()),
            vec![toast_position_view(settings, accent)],
        ));
    }
    items.into()
}

fn analytics_view<'a>(
    settings: &ConfigureSettings,
    panel: &'a AnalyticsPanel,
) -> Element<'a, ConfigureMessage> {
    let mut items = Column::new().spacing(18).width(Fill);

    items = items.push(group(
        None,
        vec![
            toggle_row(
                "Record battery analytics",
                Some("Learns how long your controllers charge and last"),
                settings.analytics_enabled,
                ConfigureMessage::SetAnalyticsEnabled,
            ),
            note("Local only. Full charge / full drain are estimated totals for a complete cycle. Remaining time on the tray uses your current (or last-known) percent. Mid-cycle unplug or charge does not wipe learned steps."),
        ],
    ));

    if settings.analytics_enabled {
        if panel.rows.is_empty() {
            items = items.push(
                container(
                    text("Learning… play or charge through a battery step to seed estimates.")
                        .size(13.0)
                        .color(theme::MUTED),
                )
                .padding([0, 4]),
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
        .push(
            text(&row.label)
                .size(15.0)
                .font(semibold())
                .color(theme::INK),
        )
        .push(
            text(format!("{charge} · {play}"))
                .size(theme::type_scale::META)
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
        .push(space().height(Length::Fixed(4.0)))
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
        .padding(GROUP_PAD)
        .width(Fill)
        .style(theme::glass(theme::radius::MD))
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
                    .width(Length::Fixed(3.0))
                    .height(Fill)
                    .style(theme::position_rail(selected))
            ]
            .padding([4, 4])
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

    let stage: Element<'a, ConfigureMessage> = container(
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
    .into();

    column![
        stage,
        container(note(
            "Click a spot to move toasts there. A preview toast shows the new position."
        ))
        .padding([0, 4]),
    ]
    .spacing(10)
    .width(Fill)
    .into()
}

fn lightbar_view<'a>(
    state: &'a ConfigureState,
    settings: &ConfigureSettings,
) -> Element<'a, ConfigureMessage> {
    let mut content = Column::new().spacing(18).width(Fill);

    let mut master = vec![toggle_row(
        "Battery lightbar",
        Some("Color the lightbar by charge level"),
        settings.lightbar_enabled,
        ConfigureMessage::SetLightbarEnabled,
    )];
    if !settings.lightbar_enabled {
        master.push(note(
            "Battery-driven lightbar colors and the low-battery pulse are paused. Identify still works.",
        ));
        content = content.push(group(None, master));
        return content.into();
    }
    content = content.push(group(None, master));

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
                    .padding([6, 10])
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

    let mut gradient: Vec<Element<'a, ConfigureMessage>> = vec![
        bar.into(),
        note(
            "Full charge on the right, empty on the left. Click the bar to add a stop; drag a stop off the bar or right-click it to remove.",
        ),
        stops.into(),
    ];
    if let Some(error) = state.error.as_deref() {
        gradient.push(text(error).size(12.0).color(theme::WARNING).into());
    }
    content = content.push(group(Some("Gradient".to_string()), gradient));

    content = content.push(group(
        Some("Selected stop color".to_string()),
        vec![
            sv.into(),
            hue.into(),
            action_button("Reset to defaults", ConfigureMessage::ResetSpectrum),
        ],
    ));

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
            bit("dpad_left", panel.dpad_left),
            bit("dpad_right", panel.dpad_right),
            text(format!(
                "stick={}  stick_x={:.2}  stick_y={:.2}",
                panel.stick_band, panel.stick_x, panel.stick_y
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

/// Controllers tab (debug only): the emulated fleet, one compact row per pad.
///
/// Each row carries its own disclosure chevron; expanding a row reveals that
/// pad's controls inline, so several pads can be adjusted at once. Rows
/// address pads by serial, and `pending` carries the debounce state for
/// whichever pads are mid-publish.
#[cfg(debug_assertions)]
fn emulators_view<'a>(
    state: &'a ConfigureState,
    fleet: &'a [EmulatedPad],
    pending: &[PendingEmulatorChange],
) -> Element<'a, ConfigureMessage> {
    let mut sections = Column::new().spacing(14).width(Fill);

    if fleet.is_empty() {
        sections = sections.push(
            container(
                text("No emulated controllers. Add one to drive the app without hardware.")
                    .size(13.0)
                    .color(theme::DIM),
            )
            .padding(16)
            .width(Fill)
            .style(theme::glass(theme::radius::MD)),
        );
    } else {
        for pad in fleet {
            let expanded = state.expanded_emulators.contains(pad.serial());
            let pad_pending = pending.iter().find(|p| p.serial == pad.serial());
            sections = sections.push(emulator_row(pad, expanded, pad_pending));
        }
    }

    sections
        .push(
            row![space().width(Fill), add_pad_button()]
                .width(Fill)
                .align_y(Alignment::Center),
        )
        .into()
}

/// Indeterminate arc for a value that is previewed but not yet published.
#[cfg(debug_assertions)]
fn pending_spinner<'a>(tick: u32) -> Element<'a, ConfigureMessage> {
    // The commit tick is the only clock running here, so the angle steps off
    // the frame counter instead of wall time.
    let angle = (tick % 12) as f32 * 30.0;
    svg(svg::Handle::from_memory(svg_icon::SPINNER_SVG.as_bytes()))
        .width(Length::Fixed(14.0))
        .height(Length::Fixed(14.0))
        .rotation(angle)
        .style(|_theme, _status| svg::Style {
            color: Some(theme::MUTED),
        })
        .into()
}

/// Disclosure chevron; rotated a quarter turn when the row is open.
#[cfg(debug_assertions)]
fn chevron<'a>(open: bool) -> Element<'a, ConfigureMessage> {
    svg(svg::Handle::from_memory(svg_icon::CHEVRON_SVG.as_bytes()))
        .width(Length::Fixed(14.0))
        .height(Length::Fixed(14.0))
        .rotation(if open { 90.0 } else { 0.0 })
        .style(|_theme, _status| svg::Style {
            color: Some(theme::DIM),
        })
        .into()
}

#[cfg(debug_assertions)]
fn add_pad_button<'a>() -> Element<'a, ConfigureMessage> {
    button(text("+ Add controller").size(13.0))
        .padding([7, 14])
        .on_press(ConfigureMessage::Emulator(EmulatorCommand::AddPad))
        .style(theme::primary)
        .into()
}

/// One pad: header row (disclosure, identity, level, connect switch) plus,
/// when open, that pad's controls inline.
#[cfg(debug_assertions)]
fn emulator_row<'a>(
    pad: &EmulatedPad,
    expanded: bool,
    pending: Option<&PendingEmulatorChange>,
) -> Element<'a, ConfigureMessage> {
    let serial = pad.serial().to_string();
    let toggle = toggler(pad.connected)
        .size(18.0)
        .on_toggle({
            let serial = serial.clone();
            move |connected| {
                ConfigureMessage::Emulator(EmulatorCommand::SetConnected(serial.clone(), connected))
            }
        })
        .style(theme::toggle);

    let meta = if pad.connected {
        format!(
            "{} · {}",
            pad.status.connection.as_str(),
            pad.status.state.as_str()
        )
    } else if pad.remembered {
        "off".to_string()
    } else {
        "off · forgotten".to_string()
    };

    let chevron_serial = serial.clone();
    // The switch and the chevron sit *beside* the header button rather than
    // inside it: nested pressables fight over the click and the outer wins.
    let header = row![
        button(
            row![
                chevron(expanded),
                text(pad.serial().to_string())
                    .size(theme::type_scale::BODY)
                    .color(if pad.connected {
                        theme::INK
                    } else {
                        theme::DIM
                    })
                    .width(Length::Fixed(64.0)),
                text(meta)
                    .size(theme::type_scale::META)
                    .color(theme::DIM)
                    .width(Fill),
                text(format!("{}%", pad.status.percent))
                    .size(theme::type_scale::BODY)
                    .color(theme::MUTED),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .padding([9, 10])
        .width(Fill)
        .on_press(ConfigureMessage::ToggleEmulator(chevron_serial))
        .style(theme::nav_item(expanded)),
        toggle,
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .width(Fill);

    if !expanded {
        return container(header)
            .width(Fill)
            .style(theme::glass(theme::radius::SM))
            .into();
    }

    let remember_serial = pad.serial().to_string();
    let controls = column![
        battery_row(pad, pending),
        connection_row(pad),
        analytics_block(pad),
        row![
            // Same pin the popup offers real controllers: forget keeps the pad
            // off the remembered list, and once it is also unplugged the row
            // goes away.
            button(
                text(if pad.remembered {
                    "Forget this controller"
                } else {
                    "Remember this controller"
                })
                .size(12.0)
            )
            .padding([6, 10])
            .on_press(ConfigureMessage::Emulator(EmulatorCommand::SetRemembered(
                remember_serial,
                !pad.remembered,
            )))
            .style(if pad.remembered {
                theme::secondary
            } else {
                theme::danger
            }),
            button(text("Remove controller").size(12.0))
                .padding([6, 10])
                .on_press(ConfigureMessage::Emulator(EmulatorCommand::RemovePad(
                    serial,
                )))
                .style(theme::danger),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    ]
    .spacing(12)
    .padding(iced::Padding {
        top: 4.0,
        right: 12.0,
        bottom: 12.0,
        left: 12.0,
    })
    .width(Fill);

    container(column![header, controls].spacing(0).width(Fill))
        .width(Fill)
        .style(theme::glass(theme::radius::SM))
        .into()
}

/// Battery slider, snapped to the levels a DualSense reports.
///
/// A connected pad debounces publication by a second (so a drag cannot spam
/// preference-driven toasts or Start opens) and spins while it waits; a
/// disconnected pad has nothing to publish to, so it applies immediately.
#[cfg(debug_assertions)]
fn battery_row<'a>(
    pad: &EmulatedPad,
    pending: Option<&PendingEmulatorChange>,
) -> Element<'a, ConfigureMessage> {
    let level = crate::controller::dualsense::battery::level_from_percent(pad.status.percent);
    let preview_serial = pad.serial().to_string();
    let slider = slider(0.0..=MAX_EMU_LEVEL as f32, level as f32, move |value| {
        ConfigureMessage::Emulator(EmulatorCommand::PreviewPercent(
            preview_serial.clone(),
            crate::controller::dualsense::battery::percent_from_level(
                value.round() as u8,
                PowerState::Discharging,
            ),
        ))
    })
    .step(1.0_f32)
    .style(theme::slider);

    let label = match pending {
        Some(p) => format!("{}% · applying…", p.percent),
        None => format!("{}%", pad.status.percent),
    };

    let mut rows = vec![slider_row("Battery", label, slider)];
    if let Some(p) = pending {
        rows.push(
            row![
                pending_spinner(p.tick),
                text("Waiting a moment before applying, so a drag can't repeat the notification.")
                    .size(11.0)
                    .color(theme::DIM)
                    .width(Fill),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
        );
    }
    Column::with_children(rows).spacing(12).width(Fill).into()
}

/// The pad's link, and the charge state that follows from it.
///
/// Only the link is a control: USB charges (reporting `Complete` once topped
/// off), Bluetooth runs down. A separate charge-state picker would let the
/// emulator describe states real hardware cannot be in.
#[cfg(debug_assertions)]
fn connection_row<'a>(pad: &EmulatedPad) -> Element<'a, ConfigureMessage> {
    let mut options = Vec::new();
    for option in [Connection::Usb, Connection::Bluetooth] {
        let serial = pad.serial().to_string();
        let derived = emulate::state_for(option, pad.status.percent);
        options.push(
            column![
                button(text(option.as_str()).size(12.0))
                    .padding([6, 10])
                    .on_press(ConfigureMessage::Emulator(EmulatorCommand::SetConnection(
                        serial, option,
                    )))
                    .style(theme::chip(option == pad.status.connection)),
                text(power_state_label(derived))
                    .size(10.0)
                    .color(theme::DIM),
            ]
            .spacing(4)
            .align_x(Alignment::Center)
            .into(),
        );
    }
    segmented_row("Link", options)
}

/// Battery-analytics actions for one pad.
#[cfg(debug_assertions)]
fn analytics_block<'a>(pad: &EmulatedPad) -> Element<'a, ConfigureMessage> {
    let seed_serial = pad.serial().to_string();
    let charge_serial = pad.serial().to_string();
    let drain_serial = pad.serial().to_string();
    column![
        text("Battery analytics")
            .size(theme::type_scale::CAPTION + 1.0)
            .font(semibold())
            .color(theme::DIM),
        text("Seed fills typical charge and play samples; each step credits active time and walks the cycle.")
            .size(11.0)
            .color(theme::DIM),
        row![
            button(text("Seed estimates").size(12.0))
                .padding([6, 10])
                .on_press(ConfigureMessage::Emulator(EmulatorCommand::SeedAnalytics(
                    seed_serial,
                )))
                .style(theme::secondary),
            button(text("Charge +15m").size(12.0))
                .padding([6, 10])
                .on_press(ConfigureMessage::Emulator(EmulatorCommand::StepCharge(
                    charge_serial,
                )))
                .style(theme::secondary),
            button(text("Drain +25m").size(12.0))
                .padding([6, 10])
                .on_press(ConfigureMessage::Emulator(EmulatorCommand::StepDrain(
                    drain_serial,
                )))
                .style(theme::secondary),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    ]
    .spacing(6)
    .width(Fill)
    .into()
}

/// Highest battery level the DualSense reports (10 → 100%).
#[cfg(debug_assertions)]
const MAX_EMU_LEVEL: u8 = 10;

#[cfg(debug_assertions)]
fn power_state_label(state: PowerState) -> &'static str {
    match state {
        PowerState::Discharging => "Discharging",
        PowerState::Charging => "Charging",
        PowerState::Complete => "Full",
        _ => "Other",
    }
}

/// Label above a row of exclusive choice chips.
#[cfg(debug_assertions)]
fn segmented_row<'a>(
    label: &'static str,
    options: Vec<Element<'a, ConfigureMessage>>,
) -> Element<'a, ConfigureMessage> {
    column![
        text(label)
            .size(theme::type_scale::BODY)
            .color(theme::INK)
            .width(Fill),
        Row::with_children(options)
            .spacing(8)
            .align_y(Alignment::Center)
            .width(Fill),
    ]
    .spacing(8)
    .width(Fill)
    .into()
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_groups_debug_sections_last_in_debug_builds_only() {
        // No flag, no pref: the sidebar follows the build profile, so a plain
        // `cargo run` already exposes the emulator.
        let sections = Section::all();
        assert!(sections.contains(&Section::PadInput));
        #[cfg(debug_assertions)]
        {
            assert!(sections.contains(&Section::Emulators));
            assert!(sections.contains(&Section::Diagnostics));
            // Debug tools sit after everything a release build ships, and the
            // group is contiguous so the "Developer mode" caption labels once.
            let shipped = [
                Section::System,
                Section::StartScreen,
                Section::Notifications,
                Section::Lightbar,
                Section::Analytics,
            ];
            let first_debug = sections
                .iter()
                .position(|s| s.is_debug_only())
                .expect("debug tabs present");
            assert_eq!(&sections[..first_debug], &shipped[..]);
            assert!(
                sections[first_debug..].iter().all(|s| s.is_debug_only()),
                "debug group must be contiguous: {sections:?}"
            );
            assert_eq!(first_debug, shipped.len());
        }
        #[cfg(not(debug_assertions))]
        {
            // Release compiles the variants out; the list must not name them.
            assert!(sections.len() == 5, "sections={sections:?}");
        }
    }

    #[test]
    #[cfg(debug_assertions)]
    fn emulators_view_builds_for_empty_and_populated_fleets() {
        // Building the element tree is the only offline check that the new
        // Controllers tab composes without panicking.
        let mut state = ConfigureState::new(BatterySpectrum::default_spectrum());
        let empty: [EmulatedPad; 0] = [];
        // Build the fixture through the emulator's own constructors so the
        // pads obey the link/state invariant, and cover every link state the
        // picker can label: charging, complete, discharging.
        let plugged = emulate::new_pad(&[], 50, Connection::Usb);
        let mut fleet = emulate::set_percent(std::slice::from_ref(&plugged), "emu-1", 100);
        fleet.push(emulate::new_pad(&fleet, 20, Connection::Bluetooth));
        fleet = emulate::update(&fleet, "emu-1", |p| p.connected = true);
        let pending = [PendingEmulatorChange {
            serial: "emu-1".into(),
            percent: 75,
            tick: 3,
        }];

        // Empty fleet shows the hint; rows build collapsed, expanded, and
        // mid-debounce alike.
        drop(emulators_view(&state, &empty, &[]));
        drop(emulators_view(&state, &fleet, &[]));
        drop(emulators_view(&state, &fleet, &pending));
        state.expanded_emulators.insert("emu-1".into());
        drop(emulators_view(&state, &fleet, &[]));
        // An unknown expansion is simply not drawn.
        state.expanded_emulators.insert("emu-9".into());
        drop(emulators_view(&state, &fleet, &pending));
        // The forget/remember button label follows the pad's remembered flag.
        let forgotten = emulate::update(&fleet, "emu-1", |p| p.remembered = false);
        drop(emulators_view(&state, &forgotten, &[]));
    }

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

    #[test]
    fn start_screen_view_builds_when_disabled_and_enabled() {
        let mut settings = ConfigureSettings {
            notify_low: true,
            notify_charged: true,
            notify_connect: true,
            notify_disconnect: true,
            low_battery_percent: 20,
            toast_position: ToastPosition::BottomRight,
            analytics_enabled: false,
            lightbar_enabled: true,
            start_screen_enabled: false,
            start_screen_auto_open: StartAutoOpen::Any,
            start_screen_always_immersive: false,
            start_screen_immersive_layout: ImmersiveLayout::Vertical,
            start_screen_clock_enabled: true,
            start_screen_inactive_secs: 60,
            start_screen_sleep_secs: 300,
            start_screen_inactive_dim_percent: 50,
            start_screen_sounds_enabled: true,
            start_screen_sound_volume: 80,
            start_screen_haptics_enabled: true,
            start_screen_haptics_strength: 80,
            #[cfg(windows)]
            autostart: false,
            extra_steam_paths: Vec::new(),
            steam_path_error: None,
        };
        let _disabled_el = start_screen_view(&settings);
        settings.start_screen_enabled = true;
        let _enabled_el = start_screen_view(&settings);
    }
}
