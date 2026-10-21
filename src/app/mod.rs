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

use crate::controller::dualsense::lightbar::ClaimStore;
use crate::domain::color::SpectrumService;
use crate::domain::pad::InputHub;

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
use crate::games::GamesCatalog;
use crate::games::process_match::RunningSession;
use crate::games::steam::SteamGame;
use crate::persist::analytics::AnalyticsStore;
use crate::persist::notify::NotifyEvent;
use crate::persist::prefs::{NavbarVisibility, Prefs};
use crate::platform::app_log;
use crate::platform::host::{Autostart as _, Host, Hotkeys as _, Watcher as _};
use crate::platform::ui_sound::UiSoundKind;
use crate::ui::configure::{
    self as configure_view, AnalyticsPanel, ConfigureMessage, ConfigureState, PadInputPanel,
    Section,
};
use crate::ui::layout::{
    MonitorCover, ToastPlacement, TrayAnchor, focus_window, raise_window_topmost,
};
use crate::ui::popup::{self as popup_view, ControllerRow, PopupMessage};
use crate::ui::start::art_worker::ArtOwner;
use crate::ui::start::cursor_hide;
use crate::ui::start::gesture::{self, ChordReleaseGate};
use crate::ui::start::input::{
    self as start_input, CrossHold, FaceHeld, GestureDetectorBank, NavAction, NavLogSnapshot,
    NavSource, PadNavBank,
};
use crate::ui::start::mode::{self as start_mode};
use crate::ui::start::view::{self as start_view, StartMessage};
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
    /// Owned pad-input hub shared with the HID worker thread and the
    /// pad-input subscription (no process-global).
    input_hub: Arc<InputHub>,
    /// Owned lightbar-claim unit shared with the HID worker (presence-empty
    /// fast path syncs through this clone; no process-global).
    claims: Arc<ClaimStore>,
    /// Owned Start-art decode unit (single-flight mailbox + iced event sink;
    /// no process-global).
    art: Arc<ArtOwner>,

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

        #[cfg(windows)]
        Host::ensure_quiet_entry();

        let hid_worker = HidWorkerHandle::start(spectrum.clone());
        let identifying = hid_worker.identifying();
        let input_hub = hid_worker.input_hub();
        let claims = hid_worker.claims();
        claims.set_enabled(prefs.lightbar_enabled);
        let art = Arc::new(ArtOwner::new());

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
            input_hub,
            claims,
            art,
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
        start_hitch_hotkey_worker(app.input_hub.clone());
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
            subscriptions.push(Subscription::run_with(
                self.input_hub.clone(),
                pad_input_edge_stream,
            ));
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
                Subscription::run_with(
                    self.art.clone(),
                    crate::ui::start::art_worker::event_stream,
                )
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
                self.art.shutdown();
                // Idempotent with `InputEdgeBindGuard::drop` (subscription
                // teardown also clears).
                self.input_hub.clear();
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
            self.claims.sync(std::iter::empty::<&str>());
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
                    lightbar_enabled: self.claims.is_enabled(),
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
            lightbar_enabled: self.claims.is_enabled(),
        };
        let effects = self.session.apply_controllers(controllers, ctx);
        self.apply_session_effects(effects)
    }

    fn sync_low_battery(&mut self) {
        if let Some(effect) = self.session.refresh_low_battery(self.claims.is_enabled()) {
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

    /// True when HID/UI input sampling should run at the active (~report-rate) rate.
    #[allow(dead_code)] // kept for callers / diagnostics that still want the hot predicate
    fn pad_input_hot(&self) -> bool {
        self.start_visible
            || (self.configure_window.is_some()
                && self.configure_state.section == Section::PadInput)
            || (self.session.prefs.start_screen_enabled && !self.session.controllers.is_empty())
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
fn pad_input_edge_stream(hub: &Arc<InputHub>) -> impl Stream<Item = Message> + Send + use<> {
    // Deep enough to ride out Start-frame hitches without dropping taps:
    // the worker publishes at report rate while hot.
    let hub = Arc::clone(hub);
    stream::channel(128, async move |mut output| {
        let (tx, mut rx) = iced::futures::channel::mpsc::channel(128);
        hub.bind(tx);
        let _guard = InputEdgeBindGuard { hub: hub.clone() };
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

struct InputEdgeBindGuard {
    hub: Arc<InputHub>,
}
impl Drop for InputEdgeBindGuard {
    fn drop(&mut self) {
        // Idempotent with the `Message::Exit` clear below (clearing an
        // unbound hub is a no-op).
        self.hub.clear();
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
        let _watched = Host::spawn(move || {
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
fn stamp_hitch_report(hub: &InputHub, kind: &str, source: &str, extra: Option<String>) {
    let (pads, reason, age_ms, seq) = match hub.read() {
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
fn start_hitch_hotkey_worker(hub: Arc<InputHub>) {
    use std::sync::OnceLock;
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        thread::spawn(move || {
            if !Host::register_hitch_lightbar() {
                app_log::warn("hitch hotkey: failed to register F7");
                return;
            }
            if !Host::register_hitch_input() {
                app_log::warn("hitch hotkey: failed to register F8");
                Host::unregister_hitch_lightbar();
                return;
            }
            app_log::info("hitch hotkeys armed: F7=lightbar F8=input");

            while let Some(id) = Host::next_hotkey() {
                let kind = if id == Host::HITCH_LIGHTBAR_ID {
                    "lightbar"
                } else if id == Host::HITCH_INPUT_ID {
                    "input"
                } else {
                    continue;
                };
                // Stamp immediately on this thread — do not wait for iced.
                stamp_hitch_report(&hub, kind, "hotkey", None);
            }

            Host::unregister_hitch_lightbar();
            Host::unregister_hitch_input();
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

    thread::spawn(move || {
        // `RegisterHotKey` binds to the calling thread's message queue, so the
        // whole lifecycle has to live on this worker.
        let _ = id_sender.send(Host::current_thread_id());
        if !Host::register_escape() {
            return;
        }

        while let Some(id) = Host::next_hotkey() {
            if id == Host::ESCAPE_ID {
                let _ = sender.send(());
                break;
            }
        }

        Host::unregister_escape();
    });

    // Posting `WM_QUIT` unblocks `GetMessageW` when the toast is dismissed by
    // some other means and this future is dropped.
    let _guard = Host::quit_guard(id_receiver.recv().unwrap_or(0));
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
