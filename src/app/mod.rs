//! iced `daemon` shell for the DualSense battery tray app.
//!
//! The daemon boots windowless: it owns the tray icon and only opens windows on
//! demand (controller popup, configure window, overlay toast). The toast window
//! is pre-created hidden after the tray appears so iced keeps a warm GPU
//! compositor; popup and Settings stay ephemeral (create/destroy on each open).

use crate::controller::dualsense::identity as dualsense;
use crate::controller::dualsense::lightbar::{
    self, LOW_BATTERY_ORANGE, LOW_BATTERY_PULSE_GAP_MS, LOW_BATTERY_PULSE_ON_MS,
};
#[cfg(feature = "dev-emulate")]
use crate::controller::emulate::{self, Preset};
use crate::controller::hid::poll::{
    BATTERY_INTERVAL, LIVENESS_INTERVAL, PRESENCE_INTERVAL, PRESENCE_INTERVAL_EMPTY,
    UNREAD_RETRY_INTERVAL,
};
use crate::controller::hid::worker::HidWorkerHandle;
use crate::controller::known::KnownControllers;
use crate::controller::model::{Connection, ControllerStatus};
use crate::games::launch;
use crate::games::macro_lib::{self, ImportBind, MacroLibrary};
use crate::games::macro_run;
use crate::games::macro_text::{self, GameRef};
use crate::games::process_match::{self, RunningSession};
use crate::games::steam::{self, SteamGame};
use crate::games::{self, GamesCatalog};
use crate::persist::analytics::{self, AnalyticsStore};
use crate::persist::notify::{NotifyEvent, NotifyTracker};
use crate::persist::paths;
use crate::persist::prefs::{Prefs, clamp_low_battery_percent, clamp_start_screen_sound_volume};
use crate::platform::app_log;
#[cfg(windows)]
use crate::platform::autostart;
use crate::platform::crash_restart;
use crate::platform::ui_sound::{self, UiSoundKind};
#[cfg(windows)]
use crate::platform::win32;
use crate::ui::color::{self, BatterySpectrum, color_for_battery_percent};
use crate::ui::configure::{
    self as configure_view, AnalyticsPadRow, AnalyticsPanel, ConfigureMessage, ConfigureSettings,
    ConfigureState, NotificationSetting, PadInputPanel, Section,
};
use crate::ui::layout::{
    TOAST_SLIDE_DURATION, ToastPlacement, TrayAnchor, hide_toast, invalidate_toast,
    overlay_platform_specific, popup_position, raise_window_topmost, remount_toast_surface,
    set_toast_topmost, show_toast_without_activate, slide_y, toast_placement,
    window_platform_specific,
};
use crate::ui::popup::{self as popup_view, ControllerRow, PopupMessage};
use crate::ui::start::gesture::{self, GestureRecorder};
use crate::ui::start::input::{
    self as start_input, CrossHold, FaceHeld, GestureDetectorBank, GestureRecordLatch, NavAction,
    NavLogSnapshot, NavSource, PadNavBank,
};
use crate::ui::start::view::{
    self as start_view, MacroListRow, MacroOverlay, ReplaceConfirm, StartMessage, StartSlide,
};
use crate::ui::theme;
use crate::ui::toast::ToastMessage;
use crate::ui::toast::view as toast_view;
use crate::ui::tray::{self, QUIT_ID, SETTINGS_ID};

use iced::futures::Stream;
use iced::futures::StreamExt;
use iced::futures::channel::{mpsc, oneshot};
use iced::keyboard;
use iced::widget::{container, operation, space};
use iced::{Element, Point, Size, Subscription, Task, Theme, clipboard, stream, window};

use std::collections::{HashMap, HashSet, VecDeque};
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
/// DualSense poll rate while pad input is live (~one wired report).
const PAD_POLL_ACTIVE: Duration = Duration::from_millis(4);
/// UI animation tick (~60Hz). Prefer this over `window::frames()` so an
/// AlwaysOnTop toast cannot starve other iced windows on Windows (iced#3108).
const UI_TICK: Duration = Duration::from_millis(16);
/// Process/catalog running checks are expensive; never run them at pad-poll rate.
const RUNNING_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const PAD_POLL_STALL_MS: u128 = 80;
const PAD_POLL_SLOW_MS: u128 = 20;
const SNAPSHOT_STALE_MS: u128 = 100;
const START_NAV_NOT_READY_MS: u128 = 200;

#[derive(Debug, Clone)]
pub enum Message {
    /// Periodic presence scan.
    Tick,
    /// Result of a background `poll_controllers` call.
    PollResult(Result<Vec<ControllerStatus>, String>),
    /// Result of a background process-image snapshot (running badge).
    ProcessEnumResult(Result<Vec<(u32, PathBuf)>, String>),

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
    /// Drive start-screen slide / Triangle-hold animation frames.
    StartFrame,
    Start(StartMessage),
    /// Keyboard while some iced window has focus; filtered to the start screen in update.
    StartKey {
        id: window::Id,
        action: StartKeyAction,
        pressed: bool,
    },
    /// Periodic DualSense sample for gestures / start-screen navigation.
    PadPoll,
    ManualFilePicked(Option<PathBuf>),
    ManualIconPicked(Option<PathBuf>),
    SteamScanDone(Result<Vec<SteamGame>, String>),

    /// Macro focus completed; close start then run key steps.
    MacroFocusDone {
        name: String,
        paths: Vec<PathBuf>,
        steps: Vec<macro_text::Step>,
        result: Result<(), String>,
    },
    MacroRunDone {
        name: String,
        result: Result<(), String>,
    },

    PlaceToast {
        id: window::Id,
        monitor: Option<Size>,
        generation: u64,
    },
    /// Advance the active toast slide animation.
    ToastFrame,
    /// Redraw the controller popup while an Identify ring flash is running.
    IdentifyFrame,
    /// Begin dismiss (slide-out) for the toast of the given generation.
    ToastDismiss(u64),

    /// Debounced spectrum prefs save + lightbar HID apply.
    SpectrumCommit(u64),

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
    /// Per-game macro library (`macros.json`).
    macros: MacroLibrary,
    /// In-progress start-screen edit checklist; committed on Save, discarded on Cancel.
    edit_draft: Option<GamesCatalog>,

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
    /// Serializes DualSense HID for the daemon (never on the UI thread).
    hid_worker: HidWorkerHandle,

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
    pad_input_panel: PadInputPanel,

    start_window: Option<window::Id>,
    /// False until StartOpened shows the window — blocks pad nav / sounds early.
    start_nav_ready: bool,
    start_state: start_view::State,
    /// True while an rfd picker is open from the start screen (suppress unfocus-close).
    start_file_dialog_open: bool,
    /// After last pad disconnect, suppress 0→1 auto-open briefly.
    start_connect_cooldown_until: Option<Instant>,
    gesture_detectors: GestureDetectorBank,
    gesture_recorder: GestureRecorder,
    /// Latch Settings gesture recording to one pad (no cross-pad union).
    gesture_record_latch: GestureRecordLatch,
    pad_nav: PadNavBank,
    /// Keyboard Enter hold for replace-confirm (not folded into pad slots).
    keyboard_cross_hold: CrossHold,
    /// Keyboard Enter held (for hold-to-proceed on replace confirm / Cross press styling).
    confirm_key_held: bool,
    /// Keyboard Escape held (Circle press styling).
    cancel_key_held: bool,
    /// Last armed-pad face held snapshot (merged with keyboard in [`Self::sync_start_held`]).
    pad_held: FaceHeld,
    running_session: Option<RunningSession>,
    /// Throttle for process enumeration / catalog restore (not pad UI).
    last_running_check: Option<Instant>,
    /// True while a background process enum task is outstanding.
    process_enum_inflight: bool,
    /// Cached `match_paths_for_target` results (cleared on Steam library refresh).
    match_path_cache: HashMap<String, Vec<PathBuf>>,
    steam_by_id: HashMap<u32, SteamGame>,
    /// `Some` after a successful Steam library scan (installed appids). `None` = unknown.
    steam_installed: Option<Vec<u32>>,
    hid_exclusive_warned: bool,
    /// One-shot when both Gaming.Input and DualSense HID fail while start is open.
    nav_missing_warned: bool,
    /// One-shot while every live pad is unarmed (waiting for rest).
    nav_unarmed_warned: bool,
    /// Logged once per start-session which nav backend succeeded.
    nav_source_logged: Option<NavSource>,
    /// Last edge-logged nav snapshot (cleared when start closes).
    last_nav_log: Option<NavLogSnapshot>,
    /// Last PadPoll Instant while start window open (stall detection).
    last_pad_poll_at: Option<Instant>,
    /// One-shot while snapshot is empty (tracks gap start).
    nav_missing_since: Option<Instant>,
    /// One-shot while snapshot is non-empty but stale.
    nav_stale_warned: bool,
    /// One-shot when start_nav_ready stays false after open.
    nav_not_ready_warned: bool,
    /// When the start window was opened (for not-ready timing).
    start_opened_at: Option<Instant>,

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
        let macros = MacroLibrary::load();
        color::set_active_spectrum(prefs.spectrum.clone());
        lightbar::set_enabled(prefs.lightbar_enabled);

        #[cfg(windows)]
        autostart::ensure_quiet_entry();

        let hid_worker = HidWorkerHandle::start();
        let identifying = hid_worker.identifying();
        let low_battery = Arc::new(Mutex::new(Vec::new()));
        start_low_battery_pulse_thread(hid_worker.clone(), Arc::clone(&low_battery));

        let configure_state = ConfigureState::new(prefs.spectrum.clone());

        let last_discovered = hid_worker.presence_paths();
        let mut app = Self {
            prefs,
            known,
            notify: NotifyTracker::new(),
            analytics,
            games,
            macros,
            edit_draft: None,
            controllers: Vec::new(),
            last_discovered,
            last_battery_poll: Instant::now(),
            identifying,
            refreshing: Arc::new(AtomicBool::new(false)),
            low_battery,
            hid_worker,
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
            pad_input_panel: PadInputPanel::default(),
            start_window: None,
            start_nav_ready: false,
            start_state: start_view::State::default(),
            start_file_dialog_open: false,
            start_connect_cooldown_until: None,
            gesture_detectors: GestureDetectorBank::default(),
            gesture_recorder: GestureRecorder::default(),
            gesture_record_latch: GestureRecordLatch::default(),
            pad_nav: PadNavBank::default(),
            keyboard_cross_hold: CrossHold::default(),
            confirm_key_held: false,
            cancel_key_held: false,
            pad_held: FaceHeld::default(),
            running_session: None,
            last_running_check: None,
            process_enum_inflight: false,
            match_path_cache: HashMap::new(),
            steam_by_id: HashMap::new(),
            steam_installed: None,
            hid_exclusive_warned: false,
            nav_missing_warned: false,
            nav_unarmed_warned: false,
            nav_source_logged: None,
            last_nav_log: None,
            last_pad_poll_at: None,
            nav_missing_since: None,
            nav_stale_warned: false,
            nav_not_ready_warned: false,
            start_opened_at: None,
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

        // Show the tray immediately, then poll in the background so a stuck HID
        // read cannot delay the icon for tens of seconds. Pre-create the toast
        // window hidden so iced keeps a warm GPU compositor for fast popup/Settings opens.
        app.create_tray();
        #[cfg(all(windows, debug_assertions))]
        start_hitch_hotkey_worker();
        app.sync_popup_rows();
        app.sync_low_battery();
        app.refresh_analytics_panel();

        let toast_boot = app
            .ensure_toast_window()
            .chain(app.maybe_show_crash_restart_toast());
        let task = Task::batch([
            app.request_refresh(),
            toast_boot,
            app.refresh_steam_library(),
        ]);
        (app, task)
    }

