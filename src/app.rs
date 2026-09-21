//! iced `daemon` shell for the DualSense battery tray app.
//!
//! The daemon boots windowless: it owns the tray icon and only opens windows on
//! demand (controller popup, configure window, overlay toast). The toast window
//! is pre-created hidden after the tray appears so iced keeps a warm GPU
//! compositor; popup and Settings stay ephemeral (create/destroy on each open).

use crate::analytics::{self, AnalyticsStore};
use crate::app_log;
#[cfg(windows)]
use crate::autostart;
use crate::battery::{self, ControllerStatus};
use crate::color::{self, BatterySpectrum, color_for_battery_percent};
use crate::configure_view::{
    self, AnalyticsPadRow, AnalyticsPanel, ConfigureMessage, ConfigureSettings, ConfigureState,
    NotificationSetting, PadInputPanel, Section, StartScreenPanel,
};
use crate::dualsense;
#[cfg(feature = "dev-emulate")]
use crate::emulate::{self, Preset};
use crate::games::{self, GamesCatalog};
use crate::gesture::{self, GestureDetector, GestureRecorder};
use crate::known::KnownControllers;
use crate::launch;
use crate::lightbar::{
    self, LOW_BATTERY_ORANGE, LOW_BATTERY_PULSE_GAP_MS, LOW_BATTERY_PULSE_ON_MS,
};
use crate::notify::{NotifyEvent, NotifyTracker};
use crate::paths;
use crate::poll::{
    self, BATTERY_INTERVAL, LIVENESS_INTERVAL, PRESENCE_INTERVAL, UNREAD_RETRY_INTERVAL,
};
use crate::popup_view::{self, ControllerRow, PopupMessage};
use crate::prefs::{Prefs, clamp_low_battery_percent};
use crate::start_input::{self, ButtonEdges, NavAction, NavLogSnapshot, NavSource, NavStepper};
use crate::start_view::{self, StartMessage};
use crate::steam::{self, SteamGame};
use crate::theme;
use crate::toast::ToastMessage;
use crate::toast_view;
use crate::tray::{self, QUIT_ID, SETTINGS_ID};
#[cfg(windows)]
use crate::win32;
use crate::window_layout::{
    TOAST_SLIDE_DURATION, ToastPlacement, TrayAnchor, hide_toast, overlay_platform_specific,
    popup_position, show_toast_without_activate, slide_y, toast_placement,
    window_platform_specific,
};

use iced::futures::Stream;
use iced::futures::StreamExt;
use iced::futures::channel::{mpsc, oneshot};
use iced::keyboard;
use iced::widget::{container, operation, space};
use iced::{Element, Point, Size, Subscription, Task, Theme, stream, window};

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tray_icon::TrayIcon;

