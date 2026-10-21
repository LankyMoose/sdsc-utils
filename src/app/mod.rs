//! iced `daemon` app for the DualSense battery tray.
//!
//! The daemon boots windowless: it owns the tray icon and only opens windows on
//! demand (controller popup, configure window, overlay toast). The toast window
//! is pre-created hidden after the tray appears so iced keeps a warm GPU
//! compositor; popup, Settings, and Start stay ephemeral (create/destroy on each
//! open — Start uses a normal OS window open).

mod configure;
mod popup;
mod start;
mod toast;

use crate::controller::dualsense::lightbar;
use crate::controller::dualsense::rumble::MotorPulse;
use crate::domain::color::SpectrumService;

#[cfg(debug_assertions)]
use crate::controller::emulate;
use crate::controller::hid::poll::{
    BATTERY_INTERVAL, LIVENESS_INTERVAL, PRESENCE_INTERVAL, PRESENCE_INTERVAL_EMPTY,
    UNREAD_RETRY_INTERVAL,
};
use crate::controller::hid::worker::HidWorkerHandle;
use crate::controller::known::KnownControllers;
#[cfg(debug_assertions)]
use crate::controller::model::Connection;
use crate::controller::model::ControllerStatus;
use crate::games::launch;
use crate::games::process_match::{self, RunningSession};
use crate::games::steam::{self, SteamGame};
use crate::games::{self, GamesCatalog};
use crate::persist::analytics::{self, AnalyticsStore};
use crate::persist::notify::NotifyEvent;
use crate::persist::prefs::{
    NavbarVisibility, Prefs, ReducedMotion, StartAutoOpen, StartSelectionMode,
    clamp_start_screen_haptics_strength, clamp_start_screen_idle_timeouts,
    clamp_start_screen_inactive_dim_percent, clamp_start_screen_sound_volume,
    renderer_restart_needed,
};
use crate::platform::app_log;
#[cfg(windows)]
use crate::platform::autostart;
use crate::platform::ui_sound::{self, UiSoundKind};
#[cfg(windows)]
use crate::platform::win32;
use crate::ui::configure::{
    self as configure_view, AnalyticsPanel, ConfigureMessage, ConfigureState, PadInputPanel,
    Section,
};
use crate::ui::layout::{
    MonitorCover, ToastPlacement, TrayAnchor, cursor_on_primary_monitor, focus_window, move_window,
    open_visible, overlay_platform_specific, primary_monitor_cover, raise_window_topmost,
    window_hwnd,
};
use crate::ui::popup::{self as popup_view, ControllerRow, PopupMessage};
use crate::ui::start::cursor_hide;
use crate::ui::start::gesture::{self, ChordReleaseGate, ChordReleaseTick};
use crate::ui::start::input::{
    self as start_input, CrossHold, FaceHeld, GestureDetectorBank, NavAction, NavLogSnapshot,
    NavSource, PadNavBank,
};
use crate::ui::start::mode::{self as start_mode, StartPresentation};
use crate::ui::start::view::{self as start_view, ReplaceConfirm, StartMessage, StartSlide};
use crate::ui::theme;
use crate::ui::toast::ToastMessage;
use crate::ui::toast::machine::{self as toast_machine, AfterToast};
use crate::ui::toast::view as toast_view;
use crate::ui::tray::{self, QUIT_ID, SETTINGS_ID};

use iced::futures::SinkExt;
use iced::futures::Stream;
use iced::futures::StreamExt;
use iced::futures::channel::{mpsc, oneshot};
use iced::keyboard;
use iced::mouse;
use iced::widget::{container, operation, space, stack};
use iced::{Element, Fill, Point, Size, Subscription, Task, Theme, stream, window};

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tray_icon::TrayIcon;

/// Debounce spectrum prefs save + HID apply after the last color edit.
const SPECTRUM_DEBOUNCE: Duration = Duration::from_millis(150);
/// Debug builds: comma list of windows to open at boot (see [`Message::DebugOpen`]).
#[cfg(debug_assertions)]
const DEBUG_OPEN_ENV: &str = crate::platform::debug_quiet::DEBUG_OPEN_ENV;
/// How long an overlay toast stays on screen.
const TOAST_LIFETIME: Duration = Duration::from_secs(5);
/// DualSense poll rate while pad input is live (~one wired report).
#[allow(dead_code)] // documentation / parity with worker ACTIVE_POLL
const PAD_POLL_ACTIVE: Duration = Duration::from_millis(4);
/// UI animation tick (~60Hz). Prefer this over `window::frames()` so an
/// AlwaysOnTop toast cannot starve other iced windows on Windows (iced#3108).
const UI_TICK: Duration = Duration::from_millis(16);
/// Feedback delay between the reload click (button flips to "Restarting…")
/// and the actual app exit.
const RELOAD_FEEDBACK_DELAY: Duration = Duration::from_millis(700);
/// How long a restore stays in its settle window after relaunch: row
/// rebuilds re-pin the persisted selection by key instead of trusting the
/// index-clamped position (see scan-done). Never used for offsets.
const SCROLL_RESTORE_AUTHORITY: Duration = Duration::from_secs(30);
/// Process/catalog running checks are expensive; never run them at pad-poll rate.
const RUNNING_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// Closed-Start cadence for the running-badge wall-clock tick (open Start
/// refreshes at `RUNNING_CHECK_INTERVAL` through pad polls already). Slower
/// keeps background process scans cheap during real gameplay.
const RUNNING_CHECK_CLOSED_INTERVAL: Duration = Duration::from_secs(5);
const PAD_POLL_STALL_MS: u128 = 80;
const PAD_POLL_SLOW_MS: u128 = 20;
const SNAPSHOT_STALE_MS: u128 = 100;
const START_NAV_NOT_READY_MS: u128 = 200;
/// Ignore Start WindowUnfocused briefly after reveal (toast z-order focus blips).
const START_UNFOCUS_GRACE: Duration = Duration::from_millis(150);
/// Hide the mouse cursor after this idle while immersive Start covers the primary.
const START_CURSOR_IDLE_HIDE: Duration = Duration::from_secs(2);
/// Kick Steam scan if Start never opened / settled (tray-only boot).
const STEAM_SCAN_IDLE_FALLBACK: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub enum Message {
    /// Periodic presence scan.
    Tick,
    /// OS device arrival/removal hint (event-driven wake; see
    /// `presence_hint_stream`). Handled like `Tick` but fires immediately
    /// instead of waiting for the next presence cadence.
    PresenceHint,
    /// Follow-up poll after a suspected absence (see
    /// `maybe_schedule_absence_recheck`).
    AbsenceRecheck,
    /// Debounce clock for a previewed emulated-pad battery level.
    ///
    /// Only subscribed while a preview is pending.
    #[cfg(debug_assertions)]
    EmulatorCommitTick,
    /// Result of a background `poll_controllers` call.
    PollResult(Result<Vec<ControllerStatus>, String>),
    /// Result of a background process-image snapshot (running badge).
    ProcessEnumResult(Result<Vec<(u32, PathBuf)>, String>),
    /// Steam `Apps` registry changed (see `platform::steam_watch`): re-read
    /// the session verdict now instead of waiting for the next badge tick.
    SteamAppsChanged,
    /// Wall-clock tick for the running-game badge. Owns live sessions while
    /// Start is closed / pad-idle so a failed launch cannot freeze `Playing`
    /// until the next open.
    RunningBadgeTick,
    /// Background game-close finished (WM_CLOSE + terminate); badge refresh clears "Closing".
    CloseGameDone(Result<(), String>),

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
    /// Debug-only standalone ambient preview window opened.
    #[cfg(debug_assertions)]
    AmbientPreviewOpened(window::Id),
    /// Debug-only 60Hz preview clock (only while the preview window is open).
    #[cfg(debug_assertions)]
    AmbientPreviewTick,
    /// Debug-only: begin an OS drag of the undecorated preview window.
    #[cfg(debug_assertions)]
    PreviewDrag,
    /// Debug-only: close the ambient preview window.
    #[cfg(debug_assertions)]
    PreviewClose,

    StartOpened(window::Id),
    /// Drive start-screen slide / Triangle-hold animation frames.
    StartFrame,
    /// Mouse move/button/wheel on a window — resets immersive activity (cursor + idle/sleep).
    StartCursorActivity(window::Id),
    /// HWND resize finished — apply immersive flag after promote/demote veil.
    StartImmersiveSettled,
    /// Single-flight art worker milestone (selected / near / job done).
    StartArtEvent(crate::ui::start::art_worker::ArtEvent),
    Start(StartMessage),
    /// Keyboard while some iced window has focus; filtered to the start screen in update.
    StartKey {
        id: window::Id,
        action: StartKeyAction,
        pressed: bool,
    },
    /// Pad input edge from the hid-worker (report-rate; replaces UI PadPoll timer).
    PadInput(crate::domain::pad::InputEdge),
    ManualFilePicked(Option<PathBuf>),
    ManualIconPicked(Option<PathBuf>),
    SteamPathPicked(Option<PathBuf>),
    SteamScanDone(Result<Vec<SteamGame>, String>),
    /// Boot idle fallback: start deferred Steam scan if still not kicked.
    SteamScanIdleFallback,
    /// Post-cold settle delay elapsed — kick background Steam refresh.
    SteamScanAfterSettle,
    /// Debug-only window stress driver (Diagnostics section in Settings):
    /// rapid toast + cold Start + configure + edit churn to shake out
    /// renderer (e.g. wgpu atlas) crashes. Takes over windows while running.
    #[cfg(debug_assertions)]
    StressWindows(u8),
    /// Debug-only: open the windows named in `SDSC_DEBUG_OPEN` after boot
    /// (`popup`, `settings[:section]`, `start`, `toast`; comma-separated) so
    /// UI work can be screenshotted without clicking through the tray.
    #[cfg(debug_assertions)]
    DebugOpen(String),
    /// Debug-only: flip the layout-bounding-box overlay (Diagnostics toggle or F9).
    #[cfg(debug_assertions)]
    ToggleDebugLayoutBounds,

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
    /// Drive the reload-button spinner while a renderer reload is queued.
    RendererReloadTick,
    /// Fire the queued renderer reload (short feedback delay after the click).
    RendererReloadDue,
    /// Slow utility-ambient drift tick (≤2fps, only while a utility window is
    /// visible — popup, Settings, or non-immersive Start). Toasts excluded.
    UtilityAmbientTick,
    /// Begin dismiss (slide-out) for the toast of the given generation.
    ToastDismiss(u64),
    /// Hide toast HWND after a short defer (avoid create_renderer vs hide stress).
    ToastHideDeferred,

    /// Debounced spectrum prefs save + lightbar HID apply.
    SpectrumCommit(u64),

    Exit,
}

impl Message {
    /// True for user-originated input: pad, mouse, keyboard, tray, and file
    /// pickers. While a renderer reload is queued, `update()` drops these so
    /// nothing can act in the feedback window before exit.
    ///
    /// Deliberately exhaustive (no wildcard): a new variant fails to compile
    /// here until it is classified as input or system. Ticks, background
    /// results, and window lifecycle are system.
    ///
    /// Tray Quit is NOT input: quitting cancels the reload and must always
    /// work.
    fn is_user_input(&self) -> bool {
        match self {
            Message::Tick | Message::PresenceHint | Message::AbsenceRecheck => false,
            #[cfg(debug_assertions)]
            Message::EmulatorCommitTick => false,
            Message::PollResult(_)
            | Message::ProcessEnumResult(_)
            | Message::RunningBadgeTick
            | Message::SteamAppsChanged
            | Message::CloseGameDone(_) => false,
            Message::TrayMenu(id) => id.as_str() != QUIT_ID,
            Message::TrayLeftClick(_) => true,
            Message::WindowClosed(_)
            | Message::WindowUnfocused(_)
            | Message::PopupOpened(_)
            | Message::PlacePopup { .. }
            | Message::ConfigureOpened(_)
            | Message::StartOpened(_)
            | Message::StartFrame
            | Message::StartArtEvent(_)
            | Message::StartImmersiveSettled => false,
            #[cfg(debug_assertions)]
            Message::AmbientPreviewOpened(_) | Message::AmbientPreviewTick => false,
            #[cfg(debug_assertions)]
            Message::PreviewDrag | Message::PreviewClose => true,
            Message::Popup(_)
            | Message::Configure(_)
            | Message::Start(_)
            | Message::StartCursorActivity(_)
            | Message::PadInput(_) => true,
            Message::StartKey { .. } => true,
            Message::ManualFilePicked(_)
            | Message::ManualIconPicked(_)
            | Message::SteamPathPicked(_) => true,
            Message::SteamScanDone(_)
            | Message::SteamScanIdleFallback
            | Message::SteamScanAfterSettle => false,
            #[cfg(debug_assertions)]
            Message::StressWindows(_) => true,
            #[cfg(debug_assertions)]
            Message::DebugOpen(_) => true,
            #[cfg(debug_assertions)]
            Message::ToggleDebugLayoutBounds => true,
            Message::PlaceToast { .. }
            | Message::ToastShown { .. }
            | Message::ToastFrame
            | Message::IdentifyFrame
            | Message::RendererReloadTick
            | Message::RendererReloadDue
            | Message::UtilityAmbientTick
            | Message::ToastDismiss(_)
            | Message::ToastHideDeferred
            | Message::SpectrumCommit(_) => false,
            Message::Exit => false,
        }
    }
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

/// Animated promote growth endpoints: centered compact → monitor cover.
#[derive(Debug, Clone, Copy)]
struct ResizeAnim {
    from_size: Size,
    to_size: Size,
    from_pos: Point,
    to_pos: Point,
    started: Instant,
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
    /// An absence-recheck poll is already queued (one-shot; see
    /// `maybe_schedule_absence_recheck`). Prevents a miss from scheduling
    /// a recheck per liveness tick while a pad flaps.
    absence_recheck_pending: bool,
    /// Serializes DualSense HID for the daemon (never on the UI thread).
    hid_worker: HidWorkerHandle,
    /// Owned battery spectrum shared with the HID worker (clone-then-release
    /// reads; never held across HID writes).
    spectrum: SpectrumService,

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
    /// Debug-only standalone ambient preview window (Diagnostics "View
    /// immersive"): immersive-rate background without opening Start.
    #[cfg(debug_assertions)]
    preview_window: Option<window::Id>,
    /// Debug-only R3 release latch for the tuning chrome toggle (holding R3
    /// must not strobe the hide/show).
    #[cfg(debug_assertions)]
    ambient_r3_latched: bool,
    /// Debug-only window stress test in flight (Diagnostics section).
    #[cfg(debug_assertions)]
    stress_running: bool,
    /// Debug-only layout-bounds overlay (Diagnostics toggle / F9). Local UI
    /// state: never persisted.
    #[cfg(debug_assertions)]
    pub debug_layout_bounds: bool,
    /// Debug-only ambient pulse + ken-burns tuning (Diagnostics sliders).
    /// Local UI state: never persisted; defaults
    /// reproduce release visuals. Mirrored into the process-wide tuning
    /// atomics (`ui::shader::ambient`, `ui::start::mode`) on every edit.
    #[cfg(debug_assertions)]
    debug_ambient_freq: f32,
    #[cfg(debug_assertions)]
    debug_ambient_veil_freq: f32,
    #[cfg(debug_assertions)]
    debug_ambient_pulse_min: f32,
    #[cfg(debug_assertions)]
    debug_ambient_pulse_max: f32,
    #[cfg(debug_assertions)]
    debug_ambient_orb_speed: f32,
    #[cfg(debug_assertions)]
    debug_ambient_glow_hue: f32,
    #[cfg(debug_assertions)]
    debug_ambient_orb_min_x: f32,
    #[cfg(debug_assertions)]
    debug_ambient_orb_max_x: f32,
    #[cfg(debug_assertions)]
    debug_ambient_orb_min_y: f32,
    #[cfg(debug_assertions)]
    debug_ambient_orb_max_y: f32,
    #[cfg(debug_assertions)]
    debug_ambient_grain: f32,
    #[cfg(debug_assertions)]
    debug_ambient_time_scale: f32,
    #[cfg(debug_assertions)]
    debug_kenburns_amp: f32,
    #[cfg(debug_assertions)]
    debug_kenburns_pan: f32,
    #[cfg(debug_assertions)]
    debug_kenburns_speed: f32,
    configure_state: ConfigureState,
    /// Snapshot for the Analytics settings tab (refreshed with controller/analytics changes).
    analytics_panel: AnalyticsPanel,
    pad_input_panel: PadInputPanel,

    start_window: Option<window::Id>,
    /// True only while Start is revealed (warm HWND may exist while false).
    start_visible: bool,
    /// Last Start-window paint (for the transition paint-gap witness;
    /// `Cell` because `view()` only gets `&self`).
    last_start_paint_at: std::cell::Cell<Option<Instant>>,
    /// Whether that paint happened with a transition ceremony live (gates the
    /// witness so idle gaps never log).
    last_start_paint_active: std::cell::Cell<bool>,
    /// Demote-settle scroll stowed for the next tick, when the remounted
    /// list exists. Fires once, then clears.
    deferred_settle_scroll: Option<(start_view::StartListId, f32)>,
    /// Animated promote growth (compact → cover) while `transition` is
    /// `Promoting` / phase is `Resizing`. Stepped resizes keep presenting;
    /// content holds black so joints are invisible.
    start_resize_anim: Option<ResizeAnim>,
    /// False until Start is revealed and ready — blocks pad nav / sounds early.
    start_nav_ready: bool,
    start_state: start_view::State,
    /// Monitor covered while immersive (used to center compact on demote).
    start_monitor_cover: Option<MonitorCover>,
    /// Last user activity on Start (mouse / keyboard / pad) for immersive idle/sleep.
    start_last_activity: Instant,
    /// Last mouse activity on Start (immersive cursor hide only — pad must not unhide).
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
    /// Steam appids whose install dir failed pre-flight (missing/empty):
    /// launch is blocked and the row dims until the next successful scan.
    launch_blocked_steam: HashSet<u32>,
    /// Match paths for the target currently showing "Closing" (cleared on process exit).
    closing_match_paths: Vec<PathBuf>,
    /// Throttle for process enumeration / catalog restore (not pad UI).
    last_running_check: Option<Instant>,
    /// True while a background process enum task is outstanding.
    process_enum_inflight: bool,
    /// Cached `match_paths_for_target` results (cleared on Steam library refresh).
    match_path_cache: HashMap<String, Vec<PathBuf>>,
    steam_by_id: HashMap<u32, SteamGame>,
    /// True until the first `SteamScanDone` (Ok or Err).
    steam_scan_pending: bool,
    /// Wall clock when the current Steam library scan was kicked off (debug timing).
    steam_scan_started: Option<Instant>,
    /// True after the first live `SteamScanDone` this process (cache seed does not set it).
    steam_library_refreshed: bool,
    /// `Some` after a successful Steam library scan (installed appids). `None` = unknown.
    steam_installed: Option<Vec<u32>>,
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
    /// Last hid-worker edge seq seen (gap detection for silently dropped edges).
    last_edge_seq: Option<u64>,
    /// One-shot while snapshot is empty (tracks gap start).
    nav_missing_since: Option<Instant>,
    /// One-shot while snapshot is non-empty but stale.
    nav_stale_warned: bool,
    /// One-shot when start_nav_ready stays false after open.
    nav_not_ready_warned: bool,
    /// When the start window was opened (for not-ready timing).
    start_opened_at: Option<Instant>,
    /// Last applied start-screen selection (restart restores, row rebuilds
    /// re-pin by key while the restore settle window is fresh).
    start_selection_shadow: Option<crate::session::StartSelection>,
    /// Renderer prefs at boot (AA/vsync are boot-only iced settings).
    renderer_boot_aa: bool,
    renderer_boot_vsync: bool,
    /// True while exiting for a renderer-pref reload (keeps the restore
    /// record; plain Quit clears it so a relaunch starts clean).
    restarting_for_renderer: bool,
    /// Reload button pressed, restart due after a short feedback delay.
    /// Disables the button and spins both reload buttons until exit.
    renderer_reload_queued: bool,
    /// One-shot immersive override for the next `open_start_screen` (reload
    /// restore reopens the pre-restart presentation even when the
    /// always-immersive pref would open compact).
    force_immersive_open: Option<bool>,
    /// One-shot nav-reset bypass for the next `open_start_screen`. Reload
    /// restore sets all UI state (drawer, scrolls, selections) BEFORE
    /// opening; the open path must not `reset_to_games()` it away.
    restore_preserve_nav: bool,
    /// Saved list offsets (games / controllers / settings drawer) to re-apply
    /// on `StartOpened`, when the scrollables exist. One-shot per window
    /// open and never re-asserted: once rows rebuild, offsets go stale and
    /// only the persisted selection stays authoritative. The drawer offset
    /// applies once here; the drawer is row-independent, so scans and
    /// snapshots must leave it alone.
    pending_scroll_restore: Option<(f32, f32, f32)>,
    /// Settle window after a restore during which row rebuilds re-pin the
    /// persisted selection by key (see `SCROLL_RESTORE_AUTHORITY`).
    scroll_restore_until: Option<Instant>,
    /// Saved Configure content offset, applied once on `ConfigureOpened`.
    pending_configure_scroll: Option<f32>,