    fn title(&self, window: window::Id) -> String {
        if Some(window) == self.configure_window {
            "Settings".to_string()
        } else if Some(window) == self.start_window {
            String::new()
        } else {
            crate::platform::app_meta::DISPLAY_NAME.to_string()
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
                    Some(Message::StartKey {
                        id,
                        action,
                        pressed: true,
                    })
                }
                iced::Event::Keyboard(keyboard::Event::KeyReleased { key, .. }) => {
                    let action = match key {
                        keyboard::Key::Named(keyboard::key::Named::Escape) => {
                            Some(StartKeyAction::Cancel)
                        }
                        keyboard::Key::Named(keyboard::key::Named::Enter) => {
                            Some(StartKeyAction::Confirm)
                        }
                        _ => None,
                    }?;
                    Some(Message::StartKey {
                        id,
                        action,
                        pressed: false,
                    })
                }
                _ => None,
            }),
            iced::time::every(
                if self.prefs.start_screen_enabled && self.controllers.is_empty() {
                    PRESENCE_INTERVAL_EMPTY
                } else {
                    PRESENCE_INTERVAL
                },
            )
            .map(|_| Message::Tick),
            Subscription::run(tray_events_mapped),
        ];

        if self.toast_message.is_some() {
            subscriptions.push(Subscription::run_with(
                self.toast_generation,
                |generation| escape_hotkey(*generation),
            ));
        }

        if self.toast_message.is_some() {
            // Tick for the whole toast lifetime so content swaps present, not only
            // during the slide animation.
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::ToastFrame));
        }

        if self.popup_state.identify_flash_active() || self.start_state.identify_flash_active() {
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::IdentifyFrame));
        }

        let pad_input_live =
            self.configure_window.is_some() && self.configure_state.section == Section::PadInput;
        let pad_listening = pad_input_live
            || (self.prefs.start_screen_enabled
                && (!self.controllers.is_empty() || self.gesture_recorder.is_active()));
        // Fast path whenever we are listening — reopen gesture needs the same cadence.
        self.hid_worker.set_input_hot(pad_listening);
        if pad_listening {
            subscriptions.push(iced::time::every(PAD_POLL_ACTIVE).map(|_| Message::PadPoll));
        }

        if self.start_window.is_some() && self.start_state.needs_frames() {
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::StartFrame));
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
                &self.pad_input_panel,
            )
            .map(Message::Configure);
        }

        if Some(window) == self.start_window {
            return start_view::view(&self.start_state, &self.prefs.spectrum, Instant::now())
                .map(Message::Start);
        }

        if Some(window) == self.toast_window {
            return match self.toast_message.as_ref() {
                Some(message) => toast_view::view(
                    message,
                    self.toast_generation,
                    Message::ToastDismiss(self.toast_generation),
                ),
                None => toast_view::empty(),
            };
        }

        container(space()).style(theme::root).into()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => self.on_tick(),
            Message::PollResult(result) => self.on_poll_result(result),
            Message::ProcessEnumResult(result) => self.on_process_enum_result(result),

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
                    self.sync_toast_zorder()
                } else if Some(id) == self.configure_window {
                    self.configure_window = None;
                    self.cancel_gesture_recording();
                    self.sync_toast_zorder()
                } else if Some(id) == self.start_window {
                    self.start_window = None;
                    self.start_nav_ready = false;
                    self.pad_nav.reset();
                    self.clear_start_nav_diag();
                    self.sync_toast_zorder()
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
                    // File dialogs (add/edit shortcut, choose image) steal focus — keep the
                    // start screen open so the modal / draft is not discarded.
                    if self.start_state.manual_add.is_some()
                        || self.start_state.macro_overlay.is_some()
                        || self.start_file_dialog_open
                    {
                        Task::none()
                    } else {
                        self.close_start_screen()
                    }
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
                self.start_nav_ready = true;
                self.nav_not_ready_warned = false;
                window::set_mode(id, window::Mode::Windowed).chain(window::gain_focus(id))
            }
            Message::StartFrame => {
                let now = Instant::now();
                let _ = self.start_state.tick_anim(now);
                // Keyboard-only hold progress when pad poll is not running.
                if self.start_state.replace_confirm.is_some() {
                    let (progress, completed) =
                        self.keyboard_cross_hold.update(self.confirm_key_held, now);
                    self.start_state.cross_progress = self.start_state.cross_progress.max(progress);
                    if completed {
                        self.play_start_sound(UiSoundKind::Hold);
                        return self.complete_replace_confirm();
                    }
                }
                self.sync_start_held();
                self.start_state.tick_hint_anims(now);
                Task::none()
            }
            Message::Start(message) => self.on_start_message(message),
            Message::StartKey {
                id,
                action,
                pressed,
            } => {
                if Some(id) == self.start_window {
                    // Leftover-chord arming is pad-local; keyboard stays live.
                    return match action {
                        StartKeyAction::Up if pressed => {
                            self.cancel_start_holds();
                            self.on_start_message(StartMessage::MoveUp)
                        }
                        StartKeyAction::Down if pressed => {
                            self.cancel_start_holds();
                            self.on_start_message(StartMessage::MoveDown)
                        }
                        StartKeyAction::Confirm => {
                            self.confirm_key_held = pressed;
                            self.sync_start_held();
                            if self.start_state.replace_confirm.is_some() {
                                if pressed {
                                    self.pad_nav.cancel_holds();
                                }
                                Task::none()
                            } else if !pressed {
                                self.cancel_start_holds();
                                self.on_start_message(StartMessage::Confirm)
                            } else {
                                Task::none()
                            }
                        }
                        StartKeyAction::Cancel => {
                            self.cancel_key_held = pressed;
                            self.sync_start_held();
                            if !pressed {
                                self.cancel_start_holds();
                                self.on_start_message(StartMessage::Close)
                            } else {
                                Task::none()
                            }
                        }
                        _ => Task::none(),
                    };
                }
                if Some(id) == self.configure_window
                    && action == StartKeyAction::Cancel
                    && pressed
                    && self.gesture_recorder.is_active()
                {
                    self.cancel_gesture_recording();
                }
                Task::none()
            }
            Message::PadPoll => self.on_pad_poll(),
            Message::ManualFilePicked(path) => self.on_manual_file_picked(path),
            Message::ManualIconPicked(path) => self.on_manual_icon_picked(path),
            Message::SteamScanDone(result) => self.on_steam_scan_done(result),

            Message::MacroFocusDone {
                name,
                paths,
                steps,
                result,
            } => self.on_macro_focus_done(name, paths, steps, result),
            Message::MacroRunDone { name, result } => {
                match result {
                    Ok(()) => app_log::info(format!("macro: finished name={name}")),
                    Err(err) => app_log::warn(format!("macro: failed name={name} err={err}")),
                }
                Task::none()
            }

            Message::PlaceToast {
                id,
                monitor,
                generation,
            } => {
                if generation != self.toast_generation || self.toast_message.is_none() {
                    return Task::none();
                }
                let placement = toast_placement(self.prefs.toast_position, monitor);
                self.toast_placement = Some(placement);
                self.toast_anim_started = Instant::now();
                self.toast_dismissing = false;
                let start = Point::new(placement.x, placement.outside_y);
                let percent = self
                    .toast_message
                    .as_ref()
                    .map(|m| m.percent())
                    .unwrap_or(0);
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: place toast gen={generation} percent={percent}"
                ));
                // Toast stays TOPMOST (above Cursor). Raise Start/Settings above it
                // in the same band so they keep presents (iced#3320); toast at the
                // screen edge stays visible beside the centered Start window.
                remount_toast_surface(id, generation)
                    .chain(window::move_to(id, start))
                    .chain(window::set_level(id, window::Level::AlwaysOnTop))
                    .chain(show_toast_without_activate(id))
                    .chain(invalidate_toast(id))
                    .chain(self.raise_interactive_ui_above_toast(true))
            }
            Message::ToastFrame => {
                let anim = if self.toast_animating() {
                    self.animate_toast()
                } else {
                    Task::none()
                };
                // Re-pin UI above toast so Start/Settings keep presenting while
                // the toast animates (do not raise toast above UI every tick).
                anim.chain(self.raise_interactive_ui_above_toast(false))
            }
            Message::IdentifyFrame => {
                self.popup_state.tick_identify_flash();
                self.start_state.tick_identify_flash();
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

            Message::Exit => {
                self.known.save();
                self.hid_worker.shutdown();
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

        let discovered = self.hid_worker.presence_paths();

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
        // While the tray is empty, retry on the fast presence cadence so 0→1 open is snappy.
        let unread_retry = !self.last_discovered.is_empty()
            && self.controllers.is_empty()
            && self.last_battery_poll.elapsed()
                >= if self.prefs.start_screen_enabled {
                    PRESENCE_INTERVAL_EMPTY
                } else {
                    UNREAD_RETRY_INTERVAL
                };

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
        let worker = self.hid_worker.clone();
        Task::perform(worker.poll(previously_connected), Message::PollResult)
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
        let notify = tray.chain(self.queue_notifications(events));
        // Batch with toast work: show_next_toast's Task includes the ~5s expire
        // delay, so chaining open_start_screen after it deferred Start until the
        // toast finished.
        let start = if self.controllers.is_empty() && self.start_window.is_some() {
            self.close_start_screen()
        } else if opened_from_empty && self.should_auto_open_start() {
            self.open_start_screen()
        } else {
            Task::none()
        };
        Task::batch([notify, start])
    }

    fn should_auto_open_start(&self) -> bool {
        let cooldown_active = self
            .start_connect_cooldown_until
            .is_some_and(|until| Instant::now() < until);
        should_auto_open_start(
            self.prefs.start_screen_enabled,
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
        let before = self.popup_window_height();
        self.sync_popup_rows();
        let after = self.popup_window_height();
        if before == after {
            Task::none()
        } else {
            self.fit_popup_window()
        }
    }

    fn popup_window_height(&self) -> f32 {
        popup_view::window_height(self.popup_rows.len(), self.popup_monitor.map(|m| m.height))
    }

    fn fit_popup_window(&self) -> Task<Message> {
        let Some(id) = self.popup_window else {
            return Task::none();
        };
        let height = self.popup_window_height();
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
        let height = self.popup_window_height();
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
            .chain(self.sync_toast_zorder())
    }

    fn reveal_popup(&self, id: window::Id) -> Task<Message> {
        let height = self.popup_window_height();
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
                    self.start_state.begin_identify_flash(&serial);
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
            start_screen_sounds_enabled: self.prefs.start_screen_sounds_enabled,
            start_screen_sound_volume: self.prefs.start_screen_sound_volume,
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
            // AlwaysOnTop like Start: a Normal Settings window loses presents to the
            // toast on Windows even when the toast is demoted (iced#3320).
            level: window::Level::AlwaysOnTop,
            exit_on_close_request: true,
            platform_specific: window_platform_specific(),
            ..window::Settings::default()
        });

        self.configure_window = Some(id);
        open.map(Message::ConfigureOpened)
            .chain(self.sync_toast_zorder())
    }

    fn on_configure_message(&mut self, message: ConfigureMessage) -> Task<Message> {
        match message {
            ConfigureMessage::SelectSection(section) => {
                if self.configure_state.section == Section::System && section != Section::System {
                    self.cancel_gesture_recording();
                }
                self.configure_state.select_section(section);
                if section == Section::PadInput {
                    match start_input::read_nav_readings() {
                        start_input::NavReadingsOutcome::Readings { readings, .. } => {
                            self.apply_pad_input_panel_from_readings(&readings);
                        }
                        start_input::NavReadingsOutcome::Missing { .. } => {
                            self.apply_pad_input_panel_from_readings(&[]);
                        }
                    }
                    return Task::none();
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
                if !enabled {
                    self.cancel_gesture_recording();
                    if self.start_window.is_some() {
                        return self.close_start_screen();
                    }
                }
                Task::none()
            }
            ConfigureMessage::SetStartScreenSounds(enabled) => {
                self.prefs.start_screen_sounds_enabled = enabled;
                self.prefs.save();
                if enabled {
                    self.play_start_sound(UiSoundKind::Nav);
                }
                Task::none()
            }
            ConfigureMessage::SetStartScreenSoundVolume(volume) => {
                let volume = clamp_start_screen_sound_volume(volume);
                if self.prefs.start_screen_sound_volume != volume {
                    self.prefs.start_screen_sound_volume = volume;
                    self.prefs.save();
                    if self.prefs.start_screen_sounds_enabled {
                        self.play_start_sound(UiSoundKind::Nav);
                    }
                }
                Task::none()
            }
            ConfigureMessage::StartGestureRecord => {
                self.gesture_recorder.start();
                self.gesture_record_latch.clear();
                self.gesture_detectors.reset();
                Task::none()
            }
            ConfigureMessage::ResetStartGesture => {
                self.cancel_gesture_recording();
                self.prefs.start_screen_gesture = gesture::default_gesture();
                self.prefs.save();
                // Clear detectors so a held default chord cannot reopen immediately.
                self.gesture_detectors.reset();
                Task::none()
            }
            ConfigureMessage::CancelGestureRecord => {
                self.cancel_gesture_recording();
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
            ConfigureMessage::RemoveStopAt(index) => {
                let next = self.configure_state.remove_at(index);
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

        for (serial, color) in targets {
            self.hid_worker.set_rgb(serial, color);
        }
        Task::none()
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
        if !controller.supports_lightbar {
            return false;
        }

        self.hid_worker
            .identify(serial.to_string(), controller.percent)
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
        if !controller.supports_power_off || !controller.connection.is_bluetooth() {
            app_log::warn(format!(
                "power-off ignored for {serial} ({})",
                controller.connection
            ));
            return;
        }

        self.hid_worker.power_off(serial.to_string());
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
            && !self
                .controllers
                .iter()
                .any(|c| c.serial == emulate::PRIMARY_SERIAL && c.state == PowerState::Complete);

        let next = if preset == Preset::AnalyticsResume {
            let percent = self
                .dev_paused_percent
                .or_else(|| {
                    self.analytics
                        .in_progress(emulate::PRIMARY_SERIAL)
                        .map(|p| p.percent)
                })
                .unwrap_or(60);
            vec![crate::controller::dualsense::battery::dualsense_status(
                1,
                "DualSense",
                Connection::Bluetooth,
                emulate::PRIMARY_SERIAL.to_string(),
                percent,
                PowerState::Discharging,
            )]
        } else {
            emulate::apply_preset(preset, &self.controllers)
        };

        self.emulating = true;
        self.analytics.save();
        self.refresh_analytics_panel();

        if ensure_complete {
            let complete = vec![crate::controller::dualsense::battery::dualsense_status(
                1,
                "DualSense",
                Connection::Usb,
                emulate::PRIMARY_SERIAL.to_string(),
                100,
                PowerState::Complete,
            )];
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
        let steam_by_id = &self.steam_by_id;
        self.games.merge_sorted(
            self.steam_installed.as_deref(),
            self.prefs.games_sort_mode,
            |entry| match entry {
                crate::games::GameEntry::Steam { appid } => steam_by_id
                    .get(appid)
                    .map(|g| g.name.clone())
                    .unwrap_or_else(|| format!("Steam {appid}")),
                crate::games::GameEntry::Manual { title, .. } => title.clone(),
            },
        )
    }

    fn refresh_start_rows(&mut self) {
        self.start_state.sort_mode = self.prefs.games_sort_mode;
        let mut rows = if self.start_state.editing {
            self.edit_checklist_rows()
        } else {
            self.display_catalog()
                .iter()
                .map(|entry| start_view::StartRow::from_entry(entry, &self.steam_by_id))
                .collect()
        };
        for row in &mut rows {
            row.has_macros = self.macros.has_macros(&row.play_key);
        }
        self.start_state.set_rows(rows);
    }

    fn edit_catalog(&self) -> &GamesCatalog {
        self.edit_draft.as_ref().unwrap_or(&self.games)
    }

    fn edit_catalog_mut(&mut self) -> &mut GamesCatalog {
        self.edit_draft.as_mut().unwrap_or(&mut self.games)
    }

    fn edit_checklist_rows(&self) -> Vec<start_view::StartRow> {
        let catalog = self.edit_catalog();
        let mut rows: Vec<_> = self
            .steam_by_id
            .values()
            .map(|game| start_view::StartRow::steam_edit(game, catalog.contains_steam(game.appid)))
            .collect();
        for entry in &catalog.entries {
            if let Some(row) = start_view::StartRow::manual_edit(entry) {
                rows.push(row);
            }
        }
        rows.sort_by_key(|a| a.title.to_lowercase());
        rows
    }

    fn discard_edit_draft(&mut self) {
        self.edit_draft = None;
        self.start_state.editing = false;
        self.start_state.edit_anchor_play_key = None;
        self.start_state.manual_add = None;
        self.start_state.macro_overlay = None;
    }

    fn enter_start_edit(&mut self) -> Task<Message> {
        self.start_state.capture_edit_anchor();
        self.edit_draft = Some(self.games.clone());
        self.start_state.editing = true;
        self.refresh_start_rows();
        let key = self.start_state.edit_anchor_play_key.clone();
        self.start_state.select_game_by_play_key(key.as_deref());
        self.scroll_start_selection_to_center(true)
    }

    fn commit_start_edit(&mut self) -> Task<Message> {
        if let Some(draft) = self.edit_draft.take() {
            let keep: HashSet<String> = draft.entries.iter().map(|e| e.play_key()).collect();
            let orphaned: Vec<String> = self
                .macros
                .games
                .keys()
                .filter(|k| !keep.contains(*k))
                .cloned()
                .collect();
            let mut dirty = false;
            for key in orphaned {
                if self.macros.remove_game(&key) {
                    dirty = true;
                }
            }
            if dirty {
                self.macros.save();
            }
            self.games = draft;
            self.games.save();
        }
        self.start_state.editing = false;
        self.start_state.macro_overlay = None;
        self.refresh_start_rows();
        self.start_state.restore_edit_anchor();
        self.scroll_start_selection_to_center(false)
    }

    fn cancel_start_edit(&mut self) -> Task<Message> {
        let key = self.start_state.edit_anchor_play_key.take();
        self.discard_edit_draft();
        self.refresh_start_rows();
        self.start_state.select_game_by_play_key(key.as_deref());
        self.scroll_start_selection_to_center(true)
    }

    fn refresh_start_controllers(&mut self) {
        self.start_state.show_all_controllers = self.prefs.show_all_controllers;
        let mut rows: Vec<_> = self
            .controllers
            .iter()
            .map(|c| {
                let nickname = self.known.nickname(&c.serial);
                let eta = if self.prefs.analytics_enabled {
                    self.analytics.eta_for(c).map(analytics::format_eta_ring)
                } else {
                    None
                };
                start_view::StartControllerRow {
                    serial: c.serial.clone(),
                    title: start_view::controller_title(c.product, nickname),
                    connection: c.connection.to_string(),
                    state: if c.is_low_battery(self.prefs.low_battery_percent) {
                        "low battery".into()
                    } else {
                        start_view::power_state_label(c.state).into()
                    },
                    percent: c.percent,
                    low: c.is_low_battery(self.prefs.low_battery_percent),
                    bluetooth: c.connection.is_bluetooth() && c.supports_power_off,
                    connected: true,
                    eta,
                }
            })
            .collect();
        if self.prefs.show_all_controllers {
            for controller in self.known.remembered_disconnected(&self.controllers) {
                let nickname = self.known.nickname(&controller.serial);
                let eta = if self.prefs.analytics_enabled {
                    self.analytics
                        .eta_play_at(&controller.serial, controller.percent)
                        .map(analytics::format_eta_ring)
                } else {
                    None
                };
                rows.push(start_view::StartControllerRow {
                    serial: controller.serial.clone(),
                    title: start_view::controller_title(&controller.product, nickname),
                    connection: controller.connection.clone(),
                    state: "disconnected".into(),
                    percent: controller.percent,
                    low: false,
                    bluetooth: false,
                    connected: false,
                    eta,
                });
            }
        }
        self.start_state.set_controllers(rows);
    }

    fn refresh_running_badge(&mut self) -> Task<Message> {
        self.refresh_running_badge_inner(false)
    }

    fn refresh_running_badge_inner(&mut self, _force: bool) -> Task<Message> {
        let now = Instant::now();

        // Optimistic UI during Steam/game start — no process enumeration.
        if let Some(session) = self.running_session.as_ref()
            && now.duration_since(session.launched_at) < process_match::LAUNCH_GRACE
        {
            self.start_state.running_target = Some(session.target.clone());
            return Task::none();
        }

        if self.process_enum_inflight {
            return Task::none();
        }

        if self
            .last_running_check
            .is_some_and(|t| now.duration_since(t) < RUNNING_CHECK_INTERVAL)
        {
            return Task::none();
        }
        self.last_running_check = Some(now);
        self.process_enum_inflight = true;

        Task::perform(
            spawn_blocking(process_match::list_process_images),
            Message::ProcessEnumResult,
        )
    }

    fn on_process_enum_result(
        &mut self,
        result: Result<Vec<(u32, PathBuf)>, String>,
    ) -> Task<Message> {
        self.process_enum_inflight = false;
        let images = match result {
            Ok(images) => images,
            Err(err) => {
                app_log::warn(format!("process enum failed: {err}"));
                return Task::none();
            }
        };
        self.apply_running_badge_from_images(&images);
        Task::none()
    }

    fn apply_running_badge_from_images(&mut self, images: &[(u32, PathBuf)]) {
        let now = Instant::now();

        if let Some(session) = self.running_session.as_mut() {
            if process_match::any_matching_in_images(&session.match_paths, images) {
                session.miss_since = None;
                self.start_state.running_target = Some(session.target.clone());
                return;
            }

            match session.miss_since {
                None => {
                    session.miss_since = Some(now);
                    self.start_state.running_target = Some(session.target.clone());
                    return;
                }
                Some(since) if now.duration_since(since) < process_match::MISS_CLEAR => {
                    self.start_state.running_target = Some(session.target.clone());
                    return;
                }
                Some(_) => {
                    let target = session.target.clone();
                    app_log::info(format!("running session cleared (process miss): {target}"));
                    self.running_session = None;
                    self.start_state.running_target = None;
                }
            }
        } else {
            self.start_state.running_target = None;
        }

        self.try_restore_running_from_catalog(images);
    }

    fn cached_match_paths(&mut self, target: &str) -> Vec<PathBuf> {
        if let Some(paths) = self.match_path_cache.get(target) {
            return paths.clone();
        }
        let paths = process_match::match_paths_for_target(target);
        self.match_path_cache
            .insert(target.to_string(), paths.clone());
        paths
    }

    /// Scan catalog targets for a live process (single process snapshot).
    fn try_restore_running_from_catalog(&mut self, images: &[(u32, PathBuf)]) {
        let candidates: Vec<(String, String)> = self
            .start_state
            .rows
            .iter()
            .map(|r| (r.title.clone(), r.target.clone()))
            .collect();
        if candidates.is_empty() {
            return;
        }

        let path_sets: Vec<Vec<PathBuf>> = candidates
            .iter()
            .map(|(_, target)| self.cached_match_paths(target))
            .collect();

        let Some(idx) = process_match::first_matching_index_in_images(&path_sets, images) else {
            return;
        };
        let (title, target) = candidates[idx].clone();
        if self
            .running_session
            .as_ref()
            .is_some_and(|s| s.matches_target(&target))
        {
            self.start_state.running_target = Some(target);
            return;
        }
        let match_paths = path_sets[idx].clone();
        app_log::info(format!("running session restored: {target}"));
        self.running_session = Some(RunningSession::restored(title, target.clone(), match_paths));
        self.start_state.running_target = Some(target);
    }

    fn begin_running_session(&mut self, title: String, target: String) {
        let match_paths = self.cached_match_paths(&target);
        if match_paths.is_empty() {
            app_log::warn(format!(
                "running session: empty match_paths for {target} (badge may clear after grace)"
            ));
        }
        self.running_session = Some(RunningSession::new(title, target.clone(), match_paths));
        self.start_state.running_target = Some(target);
    }

    fn touch_last_played(&mut self, play_key: &str) {
        self.games.touch_played_key(play_key);
        self.games.save();
    }

    fn close_running_game(&mut self) {
        let Some(session) = self.running_session.clone() else {
            return;
        };
        if let Err(err) = process_match::close_matching(&session.match_paths) {
            app_log::warn(format!("close game failed for {}: {err}", session.target));
        }
        self.running_session = None;
        self.start_state.running_target = None;
    }

    /// Hold-Cross close: only when the selected row is the running game.
    fn close_running_game_if_selected(&mut self) {
        let Some(session) = self.running_session.as_ref() else {
            return;
        };
        let Some(row) = self.start_state.rows.get(self.start_state.game_selected) else {
            return;
        };
        if !session.matches_target(&row.target) {
            return;
        }
        self.close_running_game();
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
            }
            Err(err) => {
                steam::warn_scan_error(&err);
                // Keep prior successful install list if any; otherwise stay Unknown so
                // curated Steam rows remain visible.
            }
        }
        self.match_path_cache.clear();
        self.refresh_start_rows();
        Task::none()
    }

    fn on_manual_file_picked(&mut self, path: Option<PathBuf>) -> Task<Message> {
        self.start_file_dialog_open = false;
        let Some(path) = path else {
            return Task::none();
        };
        if !self.start_state.editing {
            return Task::none();
        }
        let target = path.to_string_lossy().into_owned();
        let title = games::title_from_target(&target);
        let shell_icon = start_view::probe_shell_icon(&target);
        self.start_state.manual_add = Some(start_view::ManualAddDraft {
            edit_id: None,
            target,
            title,
            args: String::new(),
            icon_path: None,
            shell_icon,
        });
        Task::none()
    }

    fn on_manual_icon_picked(&mut self, path: Option<PathBuf>) -> Task<Message> {
        self.start_file_dialog_open = false;
        let Some(draft) = self.start_state.manual_add.as_mut() else {
            return Task::none();
        };
        if let Some(path) = path {
            draft.icon_path = Some(path);
        }
        Task::none()
    }

    fn confirm_manual_add(&mut self) -> Task<Message> {
        let Some(draft) = self.start_state.manual_add.take() else {
            return Task::none();
        };
        let title = draft.title.trim();
        let title = if title.is_empty() {
            games::title_from_target(&draft.target)
        } else {
            title.to_string()
        };
        let icon = draft
            .icon_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let args = draft.args.trim().to_string();
        if let Some(id) = draft.edit_id.as_deref() {
            self.edit_catalog_mut().update_manual(id, title, args, icon);
        } else {
            self.edit_catalog_mut()
                .add_manual(title, draft.target, args, icon);
        }
        self.refresh_start_rows();
        self.scroll_start_selection_into_view()
    }

    fn cancel_manual_add(&mut self) -> Task<Message> {
        self.start_state.manual_add = None;
        Task::none()
    }

    fn cancel_gesture_recording(&mut self) {
        self.gesture_recorder.cancel();
        self.gesture_record_latch.clear();
    }

    /// True when HID/UI input sampling should run at the active (~report-rate) rate.
    #[allow(dead_code)] // kept for callers / diagnostics that still want the hot predicate
    fn pad_input_hot(&self) -> bool {
        self.start_window.is_some()
            || self.gesture_recorder.is_active()
            || (self.configure_window.is_some()
                && self.configure_state.section == Section::PadInput)
            || (self.prefs.start_screen_enabled && !self.controllers.is_empty())
    }

    fn open_start_screen(&mut self) -> Task<Message> {
        if !self.prefs.start_screen_enabled {
            return Task::none();
        }
        self.start_state.editing = false;
        self.start_state.edit_anchor_play_key = None;
        self.edit_draft = None;
        self.refresh_start_rows();
        self.refresh_start_controllers();

        self.prepare_start_nav_on_open(false);

        if let Some(id) = self.start_window {
            // Re-focus existing window; force running restore in case game started while closed.
            self.last_running_check = None;
            let badge = self.refresh_running_badge();
            return Task::batch([badge, window::gain_focus(id)]);
        }

        self.start_nav_ready = false;
        self.gesture_detectors.reset();
        self.clear_start_nav_diag();
        self.start_opened_at = Some(Instant::now());
        // refresh_start_rows already ran a throttled check; force one restore on open.
        self.last_running_check = None;
        let badge = self.refresh_running_badge();

        let (id, open) = window::open(window::Settings {
            size: Size::new(start_view::WIDTH, start_view::HEIGHT),
            position: window::Position::Centered,
            // Visible immediately; StartOpened still arms focus + start_nav_ready.
            visible: true,
            resizable: false,
            decorations: false,
            level: window::Level::AlwaysOnTop,
            exit_on_close_request: true,
            platform_specific: overlay_platform_specific(),
            ..window::Settings::default()
        });
        self.start_window = Some(id);
        Task::batch([
            badge,
            open.map(Message::StartOpened),
            self.sync_toast_zorder(),
        ])
    }

    /// Reset to Games and (optionally) require a full control release before nav fires.
    fn prepare_start_nav_on_open(&mut self, arm_immediately: bool) {
        self.start_state.reset_to_games();
        self.pad_nav.prepare_on_open(arm_immediately);
        self.keyboard_cross_hold.reset();
        self.confirm_key_held = false;
        self.cancel_key_held = false;
        self.pad_held = FaceHeld::default();
    }

    fn close_start_screen(&mut self) -> Task<Message> {
        self.start_nav_ready = false;
        self.pad_nav.reset();
        self.keyboard_cross_hold.reset();
        self.confirm_key_held = false;
        self.cancel_key_held = false;
        self.pad_held = FaceHeld::default();
        self.start_state.reset_to_games();
        self.discard_edit_draft();
        self.clear_start_nav_diag();
        match self.start_window {
            Some(id) => window::set_mode(id, window::Mode::Hidden).chain(window::close(id)),
            None => Task::none(),
        }
    }

    fn clear_start_nav_diag(&mut self) {
        self.nav_missing_warned = false;
        self.nav_unarmed_warned = false;
        self.nav_source_logged = None;
        self.last_nav_log = None;
        self.last_pad_poll_at = None;
        self.nav_missing_since = None;
        self.nav_stale_warned = false;
        self.nav_not_ready_warned = false;
        self.start_opened_at = None;
    }

    fn game_header_for_row(&self, row: &start_view::StartRow) -> String {
        if let Some(appid) = row.play_key.strip_prefix("steam:") {
            return GameRef::format(appid.parse().ok(), &row.title);
        }
        if let Some(stored) = self
            .macros
            .for_game(&row.play_key)
            .and_then(|g| g.headers.get("game"))
        {
            return stored.clone();
        }
        GameRef::format(None, &row.title)
    }

    fn macro_list_rows(&self, play_key: &str) -> Vec<MacroListRow> {
        self.macros
            .for_game(play_key)
            .map(|g| {
                g.macros
                    .iter()
                    .map(|m| MacroListRow {
                        id: m.id.clone(),
                        name: m.name.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn open_macros_overlay(&mut self) {
        let Some(row) = self
            .start_state
            .rows
            .get(self.start_state.game_selected)
            .cloned()
        else {
            return;
        };
        if !row.in_catalog() {
            return;
        }
        let run_mode = !self.start_state.editing
            && self
                .start_state
                .running_target
                .as_ref()
                .is_some_and(|t| t == &row.target)
            && row.has_macros;
        if !self.start_state.editing && !run_mode {
            return;
        }
        let header = self.game_header_for_row(&row);
        self.macros.ensure_game_header(&row.play_key, header);
        let rows = self.macro_list_rows(&row.play_key);
        self.start_state
            .open_macro_list(row.play_key, row.title, run_mode, rows, None);
    }

    fn return_to_macro_list(&mut self, status: Option<String>) {
        let play_key = self
            .start_state
            .macro_overlay
            .as_ref()
            .map(|o| o.play_key().to_string());
        let Some(play_key) = play_key else {
            return;
        };
        let title = self
            .start_state
            .rows
            .iter()
            .find(|r| r.play_key == play_key)
            .map(|r| r.title.clone())
            .unwrap_or_else(|| play_key.clone());
        let rows = self.macro_list_rows(&play_key);
        let run_mode = false;
        self.start_state
            .open_macro_list(play_key, title, run_mode, rows, status);
        self.refresh_start_rows();
    }

    fn open_macro_editor(&mut self, id: Option<String>) {
        let play_key = match self.start_state.macro_overlay.as_ref() {
            Some(o) => o.play_key().to_string(),
            None => return,
        };
        let (name, action) = if let Some(ref mid) = id {
            self.macros
                .find_macro(&play_key, mid)
                .map(|m| (m.name.clone(), m.body.clone()))
                .unwrap_or_default()
        } else {
            (String::new(), String::new())
        };
        self.start_state.macro_overlay = Some(MacroOverlay::Edit {
            play_key,
            id,
            name,
            action,
            error: None,
        });
    }

    fn open_macro_editor_selected(&mut self) {
        let id = match self.start_state.macro_overlay.as_ref() {
            Some(MacroOverlay::List { selected, rows, .. }) => {
                rows.get(*selected).map(|r| r.id.clone())
            }
            _ => None,
        };
        let Some(id) = id else {
            return;
        };
        self.open_macro_editor(Some(id));
    }

    fn save_macro_editor(&mut self) {
        let Some(MacroOverlay::Edit {
            play_key,
            id,
            name,
            action,
            ..
        }) = self.start_state.macro_overlay.clone()
        else {
            return;
        };
        let name = name.trim().to_string();
        let action = action.trim().to_string();
        if name.is_empty() {
            if let Some(MacroOverlay::Edit { error, .. }) = self.start_state.macro_overlay.as_mut()
            {
                *error = Some("name must be non-empty".into());
            }
            return;
        }
        // Allow pasting a full `action: …` line into the field.
        let action = action
            .strip_prefix("action:")
            .map(|s| s.trim().to_string())
            .unwrap_or(action);
        let result = match id {
            Some(ref mid) => self
                .macros
                .update_macro(&play_key, mid, name, action)
                .map(|_| ()),
            None => self.macros.add_macro(&play_key, name, action).map(|_| ()),
        };
        match result {
            Ok(()) => {
                self.macros.save();
                self.return_to_macro_list(None);
            }
            Err(err) => {
                if let Some(MacroOverlay::Edit { error, .. }) =
                    self.start_state.macro_overlay.as_mut()
                {
                    *error = Some(err);
                }
            }
        }
    }

    fn remove_selected_macro(&mut self) {
        let (play_key, id) = match self.start_state.macro_overlay.as_ref() {
            Some(MacroOverlay::List {
                play_key,
                selected,
                rows,
                run_mode: false,
                ..
            }) => {
                let Some(row) = rows.get(*selected) else {
                    return;
                };
                (play_key.clone(), row.id.clone())
            }
            _ => return,
        };
        if self.macros.remove_macro(&play_key, &id) {
            self.macros.save();
        }
        let rows = self.macro_list_rows(&play_key);
        self.start_state.refresh_macro_list_rows(rows);
        self.refresh_start_rows();
    }

    fn copy_macro_catalog(&mut self) -> Task<Message> {
        let Some(MacroOverlay::List { play_key, .. }) = self.start_state.macro_overlay.as_ref()
        else {
            return Task::none();
        };
        let play_key = play_key.clone();
        let fallback = self
            .start_state
            .rows
            .iter()
            .find(|r| r.play_key == play_key)
            .map(|r| self.game_header_for_row(r))
            .unwrap_or_else(|| play_key.clone());
        let Some(text) = self.macros.export_catalog(&play_key, &fallback) else {
            return Task::none();
        };
        self.play_start_sound(UiSoundKind::Action);
        clipboard::write(text)
    }

    fn copy_selected_macro(&mut self) -> Task<Message> {
        let (play_key, id) = match self.start_state.macro_overlay.as_ref() {
            Some(MacroOverlay::List {
                play_key,
                selected,
                rows,
                ..
            }) => {
                let Some(row) = rows.get(*selected) else {
                    return Task::none();
                };
                (play_key.clone(), row.id.clone())
            }
            _ => return Task::none(),
        };
        let fallback = self
            .start_state
            .rows
            .iter()
            .find(|r| r.play_key == play_key)
            .map(|r| self.game_header_for_row(r))
            .unwrap_or_else(|| play_key.clone());
        let Some(text) = self.macros.export_macro(&play_key, &id, &fallback) else {
            return Task::none();
        };
        self.play_start_sound(UiSoundKind::Action);
        clipboard::write(text)
    }

    fn import_macros_from_text(&mut self, text: String) {
        let open_key = match self.start_state.macro_overlay.as_ref() {
            Some(o) => o.play_key().to_string(),
            None => return,
        };
        let doc = match macro_lib::parse_import(&text) {
            Ok(doc) => doc,
            Err(err) => {
                if let Some(MacroOverlay::List { status, .. }) =
                    self.start_state.macro_overlay.as_mut()
                {
                    *status = Some(err.to_string());
                }
                return;
            }
        };
        let catalog = self.display_catalog();
        let bind = self
            .macros
            .resolve_import_target(&doc, &open_key, &catalog, |e| match e {
                crate::games::GameEntry::Steam { appid } => self
                    .steam_by_id
                    .get(appid)
                    .map(|g| g.name.clone())
                    .unwrap_or_else(|| format!("Steam {appid}")),
                crate::games::GameEntry::Manual { title, .. } => title.clone(),
            });
        let (play_key, warning) = match bind {
            ImportBind::Target(key) => (key, None),
            ImportBind::Fallback { play_key, warning } => (play_key, Some(warning)),
            ImportBind::Ambiguous(msg) => {
                if let Some(MacroOverlay::List { status, .. }) =
                    self.start_state.macro_overlay.as_mut()
                {
                    *status = Some(msg);
                }
                return;
            }
        };
        match self.macros.merge_document(&play_key, doc) {
            Ok(result) => {
                self.macros.save();
                let status = Some(format!(
                    "imported +{} ~{}{}",
                    result.added,
                    result.updated,
                    warning
                        .as_ref()
                        .map(|w| format!(" ({w})"))
                        .unwrap_or_default()
                ));
                if play_key == open_key {
                    let rows = self.macro_list_rows(&play_key);
                    if let Some(MacroOverlay::List {
                        rows: dest,
                        selected,
                        status: st,
                        ..
                    }) = self.start_state.macro_overlay.as_mut()
                    {
                        *dest = rows;
                        if dest.is_empty() {
                            *selected = 0;
                        } else {
                            *selected = (*selected).min(dest.len() - 1);
                        }
                        *st = status;
                    }
                } else {
                    // Switched to another game — reopen that list.
                    let title = self
                        .start_state
                        .rows
                        .iter()
                        .find(|r| r.play_key == play_key)
                        .map(|r| r.title.clone())
                        .unwrap_or_else(|| play_key.clone());
                    let rows = self.macro_list_rows(&play_key);
                    self.start_state
                        .open_macro_list(play_key, title, false, rows, status);
                }
                self.refresh_start_rows();
            }
            Err(err) => {
                if let Some(MacroOverlay::List { status, .. }) =
                    self.start_state.macro_overlay.as_mut()
                {
                    *status = Some(err);
                }
            }
        }
    }

    fn run_selected_macro(&mut self) -> Task<Message> {
        let (play_key, id, name) = match self.start_state.macro_overlay.as_ref() {
            Some(MacroOverlay::List {
                play_key,
                selected,
                rows,
                run_mode: true,
                ..
            }) => {
                let Some(row) = rows.get(*selected) else {
                    return Task::none();
                };
                (play_key.clone(), row.id.clone(), row.name.clone())
            }
            _ => return Task::none(),
        };
        let Some(record) = self.macros.find_macro(&play_key, &id).cloned() else {
            return Task::none();
        };
        let steps = match macro_text::parse_action(&record.body) {
            Ok(s) => s,
            Err(err) => {
                app_log::warn(format!("macro: bad action name={name}: {err}"));
                return Task::none();
            }
        };
        let paths = self
            .running_session
            .as_ref()
            .map(|s| s.match_paths.clone())
            .unwrap_or_default();
        if paths.is_empty() {
            app_log::warn(format!("macro: no match paths for {play_key}"));
            return Task::none();
        }
        let name_for_focus = name.clone();
        Task::perform(
            spawn_blocking(move || {
                macro_run::focus_game(&paths)
                    .map_err(|e| e.to_string())
                    .map(|()| (paths, steps))
            }),
            move |result| match result {
                Ok(Ok((paths, steps))) => Message::MacroFocusDone {
                    name: name_for_focus,
                    paths,
                    steps,
                    result: Ok(()),
                },
                Ok(Err(err)) => Message::MacroFocusDone {
                    name: name_for_focus,
                    paths: Vec::new(),
                    steps: Vec::new(),
                    result: Err(err),
                },
                Err(err) => Message::MacroFocusDone {
                    name: name_for_focus,
                    paths: Vec::new(),
                    steps: Vec::new(),
                    result: Err(err),
                },
            },
        )
    }

    fn on_macro_focus_done(
        &mut self,
        name: String,
        paths: Vec<PathBuf>,
        steps: Vec<macro_text::Step>,
        result: Result<(), String>,
    ) -> Task<Message> {
        if let Err(err) = result {
            app_log::warn(format!("macro: focus failed name={name} err={err}"));
            return Task::none();
        }
        let close = self.close_start_screen();
        let name_run = name.clone();
        let run = Task::perform(
            spawn_blocking(move || macro_run::run_keys(&paths, &steps).map_err(|e| e.to_string())),
            move |result| Message::MacroRunDone {
                name: name_run,
                result: match result {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(err)) | Err(err) => Err(err),
                },
            },
        );
        close.chain(run)
    }

    /// Mouse report buttons — stamp both logs with kind + live snapshot context.
    #[cfg(debug_assertions)]
    fn mark_hitch_now(&mut self, kind: &str) {
        let start_open = self.start_window.is_some();
        let nav_ready = self.start_nav_ready;
        let controllers = self.controllers.len();
        stamp_hitch_report(
            kind,
            "button",
            Some(format!(
                "start_open={start_open} nav_ready={nav_ready} controllers={controllers}"
            )),
        );
    }

    fn log_start_nav_diag(&mut self, reading: &start_input::NavReading, pad_count: usize) {
        if self.nav_source_logged.is_none() {
            app_log::info(format!(
                "start-nav: source={} pads={} id={}",
                reading.source.as_str(),
                pad_count,
                reading.id.as_str()
            ));
            self.nav_source_logged = Some(reading.source);
        }

        let snap = NavLogSnapshot::from_sample(&reading.sample);
        if self.last_nav_log != Some(snap) {
            app_log::info(snap.format_line(reading.source));
            self.last_nav_log = Some(snap);
        }
    }

    fn on_start_message(&mut self, message: StartMessage) -> Task<Message> {
        // Keyboard / UI actions other than pure scroll cancel an in-progress hold.
        match &message {
            StartMessage::GamesScrolled(..)
            | StartMessage::ControllersScrolled(..)
            | StartMessage::ManualAddTitle(_)
            | StartMessage::ManualAddArgs(_)
            | StartMessage::MacroEditName(_)
            | StartMessage::MacroEditAction(_) => {}
            #[cfg(debug_assertions)]
            StartMessage::ReportLightbarFailure | StartMessage::ReportInputFailure => {}
            _ => self.cancel_start_holds(),
        }
        match message {
            StartMessage::Launch(index) => {
                if index < self.start_state.rows.len() {
                    self.start_state.game_selected = index;
                }
                let scroll = self.scroll_start_selection_into_view();
                if self.start_state.editing {
                    match self.edit_toggle_selected() {
                        Some(task) => {
                            self.play_start_sound(UiSoundKind::Action);
                            scroll.chain(task)
                        }
                        None => scroll,
                    }
                } else if self.launch_selected() {
                    self.play_start_sound(UiSoundKind::Action);
                    scroll
                } else {
                    scroll
                }
            }
            StartMessage::SelectController(index) => {
                if index < self.start_state.controllers.len()
                    && self.start_state.controller_selected != index
                {
                    self.start_state.controller_selected = index;
                    self.play_start_sound(UiSoundKind::Nav);
                }
                self.scroll_start_selection_into_view()
            }
            StartMessage::MoveUp => {
                let direction = self.start_state.move_selection(-1);
                if direction.is_some() {
                    self.play_start_sound(UiSoundKind::Nav);
                }
                self.scroll_start_selection_into_view_dir(
                    direction.unwrap_or(start_view::ScrollReveal::Up),
                )
            }
            StartMessage::MoveDown => {
                let direction = self.start_state.move_selection(1);
                if direction.is_some() {
                    self.play_start_sound(UiSoundKind::Nav);
                }
                self.scroll_start_selection_into_view_dir(
                    direction.unwrap_or(start_view::ScrollReveal::Down),
                )
            }
            StartMessage::Confirm => self.on_start_confirm(),
            StartMessage::Close => {
                if matches!(
                    self.start_state.macro_overlay,
                    Some(MacroOverlay::Edit { .. })
                ) {
                    self.play_start_sound(UiSoundKind::Action);
                    self.return_to_macro_list(None);
                    Task::none()
                } else if self.start_state.macro_overlay.take().is_some() {
                    self.play_start_sound(UiSoundKind::Action);
                    Task::none()
                } else if self.start_state.manual_add.is_some() {
                    self.play_start_sound(UiSoundKind::Action);
                    self.cancel_manual_add()
                } else if self.start_state.replace_confirm.take().is_some() {
                    self.play_start_sound(UiSoundKind::Action);
                    Task::none()
                } else if self.start_state.editing {
                    self.play_start_sound(UiSoundKind::Action);
                    self.cancel_start_edit()
                } else if self.start_window.is_some() {
                    self.play_start_sound(UiSoundKind::Action);
                    self.close_start_screen()
                } else {
                    Task::none()
                }
            }
            StartMessage::PrevSlide => {
                if self.start_state.overlay_blocking() {
                    return Task::none();
                }
                if self.start_state.editing {
                    self.discard_edit_draft();
                    self.refresh_start_rows();
                }
                // L2: Controllers → Games only (no wrap). Interruptible mid-anim.
                self.start_state
                    .request_slide(StartSlide::Games, Instant::now());
                Task::none()
            }
            StartMessage::NextSlide => {
                if self.start_state.overlay_blocking() {
                    return Task::none();
                }
                if self.start_state.editing {
                    self.discard_edit_draft();
                    self.refresh_start_rows();
                }
                // R2: Games → Controllers only (no wrap). Interruptible mid-anim.
                self.start_state
                    .request_slide(StartSlide::Controllers, Instant::now());
                Task::none()
            }
            StartMessage::ToggleEdit => match self.toggle_start_edit() {
                Some(task) => {
                    self.play_start_sound(UiSoundKind::Action);
                    task
                }
                None => Task::none(),
            },
            StartMessage::CycleSort => match self.on_start_cycle_sort() {
                Some(task) => {
                    self.play_start_sound(UiSoundKind::Action);
                    task
                }
                None => Task::none(),
            },
            StartMessage::AddShortcut => {
                if self.start_state.editing && !self.start_state.overlay_blocking() {
                    self.play_start_sound(UiSoundKind::Action);
                    self.pick_manual_shortcut()
                } else {
                    Task::none()
                }
            }
            StartMessage::EditManual => {
                let before = self.start_state.manual_add.is_some();
                let task = self.begin_manual_edit();
                if !before && self.start_state.manual_add.is_some() {
                    self.play_start_sound(UiSoundKind::Action);
                }
                task
            }
            StartMessage::OpenMacros => {
                self.play_start_sound(UiSoundKind::Action);
                self.open_macros_overlay();
                Task::none()
            }
            StartMessage::MacroSelect(index) => {
                if let Some(MacroOverlay::List { selected, rows, .. }) =
                    self.start_state.macro_overlay.as_mut()
                    && index < rows.len()
                {
                    *selected = index;
                    self.play_start_sound(UiSoundKind::Nav);
                }
                Task::none()
            }
            StartMessage::MacroAdd => {
                self.play_start_sound(UiSoundKind::Action);
                self.open_macro_editor(None);
                Task::none()
            }
            StartMessage::MacroImport => {
                self.play_start_sound(UiSoundKind::Action);
                clipboard::read().map(|text| Message::Start(StartMessage::MacroImportPaste(text)))
            }
            StartMessage::MacroImportPaste(text) => {
                self.import_macros_from_text(text.unwrap_or_default());
                Task::none()
            }
            StartMessage::MacroCopyCatalog => self.copy_macro_catalog(),
            StartMessage::MacroCopySelected => self.copy_selected_macro(),
            StartMessage::MacroEditSelected => {
                self.play_start_sound(UiSoundKind::Action);
                self.open_macro_editor_selected();
                Task::none()
            }
            StartMessage::MacroRemoveSelected => {
                self.play_start_sound(UiSoundKind::Action);
                self.remove_selected_macro();
                Task::none()
            }
            StartMessage::MacroEditName(name) => {
                if let Some(MacroOverlay::Edit { name: dest, .. }) =
                    self.start_state.macro_overlay.as_mut()
                {
                    *dest = name;
                }
                Task::none()
            }
            StartMessage::MacroEditAction(action) => {
                if let Some(MacroOverlay::Edit { action: dest, .. }) =
                    self.start_state.macro_overlay.as_mut()
                {
                    *dest = action;
                }
                Task::none()
            }
            StartMessage::MacroEditSave => {
                self.play_start_sound(UiSoundKind::Action);
                self.save_macro_editor();
                Task::none()
            }
            StartMessage::GamesScrolled(y, viewport_h) => {
                self.start_state.set_games_scroll(y, viewport_h);
                Task::none()
            }
            StartMessage::ControllersScrolled(y, viewport_h) => {
                self.start_state.set_controllers_scroll(y, viewport_h);
                Task::none()
            }
            StartMessage::ManualAddTitle(title) => {
                if let Some(draft) = self.start_state.manual_add.as_mut() {
                    draft.title = title;
                }
                Task::none()
            }
            StartMessage::ManualAddArgs(args) => {
                if let Some(draft) = self.start_state.manual_add.as_mut() {
                    draft.args = args;
                }
                Task::none()
            }
            StartMessage::ManualPickIcon => {
                self.play_start_sound(UiSoundKind::Action);
                self.pick_manual_icon()
            }
            StartMessage::ManualClearIcon => {
                if let Some(draft) = self.start_state.manual_add.as_mut() {
                    draft.icon_path = None;
                    self.play_start_sound(UiSoundKind::Action);
                }
                Task::none()
            }
            #[cfg(debug_assertions)]
            StartMessage::ReportLightbarFailure => {
                self.mark_hitch_now("lightbar");
                Task::none()
            }
            #[cfg(debug_assertions)]
            StartMessage::ReportInputFailure => {
                self.mark_hitch_now("input");
                Task::none()
            }
        }
    }

    fn play_start_sound(&self, kind: UiSoundKind) {
        if !self.prefs.start_screen_sounds_enabled {
            return;
        }
        let volume = f32::from(self.prefs.start_screen_sound_volume) / 100.0;
        ui_sound::play(kind, volume);
    }

    fn cancel_start_holds(&mut self) {
        self.pad_nav.cancel_holds();
        self.pad_nav.cancel_held_face_releases();
        self.keyboard_cross_hold.cancel();
        self.start_state.triangle_progress = 0.0;
        self.start_state.cross_progress = 0.0;
    }

    fn sync_start_held(&mut self) {
        let mut held = self.pad_held;
        if self.confirm_key_held {
            held.cross = true;
        }
        if self.cancel_key_held {
            held.circle = true;
        }
        self.start_state.held = held;
    }

    fn scroll_start_selection_into_view(&mut self) -> Task<Message> {
        self.scroll_start_selection_into_view_dir(start_view::ScrollReveal::Either)
    }

    fn scroll_start_selection_into_view_dir(
        &mut self,
        direction: start_view::ScrollReveal,
    ) -> Task<Message> {
        let Some((id, y)) = self.start_state.selection_scroll_y(direction) else {
            return Task::none();
        };
        self.start_state.note_scroll_y(&id, y);
        operation::scroll_to(
            id,
            operation::AbsoluteOffset {
                x: None,
                y: Some(y),
            },
        )
    }

    fn scroll_start_selection_to_center(&mut self, force: bool) -> Task<Message> {
        let target = if force {
            self.start_state.selection_scroll_y_center_prefer()
        } else {
            self.start_state.selection_scroll_y_center()
        };
        let Some((id, y)) = target else {
            return Task::none();
        };
        self.start_state.note_scroll_y(&id, y);
        operation::scroll_to(
            id,
            operation::AbsoluteOffset {
                x: None,
                y: Some(y),
            },
        )
    }

    fn toggle_start_edit(&mut self) -> Option<Task<Message>> {
        if self.start_state.slide != StartSlide::Games
            || self.start_state.overlay_blocking()
            || self.start_state.animating()
        {
            return None;
        }
        Some(if self.start_state.editing {
            self.commit_start_edit()
        } else {
            self.enter_start_edit()
        })
    }

    fn cycle_games_sort(&mut self) -> Option<Task<Message>> {
        if self.start_state.editing
            || self.start_state.slide != StartSlide::Games
            || self.start_state.overlay_blocking()
        {
            return None;
        }
        self.prefs.games_sort_mode = self.prefs.games_sort_mode.cycle();
        self.prefs.save();
        self.refresh_start_rows();
        Some(self.scroll_start_selection_into_view())
    }

    fn on_start_cycle_sort(&mut self) -> Option<Task<Message>> {
        match self.start_state.slide {
            StartSlide::Games => self.cycle_games_sort(),
            StartSlide::Controllers => {
                let next = !self.prefs.show_all_controllers;
                self.set_show_all_controllers(next)
            }
        }
    }

    fn set_show_all_controllers(&mut self, show_all: bool) -> Option<Task<Message>> {
        if self.start_state.slide != StartSlide::Controllers
            || self.start_state.overlay_blocking()
            || self.prefs.show_all_controllers == show_all
        {
            return None;
        }
        self.prefs.show_all_controllers = show_all;
        self.prefs.save();
        app_log::hid_trace(format!(
            "start-controllers: show_all={}",
            u8::from(show_all)
        ));
        self.refresh_start_controllers();
        Some(self.scroll_start_selection_into_view())
    }

    fn pick_manual_shortcut(&mut self) -> Task<Message> {
        self.start_file_dialog_open = true;
        Task::perform(
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
        )
    }

    fn pick_manual_icon(&mut self) -> Task<Message> {
        self.start_file_dialog_open = true;
        Task::perform(
            spawn_blocking(|| {
                rfd::FileDialog::new()
                    .add_filter("Images", &["png", "jpg", "jpeg", "ico", "webp"])
                    .pick_file()
                    .map(|p| p.to_string_lossy().into_owned())
            }),
            |result| match result {
                Ok(path) => Message::ManualIconPicked(path.map(PathBuf::from)),
                Err(_) => Message::ManualIconPicked(None),
            },
        )
    }

    fn begin_manual_edit(&mut self) -> Task<Message> {
        if !self.start_state.editing || self.start_state.overlay_blocking() {
            return Task::none();
        }
        let Some(row) = self
            .start_state
            .rows
            .get(self.start_state.game_selected)
            .cloned()
        else {
            return Task::none();
        };
        let Some(start_view::EditRow::Manual { id }) = row.edit.as_ref() else {
            return Task::none();
        };
        let icon_path = self.edit_catalog().entries.iter().find_map(|e| match e {
            crate::games::GameEntry::Manual { id: mid, icon, .. } if mid == id => {
                icon.as_ref().map(PathBuf::from)
            }
            _ => None,
        });
        let shell_icon = start_view::probe_shell_icon(&row.target);
        self.start_state.manual_add = Some(start_view::ManualAddDraft {
            edit_id: Some(id.clone()),
            target: row.target,
            title: row.title,
            args: row.args,
            icon_path,
            shell_icon,
        });
        Task::none()
    }

    fn edit_toggle_selected(&mut self) -> Option<Task<Message>> {
        let row = self
            .start_state
            .rows
            .get(self.start_state.game_selected)
            .cloned()?;
        let changed = match row.edit.as_ref() {
            Some(start_view::EditRow::Steam { appid, in_catalog }) => {
                self.edit_catalog_mut().toggle_steam(*appid, !*in_catalog)
            }
            Some(start_view::EditRow::Manual { id }) => {
                self.edit_catalog_mut().remove_manual_id(id)
            }
            None => false,
        };
        if changed {
            self.refresh_start_rows();
            Some(self.scroll_start_selection_into_view())
        } else {
            None
        }
    }

    fn on_start_confirm(&mut self) -> Task<Message> {
        if let Some(MacroOverlay::Edit { .. }) = self.start_state.macro_overlay.as_ref() {
            self.play_start_sound(UiSoundKind::Action);
            self.save_macro_editor();
            return Task::none();
        }
        if let Some(MacroOverlay::List { run_mode, .. }) = self.start_state.macro_overlay.as_ref() {
            self.play_start_sound(UiSoundKind::Action);
            if *run_mode {
                return self.run_selected_macro();
            }
            self.open_macro_editor_selected();
            return Task::none();
        }
        if self.start_state.manual_add.is_some() {
            self.play_start_sound(UiSoundKind::Action);
            return self.confirm_manual_add();
        }
        // Replace confirm requires hold-Cross / hold-Enter (see pad poll / key hold).
        if self.start_state.replace_confirm.is_some() {
            return Task::none();
        }
        match self.start_state.slide {
            StartSlide::Games if self.start_state.editing => match self.edit_toggle_selected() {
                Some(task) => {
                    self.play_start_sound(UiSoundKind::Action);
                    task
                }
                None => Task::none(),
            },
            StartSlide::Games => {
                if self.launch_selected() {
                    self.play_start_sound(UiSoundKind::Action);
                }
                Task::none()
            }
            StartSlide::Controllers => {
                if let Some(row) = self.start_state.selected_controller()
                    && row.connected
                {
                    let serial = row.serial.clone();
                    if self.identify(&serial) {
                        self.popup_state.begin_identify_flash(&serial);
                        self.start_state.begin_identify_flash(&serial);
                        self.play_start_sound(UiSoundKind::Action);
                    }
                }
                Task::none()
            }
        }
    }

    fn complete_replace_confirm(&mut self) -> Task<Message> {
        let Some(confirm) = self.start_state.replace_confirm.take() else {
            return Task::none();
        };
        self.keyboard_cross_hold.reset();
        self.confirm_key_held = false;
        self.start_state.cross_progress = 0.0;
        self.close_running_game();
        self.start_state.game_selected = confirm.next_index;
        let _ = self.launch_selected();
        Task::none()
    }

    /// Launch the selected game. Returns true when launch, replace-confirm, or a real attempt ran.
    fn launch_selected(&mut self) -> bool {
        let Some(row) = self
            .start_state
            .rows
            .get(self.start_state.game_selected)
            .cloned()
        else {
            return false;
        };

        if let Some(session) = self.running_session.as_ref() {
            if session.matches_target(&row.target) {
                return false;
            }
            if process_match::any_matching_running(&session.match_paths) {
                self.start_state.replace_confirm = Some(ReplaceConfirm {
                    running_title: session.title.clone(),
                    next_title: row.title.clone(),
                    next_index: self.start_state.game_selected,
                });
                return true;
            }
            self.running_session = None;
            self.start_state.running_target = None;
        }

        match launch::launch_target(&row.target, &row.args) {
            Ok(()) => {
                self.touch_last_played(&row.play_key);
                self.begin_running_session(row.title, row.target);
                true
            }
            Err(err) => {
                app_log::warn(format!("launch failed for {}: {err}", row.target));
                false
            }
        }
    }

    fn on_pad_poll(&mut self) -> Task<Message> {
        let poll_started = Instant::now();
        let start_open = self.start_window.is_some();

        if start_open {
            if let Some(prev) = self.last_pad_poll_at {
                let gap_ms = prev.elapsed().as_millis();
                if gap_ms >= PAD_POLL_STALL_MS {
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: pad-poll stall gap_ms={gap_ms}"
                    ));
                }
            }
            self.last_pad_poll_at = Some(poll_started);

            if !self.start_nav_ready
                && !self.nav_not_ready_warned
                && self
                    .start_opened_at
                    .is_some_and(|t| t.elapsed().as_millis() >= START_NAV_NOT_READY_MS)
            {
                let for_ms = self
                    .start_opened_at
                    .map(|t| t.elapsed().as_millis())
                    .unwrap_or(0);
                app_log::warn(format!("start-nav: not ready for_ms={for_ms}"));
                self.nav_not_ready_warned = true;
            }
        }

        let pad_input_open =
            self.configure_window.is_some() && self.configure_state.section == Section::PadInput;
        let listening = self.prefs.start_screen_enabled
            && (!self.controllers.is_empty() || self.gesture_recorder.is_active());

        let (readings, meta) = match start_input::read_nav_readings() {
            start_input::NavReadingsOutcome::Readings { readings, meta } => (readings, Some(meta)),
            start_input::NavReadingsOutcome::Missing { meta } => (Vec::new(), Some(meta)),
        };

        if pad_input_open {
            self.apply_pad_input_panel_from_readings(&readings);
        }

        if !listening {
            return Task::none();
        }

        if self.gesture_recorder.is_active() {
            return self.on_gesture_record(&readings);
        }

        if start_open {
            if !self.start_nav_ready {
                return Task::none();
            }
            if readings.is_empty() {
                if let Some(meta) = &meta {
                    let age_ms = meta.published_at.elapsed().as_millis();
                    if self.nav_missing_since.is_none() {
                        self.nav_missing_since = Some(Instant::now());
                    }
                    if !self.nav_missing_warned && !self.controllers.is_empty() {
                        app_log::warn(format!(
                            "start-nav: no hid-worker input snapshot yet; keyboard/mouse still work reason={} age_ms={age_ms} seq={}",
                            meta.reason.as_str(),
                            meta.seq
                        ));
                        self.nav_missing_warned = true;
                    }
                }
                return Task::none();
            }

            if let Some(since) = self.nav_missing_since.take() {
                let gap_ms = since.elapsed().as_millis();
                let reason = meta.as_ref().map(|m| m.reason.as_str()).unwrap_or("-");
                app_log::info(format!(
                    "start-nav: snapshot restored gap_ms={gap_ms} pads={} reason={reason}",
                    readings.len()
                ));
                self.nav_missing_warned = false;
            }

            if let Some(meta) = &meta {
                let age_ms = meta.published_at.elapsed().as_millis();
                if age_ms >= SNAPSHOT_STALE_MS {
                    if !self.nav_stale_warned {
                        app_log::warn(format!(
                            "start-nav: snapshot stale age_ms={age_ms} seq={} reason={}",
                            meta.seq,
                            meta.reason.as_str()
                        ));
                        self.nav_stale_warned = true;
                    }
                } else {
                    self.nav_stale_warned = false;
                }
            }

            let task = self.handle_start_nav_readings(&readings);
            let total_ms = poll_started.elapsed().as_millis();
            if total_ms >= PAD_POLL_SLOW_MS {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: pad-poll slow total_ms={total_ms}"
                ));
            }
            return task;
        }

        self.on_reopen_gesture(&readings)
    }

    fn handle_start_nav_readings(&mut self, readings: &[start_input::NavReading]) -> Task<Message> {
        self.nav_missing_warned = false;

        let now = Instant::now();
        let animating = self.start_state.animating();
        // Mid-slide: keep L2/R2 edges live for interruptible request_slide,
        // but skip controller-row rebuilds (those hitch the UI thread).
        let badge_task = if !animating {
            let controllers_started = Instant::now();
            self.refresh_start_controllers();
            let controllers_ms = controllers_started.elapsed().as_millis();

            let badge_started = Instant::now();
            let badge = self.refresh_running_badge();
            let badge_ms = badge_started.elapsed().as_millis();

            if controllers_ms >= PAD_POLL_SLOW_MS || badge_ms >= PAD_POLL_SLOW_MS {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: pad-poll slow controllers_ms={controllers_ms} badge_ms={badge_ms}"
                ));
            }
            badge
        } else {
            Task::none()
        };

        let _ = self.start_state.tick_anim(now);

        let replace_confirm = self.start_state.replace_confirm.is_some();
        let manual_add = self.start_state.manual_add.is_some();
        let macro_list = matches!(
            self.start_state.macro_overlay,
            Some(MacroOverlay::List { .. })
        );
        let macro_edit = matches!(
            self.start_state.macro_overlay,
            Some(MacroOverlay::Edit { .. })
        );
        let allow_nav_move = !animating && !replace_confirm && !manual_add && !macro_edit;
        let editing = self.start_state.editing;
        let hold_cross_close = !editing
            && !replace_confirm
            && !manual_add
            && !macro_list
            && !macro_edit
            && !animating
            && matches!(self.start_state.slide, StartSlide::Games);
        let hold_triangle_power = !editing
            && !replace_confirm
            && !manual_add
            && !macro_list
            && !macro_edit
            && !animating
            && matches!(self.start_state.slide, StartSlide::Controllers);
        let tick = self.pad_nav.tick(
            readings,
            now,
            allow_nav_move,
            replace_confirm && !animating,
            editing && !macro_list && !macro_edit,
            hold_cross_close,
            hold_triangle_power,
        );
        if let Some(diag) = &tick.diag {
            self.log_start_nav_diag(diag, readings.len());
        }
        if tick.unarmed_pads > 0 && tick.armed_pads == 0 {
            if !self.nav_unarmed_warned {
                if let Some(diag) = &tick.diag {
                    app_log::warn(format!(
                        "start-nav: pad unarmed waiting for rest (stick_y={:.3} l2={} r2={} cross={} circle={})",
                        diag.sample.stick_y,
                        u8::from(diag.sample.l2),
                        u8::from(diag.sample.r2),
                        u8::from(diag.sample.cross),
                        u8::from(diag.sample.circle),
                    ));
                }
                self.nav_unarmed_warned = true;
            }
        } else if tick.armed_pads > 0 {
            self.nav_unarmed_warned = false;
        }

        self.pad_held = tick.held;
        self.sync_start_held();

        let nav_task = if animating {
            self.start_state.tick_hint_anims(now);
            if let Some(action) = tick.action {
                match action {
                    NavAction::PrevSlide => self.on_start_message(StartMessage::PrevSlide),
                    NavAction::NextSlide => self.on_start_message(StartMessage::NextSlide),
                    NavAction::Cancel => self.on_start_message(StartMessage::Close),
                    _ => Task::none(),
                }
            } else {
                Task::none()
            }
        } else if replace_confirm {
            let mut cross_progress = tick.cross_progress;
            let mut cross_completed = tick.cross_completed;
            let (k_progress, k_completed) =
                self.keyboard_cross_hold.update(self.confirm_key_held, now);
            cross_progress = cross_progress.max(k_progress);
            cross_completed = cross_completed || k_completed;
            self.start_state.cross_progress = cross_progress;
            self.start_state.triangle_progress = 0.0;
            self.start_state.tick_hint_anims(now);
            if cross_completed {
                self.play_start_sound(UiSoundKind::Hold);
                self.complete_replace_confirm()
            } else if tick.action == Some(NavAction::Cancel) {
                self.keyboard_cross_hold.cancel();
                self.start_state.cross_progress = 0.0;
                self.on_start_message(StartMessage::Close)
            } else {
                Task::none()
            }
        } else if manual_add || macro_edit {
            self.start_state.cross_progress = 0.0;
            self.start_state.triangle_progress = 0.0;
            self.keyboard_cross_hold.reset();
            self.start_state.tick_hint_anims(now);
            if let Some(action) = tick.action {
                match action {
                    NavAction::Confirm | NavAction::Cancel => self.on_start_message(match action {
                        NavAction::Confirm => StartMessage::Confirm,
                        _ => StartMessage::Close,
                    }),
                    _ => Task::none(),
                }
            } else {
                Task::none()
            }
        } else if macro_list {
            self.start_state.cross_progress = 0.0;
            self.start_state.triangle_progress = 0.0;
            self.keyboard_cross_hold.reset();
            self.start_state.tick_hint_anims(now);
            let run_mode = matches!(
                self.start_state.macro_overlay,
                Some(MacroOverlay::List { run_mode: true, .. })
            );
            if let Some(action) = tick.action {
                match action {
                    NavAction::Up => self.on_start_message(StartMessage::MoveUp),
                    NavAction::Down => self.on_start_message(StartMessage::MoveDown),
                    NavAction::Confirm => self.on_start_message(StartMessage::Confirm),
                    NavAction::Cancel => self.on_start_message(StartMessage::Close),
                    NavAction::CycleSort if !run_mode => {
                        self.on_start_message(StartMessage::MacroCopySelected)
                    }
                    NavAction::ToggleEdit if !run_mode => {
                        self.on_start_message(StartMessage::MacroRemoveSelected)
                    }
                    _ => Task::none(),
                }
            } else {
                Task::none()
            }
        } else {
            self.keyboard_cross_hold.reset();

            if hold_cross_close {
                self.start_state.cross_progress = tick.cross_progress;
                if tick.cross_completed {
                    self.play_start_sound(UiSoundKind::Hold);
                    self.close_running_game_if_selected();
                }
            } else {
                self.start_state.cross_progress = 0.0;
            }

            if hold_triangle_power {
                self.start_state.triangle_progress = tick.triangle_progress;
                if tick.triangle_completed {
                    self.play_start_sound(UiSoundKind::Hold);
                    if let Some(row) = self.start_state.selected_controller()
                        && row.bluetooth
                    {
                        let serial = row.serial.clone();
                        self.power_off(&serial);
                    }
                }
            } else {
                self.start_state.triangle_progress = 0.0;
            }

            self.start_state.tick_hint_anims(now);

            if tick.cross_completed && hold_cross_close {
                // Hold-close already handled; do not also Confirm/Launch.
                Task::none()
            } else if let Some(action) = tick.action {
                match action {
                    NavAction::Up => self.on_start_message(StartMessage::MoveUp),
                    NavAction::Down => self.on_start_message(StartMessage::MoveDown),
                    NavAction::Confirm => self.on_start_message(StartMessage::Confirm),
                    NavAction::Cancel => self.on_start_message(StartMessage::Close),
                    NavAction::PrevSlide => self.on_start_message(StartMessage::PrevSlide),
                    NavAction::NextSlide => self.on_start_message(StartMessage::NextSlide),
                    NavAction::ToggleEdit => self.on_start_message(StartMessage::ToggleEdit),
                    NavAction::CycleSort => self.on_start_message(StartMessage::CycleSort),
                    NavAction::Triangle if self.start_state.editing => {
                        self.on_start_message(StartMessage::EditManual)
                    }
                    NavAction::Triangle => Task::none(),
                    NavAction::Macros => self.on_start_message(StartMessage::OpenMacros),
                }
            } else {
                Task::none()
            }
        };

        Task::batch([badge_task, nav_task])
    }

    fn on_gesture_record(&mut self, readings: &[start_input::NavReading]) -> Task<Message> {
        if readings.is_empty() {
            if !self.hid_exclusive_warned && !self.controllers.is_empty() {
                app_log::warn("could not read controller for gesture recording; try again");
                self.hid_exclusive_warned = true;
            }
            return Task::none();
        }
        self.hid_exclusive_warned = false;
        if let Some(sample) = self.gesture_record_latch.select(readings)
            && let Some(peak) = self.gesture_recorder.update(&sample.held)
        {
            self.prefs.start_screen_gesture = peak;
            self.prefs.save();
            self.gesture_record_latch.clear();
            self.gesture_detectors.consume_pending_match(readings);
        }
        Task::none()
    }

    fn on_reopen_gesture(&mut self, readings: &[start_input::NavReading]) -> Task<Message> {
        if readings.is_empty() {
            return Task::none();
        }
        if self.prefs.start_screen_enabled
            && !self.prefs.start_screen_gesture.is_empty()
            && self
                .gesture_detectors
                .update(&self.prefs.start_screen_gesture, readings)
        {
            self.open_start_screen()
        } else {
            Task::none()
        }
    }

    fn apply_pad_input_panel_from_readings(&mut self, readings: &[start_input::NavReading]) {
        let next = if let Some(reading) = start_input::preferred_reading(readings) {
            let held: Vec<_> = reading.sample.held.iter().copied().collect();
            PadInputPanel {
                has_sample: true,
                source: reading.source.as_str().to_string(),
                buttons0: None,
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
        } else {
            PadInputPanel::default()
        };
        self.pad_input_panel = next;
    }

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
        let status = crate::controller::dualsense::battery::dualsense_status(
            0,
            "DualSense",
            Connection::Usb,
            event.serial.clone(),
            event.percent.unwrap_or(0),
            event.state,
        );
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

    /// One-shot toast after crash auto-relaunch (or `--test-crash-toast`).
    fn maybe_show_crash_restart_toast(&mut self) -> Task<Message> {
        if !crash_restart::take_pending_notice() {
            return Task::none();
        }
        app_log::info("crash-restart: notice toast");
        self.toast_queue.push_back(ToastMessage::crash_restart());
        self.show_next_toast()
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

        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: toast show heading={:?} percent={} queue_left={}",
            message.heading,
            message.percent(),
            self.toast_queue.len()
        ));
        self.toast_message = Some(message);
        self.toast_generation = self.toast_generation.wrapping_add(1);
        let generation = self.toast_generation;

        let expire = Task::perform(delay(TOAST_LIFETIME), move |()| {
            Message::ToastDismiss(generation)
        });

        if let Some(id) = self.toast_window {
            return place_toast(id, generation).chain(expire);
        }

        let (_id, open) = self.create_toast_window();
        open.then(move |id| place_toast(id, generation))
            .chain(expire)
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

        // Stay visible across handoff so the next message can remount/present on
        // the same HWND (closing/recreating dropped the follow-up toast).
        if !self.toast_queue.is_empty() {
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: toast handoff remount queue={}",
                self.toast_queue.len()
            ));
            return self.show_next_toast();
        }

        // Keep the window alive but hidden: it doubles as the GPU compositor
        // sentinel so popup/Settings can open without a cold wgpu init.
        match self.toast_window {
            Some(id) => hide_toast(id),
            None => Task::none(),
        }
    }

    fn toast_animating(&self) -> bool {
        self.toast_message.is_some()
            && self.toast_placement.is_some()
            && (self.toast_dismissing || self.toast_anim_started.elapsed() < TOAST_SLIDE_DURATION)
    }

    /// Prefer Settings, then Start, then popup as the focused presenting window.
    fn refocus_interactive_ui(&self) -> Task<Message> {
        if let Some(id) = self.configure_window {
            return window::gain_focus(id);
        }
        if let Some(id) = self.start_window {
            return window::gain_focus(id);
        }
        if let Some(id) = self.popup_window {
            return window::gain_focus(id);
        }
        Task::none()
    }

    /// Raise interactive UI into the topmost band above the toast (last wins).
    /// Toast stays TOPMOST vs Cursor; UI above toast keeps presents (iced#3320).
    fn raise_interactive_ui_above_toast(&self, focus: bool) -> Task<Message> {
        let mut task = Task::none();
        if let Some(id) = self.popup_window {
            task = task.chain(raise_window_topmost(id));
        }
        if let Some(id) = self.start_window {
            task = task.chain(raise_window_topmost(id));
        }
        if let Some(id) = self.configure_window {
            task = task.chain(raise_window_topmost(id));
        }
        if focus {
            task.chain(self.refocus_interactive_ui())
        } else {
            task
        }
    }

    /// Re-assert toast topmost, then UI above it, after Settings / Start / popup
    /// open or close.
    fn sync_toast_zorder(&self) -> Task<Message> {
        let Some(id) = self.toast_window else {
            return Task::none();
        };
        if self.toast_message.is_none() {
            return Task::none();
        }
        crate::controller::hid::diag::diag_info("ui-diag: sync toast z-order (toast + raise UI)");
        window::set_level(id, window::Level::AlwaysOnTop)
            .chain(set_toast_topmost(id))
            .chain(self.raise_interactive_ui_above_toast(true))
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

fn place_toast(id: window::Id, generation: u64) -> Task<Message> {
    window::monitor_size(id).map(move |monitor| Message::PlaceToast {
        id,
        monitor,
        generation,
    })
}

/// Map tray events into iced `Message` values.
fn tray_events_mapped() -> impl Stream<Item = Message> {
    tray::tray_events().map(|event| match event {
        tray::TrayEvent::Menu(id) => Message::TrayMenu(id),
        tray::TrayEvent::LeftClick(anchor) => Message::TrayLeftClick(anchor),
    })
}

/// Stamp a hitch mark using the shared input snapshot (safe off the UI thread).
#[cfg(debug_assertions)]
fn stamp_hitch_report(kind: &str, source: &str, extra: Option<String>) {
    let (pads, reason, age_ms, seq) = match start_input::read_nav_readings() {
        start_input::NavReadingsOutcome::Readings { readings, meta } => (
            readings.len(),
            meta.reason.as_str(),
            meta.published_at.elapsed().as_millis(),
            meta.seq,
        ),
        start_input::NavReadingsOutcome::Missing { meta } => (
            0,
            meta.reason.as_str(),
            meta.published_at.elapsed().as_millis(),
            meta.seq,
        ),
    };
    let ctx = crate::controller::hid::diag::failure_context();
    let extra = extra.map(|e| format!(" {e}")).unwrap_or_default();
    app_log::mark_hitch(format!(
        "kind={kind} source={source}{extra} pads={pads} reason={reason} age_ms={age_ms} seq={seq} {ctx}"
    ));
}

/// Global F7 / F8 hitch markers — own Win32 message pump so marks still land if iced is stuck.
/// Debug builds only (`cfg(debug_assertions)`).
#[cfg(all(windows, debug_assertions))]
fn start_hitch_hotkey_worker() {
    use std::sync::OnceLock;
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        thread::spawn(|| unsafe {
            if win32::RegisterHotKey(
                0,
                win32::HOTKEY_ID_LIGHTBAR_HITCH,
                win32::MOD_NOREPEAT,
                win32::VK_F7,
            ) == 0
            {
                app_log::warn("hitch hotkey: failed to register F7");
                return;
            }
            if win32::RegisterHotKey(
                0,
                win32::HOTKEY_ID_INPUT_HITCH,
                win32::MOD_NOREPEAT,
                win32::VK_F8,
            ) == 0
            {
                app_log::warn("hitch hotkey: failed to register F8");
                win32::UnregisterHotKey(0, win32::HOTKEY_ID_LIGHTBAR_HITCH);
                return;
            }
            app_log::info("hitch hotkeys armed: F7=lightbar F8=input");

            let mut message = win32::Message::default();
            while win32::GetMessageW(&mut message, 0, 0, 0) > 0 {
                if message.message != win32::WM_HOTKEY {
                    continue;
                }
                let kind = if message.w_param == win32::HOTKEY_ID_LIGHTBAR_HITCH as usize {
                    "lightbar"
                } else if message.w_param == win32::HOTKEY_ID_INPUT_HITCH as usize {
                    "input"
                } else {
                    continue;
                };
                // Stamp immediately on this thread — do not wait for iced.
                stamp_hitch_report(kind, "hotkey", None);
            }

            win32::UnregisterHotKey(0, win32::HOTKEY_ID_LIGHTBAR_HITCH);
            win32::UnregisterHotKey(0, win32::HOTKEY_ID_INPUT_HITCH);
        });
    });
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
    hid_worker: HidWorkerHandle,
    low_battery: Arc<Mutex<Vec<(String, u8)>>>,
) {
    thread::spawn(move || {
        let on = Duration::from_millis(LOW_BATTERY_PULSE_ON_MS);
        let gap = Duration::from_millis(LOW_BATTERY_PULSE_GAP_MS);
        let identifying = hid_worker.identifying();

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

                hid_worker.set_rgb(serial.clone(), LOW_BATTERY_ORANGE);
                thread::sleep(on);

                if identifying.load(Ordering::SeqCst) || !lightbar::is_enabled() {
                    break;
                }

                let color = color_for_battery_percent(percent);
                hid_worker.set_rgb(serial, color);
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
    previous_empty: bool,
    next_nonempty: bool,
    already_open: bool,
    cooldown_active: bool,
    fullscreen: bool,
) -> bool {
    enabled && previous_empty && next_nonempty && !already_open && !cooldown_active && !fullscreen
}

#[cfg(test)]
mod start_gate_tests {
    use super::should_auto_open_start;

    #[test]
    fn opens_on_clean_zero_to_one() {
        assert!(should_auto_open_start(
            true, true, true, false, false, false
        ));
    }

    #[test]
    fn skips_when_disabled_empty_open_cooldown_or_fullscreen() {
        assert!(!should_auto_open_start(
            false, true, true, false, false, false
        ));
        assert!(!should_auto_open_start(
            true, false, true, false, false, false
        ));
        assert!(!should_auto_open_start(
            true, true, true, true, false, false
        ));
        assert!(!should_auto_open_start(
            true, true, true, false, true, false
        ));
        assert!(!should_auto_open_start(
            true, true, true, false, false, true
        ));
    }
}