/// Debounce spectrum prefs save + HID apply after the last color edit.
const SPECTRUM_DEBOUNCE: Duration = Duration::from_millis(150);
/// How long an overlay toast stays on screen.
const TOAST_LIFETIME: Duration = Duration::from_secs(5);
/// Ignore 0→1 auto-open briefly after the last pad vanished (BT ghost flaps).
const START_CONNECT_COOLDOWN: Duration = Duration::from_secs(5);
/// DualSense poll rate while start-screen input / gesture listening is active.
const PAD_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub enum Message {
    /// Periodic presence scan.
    Tick,
    /// Result of a background `poll_controllers` call.
    PollResult(Result<Vec<ControllerStatus>, String>),

    /// A tray menu item was activated.
    TrayMenu(String),
    /// The tray icon was left-clicked at the given screen rectangle.
    TrayLeftClick(TrayAnchor),

    WindowClosed(window::Id),
    WindowUnfocused(window::Id),

    PopupOpened(window::Id),
    PlacePopup {
        id: window::Id,
        scale: f32,
        monitor: Option<Size>,
    },
    Popup(PopupMessage),

    ConfigureOpened(window::Id),
    Configure(ConfigureMessage),

    StartOpened(window::Id),
    Start(StartMessage),
    /// Keyboard while some iced window has focus; filtered to the start screen in update.
    StartKey {
        id: window::Id,
        action: StartKeyAction,
    },
    /// Periodic DualSense sample for gestures / start-screen navigation.
    PadPoll,
    ManualFilePicked(Option<PathBuf>),
    SteamScanDone(Result<Vec<SteamGame>, String>),

    PlaceToast {
        id: window::Id,
        monitor: Option<Size>,
    },
    /// Advance the active toast slide animation.
    ToastFrame,
    /// Redraw the controller popup while an Identify ring flash is running.
    IdentifyFrame,
    /// Begin dismiss (slide-out) for the toast of the given generation.
    ToastDismiss(u64),

    /// Debounced spectrum prefs save + lightbar HID apply.
    SpectrumCommit(u64),
    /// Background spectrum HID apply finished (no-op handler).
    SpectrumHidDone,

    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartKeyAction {
    Up,
    Down,
    Confirm,
    Cancel,
}

pub struct App {
    prefs: Prefs,
    known: KnownControllers,
    notify: NotifyTracker,
    analytics: AnalyticsStore,
    games: GamesCatalog,

    controllers: Vec<ControllerStatus>,
    /// Last HID presence snapshot (serials), used to detect connect/disconnect.
    last_discovered: Vec<String>,
    last_battery_poll: Instant,

    /// True while an identify flash sequence is running (pulse should yield).
    identifying: Arc<AtomicBool>,
    /// True while a background battery poll is in flight.
    refreshing: Arc<AtomicBool>,
    /// Controllers currently in the critical low-battery bucket (serial, percent).
    low_battery: Arc<Mutex<Vec<(String, u8)>>>,

    tray_icon: Option<TrayIcon>,
    tray_anchor: TrayAnchor,

    popup_window: Option<window::Id>,
    /// Cached scale / monitor from the last popup place (skips iced queries on reopen).
    popup_scale: f32,
    popup_monitor: Option<Size>,
    popup_metrics_cached: bool,
    popup_state: popup_view::State,
    popup_rows: Vec<ControllerRow>,

    configure_window: Option<window::Id>,
    configure_state: ConfigureState,
    /// Snapshot for the Analytics settings tab (refreshed with controller/analytics changes).
    analytics_panel: AnalyticsPanel,
    start_screen_panel: StartScreenPanel,
    pad_input_panel: PadInputPanel,

    start_window: Option<window::Id>,
    start_state: start_view::State,
    /// After last pad disconnect, suppress 0→1 auto-open briefly.
    start_connect_cooldown_until: Option<Instant>,
    gesture_detector: GestureDetector,
    gesture_recorder: GestureRecorder,
    nav_stepper: NavStepper,
    button_edges: ButtonEdges,
    steam_by_id: HashMap<u32, SteamGame>,
    /// `Some` after a successful Steam library scan (installed appids). `None` = unknown.
    steam_installed: Option<Vec<u32>>,
    hid_exclusive_warned: bool,
    /// One-shot when both Gaming.Input and DualSense HID fail while start is open.
    nav_missing_warned: bool,
    /// Logged once per start-session which nav backend succeeded.
    nav_source_logged: Option<NavSource>,
    /// Last edge-logged nav snapshot (cleared when start closes).
    last_nav_log: Option<NavLogSnapshot>,
    /// One-shot when falling back from empty Gamepad to HID this start-session.
    nav_hid_fallback_logged: bool,

    toast_window: Option<window::Id>,
    toast_message: Option<ToastMessage>,
    toast_queue: VecDeque<ToastMessage>,
    toast_generation: u64,
    toast_placement: Option<ToastPlacement>,
    toast_anim_started: Instant,
    toast_dismissing: bool,

    /// Bumped on every spectrum edit; stale SpectrumCommit messages are ignored.
    spectrum_generation: u64,

    #[cfg(feature = "dev-emulate")]
    dev_mode: bool,
    #[cfg(feature = "dev-emulate")]
    emulating: bool,
    /// Percent to restore after AnalyticsPause (list is empty while paused).
    #[cfg(feature = "dev-emulate")]
    dev_paused_percent: Option<u8>,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

#[cfg(feature = "dev-emulate")]
pub fn run(dev_mode: bool) -> Result<(), Box<dyn std::error::Error>> {
    run_app(dev_mode)
}

#[cfg(not(feature = "dev-emulate"))]
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    run_app()
}

fn run_app(
    #[cfg(feature = "dev-emulate")] dev_mode: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "dev-emulate")]
    let boot = move || App::boot(dev_mode);
    #[cfg(not(feature = "dev-emulate"))]
    let boot = App::boot;

    iced::daemon(boot, App::update, App::view)
        .subscription(App::subscription)
        .title(App::title)
        .theme(App::theme)
        .antialiasing(true)
        .run()?;

    Ok(())
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

impl App {
    fn boot(#[cfg(feature = "dev-emulate")] dev_mode: bool) -> (Self, Task<Message>) {
        let prefs = Prefs::load();
        let known = KnownControllers::load();
        let analytics = AnalyticsStore::load();
        let games = GamesCatalog::load();
        color::set_active_spectrum(prefs.spectrum.clone());
        lightbar::set_enabled(prefs.lightbar_enabled);

        #[cfg(windows)]
        autostart::ensure_quiet_entry();

        let identifying = Arc::new(AtomicBool::new(false));
        let low_battery = Arc::new(Mutex::new(Vec::new()));
        start_low_battery_pulse_thread(Arc::clone(&identifying), Arc::clone(&low_battery));

        let configure_state = ConfigureState::new(prefs.spectrum.clone());

        let mut app = Self {
            prefs,
            known,
            notify: NotifyTracker::new(),
            analytics,
            games,
            controllers: Vec::new(),
            last_discovered: dualsense::list_presence_paths().unwrap_or_default(),
            last_battery_poll: Instant::now(),
            identifying,
            refreshing: Arc::new(AtomicBool::new(false)),
            low_battery,
            tray_icon: None,
            tray_anchor: TrayAnchor::default(),
            popup_window: None,
            popup_scale: 1.0,
            popup_monitor: None,
            popup_metrics_cached: false,
            popup_state: popup_view::State::default(),
            popup_rows: Vec::new(),
            configure_window: None,
            configure_state,
            analytics_panel: AnalyticsPanel::default(),
            start_screen_panel: StartScreenPanel::default(),
            pad_input_panel: PadInputPanel::default(),
            start_window: None,
            start_state: start_view::State::default(),
            start_connect_cooldown_until: None,
            gesture_detector: GestureDetector::new(),
            gesture_recorder: GestureRecorder::default(),
            nav_stepper: NavStepper::default(),
            button_edges: ButtonEdges::default(),
            steam_by_id: HashMap::new(),
            steam_installed: None,
            hid_exclusive_warned: false,
            nav_missing_warned: false,
            nav_source_logged: None,
            last_nav_log: None,
            nav_hid_fallback_logged: false,
            toast_window: None,
            toast_message: None,
            toast_queue: VecDeque::new(),
            toast_generation: 0,
            toast_placement: None,
            toast_anim_started: Instant::now(),
            toast_dismissing: false,
            spectrum_generation: 0,
            #[cfg(feature = "dev-emulate")]
            dev_mode,
            #[cfg(feature = "dev-emulate")]
            emulating: false,
            #[cfg(feature = "dev-emulate")]
            dev_paused_percent: None,
        };

        app.sync_start_panel_catalog();

        // Show the tray immediately, then poll in the background so a stuck HID
        // read cannot delay the icon for tens of seconds. Pre-create the toast
        // window hidden so iced keeps a warm GPU compositor for fast popup/Settings opens.
        app.create_tray();
        app.sync_popup_rows();
        app.sync_low_battery();
        app.refresh_analytics_panel();

        let task = Task::batch([
            app.request_refresh(),
            app.ensure_toast_window(),
            app.refresh_steam_library(),
        ]);
        (app, task)
    }

    fn title(&self, window: window::Id) -> String {
        if Some(window) == self.configure_window {
            "Settings".to_string()
        } else if Some(window) == self.start_window {
            "Quick launch".to_string()
        } else {
            crate::app_meta::DISPLAY_NAME.to_string()
        }
    }

    fn theme(&self, window: window::Id) -> Theme {
        if Some(window) == self.toast_window {
            theme::toast_theme()
        } else {
            theme::app_theme()
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            window::close_events().map(Message::WindowClosed),
            iced::event::listen_with(|event, _status, id| match event {
                iced::Event::Window(window::Event::Unfocused) => Some(Message::WindowUnfocused(id)),
                iced::Event::Keyboard(keyboard::Event::KeyPressed { key, .. }) => {
                    let action = match key {
                        keyboard::Key::Named(keyboard::key::Named::Escape) => {
                            Some(StartKeyAction::Cancel)
                        }
                        keyboard::Key::Named(keyboard::key::Named::Enter) => {
                            Some(StartKeyAction::Confirm)
                        }
                        keyboard::Key::Named(keyboard::key::Named::ArrowUp) => {
                            Some(StartKeyAction::Up)
                        }
                        keyboard::Key::Named(keyboard::key::Named::ArrowDown) => {
                            Some(StartKeyAction::Down)
                        }
                        _ => None,
                    }?;
                    Some(Message::StartKey { id, action })
                }
                _ => None,
            }),
            iced::time::every(PRESENCE_INTERVAL).map(|_| Message::Tick),
            Subscription::run(tray_events_mapped),
        ];

        if self.toast_message.is_some() {
            subscriptions.push(Subscription::run_with(
                self.toast_generation,
                |generation| escape_hotkey(*generation),
            ));
        }

        if self.toast_animating() {
            subscriptions.push(window::frames().map(|_| Message::ToastFrame));
        }

        if self.popup_state.identify_flash_active() {
            subscriptions.push(window::frames().map(|_| Message::IdentifyFrame));
        }

        let pad_input_live =
            self.configure_window.is_some() && self.configure_state.section == Section::PadInput;
        let pad_listening = pad_input_live
            || (self.prefs.start_screen_enabled
                && (!self.controllers.is_empty() || self.gesture_recorder.is_active()));
        if pad_listening {
            subscriptions.push(iced::time::every(PAD_POLL_INTERVAL).map(|_| Message::PadPoll));
        }

        Subscription::batch(subscriptions)
    }

    fn view(&self, window: window::Id) -> Element<'_, Message> {
        if Some(window) == self.popup_window {
            return popup_view::view(&self.popup_state, &self.popup_rows, &self.prefs.spectrum)
                .map(Message::Popup);
        }

        if Some(window) == self.configure_window {
            return configure_view::view(
                &self.configure_state,
                &self.configure_settings(),
                &self.analytics_panel,
                &self.start_screen_panel,
                &self.pad_input_panel,
            )
            .map(Message::Configure);
        }

        if Some(window) == self.start_window {
            return start_view::view(&self.start_state).map(Message::Start);
        }

        if Some(window) == self.toast_window {
            return match self.toast_message.as_ref() {
                Some(message) => {
                    toast_view::view(message, Message::ToastDismiss(self.toast_generation))
                }
                None => toast_view::empty(),
            };
        }

        container(space()).style(theme::root).into()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => self.on_tick(),
            Message::PollResult(result) => self.on_poll_result(result),

            Message::TrayMenu(id) => match id.as_str() {
                SETTINGS_ID => self.open_configure(),
                QUIT_ID => self.update(Message::Exit),
                _ => Task::none(),
            },
            Message::TrayLeftClick(anchor) => {
                self.tray_anchor = anchor;
                self.toggle_popup()
            }

            Message::WindowClosed(id) => {
                if Some(id) == self.popup_window {
                    self.popup_window = None;
                    self.popup_state.cancel();
                    Task::none()
                } else if Some(id) == self.configure_window {
                    self.configure_window = None;
                    self.cancel_gesture_recording();
                    Task::none()
                } else if Some(id) == self.start_window {
                    self.start_window = None;
                    self.nav_stepper.reset();
                    self.button_edges.reset();
                    self.clear_start_nav_diag();
                    Task::none()
                } else if Some(id) == self.toast_window {
                    self.toast_window = None;
                    // Recreate so iced keeps a warm compositor for popup/Settings.
                    self.ensure_toast_window()
                } else {
                    Task::none()
                }
            }
            Message::WindowUnfocused(id) => {
                // The popup is a transient tray flyout: dismiss it when focus moves away.
                if Some(id) == self.popup_window && !self.popup_state.is_editing_any() {
                    self.close_popup()
                } else if Some(id) == self.start_window {
                    self.close_start_screen()
                } else {
                    Task::none()
                }
            }

            Message::PopupOpened(id) => {
                if self.popup_metrics_cached {
                    self.reveal_popup(id)
                } else {
                    place_popup(id)
                }
            }
            Message::PlacePopup { id, scale, monitor } => {
                self.popup_scale = if scale > 0.0 { scale } else { 1.0 };
                self.popup_monitor = monitor;
                self.popup_metrics_cached = true;
                self.reveal_popup(id)
            }
            Message::Popup(message) => self.on_popup_message(message),

            Message::ConfigureOpened(id) => window::gain_focus(id),
            Message::Configure(message) => self.on_configure_message(message),

            Message::StartOpened(id) => {
                window::set_mode(id, window::Mode::Windowed).chain(window::gain_focus(id))
            }
            Message::Start(message) => self.on_start_message(message),
            Message::StartKey { id, action } => {
                if Some(id) == self.start_window {
                    return match action {
                        StartKeyAction::Up => self.on_start_message(StartMessage::MoveUp),
                        StartKeyAction::Down => self.on_start_message(StartMessage::MoveDown),
                        StartKeyAction::Confirm => self.on_start_message(StartMessage::Confirm),
                        StartKeyAction::Cancel => self.on_start_message(StartMessage::Close),
                    };
                }
                if Some(id) == self.configure_window
                    && action == StartKeyAction::Cancel
                    && self.gesture_recorder.is_active()
                {
                    self.cancel_gesture_recording();
                }
                Task::none()
            }
            Message::PadPoll => self.on_pad_poll(),
            Message::ManualFilePicked(path) => self.on_manual_file_picked(path),
            Message::SteamScanDone(result) => self.on_steam_scan_done(result),

            Message::PlaceToast { id, monitor } => {
                let placement = toast_placement(self.prefs.toast_position, monitor);
                self.toast_placement = Some(placement);
                self.toast_anim_started = Instant::now();
                self.toast_dismissing = false;
                let start = Point::new(placement.x, placement.outside_y);
                window::move_to(id, start).chain(show_toast_without_activate(id))
            }
            Message::ToastFrame => self.animate_toast(),
            Message::IdentifyFrame => {
                self.popup_state.tick_identify_flash();
                Task::none()
            }
            Message::ToastDismiss(generation) => {
                if generation == self.toast_generation {
                    self.dismiss_toast()
                } else {
                    Task::none()
                }
            }

            Message::SpectrumCommit(generation) => self.on_spectrum_commit(generation),
            Message::SpectrumHidDone => Task::none(),

            Message::Exit => {
                self.known.save();
                self.tray_icon.take();
                iced::exit()
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tray
    // -----------------------------------------------------------------------

    fn create_tray(&mut self) {
        self.tray_icon = tray::create_tray(&self.controllers);
    }

    fn apply_tray(&mut self) -> Task<Message> {
        if let Some(tray_icon) = self.tray_icon.as_mut() {
            tray::apply_tray(tray_icon, &self.controllers);
        }
        self.sync_popup_rows_and_fit()
    }

    // -----------------------------------------------------------------------
    // Controller polling
    // -----------------------------------------------------------------------

    fn on_tick(&mut self) -> Task<Message> {
        #[cfg(feature = "dev-emulate")]
        if self.emulating {
            return Task::none();
        }

        if self.identifying.load(Ordering::SeqCst) || self.refreshing.load(Ordering::SeqCst) {
            return Task::none();
        }

        let discovered = match dualsense::list_presence_paths() {
            Ok(serials) => serials,
            Err(err) => {
                app_log::warn(format!("presence scan failed: {err}"));
                return Task::none();
            }
        };

        let membership_changed = discovered != self.last_discovered;
        self.last_discovered = discovered;

        // HID list went empty — update the tray immediately. A powered-off DualSense
        // often disappears from enumeration well before the next battery poll.
        if membership_changed && self.last_discovered.is_empty() {
            // Presence-only clear never runs HID poll; drop claims so reconnect
            // always gets LIGHT_OUT + RGB.
            lightbar::sync_lightbar_claims(std::iter::empty::<&str>());
            let task = if self.controllers.is_empty() {
                Task::none()
            } else {
                self.apply_controllers(Vec::new())
            };
            self.last_battery_poll = Instant::now();
            return task;
        }

        let battery_due = self.last_battery_poll.elapsed() >= BATTERY_INTERVAL;
        let liveness_due =
            !self.controllers.is_empty() && self.last_battery_poll.elapsed() >= LIVENESS_INTERVAL;
        // HID can list a pad that we cannot open/read yet (sleeping BT, exclusive access).
        // Retry on a moderate interval — not every presence tick — so the UI stays responsive.
        let unread_retry = !self.last_discovered.is_empty()
            && self.controllers.is_empty()
            && self.last_battery_poll.elapsed() >= UNREAD_RETRY_INTERVAL;

        if membership_changed || battery_due || liveness_due || unread_retry {
            // Reassert lightbar on every poll (~5s liveness included) so other HID
            // writers (game launchers, etc.) do not keep the bar after a one-shot overwrite.
            self.request_refresh()
        } else {
            Task::none()
        }
    }

    fn request_refresh(&mut self) -> Task<Message> {
        #[cfg(feature = "dev-emulate")]
        if self.emulating {
            return Task::none();
        }

        if self.identifying.load(Ordering::SeqCst) {
            return Task::none();
        }
        if self
            .refreshing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Task::none();
        }

        let previously_connected: Vec<String> =
            self.controllers.iter().map(|c| c.serial.clone()).collect();
        Task::perform(
            spawn_blocking(move || poll::poll_controllers(&previously_connected)),
            |outcome| Message::PollResult(outcome.unwrap_or_else(Err)),
        )
    }

    fn on_poll_result(&mut self, result: Result<Vec<ControllerStatus>, String>) -> Task<Message> {
        self.refreshing.store(false, Ordering::SeqCst);
        self.last_battery_poll = Instant::now();

        #[cfg(feature = "dev-emulate")]
        if self.emulating {
            return Task::none();
        }

        match result {
            Ok(controllers) => self.apply_controllers(controllers),
            Err(err) => {
                app_log::warn(format!("refresh failed: {err}"));
                Task::none()
            }
        }
    }

    fn apply_controllers(&mut self, controllers: Vec<ControllerStatus>) -> Task<Message> {
        let known_changed = self.known.sync_from_live(&controllers);
        let controllers_changed = !controllers_equivalent(&self.controllers, &controllers);

        // Analytics heartbeat runs even when the snapshot is unchanged so open
        // sessions accumulate active time between percent/state edges.
        if self.prefs.analytics_enabled {
            let previous = self.controllers.clone();
            let next = if controllers_changed {
                controllers.as_slice()
            } else {
                self.controllers.as_slice()
            };
            let keep = |serial: &str| {
                self.known.is_remembered(serial) || self.known.nickname(serial).is_some()
            };
            self.analytics
                .observe(&previous, next, true, keep, std::time::SystemTime::now());
            self.analytics.save();
            self.refresh_analytics_panel();
        }

        if !controllers_changed && !known_changed {
            if self.prefs.analytics_enabled {
                return self.sync_popup_rows_and_fit();
            }
            return Task::none();
        }

        let mut events = Vec::new();
        let mut opened_from_empty = false;
        if controllers_changed {
            let previous = std::mem::replace(&mut self.controllers, controllers);
            opened_from_empty = previous.is_empty() && !self.controllers.is_empty();
            if !previous.is_empty() && self.controllers.is_empty() {
                self.start_connect_cooldown_until = Some(Instant::now() + START_CONNECT_COOLDOWN);
            }
            events = self
                .notify
                .evaluate(&previous, &self.controllers, &self.prefs, |serial| {
                    self.known.nickname(serial).map(str::to_string)
                });
            self.sync_low_battery();
        }

        self.known.save();
        let tray = self.apply_tray();
        let mut task = tray.chain(self.queue_notifications(events));

        if self.controllers.is_empty() && self.start_window.is_some() {
            task = task.chain(self.close_start_screen());
        } else if opened_from_empty && self.should_auto_open_start() {
            task = task.chain(self.open_start_screen());
        }

        task
    }

    fn should_auto_open_start(&self) -> bool {
        let cooldown_active = self
            .start_connect_cooldown_until
            .is_some_and(|until| Instant::now() < until);
        should_auto_open_start(
            self.prefs.start_screen_enabled,
            !self.display_catalog().is_empty(),
            true,
            true,
            self.start_window.is_some(),
            cooldown_active,
            start_input::foreground_is_exclusive_fullscreen(),
        )
    }

    fn sync_low_battery(&self) {
        let list: Vec<(String, u8)> = if lightbar::is_enabled() {
            let threshold = self.prefs.low_battery_percent;
            self.controllers
                .iter()
                .filter(|c| c.is_low_battery(threshold) && !is_emulated_serial(&c.serial))
                .map(|c| (c.serial.clone(), c.percent))
                .collect()
        } else {
            Vec::new()
        };
        if let Ok(mut guard) = self.low_battery.lock() {
            *guard = list;
        }
    }

    // -----------------------------------------------------------------------
    // Popup window
    // -----------------------------------------------------------------------

    fn sync_popup_rows(&mut self) {
        let threshold = self.prefs.low_battery_percent;
        let mut rows = Vec::new();
        for controller in &self.controllers {
            let eta = if self.prefs.analytics_enabled {
                self.analytics
                    .eta_for(controller)
                    .map(analytics::format_eta_ring)
            } else {
                None
            };
            rows.push(ControllerRow::connected(
                controller,
                self.known.is_remembered(&controller.serial),
                dualsense::is_storable_serial(&controller.serial)
                    && !is_emulated_serial(&controller.serial),
                self.known.nickname(&controller.serial).map(str::to_string),
                threshold,
                eta,
            ));
        }
        for controller in self.known.remembered_disconnected(&self.controllers) {
            let nickname = self.known.nickname(&controller.serial).map(str::to_string);
            let eta = if self.prefs.analytics_enabled {
                self.analytics
                    .eta_play_at(&controller.serial, controller.percent)
                    .map(analytics::format_eta_ring)
            } else {
                None
            };
            rows.push(ControllerRow::disconnected(controller, nickname, eta));
        }
        self.popup_rows = rows;
    }

    /// Rebuild popup rows and, when the flyout is open, resize/re-anchor if height changed.
    fn sync_popup_rows_and_fit(&mut self) -> Task<Message> {
        let before = popup_view::window_height(self.popup_rows.len());
        self.sync_popup_rows();
        let after = popup_view::window_height(self.popup_rows.len());
        if before == after {
            Task::none()
        } else {
            self.fit_popup_window()
        }
    }

    fn fit_popup_window(&self) -> Task<Message> {
        let Some(id) = self.popup_window else {
            return Task::none();
        };
        let height = popup_view::window_height(self.popup_rows.len());
        let size = Size::new(popup_view::WIDTH, height);
        let position = popup_position(self.tray_anchor, self.popup_scale, self.popup_monitor, size);
        window::resize(id, size).chain(window::move_to(id, position))
    }

    fn toggle_popup(&mut self) -> Task<Message> {
        if self.popup_window.is_some() {
            self.close_popup()
        } else {
            self.open_popup()
        }
    }

    fn open_popup(&mut self) -> Task<Message> {
        if let Some(id) = self.popup_window {
            return window::gain_focus(id);
        }

        self.popup_state.cancel();
        let height = popup_view::window_height(self.popup_rows.len());
        let (id, open) = window::open(window::Settings {
            size: Size::new(popup_view::WIDTH, height),
            position: window::Position::Default,
            visible: false,
            resizable: false,
            decorations: false,
            level: window::Level::AlwaysOnTop,
            exit_on_close_request: true,
            platform_specific: overlay_platform_specific(),
            ..window::Settings::default()
        });

        self.popup_window = Some(id);
        open.map(Message::PopupOpened)
    }

    fn reveal_popup(&self, id: window::Id) -> Task<Message> {
        let height = popup_view::window_height(self.popup_rows.len());
        let size = Size::new(popup_view::WIDTH, height);
        let position = popup_position(self.tray_anchor, self.popup_scale, self.popup_monitor, size);
        window::resize(id, size)
            .chain(window::move_to(id, position))
            .chain(window::set_mode(id, window::Mode::Windowed))
            .chain(window::gain_focus(id))
    }

    fn close_popup(&mut self) -> Task<Message> {
        self.popup_state.cancel();
        // Hide first (same as Settings) so iced's surface teardown cannot paint a
        // brief empty root-colored frame while the HWND is still visible.
        // Keep `popup_window` until WindowClosed.
        match self.popup_window {
            Some(id) => window::set_mode(id, window::Mode::Hidden).chain(window::close(id)),
            None => Task::none(),
        }
    }

    fn on_popup_message(&mut self, message: PopupMessage) -> Task<Message> {
        match message {
            PopupMessage::OpenSettings => {
                let close = self.close_popup();
                close.chain(self.open_configure())
            }
            PopupMessage::Identify(serial) => {
                if self.identify(&serial) {
                    self.popup_state.begin_identify_flash(&serial);
                }
                Task::none()
            }
            PopupMessage::PowerOff(serial) => {
                self.power_off(&serial);
                Task::none()
            }
            PopupMessage::ToggleRemember(serial) => self.toggle_remember(&serial),
            PopupMessage::BeginEdit(serial) => {
                let current = self.known.nickname(&serial).map(str::to_string);
                self.popup_state.begin_edit(&serial, current.as_deref());
                Task::batch([
                    operation::focus(popup_view::nickname_input_id()),
                    operation::select_all(popup_view::nickname_input_id()),
                ])
            }
            PopupMessage::DraftChanged(value) => {
                self.popup_state.draft = value;
                Task::none()
            }
            PopupMessage::CommitNickname => {
                if let Some((serial, nickname)) = self.popup_state.commit() {
                    self.set_nickname(&serial, nickname)
                } else {
                    Task::none()
                }
            }
            PopupMessage::CancelEdit => {
                self.popup_state.cancel();
                Task::none()
            }
        }
    }

    // -----------------------------------------------------------------------
    // Configure window
    // -----------------------------------------------------------------------

    fn configure_settings(&self) -> ConfigureSettings {
        ConfigureSettings {
            notify_low: self.prefs.notify_low,
            notify_charged: self.prefs.notify_charged,
            notify_connect: self.prefs.notify_connect,
            notify_disconnect: self.prefs.notify_disconnect,
            low_battery_percent: self.prefs.low_battery_percent,
            toast_position: self.prefs.toast_position,
            analytics_enabled: self.prefs.analytics_enabled,
            lightbar_enabled: self.prefs.lightbar_enabled,
            start_screen_enabled: self.prefs.start_screen_enabled,
            start_screen_gesture: self.prefs.start_screen_gesture.clone(),
            gesture_recording: self.gesture_recorder.is_active(),
            gesture_recording_live: {
                let peak: Vec<_> = self.gesture_recorder.peak().iter().copied().collect();
                if peak.is_empty() {
                    String::new()
                } else {
                    gesture::format_gesture(&peak)
                }
            },
            #[cfg(windows)]
            autostart: autostart::is_enabled(),
            show_developer: {
                #[cfg(feature = "dev-emulate")]
                {
                    self.dev_mode
                }
                #[cfg(not(feature = "dev-emulate"))]
                {
                    false
                }
            },
        }
    }

    fn analytics_panel(&self) -> AnalyticsPanel {
        if !self.prefs.analytics_enabled {
            return AnalyticsPanel::default();
        }
        let rows = self
            .analytics
            .panel_rows()
            .iter()
            .map(|row| {
                let label = self
                    .known
                    .nickname(&row.serial)
                    .map(str::to_string)
                    .filter(|name| !name.is_empty())
                    .or_else(|| {
                        self.controllers
                            .iter()
                            .find(|c| c.serial == row.serial)
                            .map(|c| c.product.to_string())
                    })
                    .unwrap_or_else(|| {
                        let serial = &row.serial;
                        if serial.len() > 8 {
                            format!("Pad …{}", &serial[serial.len() - 6..])
                        } else {
                            serial.clone()
                        }
                    });
                AnalyticsPadRow::from_analytics(row, label)
            })
            .collect();
        AnalyticsPanel { rows }
    }

    fn refresh_analytics_panel(&mut self) {
        self.analytics_panel = self.analytics_panel();
    }

    fn open_configure(&mut self) -> Task<Message> {
        if let Some(id) = self.configure_window {
            return window::gain_focus(id);
        }

        self.configure_state
            .set_spectrum(self.prefs.spectrum.clone());

        let (id, open) = window::open(window::Settings {
            size: Size::new(configure_view::WIDTH, configure_view::HEIGHT),
            position: window::Position::Centered,
            resizable: false,
            decorations: false,
            exit_on_close_request: true,
            platform_specific: window_platform_specific(),
            ..window::Settings::default()
        });

        self.configure_window = Some(id);
        open.map(Message::ConfigureOpened)
    }

    fn on_configure_message(&mut self, message: ConfigureMessage) -> Task<Message> {
        match message {
            ConfigureMessage::SelectSection(section) => {
                if self.configure_state.section == Section::StartScreen
                    && section != Section::StartScreen
                {
                    self.cancel_gesture_recording();
                }
                self.configure_state.select_section(section);
                if section == Section::StartScreen {
                    return self.refresh_steam_library();
                }
                if section == Section::PadInput {
                    return self.refresh_pad_input_panel();
                }
                Task::none()
            }
            ConfigureMessage::SetNotification(setting, enabled) => {
                match setting {
                    NotificationSetting::Connect => self.prefs.notify_connect = enabled,
                    NotificationSetting::Disconnect => self.prefs.notify_disconnect = enabled,
                    NotificationSetting::Low => self.prefs.notify_low = enabled,
                    NotificationSetting::Charged => self.prefs.notify_charged = enabled,
                }
                self.prefs.save();
                Task::none()
            }
            ConfigureMessage::SetLowBatteryPercent(percent) => {
                let percent = clamp_low_battery_percent(percent);
                if self.prefs.low_battery_percent != percent {
                    self.prefs.low_battery_percent = percent;
                    self.prefs.save();
                    self.sync_low_battery();
                    return self.sync_popup_rows_and_fit();
                }
                Task::none()
            }
            ConfigureMessage::SetToastPosition(position) => {
                self.prefs.toast_position = position;
                self.prefs.save();
                self.show_position_preview()
            }
            ConfigureMessage::SetAnalyticsEnabled(enabled) => {
                self.prefs.analytics_enabled = enabled;
                self.prefs.save();
                self.refresh_analytics_panel();
                self.sync_popup_rows_and_fit()
            }
            ConfigureMessage::SetLightbarEnabled(enabled) => {
                self.prefs.lightbar_enabled = enabled;
                self.prefs.save();
                lightbar::set_enabled(enabled);
                self.sync_low_battery();
                if enabled {
                    // Re-apply spectrum colors now that automatic writes are back on.
                    self.apply_spectrum(self.prefs.spectrum.clone())
                } else {
                    Task::none()
                }
            }
            ConfigureMessage::SetStartScreenEnabled(enabled) => {
                self.prefs.start_screen_enabled = enabled;
                self.prefs.save();
                Task::none()
            }
            ConfigureMessage::PreviewStartScreen => self.open_start_screen(),
            ConfigureMessage::StartGestureRecord => {
                self.gesture_recorder.start();
                self.gesture_detector.reset();
                Task::none()
            }
            ConfigureMessage::ResetStartGesture => {
                self.cancel_gesture_recording();
                self.prefs.start_screen_gesture = gesture::default_gesture();
                self.prefs.save();
                Task::none()
            }
            ConfigureMessage::CancelGestureRecord => {
                self.cancel_gesture_recording();
                Task::none()
            }
            ConfigureMessage::RefreshSteamLibrary => self.refresh_steam_library(),
            ConfigureMessage::ToggleSteamGame(appid, enabled) => {
                if self.games.toggle_steam(appid, enabled) {
                    self.games.save();
                    self.sync_start_panel_catalog();
                    self.refresh_start_rows();
                }
                Task::none()
            }
            ConfigureMessage::AddManualPath => {
                let draft = self.start_screen_panel.manual_draft.trim().to_string();
                if draft.is_empty() {
                    return Task::none();
                }
                let title = games::title_from_target(&draft);
                self.games.add_manual(title, draft);
                self.games.save();
                self.start_screen_panel.manual_draft.clear();
                self.sync_start_panel_catalog();
                self.refresh_start_rows();
                Task::none()
            }
            ConfigureMessage::PickManualFile => Task::perform(
                spawn_blocking(|| {
                    rfd::FileDialog::new()
                        .add_filter("Programs", &["exe", "lnk", "url"])
                        .pick_file()
                        .map(|p| p.to_string_lossy().into_owned())
                }),
                |result| match result {
                    Ok(path) => Message::ManualFilePicked(path.map(PathBuf::from)),
                    Err(_) => Message::ManualFilePicked(None),
                },
            ),
            ConfigureMessage::ManualDraftChanged(value) => {
                self.start_screen_panel.manual_draft = value;
                Task::none()
            }
            ConfigureMessage::RenameCatalogEntry(index, title) => {
                if self.games.set_manual_title(index, title) {
                    self.games.save();
                    self.sync_start_panel_catalog();
                    self.refresh_start_rows();
                }
                Task::none()
            }
            ConfigureMessage::MoveCatalogEntry(index, up) => {
                if self.games.move_entry(index, up) {
                    self.games.save();
                    self.sync_start_panel_catalog();
                    self.refresh_start_rows();
                }
                Task::none()
            }
            ConfigureMessage::RemoveCatalogEntry(index) => {
                if self.games.remove_at(index) {
                    self.games.save();
                    self.sync_start_panel_catalog();
                    self.refresh_start_rows();
                }
                Task::none()
            }
            ConfigureMessage::OpenDataFolder => {
                if let Err(err) = paths::open_data_folder() {
                    app_log::warn(format!("open data folder failed: {err}"));
                }
                Task::none()
            }
            #[cfg(windows)]
            ConfigureMessage::SetAutostart(enabled) => {
                if let Err(err) = autostart::set_enabled(enabled) {
                    app_log::error(format!("autostart toggle failed: {err}"));
                }
                Task::none()
            }
            ConfigureMessage::SelectStop(index) => {
                self.configure_state.select(index);
                Task::none()
            }
            ConfigureMessage::MoveStop(percent) => {
                let next = self.configure_state.move_stop(percent);
                self.apply_spectrum_maybe(next)
            }
            ConfigureMessage::AddStopAt(percent) => {
                let next = self.configure_state.add_stop_at(percent);
                self.apply_spectrum_maybe(next)
            }
            ConfigureMessage::RemoveStop => {
                let next = self.configure_state.remove_selected();
                self.apply_spectrum_maybe(next)
            }
            ConfigureMessage::HueChanged(hue) => {
                let next = self.configure_state.set_hue(hue);
                self.apply_spectrum_maybe(next)
            }
            ConfigureMessage::SaturationValueChanged(saturation, value) => {
                let next = self.configure_state.set_saturation_value(saturation, value);
                self.apply_spectrum_maybe(next)
            }
            ConfigureMessage::ResetSpectrum => {
                let next = self.configure_state.reset();
                self.apply_spectrum(next)
            }
            #[cfg(feature = "dev-emulate")]
            ConfigureMessage::DeveloperPreset(preset) => self.apply_dev_preset(preset),
            // Hide first so DWM cannot flash the default (white) brush while the
            // wgpu surface is torn down; keep id until WindowClosed.
            ConfigureMessage::Close => {
                self.cancel_gesture_recording();
                match self.configure_window {
                    Some(id) => window::set_mode(id, window::Mode::Hidden).chain(window::close(id)),
                    None => Task::none(),
                }
            }
            ConfigureMessage::DragWindow => match self.configure_window {
                Some(id) => window::drag(id),
                None => Task::none(),
            },
        }
    }

    fn apply_spectrum_maybe(&mut self, spectrum: Option<BatterySpectrum>) -> Task<Message> {
        if let Some(spectrum) = spectrum {
            self.apply_spectrum(spectrum)
        } else {
            Task::none()
        }
    }

    /// Update in-memory spectrum immediately; debounce prefs save + HID write.
    fn apply_spectrum(&mut self, spectrum: BatterySpectrum) -> Task<Message> {
        self.prefs.spectrum = spectrum.clone();
        color::set_active_spectrum(spectrum);
        self.spectrum_generation = self.spectrum_generation.wrapping_add(1);
        let generation = self.spectrum_generation;
        Task::perform(delay(SPECTRUM_DEBOUNCE), move |_| {
            Message::SpectrumCommit(generation)
        })
    }

    fn on_spectrum_commit(&mut self, generation: u64) -> Task<Message> {
        if self.spectrum_generation != generation {
            return Task::none();
        }

        self.prefs.save();

        if !lightbar::is_enabled() {
            return Task::none();
        }

        let targets: Vec<(String, color::Rgb)> = self
            .controllers
            .iter()
            .filter(|c| !is_emulated_serial(&c.serial))
            .map(|c| {
                (
                    c.serial.clone(),
                    self.prefs.spectrum.color_at_percent(c.percent),
                )
            })
            .collect();

        if targets.is_empty() {
            return Task::none();
        }

        Task::perform(
            spawn_blocking(move || {
                for (serial, color) in targets {
                    if let Err(err) = lightbar::apply_lightbar_rgb(&serial, color) {
                        app_log::warn(format!(
                            "failed to apply spectrum color for {serial}: {err}"
                        ));
                    }
                }
            }),
            |_| Message::SpectrumHidDone,
        )
    }

    // -----------------------------------------------------------------------
    // Controller actions
    // -----------------------------------------------------------------------

    fn identify(&self, serial: &str) -> bool {
        if is_emulated_serial(serial) {
            app_log::info(format!("identify skipped for emulated controller {serial}"));
            return false;
        }

        let Some(controller) = self.controllers.iter().find(|c| c.serial == serial) else {
            return false;
        };

        if self
            .identifying
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }

        let serial = serial.to_string();
        let percent = controller.percent;
        let identifying = Arc::clone(&self.identifying);

        thread::spawn(move || {
            if let Err(err) = lightbar::identify_controller(&serial, percent) {
                app_log::warn(format!("identify failed for {serial}: {err}"));
            }
            identifying.store(false, Ordering::SeqCst);
        });
        true
    }

    fn power_off(&self, serial: &str) {
        if is_emulated_serial(serial) {
            app_log::info(format!(
                "power-off skipped for emulated controller {serial}"
            ));
            return;
        }

        let Some(controller) = self.controllers.iter().find(|c| c.serial == serial) else {
            return;
        };
        if controller.connection != "Bluetooth" {
            app_log::warn(format!(
                "power-off ignored for {serial} ({})",
                controller.connection
            ));
            return;
        }

        let serial = serial.to_string();
        thread::spawn(move || {
            if let Err(err) = battery::power_off_bluetooth(&serial) {
                app_log::warn(format!("power-off failed for {serial}: {err}"));
            } else {
                app_log::info(format!("power-off sent for {serial}"));
            }
        });
    }

    fn toggle_remember(&mut self, serial: &str) -> Task<Message> {
        if is_emulated_serial(serial) {
            return Task::none();
        }

        if self.known.is_remembered(serial) {
            self.known.forget(serial);
        } else if let Some(controller) = self.controllers.iter().find(|c| c.serial == serial) {
            self.known.remember(controller);
        }

        self.known.save();
        self.sync_popup_rows_and_fit()
    }

    fn set_nickname(&mut self, serial: &str, nickname: Option<String>) -> Task<Message> {
        if is_emulated_serial(serial) {
            return Task::none();
        }
        if self.known.set_nickname(serial, nickname) {
            self.known.save();
            self.sync_popup_rows_and_fit()
        } else {
            Task::none()
        }
    }

    #[cfg(feature = "dev-emulate")]
    fn apply_dev_preset(&mut self, preset: Preset) -> Task<Message> {
        if preset == Preset::Clear {
            self.emulating = false;
            self.dev_paused_percent = None;
            let task = self.apply_controllers(Vec::new());
            self.last_battery_poll = Instant::now();
            return task.chain(self.request_refresh());
        }

        if preset.is_analytics() && !self.prefs.analytics_enabled {
            self.prefs.analytics_enabled = true;
            self.prefs.save();
            app_log::info("analytics enabled for developer preset");
        }

        // Credit active time before state edges that may finish a cycle.
        if matches!(
            preset,
            Preset::AnalyticsChargeAdvance
                | Preset::AnalyticsDrainAdvance
                | Preset::AnalyticsPlugMidDrain
        ) {
            let credit = if preset == Preset::AnalyticsChargeAdvance {
                Duration::from_secs(15 * 60)
            } else {
                Duration::from_secs(25 * 60)
            };
            self.analytics
                .dev_credit_active(emulate::PRIMARY_SERIAL, credit);
        }

        if preset == Preset::AnalyticsSeedEstimates {
            self.analytics.dev_seed_estimates(emulate::PRIMARY_SERIAL);
            self.analytics.save();
            self.refresh_analytics_panel();
        }

        if preset == Preset::AnalyticsPause {
            self.dev_paused_percent = self
                .controllers
                .iter()
                .find(|c| c.serial == emulate::PRIMARY_SERIAL)
                .map(|c| c.percent)
                .or(self.dev_paused_percent);
        }

        // Unplug-from-full needs a Complete → Discharging edge.
        let ensure_complete = preset == Preset::AnalyticsUnplugFull
            && !self.controllers.iter().any(|c| {
                c.serial == emulate::PRIMARY_SERIAL && c.state == battery::PowerState::Complete
            });

        let next = if preset == Preset::AnalyticsResume {
            let percent = self
                .dev_paused_percent
                .or_else(|| {
                    self.analytics
                        .in_progress(emulate::PRIMARY_SERIAL)
                        .map(|p| p.percent)
                })
                .unwrap_or(60);
            vec![ControllerStatus {
                index: 1,
                product: "DualSense",
                connection: "Bluetooth",
                serial: emulate::PRIMARY_SERIAL.to_string(),
                percent,
                state: battery::PowerState::Discharging,
            }]
        } else {
            emulate::apply_preset(preset, &self.controllers)
        };

        self.emulating = true;
        self.analytics.save();
        self.refresh_analytics_panel();

        if ensure_complete {
            let complete = vec![ControllerStatus {
                index: 1,
                product: "DualSense",
                connection: "USB",
                serial: emulate::PRIMARY_SERIAL.to_string(),
                percent: 100,
                state: battery::PowerState::Complete,
            }];
            let first = self.apply_controllers(complete);
            let second = self.apply_controllers(next);
            return first.chain(second);
        }

        self.apply_controllers(next)
    }

    // -----------------------------------------------------------------------
    // Start screen
    // -----------------------------------------------------------------------

    fn display_catalog(&self) -> Vec<crate::games::GameEntry> {
        self.games
            .merge_for_display(self.steam_installed.as_deref())
    }

    fn sync_start_panel_catalog(&mut self) {
        self.start_screen_panel.catalog = self.games.entries.clone();
    }

    fn refresh_start_rows(&mut self) {
        let rows: Vec<_> = self
            .display_catalog()
            .iter()
            .map(|entry| start_view::StartRow::from_entry(entry, &self.steam_by_id))
            .collect();
        self.start_state.set_rows(rows);
    }

    fn refresh_steam_library(&self) -> Task<Message> {
        Task::perform(
            spawn_blocking(steam::list_installed_games),
            |result| match result {
                Ok(Ok(games)) => Message::SteamScanDone(Ok(games)),
                Ok(Err(err)) => Message::SteamScanDone(Err(err)),
                Err(err) => Message::SteamScanDone(Err(err)),
            },
        )
    }

    fn on_steam_scan_done(&mut self, result: Result<Vec<SteamGame>, String>) -> Task<Message> {
        match result {
            Ok(games) => {
                self.steam_installed = Some(games.iter().map(|g| g.appid).collect());
                self.steam_by_id = games.iter().map(|g| (g.appid, g.clone())).collect();
                self.start_screen_panel.steam_games = games;
                self.start_screen_panel.steam_error = None;
            }
            Err(err) => {
                steam::warn_scan_error(&err);
                // Keep prior successful install list if any; otherwise stay Unknown so
                // curated Steam rows remain visible.
                self.start_screen_panel.steam_error = Some(err);
                self.start_screen_panel.steam_games.clear();
            }
        }
        self.refresh_start_rows();
        Task::none()
    }

    fn on_manual_file_picked(&mut self, path: Option<PathBuf>) -> Task<Message> {
        let Some(path) = path else {
            return Task::none();
        };
        let target = path.to_string_lossy().into_owned();
        let title = games::title_from_target(&target);
        self.games.add_manual(title, target);
        self.games.save();
        self.sync_start_panel_catalog();
        self.refresh_start_rows();
        Task::none()
    }

    fn cancel_gesture_recording(&mut self) {
        self.gesture_recorder.cancel();
    }

    fn open_start_screen(&mut self) -> Task<Message> {
        if !self.prefs.start_screen_enabled {
            return Task::none();
        }
        self.refresh_start_rows();
        if self.start_state.rows.is_empty() {
            return Task::none();
        }
        if let Some(id) = self.start_window {
            return window::gain_focus(id);
        }

        self.nav_stepper.reset();
        self.button_edges.reset();
        self.gesture_detector.reset();
        self.clear_start_nav_diag();

        let (id, open) = window::open(window::Settings {
            size: Size::new(start_view::WIDTH, start_view::HEIGHT),
            position: window::Position::Centered,
            visible: false,
            resizable: false,
            decorations: false,
            level: window::Level::AlwaysOnTop,
            exit_on_close_request: true,
            platform_specific: overlay_platform_specific(),
            ..window::Settings::default()
        });
        self.start_window = Some(id);
        open.map(Message::StartOpened)
    }

    fn close_start_screen(&mut self) -> Task<Message> {
        self.nav_stepper.reset();
        self.button_edges.reset();
        self.clear_start_nav_diag();
        match self.start_window {
            Some(id) => window::set_mode(id, window::Mode::Hidden).chain(window::close(id)),
            None => Task::none(),
        }
    }

    fn clear_start_nav_diag(&mut self) {
        self.nav_missing_warned = false;
        self.nav_source_logged = None;
        self.last_nav_log = None;
        self.nav_hid_fallback_logged = false;
    }

    fn log_start_nav_diag(&mut self, sample: &start_input::PadSample, source: NavSource) {
        if self.nav_source_logged.is_none() {
            if source == NavSource::Hid && !self.nav_hid_fallback_logged {
                app_log::info(
                    "start-nav: Gaming.Input empty or unavailable; using DualSense HID fallback",
                );
                self.nav_hid_fallback_logged = true;
            }
            if source == NavSource::Hid {
                app_log::info(format!(
                    "start-nav: source={} buttons0=0x{:02x}",
                    source.as_str(),
                    sample.buttons0
                ));
            } else {
                app_log::info(format!("start-nav: source={}", source.as_str()));
            }
            self.nav_source_logged = Some(source);
        }

        let snap = NavLogSnapshot::from_sample(sample);
        if self.last_nav_log != Some(snap) {
            app_log::info(snap.format_line(source));
            self.last_nav_log = Some(snap);
        }
    }

    fn on_start_message(&mut self, message: StartMessage) -> Task<Message> {
        match message {
            StartMessage::Launch(index) => {
                if index < self.start_state.rows.len() {
                    self.start_state.selected = index;
                }
                self.launch_selected()
            }
            StartMessage::MoveUp => {
                self.start_state.move_selection(-1);
                Task::none()
            }
            StartMessage::MoveDown => {
                self.start_state.move_selection(1);
                Task::none()
            }
            StartMessage::Confirm => self.launch_selected(),
            StartMessage::Close => self.close_start_screen(),
        }
    }

    fn launch_selected(&mut self) -> Task<Message> {
        let Some(target) = self.start_state.selected_target().map(str::to_string) else {
            return Task::none();
        };
        match launch::launch_target(&target) {
            Ok(()) => self.close_start_screen(),
            Err(err) => {
                app_log::warn(format!("launch failed for {target}: {err}"));
                // Keep the card open so a failed launch is obvious (not a silent no-op).
                Task::none()
            }
        }
    }

    fn on_pad_poll(&mut self) -> Task<Message> {
        let mut task = Task::none();
        if self.configure_window.is_some() && self.configure_state.section == Section::PadInput {
            task = task.chain(self.refresh_pad_input_panel());
        }

        let listening = self.prefs.start_screen_enabled
            && (!self.controllers.is_empty() || self.gesture_recorder.is_active());
        if !listening {
            return task;
        }

        // Gesture recording needs raw DualSense bits (L2/R2/L3/R3, etc.).
        if self.gesture_recorder.is_active() {
            let Some(sample) = start_input::read_gesture_sample() else {
                if !self.hid_exclusive_warned && !self.controllers.is_empty() {
                    app_log::warn(
                        "could not read DualSense HID for gesture recording (device may be exclusive); try again",
                    );
                    self.hid_exclusive_warned = true;
                }
                return task;
            };
            self.hid_exclusive_warned = false;
            if let Some(peak) = self.gesture_recorder.update(&sample.held) {
                self.prefs.start_screen_gesture = peak;
                self.prefs.save();
            }
            return task;
        }

        if self.start_window.is_some() {
            match start_input::read_nav_sample() {
                start_input::NavReadOutcome::Missing {
                    hid_fallback_attempted,
                } => {
                    if hid_fallback_attempted && !self.nav_hid_fallback_logged {
                        app_log::info(
                            "start-nav: Gaming.Input empty or unavailable; DualSense HID fallback attempted",
                        );
                        self.nav_hid_fallback_logged = true;
                    }
                    if !self.nav_missing_warned && !self.controllers.is_empty() {
                        app_log::warn(
                            "start-nav: no Gaming.Input gamepad and DualSense HID read failed; keyboard/mouse still work",
                        );
                        self.nav_missing_warned = true;
                    }
                    return task;
                }
                start_input::NavReadOutcome::Sample(reading) => {
                    self.nav_missing_warned = false;
                    self.log_start_nav_diag(&reading.sample, reading.source);
                    let sample = reading.sample;

                    let now = Instant::now();
                    if let Some(action) = self.nav_stepper.update(&sample, now) {
                        let next = match action {
                            NavAction::Up => self.on_start_message(StartMessage::MoveUp),
                            NavAction::Down => self.on_start_message(StartMessage::MoveDown),
                            NavAction::Confirm | NavAction::Cancel => Task::none(),
                        };
                        task = task.chain(next);
                    }
                    if let Some(action) = self.button_edges.update(&sample) {
                        let next = match action {
                            NavAction::Confirm => self.on_start_message(StartMessage::Confirm),
                            NavAction::Cancel => self.on_start_message(StartMessage::Close),
                            NavAction::Up | NavAction::Down => Task::none(),
                        };
                        return task.chain(next);
                    }
                    return task;
                }
            }
        }

        // Background reopen gesture via short DualSense HID read.
        let Some(sample) = start_input::read_gesture_sample() else {
            return task;
        };
        if self.prefs.start_screen_enabled
            && !self.prefs.start_screen_gesture.is_empty()
            && self
                .gesture_detector
                .update(&self.prefs.start_screen_gesture, &sample.held)
        {
            return task.chain(self.open_start_screen());
        }

        task
    }

    fn refresh_pad_input_panel(&mut self) -> Task<Message> {
        // Full DualSense HID sample for held controls (same short open/read/close as gestures).
        let next = if let Some(sample) = start_input::read_gesture_sample() {
            let held: Vec<_> = sample.held.iter().copied().collect();
            PadInputPanel {
                has_sample: true,
                source: "hid".to_string(),
                buttons0: Some(sample.buttons0),
                cross: sample.cross,
                circle: sample.circle,
                dpad_up: sample.dpad_up,
                dpad_down: sample.dpad_down,
                stick_band: start_input::StickBand::from_stick_y(sample.stick_y)
                    .as_str()
                    .to_string(),
                stick_y: sample.stick_y,
                held: gesture::format_gesture(&held),
            }
        } else {
            match start_input::read_nav_sample() {
                start_input::NavReadOutcome::Sample(reading) => {
                    let held: Vec<_> = reading.sample.held.iter().copied().collect();
                    PadInputPanel {
                        has_sample: true,
                        source: reading.source.as_str().to_string(),
                        buttons0: if reading.source == NavSource::Hid {
                            Some(reading.sample.buttons0)
                        } else {
                            None
                        },
                        cross: reading.sample.cross,
                        circle: reading.sample.circle,
                        dpad_up: reading.sample.dpad_up,
                        dpad_down: reading.sample.dpad_down,
                        stick_band: start_input::StickBand::from_stick_y(reading.sample.stick_y)
                            .as_str()
                            .to_string(),
                        stick_y: reading.sample.stick_y,
                        held: gesture::format_gesture(&held),
                    }
                }
                start_input::NavReadOutcome::Missing { .. } => PadInputPanel {
                    has_sample: false,
                    source: "none".to_string(),
                    ..PadInputPanel::default()
                },
            }
        };

        if next == self.pad_input_panel {
            return Task::none();
        }
        self.pad_input_panel = next;
        Task::none()
    }

    // -----------------------------------------------------------------------
    // Toasts
    // -----------------------------------------------------------------------

    fn queue_notifications(&mut self, events: Vec<NotifyEvent>) -> Task<Message> {
        for event in events {
            let eta = self.toast_eta_for(&event);
            self.toast_queue.push_back(ToastMessage::from_notification(
                event,
                self.prefs.spectrum.clone(),
                eta,
            ));
        }
        self.show_next_toast()
    }

    fn toast_eta_for(&self, event: &NotifyEvent) -> Option<String> {
        if !self.prefs.analytics_enabled {
            return None;
        }
        let status = ControllerStatus {
            index: 0,
            product: "DualSense",
            connection: "",
            serial: event.serial.clone(),
            percent: event.percent.unwrap_or(0),
            state: event.state,
        };
        self.analytics
            .eta_for(&status)
            .map(analytics::format_eta_ring)
    }

    fn show_position_preview(&mut self) -> Task<Message> {
        self.toast_queue.clear();
        self.toast_queue
            .push_back(ToastMessage::preview(&self.prefs.spectrum));
        let finish = self.finish_toast();
        finish.chain(self.show_next_toast())
    }

    /// Pre-create the toast window hidden so iced keeps a warm GPU compositor.
    /// Also used as the first-toast path; does not show or queue a message.
    fn ensure_toast_window(&mut self) -> Task<Message> {
        if self.toast_window.is_some() {
            return Task::none();
        }
        let (_id, open) = self.create_toast_window();
        open.discard()
    }

    fn create_toast_window(&mut self) -> (window::Id, Task<window::Id>) {
        let (id, open) = window::open(window::Settings {
            size: Size::new(toast_view::WIDTH, toast_view::HEIGHT),
            position: window::Position::Default,
            visible: false,
            resizable: false,
            decorations: false,
            transparent: true,
            level: window::Level::AlwaysOnTop,
            exit_on_close_request: false,
            platform_specific: overlay_platform_specific(),
            ..window::Settings::default()
        });
        self.toast_window = Some(id);
        (id, open)
    }

    fn show_next_toast(&mut self) -> Task<Message> {
        if self.toast_message.is_some() {
            return Task::none();
        }
        let Some(message) = self.toast_queue.pop_front() else {
            return Task::none();
        };

        self.toast_message = Some(message);
        self.toast_generation = self.toast_generation.wrapping_add(1);
        let generation = self.toast_generation;

        let expire = Task::perform(delay(TOAST_LIFETIME), move |()| {
            Message::ToastDismiss(generation)
        });

        if let Some(id) = self.toast_window {
            return place_toast(id).chain(expire);
        }

        let (_id, open) = self.create_toast_window();
        open.then(place_toast).chain(expire)
    }

    fn dismiss_toast(&mut self) -> Task<Message> {
        if self.toast_message.is_none() || self.toast_dismissing {
            return Task::none();
        }
        self.toast_dismissing = true;
        self.toast_anim_started = Instant::now();
        self.animate_toast()
    }

    fn animate_toast(&mut self) -> Task<Message> {
        let Some(id) = self.toast_window else {
            return Task::none();
        };
        let Some(placement) = self.toast_placement else {
            return Task::none();
        };

        let progress = (self.toast_anim_started.elapsed().as_secs_f32()
            / TOAST_SLIDE_DURATION.as_secs_f32())
        .min(1.0);
        let y = slide_y(placement, progress, self.toast_dismissing);
        let move_task = window::move_to(id, Point::new(placement.x, y));

        if self.toast_dismissing && progress >= 1.0 {
            move_task.chain(self.finish_toast())
        } else {
            move_task
        }
    }

    fn finish_toast(&mut self) -> Task<Message> {
        if self.toast_message.take().is_none() {
            return Task::none();
        }
        // Invalidate any in-flight expiry for this toast.
        self.toast_generation = self.toast_generation.wrapping_add(1);
        self.toast_placement = None;
        self.toast_dismissing = false;

        // Keep the window alive but hidden: it doubles as the GPU compositor
        // sentinel so popup/Settings can open without a cold wgpu init.
        let hide = match self.toast_window {
            Some(id) => hide_toast(id),
            None => Task::none(),
        };
        hide.chain(self.show_next_toast())
    }

    fn toast_animating(&self) -> bool {
        self.toast_message.is_some()
            && self.toast_placement.is_some()
            && (self.toast_dismissing || self.toast_anim_started.elapsed() < TOAST_SLIDE_DURATION)
    }
}

// ---------------------------------------------------------------------------
// Window placement (message-producing wrappers)
// ---------------------------------------------------------------------------

fn place_popup(id: window::Id) -> Task<Message> {
    window::scale_factor(id).then(move |scale| {
        window::monitor_size(id).map(move |monitor| Message::PlacePopup { id, scale, monitor })
    })
}

fn place_toast(id: window::Id) -> Task<Message> {
    window::monitor_size(id).map(move |monitor| Message::PlaceToast { id, monitor })
}

/// Map tray events into iced `Message` values.
fn tray_events_mapped() -> impl Stream<Item = Message> {
    tray::tray_events().map(|event| match event {
        tray::TrayEvent::Menu(id) => Message::TrayMenu(id),
        tray::TrayEvent::LeftClick(anchor) => Message::TrayLeftClick(anchor),
    })
}

/// Global Escape hotkey while an overlay toast is visible.
///
/// The toast window never takes focus, so a regular keyboard subscription would
/// not see the key press.
fn escape_hotkey(generation: u64) -> impl Stream<Item = Message> {
    stream::channel(1, async move |mut output: mpsc::Sender<Message>| {
        if let Some(()) = wait_for_escape().await {
            let _ = output.try_send(Message::ToastDismiss(generation));
        }
    })
}

#[cfg(windows)]
async fn wait_for_escape() -> Option<()> {
    let (id_sender, id_receiver) = std::sync::mpsc::channel();
    let (sender, receiver) = oneshot::channel();

    thread::spawn(move || unsafe {
        // `RegisterHotKey` binds to the calling thread's message queue, so the
        // whole lifecycle has to live on this worker.
        let _ = id_sender.send(win32::GetCurrentThreadId());
        if win32::RegisterHotKey(0, win32::HOTKEY_ID, win32::MOD_NOREPEAT, win32::VK_ESCAPE) == 0 {
            return;
        }

        let mut message = win32::Message::default();
        while win32::GetMessageW(&mut message, 0, 0, 0) > 0 {
            if message.message == win32::WM_HOTKEY && message.w_param == win32::HOTKEY_ID as usize {
                let _ = sender.send(());
                break;
            }
        }

        win32::UnregisterHotKey(0, win32::HOTKEY_ID);
    });

    // Posting `WM_QUIT` unblocks `GetMessageW` when the toast is dismissed by
    // some other means and this future is dropped.
    let _guard = win32::ThreadQuitGuard(id_receiver.recv().unwrap_or(0));
    receiver.await.ok()
}

#[cfg(not(windows))]
async fn wait_for_escape() -> Option<()> {
    std::future::pending::<()>().await
}

// ---------------------------------------------------------------------------
// Background helpers
// ---------------------------------------------------------------------------

/// Run a blocking closure on a worker thread and await its result.
fn spawn_blocking<T, F>(f: F) -> impl Future<Output = Result<T, String>> + Send
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (sender, receiver) = oneshot::channel();
    thread::spawn(move || {
        let _ = sender.send(f());
    });
    async move {
        receiver
            .await
            .map_err(|_| "background task was cancelled".to_string())
    }
}

