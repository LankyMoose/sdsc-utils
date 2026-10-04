//! iced `daemon` shell for the DualSense battery tray app.
//!
//! The daemon boots windowless: it owns the tray icon and only opens windows on
//! demand (controller popup, configure window, overlay toast). The toast window
//! is pre-created hidden after the tray appears so iced keeps a warm GPU
//! compositor; popup, Settings, and Start stay ephemeral (create/destroy on each
//! open — Start uses a normal OS window open).

use crate::controller::dualsense::identity as dualsense;
use crate::controller::dualsense::lightbar;
use crate::controller::dualsense::rumble::MotorPulse;
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
use crate::games::process_match::{self, RunningSession};
use crate::games::steam::{self, SteamGame};
use crate::games::{self, GamesCatalog};
use crate::persist::analytics::{self, AnalyticsStore};
use crate::persist::notify::NotifyEvent;
use crate::persist::paths;
use crate::persist::prefs::{
    Prefs, clamp_low_battery_percent, clamp_start_screen_haptics_strength,
    clamp_start_screen_sound_volume,
};
use crate::platform::app_log;
#[cfg(windows)]
use crate::platform::autostart;
use crate::platform::crash_restart;
use crate::platform::ui_sound::{self, UiSoundKind};
#[cfg(windows)]
use crate::platform::win32;
use crate::ui::color::{self, BatterySpectrum};
use crate::ui::configure::{
    self as configure_view, AnalyticsPadRow, AnalyticsPanel, ConfigureMessage, ConfigureSettings,
    ConfigureState, NotificationSetting, PadInputPanel, Section,
};
use crate::ui::layout::{
    MonitorCover, ToastPlacement, TrayAnchor, cursor_on_primary_monitor, hide_toast,
    invalidate_toast, overlay_platform_specific, popup_position, primary_monitor_cover,
    raise_window_topmost, remount_toast_surface, set_toast_topmost, show_toast_without_activate,
    slide_y, toast_local_in_cover, toast_placement, window_hwnd, window_platform_specific,
};
use crate::ui::popup::{self as popup_view, ControllerRow, PopupMessage};
use crate::ui::start::cursor_hide;
use crate::ui::start::gesture::{self, ChordReleaseGate, ChordReleaseTick, GestureRecorder};
use crate::ui::start::input::{
    self as start_input, CrossHold, FaceHeld, GestureDetectorBank, GestureRecordLatch, NavAction,
    NavLogSnapshot, NavSource, PadNavBank,
};
use crate::ui::start::mode::{self as start_mode, StartPresentation};
use crate::ui::start::view::{self as start_view, ReplaceConfirm, StartMessage, StartSlide};
use crate::ui::theme;
use crate::ui::toast::ToastMessage;
use crate::ui::toast::machine::{self as toast_machine, AfterToast, Effect as ToastEffect};
use crate::ui::toast::view as toast_view;
use crate::ui::tray::{self, QUIT_ID, SETTINGS_ID};

use iced::futures::SinkExt;
use iced::futures::Stream;
use iced::futures::StreamExt;
use iced::futures::channel::{mpsc, oneshot};
use iced::keyboard;
use iced::mouse;
use iced::widget::{Float, container, operation, space, stack};
use iced::{Element, Fill, Point, Size, Subscription, Task, Theme, Vector, stream, window};

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tray_icon::TrayIcon;

/// Debounce spectrum prefs save + HID apply after the last color edit.
const SPECTRUM_DEBOUNCE: Duration = Duration::from_millis(150);
/// How long an overlay toast stays on screen.
const TOAST_LIFETIME: Duration = Duration::from_secs(5);
/// DualSense poll rate while pad input is live (~one wired report).
#[allow(dead_code)] // documentation / parity with worker ACTIVE_POLL
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
/// Ignore Start WindowUnfocused briefly after reveal (toast z-order focus blips).
const START_UNFOCUS_GRACE: Duration = Duration::from_millis(150);
/// Hide the mouse cursor after this idle while immersive Start covers the primary.
const START_CURSOR_IDLE_HIDE: Duration = Duration::from_secs(2);

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
    /// Mouse move/button/wheel on a window — used to reset immersive cursor idle hide.
    StartCursorActivity(window::Id),
    /// HWND resize finished — apply immersive flag after promote/demote veil.
    StartImmersiveSettled,
    /// Background immersive art warm finished — peek cache + fade-in.
    ImmersiveArtReady,
    Start(StartMessage),
    /// Keyboard while some iced window has focus; filtered to the start screen in update.
    StartKey {
        id: window::Id,
        action: StartKeyAction,
        pressed: bool,
    },
    /// Pad input edge from the hid-worker (report-rate; replaces UI PadPoll timer).
    PadInput(crate::domain::pad::InputEdge),
    /// Client mode: reconcile shell-side pad-input hot flag with the service.
    ClientIpcSync,
    /// Message from the HID/tray service (shell-client mode).
    Service(crate::ipc::ServiceMessage),
    ManualFilePicked(Option<PathBuf>),
    ManualIconPicked(Option<PathBuf>),
    SteamScanDone(Result<Vec<SteamGame>, String>),

    PlaceToast {
        id: window::Id,
        generation: u64,
    },
    /// Toast HWND is shown at outside_y; start the slide-in clock.
    ToastShown {
        generation: u64,
    },
    /// Advance the active toast presentation machine.
    ToastFrame,
    /// Redraw the controller popup while an Identify ring flash is running.
    IdentifyFrame,
    /// Begin dismiss (slide-out) for the toast of the given generation.
    ToastDismiss(u64),
    /// Hide toast HWND after a short defer (avoid create_renderer vs hide stress).
    ToastHideDeferred,

    /// Debounced spectrum prefs save + lightbar HID apply.
    SpectrumCommit(u64),

    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartKeyAction {
    Up,
    Down,
    Left,
    Right,
    Confirm,
    Cancel,
}

pub struct App {
    session: crate::session::DeviceSession,
    games: GamesCatalog,
    /// In-progress start-screen edit checklist; committed on Save, discarded on Cancel.
    edit_draft: Option<GamesCatalog>,

    /// Last HID presence snapshot (serials), used to detect connect/disconnect.
    last_discovered: Vec<String>,
    last_battery_poll: Instant,