    toast_window: Option<window::Id>,
    toast_message: Option<ToastMessage>,
    toast_queue: VecDeque<ToastMessage>,
    toast_generation: u64,
    toast_placement: Option<ToastPlacement>,
    /// Last frame Instant for capped slide dt (set on ToastShown / dismiss / each frame).
    toast_anim_started: Instant,
    /// Pure toast / deferred-Start presentation machine.
    toast_machine: toast_machine::State,
    /// True until the first Start HWND of this process finishes its cold-load ceremony.
    start_cold_load_remaining: bool,
    /// Compact Steam settle fired during promote ceremony — kick after EnterImmersive.
    steam_scan_deferred_for_promote: bool,

    /// Bumped on every spectrum edit; stale SpectrumCommit messages are ignored.
    spectrum_generation: u64,

    /// Emulated pads the developer emulator knows about, connected or not.
    /// Debug-only; the session sees the connected slice via
    /// [`App::live_controllers`].
    #[cfg(debug_assertions)]
    emulated_pads: Vec<emulate::EmulatedPad>,
    /// Battery levels previewed but not yet published, keyed by serial.
    #[cfg(debug_assertions)]
    pending_emulator_commits: std::collections::HashMap<String, PendingEmulatorCommit>,
}

/// A battery level the slider previewed, waiting out its debounce.
///
/// Publishing runs the normal connect/notify path, so it is deliberately
/// debounced: a drag must not emit a toast (or open Start) per frame.
/// The previewed level lives here until the debounce commits it.
#[cfg(debug_assertions)]
struct PendingEmulatorCommit {
    deadline: Instant,
    /// Advanced by the commit tick so the pending spinner animates.
    tick: u32,
    /// Previewed level, shown in the tab until the commit lands.
    percent: u8,
}

/// Quiet period after the last battery-slider change before it publishes.
#[cfg(debug_assertions)]
const EMULATOR_COMMIT_DEBOUNCE: Duration = Duration::from_secs(1);
/// How often the debounce is re-checked (and the pending spinner advanced).
#[cfg(debug_assertions)]
const EMULATOR_COMMIT_POLL: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// In-process daemon: tray, HID, session, and iced UI in one process.
///
/// Developer tooling (Configure → Controllers, emulated pads) is compiled
/// in for every non-release build, so a plain `cargo run` already has it — no
/// flag needed. Release builds drop it at compile time.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    crate::platform::wgpu_diag::log_adapters_at_boot();

    // Boot-only renderer prefs: iced `Settings` cannot flip at runtime, so the
    // app reads them before the daemon builds. `App::boot` re-loads the same
    // file for session state; prefs win over any `ICED_PRESENT_MODE` env.
    let prefs = Prefs::load();
    let settings = iced::Settings {
        antialiasing: prefs.perf_antialiasing,
        vsync: prefs.perf_vsync,
        ..iced::Settings::default()
    };
    let boot = App::boot;

    iced::daemon(boot, App::update, App::view)
        .subscription(App::subscription)
        .title(App::title)
        .theme(App::theme)
        .style(App::style)
        .settings(settings)
        .run()?;

    Ok(())
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

impl App {
    fn boot() -> (Self, Task<Message>) {
        let prefs = Prefs::load();
        let known = KnownControllers::load();
        let analytics = AnalyticsStore::load();
        let games = GamesCatalog::load();
        let spectrum = SpectrumService::new(prefs.spectrum.clone());
        lightbar::set_enabled(prefs.lightbar_enabled);

        #[cfg(windows)]
        autostart::ensure_quiet_entry();

        let hid_worker = HidWorkerHandle::start(spectrum.clone());
        let identifying = hid_worker.identifying();

        let configure_state = ConfigureState::new(prefs.spectrum.clone());

        // Rebuild the Controllers-tab fleet from remembered emulated serials,
        // so a pad added in a previous run is still listed after a restart
        // instead of lingering only in the popup's remembered list. Must be
        // read before `known` moves into the session; all pads come back
        // disconnected.
        #[cfg(debug_assertions)]
        let emulated_pads = {
            let remembered = known
                .remembered_disconnected(&[])
                .into_iter()
                .map(|r| {
                    (
                        r.serial.clone(),
                        r.percent,
                        Connection::from_label(&r.connection).unwrap_or(Connection::Bluetooth),
                    )
                })
                .collect::<Vec<_>>();
            emulate::rehydrate(&remembered)
        };
        #[cfg(debug_assertions)]
        let debug_tune = crate::ui::configure::DebugAmbientTune::default();
        let session = crate::session::DeviceSession::new(prefs, known, analytics);
        // Boot-only renderer prefs (iced Settings); a later change auto-restarts.
        let (renderer_boot_aa, renderer_boot_vsync) =
            (session.prefs.perf_antialiasing, session.prefs.perf_vsync);

        let last_discovered = hid_worker.presence_paths();
        let mut app = Self {
            session,
            games,
            edit_draft: None,
            last_discovered,
            last_battery_poll: Instant::now(),
            identifying,
            refreshing: Arc::new(AtomicBool::new(false)),
            absence_recheck_pending: false,
            hid_worker,
            spectrum,
            tray_icon: None,
            tray_anchor: TrayAnchor::default(),
            popup_window: None,
            popup_scale: 1.0,
            popup_monitor: None,
            popup_metrics_cached: false,
            popup_state: popup_view::State::default(),
            popup_rows: Vec::new(),
            configure_window: None,
            #[cfg(debug_assertions)]
            preview_window: None,
            #[cfg(debug_assertions)]
            ambient_r3_latched: false,
            #[cfg(debug_assertions)]
            stress_running: false,
            #[cfg(debug_assertions)]
            debug_layout_bounds: false,
            #[cfg(debug_assertions)]
            debug_ambient_freq: debug_tune.pulse_freq,
            #[cfg(debug_assertions)]
            debug_ambient_veil_freq: debug_tune.veil_freq,
            #[cfg(debug_assertions)]
            debug_ambient_pulse_min: debug_tune.pulse_min,
            #[cfg(debug_assertions)]
            debug_ambient_pulse_max: debug_tune.pulse_max,
            #[cfg(debug_assertions)]
            debug_ambient_orb_speed: debug_tune.orb_speed,
            #[cfg(debug_assertions)]
            debug_ambient_glow_hue: debug_tune.glow_hue,
            #[cfg(debug_assertions)]
            debug_ambient_orb_min_x: debug_tune.orb_min_x,
            #[cfg(debug_assertions)]
            debug_ambient_orb_max_x: debug_tune.orb_max_x,
            #[cfg(debug_assertions)]
            debug_ambient_orb_min_y: debug_tune.orb_min_y,
            #[cfg(debug_assertions)]
            debug_ambient_orb_max_y: debug_tune.orb_max_y,
            #[cfg(debug_assertions)]
            debug_ambient_grain: debug_tune.grain_gain,
            #[cfg(debug_assertions)]
            debug_ambient_time_scale: debug_tune.time_scale,
            #[cfg(debug_assertions)]
            debug_kenburns_amp: debug_tune.kenburns_amp,
            #[cfg(debug_assertions)]
            debug_kenburns_pan: debug_tune.kenburns_pan,
            #[cfg(debug_assertions)]
            debug_kenburns_speed: debug_tune.kenburns_speed,
            configure_state,
            analytics_panel: AnalyticsPanel::default(),
            pad_input_panel: PadInputPanel::default(),
            start_window: None,
            start_visible: false,
            last_start_paint_at: std::cell::Cell::new(None),
            last_start_paint_active: std::cell::Cell::new(false),
            deferred_settle_scroll: None,
            start_nav_ready: false,
            start_state: start_view::State::default(),
            start_monitor_cover: None,
            start_resize_anim: None,
            start_last_activity: Instant::now(),
            start_cursor_last_active: Instant::now(),
            start_cursor_hidden: false,
            start_file_dialog_open: false,
            chord_release_gate: ChordReleaseGate::default(),
            start_revealed_at: None,
            gesture_detectors: GestureDetectorBank::default(),
            pad_nav: PadNavBank::default(),
            haptic_pad_serial: None,
            keyboard_cross_hold: CrossHold::default(),
            confirm_key_held: false,
            cancel_key_held: false,
            pad_held: FaceHeld::default(),
            running_session: None,
            launch_blocked_steam: HashSet::new(),
            closing_match_paths: Vec::new(),
            last_running_check: None,
            process_enum_inflight: false,
            match_path_cache: HashMap::new(),
            steam_by_id: HashMap::new(),
            steam_scan_pending: true,
            steam_scan_started: None,
            steam_library_refreshed: false,
            steam_installed: None,
            nav_missing_warned: false,
            nav_unarmed_warned: false,
            nav_source_logged: None,
            last_nav_log: None,
            last_pad_poll_at: None,
            last_edge_seq: None,
            nav_missing_since: None,
            nav_stale_warned: false,
            nav_not_ready_warned: false,
            start_opened_at: None,
            start_selection_shadow: None,
            renderer_boot_aa,
            renderer_boot_vsync,
            restarting_for_renderer: false,
            renderer_reload_queued: false,
            force_immersive_open: None,
            restore_preserve_nav: false,
            pending_scroll_restore: None,
            scroll_restore_until: None,
            pending_configure_scroll: None,
            toast_window: None,
            toast_message: None,
            toast_queue: VecDeque::new(),
            toast_generation: 0,
            toast_placement: None,
            toast_anim_started: Instant::now(),
            toast_machine: toast_machine::State::Idle,
            start_cold_load_remaining: true,
            steam_scan_deferred_for_promote: false,
            spectrum_generation: 0,
            #[cfg(debug_assertions)]
            emulated_pads,
            #[cfg(debug_assertions)]
            pending_emulator_commits: std::collections::HashMap::new(),
        };

        // Show the tray immediately; the daemon owns it for its whole lifetime.
        app.create_tray();
        #[cfg(all(windows, debug_assertions))]
        start_hitch_hotkey_worker();
        app.sync_popup_rows();
        app.sync_low_battery();
        app.refresh_analytics_panel();
        app.seed_steam_library_from_cache();

        // Renderer-pref restart: reopen to the pre-restart UI state.
        // Consumed once; stale (>120s) records are ignored by `take()`.
        let restart_restore = crate::persist::restart_ui::take()
            .map(|rs| app.apply_restart_ui(rs))
            .unwrap_or_else(Task::none);

        let toast_boot = app
            .ensure_toast_window()
            .chain(app.maybe_show_crash_restart_toast());
        // Live Steam scan kicks on StartOpened (or idle fallback when Start is disabled).
        let steam_fallback = Task::perform(delay(STEAM_SCAN_IDLE_FALLBACK), |()| {
            Message::SteamScanIdleFallback
        });
        #[cfg_attr(not(debug_assertions), allow(unused_mut))]
        let mut task = Task::batch([
            app.request_refresh(),
            toast_boot,
            steam_fallback,
            restart_restore,
        ]);
        #[cfg(debug_assertions)]
        if let Ok(spec) = std::env::var(DEBUG_OPEN_ENV) {
            // Let the tray / toast window settle before opening on top of them.
            task = Task::batch([
                task,
                Task::perform(delay(Duration::from_millis(1500)), move |()| {
                    Message::DebugOpen(spec.clone())
                }),
            ]);
        }
        (app, task)
    }

    /// Open windows for `SDSC_DEBUG_OPEN` (see [`Message::DebugOpen`]).
    #[cfg(debug_assertions)]
    fn debug_open(&mut self, spec: &str) -> Task<Message> {
        use crate::ui::configure::Section;
        let mut tasks = Vec::new();
        for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (name, arg) = item.split_once(':').unwrap_or((item, ""));
            app_log::info(format!("debug-open: {name} {arg}"));
            match name {
                "popup" => tasks.push(self.open_popup()),
                "settings" => {
                    let section = match arg {
                        "start" => Some(Section::StartScreen),
                        "notifications" => Some(Section::Notifications),
                        "toast" => Some(Section::Notifications),
                        "lightbar" => Some(Section::Lightbar),
                        "analytics" => Some(Section::Analytics),
                        "diagnostics" => Some(Section::Diagnostics),
                        _ => None,
                    };
                    tasks.push(self.open_configure());
                    if let Some(section) = section {
                        self.configure_state.select_section(section);
                        // Fresh windows mount at offset 0; for an already-open
                        // window this resets the content scroll to top.
                        tasks.push(operation::scroll_to(
                            configure_view::content_scroll_id(),
                            operation::AbsoluteOffset {
                                x: None,
                                y: Some(0.0),
                            },
                        ));
                    }
                }
                "start" => tasks.push(self.open_start_screen()),
                "start-controllers" => {
                    tasks.push(self.open_start_screen());
                    self.start_state.request_slide(
                        crate::ui::start::view::StartSlide::Controllers,
                        Instant::now(),
                    );
                }
                "start-edit" => {
                    tasks.push(self.open_start_screen());
                    let _ = self.toggle_start_edit();
                }
                "toast" => {
                    ToastMessage::enqueue(
                        &mut self.toast_queue,
                        ToastMessage::preview(&self.session.prefs.spectrum),
                    );
                    tasks.push(self.show_next_toast());
                }
                other => app_log::warn(format!("debug-open: unknown window {other:?}")),
            }
        }
        Task::batch(tasks)
    }

    fn title(&self, window: window::Id) -> String {
        #[cfg(debug_assertions)]
        if Some(window) == self.preview_window {
            return "Ambient preview".to_string();
        }
        if Some(window) == self.configure_window {
            "Settings".to_string()
        } else if Some(window) == self.start_window {
            String::new()
        } else {
            crate::platform::app_meta::DISPLAY_NAME.to_string()
        }
    }

    /// Clear color per frame. Transparent windows (alpha-capable backend) must
    /// clear to transparent so self-drawn rounded corners show the desktop;
    /// every window paints its own full backdrop on top.
    fn style(&self, theme: &Theme) -> iced::theme::Style {
        let palette = theme.palette();
        iced::theme::Style {
            background_color: if crate::ui::chrome::transparent_windows() {
                iced::Color::TRANSPARENT
            } else {
                palette.background
            },
            text_color: palette.text,
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
                    #[cfg(debug_assertions)]
                    if key == keyboard::Key::Named(keyboard::key::Named::F9) {
                        return Some(Message::ToggleDebugLayoutBounds);
                    }
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

        subscriptions.push(
            iced::time::every(
                if self.session.prefs.start_screen_enabled && self.session.controllers.is_empty() {
                    PRESENCE_INTERVAL_EMPTY
                } else {
                    PRESENCE_INTERVAL
                },
            )
            .map(|_| Message::Tick),
        );
        subscriptions.push(Subscription::run(tray_events_mapped));
        // OS device arrival/removal wake (event-driven; the Tick timer above
        // is only a backstop). Always subscribed like tray events: the stream
        // owns its watcher registration for the process lifetime.
        subscriptions.push(Subscription::run(presence_hint_stream));

        if self.toast_message.is_some() {
            subscriptions.push(Subscription::run_with(
                self.toast_generation,
                |generation| escape_hotkey(*generation),
            ));
        }

        // Only spun up while a previewed emulated battery level is waiting out
        // its debounce — an idle app subscribes to nothing extra.
        #[cfg(debug_assertions)]
        if !self.pending_emulator_commits.is_empty() {
            subscriptions
                .push(iced::time::every(EMULATOR_COMMIT_POLL).map(|_| Message::EmulatorCommitTick));
        }

        if self.toast_message.is_some() || self.toast_machine.wants_frames() {
            // Tick for the whole toast lifetime so content swaps present, and while
            // Placing so the placement failsafe can fire without a Shown event.
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::ToastFrame));
        }