/// Timer future backed by a worker thread, so no async runtime is required.
fn delay(duration: Duration) -> impl Future<Output = ()> + Send {
    let (sender, receiver) = oneshot::channel::<()>();
    thread::spawn(move || {
        thread::sleep(duration);
        let _ = sender.send(());
    });
    async move {
        let _ = receiver.await;
    }
}

fn start_low_battery_pulse_thread(
    identifying: Arc<AtomicBool>,
    low_battery: Arc<Mutex<Vec<(String, u8)>>>,
) {
    thread::spawn(move || {
        let on = Duration::from_millis(LOW_BATTERY_PULSE_ON_MS);
        let gap = Duration::from_millis(LOW_BATTERY_PULSE_GAP_MS);

        loop {
            thread::sleep(gap);

            if identifying.load(Ordering::SeqCst) || !lightbar::is_enabled() {
                continue;
            }

            let targets = low_battery
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default();

            for (serial, percent) in targets {
                if identifying.load(Ordering::SeqCst) || !lightbar::is_enabled() {
                    break;
                }

                if is_emulated_serial(&serial) {
                    continue;
                }

                if let Err(err) = lightbar::apply_lightbar_rgb(&serial, LOW_BATTERY_ORANGE) {
                    app_log::warn(format!("low-battery pulse failed for {serial}: {err}"));
                }
                thread::sleep(on);

                if identifying.load(Ordering::SeqCst) || !lightbar::is_enabled() {
                    break;
                }

                let color = color_for_battery_percent(percent);
                if let Err(err) = lightbar::apply_lightbar_rgb(&serial, color) {
                    app_log::warn(format!("low-battery restore failed for {serial}: {err}"));
                }
            }
        }
    });
}