    /// True while an identify flash sequence is running (pulse should yield).
    identifying: Arc<AtomicBool>,
    /// True while a background battery poll is in flight.
    refreshing: Arc<AtomicBool>,
    /// Serializes DualSense HID for the daemon (never on the UI thread).
    /// `None` in shell-client mode (service owns HID).
    hid_worker: Option<HidWorkerHandle>,
    /// True when this process is the iced shell attached to the service.
    client_mode: bool,
    /// Last [`ShellCommand::SetInputHot`] sent to the service (client mode).
    client_input_hot: bool,

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
    /// True only while Start is revealed (warm HWND may exist while false).
    start_visible: bool,
    /// False until Start is revealed and ready — blocks pad nav / sounds early.
    start_nav_ready: bool,
    start_state: start_view::State,
    /// Monitor covered while immersive (used to center compact on demote).
    start_monitor_cover: Option<MonitorCover>,
    /// Last mouse activity on the Start window (immersive idle cursor hide).
    start_cursor_last_active: Instant,
    /// When true, Start view forces [`mouse::Interaction::Hidden`].
    start_cursor_hidden: bool,
    /// True while an rfd picker is open from the start screen (suppress unfocus-close).
    start_file_dialog_open: bool,
    /// After open / hide / promote: ignore reopen until a debounced full release.
    chord_release_gate: ChordReleaseGate,
    /// When Start was last revealed (unfocus grace).
    start_revealed_at: Option<Instant>,
    gesture_detectors: GestureDetectorBank,
    gesture_recorder: GestureRecorder,
    /// Latch Settings gesture recording to one pad (no cross-pad union).
    gesture_record_latch: GestureRecordLatch,
    pad_nav: PadNavBank,
    /// When set, the next start cue may rumble this DualSense serial (pad-originated).
    haptic_pad_serial: Option<String>,
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
    /// True until the first `SteamScanDone` (Ok or Err). Drives Games-list skeleton rows.
    steam_scan_pending: bool,
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
    /// Last frame Instant for capped slide dt (set on ToastShown / dismiss / each frame).
    toast_anim_started: Instant,
    /// Pure toast / deferred-Start presentation machine.
    toast_machine: toast_machine::State,

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

/// Iced shell attached to the HID/tray service (no local DualSense HID owner).
pub fn run_shell_client() -> Result<(), Box<dyn std::error::Error>> {
    crate::platform::app_log::info("shell: client mode (service owns HID + tray)");
    // Wait briefly for the service pipe to come up.
    for _ in 0..50 {
        if crate::ipc::PipeClient::connect().is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    run_app_as_client()
}

fn run_app(
    #[cfg(feature = "dev-emulate")] dev_mode: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    crate::platform::wgpu_diag::log_adapters_at_boot();

    #[cfg(feature = "dev-emulate")]
    let boot = move || App::boot(dev_mode, false);
    #[cfg(not(feature = "dev-emulate"))]
    let boot = || App::boot(false);

    iced::daemon(boot, App::update, App::view)
        .subscription(App::subscription)
        .title(App::title)
        .theme(App::theme)
        .antialiasing(true)
        .run()?;

    Ok(())
}

fn run_app_as_client() -> Result<(), Box<dyn std::error::Error>> {
    crate::platform::wgpu_diag::log_adapters_at_boot();
    let boot = || App::boot(true);
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
    fn boot(
        #[cfg(feature = "dev-emulate")] dev_mode: bool,
        client_mode: bool,
    ) -> (Self, Task<Message>) {
        let prefs = Prefs::load();
        let known = KnownControllers::load();
        let analytics = AnalyticsStore::load();
        let games = GamesCatalog::load();
        color::set_active_spectrum(prefs.spectrum.clone());
        lightbar::set_enabled(prefs.lightbar_enabled);

        #[cfg(windows)]
        if !client_mode {
            autostart::ensure_quiet_entry();
        }

        let (hid_worker, identifying) = if client_mode {
            (None, Arc::new(AtomicBool::new(false)))
        } else {
            let worker = HidWorkerHandle::start();
            let identifying = worker.identifying();
            (Some(worker), identifying)
        };

        let configure_state = ConfigureState::new(prefs.spectrum.clone());
        let session = crate::session::DeviceSession::new(prefs, known, analytics);

        let last_discovered = hid_worker
            .as_ref()
            .map(|w| w.presence_paths())
            .unwrap_or_default();
        let mut app = Self {
            session,
            games,
            edit_draft: None,
            last_discovered,
            last_battery_poll: Instant::now(),
            identifying,
            refreshing: Arc::new(AtomicBool::new(false)),
            hid_worker,
            client_mode,
            client_input_hot: false,
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
            start_visible: false,
            start_nav_ready: false,
            start_state: start_view::State::default(),
            start_monitor_cover: None,
            start_cursor_last_active: Instant::now(),
            start_cursor_hidden: false,
            start_file_dialog_open: false,
            chord_release_gate: ChordReleaseGate::default(),
            start_revealed_at: None,
            gesture_detectors: GestureDetectorBank::default(),
            gesture_recorder: GestureRecorder::default(),
            gesture_record_latch: GestureRecordLatch::default(),
            pad_nav: PadNavBank::default(),
            haptic_pad_serial: None,
            keyboard_cross_hold: CrossHold::default(),
            confirm_key_held: false,
            cancel_key_held: false,
            pad_held: FaceHeld::default(),
            running_session: None,
            last_running_check: None,
            process_enum_inflight: false,
            match_path_cache: HashMap::new(),
            steam_by_id: HashMap::new(),
            steam_scan_pending: true,
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
            toast_machine: toast_machine::State::Idle,
            spectrum_generation: 0,
            #[cfg(feature = "dev-emulate")]
            dev_mode,
            #[cfg(feature = "dev-emulate")]
            emulating: false,
            #[cfg(feature = "dev-emulate")]
            dev_paused_percent: None,
        };

        // Show the tray immediately (standalone only — service owns the tray in client mode).
        if !client_mode {
            app.create_tray();
        }
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
                        keyboard::Key::Named(keyboard::key::Named::ArrowLeft) => {
                            Some(StartKeyAction::Left)
                        }
                        keyboard::Key::Named(keyboard::key::Named::ArrowRight) => {
                            Some(StartKeyAction::Right)
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
                iced::Event::Mouse(
                    mouse::Event::CursorMoved { .. }
                    | mouse::Event::ButtonPressed(_)
                    | mouse::Event::ButtonReleased(_)
                    | mouse::Event::WheelScrolled { .. },
                ) => Some(Message::StartCursorActivity(id)),
                _ => None,
            }),
        ];

        if !self.client_mode {
            subscriptions.push(
                iced::time::every(
                    if self.session.prefs.start_screen_enabled
                        && self.session.controllers.is_empty()
                    {
                        PRESENCE_INTERVAL_EMPTY
                    } else {
                        PRESENCE_INTERVAL
                    },
                )
                .map(|_| Message::Tick),
            );
            subscriptions.push(Subscription::run(tray_events_mapped));
        } else {
            subscriptions.push(Subscription::run(service_message_stream));
            subscriptions.push(
                iced::time::every(Duration::from_millis(250)).map(|_| Message::ClientIpcSync),
            );
        }

        if self.toast_message.is_some() {
            subscriptions.push(Subscription::run_with(
                self.toast_generation,
                |generation| escape_hotkey(*generation),
            ));
        }

        if self.toast_message.is_some() || self.toast_machine.wants_frames() {
            // Tick for the whole toast lifetime so content swaps present, and while
            // Placing so the placement failsafe can fire without a Shown event.
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::ToastFrame));
        }

        if self.popup_state.identify_flash_active() || self.start_state.identify_flash_active() {
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::IdentifyFrame));
        }

        if !self.client_mode {
            let pad_input_live = self.configure_window.is_some()
                && self.configure_state.section == Section::PadInput;
            let pad_listening = pad_input_live
                || (self.session.prefs.start_screen_enabled
                    && (!self.session.controllers.is_empty() || self.gesture_recorder.is_active()));
            if let Some(w) = self.hid_worker.as_ref() {
                w.set_input_hot(pad_listening);
            }
            if pad_listening {
                subscriptions.push(Subscription::run(pad_input_edge_stream));
            }
        }
        // Client mode: reopen gesture stays on the service; pad edges only when
        // Start / Settings pad-input / gesture record need them (see shell_wants_pad_input).

        if self.start_visible && self.start_state.needs_frames() {
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::StartFrame));
        }

        Subscription::batch(subscriptions)
    }

    fn view(&self, window: window::Id) -> Element<'_, Message> {
        if Some(window) == self.popup_window {
            return popup_view::view(
                &self.popup_state,
                &self.popup_rows,
                &self.session.prefs.spectrum,
            )
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
            let stage_h = self.start_monitor_cover.map(|c| c.height).unwrap_or(1080.0);
            let start = start_view::view(
                &self.start_state,
                &self.session.prefs.spectrum,
                Instant::now(),
                self.session.prefs.start_screen_always_immersive,
                &self.session.prefs.start_screen_gesture,
                stage_h,
            )
            .map(Message::Start);
            let start = match self.immersive_toast_overlay() {
                Some(toast) => stack![start, toast].width(Fill).height(Fill).into(),
                None => start,
            };
            return cursor_hide::force_cursor(start, self.start_cursor_hidden).into();
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
                    self.start_visible = false;
                    self.report_start_visible(false);
                    self.sync_client_input_hot();
                    self.start_nav_ready = false;
                    self.pad_nav.reset();
                    self.clear_start_nav_diag();
                    self.consume_reopen_gesture_chord();
                    let sync = self.sync_toast_zorder();
                    if self.session.start_auto_open_pending
                        && self.session.prefs.start_screen_enabled
                        && crate::session::has_start_presence(
                            &self.session.controllers,
                            self.session.prefs.start_screen_usb_controllers,
                        )
                    {
                        crate::controller::hid::diag::diag_info(
                            "ui-diag: retry start open after close",
                        );
                        sync.chain(self.open_start_screen())
                    } else {
                        sync
                    }
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
                } else if Some(id) == self.start_window && self.start_visible {
                    // File dialogs (add/edit shortcut, choose image) steal focus — keep the
                    // start screen open so the modal / draft is not discarded.
                    if self.start_state.manual_add.is_some() || self.start_file_dialog_open {
                        Task::none()
                    } else if self
                        .start_revealed_at
                        .is_some_and(|t| t.elapsed() < START_UNFOCUS_GRACE)
                    {
                        crate::controller::hid::diag::diag_info(
                            "ui-diag: start unfocus skipped (reveal grace)",
                        );
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
                if Some(id) == self.start_window && self.start_visible {
                    self.start_nav_ready = true;
                    self.nav_not_ready_warned = false;
                    self.start_revealed_at = Some(Instant::now());
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: start opened id={id:?}"
                    ));
                    // Raise/focus after create_renderer has a chance to finish —
                    // chaining sync on the same turn as window::open correlated
                    // with atlas create_renderer panics.
                    let mut task = window::gain_focus(id)
                        .chain(raise_window_topmost(id))
                        .chain(self.sync_toast_zorder());
                    if self.start_state.immersive {
                        task = task.chain(self.warm_immersive_art_task());
                    }
                    task
                } else {
                    Task::none()
                }
            }
            Message::StartImmersiveSettled => self.finish_start_immersive_transition(),
            Message::ImmersiveArtReady => {
                let now = Instant::now();
                if self.start_state.immersive {
                    if self
                        .start_state
                        .rows
                        .get(self.start_state.game_selected)
                        .is_some_and(|r| r.backdrop_icon().is_some())
                    {
                        self.start_state.note_backdrop_ready(now);
                    }
                    crate::controller::hid::diag::diag_info("ui-diag: immersive art warm done");
                }
                Task::none()
            }
            Message::StartFrame => {
                let now = Instant::now();
                let _ = self.start_state.tick_anim(now);
                if let Some(task) = self.tick_immersive_transition_phase(now) {
                    return task;
                }
                // Keyboard-only hold progress when pad poll is not running.
                if self.start_state.replace_confirm.is_some() {
                    let (progress, completed) =
                        self.keyboard_cross_hold.update(self.confirm_key_held, now);
                    self.start_state.cross_progress = self.start_state.cross_progress.max(progress);
                    if completed {
                        self.haptic_pad_serial = None;
                        self.play_start_cue(UiSoundKind::Hold);
                        return self.complete_replace_confirm();
                    }
                }
                self.sync_start_held();
                self.start_state.tick_hint_anims(now);
                self.tick_start_cursor_idle(now);
                Task::none()
            }
            Message::StartCursorActivity(id) => {
                if Some(id) == self.start_window && self.start_visible {
                    self.note_start_cursor_activity();
                }
                Task::none()
            }
            Message::Start(message) => {
                // Mouse / iced UI messages are not pad-originated.
                self.haptic_pad_serial = None;
                self.on_start_message(message)
            }
            Message::StartKey {
                id,
                action,
                pressed,
            } => {
                // Keyboard is never pad-originated.
                self.haptic_pad_serial = None;
                if Some(id) == self.start_window && self.start_visible {
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
                        StartKeyAction::Left if pressed && self.start_state.immersive => {
                            self.cancel_start_holds();
                            self.on_start_message(StartMessage::PrevSlide)
                        }
                        StartKeyAction::Right if pressed && self.start_state.immersive => {
                            self.cancel_start_holds();
                            self.on_start_message(StartMessage::NextSlide)
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
            Message::PadInput(edge) => self.on_pad_input(edge),
            Message::ClientIpcSync => {
                self.sync_client_input_hot();
                Task::none()
            }
            Message::Service(msg) => self.on_service_message(msg),
            Message::ManualFilePicked(path) => self.on_manual_file_picked(path),
            Message::ManualIconPicked(path) => self.on_manual_icon_picked(path),
            Message::SteamScanDone(result) => self.on_steam_scan_done(result),

            Message::PlaceToast { id, generation } => {
                if generation != self.toast_generation || self.toast_message.is_none() {
                    return Task::none();
                }
                // Sync placement (Win32 work area on Windows); no monitor_size hop.
                let placement = toast_placement(self.session.prefs.toast_position, None);
                self.toast_placement = Some(placement);
                let start = Point::new(placement.x, placement.outside_y);
                let percent = self
                    .toast_message
                    .as_ref()
                    .map(|m| m.percent())
                    .unwrap_or(0);
                let body = self
                    .toast_message
                    .as_ref()
                    .map(|m| m.body.as_str())
                    .unwrap_or("-");
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: place toast gen={generation} body={body} percent={percent}"
                ));
                crate::platform::wgpu_diag::note_toast_remount();
                // Do not raise interactive UI here — machine emits RaiseInteractive
                // only after Resting so Start create cannot starve the slide.
                remount_toast_surface(id, generation)
                    .chain(window::move_to(id, start))
                    .chain(window::set_level(id, window::Level::AlwaysOnTop))
                    .chain(show_toast_without_activate(id))
                    .chain(invalidate_toast(id))
                    .chain(Task::done(Message::ToastShown { generation }))
            }
            Message::ToastShown { generation } => {
                self.toast_step(toast_machine::Event::Shown { generation })
            }
            Message::ToastFrame => {
                let Some(generation) = self.toast_machine.generation() else {
                    return Task::none();
                };
                let now = Instant::now();
                let raw_dt = now.saturating_duration_since(self.toast_anim_started);
                self.toast_anim_started = now;
                self.toast_step(toast_machine::Event::Frame { generation, raw_dt })
            }
            Message::IdentifyFrame => {
                self.popup_state.tick_identify_flash();
                self.start_state.tick_identify_flash();
                Task::none()
            }
            Message::ToastDismiss(generation) => {
                self.toast_step(toast_machine::Event::Dismiss { generation })
            }
            Message::ToastHideDeferred => {
                if self.toast_message.is_some() || !self.toast_queue.is_empty() {
                    return Task::none();
                }
                match self.toast_window {
                    Some(id) => {
                        crate::controller::hid::diag::diag_info("ui-diag: toast hide (deferred)");
                        crate::platform::wgpu_diag::note_toast_hide();
                        hide_toast(id)
                    }
                    None => Task::none(),
                }
            }

            Message::SpectrumCommit(generation) => self.on_spectrum_commit(generation),

            Message::Exit => {
                self.session.known.save();
                if self.client_mode {
                    crate::ipc::clear_command_client();
                } else {
                    if let Some(w) = self.hid_worker.as_ref() {
                        w.shutdown();
                    }
                }
                self.tray_icon.take();
                iced::exit()
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tray
    // -----------------------------------------------------------------------

    fn create_tray(&mut self) {
        self.tray_icon = tray::create_tray(&self.session.controllers);
    }

    fn apply_tray(&mut self) -> Task<Message> {
        if let Some(tray_icon) = self.tray_icon.as_mut() {
            tray::apply_tray(tray_icon, &self.session.controllers);
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

        let discovered = self
            .hid_worker
            .as_ref()
            .map(|w| w.presence_paths())
            .unwrap_or_default();

        let membership_changed = discovered != self.last_discovered;
        self.last_discovered = discovered;

        // HID list went empty — update the tray immediately. A powered-off DualSense
        // often disappears from enumeration well before the next battery poll.
        if membership_changed && self.last_discovered.is_empty() {
            // Presence-only clear never runs HID poll; drop claims so reconnect
            // always gets LIGHT_OUT + RGB. Skip the one-miss hold — the device is gone.
            lightbar::sync_lightbar_claims(std::iter::empty::<&str>());
            self.session.clear_missed_polls();
            let task = if self.session.controllers.is_empty() {
                Task::none()
            } else {
                self.apply_controllers(Vec::new())
            };
            self.last_battery_poll = Instant::now();
            return task;
        }

        let battery_due = self.last_battery_poll.elapsed() >= BATTERY_INTERVAL;
        let liveness_due = !self.session.controllers.is_empty()
            && self.last_battery_poll.elapsed() >= LIVENESS_INTERVAL;
        // HID can list a pad that we cannot open/read yet (sleeping BT, exclusive access).
        // While the tray is empty, retry on the fast presence cadence so 0→1 open is snappy.
        let unread_retry = !self.last_discovered.is_empty()
            && self.session.controllers.is_empty()
            && self.last_battery_poll.elapsed()
                >= if self.session.prefs.start_screen_enabled {
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

        let previously_connected: Vec<String> = self
            .session
            .controllers
            .iter()
            .map(|c| c.serial.clone())
            .collect();
        let Some(worker) = self.hid_worker.clone() else {
            self.refreshing.store(false, Ordering::SeqCst);
            return Task::none();
        };
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
            Ok(controllers) => {
                if let Some(worker) = self.hid_worker.as_ref() {
                    self.session
                        .notify
                        .queue_launch_quiet(worker.take_launch_serials());
                }
                let ctx = crate::session::ApplyContext {
                    start_visible: self.start_visible,
                    fullscreen: start_input::foreground_is_exclusive_fullscreen(),
                    now: Instant::now(),
                    lightbar_enabled: lightbar::is_enabled(),
                };
                let effects = self.session.on_poll_result(controllers, ctx);
                self.apply_session_effects(effects)
            }
            Err(err) => {
                app_log::warn(format!("refresh failed: {err}"));
                Task::none()
            }
        }
    }

    fn apply_session_effects(
        &mut self,
        effects: Vec<crate::session::SessionEffect>,
    ) -> Task<Message> {
        use crate::session::SessionEffect;
        let mut tasks = Vec::new();
        for effect in effects {
            match effect {
                SessionEffect::SetTray { connected: _ } => {
                    if !self.client_mode {
                        tasks.push(self.apply_tray());
                    } else {
                        tasks.push(self.sync_popup_rows_and_fit());
                    }
                }
                SessionEffect::SetLowBatteryTargets { targets } => {
                    if self.client_mode {
                        let _ = crate::ipc::send_command(
                            &crate::ipc::ShellCommand::SetLowBatteryTargets { targets },
                        );
                    } else {
                        if let Some(w) = self.hid_worker.as_ref() {
                            w.set_low_battery_targets(targets);
                        }
                    }
                }
                SessionEffect::QueueNotifications {
                    events,
                    open_start_after_toast,
                } => {
                    tasks.push(self.queue_notifications(events, open_start_after_toast));
                }
                SessionEffect::OpenStart => {
                    // Service may fire the reopen chord during the connect-toast
                    // latch; the toast's own AfterToast::OpenStart still opens Start.
                    if self.toast_machine.suppresses_reopen_gesture() {
                        crate::controller::hid::diag::diag_info(
                            "ui-diag: service OpenStart ignored (toast OpenStart pending)",
                        );
                    } else {
                        tasks.push(self.open_start_screen());
                    }
                }
                SessionEffect::CloseStart => {
                    tasks.push(self.close_start_screen());
                }
                SessionEffect::ClearStartLatch => {
                    self.toast_machine.clear_after();
                }
                SessionEffect::SaveKnown => {
                    if !self.client_mode {
                        self.session.known.save();
                    }
                }
                SessionEffect::SaveAnalytics => {
                    if !self.client_mode {
                        self.session.analytics.save();
                    }
                }
                SessionEffect::RefreshAnalyticsPanel => {
                    if self.client_mode {
                        self.session.analytics = AnalyticsStore::load();
                    }
                    self.refresh_analytics_panel();
                }
                SessionEffect::SyncPopup => {
                    tasks.push(self.sync_popup_rows_and_fit());
                }
            }
        }
        Task::batch(tasks)
    }

    fn report_start_visible(&self, visible: bool) {
        if self.client_mode {
            let _ =
                crate::ipc::send_command(&crate::ipc::ShellCommand::ReportStartVisible { visible });
        }
    }

    /// True when the shell (not the service) should receive live pad-input edges.
    fn shell_wants_pad_input(&self) -> bool {
        let pad_input_live =
            self.configure_window.is_some() && self.configure_state.section == Section::PadInput;
        pad_input_live
            || self.gesture_recorder.is_active()
            || (self.start_visible
                && self.session.prefs.start_screen_enabled
                && !self.session.controllers.is_empty())
    }

    fn sync_client_input_hot(&mut self) {
        if !self.client_mode {
            return;
        }
        let hot = self.shell_wants_pad_input();
        if self.client_input_hot == hot {
            return;
        }
        self.client_input_hot = hot;
        if crate::ipc::send_command(&crate::ipc::ShellCommand::SetInputHot { hot }).is_err() {
            self.client_input_hot = !hot;
        }
    }

    /// Apply a controller snapshot through the session (used by presence clear + emulate).
    fn apply_controllers(&mut self, controllers: Vec<ControllerStatus>) -> Task<Message> {
        let ctx = crate::session::ApplyContext {
            start_visible: self.start_visible,
            fullscreen: start_input::foreground_is_exclusive_fullscreen(),
            now: Instant::now(),
            lightbar_enabled: lightbar::is_enabled(),
        };
        let effects = self.session.apply_controllers(controllers, ctx);
        self.apply_session_effects(effects)
    }

    fn sync_low_battery(&mut self) {
        if let Some(effect) = self.session.refresh_low_battery() {
            let _ = self.apply_session_effects(vec![effect]);
        }
    }

    // -----------------------------------------------------------------------
    // Popup window
    // -----------------------------------------------------------------------

    fn sync_popup_rows(&mut self) {
        let threshold = self.session.prefs.low_battery_percent;
        let mut rows = Vec::new();
        for controller in &self.session.controllers {
            let eta = if self.session.prefs.analytics_enabled {
                self.session
                    .analytics
                    .eta_for(controller)
                    .map(analytics::format_eta_ring)
            } else {
                None
            };
            rows.push(ControllerRow::connected(
                controller,
                self.session.known.is_remembered(&controller.serial),
                dualsense::is_storable_serial(&controller.serial)
                    && !is_emulated_serial(&controller.serial),
                self.session
                    .known
                    .nickname(&controller.serial)
                    .map(str::to_string),
                threshold,
                eta,
            ));
        }
        for controller in self
            .session
            .known
            .remembered_disconnected(&self.session.controllers)
        {
            let nickname = self
                .session
                .known
                .nickname(&controller.serial)
                .map(str::to_string);
            let eta = if self.session.prefs.analytics_enabled {
                self.session
                    .analytics
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
                let current = self.session.known.nickname(&serial).map(str::to_string);
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
            notify_low: self.session.prefs.notify_low,
            notify_charged: self.session.prefs.notify_charged,
            notify_connect: self.session.prefs.notify_connect,
            notify_disconnect: self.session.prefs.notify_disconnect,
            low_battery_percent: self.session.prefs.low_battery_percent,
            toast_position: self.session.prefs.toast_position,
            analytics_enabled: self.session.prefs.analytics_enabled,
            lightbar_enabled: self.session.prefs.lightbar_enabled,
            start_screen_enabled: self.session.prefs.start_screen_enabled,
            start_screen_always_immersive: self.session.prefs.start_screen_always_immersive,
            start_screen_usb_controllers: self.session.prefs.start_screen_usb_controllers,
            start_screen_gesture: self.session.prefs.start_screen_gesture.clone(),
            start_screen_sounds_enabled: self.session.prefs.start_screen_sounds_enabled,
            start_screen_sound_volume: self.session.prefs.start_screen_sound_volume,
            start_screen_haptics_enabled: self.session.prefs.start_screen_haptics_enabled,
            start_screen_haptics_strength: self.session.prefs.start_screen_haptics_strength,
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
        if !self.session.prefs.analytics_enabled {
            return AnalyticsPanel::default();
        }
        let rows = self
            .session
            .analytics
            .panel_rows()
            .iter()
            .map(|row| {
                let label = self
                    .session
                    .known
                    .nickname(&row.serial)
                    .map(str::to_string)
                    .filter(|name| !name.is_empty())
                    .or_else(|| {
                        self.session
                            .controllers
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
            .set_spectrum(self.session.prefs.spectrum.clone());

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
                    NotificationSetting::Connect => self.session.prefs.notify_connect = enabled,
                    NotificationSetting::Disconnect => {
                        self.session.prefs.notify_disconnect = enabled
                    }
                    NotificationSetting::Low => self.session.prefs.notify_low = enabled,
                    NotificationSetting::Charged => self.session.prefs.notify_charged = enabled,
                }
                self.session.prefs.save();
                Task::none()
            }
            ConfigureMessage::SetLowBatteryPercent(percent) => {
                let percent = clamp_low_battery_percent(percent);
                if self.session.prefs.low_battery_percent != percent {
                    self.session.prefs.low_battery_percent = percent;
                    self.session.prefs.save();
                    self.sync_low_battery();
                    return self.sync_popup_rows_and_fit();
                }
                Task::none()
            }
            ConfigureMessage::SetToastPosition(position) => {
                self.session.prefs.toast_position = position;
                self.session.prefs.save();
                self.show_position_preview()
            }
            ConfigureMessage::SetAnalyticsEnabled(enabled) => {
                self.session.prefs.analytics_enabled = enabled;
                self.session.prefs.save();
                self.refresh_analytics_panel();
                self.sync_popup_rows_and_fit()
            }
            ConfigureMessage::SetLightbarEnabled(enabled) => {
                self.session.prefs.lightbar_enabled = enabled;
                self.session.prefs.save();
                lightbar::set_enabled(enabled);
                self.sync_low_battery();
                if enabled {
                    // Re-apply spectrum colors now that automatic writes are back on.
                    self.apply_spectrum(self.session.prefs.spectrum.clone())
                } else {
                    Task::none()
                }
            }
            ConfigureMessage::SetStartScreenEnabled(enabled) => {
                self.session.prefs.start_screen_enabled = enabled;
                self.session.prefs.save();
                if !enabled {
                    self.cancel_gesture_recording();
                    if self.start_visible {
                        return self.close_start_screen();
                    }
                }
                Task::none()
            }
            ConfigureMessage::SetStartScreenAlwaysImmersive(enabled) => {
                self.session.prefs.start_screen_always_immersive = enabled;
                self.session.prefs.save();
                Task::none()
            }
            ConfigureMessage::SetStartScreenUsbControllers(enabled) => {
                if self.session.prefs.start_screen_usb_controllers == enabled {
                    return Task::none();
                }
                self.session.prefs.start_screen_usb_controllers = enabled;
                self.session.prefs.save();
                // Client mode: service also reloads prefs within ~2s and may re-emit
                // OpenStart/CloseStart; shell applies immediately for snappy UI.
                let ctx = crate::session::ApplyContext {
                    start_visible: self.start_visible,
                    fullscreen: start_input::foreground_is_exclusive_fullscreen(),
                    now: Instant::now(),
                    lightbar_enabled: lightbar::is_enabled(),
                };
                let effects = self.session.reevaluate_start_presence(ctx);
                self.apply_session_effects(effects)
            }
            ConfigureMessage::SetStartScreenSounds(enabled) => {
                self.session.prefs.start_screen_sounds_enabled = enabled;
                self.session.prefs.save();
                if enabled {
                    self.play_start_sound(UiSoundKind::Nav);
                }
                Task::none()
            }
            ConfigureMessage::SetStartScreenSoundVolume(volume) => {
                let volume = clamp_start_screen_sound_volume(volume);
                if self.session.prefs.start_screen_sound_volume != volume {
                    self.session.prefs.start_screen_sound_volume = volume;
                    self.session.prefs.save();
                    if self.session.prefs.start_screen_sounds_enabled {
                        self.play_start_sound(UiSoundKind::Nav);
                    }
                }
                Task::none()
            }
            ConfigureMessage::SetStartScreenHaptics(enabled) => {
                self.session.prefs.start_screen_haptics_enabled = enabled;
                self.session.prefs.save();
                if enabled {
                    self.preview_haptic_all(MotorPulse::NAV);
                } else if self.client_mode {
                    let _ = crate::ipc::send_command(&crate::ipc::ShellCommand::RumbleStopAll);
                } else {
                    if let Some(w) = self.hid_worker.as_ref() {
                        w.rumble_stop_all();
                    }
                }
                Task::none()
            }
            ConfigureMessage::SetStartScreenHapticsStrength(strength) => {
                let strength = clamp_start_screen_haptics_strength(strength);
                if self.session.prefs.start_screen_haptics_strength != strength {
                    self.session.prefs.start_screen_haptics_strength = strength;
                    self.session.prefs.save();
                    if self.session.prefs.start_screen_haptics_enabled {
                        self.preview_haptic_all(MotorPulse::NAV);
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
                self.session.prefs.start_screen_gesture = gesture::default_gesture();
                self.session.prefs.save();
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
            ConfigureMessage::OpenExternalLink(url) => {
                if let Err(err) = launch::launch_target(url, "") {
                    app_log::warn(format!("open external link failed: {err}"));
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
        self.session.prefs.spectrum = spectrum.clone();
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

        self.session.prefs.save();

        if !lightbar::is_enabled() {
            return Task::none();
        }

        let targets: Vec<(String, color::Rgb)> = self
            .session
            .controllers
            .iter()
            .filter(|c| !is_emulated_serial(&c.serial))
            .map(|c| {
                (
                    c.serial.clone(),
                    self.session.prefs.spectrum.color_at_percent(c.percent),
                )
            })
            .collect();

        if targets.is_empty() {
            return Task::none();
        }

        for (serial, color) in targets {
            if self.client_mode {
                let _ =
                    crate::ipc::send_command(&crate::ipc::ShellCommand::SetRgb { serial, color });
            } else {
                if let Some(w) = self.hid_worker.as_ref() {
                    w.set_rgb(serial, color);
                }
            }
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

        let Some(controller) = self.session.controllers.iter().find(|c| c.serial == serial) else {
            return false;
        };
        if !controller.supports_lightbar {
            return false;
        }

        if self.client_mode {
            crate::ipc::send_command(&crate::ipc::ShellCommand::Identify {
                serial: serial.to_string(),
                percent: controller.percent,
            })
            .is_ok()
        } else {
            self.hid_worker
                .as_ref()
                .is_some_and(|w| w.identify(serial.to_string(), controller.percent))
        }
    }

    fn power_off(&mut self, serial: &str) {
        if is_emulated_serial(serial) {
            app_log::info(format!(
                "power-off skipped for emulated controller {serial}"
            ));
            return;
        }

        let Some(controller) = self.session.controllers.iter().find(|c| c.serial == serial) else {
            return;
        };
        if !controller.supports_power_off || !controller.connection.is_bluetooth() {
            app_log::warn(format!(
                "power-off ignored for {serial} ({})",
                controller.connection
            ));
            return;
        }

        // Reconnect after user power-off should open Start again (not ghost-flap cooldown),
        // but only when this was the last pad — a sibling must not look like a fresh 0→1.
        if crate::session::should_skip_connect_cooldown_on_power_off(
            &self.session.controllers,
            serial,
            self.session.prefs.start_screen_usb_controllers,
        ) {
            self.session.mark_skip_connect_cooldown();
        }
        if self.client_mode {
            let _ = crate::ipc::send_command(&crate::ipc::ShellCommand::PowerOff {
                serial: serial.to_string(),
            });
        } else {
            if let Some(w) = self.hid_worker.as_ref() {
                w.power_off(serial.to_string());
            }
        }
    }

    fn toggle_remember(&mut self, serial: &str) -> Task<Message> {
        if is_emulated_serial(serial) {
            return Task::none();
        }

        if self.session.known.is_remembered(serial) {
            self.session.known.forget(serial);
        } else if let Some(controller) =
            self.session.controllers.iter().find(|c| c.serial == serial)
        {
            self.session.known.remember(controller);
        }

        self.session.known.save();
        self.sync_popup_rows_and_fit()
    }

    fn set_nickname(&mut self, serial: &str, nickname: Option<String>) -> Task<Message> {
        if is_emulated_serial(serial) {
            return Task::none();
        }
        if self.session.known.set_nickname(serial, nickname) {
            self.session.known.save();
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

        if preset.is_analytics() && !self.session.prefs.analytics_enabled {
            self.session.prefs.analytics_enabled = true;
            self.session.prefs.save();
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
            self.session
                .analytics
                .dev_credit_active(emulate::PRIMARY_SERIAL, credit);
        }

        if preset == Preset::AnalyticsSeedEstimates {
            self.session
                .analytics
                .dev_seed_estimates(emulate::PRIMARY_SERIAL);
            self.session.analytics.save();
            self.refresh_analytics_panel();
        }

        if preset == Preset::AnalyticsPause {
            self.dev_paused_percent = self
                .session
                .controllers
                .iter()
                .find(|c| c.serial == emulate::PRIMARY_SERIAL)
                .map(|c| c.percent)
                .or(self.dev_paused_percent);
        }

        // Unplug-from-full needs a Complete → Discharging edge.
        let ensure_complete =
            preset == Preset::AnalyticsUnplugFull
                && !self.session.controllers.iter().any(|c| {
                    c.serial == emulate::PRIMARY_SERIAL && c.state == PowerState::Complete
                });

        let next = if preset == Preset::AnalyticsResume {
            let percent = self
                .dev_paused_percent
                .or_else(|| {
                    self.session
                        .analytics
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
            emulate::apply_preset(preset, &self.session.controllers)
        };

        self.emulating = true;
        self.session.analytics.save();
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
            self.session.prefs.games_sort_mode,
            |entry| match entry {
                crate::games::GameEntry::Steam { appid } => steam_by_id
                    .get(appid)
                    .map(|g| g.name.clone())
                    .unwrap_or_else(|| format!("Steam {appid}")),
                crate::games::GameEntry::Manual { title, .. } => title.clone(),
            },
            |entry| {
                let catalog_ms = self.games.last_played(entry).unwrap_or(0);
                let steam_ms = match entry {
                    crate::games::GameEntry::Steam { appid } => steam_by_id
                        .get(appid)
                        .and_then(|g| g.last_played_unix)
                        .map(|secs| secs.saturating_mul(1000))
                        .unwrap_or(0),
                    crate::games::GameEntry::Manual { .. } => 0,
                };
                let ms = catalog_ms.max(steam_ms);
                (ms > 0).then_some(ms)
            },
        )
    }

    fn refresh_start_rows(&mut self) {
        self.start_state.sort_mode = self.session.prefs.games_sort_mode;
        let rows = if self.start_state.editing {
            self.edit_checklist_rows()
        } else {
            self.display_catalog()
                .iter()
                .map(|entry| {
                    start_view::StartRow::from_entry(
                        entry,
                        &self.steam_by_id,
                        self.steam_scan_pending,
                    )
                })
                .collect()
        };
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
            self.games = draft;
            self.games.save();
        }
        self.start_state.editing = false;
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
        self.start_state.show_all_controllers = self.session.prefs.show_all_controllers;
        let mut rows: Vec<_> = self
            .session
            .controllers
            .iter()
            .map(|c| {
                let nickname = self.session.known.nickname(&c.serial);
                let eta = if self.session.prefs.analytics_enabled {
                    self.session
                        .analytics
                        .eta_for(c)
                        .map(analytics::format_eta_ring)
                } else {
                    None
                };
                start_view::StartControllerRow {
                    serial: c.serial.clone(),
                    title: start_view::controller_title(&c.product, nickname),
                    connection: c.connection.to_string(),
                    state: if c.is_low_battery(self.session.prefs.low_battery_percent) {
                        "low battery".into()
                    } else {
                        start_view::power_state_label(c.state).into()
                    },
                    percent: c.percent,
                    low: c.is_low_battery(self.session.prefs.low_battery_percent),
                    bluetooth: c.connection.is_bluetooth() && c.supports_power_off,
                    connected: true,
                    eta,
                }
            })
            .collect();
        if self.session.prefs.show_all_controllers {
            for controller in self
                .session
                .known
                .remembered_disconnected(&self.session.controllers)
            {
                let nickname = self.session.known.nickname(&controller.serial);
                let eta = if self.session.prefs.analytics_enabled {
                    self.session
                        .analytics
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
        // List icons only here — full immersive prepare must not block SteamScanDone
        // (compact stayed on skeletons until every hero/backdrop decoded).
        Task::perform(
            spawn_blocking(|| {
                let games = steam::list_installed_games()?;
                crate::ui::start::icon_cache::prepare_paths(
                    games.iter().filter_map(|g| g.icon_path.as_deref()),
                );
                Ok::<_, String>(games)
            }),
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
        self.steam_scan_pending = false;
        self.match_path_cache.clear();
        self.refresh_start_rows();
        let mut task = self.prepare_immersive_library_task();
        if self.start_visible && self.start_state.immersive {
            task = task.chain(self.warm_immersive_art_task());
        }
        task
    }

    /// Offline warm of library hero/backdrop tiers after scan (does not block skeletons).
    fn prepare_immersive_library_task(&self) -> Task<Message> {
        let heroes: Vec<_> = self
            .steam_by_id
            .values()
            .filter_map(|g| g.icon_path.clone())
            .collect();
        let backdrops: Vec<_> = self
            .steam_by_id
            .values()
            .filter_map(|g| g.backdrop_path.clone())
            .collect();
        if heroes.is_empty() && backdrops.is_empty() {
            return Task::none();
        }
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: immersive art library prepare begin heroes={} backdrops={}",
            heroes.len(),
            backdrops.len()
        ));
        Task::perform(
            spawn_blocking(move || {
                crate::ui::start::icon_cache::prepare_immersive(&heroes, &backdrops);
            }),
            |_| Message::ImmersiveArtReady,
        )
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
        self.start_visible
            || self.gesture_recorder.is_active()
            || (self.configure_window.is_some()
                && self.configure_state.section == Section::PadInput)
            || (self.session.prefs.start_screen_enabled && !self.session.controllers.is_empty())
    }

    fn open_start_screen(&mut self) -> Task<Message> {
        if !self.session.prefs.start_screen_enabled {
            return Task::none();
        }
        self.start_state.editing = false;
        self.start_state.edit_anchor_play_key = None;
        self.edit_draft = None;
        self.refresh_start_rows();
        self.refresh_start_controllers();

        self.prepare_start_nav_on_open(false);

        // Already open: re-focus; force running restore in case a game started.
        // Duplicate OpenStart (service glitch before ReportStartVisible) must
        // re-arm the release latch so the opening press cannot promote.
        if self.start_visible
            && let Some(id) = self.start_window
        {
            self.session.start_auto_open_pending = false;
            self.last_running_check = None;
            self.arm_chord_release_latch();
            let badge = self.refresh_running_badge();
            return Task::batch([badge, window::gain_focus(id)]);
        }

        // Mark the current chord consumed (do not reset): a held PS from power-on
        // must not reopen Start immediately after a focus-close.
        self.arm_chord_release_latch();
        self.clear_start_nav_diag();
        self.start_opened_at = Some(Instant::now());
        self.start_revealed_at = Some(Instant::now());
        // refresh_start_rows already ran a throttled check; force one restore on open.
        self.last_running_check = None;
        let badge = self.refresh_running_badge();

        // Cold open — OS window entrance. Do not chain sync_toast_zorder here;
        // StartOpened raises after the window/renderer exists.
        if self.start_window.is_some() {
            // Stale id without visibility (close in flight) — wait for WindowClosed.
            // Leave start_auto_open_pending set so WindowClosed can retry.
            crate::controller::hid::diag::diag_info(
                "ui-diag: start open skipped (window still closing)",
            );
            return Task::none();
        }

        let immersive = matches!(
            start_mode::on_open(self.session.prefs.start_screen_always_immersive),
            start_mode::ImmersiveTransition::OpenImmersive
        );
        self.start_state.immersive = immersive;
        self.clear_start_cursor_hide();
        let (size, position) = if immersive {
            let cover = primary_monitor_cover().unwrap_or(MonitorCover {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            });
            self.start_monitor_cover = Some(cover);
            // Cached splash must not paint at opacity 1 then restart the fade.
            self.start_state.prime_backdrop_for_enter(Instant::now());
            crate::controller::hid::diag::diag_info("ui-diag: start immersive enter");
            (cover.size(), window::Position::Specific(cover.origin()))
        } else {
            self.start_monitor_cover = None;
            (
                Size::new(start_view::WIDTH, start_view::HEIGHT),
                window::Position::Centered,
            )
        };

        self.start_nav_ready = false;
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: start cold open immersive={}",
            u8::from(immersive)
        ));
        let (id, open) = window::open(window::Settings {
            size,
            position,
            visible: true,
            resizable: false,
            decorations: false,
            level: window::Level::AlwaysOnTop,
            exit_on_close_request: true,
            platform_specific: overlay_platform_specific(),
            ..window::Settings::default()
        });
        self.start_window = Some(id);
        self.start_visible = true;
        self.report_start_visible(true);
        self.sync_client_input_hot();
        self.session.start_auto_open_pending = false;
        crate::platform::wgpu_diag::note_start_open();
        Task::batch([badge, open.map(Message::StartOpened)])
    }

    fn start_presentation(&self) -> StartPresentation {
        if self.start_state.immersive {
            StartPresentation::Immersive
        } else {
            StartPresentation::Compact
        }
    }

    fn immersive_transition_busy(&self) -> bool {
        self.start_state.transition.is_some() || self.start_state.transition_phase.is_some()
    }

    fn enter_start_immersive(&mut self) -> Task<Message> {
        let Some(_id) = self.start_window.filter(|_| self.start_visible) else {
            return Task::none();
        };
        let Some(_transition) =
            start_mode::begin_promote(self.start_state.immersive, self.immersive_transition_busy())
        else {
            return Task::none();
        };
        let cover = primary_monitor_cover().unwrap_or(MonitorCover {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        });
        self.start_monitor_cover = Some(cover);
        self.clear_start_cursor_hide();
        self.arm_chord_release_latch();
        crate::controller::hid::diag::diag_info("ui-diag: start immersive enter");
        self.start_state
            .begin_transition_phase(start_mode::TransitionPhase::ExitCompact, Instant::now());
        Task::none()
    }

    fn leave_start_immersive(&mut self) -> Task<Message> {
        let Some(_id) = self.start_window.filter(|_| self.start_visible) else {
            return Task::none();
        };
        // EnterImmersive veil is interruptible; HWND resize / ExitImmersive are not.
        let blocked = start_mode::demote_blocked(
            self.start_state.transition.is_some(),
            self.start_state.transition_phase.map(|(p, _)| p),
        );
        let Some(_transition) = start_mode::begin_demote(self.start_state.immersive, blocked)
        else {
            return Task::none();
        };
        self.arm_chord_release_latch();
        crate::controller::hid::diag::diag_info("ui-diag: start immersive leave");
        self.clear_start_cursor_hide();
        self.start_state
            .begin_transition_phase(start_mode::TransitionPhase::ExitImmersive, Instant::now());
        Task::none()
    }

    fn tick_immersive_transition_phase(&mut self, now: Instant) -> Option<Task<Message>> {
        if !self.start_state.phase_finished(now) {
            return None;
        }
        let phase = self.start_state.transition_phase.map(|(p, _)| p)?;
        match phase {
            start_mode::TransitionPhase::ExitCompact => Some(self.perform_immersive_resize(true)),
            start_mode::TransitionPhase::ExitImmersive => {
                Some(self.perform_immersive_resize(false))
            }
            start_mode::TransitionPhase::EnterImmersive => {
                self.start_state.clear_transition_phase();
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: start immersive transition phase={} done",
                    phase.label()
                ));
                None
            }
            start_mode::TransitionPhase::EnterCompact => {
                self.start_state.clear_transition_phase();
                self.start_state.ambient_time = 0.0;
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: start immersive transition phase={} done",
                    phase.label()
                ));
                // After the Float scale veil clears, force the compact list onto the selection
                // (stale games_scroll_y from pre-promote often makes into-view a no-op).
                Some(self.scroll_start_selection_to_center(true))
            }
            start_mode::TransitionPhase::Resizing => None,
        }
    }

    fn perform_immersive_resize(&mut self, promoting: bool) -> Task<Message> {
        let Some(id) = self.start_window.filter(|_| self.start_visible) else {
            self.start_state.clear_transition_phase();
            return Task::none();
        };
        let transition = if promoting {
            start_mode::StartTransition::Promoting
        } else {
            start_mode::StartTransition::Demoting
        };
        self.start_state.transition = Some(transition);
        self.start_state
            .begin_transition_phase(start_mode::TransitionPhase::Resizing, Instant::now());
        if promoting {
            let cover = self.start_monitor_cover.unwrap_or_else(|| {
                primary_monitor_cover().unwrap_or(MonitorCover {
                    x: 0.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                })
            });
            self.start_monitor_cover = Some(cover);
            window::resize(id, cover.size())
                .chain(window::move_to(id, cover.origin()))
                .chain(Task::done(Message::StartImmersiveSettled))
        } else {
            let size = Size::new(start_view::WIDTH, start_view::HEIGHT);
            let position = self
                .start_monitor_cover
                .or_else(primary_monitor_cover)
                .map(|cover| cover.center_for(size))
                .unwrap_or(Point::new(
                    ((1920.0 - size.width) / 2.0).max(0.0),
                    ((1080.0 - size.height) / 2.0).max(0.0),
                ));
            window::resize(id, size)
                .chain(window::move_to(id, position))
                .chain(Task::done(Message::StartImmersiveSettled))
        }
    }

    fn finish_start_immersive_transition(&mut self) -> Task<Message> {
        let Some(transition) = self.start_state.transition.take() else {
            return Task::none();
        };
        let immersive = start_mode::settle_immersive(transition);
        self.start_state.immersive = immersive;
        self.start_state.dock_expanded = false;
        self.start_state.slide = StartSlide::Games;
        // Dock/strip anims cleared via transition phase begin + request_dock defaults.
        let now = Instant::now();
        if immersive {
            // Keep ambient_time; open aperture + chrome.
            // Prime (not reset): no prior splash to crossfade; avoid blink on cached art.
            self.start_state.prime_backdrop_for_enter(now);
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: start immersive settle promote backdrop re-arm sel={}",
                self.start_state.game_selected
            ));
            self.start_state
                .begin_transition_phase(start_mode::TransitionPhase::EnterImmersive, now);
        } else {
            self.start_monitor_cover = None;
            self.clear_start_cursor_hide();
            self.start_state.clear_backdrop_transition();
            self.start_state
                .begin_transition_phase(start_mode::TransitionPhase::EnterCompact, now);
        }
        self.arm_chord_release_latch();
        let kind = if immersive { "promote" } else { "demote" };
        crate::controller::hid::diag::diag_info(format!("ui-diag: start immersive settle {kind}"));
        let focus = self
            .start_window
            .map(|id| window::gain_focus(id).chain(raise_window_topmost(id)))
            .unwrap_or_else(Task::none);
        if immersive {
            focus.chain(self.warm_immersive_art_task())
        } else {
            // Center selection as soon as compact chrome exists; EnterCompact completion
            // re-centers once the scale Float is gone (see tick_immersive_transition_phase).
            focus.chain(self.scroll_start_selection_to_center(true))
        }
    }

    fn warm_immersive_art_task(&self) -> Task<Message> {
        let (heroes, shells, backdrops) = self.start_state.immersive_warm_paths();
        if heroes.is_empty() && shells.is_empty() && backdrops.is_empty() {
            return Task::none();
        }
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: immersive art warm begin heroes={} shells={} backdrops={}",
            heroes.len(),
            shells.len(),
            backdrops.len()
        ));
        Task::perform(
            spawn_blocking(move || {
                crate::ui::start::icon_cache::warm_immersive_paths(&heroes, &backdrops);
                for path in shells {
                    let _ = crate::ui::start::icon_cache::hero_for_shell(&path);
                }
            }),
            |_| Message::ImmersiveArtReady,
        )
    }

    /// Arm the debounced release latch and mark any locally visible chord consumed.
    ///
    /// In client mode the HID snapshot lives in the service process, so
    /// [`read_nav_readings`] is usually Missing — leave detectors alone (do not
    /// reset/disarm). Live pad edges while latched call `consume_pending_match`.
    fn arm_chord_release_latch(&mut self) {
        self.chord_release_gate.arm();
        self.consume_reopen_gesture_chord();
    }

    /// Mark the live reopen chord as already matched so a sticky hold cannot fire.
    fn consume_reopen_gesture_chord(&mut self) {
        match start_input::read_nav_readings() {
            start_input::NavReadingsOutcome::Readings { readings, .. } => {
                self.gesture_detectors.consume_pending_match(&readings);
            }
            start_input::NavReadingsOutcome::Missing { .. } => {
                // Shell has no local snapshot — resetting would disarm detectors
                // and let the same press promote after a one-sample gap.
            }
        }
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

    fn note_start_cursor_activity(&mut self) {
        self.start_cursor_last_active = Instant::now();
        self.set_start_cursor_hidden(false);
    }

    fn clear_start_cursor_hide(&mut self) {
        self.start_cursor_last_active = Instant::now();
        self.set_start_cursor_hidden(false);
    }

    fn set_start_cursor_hidden(&mut self, hidden: bool) {
        if self.start_cursor_hidden == hidden {
            return;
        }
        self.start_cursor_hidden = hidden;
        if hidden {
            crate::controller::hid::diag::diag_info("ui-diag: start cursor hide");
        } else {
            crate::controller::hid::diag::diag_info("ui-diag: start cursor show");
        }
    }

    fn tick_start_cursor_idle(&mut self, now: Instant) {
        if !self.start_visible || !self.start_state.immersive {
            self.set_start_cursor_hidden(false);
            return;
        }
        if !cursor_on_primary_monitor() {
            self.set_start_cursor_hidden(false);
            return;
        }
        let idle = now.saturating_duration_since(self.start_cursor_last_active);
        if idle >= START_CURSOR_IDLE_HIDE {
            self.set_start_cursor_hidden(true);
        }
    }

    fn close_start_screen(&mut self) -> Task<Message> {
        // Clear visibility before any hide task so Resting toast RaiseInteractive
        // cannot resurface Start while the hide is in flight.
        self.start_visible = false;
        self.report_start_visible(false);
        self.sync_client_input_hot();
        self.start_nav_ready = false;
        self.start_revealed_at = None;
        self.start_state.immersive = false;
        self.start_state.clear_immersive_session();
        self.start_monitor_cover = None;
        self.clear_start_cursor_hide();
        self.haptic_pad_serial = None;
        if self.client_mode {
            let _ = crate::ipc::send_command(&crate::ipc::ShellCommand::RumbleStopAll);
        } else {
            if let Some(w) = self.hid_worker.as_ref() {
                w.rumble_stop_all();
            }
        }
        self.pad_nav.reset();
        self.keyboard_cross_hold.reset();
        self.confirm_key_held = false;
        self.cancel_key_held = false;
        self.pad_held = FaceHeld::default();
        self.start_state.reset_to_games();
        self.discard_edit_draft();
        self.clear_start_nav_diag();
        // Focus-close often leaves the reopen chord held (e.g. PS); latch until
        // a debounced full release so the next PadPoll cannot immediately reopen.
        self.arm_chord_release_latch();
        match self.start_window {
            Some(id) => {
                crate::controller::hid::diag::diag_info("ui-diag: start close");
                window::set_mode(id, window::Mode::Hidden).chain(window::close(id))
            }
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

    /// Mouse report buttons — stamp both logs with kind + live snapshot context.
    #[cfg(debug_assertions)]
    fn mark_hitch_now(&mut self, kind: &str) {
        let start_open = self.start_visible;
        let nav_ready = self.start_nav_ready;
        let controllers = self.session.controllers.len();
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
            | StartMessage::ManualAddArgs(_) => {}
            #[cfg(debug_assertions)]
            StartMessage::ReportLightbarFailure | StartMessage::ReportInputFailure => {}
            _ => self.cancel_start_holds(),
        }
        match message {
            StartMessage::Launch(index) => {
                if self.start_state.immersive
                    && index < self.start_state.rows.len()
                    && index != self.start_state.game_selected
                {
                    // Neighbor capsule click selects only (center / Cross launches).
                    return self.on_start_message(StartMessage::SelectGame(index));
                }
                if index < self.start_state.rows.len() {
                    self.start_state.game_selected = index;
                }
                let scroll = self.scroll_start_selection_into_view();
                if self.start_state.editing {
                    match self.edit_toggle_selected() {
                        Some(task) => {
                            self.play_start_cue(UiSoundKind::Action);
                            scroll.chain(task)
                        }
                        None => scroll,
                    }
                } else if self.launch_selected() {
                    self.play_start_cue(UiSoundKind::Action);
                    if self.start_state.immersive {
                        scroll.chain(self.close_start_screen())
                    } else {
                        scroll
                    }
                } else {
                    scroll
                }
            }
            StartMessage::SelectGame(index) => {
                if index >= self.start_state.rows.len() || index == self.start_state.game_selected {
                    return Task::none();
                }
                let prev_game = self.start_state.game_selected;
                self.start_state.game_selected = index;
                let mut task = self.scroll_start_selection_into_view();
                if self.start_state.immersive {
                    self.start_state
                        .begin_strip_anim(prev_game as f32, Instant::now());
                    self.start_state.reset_backdrop_fade();
                    task = task.chain(self.warm_immersive_art_task());
                }
                self.play_start_cue(UiSoundKind::Nav);
                task
            }
            StartMessage::SelectController(index) => {
                if index < self.start_state.controllers.len()
                    && self.start_state.controller_selected != index
                {
                    self.start_state.controller_selected = index;
                    self.play_start_cue(UiSoundKind::Nav);
                }
                if self.start_state.immersive && !self.start_state.dock_expanded {
                    let _ = self.start_state.request_dock(true, Instant::now());
                }
                self.scroll_start_selection_into_view()
            }
            StartMessage::MoveUp => {
                let prev_game = self.start_state.game_selected;
                let direction = self.start_state.move_selection(-1);
                let mut task = self.scroll_start_selection_into_view_dir(
                    direction.unwrap_or(start_view::ScrollReveal::Up),
                );
                if direction.is_some() {
                    self.play_start_cue(UiSoundKind::Nav);
                    if self.start_state.immersive
                        && matches!(self.start_state.slide, StartSlide::Games)
                        && self.start_state.game_selected != prev_game
                    {
                        self.start_state
                            .begin_strip_anim(prev_game as f32, Instant::now());
                        self.start_state.reset_backdrop_fade();
                        task = task.chain(self.warm_immersive_art_task());
                    }
                }
                task
            }
            StartMessage::MoveDown => {
                let prev_game = self.start_state.game_selected;
                let direction = self.start_state.move_selection(1);
                let mut task = self.scroll_start_selection_into_view_dir(
                    direction.unwrap_or(start_view::ScrollReveal::Down),
                );
                if direction.is_some() {
                    self.play_start_cue(UiSoundKind::Nav);
                    if self.start_state.immersive
                        && matches!(self.start_state.slide, StartSlide::Games)
                        && self.start_state.game_selected != prev_game
                    {
                        self.start_state
                            .begin_strip_anim(prev_game as f32, Instant::now());
                        self.start_state.reset_backdrop_fade();
                        task = task.chain(self.warm_immersive_art_task());
                    }
                }
                task
            }
            StartMessage::Confirm => self.on_start_confirm(),
            StartMessage::Close => {
                if self.start_state.manual_add.is_some() {
                    self.play_start_cue(UiSoundKind::Action);
                    self.cancel_manual_add()
                } else if self.start_state.replace_confirm.take().is_some() {
                    self.play_start_cue(UiSoundKind::Action);
                    Task::none()
                } else if self.start_state.editing {
                    self.play_start_cue(UiSoundKind::Action);
                    self.cancel_start_edit()
                } else if self.start_visible {
                    // Immersive + dock open: Circle collapses the drawer first (same as Left).
                    if self.start_state.immersive
                        && start_mode::dock_collapse_target(self.start_state.dock_expanded)
                            .is_some()
                        && self.start_state.request_dock(false, Instant::now())
                    {
                        self.play_start_cue(UiSoundKind::Slide);
                        crate::controller::hid::diag::diag_info(
                            "ui-diag: start dock collapse (cancel)",
                        );
                        return Task::none();
                    }
                    self.play_start_cue(UiSoundKind::Action);
                    match start_mode::on_cancel(
                        self.start_presentation(),
                        self.session.prefs.start_screen_always_immersive,
                    ) {
                        start_mode::ImmersiveTransition::Demote => self.leave_start_immersive(),
                        _ => self.close_start_screen(),
                    }
                } else {
                    Task::none()
                }
            }
            StartMessage::PrevSlide => {
                if self.start_state.overlay_blocking() || self.start_state.transition.is_some() {
                    return Task::none();
                }
                if self.start_state.editing {
                    self.discard_edit_draft();
                    self.refresh_start_rows();
                }
                let now = Instant::now();
                if self.start_state.immersive {
                    // Left: collapse controllers island.
                    if start_mode::dock_collapse_target(self.start_state.dock_expanded).is_some()
                        && self.start_state.request_dock(false, now)
                    {
                        self.play_start_cue(UiSoundKind::Slide);
                        crate::controller::hid::diag::diag_info("ui-diag: start dock collapse");
                    }
                } else if self.start_state.request_slide(StartSlide::Games, now) {
                    // Left: Controllers → Games only (no wrap). Interruptible mid-anim.
                    self.play_start_cue(UiSoundKind::Slide);
                }
                Task::none()
            }
            StartMessage::NextSlide => {
                if self.start_state.overlay_blocking() || self.start_state.transition.is_some() {
                    return Task::none();
                }
                if self.start_state.editing {
                    self.discard_edit_draft();
                    self.refresh_start_rows();
                }
                let now = Instant::now();
                if self.start_state.immersive {
                    // Right: expand controllers island.
                    if start_mode::dock_expand_target(self.start_state.dock_expanded).is_some()
                        && self.start_state.request_dock(true, now)
                    {
                        self.play_start_cue(UiSoundKind::Slide);
                        crate::controller::hid::diag::diag_info("ui-diag: start dock expand");
                    }
                } else if self.start_state.request_slide(StartSlide::Controllers, now) {
                    // Right: Games → Controllers only (no wrap). Interruptible mid-anim.
                    self.play_start_cue(UiSoundKind::Slide);
                }
                Task::none()
            }
            StartMessage::ToggleEdit => match self.toggle_start_edit() {
                Some(task) => {
                    self.play_start_cue(UiSoundKind::Action);
                    task
                }
                None => Task::none(),
            },
            StartMessage::CycleSort => match self.on_start_cycle_sort() {
                Some(task) => {
                    self.play_start_cue(UiSoundKind::Action);
                    task
                }
                None => Task::none(),
            },
            StartMessage::AddShortcut => {
                if self.start_state.editing && !self.start_state.overlay_blocking() {
                    self.play_start_cue(UiSoundKind::Action);
                    self.pick_manual_shortcut()
                } else {
                    Task::none()
                }
            }
            StartMessage::EditManual => {
                let before = self.start_state.manual_add.is_some();
                let task = self.begin_manual_edit();
                if !before && self.start_state.manual_add.is_some() {
                    self.play_start_cue(UiSoundKind::Action);
                }
                task
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
                self.play_start_cue(UiSoundKind::Action);
                self.pick_manual_icon()
            }
            StartMessage::ManualClearIcon => {
                if let Some(draft) = self.start_state.manual_add.as_mut() {
                    draft.icon_path = None;
                    self.play_start_cue(UiSoundKind::Action);
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
        if !self.session.prefs.start_screen_sounds_enabled {
            return;
        }
        let volume = f32::from(self.session.prefs.start_screen_sound_volume) / 100.0;
        ui_sound::play(kind, volume);
    }

    /// Sound + optional pad haptic for a start-screen cue.
    fn play_start_cue(&self, kind: UiSoundKind) {
        self.play_start_sound(kind);
        self.play_start_haptic(kind);
    }

    fn play_start_haptic(&self, kind: UiSoundKind) {
        if !self.session.prefs.start_screen_haptics_enabled {
            return;
        }
        let Some(serial) = self.haptic_pad_serial.as_ref() else {
            return;
        };
        let pulse = match kind {
            UiSoundKind::Nav => MotorPulse::NAV,
            UiSoundKind::Action => MotorPulse::ACTION,
            UiSoundKind::Hold => MotorPulse::HOLD,
            UiSoundKind::Slide => MotorPulse::SLIDE,
        };
        let (right, left, duration) =
            pulse.scaled(self.session.prefs.start_screen_haptics_strength);
        if right == 0 && left == 0 {
            return;
        }
        if self.client_mode {
            let _ = crate::ipc::send_command(&crate::ipc::ShellCommand::Rumble {
                serial: serial.clone(),
                right,
                left,
                duration_ms: duration.as_millis() as u64,
            });
        } else {
            if let Some(w) = self.hid_worker.as_ref() {
                w.rumble(serial.clone(), right, left, duration.as_millis() as u64);
            }
        }
    }

    /// Preview haptic on every connected DualSense (settings toggle / strength slider).
    fn preview_haptic_all(&self, pulse: MotorPulse) {
        if !self.session.prefs.start_screen_haptics_enabled {
            return;
        }
        let (right, left, duration) =
            pulse.scaled(self.session.prefs.start_screen_haptics_strength);
        if right == 0 && left == 0 {
            return;
        }
        let ms = duration.as_millis() as u64;
        for controller in &self.session.controllers {
            if self.client_mode {
                let _ = crate::ipc::send_command(&crate::ipc::ShellCommand::Rumble {
                    serial: controller.serial.clone(),
                    right,
                    left,
                    duration_ms: ms,
                });
            } else if let Some(w) = self.hid_worker.as_ref() {
                w.rumble(controller.serial.clone(), right, left, ms);
            }
        }
    }

    fn set_haptic_pad_from_id(&mut self, pad: Option<&start_input::PadId>) {
        self.haptic_pad_serial =
            pad.and_then(|id| id.as_str().strip_prefix("hid:").map(|s| s.to_string()));
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
        self.session.prefs.games_sort_mode = self.session.prefs.games_sort_mode.cycle();
        self.session.prefs.save();
        self.refresh_start_rows();
        Some(self.scroll_start_selection_into_view())
    }

    fn on_start_cycle_sort(&mut self) -> Option<Task<Message>> {
        match self.start_state.slide {
            StartSlide::Games => self.cycle_games_sort(),
            StartSlide::Controllers => {
                let next = !self.session.prefs.show_all_controllers;
                self.set_show_all_controllers(next)
            }
        }
    }

    fn set_show_all_controllers(&mut self, show_all: bool) -> Option<Task<Message>> {
        if self.start_state.slide != StartSlide::Controllers
            || self.start_state.overlay_blocking()
            || self.session.prefs.show_all_controllers == show_all
        {
            return None;
        }
        self.session.prefs.show_all_controllers = show_all;
        self.session.prefs.save();
        app_log::hid_trace(format!(
            "start-controllers: show_all={}",
            u8::from(show_all)
        ));
        self.refresh_start_controllers();
        Some(self.scroll_start_selection_into_view())
    }

    fn pick_manual_shortcut(&mut self) -> Task<Message> {
        self.start_file_dialog_open = true;
        self.pick_file_dialog(
            "Programs",
            &["exe", "lnk", "url"],
            Message::ManualFilePicked,
        )
    }

    fn pick_manual_icon(&mut self) -> Task<Message> {
        self.start_file_dialog_open = true;
        self.pick_file_dialog(
            "Images",
            &["png", "jpg", "jpeg", "ico", "webp"],
            Message::ManualIconPicked,
        )
    }

    /// Native file picker; on Windows parents to Start so it clears immersive cover.
    fn pick_file_dialog(
        &self,
        filter_name: &'static str,
        extensions: &'static [&'static str],
        to_message: impl Fn(Option<PathBuf>) -> Message + Send + 'static,
    ) -> Task<Message> {
        let parent = match self.start_window.filter(|_| self.start_visible) {
            Some(id) => window_hwnd(id),
            None => Task::done(None),
        };
        let mut to_message = Some(to_message);
        parent.then(move |parent_hwnd| {
            let to_message = to_message
                .take()
                .expect("file dialog mapping consumed once");
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: file dialog parent hwnd={}",
                parent_hwnd
                    .map(|h| format!("0x{h:x}"))
                    .unwrap_or_else(|| "none".into())
            ));
            Task::perform(
                spawn_blocking(move || {
                    let mut dialog = rfd::FileDialog::new().add_filter(filter_name, extensions);
                    #[cfg(windows)]
                    if let Some(parent) = parent_hwnd.and_then(Win32DialogParent::new) {
                        dialog = dialog.set_parent(&parent);
                    }
                    #[cfg(not(windows))]
                    let _ = parent_hwnd;
                    dialog.pick_file().map(|p| p.to_string_lossy().into_owned())
                }),
                move |result| match result {
                    Ok(path) => to_message(path.map(PathBuf::from)),
                    Err(_) => to_message(None),
                },
            )
        })
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
        if self.start_state.manual_add.is_some() {
            self.play_start_cue(UiSoundKind::Action);
            return self.confirm_manual_add();
        }
        // Replace confirm requires hold-Cross / hold-Enter (see pad poll / key hold).
        if self.start_state.replace_confirm.is_some() {
            return Task::none();
        }
        match self.start_state.slide {
            StartSlide::Games if self.start_state.editing => match self.edit_toggle_selected() {
                Some(task) => {
                    self.play_start_cue(UiSoundKind::Action);
                    task
                }
                None => Task::none(),
            },
            StartSlide::Games => {
                if self.launch_selected() {
                    self.play_start_cue(UiSoundKind::Action);
                    if self.start_state.immersive {
                        return self.close_start_screen();
                    }
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
                        self.play_start_cue(UiSoundKind::Action);
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
        if self.launch_selected() && self.start_state.immersive {
            return self.close_start_screen();
        }
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

    fn on_service_message(&mut self, msg: crate::ipc::ServiceMessage) -> Task<Message> {
        use crate::ipc::ServiceMessage;
        match msg {
            ServiceMessage::Controllers(controllers) => {
                // Service already ran session + HID; refresh shell UI only when changed.
                if !crate::session::controllers_equivalent(&self.session.controllers, &controllers)
                {
                    self.session.controllers = controllers;
                    self.sync_popup_rows();
                    if self.start_visible {
                        self.refresh_start_controllers();
                    }
                }
                self.sync_client_input_hot();
                Task::none()
            }
            ServiceMessage::Effects(effects) => self.apply_session_effects(effects),
            ServiceMessage::PadInput(edge) => {
                if self.shell_wants_pad_input() {
                    self.on_pad_input(edge)
                } else {
                    Task::none()
                }
            }
            ServiceMessage::TrayOpenPopup { anchor } => {
                app_log::info("shell: apply tray open popup");
                self.tray_anchor = anchor;
                self.toggle_popup()
            }
            ServiceMessage::TrayOpenSettings => {
                app_log::info("shell: apply tray open settings");
                self.open_configure()
            }
            ServiceMessage::Notifications {
                events,
                open_start_after_toast,
            } => self.queue_notifications(events, open_start_after_toast),
            ServiceMessage::Shutdown => self.update(Message::Exit),
            ServiceMessage::Ack
            | ServiceMessage::ControllerList(_)
            | ServiceMessage::LightbarSet { .. } => Task::none(),
        }
    }

    fn on_pad_input(&mut self, edge: crate::domain::pad::InputEdge) -> Task<Message> {
        let poll_started = Instant::now();
        let start_open = self.start_visible;
        let meta = Some(edge.meta());
        let readings = edge.readings;

        if start_open {
            if let Some(prev) = self.last_pad_poll_at {
                let gap_ms = prev.elapsed().as_millis();
                if gap_ms >= PAD_POLL_STALL_MS {
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: pad-input stall gap_ms={gap_ms}"
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
        let listening = self.shell_wants_pad_input();

        if pad_input_open {
            self.apply_pad_input_panel_from_readings(&readings);
        }

        if !listening {
            return Task::none();
        }

        if self.gesture_recorder.is_active() {
            return self.on_gesture_record(&readings);
        }

        // Continue with the same body as the old on_pad_poll from start_open onward.
        self.on_pad_readings(readings, meta, start_open)
    }

    fn on_pad_readings(
        &mut self,
        readings: Vec<start_input::NavReading>,
        meta: Option<start_input::SnapshotMeta>,
        start_open: bool,
    ) -> Task<Message> {
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
                    if !self.nav_missing_warned && !self.session.controllers.is_empty() {
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

            return self.handle_start_nav_readings(&readings);
        }

        self.on_reopen_gesture(&readings)
    }

    fn handle_start_nav_readings(&mut self, readings: &[start_input::NavReading]) -> Task<Message> {
        self.nav_missing_warned = false;

        let gesture = &self.session.prefs.start_screen_gesture;
        self.start_state.reopen_chord_held =
            start_mode::promote_gesture_usable(gesture) && chord_held_on_any_pad(gesture, readings);

        // Reopen chord toggles compact ↔ immersive (service only listens when closed).
        if let Some(toggle) = self.try_toggle_immersive_from_gesture(readings) {
            return toggle;
        }

        let now = Instant::now();
        let animating = self.start_state.animating();
        // Mid-slide: keep Left/Right nav live for interruptible request_slide,
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
        let allow_nav_move = !animating && !replace_confirm && !manual_add;
        let editing = self.start_state.editing;
        // Only when the selected row shows Close game (running target match).
        let hold_cross_close = !editing
            && !replace_confirm
            && !manual_add
            && !animating
            && matches!(self.start_state.slide, StartSlide::Games)
            && self.start_state.running_target.as_ref().is_some_and(|t| {
                self.start_state
                    .rows
                    .get(self.start_state.game_selected)
                    .is_some_and(|row| &row.target == t)
            });
        // Match the row hint: Triangle hold only when Power off is shown.
        let hold_triangle_power = !editing
            && !replace_confirm
            && !manual_add
            && !animating
            && matches!(self.start_state.slide, StartSlide::Controllers)
            && self
                .start_state
                .selected_controller()
                .is_some_and(|row| row.show_power_off());
        // Immersive: left/right slides. Compact: L2/R2 slides (stepper stays vertical-only).
        let horizontal_nav = self.start_state.immersive;
        let tick = self.pad_nav.tick(
            readings,
            now,
            allow_nav_move,
            horizontal_nav,
            replace_confirm && !animating,
            editing,
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
                        "start-nav: pad unarmed waiting for rest (stick_x={:.3} stick_y={:.3} dpad_l={} dpad_r={} cross={} circle={})",
                        diag.sample.stick_x,
                        diag.sample.stick_y,
                        u8::from(diag.sample.dpad_left),
                        u8::from(diag.sample.dpad_right),
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

        // Pad-originated cues rumble the pad that fired (or completed a hold).
        let haptic_pad = tick
            .action_pad
            .as_ref()
            .or(tick.cross_completed_pad.as_ref())
            .or(tick.triangle_completed_pad.as_ref());
        self.set_haptic_pad_from_id(haptic_pad);

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
                // Keyboard-only complete: no pad serial → no rumble.
                if k_completed && tick.cross_completed_pad.is_none() {
                    self.haptic_pad_serial = None;
                }
                self.play_start_cue(UiSoundKind::Hold);
                self.complete_replace_confirm()
            } else if tick.action == Some(NavAction::Cancel) {
                self.keyboard_cross_hold.cancel();
                self.start_state.cross_progress = 0.0;
                self.on_start_message(StartMessage::Close)
            } else {
                Task::none()
            }
        } else if manual_add {
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
        } else {
            self.keyboard_cross_hold.reset();

            if hold_cross_close {
                self.start_state.cross_progress = tick.cross_progress;
                if tick.cross_completed {
                    self.play_start_cue(UiSoundKind::Hold);
                    self.close_running_game_if_selected();
                }
            } else {
                self.start_state.cross_progress = 0.0;
            }

            if hold_triangle_power {
                self.start_state.triangle_progress = tick.triangle_progress;
                if tick.triangle_completed
                    && let Some(row) = self.start_state.selected_controller()
                    && row.show_power_off()
                {
                    let serial = row.serial.clone();
                    self.play_start_cue(UiSoundKind::Hold);
                    self.power_off(&serial);
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
                }
            } else {
                Task::none()
            }
        };

        Task::batch([badge_task, nav_task])
    }

    fn on_gesture_record(&mut self, readings: &[start_input::NavReading]) -> Task<Message> {
        if readings.is_empty() {
            if !self.hid_exclusive_warned && !self.session.controllers.is_empty() {
                app_log::warn("could not read controller for gesture recording; try again");
                self.hid_exclusive_warned = true;
            }
            return Task::none();
        }
        self.hid_exclusive_warned = false;
        if let Some(sample) = self.gesture_record_latch.select(readings)
            && let Some(peak) = self.gesture_recorder.update(&sample.held)
        {
            self.session.prefs.start_screen_gesture = peak;
            self.session.prefs.save();
            self.gesture_record_latch.clear();
            self.gesture_detectors.consume_pending_match(readings);
        }
        Task::none()
    }

    fn on_reopen_gesture(&mut self, readings: &[start_input::NavReading]) -> Task<Message> {
        if readings.is_empty() {
            return Task::none();
        }
        // 0→1 auto-open owns this window; a PS power-on chord must not open Start
        // while the connect toast is still Placing / SlidingIn.
        if self.toast_machine.suppresses_reopen_gesture() {
            crate::controller::hid::diag::diag_info(
                "ui-diag: reopen gesture suppressed (toast OpenStart pending)",
            );
            return Task::none();
        }
        if !self.session.prefs.start_screen_enabled
            || self.session.prefs.start_screen_gesture.is_empty()
        {
            return Task::none();
        }
        let required = &self.session.prefs.start_screen_gesture;
        let chord_held = chord_held_on_any_pad(required, readings);
        let now = Instant::now();
        let (tick, glitch) = self.chord_release_gate.tick(chord_held, now);
        if glitch {
            crate::controller::hid::diag::diag_info("ui-diag: reopen chord glitch ignored");
        }
        match tick {
            ChordReleaseTick::Blocked => {
                if chord_held {
                    self.gesture_detectors.consume_pending_match(readings);
                }
                return Task::none();
            }
            ChordReleaseTick::Cleared => {
                // Absent sample disarms detectors; next held rising edge may fire.
                let _ = self.gesture_detectors.update(required, readings);
                return Task::none();
            }
            ChordReleaseTick::Open => {}
        }
        if self.gesture_detectors.update(required, readings) {
            let held = start_input::preferred_reading(readings)
                .map(|r| {
                    gesture::format_gesture(&r.sample.held.iter().copied().collect::<Vec<_>>())
                })
                .unwrap_or_else(|| "none".to_string());
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: reopen gesture fire held={held}"
            ));
            self.open_start_screen()
        } else {
            Task::none()
        }
    }

    /// Rising-edge reopen chord while Start is open → promote or demote (not when always-immersive).
    fn try_toggle_immersive_from_gesture(
        &mut self,
        readings: &[start_input::NavReading],
    ) -> Option<Task<Message>> {
        if !self.start_visible || readings.is_empty() {
            return None;
        }
        if self.start_state.overlay_blocking() || self.start_state.transition.is_some() {
            return None;
        }
        let required = &self.session.prefs.start_screen_gesture;
        if required.is_empty() || start_mode::chord_is_circle_only(required) {
            return None;
        }
        let chord_held = chord_held_on_any_pad(required, readings);
        let now = Instant::now();
        let (tick, glitch) = self.chord_release_gate.tick(chord_held, now);
        if glitch {
            crate::controller::hid::diag::diag_info("ui-diag: reopen chord glitch ignored");
        }
        match tick {
            ChordReleaseTick::Blocked => {
                if chord_held {
                    self.gesture_detectors.consume_pending_match(readings);
                }
                return None;
            }
            ChordReleaseTick::Cleared => {
                let _ = self.gesture_detectors.update(required, readings);
                return None;
            }
            ChordReleaseTick::Open => {}
        }
        if !self.gesture_detectors.update(required, readings) {
            return None;
        }
        let always = self.session.prefs.start_screen_always_immersive;
        match start_mode::on_reopen_chord(self.start_presentation(), always) {
            start_mode::ImmersiveTransition::Promote => {
                crate::controller::hid::diag::diag_info(
                    "ui-diag: reopen gesture promote immersive",
                );
                Some(self.enter_start_immersive())
            }
            start_mode::ImmersiveTransition::Demote => {
                crate::controller::hid::diag::diag_info("ui-diag: reopen gesture demote immersive");
                Some(self.leave_start_immersive())
            }
            _ => None,
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
                dpad_left: reading.sample.dpad_left,
                dpad_right: reading.sample.dpad_right,
                stick_band: start_input::StickBand::from_stick(
                    reading.sample.stick_x,
                    reading.sample.stick_y,
                )
                .as_str()
                .to_string(),
                stick_x: reading.sample.stick_x,
                stick_y: reading.sample.stick_y,
                held: gesture::format_gesture(&held),
            }
        } else {
            PadInputPanel::default()
        };
        self.pad_input_panel = next;
    }

    fn queue_notifications(
        &mut self,
        events: Vec<NotifyEvent>,
        mut open_start_after: bool,
    ) -> Task<Message> {
        for event in events {
            let eta = self.toast_eta_for(&event);
            let mut message =
                ToastMessage::from_notification(event, self.session.prefs.spectrum.clone(), eta);
            if open_start_after && message.body == "Connected" {
                message.after = AfterToast::OpenStart;
                open_start_after = false;
                crate::controller::hid::diag::diag_info(
                    "ui-diag: defer start until toast slide settles",
                );
            }
            self.toast_queue.push_back(message);
        }
        self.show_next_toast()
    }

    fn toast_eta_for(&self, event: &NotifyEvent) -> Option<String> {
        if !self.session.prefs.analytics_enabled {
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
        self.session
            .analytics
            .eta_for(&status)
            .map(analytics::format_eta_ring)
    }

    fn show_position_preview(&mut self) -> Task<Message> {
        self.toast_queue.clear();
        self.toast_queue
            .push_back(ToastMessage::preview(&self.session.prefs.spectrum));
        // Force-finish any in-flight toast without waiting for the machine.
        self.toast_machine = toast_machine::State::Idle;
        let finish = self.effect_toast_finished();
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

        self.toast_generation = self.toast_generation.wrapping_add(1);
        let generation = self.toast_generation;
        let after = message.after;
        let body = message.body.clone();
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: toast show heading={:?} body={body} percent={} after={after:?} queue_left={}",
            message.heading,
            message.percent(),
            self.toast_queue.len()
        ));
        self.toast_message = Some(message);

        let expire = Task::perform(delay(TOAST_LIFETIME), move |()| {
            Message::ToastDismiss(generation)
        });
        self.toast_step(toast_machine::Event::Show {
            generation,
            after,
            body,
        })
        .chain(expire)
    }

    /// Drive the presentation machine; log phase transitions in debug builds.
    fn toast_step(&mut self, event: toast_machine::Event) -> Task<Message> {
        let prev = self
            .toast_machine
            .phase()
            .map(toast_machine::phase_name)
            .unwrap_or("Idle");
        let prev_gen = self.toast_machine.generation();
        let (next, effects) = toast_machine::step(std::mem::take(&mut self.toast_machine), event);
        let next_name = next
            .phase()
            .map(toast_machine::phase_name)
            .unwrap_or("Idle");
        if prev != next_name || (!effects.is_empty() && prev == "Idle") {
            let body = match &next {
                toast_machine::State::Active(active) => active.body.as_str(),
                toast_machine::State::Idle => "-",
            };
            let generation = next.generation().or(prev_gen).unwrap_or(0);
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: toast {prev}->{next_name} gen={generation} body={body}"
            ));
        }
        // Shown starts the slide clock after the HWND is visible at outside_y.
        if next_name == "SlidingIn" && prev == "Placing" {
            self.toast_anim_started = Instant::now();
            if self.start_visible && self.start_state.immersive {
                let generation = next.generation().or(prev_gen).unwrap_or(0);
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: immersive toast composite gen={generation}"
                ));
            }
        }
        if next_name == "SlidingOut" && prev != "SlidingOut" {
            self.toast_anim_started = Instant::now();
        }
        self.toast_machine = next;
        self.apply_toast_effects(effects)
    }

    /// Draw the active toast inside immersive Start (cover occludes the toast HWND).
    fn immersive_toast_overlay(&self) -> Option<Element<'_, Message>> {
        if !self.start_visible || !self.start_state.immersive {
            return None;
        }
        let message = self.toast_message.as_ref()?;
        let (progress, dismissing) = self.toast_machine.slide_pose()?;
        let placement = self.toast_placement?;
        let cover = self.start_monitor_cover?;
        let local = toast_local_in_cover(placement, progress, dismissing, cover);
        let card = toast_view::view(
            message,
            self.toast_generation,
            Message::ToastDismiss(self.toast_generation),
        );
        // Layout at (0,0), then Float-translate into cover-local pose. Non-zero
        // translate (or a tiny nudge) puts the card in the overlay band above the
        // stage/veil and dock Floats; only the card's mouse_area captures clicks.
        let x = local.x;
        let y = local.y;
        Some(
            Float::new(card)
                .scale(1.0)
                .translate(move |_bounds, _viewport| {
                    if x == 0.0 && y == 0.0 {
                        Vector::new(0.0, 0.001)
                    } else {
                        Vector::new(x, y)
                    }
                })
                .into(),
        )
    }

    fn apply_toast_effects(&mut self, effects: Vec<ToastEffect>) -> Task<Message> {
        let mut task = Task::none();
        for effect in effects {
            task = task.chain(self.apply_toast_effect(effect));
        }
        task
    }

    fn apply_toast_effect(&mut self, effect: ToastEffect) -> Task<Message> {
        match effect {
            ToastEffect::PlaceShow => self.effect_place_show(),
            ToastEffect::Move {
                progress,
                dismissing,
            } => self.effect_toast_move(progress, dismissing),
            ToastEffect::SlideDtCapped {
                raw_ms,
                progress,
                dismissing,
            } => {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: toast slide dt capped ms={raw_ms} progress={progress:.2} dismissing={dismissing}"
                ));
                Task::none()
            }
            ToastEffect::OpenStart => self.effect_open_start(),
            ToastEffect::Finished => self.effect_toast_finished(),
            ToastEffect::RaiseInteractive { focus } => self.raise_interactive_ui_above_toast(focus),
        }
    }

    fn effect_place_show(&mut self) -> Task<Message> {
        let generation = self.toast_generation;
        if let Some(id) = self.toast_window {
            return Task::done(Message::PlaceToast { id, generation });
        }
        let (_id, open) = self.create_toast_window();
        open.map(move |id| Message::PlaceToast { id, generation })
    }

    fn effect_toast_move(&self, progress: f32, dismissing: bool) -> Task<Message> {
        let Some(id) = self.toast_window else {
            return Task::none();
        };
        let Some(placement) = self.toast_placement else {
            return Task::none();
        };
        let y = slide_y(placement, progress, dismissing);
        window::move_to(id, Point::new(placement.x, y))
    }

    fn effect_open_start(&mut self) -> Task<Message> {
        if crate::session::should_flush_latched_start(
            true,
            self.session.prefs.start_screen_enabled,
            self.start_visible,
        ) {
            crate::controller::hid::diag::diag_info("ui-diag: start open after toast settle");
            self.open_start_screen()
        } else {
            crate::controller::hid::diag::diag_info(
                "ui-diag: latched start skipped (disabled or already open)",
            );
            Task::none()
        }
    }

    fn effect_toast_finished(&mut self) -> Task<Message> {
        let _ = self.toast_message.take();
        // Invalidate any in-flight expiry for this toast.
        self.toast_generation = self.toast_generation.wrapping_add(1);
        self.toast_placement = None;

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
        // When Start is visible, defer hide — Crash A lined up create_renderer
        // panics with toast Idle/hide while Start was already open.
        if self.start_visible {
            crate::controller::hid::diag::diag_info("ui-diag: toast hide deferred (start visible)");
            return Task::perform(delay(Duration::from_millis(50)), |()| {
                Message::ToastHideDeferred
            });
        }
        match self.toast_window {
            Some(id) => {
                crate::controller::hid::diag::diag_info("ui-diag: toast hide");
                crate::platform::wgpu_diag::note_toast_hide();
                hide_toast(id)
            }
            None => Task::none(),
        }
    }

    /// Prefer Settings, then Start, then popup as the focused presenting window.
    fn refocus_interactive_ui(&self) -> Task<Message> {
        if let Some(id) = self.configure_window {
            return window::gain_focus(id);
        }
        if self.start_visible
            && let Some(id) = self.start_window
        {
            return window::gain_focus(id);
        }
        if let Some(id) = self.popup_window {
            return window::gain_focus(id);
        }
        Task::none()
    }

    /// Raise interactive UI into the topmost band above the toast (last wins).
    /// Toast stays TOPMOST vs Cursor; UI above toast keeps presents (iced#3320).
    /// Only called when the machine emits RaiseInteractive (Resting / settle).
    /// Hidden warm Start must not be raised — that resurfaced Start after close.
    /// Immersive cover occludes the toast HWND; see `immersive_toast_overlay`.
    fn raise_interactive_ui_above_toast(&self, focus: bool) -> Task<Message> {
        let mut task = Task::none();
        if let Some(id) = self.popup_window {
            task = task.chain(raise_window_topmost(id));
        }
        if self.start_visible {
            if let Some(id) = self.start_window {
                task = task.chain(raise_window_topmost(id));
            }
        } else if focus && self.start_window.is_some() {
            // Resting frames raise every tick — only log on settle/sync (focus).
            crate::controller::hid::diag::diag_info("ui-diag: start raise skipped (hidden)");
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
    /// open or close — only while the toast is Resting (safe to share presents).
    fn sync_toast_zorder(&self) -> Task<Message> {
        let Some(id) = self.toast_window else {
            return Task::none();
        };
        if self.toast_message.is_none() || !self.toast_machine.is_resting() {
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

/// Map tray events into iced `Message` values.
fn tray_events_mapped() -> impl Stream<Item = Message> {
    tray::tray_events().map(|event| match event {
        tray::TrayEvent::Menu(id) => Message::TrayMenu(id),
        tray::TrayEvent::LeftClick(anchor) => Message::TrayLeftClick(anchor),
    })
}

/// Bind the hid-worker → iced pad-input edge channel while listening.
fn pad_input_edge_stream() -> impl Stream<Item = Message> {
    stream::channel(64, async move |mut output| {
        let (tx, mut rx) = iced::futures::channel::mpsc::channel(32);
        start_input::bind_input_edge_sender(tx);
        let _guard = InputEdgeBindGuard;
        while let Some(edge) = rx.next().await {
            if output.send(Message::PadInput(edge)).await.is_err() {
                break;
            }
        }
    })
}

struct InputEdgeBindGuard;
impl Drop for InputEdgeBindGuard {
    fn drop(&mut self) {
        start_input::clear_input_edge_sender();
    }
}

/// Shell-client: drain service IPC messages into iced.
///
/// Blocking TCP reads run on a dedicated thread. Doing them inside the
/// `stream::channel` future would stall iced's UI poll (same thread polls the
/// runner and the output receiver), so tray/Settings messages would never apply.
fn service_message_stream() -> impl Stream<Item = Message> {
    stream::channel(64, async move |mut output| {
        let (mut tx, mut rx) = iced::futures::channel::mpsc::channel(128);
        let _ = std::thread::Builder::new()
            .name("sdsc-ipc-recv".into())
            .spawn(move || {
                loop {
                    let client = loop {
                        match crate::ipc::PipeClient::connect() {
                            Ok(c) => break c,
                            Err(_) => std::thread::sleep(Duration::from_millis(100)),
                        }
                    };
                    let Ok(mut reader) = client.try_clone_reader() else {
                        continue;
                    };
                    // Keep the write half alive so the server does not see EOF on accept.
                    let _keep_alive = client;
                    crate::platform::app_log::info("shell: ipc receive connected");
                    loop {
                        match reader.recv() {
                            Ok(msg) => {
                                let droppable =
                                    matches!(&msg, crate::ipc::ServiceMessage::PadInput(_));
                                match tx.try_send(msg) {
                                    Ok(()) => {}
                                    Err(err) if err.is_full() => {
                                        if droppable {
                                            // Prefer a fresh pad sample over blocking HID→UI.
                                        } else {
                                            let msg = err.into_inner();
                                            // Controllers / tray / effects: wait for a free slot.
                                            let mut msg = msg;
                                            loop {
                                                match tx.try_send(msg) {
                                                    Ok(()) => break,
                                                    Err(e) if e.is_full() => {
                                                        msg = e.into_inner();
                                                        std::thread::sleep(Duration::from_millis(
                                                            1,
                                                        ));
                                                    }
                                                    Err(_) => return,
                                                }
                                            }
                                        }
                                    }
                                    Err(_) => return,
                                }
                            }
                            Err(err) => {
                                crate::platform::app_log::warn(format!(
                                    "shell: ipc receive disconnected: {err}"
                                ));
                                break;
                            }
                        }
                    }
                }
            });

        while let Some(msg) = rx.next().await {
            if output.send(Message::Service(msg)).await.is_err() {
                break;
            }
        }
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
// Native file dialog parent (Windows)
// ---------------------------------------------------------------------------

/// Thin HWND wrapper for [`rfd::FileDialog::set_parent`].
#[cfg(windows)]
struct Win32DialogParent {
    hwnd: window::raw_window_handle::Win32WindowHandle,
}

#[cfg(windows)]
impl Win32DialogParent {
    fn new(hwnd: isize) -> Option<Self> {
        use std::num::NonZeroIsize;
        let hwnd = NonZeroIsize::new(hwnd)?;
        Some(Self {
            hwnd: window::raw_window_handle::Win32WindowHandle::new(hwnd),
        })
    }
}

#[cfg(windows)]
impl window::raw_window_handle::HasWindowHandle for Win32DialogParent {
    fn window_handle(
        &self,
    ) -> Result<window::raw_window_handle::WindowHandle<'_>, window::raw_window_handle::HandleError>
    {
        Ok(unsafe {
            window::raw_window_handle::WindowHandle::borrow_raw(
                window::raw_window_handle::RawWindowHandle::Win32(self.hwnd),
            )
        })
    }
}

#[cfg(windows)]
impl window::raw_window_handle::HasDisplayHandle for Win32DialogParent {
    fn display_handle(
        &self,
    ) -> Result<window::raw_window_handle::DisplayHandle<'_>, window::raw_window_handle::HandleError>
    {
        Ok(unsafe {
            window::raw_window_handle::DisplayHandle::borrow_raw(
                window::raw_window_handle::RawDisplayHandle::Windows(
                    window::raw_window_handle::WindowsDisplayHandle::new(),
                ),
            )
        })
    }
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

fn chord_held_on_any_pad(
    required: &[gesture::GestureControl],
    readings: &[start_input::NavReading],
) -> bool {
    if required.is_empty() {
        return false;
    }
    readings
        .iter()
        .any(|r| required.iter().all(|c| r.sample.held.contains(c)))
}