        // Transient: animate the reload-button spinner until the queued exit.
        if self.renderer_reload_queued {
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::RendererReloadTick));
        }

        if self.popup_state.identify_flash_active()
            || self.start_state.identify_flash_active()
            || (self.popup_window.is_some() && self.popup_state.entrance_active(Instant::now()))
        {
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::IdentifyFrame));
        }

        let pad_input_live =
            self.configure_window.is_some() && self.configure_state.section == Section::PadInput;
        let pad_listening = pad_input_live
            || (self.session.prefs.start_screen_enabled && !self.session.controllers.is_empty());
        self.hid_worker.set_input_hot(pad_listening);
        if pad_listening {
            subscriptions.push(Subscription::run(pad_input_edge_stream));
        }

        if self.start_visible && (self.start_state.needs_frames() || self.steam_scan_in_flight()) {
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::StartFrame));
        }

        if self.popup_window.is_some()
            || self.configure_window.is_some()
            || (self.start_visible && !self.start_state.immersive)
        {
            // Slow utility-ambient drift (backdrop.rs): ≤2fps redraws only while
            // a utility window is actually visible; hidden/idle windows never
            // tick, so they never redraw for this.
            subscriptions.push(
                iced::time::every(crate::ui::backdrop::UTILITY_TICK)
                    .map(|_| Message::UtilityAmbientTick),
            );
        }

        // Standalone ambient preview (debug Diagnostics "View immersive"):
        // 60Hz immersive-rate clock, only while the preview is open.
        #[cfg(debug_assertions)]
        if self.preview_window.is_some() {
            subscriptions.push(
                iced::time::every(crate::ui::ambient_preview::PREVIEW_TICK)
                    .map(|_| Message::AmbientPreviewTick),
            );
        }

        if self.session.prefs.start_screen_enabled {
            subscriptions.push(
                Subscription::run(crate::ui::start::art_worker::event_stream)
                    .map(Message::StartArtEvent),
            );
        }

        // Wall-clock owner for the running-game badge: pad edges only flow
        // while Start is open and pads are live, so without this a failed
        // immersive launch (Start auto-closed) would freeze `Playing` until
        // the next open. Only subscribed while a session is alive.
        if self.running_session.is_some() {
            let interval = if self.start_visible {
                RUNNING_CHECK_INTERVAL
            } else {
                RUNNING_CHECK_CLOSED_INTERVAL
            };
            subscriptions.push(iced::time::every(interval).map(|_| Message::RunningBadgeTick));
            subscriptions.push(Subscription::run(steam_apps_watch_stream));
        }

        Subscription::batch(subscriptions)
    }

    fn view(&self, window: window::Id) -> Element<'_, Message> {
        if Some(window) == self.popup_window {
            return popup_view::view(
                &self.popup_state,
                &self.popup_rows,
                &self.session.prefs.spectrum,
                self.start_state.powering_off.as_deref(),
            )
            .map(Message::Popup);
        }

        #[cfg(debug_assertions)]
        if Some(window) == self.preview_window {
            return crate::ui::ambient_preview::view(Message::PreviewDrag, Message::PreviewClose);
        }

        if Some(window) == self.configure_window {
            #[cfg(debug_assertions)]
            let debug_tune = self.debug_tune_snapshot();
            return configure_view::view(
                &self.configure_state,
                &self.configure_settings(),
                &self.analytics_panel,
                &self.pad_input_panel,
                #[cfg(debug_assertions)]
                &self.emulated_pads,
                #[cfg(debug_assertions)]
                &self.pending_emulator_changes(),
                #[cfg(debug_assertions)]
                self.debug_layout_bounds,
                #[cfg(debug_assertions)]
                &debug_tune,
            )
            .map(Message::Configure);
        }

        if Some(window) == self.start_window {
            // Paint-gap witness (black-lockup forensics): while a transition
            // ceremony is live the window must repaint ~60Hz. A gap here
            // means paints starved while logic ran ahead — the render/present
            // side, never the state machine. Armed only across live phases,
            // so idle gaps never log.
            let paint_now = Instant::now();
            let paint_active = self.start_state.transition_phase.is_some()
                || self.start_state.transition.is_some();
            if paint_active
                && self.last_start_paint_active.get()
                && let Some(last) = self.last_start_paint_at.get()
            {
                let gap_ms = paint_now.saturating_duration_since(last).as_millis() as u64;
                if gap_ms > 250 {
                    let phase = self
                        .start_state
                        .transition_phase
                        .map(|(p, _)| p.label())
                        .unwrap_or("none");
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: start paint gap gap_ms={gap_ms} phase={phase}"
                    ));
                }
            }
            self.last_start_paint_at.set(Some(paint_now));
            self.last_start_paint_active.set(paint_active);
            let cover = self.start_monitor_cover.map(|c| (c.width, c.height));
            let stage_w = cover.map(|(w, _)| w).unwrap_or(1920.0);
            let stage_h = cover.map(|(_, h)| h).unwrap_or(1080.0);
            let settings_snapshot = self.start_settings_snapshot();
            let start = start_view::view(
                &self.start_state,
                &self.session.prefs.spectrum,
                Instant::now(),
                self.session.prefs.start_screen_always_immersive,
                self.session.prefs.start_screen_clock_enabled,
                // Prefs always normalize to PS on load/save.
                &self.session.prefs.start_screen_gesture,
                stage_h,
                stage_w,
                &settings_snapshot,
                #[cfg(debug_assertions)]
                self.debug_layout_bounds,
            )
            .map(Message::Start);
            // Debug R3 hide shows pure ambience: no toast composite either.
            #[cfg(debug_assertions)]
            let start = if self.start_state.ambient_tuning_active()
                && self.start_state.ambient_tune_hide_chrome
            {
                start
            } else {
                match self.immersive_toast_overlay() {
                    Some(toast) => stack![start, toast].width(Fill).height(Fill).into(),
                    None => start,
                }
            };
            #[cfg(not(debug_assertions))]
            let start = match self.immersive_toast_overlay() {
                Some(toast) => stack![start, toast].width(Fill).height(Fill).into(),
                None => start,
            };
            return cursor_hide::force_cursor(start, self.start_cursor_hidden).into();
        }

        if Some(window) == self.toast_window {
            // Immersive Start composites the card; keep the toast HWND blank.
            if self.immersive_hosts_toast() {
                return toast_view::empty();
            }
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
        // Effective reduce-motion gate (prefs choice vs OS "Show animations"
        // toggle): synced on every update so pad/keyboard/mouse-initiated
        // anims gate instantly, even with an idle Start (no StartFrame) or
        // prefs freshly arrived via snapshot. Re-syncing while set is a
        // no-op (chases are already settled); StartFrame re-syncs too so an
        // OS-toggle flip lands mid-session.
        self.start_state
            .sync_reduced_motion(crate::ui::motion::reduced_motion_effective(
                self.session.prefs.reduced_motion,
            ));
        self.start_state
            .sync_perf_throttle(self.session.prefs.perf_throttle_immersive);
        // Debug tuning keep-alive follows the local fields (the tuning
        // arms below re-sync too, since this runs before the match).
        #[cfg(debug_assertions)]
        {
            self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
        }
        // Renderer reload queued: drop all user input (pad / mouse /
        // keyboard / tray / pickers) so nothing can act in the feedback
        // window before exit. Window lifecycle and ticks flow untouched;
        // Tray Quit still exits (and cancels the reload).
        if self.renderer_reload_queued && message.is_user_input() {
            return Task::none();
        }
        match message {
            Message::Tick => self.on_tick(),
            Message::PresenceHint => self.on_presence_hint(),
            Message::AbsenceRecheck => self.on_absence_recheck(),
            #[cfg(debug_assertions)]
            Message::ToggleDebugLayoutBounds => {
                self.debug_layout_bounds = !self.debug_layout_bounds;
                Task::none()
            }
            #[cfg(debug_assertions)]
            Message::EmulatorCommitTick => self.on_emulator_commit_tick(),
            Message::PollResult(result) => self.on_poll_result(result),
            Message::ProcessEnumResult(result) => self.on_process_enum_result(result),
            Message::SteamAppsChanged => self.refresh_running_badge_inner(true),
            Message::RunningBadgeTick => self.refresh_running_badge(),
            Message::CloseGameDone(result) => self.on_close_game_done(result),

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
                    self.sync_toast_zorder()
                } else if Some(id) == self.start_window {
                    self.start_window = None;
                    self.start_visible = false;
                    self.start_nav_ready = false;
                    self.pad_nav.reset();
                    self.clear_start_nav_diag();
                    self.consume_reopen_gesture_chord();
                    let sync = self.sync_toast_zorder();
                    // The auto-open latch survives a close so a pending
                    // open can retry once presence still qualifies.
                    if self.session.start_auto_open_pending
                        && self.session.prefs.start_screen_enabled
                        && self.session.prefs.start_screen_auto_open.auto_opens()
                        && crate::session::has_start_presence(
                            &self.session.controllers,
                            self.session.prefs.start_screen_auto_open.includes_usb(),
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
                    #[cfg(debug_assertions)]
                    if Some(id) == self.preview_window {
                        self.preview_window = None;
                    }
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

            Message::PopupOpened(id) => self.handle_popup_opened(id),
            Message::PlacePopup { id, scale, monitor } => {
                self.handle_place_popup(id, scale, monitor)
            }
            Message::Popup(message) => self.on_popup_message(message),

            Message::ConfigureOpened(id) => {
                let mut task = focus_window(id);
                // Renderer-reload restore: re-apply the saved content offset
                // once, now that the scrollable exists.
                if let Some(y) = self.pending_configure_scroll.take() {
                    task = task.chain(operation::scroll_to(
                        configure_view::content_scroll_id(),
                        operation::AbsoluteOffset {
                            x: None,
                            y: Some(y),
                        },
                    ));
                }
                task
            }
            Message::Configure(message) => self.on_configure_message(message),

            #[cfg(debug_assertions)]
            Message::AmbientPreviewOpened(id) => focus_window(id),
            #[cfg(debug_assertions)]
            Message::AmbientPreviewTick => {
                crate::ui::ambient_preview::note_preview_tick();
                Task::none()
            }
            #[cfg(debug_assertions)]
            Message::PreviewDrag => match self.preview_window {
                Some(id) => window::drag(id),
                None => Task::none(),
            },
            #[cfg(debug_assertions)]
            Message::PreviewClose => match self.preview_window {
                // Hide first so DWM cannot flash while the surface tears down;
                // keep id until WindowClosed (same as Settings).
                Some(id) => window::set_mode(id, window::Mode::Hidden).chain(window::close(id)),
                None => Task::none(),
            },

            Message::StartOpened(id) => {
                if Some(id) == self.start_window && self.start_visible {
                    self.start_nav_ready = true;
                    self.nav_not_ready_warned = false;
                    let now = Instant::now();
                    self.start_revealed_at = Some(now);
                    self.start_state.steam_scan_pending = self.steam_scan_pending;
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: start opened id={id:?}"
                    ));
                    // Raise/focus after create_renderer has a chance to finish —
                    // chaining sync on the same turn as window::open correlated
                    // with atlas create_renderer panics.
                    let mut task = focus_window(id)
                        .chain(raise_window_topmost(id))
                        .chain(self.sync_toast_zorder());
                    // Settings restore first: the drawer/modal scroll lands
                    // before the list reveals, art kicks, and scan below.
                    // Games/controllers follow after the branch.
                    let mut pending_lists = None;
                    if let Some((games_y, controllers_y, settings_y)) =
                        self.pending_scroll_restore.take()
                    {
                        let settings_id = start_view::StartListId::Settings.scroll_id();
                        self.start_state.note_scroll_y(&settings_id, settings_y);
                        task = task.chain(operation::scroll_to(
                            settings_id,
                            operation::AbsoluteOffset {
                                x: None,
                                y: Some(settings_y),
                            },
                        ));
                        pending_lists = Some((games_y, controllers_y));
                    }
                    if self.start_state.immersive {
                        // Re-show the position rail on every immersive open.
                        // Warm opens skip begin_cold_load; cold opens refresh
                        // the hide clock to visible time. Pinned is a no-op.
                        self.start_state.note_position_bar_scroll(now);
                        self.start_state.arm_cold_ambient(now);
                        // Cache miss: need metadata before art/list can land — scan now.
                        // Cache hit: Preparing chip first; Steam kicks when art-ready handoff runs.
                        if self.steam_scan_pending {
                            task = task.chain(self.maybe_kick_deferred_steam_scan());
                        }
                        // Cold: lift the enter veil onto ambient+dock immediately (games list
                        // stays hidden until art). Warm reopen still waits for art before arming.
                        if self.start_state.cold_load_active {
                            self.start_state.arm_enter_reveal(now);
                            // Cache hit: Preparing chip until art sync; cache miss uses Steam.
                            if !self.steam_scan_pending {
                                self.start_state.begin_chrome_status(
                                    start_view::ChromeStatusKind::PreparingArt,
                                    now,
                                );
                            }
                        }
                        self.start_state.prime_backdrop_for_enter(now);
                        if !self.start_state.try_arm_enter_reveal_when_art_ready(now) {
                            let open_ms = self
                                .start_opened_at
                                .map(|t| now.saturating_duration_since(t).as_millis())
                                .unwrap_or(0);
                            crate::controller::hid::diag::diag_info(format!(
                                "ui-diag: start immersive enter art hold open_ms={open_ms} steam_scan={} cold={}",
                                u8::from(self.steam_scan_pending),
                                u8::from(self.start_state.cold_load_active),
                            ));
                        } else {
                            task = task.chain(self.note_cold_load_finished());
                        }
                        task = task.chain(self.warm_immersive_art_task());
                    } else {
                        // Compact: chrome + skeletons immediately; defer Steam settle.
                        // The scrollables now exist, so reveal the restored
                        // selection (a scroll_to sent with window::open would
                        // have targeted a not-yet-mounted id and been dropped).
                        task = task.chain(self.scroll_start_selection_for_open());
                        if self.start_cold_load_remaining {
                            self.start_cold_load_remaining = false;
                            task = task.chain(self.schedule_steam_scan_after_settle());
                        } else if !self.steam_library_refreshed {
                            task = task.chain(self.schedule_steam_scan_after_settle());
                        }
                        if !self.steam_by_id.is_empty() {
                            task = task.chain(self.warm_start_art_task());
                        }
                    }
                    // Restored lists land after the branch above (which may
                    // reveal the selection first); the saved offsets win.
                    if let Some((games_y, controllers_y)) = pending_lists {
                        task = task.chain(self.restore_game_lists(games_y, controllers_y));
                    }
                    task
                } else {
                    Task::none()
                }
            }
            Message::StartImmersiveSettled => self.finish_start_immersive_transition(),
            Message::StartArtEvent(ev) => {
                use crate::ui::start::art_worker::ArtEventKind;
                if !crate::ui::start::icon_cache::art_warm_is_current(ev.generation) {
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: art event stale gen={} kind={:?}",
                        ev.generation, ev.kind
                    ));
                    return Task::none();
                }
                let now = Instant::now();
                let armed = match ev.kind {
                    ArtEventKind::SelectedReady => {
                        if self.start_state.immersive
                            && self
                                .start_state
                                .rows
                                .get(self.start_state.game_selected)
                                .is_some_and(|r| r.backdrop_icon().is_some())
                        {
                            self.start_state.note_backdrop_ready(now);
                        }
                        let armed = self.start_state.try_arm_enter_reveal_when_art_ready(now);
                        crate::controller::hid::diag::diag_info(format!(
                            "ui-diag: immersive art selected ready gen={}",
                            ev.generation
                        ));
                        armed
                    }
                    ArtEventKind::NearReady => {
                        let armed = self.start_state.try_arm_enter_reveal_when_art_ready(now);
                        crate::controller::hid::diag::diag_info(format!(
                            "ui-diag: immersive art neighbors ready gen={}",
                            ev.generation
                        ));
                        armed
                    }
                    ArtEventKind::JobDone => {
                        self.start_state.art_awaiting_paint = false;
                        let armed = self.start_state.try_arm_enter_reveal_when_art_ready(now);
                        crate::controller::hid::diag::diag_info(format!(
                            "ui-diag: immersive art warm done gen={}",
                            ev.generation
                        ));
                        armed
                    }
                };
                if armed {
                    return self.note_cold_load_finished();
                }
                // List / hero / backdrop peeks are live — StartFrame redraws pick them up.
                Task::none()
            }
            Message::StartFrame => {
                let now = Instant::now();
                self.start_state.steam_scan_pending = self.steam_scan_pending;
                // Prefs-owned pin for the immersive position rail (heals across
                // snapshot syncs / restarts; the apply path syncs immediately).
                self.start_state.sync_navbar_pinned(
                    matches!(
                        self.session.prefs.start_screen_navbar,
                        NavbarVisibility::AlwaysOn
                    ),
                    now,
                );
                // Effective reduce-motion gate (also synced per-update): an OS
                // "Show animations" flip lands on the next frame mid-session.
                self.start_state
                    .sync_reduced_motion(crate::ui::motion::reduced_motion_effective(
                        self.session.prefs.reduced_motion,
                    ));
                self.start_state
                    .sync_perf_throttle(self.session.prefs.perf_throttle_immersive);
                let was_cold = self.start_state.cold_load_active;
                // Frozen exit: while the outgoing presentation is on screen
                // (promote/demote first half + resize blackout), its clocks
                // stop — strip/dock/slide chases, hint presses, chrome status,
                // idle timers and ambient time all hold their pre-transition
                // frame instead of repainting (and reflowing) under the veil.
                // Phase/veil/resize progress is wall-clock, so nothing the
                // ceremony needs stops. Enter phases keep ticking (the new UI
                // animates in by design).
                let exiting = matches!(
                    self.start_state.transition_phase.map(|(p, _)| p),
                    Some(
                        start_mode::TransitionPhase::ExitCompact
                            | start_mode::TransitionPhase::ExitImmersive
                            | start_mode::TransitionPhase::Resizing
                    )
                );
                if !exiting {
                    let _ = self.start_state.tick_anim(now);
                }
                let task = if was_cold && !self.start_state.cold_load_active {
                    self.note_cold_load_finished()
                } else {
                    Task::none()
                };
                if let Some(phase_task) = self.tick_immersive_transition_phase(now) {
                    return task.chain(phase_task);
                }
                // Keyboard-only hold progress when pad poll is not running.
                if self.start_state.replace_confirm.is_some() {
                    let (progress, completed) =
                        self.keyboard_cross_hold.update(self.confirm_key_held, now);
                    self.start_state.cross_progress = self.start_state.cross_progress.max(progress);
                    if completed {
                        self.haptic_pad_serial = None;
                        self.play_start_cue(UiSoundKind::Hold);
                        return task.chain(self.complete_replace_confirm());
                    }
                }
                if !exiting {
                    self.sync_start_held();
                    self.start_state.tick_hint_anims(now);
                    self.tick_start_cursor_idle(now);
                    if self.tick_start_idle(now) {
                        self.pad_nav.prepare_on_open(false);
                        crate::controller::hid::diag::diag_info(
                            "ui-diag: start idle pad re-arm after sleep wake",
                        );
                    }
                }
                task
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
                self.note_start_activity();
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
                    self.note_start_activity();
                    // Cold ambient: only Cancel/Close is live.
                    if self.start_state.cold_load_active {
                        return match action {
                            StartKeyAction::Cancel if !pressed => {
                                self.cancel_start_holds();
                                self.on_start_message(StartMessage::Close)
                            }
                            StartKeyAction::Cancel => {
                                self.cancel_key_held = pressed;
                                self.sync_start_held();
                                Task::none()
                            }
                            _ => Task::none(),
                        };
                    }
                    // Leftover-chord arming is pad-local; keyboard stays live.
                    // Layout transition reflows main content: drop keyboard nav for its
                    // duration. Confirm/Cancel/Close stay live (cancellable chrome).
                    if pressed
                        && matches!(
                            action,
                            StartKeyAction::Up
                                | StartKeyAction::Down
                                | StartKeyAction::Left
                                | StartKeyAction::Right
                        )
                        && self.start_state.layout_transition_active()
                    {
                        return Task::none();
                    }
                    return match action {
                        StartKeyAction::Up if pressed => {
                            self.cancel_start_holds();
                            if self.start_state.ambient_tuning_active() {
                                self.on_ambient_tuning_nav(Some(NavAction::Up))
                            } else if self.start_state.settings.open {
                                self.on_start_settings_nav(Some(NavAction::Up))
                            } else if self.immersive_horizontal_games_nav() {
                                // Horizontal Games strip scrolls Left/Right only.
                                Task::none()
                            } else {
                                self.on_start_message(StartMessage::MoveUp)
                            }
                        }
                        StartKeyAction::Down if pressed => {
                            self.cancel_start_holds();
                            if self.start_state.ambient_tuning_active() {
                                self.on_ambient_tuning_nav(Some(NavAction::Down))
                            } else if self.start_state.settings.open {
                                self.on_start_settings_nav(Some(NavAction::Down))
                            } else if self.immersive_horizontal_games_nav() {
                                // Horizontal Games strip scrolls Left/Right only.
                                Task::none()
                            } else {
                                self.on_start_message(StartMessage::MoveDown)
                            }
                        }
                        StartKeyAction::Left
                            if pressed
                                && (self.start_state.immersive
                                    || self.start_state.settings.open) =>
                        {
                            self.cancel_start_holds();
                            if self.start_state.ambient_tuning_active() {
                                self.on_ambient_tuning_nav(Some(NavAction::PrevSlide))
                            } else if self.start_state.settings.open {
                                self.on_start_settings_nav(Some(NavAction::PrevSlide))
                            } else if self.immersive_horizontal_games_nav() {
                                self.on_start_message(StartMessage::MoveUp)
                            } else {
                                self.on_start_message(StartMessage::PrevSlide)
                            }
                        }
                        StartKeyAction::Right
                            if pressed
                                && (self.start_state.immersive
                                    || self.start_state.settings.open) =>
                        {
                            self.cancel_start_holds();
                            if self.start_state.ambient_tuning_active() {
                                self.on_ambient_tuning_nav(Some(NavAction::NextSlide))
                            } else if self.start_state.settings.open {
                                self.on_start_settings_nav(Some(NavAction::NextSlide))
                            } else if self.immersive_horizontal_games_nav() {
                                self.on_start_message(StartMessage::MoveDown)
                            } else {
                                self.on_start_message(StartMessage::NextSlide)
                            }
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
                                if self.start_state.ambient_tuning_active() {
                                    self.on_ambient_tuning_nav(Some(NavAction::Confirm))
                                } else if self.start_state.settings.open {
                                    self.on_start_settings_nav(Some(NavAction::Confirm))
                                } else {
                                    self.on_start_message(StartMessage::Confirm)
                                }
                            } else {
                                Task::none()
                            }
                        }
                        StartKeyAction::Cancel => {
                            self.cancel_key_held = pressed;
                            self.sync_start_held();
                            if !pressed {
                                self.cancel_start_holds();
                                if self.start_state.ambient_tuning_active() {
                                    self.on_ambient_tuning_nav(Some(NavAction::Cancel))
                                } else if self.start_state.settings.any_menu_open() {
                                    // Expanded dropdown owns Cancel: Esc closes the
                                    // menu only, exactly like pad Circle — not the
                                    // whole settings surface.
                                    self.on_start_settings_nav(Some(NavAction::Cancel))
                                } else {
                                    self.on_start_message(StartMessage::Close)
                                }
                            } else {
                                Task::none()
                            }
                        }
                        _ => Task::none(),
                    };
                }
                Task::none()
            }
            Message::PadInput(edge) => {
                if crate::platform::debug_quiet::enabled() {
                    Task::none()
                } else {
                    self.on_pad_input(edge)
                }
            }
            Message::ManualFilePicked(path) => self.on_manual_file_picked(path),
            Message::ManualIconPicked(path) => self.on_manual_icon_picked(path),
            Message::SteamPathPicked(path) => self.on_steam_path_picked(path),
            Message::SteamScanDone(result) => self.on_steam_scan_done(result),
            Message::SteamScanIdleFallback => {
                // Only when Start screen is off — otherwise StartOpened / settle owns the kick
                // so Scanning/Preparing stay visible on first open.
                if self.session.prefs.start_screen_enabled {
                    Task::none()
                } else {
                    self.maybe_kick_deferred_steam_scan()
                }
            }
            Message::SteamScanAfterSettle => self.maybe_kick_deferred_steam_scan(),
            // Debug-only renderer stress driver: 5 fast cycles of toast +
            // cold Start + configure + edit churn (the atlas-crash shape),
            // then a final open. Takes over windows; see the variant docs.
            #[cfg(debug_assertions)]
            Message::StressWindows(step) => {
                const CYCLES: u8 = 5;
                const PER_CYCLE: u8 = 6;
                let total = CYCLES * PER_CYCLE;
                if step > total {
                    self.stress_running = false;
                    return Task::none();
                }
                if step == total {
                    self.stress_running = false;
                    crate::ui::toast::ToastMessage::enqueue(
                        &mut self.toast_queue,
                        crate::ui::toast::ToastMessage::preview(&self.session.prefs.spectrum),
                    );
                    return self.show_next_toast().chain(self.open_start_screen());
                }
                let next = Message::StressWindows(step + 1);
                match step % PER_CYCLE {
                    // Cold open under a live toast.
                    0 => {
                        crate::ui::toast::ToastMessage::enqueue(
                            &mut self.toast_queue,
                            crate::ui::toast::ToastMessage::preview(&self.session.prefs.spectrum),
                        );
                        self.show_next_toast()
                            .chain(self.open_start_screen())
                            .chain(Task::perform(delay(Duration::from_secs(2)), move |()| next))
                    }
                    // Second cold renderer while toast + Start are live.
                    1 => self.open_configure().chain(Task::perform(
                        delay(Duration::from_millis(1500)),
                        move |()| next,
                    )),
                    2..=4 => self
                        .toggle_start_edit()
                        .unwrap_or_else(Task::none)
                        .chain(Task::perform(delay(Duration::from_secs(1)), move |()| next)),
                    // Tear it all down; toast expiry overlaps the next cold open.
                    _ => {
                        let close_cfg = self
                            .configure_window
                            .map(window::close)
                            .unwrap_or_else(Task::none);
                        let _ = self.toggle_start_edit();
                        close_cfg
                            .chain(self.close_start_screen())
                            .chain(Task::perform(delay(Duration::from_secs(2)), move |()| next))
                    }
                }
            }

            #[cfg(debug_assertions)]
            Message::DebugOpen(spec) => self.debug_open(&spec),

            Message::PlaceToast { id, generation } => self.handle_place_toast(id, generation),
            Message::ToastShown { generation } => {
                self.toast_step(toast_machine::Event::Shown { generation })
            }
            Message::ToastFrame => self.handle_toast_frame(),
            Message::IdentifyFrame => {
                self.popup_state.tick_identify_flash();
                self.start_state.tick_identify_flash();
                Task::none()
            }
            // Redraw only: the wall-clock spinner phase advances in `view`.
            Message::RendererReloadTick => Task::none(),
            Message::RendererReloadDue => self.restart_app_for_renderer(),
            Message::UtilityAmbientTick => {
                crate::ui::backdrop::note_utility_tick();
                Task::none()
            }
            Message::ToastDismiss(generation) => {
                self.toast_step(toast_machine::Event::Dismiss { generation })
            }
            Message::ToastHideDeferred => self.handle_toast_hide_deferred(),

            Message::SpectrumCommit(generation) => self.on_spectrum_commit(generation),

            Message::Exit => {
                crate::controller::hid::diag::diag_info("ui-diag: process exit begin");
                // A renderer-pref restart keeps its restore record; a plain
                // Quit clears it so the next launch starts clean.
                if !self.restarting_for_renderer {
                    crate::persist::restart_ui::clear();
                }
                self.session.known.save();
                // Drop every Start image Handle before iced tears down the GPU atlas.
                self.start_visible = false;
                self.start_nav_ready = false;
                self.start_state.art_awaiting_paint = false;
                self.start_state.clear_immersive_session();
                for row in &mut self.start_state.rows {
                    row.icon = None;
                }
                let _ = crate::ui::start::icon_cache::bump_art_warm_generation();
                crate::ui::start::art_worker::shutdown();
                crate::domain::pad::clear_input_edge_sender();
                self.hid_worker.shutdown();
                self.tray_icon.take();
                crate::controller::hid::diag::diag_info("ui-diag: process exit complete");
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

    /// OS arrival/removal hint: poll immediately instead of waiting for the
    /// next `Tick`. The poll refreshes the device list itself, so this is
    /// race-free even if the worker has not processed its own hint yet.
    fn on_presence_hint(&mut self) -> Task<Message> {
        #[cfg(debug_assertions)]
        if self.emulating() {
            return Task::none();
        }

        if self.identifying.load(Ordering::SeqCst) || self.refreshing.load(Ordering::SeqCst) {
            return Task::none();
        }

        self.last_discovered = self.hid_worker.presence_paths();
        self.request_refresh()
    }

    /// Follow-up poll after a suspected absence. The miss-hold window
    /// (`POLL_MISS_HOLD`) is shorter than the liveness cadence, so without
    /// this every first miss would cost a full extra liveness tick before the
    /// drop confirms. One-shot per absence (guarded by
    /// `absence_recheck_pending`); the hold still owns correctness, this only
    /// shortens the tail.
    fn on_absence_recheck(&mut self) -> Task<Message> {
        self.absence_recheck_pending = false;
        #[cfg(debug_assertions)]
        if self.emulating() {
            return Task::none();
        }

        if self.identifying.load(Ordering::SeqCst) || self.refreshing.load(Ordering::SeqCst) {
            return Task::none();
        }

        if !self.session.has_poll_misses() {
            return Task::none();
        }

        self.request_refresh()
    }

    /// Queue one [`Message::AbsenceRecheck`] at hold expiry when a pad is
    /// currently held across a miss. No-op when nothing is missing or a
    /// recheck is already queued.
    fn maybe_schedule_absence_recheck(&mut self) -> Task<Message> {
        if self.absence_recheck_pending || !self.session.has_poll_misses() {
            return Task::none();
        }
        self.absence_recheck_pending = true;
        Task::perform(
            delay(crate::session::POLL_MISS_HOLD + Duration::from_millis(200)),
            |()| Message::AbsenceRecheck,
        )
    }

    fn on_tick(&mut self) -> Task<Message> {
        #[cfg(debug_assertions)]
        if self.emulating() {
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
        #[cfg(debug_assertions)]
        if self.emulating() {
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
        let worker = self.hid_worker.clone();
        Task::perform(worker.poll(previously_connected), Message::PollResult)
    }

    fn on_poll_result(&mut self, result: Result<Vec<ControllerStatus>, String>) -> Task<Message> {
        self.refreshing.store(false, Ordering::SeqCst);
        self.last_battery_poll = Instant::now();

        #[cfg(debug_assertions)]
        if self.emulating() {
            return Task::none();
        }

        match result {
            Ok(controllers) => {
                self.session
                    .notify
                    .queue_launch_quiet(self.hid_worker.take_launch_serials());
                let ctx = crate::session::ApplyContext {
                    start_visible: self.start_visible,
                    fullscreen: start_input::foreground_is_exclusive_fullscreen(),
                    now: Instant::now(),
                    lightbar_enabled: lightbar::is_enabled(),
                };
                let effects = self.session.on_poll_result(controllers, ctx);
                let effects_task = self.apply_session_effects(effects);
                Task::batch([effects_task, self.maybe_schedule_absence_recheck()])
            }
            Err(err) => {
                app_log::warn(format!("refresh failed: {err}"));
                self.maybe_schedule_absence_recheck()
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
                    tasks.push(self.apply_tray());
                }
                SessionEffect::SetLowBatteryTargets { targets } => {
                    self.hid_worker.set_low_battery_targets(targets);
                }
                SessionEffect::QueueNotifications {
                    events,
                    open_start_after_toast,
                } => {
                    tasks.push(self.queue_notifications(events, open_start_after_toast));
                }
                SessionEffect::OpenStart => {
                    // The reopen chord may fire during the connect-toast
                    // latch; the toast's own AfterToast::OpenStart still opens Start.
                    if self.toast_machine.suppresses_reopen_gesture() {
                        crate::controller::hid::diag::diag_info(
                            "ui-diag: OpenStart ignored (toast OpenStart pending)",
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
                    self.session.known.save();
                }
                SessionEffect::SaveAnalytics => {
                    self.session.analytics.save();
                }
                SessionEffect::RefreshAnalyticsPanel => {
                    self.session.analytics = AnalyticsStore::load();
                    self.refresh_analytics_panel();
                }
                SessionEffect::SyncPopup => {
                    tasks.push(self.sync_popup_rows_and_fit());
                }
            }
        }
        Task::batch(tasks)
    }

    /// Persist prefs to disk (single writer: this process owns `prefs.json`).
    fn save_prefs(&self) {
        self.session.prefs.save();
    }

    /// Apply a stored selection to the start lists (keys → indices).
    /// Game-axis resolve honors the persistence prefs; the controller axis is
    /// never gated. Returns true when an index moved.
    fn apply_start_selection(&mut self, selection: &crate::session::StartSelection) -> bool {
        self.start_selection_shadow = Some(selection.clone());
        self.start_state.resolve_start_selection(
            selection.game_key.as_deref(),
            selection.controller_key.as_deref(),
            self.session.prefs.start_selection_persist,
            self.session.prefs.start_selection_mode,
            selection.game_index,
        )
    }

    /// True when live pad-input edges should drive Start / diagnostics.
    fn wants_pad_input(&self) -> bool {
        // Debug quiet mode: HID input is focus-independent, so a pad press during
        // a game must never drive Start (navigate / launch).
        if crate::platform::debug_quiet::enabled() {
            return false;
        }
        let pad_input_live =
            self.configure_window.is_some() && self.configure_state.section == Section::PadInput;
        pad_input_live
            || (self.start_visible
                && self.session.prefs.start_screen_enabled
                && !self.session.controllers.is_empty())
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
    // Controller actions
    // -----------------------------------------------------------------------

    fn identify(&self, serial: &str) -> bool {
        if is_emulated_serial(serial) {
            app_log::info(format!("identify skipped for emulated controller {serial}"));
            return false;
        }
        // A pad on its way out takes no more commands.
        if self.start_state.powering_off.as_deref() == Some(serial) {
            return false;
        }

        let Some(controller) = self.session.controllers.iter().find(|c| c.serial == serial) else {
            return false;
        };
        if !controller.supports_lightbar {
            return false;
        }

        self.hid_worker
            .identify(serial.to_string(), controller.percent)
    }

    fn power_off(&mut self, serial: &str) {
        if is_emulated_serial(serial) {
            app_log::info(format!(
                "power-off skipped for emulated controller {serial}"
            ));
            return;
        }
        // Already going away: don't re-send the feature report on repeat holds.
        if self.start_state.powering_off.as_deref() == Some(serial) {
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

        // Show "Powering off" immediately (cleared when the pad leaves the list).
        self.start_state.powering_off = Some(serial.to_string());
        // Reconnect after user power-off should open Start again (not ghost-flap cooldown),
        // but only when this was the last pad — a sibling must not look like a fresh 0→1.
        if crate::session::should_skip_connect_cooldown_on_power_off(
            &self.session.controllers,
            serial,
            self.session.prefs.start_screen_auto_open.includes_usb(),
        ) {
            self.session.mark_skip_connect_cooldown();
        }
        self.hid_worker.power_off(serial.to_string());
    }

    fn toggle_remember(&mut self, serial: &str) -> Task<Message> {
        #[cfg(debug_assertions)]
        if is_emulated_serial(serial) {
            // Emulated pads ride the fleet path (`apply_emulator_command`
            // with `SetRemembered`); the popup pin is a no-op here.
            return Task::none();
        }
        #[cfg(not(debug_assertions))]
        if is_emulated_serial(serial) {
            return Task::none();
        }

        // The session is the authority; `controllers.json` is written on change.
        if self.session.known.is_remembered(serial) {
            self.session.known.forget(serial);
        } else if let Some(controller) =
            self.session.controllers.iter().find(|c| c.serial == serial)
        {
            self.session.known.remember(controller);
        } else {
            return Task::none();
        }

        self.session.known.save();
        self.sync_popup_rows_and_fit()
    }

    fn set_nickname(&mut self, serial: &str, nickname: Option<String>) -> Task<Message> {
        #[cfg(debug_assertions)]
        if is_emulated_serial(serial) {
            // Emulated-pad nicknames ride the fleet path; the direct pin is
            // a no-op here.
            return Task::none();
        }
        #[cfg(not(debug_assertions))]
        if is_emulated_serial(serial) {
            return Task::none();
        }
        if self.session.known.set_nickname(serial, nickname.clone()) {
            self.session.known.save();
            self.sync_popup_rows_and_fit()
        } else {
            Task::none()
        }
    }

    // -----------------------------------------------------------------------
    // Start screen
    // -----------------------------------------------------------------------

    fn effective_played_ms(&self, entry: &crate::games::GameEntry) -> Option<u64> {
        let catalog_ms = self.games.last_played(entry).unwrap_or(0);
        let steam_ms = match entry {
            crate::games::GameEntry::Steam { appid } => self
                .steam_by_id
                .get(appid)
                .and_then(|g| g.last_played_unix)
                .map(|secs| secs.saturating_mul(1000))
                .unwrap_or(0),
            crate::games::GameEntry::Manual { .. } => 0,
        };
        let ms = catalog_ms.max(steam_ms);
        (ms > 0).then_some(ms)
    }

    fn display_catalog(&self) -> Vec<crate::games::GameEntry> {
        let steam_by_id = &self.steam_by_id;
        // Keep uninstalled Steam entries so rows can render disabled instead of
        // vanishing. Disabled is resolved per-row in refresh_start_rows.
        self.games.merge_sorted(
            None,
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
        // Sort persists across edit enter/exit: `sort_mode` always mirrors the
        // prefs sort here, and `cycle_games_sort` stays live while editing, so
        // a mid-edit sort change carries into the browse list on save/cancel.
        self.start_state.sort_mode = self.session.prefs.games_sort_mode;
        // Pin the edit cursor by stable key across the rebuild: toggles,
        // manual adds, sort cycles, and Steam-scan reorders shift checklist
        // indices, so a bare index would drift to a different game mid-edit.
        let edit_pin = if self.start_state.editing {
            self.start_state.selected_game_key()
        } else {
            None
        };
        let rows = if self.start_state.editing {
            self.edit_checklist_rows()
        } else {
            self.display_catalog()
                .iter()
                .map(|entry| {
                    let mut row = start_view::StartRow::from_entry(
                        entry,
                        &self.steam_by_id,
                        self.steam_scan_pending,
                    );
                    row.played_at_ms = self.effective_played_ms(entry);
                    row.disabled = self.is_entry_disabled(entry, &row);
                    row
                })
                .collect()
        };
        self.start_state.set_rows(rows);
        if self.start_state.editing {
            // Keep the cursor on the same checklist game; a removed manual
            // keeps the clamped neighbor. Never fall back to 0 or re-pin the
            // IPC shadow here: the shadow still holds the pre-toggle key while
            // a toggle message is in flight, which would yank the cursor
            // (manual removal's key is already gone → shadow fallback is 0).
            self.start_state
                .select_game_by_play_key_or_keep(edit_pin.as_deref());
        } else if let Some(selection) = self.start_selection_shadow.clone() {
            self.start_state.resolve_start_selection(
                selection.game_key.as_deref(),
                None,
                self.session.prefs.start_selection_persist,
                self.session.prefs.start_selection_mode,
                selection.game_index,
            );
        }
    }

    /// True when scan proves this entry is no longer installed (disabled row).
    fn is_entry_disabled(
        &self,
        entry: &crate::games::GameEntry,
        row: &start_view::StartRow,
    ) -> bool {
        if row.skeleton {
            return false;
        }
        match entry {
            crate::games::GameEntry::Steam { appid } => {
                // Pre-flight failures dim the row until a fresh scan proves
                // the install back.
                if self.launch_blocked_steam.contains(appid) {
                    return true;
                }
                // Unknown scan (None) keeps rows enabled; known scan disables missing.
                match self.steam_installed.as_deref() {
                    Some(ids) => !ids.contains(appid),
                    None => false,
                }
            }
            crate::games::GameEntry::Manual { target, .. } => is_manual_target_missing(target),
        }
    }

    fn edit_catalog(&self) -> &GamesCatalog {
        self.edit_draft.as_ref().unwrap_or(&self.games)
    }

    fn edit_catalog_mut(&mut self) -> &mut GamesCatalog {
        self.edit_draft.as_mut().unwrap_or(&mut self.games)
    }

    fn edit_checklist_rows(&self) -> Vec<start_view::StartRow> {
        let catalog = self.edit_catalog();
        let sort_mode = self.session.prefs.games_sort_mode;
        let mut rows: Vec<_> = self
            .steam_by_id
            .values()
            .map(|game| {
                let mut row =
                    start_view::StartRow::steam_edit(game, catalog.contains_steam(game.appid));
                let catalog_ms = catalog
                    .last_played(&crate::games::GameEntry::Steam { appid: game.appid })
                    .unwrap_or(0);
                let steam_ms = game.last_played_unix.unwrap_or(0).saturating_mul(1000);
                row.played_at_ms =
                    (catalog_ms.max(steam_ms) > 0).then_some(catalog_ms.max(steam_ms));
                row
            })
            .collect();
        for entry in &catalog.entries {
            if let Some(mut row) = start_view::StartRow::manual_edit(entry) {
                row.played_at_ms = catalog.last_played(entry);
                rows.push(row);
            }
        }
        start_view::sort_edit_rows_by_mode(&mut rows, sort_mode);
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
            .chain(self.warm_start_art_task())
    }

    fn commit_start_edit(&mut self) -> Task<Message> {
        // Resolve the exit target while still on the checklist rows: keep the
        // cursor game when still toggled-on, else nearest toggled-on above /
        // after (`None` when the draft catalog is empty → empty view shows).
        // The anchor is intentionally NOT used here: the user may have
        // navigated mid-edit and the cursor must persist.
        let target = self.start_state.edit_exit_target_key();
        if let Some(draft) = self.edit_draft.take() {
            self.games = draft;
            self.games.save();
        }
        self.start_state.editing = false;
        self.start_state.edit_anchor_play_key = None;
        self.refresh_start_rows();
        // `refresh_start_rows` re-syncs `sort_mode` from prefs (a mid-edit
        // sort cycle persists), so only the selection needs resolving here.
        self.start_state
            .resolve_edit_exit_selection(target.as_deref());
        self.scroll_start_selection_to_center(false)
            .chain(self.warm_start_art_task())
    }

    fn cancel_start_edit(&mut self) -> Task<Message> {
        // Cancel discards membership changes but keeps navigation: the cursor
        // game survives when still in the restored catalog, else the pre-edit
        // anchor revives, else the nearest-enabled rule applies.
        let cursor = self.start_state.selected_game_key();
        let anchor = self.start_state.edit_anchor_play_key.take();
        self.discard_edit_draft();
        self.refresh_start_rows();
        let target = self
            .start_state
            .cancel_exit_target_key(cursor.as_deref(), anchor.as_deref());
        self.start_state
            .resolve_edit_exit_selection(target.as_deref());
        self.scroll_start_selection_to_center(true)
            .chain(self.warm_start_art_task())
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
        // Keep a restored selection pinned to its pad across membership changes.
        // Controller axis only (never gated by the game-axis persist pref):
        // the game axis resolves as disabled so it stays untouched.
        if let Some(selection) = self.start_selection_shadow.clone() {
            self.start_state.resolve_start_selection(
                None,
                selection.controller_key.as_deref(),
                false,
                StartSelectionMode::ByGame,
                None,
            );
        }
    }

    fn refresh_running_badge(&mut self) -> Task<Message> {
        self.refresh_running_badge_inner(false)
    }

    fn refresh_running_badge_inner(&mut self, force: bool) -> Task<Message> {
        let now = Instant::now();

        // No grace early-return: enums run from launch onward (throttled) so
        // a Steam handoff that never spawns a process fast-fails instead of
        // holding `Playing` blind for `LAUNCH_GRACE`. Optimism lives in
        // `apply_running_badge_from_images` via `decide_running_badge`, which
        // also means a reopen after the window spawns a real check instead of
        // re-asserting the stale badge.
        if self.process_enum_inflight {
            return Task::none();
        }

        // Registry-watch events bypass the cadence (rare: a few per launch /
        // exit) so real launches confirm immediately; ticks stay throttled.
        if !force
            && self
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
            let process_matched =
                process_match::any_matching_in_images(&session.match_paths, images);
            // Steam's own running flag (Windows registry; None = unknown /
            // off-platform / non-Steam target). Authoritative when present:
            // it covers launcher/JVM games path matching cannot see, and it
            // never flips for license-refused launches.
            let steam_running =
                steam_appid_from_target(&session.target).and_then(steam::is_appid_running);
            let was_matched = session.matched;
            if process_match::decide_running_badge(session, now, process_matched, steam_running) {
                // Stamp last-played on the false→true confirm edge only:
                // failed handoffs (Steam error, never a process) must not
                // pollute Last Played sort/metadata.
                let confirm_key = if !was_matched && session.matched {
                    session.play_key.clone()
                } else {
                    None
                };
                if let Some(play_key) = confirm_key {
                    self.touch_last_played(&play_key);
                }
                self.sync_launch_badges();
                // Keep any unrelated Closing visible; don't clear it here.
                return;
            }
            let target = session.target.clone();
            app_log::info(format!("running session cleared (process miss): {target}"));
            self.running_session = None;
            self.sync_launch_badges();
        } else {
            self.sync_launch_badges();
        }

        // "Closing" stays until its process actually exits (or paths were empty).
        if self.start_state.closing_target.is_some() {
            if !self.closing_match_paths.is_empty()
                && process_match::any_matching_in_images(&self.closing_match_paths, images)
            {
                // Still exiting: keep Closing, don't restore it as Running.
                return;
            }
            self.start_state.closing_target = None;
            self.closing_match_paths.clear();
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
            // A live process matches the session target: confirm it.
            if let Some(session) = self.running_session.as_mut() {
                session.matched = true;
            }
            self.sync_launch_badges();
            return;
        }
        let match_paths = path_sets[idx].clone();
        app_log::info(format!("running session restored: {target}"));
        self.running_session = Some(RunningSession::restored(title, target.clone(), match_paths));
        self.sync_launch_badges();
    }

    fn begin_running_session(&mut self, title: String, target: String, play_key: String) {
        let match_paths = self.cached_match_paths(&target);
        if match_paths.is_empty() {
            app_log::warn(format!(
                "running session: empty match_paths for {target} (badge may clear after grace)"
            ));
        }
        self.running_session = Some(RunningSession::new(
            title,
            target.clone(),
            match_paths,
            Some(play_key),
        ));
        self.sync_launch_badges();
    }

    /// Mirror the launch session into Start badges: a confirmed session
    /// (`matched`: process or Steam verdict) shows Running, a live but
    /// unconfirmed one shows Launching, no session clears both.
    fn sync_launch_badges(&mut self) {
        if let Some(session) = self.running_session.as_ref() {
            let target = session.target.clone();
            if session.matched {
                self.start_state.running_target = Some(target);
                self.start_state.launching_target = None;
            } else {
                self.start_state.running_target = None;
                self.start_state.launching_target = Some(target);
            }
        } else {
            self.start_state.running_target = None;
            self.start_state.launching_target = None;
        }
    }

    fn touch_last_played(&mut self, play_key: &str) {
        self.games.touch_played_key(play_key);
        self.games.save();
    }

    fn close_running_game(&mut self) -> Task<Message> {
        let Some(session) = self.running_session.clone() else {
            return Task::none();
        };
        // Show "Closing" immediately; clear "Playing" now so the status flips
        // on the next frame even though WM_CLOSE + terminate blocks.
        self.start_state.closing_target = Some(session.target.clone());
        self.closing_match_paths = session.match_paths.clone();
        self.running_session = None;
        self.sync_launch_badges();
        self.start_state.cross_progress = 0.0;
        let match_paths = session.match_paths.clone();
        let target = session.target.clone();
        crate::controller::hid::diag::diag_info(format!("ui-diag: game closing target={target}"));
        Task::perform(
            spawn_blocking(move || process_match::close_matching(&match_paths)),
            |res: Result<Result<(), String>, String>| {
                Message::CloseGameDone(res.unwrap_or_else(Err))
            },
        )
    }

    fn on_close_game_done(&mut self, result: Result<(), String>) -> Task<Message> {
        if let Err(err) = result {
            app_log::warn(format!("close game failed: {err}"));
        }
        // Force a process check so "Closing" clears as soon as the process exits.
        self.last_running_check = None;
        self.refresh_running_badge()
    }

    /// Hold-Cross close: only when the selected row is the running game.
    fn close_running_game_if_selected(&mut self) -> Task<Message> {
        let Some(session) = self.running_session.as_ref() else {
            return Task::none();
        };
        let Some(row) = self.start_state.rows.get(self.start_state.game_selected) else {
            return Task::none();
        };
        if !session.matches_target(&row.target) {
            return Task::none();
        }
        self.close_running_game()
    }

    fn seed_steam_library_from_cache(&mut self) {
        let Some(cache) = crate::persist::steam_library::load() else {
            return;
        };
        self.steam_installed = Some(cache.games.iter().map(|g| g.appid).collect());
        self.steam_by_id = cache.games.into_iter().map(|g| (g.appid, g)).collect();
        self.steam_scan_pending = false;
        self.start_state.steam_scan_pending = false;
        self.refresh_start_rows();
    }

    fn refresh_steam_library(&mut self) -> Task<Message> {
        // Metadata only — list / immersive art decode runs after SteamScanDone so the
        // selected game can paint before the rest of the library is warm.
        let now = Instant::now();
        self.steam_scan_started = Some(now);
        self.start_state.begin_steam_scan_banner(now);
        crate::controller::hid::diag::diag_info("ui-diag: steam scan start deferred=1");
        // Floor so chrome spinners stay visible even on a warm/fast refresh.
        let min_ms = crate::ui::start::mode::STEAM_SCAN_MIN_MS;
        let extra_paths: Vec<PathBuf> = self.session.prefs.extra_steam_paths.clone();
        Task::perform(
            async move {
                let started = Instant::now();
                let result =
                    spawn_blocking(move || steam::list_installed_games(&extra_paths)).await;
                let min = Duration::from_millis(min_ms);
                let elapsed = started.elapsed();
                if elapsed < min {
                    let pad = min - elapsed;
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: steam scan pad ms={}",
                        pad.as_millis()
                    ));
                    delay(pad).await;
                }
                result
            },
            |result| match result {
                Ok(Ok(games)) => Message::SteamScanDone(Ok(games)),
                Ok(Err(err)) => Message::SteamScanDone(Err(err)),
                Err(err) => Message::SteamScanDone(Err(err)),
            },
        )
    }

    fn steam_scan_in_flight(&self) -> bool {
        self.steam_scan_started.is_some() || self.start_state.steam_scan_banner_active()
    }

    /// Kick the first live Steam scan/refresh this process. Idempotent.
    fn maybe_kick_deferred_steam_scan(&mut self) -> Task<Message> {
        if self.steam_library_refreshed || self.steam_scan_started.is_some() {
            self.steam_scan_deferred_for_promote = false;
            return Task::none();
        }
        if self.immersive_transition_busy() {
            self.steam_scan_deferred_for_promote = true;
            crate::controller::hid::diag::diag_info(
                "ui-diag: steam scan defer during promote ceremony",
            );
            return Task::none();
        }
        self.steam_scan_deferred_for_promote = false;
        self.refresh_steam_library()
    }

    /// Drain a Steam kick that was deferred across promote ExitCompact→EnterImmersive.
    fn flush_deferred_steam_scan_after_promote(&mut self) -> Task<Message> {
        if !self.steam_scan_deferred_for_promote {
            return Task::none();
        }
        self.steam_scan_deferred_for_promote = false;
        crate::controller::hid::diag::diag_info("ui-diag: steam scan flush after promote enter");
        self.maybe_kick_deferred_steam_scan()
    }

    fn on_steam_scan_done(&mut self, result: Result<Vec<SteamGame>, String>) -> Task<Message> {
        let scan_ms = self
            .steam_scan_started
            .take()
            .map(|t| t.elapsed().as_millis())
            .unwrap_or(0);
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: steam scan done ms={scan_ms} ok={}",
            u8::from(result.is_ok())
        ));
        self.steam_library_refreshed = true;
        match result {
            Ok(games) => {
                self.steam_installed = Some(games.iter().map(|g| g.appid).collect());
                self.steam_by_id = games.iter().map(|g| (g.appid, g.clone())).collect();
                // A fresh scan is ground truth again: drop pre-flight blocks
                // (reinstall / license restored re-enables the row).
                self.launch_blocked_steam.clear();
                let root = steam::steam_root().ok().flatten();
                crate::persist::steam_library::save(&games, root.as_deref());
            }
            Err(err) => {
                steam::warn_scan_error(&err);
                // Keep prior successful install list if any; otherwise stay Unknown so
                // curated Steam rows remain visible.
            }
        }
        self.steam_scan_pending = false;
        self.start_state.steam_scan_pending = false;
        let now = Instant::now();
        self.start_state.note_steam_scan_banner_success(now);
        self.match_path_cache.clear();
        self.refresh_start_rows();
        // Rows just rebuilt (order may have changed): while the restore
        // settle window is fresh, re-pin the persisted selection by key
        // (index clamping alone can leave the highlight on the wrong row)
        // and reveal it. Row-independent state (drawer, config) is untouched.
        let mut task = self.prepare_immersive_library_task();
        if self.start_visible
            && self.session.prefs.start_selection_persist
            && self
                .scroll_restore_until
                .is_some_and(|until| Instant::now() < until)
            && let Some(shadow) = self.start_selection_shadow.clone()
            && self.apply_start_selection(&shadow)
        {
            task = task.chain(
                self.scroll_start_selection_into_view()
                    .chain(self.warm_start_art_task()),
            );
        }
        task
    }

    /// First Start HWND of the process has finished ambient+status; later opens are warm.
    ///
    /// Immersive: kick the deferred Steam refresh immediately so its status chip
    /// can stack under Preparing Success (event-based; no settle delay).
    fn note_cold_load_finished(&mut self) -> Task<Message> {
        if self.start_state.cold_load_active {
            self.start_state.finish_cold_load();
        }
        self.start_cold_load_remaining = false;
        crate::controller::hid::diag::diag_info(
            "ui-diag: chrome status queue kick steam (art ready)",
        );
        self.maybe_kick_deferred_steam_scan()
    }

    /// Compact-only: wait [`STEAM_SCAN_SETTLE_DELAY_MS`] then kick a live refresh (idempotent).
    fn schedule_steam_scan_after_settle(&mut self) -> Task<Message> {
        if self.steam_library_refreshed || self.steam_scan_started.is_some() {
            return Task::none();
        }
        let delay_ms = crate::ui::start::mode::STEAM_SCAN_SETTLE_DELAY_MS;
        Task::perform(delay(Duration::from_millis(delay_ms)), |()| {
            Message::SteamScanAfterSettle
        })
    }

    /// Pin ±ART_WINDOW and enqueue a latest-wins decode job on the art worker.
    fn prepare_immersive_library_task(&mut self) -> Task<Message> {
        self.request_start_art();
        Task::none()
    }

    /// Post a single-flight art job for the current selection (cheap; no Task spawn).
    fn request_start_art(&mut self) {
        // Linear strip window — Games no longer wraps.
        self.request_start_art_circular(false);
    }

    /// Like [`Self::request_start_art`] with an explicit circular/linear near window.
    fn request_start_art_circular(&mut self, circular: bool) {
        let generation = crate::ui::start::icon_cache::bump_art_warm_generation();
        let (selected_hero, selected_shell, selected_backdrop) =
            self.start_state.immersive_priority_paths();
        let near = self.start_state.art_near_set_circular(circular);
        let prefetch = self.start_state.art_retain_set_circular(circular);
        self.start_state.art_awaiting_paint = true;
        crate::ui::start::art_worker::submit(crate::ui::start::art_worker::ArtJob {
            generation,
            selected_backdrop,
            selected_hero,
            selected_shell,
            near,
            prefetch,
        });
    }

    /// Pre-warm immersive art under ExitCompact blackout (linear strip window).
    fn prewarm_immersive_art_for_promote(&mut self) {
        crate::controller::hid::diag::diag_info(
            "ui-diag: art warm promote prewarm (exit_compact linear)",
        );
        self.request_start_art_circular(false);
    }

    fn warm_start_art_task(&mut self) -> Task<Message> {
        self.request_start_art();
        Task::none()
    }

    /// After promote settle: skip gen bump when selection art is ready or still decoding.
    fn warm_immersive_art_task(&mut self) -> Task<Message> {
        if self.start_state.enter_art_sync_ready() {
            crate::controller::hid::diag::diag_info(
                "ui-diag: art warm skip (promote settle already sync)",
            );
            return Task::none();
        }
        if self.start_state.art_awaiting_paint {
            crate::controller::hid::diag::diag_info(
                "ui-diag: art warm skip (promote settle in flight)",
            );
            return Task::none();
        }
        self.request_start_art();
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

    fn on_steam_path_picked(&mut self, path: Option<PathBuf>) -> Task<Message> {
        let Some(path) = path else {
            return Task::none();
        };
        if !steam::validate_steam_library_path(&path) {
            self.configure_state.steam_path_error =
                Some("No Steam library found in that folder".to_string());
            return Task::none();
        }
        if self
            .session
            .prefs
            .extra_steam_paths
            .iter()
            .any(|existing| existing == &path)
        {
            self.configure_state.steam_path_error =
                Some("That folder is already in the list".to_string());
            return Task::none();
        }
        self.configure_state.steam_path_error = None;
        self.session.prefs.extra_steam_paths.push(path);
        self.save_prefs();
        self.refresh_steam_library()
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

    fn start_settings_snapshot(&self) -> crate::ui::start::settings::StartSettingsSnapshot {
        crate::ui::start::settings::StartSettingsSnapshot {
            auto_open: self.session.prefs.start_screen_auto_open,
            selection_persist: self.session.prefs.start_selection_persist,
            selection_mode: self.session.prefs.start_selection_mode,
            always_immersive: self.session.prefs.start_screen_always_immersive,
            clock_enabled: self.session.prefs.start_screen_clock_enabled,
            immersive_layout: self.session.prefs.start_screen_immersive_layout,
            navbar: self.session.prefs.start_screen_navbar,
            reduced_motion: self.session.prefs.reduced_motion,
            inactive_secs: self.session.prefs.start_screen_inactive_secs,
            sleep_secs: self.session.prefs.start_screen_sleep_secs,
            inactive_dim_percent: self.session.prefs.start_screen_inactive_dim_percent,
            sounds_enabled: self.session.prefs.start_screen_sounds_enabled,
            sound_volume: self.session.prefs.start_screen_sound_volume,
            haptics_enabled: self.session.prefs.start_screen_haptics_enabled,
            haptics_strength: self.session.prefs.start_screen_haptics_strength,
            perf_throttle_immersive: self.session.prefs.perf_throttle_immersive,
            perf_antialiasing: self.session.prefs.perf_antialiasing,
            perf_vsync: self.session.prefs.perf_vsync,
            perf_aa_boot: self.renderer_boot_aa,
            perf_vsync_boot: self.renderer_boot_vsync,
            perf_reload_queued: self.renderer_reload_queued,
            immersive: self.start_state.immersive,
        }
    }

    fn apply_start_screen_always_immersive(&mut self, enabled: bool) -> Task<Message> {
        self.session.prefs.start_screen_always_immersive = enabled;
        self.save_prefs();
        Task::none()
    }

    fn apply_start_screen_clock(&mut self, enabled: bool) -> Task<Message> {
        self.session.prefs.start_screen_clock_enabled = enabled;
        self.save_prefs();
        Task::none()
    }

    fn apply_start_screen_immersive_layout(
        &mut self,
        layout: crate::persist::prefs::ImmersiveLayout,
    ) -> Task<Message> {
        // Picking from the menu collapses it, even when re-picking the
        // current value.
        self.start_state.settings.close_menus();
        if self.session.prefs.start_screen_immersive_layout == layout {
            return Task::none();
        }
        let from = self.session.prefs.start_screen_immersive_layout;
        self.session.prefs.start_screen_immersive_layout = layout;
        self.save_prefs();
        // Covers both the Configure pick-list and the Start drawer dropdown:
        // animate only on an actual change, showing the old orientation
        // through the first half of the transition.
        self.start_state
            .begin_layout_transition(from, layout, Instant::now());
        Task::none()
    }

    fn apply_start_screen_navbar(&mut self, navbar: NavbarVisibility) -> Task<Message> {
        // Picking from the menu collapses it, even when re-picking the
        // current value.
        self.start_state.settings.close_menus();
        if self.session.prefs.start_screen_navbar == navbar {
            return Task::none();
        }
        self.session.prefs.start_screen_navbar = navbar;
        self.save_prefs();
        // Pin (or release) the position rail immediately; while pinned the
        // show path stays (1, 1) and the tick hide-timer is skipped.
        self.start_state
            .sync_navbar_pinned(matches!(navbar, NavbarVisibility::AlwaysOn), Instant::now());
        Task::none()
    }

    fn apply_start_screen_reduced_motion(&mut self, mode: ReducedMotion) -> Task<Message> {
        // Picking from the menu collapses it, even when re-picking the
        // current value.
        self.start_state.settings.close_menus();
        if self.session.prefs.reduced_motion == mode {
            return Task::none();
        }
        self.session.prefs.reduced_motion = mode;
        self.save_prefs();
        // Gate (or release) movement immediately; enabling also settles any
        // in-flight chase to its target.
        self.start_state
            .sync_reduced_motion(crate::ui::motion::reduced_motion_effective(mode));
        Task::none()
    }

    fn apply_perf_throttle_immersive(&mut self, enabled: bool) -> Task<Message> {
        if self.session.prefs.perf_throttle_immersive == enabled {
            return Task::none();
        }
        self.session.prefs.perf_throttle_immersive = enabled;
        self.save_prefs();
        self.start_state.sync_perf_throttle(enabled);
        Task::none()
    }

    fn apply_perf_antialiasing(&mut self, enabled: bool) -> Task<Message> {
        if self.session.prefs.perf_antialiasing == enabled {
            return Task::none();
        }
        self.session.prefs.perf_antialiasing = enabled;
        self.save_prefs();
        self.scroll_perf_reload_into_view()
    }

    fn apply_perf_vsync(&mut self, enabled: bool) -> Task<Message> {
        if self.session.prefs.perf_vsync == enabled {
            return Task::none();
        }
        self.session.prefs.perf_vsync = enabled;
        self.save_prefs();
        self.scroll_perf_reload_into_view()
    }

    /// Explicit UI reload for boot-only renderer prefs (AA/vsync are baked
    /// into the iced/wgpu renderer at boot; iced 0.14 exposes no runtime
    /// flip). The reload button shows only while prefs differ from the boot
    /// values. Queues the restart after a short feedback delay so the button
    /// can flip to its disabled "Reloading…" state; repeat presses no-op.
    fn reload_ui_for_renderer(&mut self) -> Task<Message> {
        if self.renderer_reload_queued {
            return Task::none();
        }
        if !renderer_restart_needed(
            self.session.prefs.perf_antialiasing,
            self.session.prefs.perf_vsync,
            self.renderer_boot_aa,
            self.renderer_boot_vsync,
        ) {
            return Task::none();
        }
        self.renderer_reload_queued = true;
        Task::perform(delay(RELOAD_FEEDBACK_DELAY), |()| {
            Message::RendererReloadDue
        })
    }

    /// Fire a queued renderer reload. Re-checks the dirty predicate (a toggle
    /// flipped back during the feedback delay cancels the restart).
    fn restart_app_for_renderer(&mut self) -> Task<Message> {
        self.renderer_reload_queued = false;
        if !renderer_restart_needed(
            self.session.prefs.perf_antialiasing,
            self.session.prefs.perf_vsync,
            self.renderer_boot_aa,
            self.renderer_boot_vsync,
        ) {
            return Task::none();
        }
        app_log::info("perf: renderer prefs changed; reloading UI to apply");
        // Snapshot the app UI so the relaunched process reopens to the same
        // state (windows, selection, scroll offsets). Written before Exit
        // clears Start state; consumed once at boot when fresh.
        crate::persist::restart_ui::save(&self.capture_restart_ui());
        self.restarting_for_renderer = true;
        // Relaunch detached first: the `--replace` child waits out this
        // process's single-instance guard while it exits. If the relaunch
        // fails, stay running on the old renderer rather than quitting.
        match std::env::current_exe() {
            Ok(exe) => {
                // `--replace`: the child retries the single-instance guard
                // while this process exits (see `sdsc-utils` main).
                if let Err(err) = std::process::Command::new(&exe)
                    .arg(crate::platform::app_meta::REPLACE_FLAG)
                    .spawn()
                {
                    app_log::error(format!("perf: relaunch failed: {err}"));
                    return Task::none();
                }
            }
            Err(err) => {
                app_log::error(format!("perf: current_exe failed: {err}"));
                return Task::none();
            }
        }
        self.update(Message::Exit)
    }

    /// Snapshot the app UI for a renderer-pref reload (see `restart_ui`).
    fn capture_restart_ui(&self) -> crate::persist::restart_ui::RestartUiState {
        use crate::persist::restart_ui::RestartUiState;
        RestartUiState {
            saved_at_ms: RestartUiState::now_ms(),
            start_visible: self.start_visible,
            start_immersive: self.start_state.immersive,
            start_on_controllers: matches!(self.start_state.slide, StartSlide::Controllers),
            game_key: self
                .start_state
                .rows
                .get(self.start_state.game_selected)
                .map(|r| r.play_key.clone()),
            game_index: Some(self.start_state.game_selected),
            controller_key: self
                .start_state
                .controllers
                .get(self.start_state.controller_selected)
                .map(|r| r.serial.clone()),
            games_scroll_y: self.start_state.list_offset(start_view::StartListId::Games),
            controllers_scroll_y: self
                .start_state
                .list_offset(start_view::StartListId::Controllers),
            settings_scroll_y: self
                .start_state
                .list_offset(start_view::StartListId::Settings),
            settings_open: self.start_state.settings.open,
            settings_focus: self.start_state.settings.focus,
            configure_open: self.configure_window.is_some(),
            configure_section: Some(self.configure_state.section),
            configure_scroll_y: self.configure_state.content_scroll_y,
            popup_open: self.popup_window.is_some(),
            popup_anchor: Some(self.tray_anchor),
        }
    }

    /// Reopen to a consumed restart record (renderer-pref restart only).
    /// Settings state restores FIRST, before any window opens or list
    /// rebuilds: the Start open path resets nav state (`reset_to_games`),
    /// which would wipe restored drawer/scroll/selection state. The restore
    /// open bypasses that reset (see `restore_preserve_nav`) and resolves
    /// selections from the shadow keys instead. Scroll offsets land on
    /// `StartOpened`/`ConfigureOpened`, when the scrollables exist.
    fn apply_restart_ui(
        &mut self,
        rs: crate::persist::restart_ui::RestartUiState,
    ) -> Task<Message> {
        // --- Settings state first (drawer, configure, anchor). ---
        if let Some(anchor) = rs.popup_anchor {
            self.tray_anchor = anchor;
        }
        if rs.configure_open {
            if let Some(section) = rs.configure_section {
                // No scroll-to here: the content scrollable is not mounted
                // yet, so the op would be dropped. Section applies now; the
                // saved offset lands on `ConfigureOpened`.
                self.configure_state.select_section(section);
            }
            self.configure_state.content_scroll_y = rs.configure_scroll_y;
            self.pending_configure_scroll = Some(rs.configure_scroll_y);
        }
        if rs.start_visible && self.session.prefs.start_screen_enabled {
            if rs.game_key.is_some() || rs.controller_key.is_some() {
                // Fresh boot: seed the shadow from the restore record so row
                // rebuilds re-pin the pre-restart selection by key.
                self.start_selection_shadow = Some(crate::session::StartSelection {
                    game_key: rs.game_key,
                    controller_key: rs.controller_key,
                    game_index: rs.game_index,
                });
            }
            // Reopen the pre-restart presentation (a manually promoted
            // immersive Start must not come back compact).
            self.force_immersive_open = Some(rs.start_immersive);
            // Bypass the open-time nav reset below; everything it would
            // clear is restored here instead.
            self.restore_preserve_nav = true;
            if rs.start_on_controllers {
                self.start_state.slide = StartSlide::Controllers;
            }
            if rs.settings_open {
                self.start_state.request_settings(true, Instant::now());
                // Instant restore: no drawer travel on boot.
                self.start_state.settings.anim = None;
            }
            self.start_state.settings.focus = rs.settings_focus;
            self.start_state.sync_settings_scroll(rs.settings_scroll_y);
            self.pending_scroll_restore = Some((
                rs.games_scroll_y,
                rs.controllers_scroll_y,
                rs.settings_scroll_y,
            ));
            // Settle window for selection re-pinning after row rebuilds (the
            // offsets themselves are one-shot and never re-asserted).
            self.scroll_restore_until = Some(Instant::now() + SCROLL_RESTORE_AUTHORITY);
        }
        // --- Windows after: configure + start first, popup last. ---
        let mut task = Task::none();
        if rs.configure_open {
            task = task.chain(self.open_configure());
        }
        if rs.start_visible && self.session.prefs.start_screen_enabled {
            task = task.chain(self.open_start_screen());
        }
        if rs.popup_open {
            task = task.chain(self.open_popup());
        }
        task
    }

    /// Re-apply saved games/controllers offsets once, after the settings
    /// scroll and the branch reveal above — the saved offsets win. Never
    /// re-asserted afterwards: once rows rebuild, offsets go stale and only
    /// the persisted selection stays authoritative. Unmounted ids (immersive
    /// strip/dock) are dropped by iced; the tracker sync still stands for
    /// reveal-math fallbacks.
    fn restore_game_lists(&mut self, games_y: f32, controllers_y: f32) -> Task<Message> {
        Task::batch([
            self.restore_one_list(start_view::StartListId::Games, games_y),
            self.restore_one_list(start_view::StartListId::Controllers, controllers_y),
        ])
    }

    fn restore_one_list(&mut self, list: start_view::StartListId, y: f32) -> Task<Message> {
        let id = list.scroll_id();
        self.start_state.note_scroll_y(&id, y);
        operation::scroll_to(
            id,
            operation::AbsoluteOffset {
                x: None,
                y: Some(y),
            },
        )
    }

    fn apply_start_screen_inactive_secs(&mut self, secs: u32) -> Task<Message> {
        let (inactive, sleep) =
            clamp_start_screen_idle_timeouts(secs, self.session.prefs.start_screen_sleep_secs);
        if self.session.prefs.start_screen_inactive_secs != inactive
            || self.session.prefs.start_screen_sleep_secs != sleep
        {
            self.session.prefs.start_screen_inactive_secs = inactive;
            self.session.prefs.start_screen_sleep_secs = sleep;
            self.save_prefs();
        }
        Task::none()
    }

    fn apply_start_screen_sleep_secs(&mut self, secs: u32) -> Task<Message> {
        let (inactive, sleep) =
            clamp_start_screen_idle_timeouts(self.session.prefs.start_screen_inactive_secs, secs);
        if self.session.prefs.start_screen_inactive_secs != inactive
            || self.session.prefs.start_screen_sleep_secs != sleep
        {
            self.session.prefs.start_screen_inactive_secs = inactive;
            self.session.prefs.start_screen_sleep_secs = sleep;
            self.save_prefs();
        }
        Task::none()
    }

    fn apply_start_screen_inactive_dim_percent(&mut self, percent: u8) -> Task<Message> {
        let percent = clamp_start_screen_inactive_dim_percent(percent);
        if self.session.prefs.start_screen_inactive_dim_percent != percent {
            self.session.prefs.start_screen_inactive_dim_percent = percent;
            self.save_prefs();
        }
        Task::none()
    }

    fn apply_start_screen_auto_open(&mut self, mode: StartAutoOpen) -> Task<Message> {
        // Picking from the menu collapses it, even when re-picking the
        // current value.
        self.start_state.settings.close_menus();
        let old = self.session.prefs.start_screen_auto_open;
        if old == mode {
            return Task::none();
        }
        self.session.prefs.start_screen_auto_open = mode;
        self.save_prefs();
        if !mode.auto_opens() {
            // Disabling auto-open clears any latched auto-open; never opens
            // or closes here.
            self.session.start_auto_open_pending = false;
            return Task::none();
        }
        if mode.includes_usb() == old.includes_usb() {
            return Task::none();
        }
        // Scope flip may CloseStart when USB pads no longer qualify; never opens.
        let ctx = crate::session::ApplyContext {
            start_visible: self.start_visible,
            fullscreen: start_input::foreground_is_exclusive_fullscreen(),
            now: Instant::now(),
            lightbar_enabled: lightbar::is_enabled(),
        };
        let effects = self.session.reevaluate_start_presence(ctx);
        self.apply_session_effects(effects)
    }

    /// Game-axis selection persistence master switch (drawer toggle +
    /// Configure toggle share this path). Turning it off stops reports and
    /// re-pins; turning it on re-pins from the shadow on the next rebuild.
    fn apply_start_selection_persist(&mut self, enabled: bool) -> Task<Message> {
        if self.session.prefs.start_selection_persist == enabled {
            return Task::none();
        }
        self.session.prefs.start_selection_persist = enabled;
        self.save_prefs();
        Task::none()
    }

    /// Game-axis restore mode (drawer dropdown + Configure pick-list share
    /// this path). Picking from the menu collapses it, even when re-picking
    /// the current value.
    fn apply_start_selection_mode(&mut self, mode: StartSelectionMode) -> Task<Message> {
        self.start_state.settings.close_menus();
        if self.session.prefs.start_selection_mode == mode {
            return Task::none();
        }
        self.session.prefs.start_selection_mode = mode;
        self.save_prefs();
        Task::none()
    }

    fn apply_start_screen_sounds(&mut self, enabled: bool) -> Task<Message> {
        self.session.prefs.start_screen_sounds_enabled = enabled;
        self.save_prefs();
        if enabled {
            self.play_start_sound(UiSoundKind::Nav);
        }
        Task::none()
    }

    fn apply_start_screen_sound_volume(&mut self, volume: u8) -> Task<Message> {
        let volume = clamp_start_screen_sound_volume(volume);
        if self.session.prefs.start_screen_sound_volume != volume {
            self.session.prefs.start_screen_sound_volume = volume;
            self.save_prefs();
            if self.session.prefs.start_screen_sounds_enabled {
                self.play_start_sound(UiSoundKind::Nav);
            }
        }
        Task::none()
    }

    fn apply_start_screen_haptics(&mut self, enabled: bool) -> Task<Message> {
        self.session.prefs.start_screen_haptics_enabled = enabled;
        self.save_prefs();
        if enabled {
            self.preview_haptic_all(MotorPulse::NAV);
        } else {
            self.hid_worker.rumble_stop_all();
        }
        Task::none()
    }

    fn apply_start_screen_haptics_strength(&mut self, strength: u8) -> Task<Message> {
        let strength = clamp_start_screen_haptics_strength(strength);
        if self.session.prefs.start_screen_haptics_strength != strength {
            self.session.prefs.start_screen_haptics_strength = strength;
            self.save_prefs();
            if self.session.prefs.start_screen_haptics_enabled {
                self.preview_haptic_all(MotorPulse::NAV);
            }
        }
        Task::none()
    }

    /// True when HID/UI input sampling should run at the active (~report-rate) rate.
    #[allow(dead_code)] // kept for callers / diagnostics that still want the hot predicate
    fn pad_input_hot(&self) -> bool {
        self.start_visible
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

        // Restore opens bypass the nav reset: reload restore already set
        // drawer/scroll/selection state above, and `reset_to_games` would
        // wipe it (drawer close, scroll zero, selections zeroed). Pad nav
        // arming still runs either way.
        if std::mem::take(&mut self.restore_preserve_nav) {
            self.pad_nav.prepare_on_open(false);
        } else {
            self.prepare_start_nav_on_open(false);
        }
        // Pin state for the first paint (before the first StartFrame tick).
        self.start_state.sync_navbar_pinned(
            matches!(
                self.session.prefs.start_screen_navbar,
                NavbarVisibility::AlwaysOn
            ),
            Instant::now(),
        );
        self.start_state
            .sync_perf_throttle(self.session.prefs.perf_throttle_immersive);
        // App restart: resolve the stored shadow keys back to indices now
        // that rows/controllers are rebuilt.
        // NOTE: no scroll_to here — the fresh window's scrollables do not exist
        // yet, so a scroll_to batched with window::open targets a not-mounted
        // id and is dropped by iced (compact-reopen bug: tracked offset said
        // "centered" while the real viewport stayed at 0). Selection indices
        // are restored now; the scroll_to reveal runs on StartOpened, when the
        // view exists (see scroll_start_selection_for_open).
        let mut restored_art = Task::none();
        if let Some(selection) = self.start_selection_shadow.clone()
            && self.start_state.resolve_start_selection(
                selection.game_key.as_deref(),
                selection.controller_key.as_deref(),
                self.session.prefs.start_selection_persist,
                self.session.prefs.start_selection_mode,
                selection.game_index,
            )
        {
            restored_art = self.warm_start_art_task();
        }

        // Already open: re-focus; force running restore in case a game started.
        // Duplicate OpenStart (e.g. a second effect before the first open
        // lands) must re-arm the release latch so the opening press cannot
        // promote.
        if self.start_visible
            && let Some(id) = self.start_window
        {
            self.session.start_auto_open_pending = false;
            self.last_running_check = None;
            self.arm_chord_release_latch();
            let badge = self.refresh_running_badge();
            // Scrollable already mounted — reveal is live here.
            let reveal = self.scroll_start_selection_for_open();
            return Task::batch([badge, focus_window(id), reveal]);
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

        let immersive = self.force_immersive_open.take().unwrap_or_else(|| {
            matches!(
                start_mode::on_open(self.session.prefs.start_screen_always_immersive),
                start_mode::ImmersiveTransition::OpenImmersive
            )
        });
        self.start_state.immersive = immersive;
        self.clear_start_activity();
        let (size, position) = if immersive {
            let cover = primary_monitor_cover().unwrap_or(MonitorCover {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            });
            self.start_monitor_cover = Some(cover);
            // Enter veil stays solid until StartOpened arms the capped clock.
            self.start_state.begin_transition_phase(
                start_mode::TransitionPhase::EnterImmersive,
                Instant::now(),
            );
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
        if self.start_cold_load_remaining {
            // Compact uses skeletons + deferred Steam — no cold-load ceremony.
            if immersive {
                self.start_state.begin_cold_load(Instant::now());
            }
        }
        let (id, open) = window::open(window::Settings {
            size,
            position,
            visible: open_visible(),
            transparent: crate::ui::chrome::transparent_windows(),
            resizable: false,
            decorations: false,
            level: window::Level::AlwaysOnTop,
            exit_on_close_request: true,
            platform_specific: overlay_platform_specific(),
            ..window::Settings::default()
        });
        self.start_window = Some(id);
        self.start_visible = true;
        self.session.start_auto_open_pending = false;
        crate::platform::wgpu_diag::note_start_open();
        Task::batch([badge, restored_art, open.map(Message::StartOpened)])
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
        // Promote is never the process-first ceremony.
        let steam = if self.start_state.cold_load_active {
            self.note_cold_load_finished()
        } else {
            Task::none()
        };
        let Some(_transition) =
            start_mode::begin_promote(self.start_state.immersive, self.immersive_transition_busy())
        else {
            return steam;
        };
        // The tuning panel is immersive-settled chrome: never carry it
        // across a window morph (drawer stays closed; reopen from the drawer).
        #[cfg(debug_assertions)]
        self.start_state.request_ambient_tuning(false);
        let cover = primary_monitor_cover().unwrap_or(MonitorCover {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        });
        self.start_monitor_cover = Some(cover);
        self.clear_start_activity();
        self.arm_chord_release_latch();
        crate::controller::hid::diag::diag_info("ui-diag: start immersive enter");
        let now = Instant::now();
        self.start_state
            .begin_transition_phase(start_mode::TransitionPhase::ExitCompact, now);
        // Growth waits for the exit fade: the window holds its size while the
        // outgoing chrome dims in place (frozen, no reflow), then morphs under
        // the opaque Resizing blackout. The morph clock starts there, so every
        // segment runs its full specified duration.
        self.start_state.transition = Some(start_mode::StartTransition::Promoting);
        let compact = Size::new(start_view::WIDTH, start_view::HEIGHT);
        self.start_resize_anim = Some(ResizeAnim {
            from_size: compact,
            to_size: cover.size(),
            from_pos: cover.center_for(compact),
            to_pos: cover.origin(),
            started: now,
        });
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: start promote growth {}x{} -> {}x{}",
            compact.width,
            compact.height,
            cover.size().width,
            cover.size().height
        ));
        // Decode under ExitCompact blackout so settle is more often a warm hit.
        self.prewarm_immersive_art_for_promote();
        steam
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
        // Same as promote: the tuning panel never survives a window morph.
        #[cfg(debug_assertions)]
        self.start_state.request_ambient_tuning(false);
        self.arm_chord_release_latch();
        crate::controller::hid::diag::diag_info("ui-diag: start immersive leave");
        self.clear_start_activity();
        let now = Instant::now();
        self.start_state
            .begin_transition_phase(start_mode::TransitionPhase::ExitImmersive, now);
        // Shrink waits for the exit fade (mirror of promote): the window holds
        // its size while immersive dims in place, then morphs under black.
        self.start_state.transition = Some(start_mode::StartTransition::Demoting);
        let compact = Size::new(start_view::WIDTH, start_view::HEIGHT);
        let cover = self.start_monitor_cover.or_else(primary_monitor_cover);
        let (from_size, from_pos, to_pos) = match cover {
            Some(cover) => (cover.size(), cover.origin(), cover.center_for(compact)),
            None => (
                compact,
                Point::new(
                    ((1920.0 - compact.width) / 2.0).max(0.0),
                    ((1080.0 - compact.height) / 2.0).max(0.0),
                ),
                Point::new(
                    ((1920.0 - compact.width) / 2.0).max(0.0),
                    ((1080.0 - compact.height) / 2.0).max(0.0),
                ),
            ),
        };
        self.start_resize_anim = Some(ResizeAnim {
            from_size,
            to_size: compact,
            from_pos,
            to_pos,
            started: now,
        });
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: start demote shrink {}x{} -> {}x{}",
            from_size.width, from_size.height, compact.width, compact.height
        ));
        Task::none()
    }

    fn tick_immersive_transition_phase(&mut self, now: Instant) -> Option<Task<Message>> {
        // Deferred settle scroll (demote): fires once on the tick after
        // settle, when the remounted list exists (a same-tick op can drop
        // against the pre-swap tree — the StartOpened pattern).
        if let Some(scroll) = self.take_deferred_settle_scroll() {
            return match self.tick_immersive_transition_phase_inner(now) {
                Some(task) => Some(scroll.chain(task)),
                None => Some(scroll),
            };
        }
        self.tick_immersive_transition_phase_inner(now)
    }

    fn tick_immersive_transition_phase_inner(&mut self, now: Instant) -> Option<Task<Message>> {
        // Strand recovery: Resizing advances only via the morph driver — with
        // no anim (lost settle message) wall-clock completion can never fire
        // and the window would sit on black forever. Settle directly while a
        // transition is live; otherwise drop the orphaned blackout and paint
        // current state. Age-gated: right after a normal morph completion the
        // driver is also gone (Settled message still in flight), and settling
        // there would double-settle every transition — only a strand persists
        // past morph-spec + grace.
        if matches!(
            self.start_state.transition_phase.map(|(p, _)| p),
            Some(start_mode::TransitionPhase::Resizing)
        ) && self.start_resize_anim.is_none()
        {
            let age_ms = self
                .start_state
                .transition_phase
                .map(|(_, started)| now.saturating_duration_since(started).as_millis() as u64)
                .unwrap_or(0);
            if age_ms <= start_mode::RESIZE_MS.saturating_add(1000) {
                return None;
            }
            if !self.start_visible {
                self.start_state.transition = None;
                self.start_state.clear_transition_phase();
                return Some(Task::none());
            }
            if self.start_state.transition.is_some() {
                crate::controller::hid::diag::diag_info(
                    "ui-diag: start resizing strand, settling directly",
                );
                return Some(self.finish_start_immersive_transition());
            }
            crate::controller::hid::diag::diag_info(
                "ui-diag: start resizing orphan, clearing blackout",
            );
            self.start_state.clear_transition_phase();
            return Some(Task::none());
        }
        // The HWND morph runs only under the opaque Resizing blackout, never
        // under the exit fade: the outgoing chrome holds its window size (and
        // frozen clocks) through the fade, so nothing reflows mid-exit. The
        // morph's `started` resets when Resizing begins, guaranteeing the
        // full specified duration. (Resizing never finishes by timer.)
        let exiting = matches!(
            self.start_state.transition_phase.map(|(p, _)| p),
            Some(start_mode::TransitionPhase::ExitCompact)
                | Some(start_mode::TransitionPhase::ExitImmersive)
        );
        if !exiting && let Some(task) = self.tick_resize_anim(now) {
            return Some(task);
        }
        if !self.start_state.phase_finished(now) {
            return None;
        }
        let (phase, started) = self.start_state.transition_phase?;
        // Overrun witness for the next forensics: completions are wall-clock,
        // so a phase ending far past its spec means ticks starved (the black
        // window outlived the ceremony in paint-time, not logic-time).
        let elapsed_ms = now.saturating_duration_since(started).as_millis() as u64;
        let allowance_ms = match phase {
            // Art hold (up to ENTER_ART_HOLD_MAX_MS) precedes the lift.
            start_mode::TransitionPhase::EnterImmersive => {
                crate::ui::start::mode::ENTER_ART_HOLD_MAX_MS
                    + crate::ui::start::mode::ENTER_REVEAL_MS
                    + 1000
            }
            _ => phase.duration_ms().saturating_add(1000),
        };
        if elapsed_ms > allowance_ms {
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: start phase overrun phase={} elapsed_ms={elapsed_ms} spec_ms={}",
                phase.label(),
                phase.duration_ms()
            ));
        }
        match phase {
            // Exit fade done over a static window: start the morph now, under
            // black (or settle directly if the morph already ran).
            start_mode::TransitionPhase::ExitCompact
            | start_mode::TransitionPhase::ExitImmersive => {
                if let Some(anim) = self.start_resize_anim.as_mut() {
                    anim.started = now;
                    self.start_state
                        .begin_transition_phase(start_mode::TransitionPhase::Resizing, now);
                }
                None
            }
            start_mode::TransitionPhase::EnterImmersive => {
                self.start_state.clear_transition_phase();
                // Veil phase over — begin held ambient→splash on this same clock tick.
                self.start_state.try_start_pending_splash_reveal(now);
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: start immersive transition phase={} done",
                    phase.label()
                ));
                // Second exact snap: stepped tasks can coalesce, so re-assert
                // the cover here (~one reveal after settle) — a no-op when
                // already exact, a heal when the animation lost steps.
                let resnap = self.snap_promote_target();
                Some(resnap.chain(self.flush_deferred_steam_scan_after_promote()))
            }
            start_mode::TransitionPhase::EnterCompact => {
                self.start_state.clear_transition_phase();
                self.start_state.ambient_time = 0.0;
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: start immersive transition phase={} done",
                    phase.label()
                ));
                // Center backstop, conditional: the deferred + mount
                // corrections usually landed it invisibly behind the veil. A
                // match issues nothing, so a correct reveal can never snap
                // here; a real mismatch still lands centered (visibly, but
                // correctly) and the log line marks it.
                if let Some((id, target)) = self.start_state.open_reveal_scroll() {
                    let offset = start_view::StartListId::from_scroll_id(&id)
                        .map(|list| self.start_state.list_offset(list))
                        .unwrap_or(target);
                    if (target - offset).abs() > 2.0 {
                        crate::controller::hid::diag::diag_info(format!(
                            "ui-diag: start demote reveal backstop offset={offset:.0} target={target:.0}"
                        ));
                        return Some(self.scroll_start_selection_for_open());
                    }
                }
                None
            }
            start_mode::TransitionPhase::Resizing => None,
        }
    }

    /// One animated-resize step (either direction). Returns `None` when no
    /// animation is running (normal tick flow continues). Only ticked during
    /// the opaque Resizing phase — never under an exit fade — with `started`
    /// reset at Resizing begin, so the morph always runs its full duration.
    ///
    /// Completion issues the exact target geometry chained into settle, so
    /// the enter veil only ever lifts on exact geometry — never mid-morph.
    /// (Settling mid-growth revealed chrome at the wrong size with a snap
    /// jump at the end.)
    fn tick_resize_anim(&mut self, now: Instant) -> Option<Task<Message>> {
        let anim = self.start_resize_anim?;
        let Some(id) = self.start_window.filter(|_| self.start_visible) else {
            self.start_resize_anim = None;
            self.start_state.transition = None;
            self.start_state.clear_transition_phase();
            return Some(Task::none());
        };
        let t = start_mode::phase_progress_linear(
            now.saturating_duration_since(anim.started).as_millis() as u64,
            start_mode::RESIZE_MS,
        );
        if t >= 1.0 {
            self.start_resize_anim = None;
            let elapsed_ms = now.saturating_duration_since(anim.started).as_millis() as u64;
            // Overrun witness: the morph is wall-clock, so finishing far past
            // spec means ticks starved mid-morph (black outlived the ceremony).
            if elapsed_ms > start_mode::RESIZE_MS.saturating_add(1000) {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: start resize overrun elapsed_ms={elapsed_ms} spec_ms={}",
                    start_mode::RESIZE_MS
                ));
            }
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: start resize anim done size={}x{}",
                anim.to_size.width, anim.to_size.height
            ));
            let exact = window::resize(id, anim.to_size).chain(move_window(id, anim.to_pos));
            if self.start_state.transition.is_none()
                && matches!(
                    self.start_state.transition_phase.map(|(p, _)| p),
                    Some(start_mode::TransitionPhase::Resizing)
                )
            {
                // Orphaned black tail (settle already ran or was skipped):
                // clear it so the settled chrome paints.
                self.start_state.clear_transition_phase();
            }
            return Some(exact.chain(Task::done(Message::StartImmersiveSettled)));
        }
        let eased = start_mode::ease_in_out_cubic(t);
        let size = Size::new(
            anim.from_size.width + (anim.to_size.width - anim.from_size.width) * eased,
            anim.from_size.height + (anim.to_size.height - anim.from_size.height) * eased,
        );
        let pos = Point::new(
            anim.from_pos.x + (anim.to_pos.x - anim.from_pos.x) * eased,
            anim.from_pos.y + (anim.to_pos.y - anim.from_pos.y) * eased,
        );
        Some(window::resize(id, size).chain(move_window(id, pos)))
    }

    /// Exact snap to the promote target (cover). Stepped tasks can coalesce,
    /// so settle re-asserts: a degraded animation heals here instead of
    /// leaving immersive chrome in a compact window.
    fn snap_promote_target(&self) -> Task<Message> {
        let (Some(id), Some(cover)) = (
            self.start_window.filter(|_| self.start_visible),
            self.start_monitor_cover,
        ) else {
            return Task::none();
        };
        window::resize(id, cover.size()).chain(move_window(id, cover.origin()))
    }

    /// Exact snap to the demote target (centered compact). Same heal as
    /// [`snap_promote_target`] for the shrink direction.
    fn snap_compact_target(&self) -> Task<Message> {
        let Some(id) = self.start_window.filter(|_| self.start_visible) else {
            return Task::none();
        };
        let size = Size::new(start_view::WIDTH, start_view::HEIGHT);
        let position = self
            .start_monitor_cover
            .or_else(primary_monitor_cover)
            .map(|cover| cover.center_for(size))
            .unwrap_or(Point::new(
                ((1920.0 - size.width) / 2.0).max(0.0),
                ((1080.0 - size.height) / 2.0).max(0.0),
            ));
        window::resize(id, size).chain(move_window(id, position))
    }

    fn finish_start_immersive_transition(&mut self) -> Task<Message> {
        let Some(transition) = self.start_state.transition.take() else {
            return Task::none();
        };
        // The animation already resolved (or never ran); never leave a stale
        // driver behind to resize a settled window.
        self.start_resize_anim = None;
        let immersive = start_mode::settle_immersive(transition);
        self.start_state.immersive = immersive;
        self.start_state.dock_expanded = false;
        self.start_state.slide = StartSlide::Games;
        // Dock/strip anims cleared via transition phase begin + request_dock defaults.
        let now = Instant::now();
        // Exact snap first: stepped tasks can coalesce, so settle re-asserts
        // geometry before flipping presentation — immersive chrome can never
        // land in a compact window (or vice versa).
        let snap: Task<Message> = if immersive {
            self.snap_promote_target()
        } else {
            self.snap_compact_target()
        };
        if immersive {
            // Keep ambient_time; open aperture + chrome.
            // Lift onto ambient+dock immediately (cold-style); list stays hidden until art sync.
            self.start_state
                .begin_transition_phase(start_mode::TransitionPhase::EnterImmersive, now);
            self.start_state.prime_backdrop_for_enter(now);
            self.start_state.arm_enter_reveal(now);
            // Promote settle re-shows the position rail (the transition-phase
            // begin above cleared it). Pinned is a no-op show.
            self.start_state.note_position_bar_scroll(now);
            if !self.start_state.try_arm_enter_reveal_when_art_ready(now) {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: start immersive enter art hold (promote) steam_scan={} list_hidden=1",
                    u8::from(self.steam_scan_pending)
                ));
            }
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: start immersive settle promote backdrop re-arm sel={}",
                self.start_state.game_selected
            ));
        } else {
            self.start_monitor_cover = None;
            self.clear_start_activity();
            self.start_state.clear_backdrop_transition();
            self.start_state
                .begin_transition_phase(start_mode::TransitionPhase::EnterCompact, now);
        }
        self.arm_chord_release_latch();
        let kind = if immersive { "promote" } else { "demote" };
        crate::controller::hid::diag::diag_info(format!("ui-diag: start immersive settle {kind}"));
        let focus = self
            .start_window
            .map(|id| focus_window(id).chain(raise_window_topmost(id)))
            .unwrap_or_else(Task::none);
        if immersive {
            snap.chain(focus).chain(self.warm_immersive_art_task())
        } else {
            // Center selection as soon as compact chrome exists: issue now
            // (may drop against the pre-swap tree) AND stow for the next
            // tick, when the remounted list exists (StartOpened pattern).
            // EnterCompact completion re-centers as backstop.
            let immediate = self.scroll_start_selection_for_open();
            self.stow_deferred_settle_scroll();
            if let Some((sel, rows, row_top, vh, y)) = self.start_state.games_reveal_debug() {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: start demote reveal settle sel={sel} rows={rows} row_top={row_top:.0} vh={vh:.0} y={y:.0}"
                ));
            }
            snap.chain(focus).chain(immediate)
        }
    }

    /// Stow the open-time reveal for the tick after settle: the compact list
    /// remounts under the new tree, so a same-tick `scroll_to` can drop.
    /// Fires once (next tick, still behind the opaque lift), then clears.
    fn stow_deferred_settle_scroll(&mut self) {
        let Some((id, y)) = self.start_state.open_reveal_scroll() else {
            self.deferred_settle_scroll = None;
            return;
        };
        if let Some(list) = start_view::StartListId::from_scroll_id(&id) {
            self.deferred_settle_scroll = Some((list, y));
        }
    }

    /// Issue the stowed settle scroll once, then drop it. Recomputes at fire
    /// time: the remount's first `on_scroll` may have reported the real
    /// viewport since settle, and centering against a stale/estimate viewport
    /// lands off-center (visible as a snap when the completion backstop
    /// re-issues it after the lift). Falls back to the stowed target when
    /// nothing is computable.
    fn take_deferred_settle_scroll(&mut self) -> Option<Task<Message>> {
        let (list, stowed_y) = self.deferred_settle_scroll.take()?;
        if !self.start_visible {
            return None;
        }
        let y = match self.start_state.open_reveal_scroll() {
            Some((id, y)) if start_view::StartListId::from_scroll_id(&id) == Some(list) => y,
            _ => stowed_y,
        };
        if (y - stowed_y).abs() > 0.5 {
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: start demote reveal deferred list={list:?} vh={:.0} y={y:.0} (stowed {stowed_y:.0})",
                self.start_state.list_viewport(list)
            ));
        }
        self.start_state.note_scroll_y(&list.scroll_id(), y);
        Some(operation::scroll_to(
            list.scroll_id(),
            operation::AbsoluteOffset {
                x: None,
                y: Some(y),
            },
        ))
    }

    /// Arm the debounced release latch and mark any locally visible chord consumed.
    ///
    /// When no fresh readings are published, leave detectors alone (do not
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
                // No fresh readings — resetting would disarm detectors
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

    /// Stamp idle/sleep activity (does not unhide the cursor — pad/keyboard stay mouse-free).
    fn note_start_activity(&mut self) {
        let now = Instant::now();
        self.start_last_activity = now;
        self.wake_start_idle(now, false);
    }

    /// Mouse move/click/wheel: wake idle/sleep and show the cursor.
    fn note_start_cursor_activity(&mut self) {
        let now = Instant::now();
        self.start_last_activity = now;
        self.start_cursor_last_active = now;
        self.set_start_cursor_hidden(false);
        self.wake_start_idle(now, false);
    }

    /// Reset activity clocks without treating it as a sleep-wake (open/promote/close).
    fn clear_start_activity(&mut self) {
        let now = Instant::now();
        self.start_last_activity = now;
        self.start_cursor_last_active = now;
        self.set_start_cursor_hidden(false);
        self.start_state.reset_idle_dim();
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

    /// Wake idle/sleep overlay toward Active. `from_sleep` marks pad-gated wake.
    fn wake_start_idle(&mut self, now: Instant, from_sleep: bool) {
        use crate::ui::start::idle::IdlePhase;
        if !self.start_visible || !self.start_state.immersive {
            return;
        }
        if matches!(self.start_state.idle_phase, IdlePhase::Active)
            && !self.start_state.idle_sleep_wake_pending
            && !self.start_state.idle_dim_animating(now)
        {
            return;
        }
        let was_sleep = matches!(self.start_state.idle_phase, IdlePhase::Sleep) || from_sleep;
        let inactive_dim = f32::from(self.session.prefs.start_screen_inactive_dim_percent) / 100.0;
        if self
            .start_state
            .set_idle_phase(IdlePhase::Active, inactive_dim, now, was_sleep)
        {
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: start idle phase={} wake_from_sleep={}",
                IdlePhase::Active.label(),
                u8::from(was_sleep),
            ));
        }
    }

    /// Advance idle/sleep timeouts + dim anim. Returns true if pad should re-arm after sleep wake.
    fn tick_start_idle(&mut self, now: Instant) -> bool {
        use crate::ui::start::idle::{self as start_idle};

        if !self.start_visible || !self.start_state.immersive {
            self.start_state.reset_idle_dim();
            return false;
        }
        // Don't advance idle during promote/demote ceremony.
        if self.start_state.transition_phase.is_some() || self.start_state.transition.is_some() {
            let clear_gate = self.start_state.tick_idle_dim(now);
            return clear_gate;
        }

        let clear_gate = self.start_state.tick_idle_dim(now);

        // While waking from sleep, keep targeting Active (activity already stamped).
        if self.start_state.idle_sleep_wake_pending {
            return clear_gate;
        }

        let idle_secs = now
            .saturating_duration_since(self.start_last_activity)
            .as_secs_f32();
        let target = start_idle::phase_for_idle_secs(
            idle_secs,
            self.session.prefs.start_screen_inactive_secs,
            self.session.prefs.start_screen_sleep_secs,
        );
        if target != self.start_state.idle_phase {
            let inactive_dim =
                f32::from(self.session.prefs.start_screen_inactive_dim_percent) / 100.0;
            if self
                .start_state
                .set_idle_phase(target, inactive_dim, now, false)
            {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: start idle phase={}",
                    target.label()
                ));
            }
        }
        clear_gate
    }

    fn close_start_screen(&mut self) -> Task<Message> {
        // Clear visibility before any hide task so Resting toast RaiseInteractive
        // cannot resurface Start while the hide is in flight.
        self.start_visible = false;
        self.start_nav_ready = false;
        self.start_revealed_at = None;
        self.start_state.immersive = false;
        // Tuning is immersive-settled state: never reopen with it on.
        #[cfg(debug_assertions)]
        self.start_state.request_ambient_tuning(false);
        self.start_state.clear_immersive_session();
        // Drop any in-flight promote/demote ceremony (animated growth must
        // not keep resizing a closing window; reopen starts clean).
        self.start_state.transition = None;
        self.start_state.clear_transition_phase();
        self.start_resize_anim = None;
        self.deferred_settle_scroll = None;
        // Invalidate in-flight art events; keep caches for fast reopen.
        // Process Exit is what wipes to a blank slate (`art_worker::shutdown`).
        let generation = crate::ui::start::icon_cache::bump_art_warm_generation();
        crate::ui::start::art_worker::cancel_jobs(generation);
        self.start_state.art_awaiting_paint = false;
        self.start_monitor_cover = None;
        self.clear_start_activity();
        self.haptic_pad_serial = None;
        self.hid_worker.rumble_stop_all();
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
        self.last_edge_seq = None;
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

    /// Row-aware settings nav cue: splits sound/haptic so the pre-flip nav
    /// cue never double-plays the modality the apply side previews after
    /// saving (volume rows → sound preview, strength rows → haptic preview).
    /// Sound rows play haptic-only here; haptic rows play sound-only.
    fn play_start_settings_cue(
        &self,
        row: crate::ui::start::settings::SettingsRow,
        kind: UiSoundKind,
    ) {
        match crate::ui::start::settings::settings_nav_cue(row) {
            crate::ui::start::settings::SettingsCue::Full => self.play_start_cue(kind),
            crate::ui::start::settings::SettingsCue::HapticOnly => self.play_start_haptic(kind),
            crate::ui::start::settings::SettingsCue::SoundOnly => self.play_start_sound(kind),
        }
    }

    fn play_start_haptic(&self, kind: UiSoundKind) {
        if !self.session.prefs.start_screen_haptics_enabled
            || crate::platform::debug_quiet::enabled()
        {
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
        self.hid_worker
            .rumble(serial.clone(), right, left, duration.as_millis() as u64);
    }

    /// Preview haptic on every connected DualSense (settings toggle / strength slider).
    fn preview_haptic_all(&self, pulse: MotorPulse) {
        if !self.session.prefs.start_screen_haptics_enabled
            || crate::platform::debug_quiet::enabled()
        {
            return;
        }
        let (right, left, duration) =
            pulse.scaled(self.session.prefs.start_screen_haptics_strength);
        if right == 0 && left == 0 {
            return;
        }
        let ms = duration.as_millis() as u64;
        for controller in &self.session.controllers {
            self.hid_worker
                .rumble(controller.serial.clone(), right, left, ms);
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
        // Sort stays live in edit mode: the checklist re-sorts under the same
        // mode and the prefs write persists across save/cancel.
        if self.start_state.slide != StartSlide::Games || self.start_state.overlay_blocking() {
            return None;
        }
        self.session.prefs.games_sort_mode = self.session.prefs.games_sort_mode.cycle();
        self.save_prefs();
        self.refresh_start_rows();
        if self.start_state.immersive {
            self.start_state
                .note_position_bar_scroll(std::time::Instant::now());
        }
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
        self.save_prefs();
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
        let close_task = self.close_running_game();
        self.start_state.game_selected = confirm.next_index;
        if self.launch_selected() && self.start_state.immersive {
            return close_task.chain(self.close_start_screen());
        }
        close_task
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

        // Don't relaunch while this row is still closing.
        if self
            .start_state
            .closing_target
            .as_ref()
            .is_some_and(|t| t == &row.target)
        {
            return false;
        }

        // Disabled rows never launch (uninstalled Steam or missing manual target).
        // Re-validate at launch time so a stale row (scan race / immediate launch
        // after uninstall) fails gracefully instead of opening Steam / error popup.
        if row.disabled || self.is_row_invalid_now(&row) {
            app_log::warn(format!(
                "launch blocked (no longer installed): {} ({})",
                row.title, row.target
            ));
            // Refresh disabled flags so the row dims immediately.
            self.refresh_start_rows();
            return false;
        }

        // Steam pre-flight (revoked license / wiped files, e.g. an expired
        // alpha): the manifest can linger while the install dir is missing
        // or empty, and ShellExecute would happily hand off to a Steam error
        // dialog. Block before creating a running session so `Playing`
        // never parks on a game that cannot spawn a process.
        if let Some(appid) = steam_appid_from_target(&row.target)
            && !steam::install_ready_for_appid(appid)
        {
            app_log::warn(format!(
                "launch blocked (steam install not ready): {} (appid {appid})",
                row.title
            ));
            self.launch_blocked_steam.insert(appid);
            self.refresh_start_rows();
            return false;
        }

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
            self.sync_launch_badges();
        }

        match launch::launch_target(&row.target, &row.args) {
            Ok(()) => {
                // No last-played stamp here: it lands on the first confirm
                // (process or Steam verdict) in `apply_running_badge_from_images`.
                self.begin_running_session(row.title, row.target, row.play_key);
                true
            }
            Err(err) => {
                app_log::warn(format!("launch failed for {}: {err}", row.target));
                // Immediate-launch race (uninstalled between scan and press):
                // refresh so the row dims instead of staying launchable.
                self.refresh_start_rows();
                false
            }
        }
    }

    /// Re-validate a row at press time (scan may be stale for immediate launches).
    fn is_row_invalid_now(&self, row: &start_view::StartRow) -> bool {
        if row.skeleton {
            return true;
        }
        // Steam URI: check installed list when known.
        if let Some(appid) = steam_appid_from_target(&row.target) {
            if let Some(ids) = self.steam_installed.as_deref() {
                return !ids.contains(&appid);
            }
            return false;
        }
        // Manual file target: absolute missing path is invalid.
        is_manual_target_missing(&row.target)
    }

    fn on_pad_input(&mut self, edge: crate::domain::pad::InputEdge) -> Task<Message> {
        let poll_started = Instant::now();
        let start_open = self.start_visible;
        // Edge transport is lossy by design (try_send drops when full): log
        // seq gaps while Start is open so silent d-pad losses are visible.
        // (Seq also advances on unpublished cold snapshots, so a gap at open
        // while input-hot flaps is expected; steady-state gaps mean drops.)
        if start_open {
            if let Some(prev) = self.last_edge_seq
                && edge.seq > prev + 1
            {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: pad-input edge gap dropped={} expected={} got={} reason={}",
                    edge.seq - prev - 1,
                    prev + 1,
                    edge.seq,
                    edge.reason.as_str(),
                ));
            }
            self.last_edge_seq = Some(edge.seq);
        } else {
            self.last_edge_seq = None;
        }
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
        let listening = self.wants_pad_input();

        if pad_input_open {
            self.apply_pad_input_panel_from_readings(&readings);
        }

        if !listening {
            // Start is closed and the PadInput tab is not open: the only
            // live consumer is the reopen chord (PS press to open Start).
            // Edges only arrive here while pads are connected, so this is
            // exactly the "PS to open" path — never drop it.
            if !start_open {
                return self.on_reopen_gesture(&readings);
            }
            return Task::none();
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

    /// True when left/right should scroll the Games strip instead of switching
    /// slides: horizontal immersive layout on the Games slide with no overlays
    /// (settings / replace-confirm / manual-add) open. Edit mode is a mode,
    /// not an overlay, so it stays included: left/right move selection (all
    /// rows are enabled in the checklist) while Up/Down are suppressed and
    /// L2/R2 keep slide switching.
    fn immersive_horizontal_games_nav(&self) -> bool {
        self.start_state.immersive
            && matches!(
                self.session.prefs.start_screen_immersive_layout,
                crate::persist::prefs::ImmersiveLayout::Horizontal
            )
            && matches!(self.start_state.slide, StartSlide::Games)
            && !self.start_state.settings.open
            && self.start_state.replace_confirm.is_none()
            && self.start_state.manual_add.is_none()
    }

    fn handle_start_nav_readings(&mut self, readings: &[start_input::NavReading]) -> Task<Message> {
        self.nav_missing_warned = false;

        let now = Instant::now();
        let pad_active = readings
            .iter()
            .any(|r| !start_input::sample_nav_resting(&r.sample));

        // Sleep / sleep-wake: pad may only begin wake — no nav, dock, cancel, or cues.
        if self.start_state.idle_pad_gated() {
            if pad_active {
                self.start_last_activity = now;
                self.wake_start_idle(now, true);
                crate::controller::hid::diag::diag_info(
                    "ui-diag: start idle pad gated (sleep wake)",
                );
            }
            return Task::none();
        }

        // Cold ambient: no pad nav / promote / launch (keyboard Escape still closes).
        if self.start_state.cold_load_active {
            if pad_active {
                self.note_start_activity();
            }
            return Task::none();
        }

        if pad_active {
            self.note_start_activity();
        }

        let gesture = gesture::default_gesture();
        self.start_state.reopen_chord_held = start_mode::promote_gesture_usable(&gesture)
            && chord_held_on_any_pad(&gesture, readings);

        // Reopen chord toggles compact ↔ immersive while Start is open.
        if let Some(toggle) = self.try_toggle_immersive_from_gesture(readings) {
            return toggle;
        }

        // Debug R3: hide/show all chrome while tuning (pad-only; R3 carries
        // no NavAction). Tuning-scoped, so it never fires in normal chrome.
        #[cfg(debug_assertions)]
        if let Some(task) = self.toggle_ambient_chrome_from_r3(readings) {
            return task;
        }

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
        let settings_open = self.start_state.settings.open;
        let allow_nav_move = settings_open || (!animating && !replace_confirm && !manual_add);
        let editing = self.start_state.editing;
        // Only when the selected row shows Close game (running target match).
        let hold_cross_close = !editing
            && !replace_confirm
            && !manual_add
            && !settings_open
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
            && !settings_open
            && !animating
            && matches!(self.start_state.slide, StartSlide::Controllers)
            && self
                .start_state
                .selected_controller()
                .is_some_and(|row| row.show_power_off());
        // Immersive: left/right slides. Settings: Left/Right nudge sliders (compact too).
        let horizontal_nav = self.start_state.immersive || settings_open;
        // Horizontal Games strip: left/right scroll games, L2/R2 switch slides.
        let horizontal_games_nav = self.immersive_horizontal_games_nav();
        let tick = self.pad_nav.tick(
            readings,
            now,
            allow_nav_move,
            horizontal_nav,
            horizontal_games_nav,
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

        let nav_task = if self.start_state.ambient_tuning_active() {
            // Debug tuning panel owns pad nav while open: Up/Down focus,
            // Left/Right nudge, Cross confirm, Circle done. Holds reset so
            // game close / power-off cannot arm under the panel, and
            // overlay_blocking already parks the message-driven game paths.
            self.start_state.cross_progress = 0.0;
            self.start_state.triangle_progress = 0.0;
            self.keyboard_cross_hold.reset();
            self.start_state.tick_hint_anims(now);
            self.on_ambient_tuning_nav(tick.action)
        } else if tick.action == Some(NavAction::ToggleSettings) {
            self.start_state.tick_hint_anims(now);
            self.on_start_message(StartMessage::ToggleSettings)
        } else if settings_open {
            self.start_state.cross_progress = 0.0;
            self.start_state.triangle_progress = 0.0;
            self.keyboard_cross_hold.reset();
            self.start_state.tick_hint_anims(now);
            self.on_start_settings_nav(tick.action)
        } else if animating {
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

            let mut close_task = Task::none();
            if hold_cross_close {
                self.start_state.cross_progress = tick.cross_progress;
                if tick.cross_completed {
                    self.play_start_cue(UiSoundKind::Hold);
                    close_task = self.close_running_game_if_selected();
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
                close_task
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
                    NavAction::ToggleSettings => {
                        self.on_start_message(StartMessage::ToggleSettings)
                    }
                }
            } else {
                Task::none()
            }
        };

        Task::batch([badge_task, nav_task])
    }

    /// Generic dropdown toggle for selector rows: collapsing the open row
    /// just closes it; opening inits the highlight to the current value,
    /// closes any other menu (single menu), and scrolls it into view.
    fn toggle_start_selector_menu(
        &mut self,
        row: crate::ui::start::settings::SettingsRow,
    ) -> Task<Message> {
        let snapshot = self.start_settings_snapshot();
        let opened = self.start_state.settings.toggle_menu(row, &snapshot);
        self.play_start_cue(UiSoundKind::Nav);
        if opened {
            self.scroll_settings_focus_into_view(start_view::ScrollReveal::Either)
        } else {
            Task::none()
        }
    }

    fn scroll_settings_focus_into_view(
        &mut self,
        direction: start_view::ScrollReveal,
    ) -> Task<Message> {
        let snapshot = self.start_settings_snapshot();
        let immersive = self.start_state.immersive;
        let Some(y) = self
            .start_state
            .settings
            .focus_scroll_y(&snapshot, immersive, direction)
        else {
            return Task::none();
        };
        self.start_state.sync_settings_scroll(y);
        operation::scroll_to(
            crate::ui::start::settings::settings_scroll_id(),
            operation::AbsoluteOffset {
                x: None,
                y: Some(y),
            },
        )
    }

    /// Reveal the trailing reload row after an AA/vsync toggle dirtied the
    /// renderer prefs (the row mounts below the viewport while focus stays
    /// on the toggle). No-op when prefs are clean again (unmount needs no
    /// scroll; nav clamps focus), when the settings surface is closed (e.g.
    /// the Configure pick-list path), or when the row is already visible.
    fn scroll_perf_reload_into_view(&mut self) -> Task<Message> {
        if !self.start_state.settings.open {
            return Task::none();
        }
        let snapshot = self.start_settings_snapshot();
        if !renderer_restart_needed(
            snapshot.perf_antialiasing,
            snapshot.perf_vsync,
            snapshot.perf_aa_boot,
            snapshot.perf_vsync_boot,
        ) {
            return Task::none();
        }
        let immersive = self.start_state.immersive;
        let Some(y) = self.start_state.settings.reload_scroll_y(
            &snapshot,
            immersive,
            start_view::ScrollReveal::Either,
        ) else {
            return Task::none();
        };
        self.start_state.sync_settings_scroll(y);
        operation::scroll_to(
            crate::ui::start::settings::settings_scroll_id(),
            operation::AbsoluteOffset {
                x: None,
                y: Some(y),
            },
        )
    }

    fn on_start_settings_nav(&mut self, action: Option<NavAction>) -> Task<Message> {
        let Some(action) = action else {
            return Task::none();
        };
        // Layout transition reflows main content: drop focus moves + slider nudges
        // (pad and keyboard alike) while it runs; Confirm/Cancel stay live, and
        // an open-menu Confirm still commits — deliberate picks are never
        // frozen, only navigation is.
        if self.start_state.layout_transition_active()
            && matches!(
                action,
                NavAction::Up | NavAction::Down | NavAction::PrevSlide | NavAction::NextSlide
            )
        {
            return Task::none();
        }
        // An expanded dropdown captures all directional + confirm/cancel input.
        if self.start_state.settings.any_menu_open() {
            return self.on_selector_menu_nav(action);
        }
        let snapshot = self.start_settings_snapshot();
        self.start_state.settings.clamp_focus(&snapshot);
        match action {
            NavAction::Up => {
                let before = self.start_state.settings.focus;
                if self.start_state.settings.move_focus(-1, &snapshot) {
                    self.play_start_cue(UiSoundKind::Nav);
                    let after = self.start_state.settings.focus;
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: start settings focus={after}"
                    ));
                    let dir = crate::ui::start::settings::reveal_for_focus_step(before, after, -1);
                    return self.scroll_settings_focus_into_view(dir);
                }
                Task::none()
            }
            NavAction::Down => {
                let before = self.start_state.settings.focus;
                if self.start_state.settings.move_focus(1, &snapshot) {
                    self.play_start_cue(UiSoundKind::Nav);
                    let after = self.start_state.settings.focus;
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: start settings focus={after}"
                    ));
                    let dir = crate::ui::start::settings::reveal_for_focus_step(before, after, 1);
                    return self.scroll_settings_focus_into_view(dir);
                }
                Task::none()
            }
            NavAction::PrevSlide | NavAction::NextSlide => {
                let delta = if matches!(action, NavAction::PrevSlide) {
                    -1
                } else {
                    1
                };
                let Some(row) = self.start_state.settings.focused_row(&snapshot) else {
                    return Task::none();
                };
                if let Some(msg) = crate::ui::start::settings::nudge_message(row, &snapshot, delta)
                {
                    self.play_start_settings_cue(row, UiSoundKind::Nav);
                    return self.on_start_message(msg);
                }
                Task::none()
            }
            NavAction::Confirm => {
                let Some(row) = self.start_state.settings.focused_row(&snapshot) else {
                    return Task::none();
                };
                if let Some(msg) = crate::ui::start::settings::confirm_message(row, &snapshot) {
                    self.play_start_settings_cue(row, UiSoundKind::Action);
                    let task = self.on_start_message(msg);
                    let snap = self.start_settings_snapshot();
                    self.start_state.settings.clamp_focus(&snap);
                    return task;
                }
                // Slider rows: Cross does nothing (use Left/Right).
                Task::none()
            }
            NavAction::Cancel | NavAction::ToggleSettings => {
                self.on_start_message(StartMessage::Close)
            }
            _ => Task::none(),
        }
    }

    /// Pad navigation while a dropdown selector menu is expanded.
    /// Directionals move the highlight (wrapping), Cross selects and closes,
    /// Circle/Options close without changing anything. The open menu's row
    /// parameterizes the option list; committing routes through the row's
    /// select message so apply + close run even when re-picking the value.
    fn on_selector_menu_nav(&mut self, action: NavAction) -> Task<Message> {
        let Some((row, highlight)) = self.start_state.settings.menu else {
            return Task::none();
        };
        let count = row.selector_count() as isize;
        match action {
            NavAction::Up | NavAction::PrevSlide => {
                let next = (highlight as isize - 1).rem_euclid(count) as usize;
                self.start_state.settings.set_menu_highlight(next);
                self.play_start_cue(UiSoundKind::Nav);
                self.scroll_settings_focus_into_view(start_view::ScrollReveal::Either)
            }
            NavAction::Down | NavAction::NextSlide => {
                let next = (highlight as isize + 1).rem_euclid(count) as usize;
                self.start_state.settings.set_menu_highlight(next);
                self.play_start_cue(UiSoundKind::Nav);
                self.scroll_settings_focus_into_view(start_view::ScrollReveal::Either)
            }
            NavAction::Confirm => {
                let select = row.selector_select(highlight);
                self.play_start_cue(UiSoundKind::Action);
                self.on_start_message(select)
            }
            NavAction::Cancel | NavAction::ToggleSettings => {
                self.start_state.settings.close_menus();
                self.play_start_cue(UiSoundKind::Nav);
                Task::none()
            }
            _ => Task::none(),
        }
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
        if !self.session.prefs.start_screen_enabled {
            return Task::none();
        }
        let required = gesture::default_gesture();
        let chord_held = chord_held_on_any_pad(&required, readings);
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
                let _ = self.gesture_detectors.update(&required, readings);
                return Task::none();
            }
            ChordReleaseTick::Open => {}
        }
        if self.gesture_detectors.update(&required, readings) {
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
        let required = gesture::default_gesture();
        if start_mode::chord_is_circle_only(&required) {
            return None;
        }
        let chord_held = chord_held_on_any_pad(&required, readings);
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
                let _ = self.gesture_detectors.update(&required, readings);
                return None;
            }
            ChordReleaseTick::Open => {}
        }
        if !self.gesture_detectors.update(&required, readings) {
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
        mut events: Vec<NotifyEvent>,
        mut open_start_after: bool,
    ) -> Task<Message> {
        let connect_open_start =
            open_start_after && events.iter().any(|event| event.body == "Connected");
        let immersive_connect =
            connect_open_start && self.session.prefs.start_screen_always_immersive;
        // Immersive auto-open never shows a Connected toast (cold-launch via
        // launch_quiet and hot connects must match). Session already filters,
        // but drop any that slipped through so behaviour is bullet-proof.
        if immersive_connect {
            let before = events.len();
            events.retain(|event| event.body != "Connected");
            if events.len() != before {
                crate::controller::hid::diag::diag_info(
                    "ui-diag: connect toast suppressed (immersive auto-open)",
                );
            }
            let open = self.open_start_screen();
            // Queue any non-Connected events normally (Low/Charged); Connected is gone.
            for event in events {
                let eta = self.toast_eta_for(&event);
                let message = ToastMessage::from_notification(
                    event,
                    self.session.prefs.spectrum.clone(),
                    eta,
                );
                ToastMessage::enqueue(&mut self.toast_queue, message);
            }
            return open.chain(self.show_next_toast());
        }
        // Compact auto-open is toast-first: latch Start onto the toast settle so
        // the Connected card always presents (the toast machine emits OpenStart
        // exactly once — on rest, early dismiss, or slide-out finish).
        for event in events {
            let eta = self.toast_eta_for(&event);
            let mut message =
                ToastMessage::from_notification(event, self.session.prefs.spectrum.clone(), eta);
            if open_start_after && message.body == "Connected" {
                open_start_after = false;
                message.after = AfterToast::OpenStart;
                crate::controller::hid::diag::diag_info(
                    "ui-diag: defer start until toast slide settles",
                );
            }
            ToastMessage::enqueue(&mut self.toast_queue, message);
        }
        self.show_next_toast()
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
    // Deep enough to ride out Start-frame hitches without dropping taps:
    // the worker publishes at report rate while hot.
    stream::channel(128, async move |mut output| {
        let (tx, mut rx) = iced::futures::channel::mpsc::channel(128);
        start_input::bind_input_edge_sender(tx);
        let _guard = InputEdgeBindGuard;
        while let Some(first) = rx.next().await {
            // Conflate to the latest buffered edge: readings are absolute
            // state, so intermediates are stale by definition. Without this,
            // a saturated loop pays a full nav/update/view cycle per queued
            // edge after it recovers — a backlog spree that prolongs the
            // stall instead of ending it. Normally the buffer holds ≤1 and
            // this is a no-op (taps are never at risk when the loop is
            // healthy; when saturated they are already being dropped at
            // try_send).
            let mut edge = first;
            while let Ok(next) = rx.try_recv() {
                edge = next;
            }
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

/// Steam `Apps` registry watch for iced.
///
/// Bridges the `steam_watch` thread (fires on a system wait) into the iced
/// runtime so a `Running` flip re-reads the badge immediately instead of
/// waiting for the next tick. Like `tray_events`, the guard lives as long as
/// the stream: dropping the subscription (session cleared) stops the thread.
fn steam_apps_watch_stream() -> impl Stream<Item = Message> {
    stream::channel(16, async move |mut output| {
        let (tx, mut rx) = iced::futures::channel::mpsc::channel::<()>(16);
        // `try_send` takes `&mut self`; the watcher callback is `Fn`, so
        // share the sender behind a mutex (uncontended: one watcher, one drain).
        let tx = std::sync::Mutex::new(tx);
        let _watched = crate::platform::steam_watch::spawn_steam_apps_watcher(move || {
            if let Ok(mut guard) = tx.lock() {
                let _ = guard.try_send(());
            }
        });
        while rx.next().await.is_some() {
            if output.send(Message::SteamAppsChanged).await.is_err() {
                break;
            }
        }
    })
}

/// OS device arrival/removal wake for iced.
///
/// Bridges the `device_watch` callback (fires on a system thread) into the
/// iced runtime so arrivals poll immediately instead of waiting for the next
/// `Tick`. Like `tray_events`, this stream is subscribed for the whole process
/// lifetime, so exactly one watcher registration exists. Platforms without a
/// watcher (macOS) never fire; the `Tick` backstop covers them.
fn presence_hint_stream() -> impl Stream<Item = Message> {
    stream::channel(16, async move |mut output| {
        let (tx, mut rx) = iced::futures::channel::mpsc::channel::<()>(16);
        // `try_send` takes `&mut self`; the watcher callback is `Fn`, so
        // share the sender behind a mutex (uncontended: one watcher, one drain).
        let tx = std::sync::Mutex::new(tx);
        let _watched = crate::platform::device_watch::spawn_arrival_watcher(move || {
            if let Ok(mut guard) = tx.lock() {
                let _ = guard.try_send(());
            }
        });
        while rx.next().await.is_some() {
            if output.send(Message::PresenceHint).await.is_err() {
                break;
            }
        }
    })
}

/// Hitch context: the owned input hub snapshot (`InputHub`) is the
/// single source F7/F8 stamps read from (safe off the UI thread).
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

/// True when a manual target is provably missing (absolute path that no longer exists).
///
/// URLs (`://`) and relative / shell targets stay enabled to avoid false positives.
/// Empty targets are disabled (launch would fail immediately).
fn is_manual_target_missing(target: &str) -> bool {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed.contains("://") {
        return false;
    }
    let path = std::path::Path::new(trimmed);
    if path.is_absolute() {
        return !path.exists();
    }
    false
}

fn steam_appid_from_target(target: &str) -> Option<u32> {
    let rest = target.strip_prefix("steam://rungameid/")?;
    rest.split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|s| s.parse().ok())
}

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
    #[cfg(debug_assertions)]
    {
        emulate::is_emulated(serial)
    }
    #[cfg(not(debug_assertions))]
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::controller::known::KnownControllers;
    use crate::controller::model::Connection;

    #[test]
    fn reload_gate_classifies_input_vs_system() {
        // User input drops while queued.
        assert!(Message::TrayMenu("settings".into()).is_user_input());
        assert!(Message::TrayLeftClick(TrayAnchor::default()).is_user_input());
        // ...except Quit, which cancels the reload.
        assert!(!Message::TrayMenu(crate::ui::tray::QUIT_ID.into()).is_user_input());
        assert!(!Message::Exit.is_user_input());
        assert!(!Message::Tick.is_user_input());
        assert!(!Message::RendererReloadTick.is_user_input());
        assert!(!Message::RendererReloadDue.is_user_input());
    }

    /// The forget sequence end to end: prune drops the pad from the fleet, and
    /// the store must drop it too.
    ///
    /// This is the case that was broken twice: the prune ran *before* the
    /// store sync, so the forgotten pad was already gone from `current` by the
    /// time anything looked for it, and it stayed in `controllers.json` — the
    /// row vanished from the tab and came back on the next launch.
    #[test]
    fn forgetting_an_unplugged_pad_clears_it_from_the_store() {
        let mut known = KnownControllers::default();
        let before = vec![emulate::new_pad(&[], 50, Connection::Bluetooth)];
        emulate::sync_remembered_emulated(&[], &before, &mut known);
        assert!(known.is_remembered("emu-1"), "a live pad is persisted");

        // Forget it: flag flips, prune removes it, and both happen before the
        // sync sees the fleet.
        let edited = emulate::update(&before, "emu-1", |p| p.remembered = false);
        let after = emulate::prune(edited);
        assert!(
            after.is_empty(),
            "off + forgotten means gone from the fleet"
        );

        assert!(emulate::sync_remembered_emulated(
            &before, &after, &mut known
        ));
        assert!(
            !known.is_remembered("emu-1"),
            "a forgotten pad must leave controllers.json, not just stop being written"
        );
    }

    /// The other direction: unplugging a pad that was forgotten earlier. Here
    /// the pad is still in the fleet at forget time (it was connected), so it
    /// is the *unplug* that removes it from the store.
    #[test]
    fn unplugging_a_forgotten_pad_clears_it_from_the_store() {
        let mut known = KnownControllers::default();
        let plugged = emulate::update(
            std::slice::from_ref(&emulate::new_pad(&[], 50, Connection::Bluetooth)),
            "emu-1",
            |p| p.connected = true,
        );
        emulate::sync_remembered_emulated(&[], &plugged, &mut known);

        // Forget while connected: stays in the fleet, drops from the store.
        let forgotten = emulate::update(&plugged, "emu-1", |p| p.remembered = false);
        assert_eq!(emulate::prune(forgotten.clone()).len(), 1, "live pads stay");
        emulate::sync_remembered_emulated(&plugged, &forgotten, &mut known);
        assert!(!known.is_remembered("emu-1"));

        // Now unplug it: prune removes it, and it must stay out of the store.
        let unplugged = emulate::prune(emulate::update(&forgotten, "emu-1", |p| {
            p.connected = false;
        }));
        assert!(unplugged.is_empty());
        emulate::sync_remembered_emulated(&forgotten, &unplugged, &mut known);
        assert!(!known.is_remembered("emu-1"));
    }

    /// A pad that is merely unplugged stays remembered, so it returns after a
    /// restart — the whole point of the pin.
    #[test]
    fn an_unplugged_remembered_pad_survives_for_the_next_boot() {
        let mut known = KnownControllers::default();
        let plugged = emulate::update(
            std::slice::from_ref(&emulate::new_pad(&[], 50, Connection::Bluetooth)),
            "emu-1",
            |p| p.connected = true,
        );
        let unplugged = emulate::prune(emulate::update(&plugged, "emu-1", |p| {
            p.connected = false;
        }));
        assert_eq!(unplugged.len(), 1);
        emulate::sync_remembered_emulated(&plugged, &unplugged, &mut known);
        assert!(known.is_remembered("emu-1"));

        let remembered = vec![(
            known.remembered_disconnected(&[])[0].serial.clone(),
            50,
            Connection::Bluetooth,
        )];
        assert_eq!(
            emulate::rehydrate(&remembered).len(),
            1,
            "it must come back after a restart"
        );
    }

    /// Removing one pad must not disturb the others.
    #[test]
    fn removing_one_pad_leaves_the_rest_remembered() {
        let mut known = KnownControllers::default();
        let a = emulate::new_pad(&[], 50, Connection::Bluetooth);
        let both = vec![a.clone(), emulate::new_pad(&[a], 70, Connection::Usb)];
        emulate::sync_remembered_emulated(&[], &both, &mut known);

        let edited = emulate::update(&both, "emu-1", |p| p.remembered = false);
        let after = emulate::prune(edited);
        emulate::sync_remembered_emulated(&both, &after, &mut known);

        assert!(!known.is_remembered("emu-1"));
        assert!(known.is_remembered("emu-2"), "the other pad is untouched");
    }
}