fn is_emulated_serial(serial: &str) -> bool {
    #[cfg(feature = "dev-emulate")]
    {
        emulate::is_emulated(serial)
    }
    #[cfg(not(feature = "dev-emulate"))]
    {
        let _ = serial;
        false
    }
}

fn controllers_equivalent(a: &[ControllerStatus], b: &[ControllerStatus]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(x, y)| {
        x.serial == y.serial
            && x.percent == y.percent
            && x.state == y.state
            && x.connection == y.connection
            && x.product == y.product
    })
}

/// Gate for automatic start-screen open on a 0→1 controller connect.
pub(crate) fn should_auto_open_start(
    enabled: bool,
    catalog_nonempty: bool,
    previous_empty: bool,
    next_nonempty: bool,
    already_open: bool,
    cooldown_active: bool,
    fullscreen: bool,
) -> bool {
    enabled
        && catalog_nonempty
        && previous_empty
        && next_nonempty
        && !already_open
        && !cooldown_active
        && !fullscreen
}

#[cfg(test)]
mod start_gate_tests {
    use super::should_auto_open_start;

    #[test]
    fn opens_on_clean_zero_to_one() {
        assert!(should_auto_open_start(
            true, true, true, true, false, false, false
        ));
    }

    #[test]
    fn skips_when_disabled_empty_open_cooldown_or_fullscreen() {
        assert!(!should_auto_open_start(
            false, true, true, true, false, false, false
        ));
        assert!(!should_auto_open_start(
            true, false, true, true, false, false, false
        ));
        assert!(!should_auto_open_start(
            true, true, false, true, false, false, false
        ));
        assert!(!should_auto_open_start(
            true, true, true, true, true, false, false
        ));
        assert!(!should_auto_open_start(
            true, true, true, true, false, true, false
        ));
        assert!(!should_auto_open_start(
            true, true, true, true, false, false, true
        ));
    }
}
